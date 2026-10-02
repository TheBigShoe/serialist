//! Helpers shared by the integration tests. Each test binary uses a different subset.
//!
//! # Timing tests on a manual clock
//!
//! Timing is asserted exactly, on a [`ManualClock`]: time moves only when the test moves
//! it, so a loaded machine can delay a test but never change its outcome. The pattern:
//!
//! 1. Connect on the clock (`VirtualLink::connect_with_clock`, or a world built with
//!    `SimWorld::with_clock`), then `clock.settle(n)` so the link's threads are asleep.
//!    `n` counts every thread that can act on the link: the device thread, plus a
//!    session's reader thread or a helper thread blocked in `read`.
//! 2. Move the clock with [`advance_to`], which steps and settles. Steps of up to 24 ms
//!    wake the device thread before the wire runs dry (the look-ahead's refill margin),
//!    just as a device thread that wakes on time would.
//! 3. Look. Read what has been released with [`drain`] (zero-timeout reads, which never
//!    block). A read that should block runs on a helper thread (`thread::scope`) while
//!    the test thread moves the clock; after `settle`, a helper that has not finished is
//!    provably still waiting.
//! 4. Compare with the schedule: [`released`] is what a paced wire has delivered.
//!
//! A thread blocked on a manual clock never times out by itself, so every test that
//! leaves one blocked holds an [`UnplugOnPanic`]: a failed assertion unplugs the link,
//! the blocked thread sees `Disconnected`, and the failure is reported instead of hanging.
#![allow(dead_code)]

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use serialist_core::{
    ControlLine, SerialConfig, Session, SessionEvent, TransportError, TransportReader,
};
use serialist_sim::{
    Clock, DeviceOutput, LinkHandle, ManualClock, PACED_LOOKAHEAD, PACKET_INTERVAL, SimDevice,
    SystemClock,
};

pub const MS: Duration = Duration::from_millis(1);

/// `LinkConfig::default().latency`.
pub const LATENCY: Duration = Duration::from_millis(1);

/// Sends a fixed byte string when the link comes up, then optionally unplugs itself.
pub struct BurstDevice {
    data: Vec<u8>,
    disconnect: bool,
}

impl BurstDevice {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            disconnect: false,
        }
    }

    pub fn then_disconnect(mut self) -> Self {
        self.disconnect = true;
        self
    }
}

impl SimDevice for BurstDevice {
    fn name(&self) -> &str {
        "burst"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(&self.data);
        if self.disconnect {
            out.disconnect();
        }
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {}
}

/// What a [`RecorderDevice`] saw, shared with the test.
#[derive(Default)]
pub struct Recording {
    pub chunks: Vec<(Instant, Vec<u8>)>,
    pub controls: Vec<(ControlLine, bool)>,
}

/// Records every chunk and control change it receives, with arrival times on `clock`.
pub struct RecorderDevice {
    recording: Arc<Mutex<Recording>>,
    clock: Arc<dyn Clock>,
}

impl RecorderDevice {
    pub fn new(recording: Arc<Mutex<Recording>>) -> Self {
        Self::on_clock(recording, Arc::new(SystemClock))
    }

    /// Stamp arrivals with `clock`, which should be the link's.
    pub fn on_clock(recording: Arc<Mutex<Recording>>, clock: Arc<dyn Clock>) -> Self {
        Self { recording, clock }
    }
}

impl SimDevice for RecorderDevice {
    fn name(&self) -> &str {
        "recorder"
    }

    fn on_receive(&mut self, bytes: &[u8], _out: &mut dyn DeviceOutput) {
        let at = self.clock.now();
        self.recording.lock().chunks.push((at, bytes.to_vec()));
    }

    fn on_control(&mut self, line: ControlLine, asserted: bool, _out: &mut dyn DeviceOutput) {
        self.recording.lock().controls.push((line, asserted));
    }
}

/// Panics on the first byte it receives.
pub struct PanicDevice;

impl SimDevice for PanicDevice {
    fn name(&self) -> &str {
        "panic"
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {
        panic!("simulated device failure");
    }
}

pub fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

/// Bytes per second on the wire at `baud`, 8N1: ten bits per byte.
pub fn bytes_per_second(baud: u32) -> u64 {
    assert_eq!(
        serial(baud).bytes_per_second(),
        f64::from(baud) / 10.0,
        "the schedule model assumes 8N1"
    );
    u64::from(baud) / 10
}

/// Bytes that finish on a wire at `baud` within `d` of it starting to send.
pub fn bytes_within(baud: u32, d: Duration) -> usize {
    let bps = u128::from(bytes_per_second(baud));
    usize::try_from(d.as_nanos() * bps / 1_000_000_000).unwrap()
}

/// `n` rounded down to whole packets of one [`PACKET_INTERVAL`] of data at `baud`.
pub fn whole_packets(baud: u32, n: usize) -> usize {
    let bps = u128::from(bytes_per_second(baud));
    let packet =
        usize::try_from((bps * PACKET_INTERVAL.as_nanos()).div_ceil(1_000_000_000)).unwrap();
    n / packet * packet
}

// The schedule model. Exact integer arithmetic, so it is an independent check on the
// link's own (floating-point) schedule. Both assume a wire kept busy since it started
// sending, by a device with more to send than the look-ahead holds.

/// What a paced wire at `baud` has released `elapsed` after it started sending: whole
/// packets, each [`LATENCY`] after its last byte finished.
pub fn released(baud: u32, elapsed: Duration) -> usize {
    whole_packets(baud, bytes_within(baud, elapsed.saturating_sub(LATENCY)))
}

/// What the device thread has committed to a paced wire at `baud` when woken `elapsed`
/// after it started sending: whole packets, up to one [`PACED_LOOKAHEAD`] past then.
pub fn committed(baud: u32, elapsed: Duration) -> usize {
    whole_packets(baud, bytes_within(baud, elapsed + PACED_LOOKAHEAD))
}

/// [`released`] for a wire whose first `first` bytes went at `baud` and the rest at
/// `then`, back to back: the new rate's run starts where the old one ends.
pub fn released_switching(first: usize, baud: u32, then: u32, elapsed: Duration) -> usize {
    let first_ns = first as u128 * 1_000_000_000 / u128::from(bytes_per_second(baud));
    let switch = Duration::from_nanos(u64::try_from(first_ns).unwrap());
    if elapsed <= switch + LATENCY {
        released(baud, elapsed)
    } else {
        first + released(then, elapsed - switch)
    }
}

/// Move `clock` to `target` in steps of at most `step`, settling `threads` after each.
pub fn advance_to(clock: &ManualClock, target: Instant, step: Duration, threads: usize) {
    while clock.now() < target {
        clock.advance(step.min(target - clock.now()));
        clock.settle(threads);
    }
}

/// Everything the link has released to the host so far, read without blocking (zero
/// timeouts until a read returns nothing), and whether the link then reported
/// `Disconnected`.
pub fn drain(reader: &mut dyn TransportReader) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf, Duration::ZERO) {
            Ok(0) => return (out, false),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(TransportError::Disconnected) => return (out, true),
            Err(other) => panic!("unexpected read error: {other}"),
        }
    }
}

/// Unplugs a link if the test panics while this is alive, so a thread blocked on a
/// manual clock (a helper reader, a session's reader thread) sees `Disconnected` and the
/// failure is reported instead of hanging. Declare it after the session it protects, so
/// it drops first.
pub struct UnplugOnPanic(pub LinkHandle);

impl Drop for UnplugOnPanic {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0.unplug();
        }
    }
}

/// Close `session` whose link runs on `clock`. Its reader thread only sees the stop flag
/// when its current read returns, which on a manual clock needs the clock to move, so
/// this moves it by `step` at a time until `close` returns.
pub fn close_on(clock: &ManualClock, session: Session, step: Duration) {
    let closer = thread::spawn(move || session.close());
    let limit = Instant::now() + Duration::from_secs(10);
    while !closer.is_finished() {
        assert!(Instant::now() < limit, "close never returned");
        clock.advance(step);
        thread::sleep(Duration::from_millis(1));
    }
    closer.join().expect("close panicked");
}

/// Poll `condition` in real time, failing the test after `limit`.
pub fn wait_for(limit: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen in {limit:?}"
        );
        thread::sleep(Duration::from_micros(200));
    }
}

/// Read until `n` bytes arrived, failing the test after `limit`. Also checks every read
/// respects `max_chunk`. Real-time links only: on a manual clock it would block.
pub fn read_exactly(
    reader: &mut dyn TransportReader,
    n: usize,
    max_chunk: usize,
    limit: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + limit;
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < n {
        assert!(
            Instant::now() < deadline,
            "only {} of {n} bytes after {limit:?}",
            out.len()
        );
        let got = reader
            .read(&mut buf, Duration::from_millis(20))
            .expect("link went down early");
        assert!(
            got <= max_chunk,
            "read returned {got} > max_chunk {max_chunk}"
        );
        out.extend_from_slice(&buf[..got]);
    }
    out
}

/// Read until the link reports `Disconnected`, failing the test after `limit`.
/// Real-time links only: on a manual clock it would block.
pub fn read_to_disconnect(reader: &mut dyn TransportReader, limit: Duration) -> Vec<u8> {
    let deadline = Instant::now() + limit;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        assert!(Instant::now() < deadline, "no disconnect after {limit:?}");
        match reader.read(&mut buf, Duration::from_millis(20)) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(TransportError::Disconnected) => return out,
            Err(other) => panic!("unexpected read error: {other}"),
        }
    }
}

pub fn next_event(events: &Receiver<SessionEvent>) -> SessionEvent {
    events
        .recv_timeout(Duration::from_secs(5))
        .expect("no session event within 5 s")
}

/// Collect `Data` until `done(&received)` holds, failing on any other event.
pub fn collect_until(
    events: &Receiver<SessionEvent>,
    mut done: impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    let mut received = Vec::new();
    while !done(&received) {
        match next_event(events) {
            SessionEvent::Data { bytes, .. } => received.extend_from_slice(&bytes),
            other => panic!("unexpected event {other:?} after {received:?}"),
        }
    }
    received
}

/// Every `Data` event already delivered, without waiting, failing on any other event.
pub fn take_data(events: &Receiver<SessionEvent>) -> Vec<u8> {
    let mut received = Vec::new();
    while let Ok(event) = events.try_recv() {
        match event {
            SessionEvent::Data { bytes, .. } => received.extend_from_slice(&bytes),
            other => panic!("unexpected event {other:?}"),
        }
    }
    received
}

/// Wall-clock timing assertions are reliable on a developer machine but not on shared CI
/// runners (a macOS runner has returned a 10 ms timeout after 63 ms). Link timing is
/// tested exactly on a [`ManualClock`]; the few smoke tests of the real-time path that
/// remain skip themselves under `CI` unless `SERIALIST_TIMING_TESTS` is set, and in a
/// coverage build (`cargo llvm-cov`, which passes `--cfg coverage`) always: instrumented
/// code runs several times slower, so the rates mean nothing. The same rule is in
/// `serialist-core/tests/timing/mod.rs`.
pub fn wall_clock_timing_enabled() -> bool {
    !cfg!(coverage)
        && (std::env::var_os("CI").is_none()
            || std::env::var_os("SERIALIST_TIMING_TESTS").is_some())
}

/// Return early from a wall-clock smoke test on CI or in a coverage build. See
/// [`wall_clock_timing_enabled`].
#[macro_export]
macro_rules! skip_unless_wall_clock_timing {
    () => {
        if !$crate::common::wall_clock_timing_enabled() {
            eprintln!(
                "skipped: wall-clock timing test under CI or coverage (set SERIALIST_TIMING_TESTS=1 \
                 to run it on CI)"
            );
            return;
        }
    };
}
