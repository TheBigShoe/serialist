//! The in-process virtual serial link: a host side that implements the transport traits
//! and a device thread that drives a [`SimDevice`].
//!
//! Timing model. Each direction is a wire with a release schedule. Bytes offered to a
//! paced wire queue behind whatever is still being transmitted and finish at exactly
//! `serial.bytes_per_second()`. The wire groups them into packets holding at most one
//! [`PACKET_INTERVAL`] of data (and never more than `max_chunk` bytes), like a USB
//! adapter that flushes its FIFO once per poll. A packet becomes readable when its last
//! byte has finished on the wire, plus `latency`, plus a seeded `jitter` sample. Release
//! times never go backwards, so bytes stay in order however large the jitter. An unpaced
//! wire skips the transmission time but keeps latency, jitter and chunking.
//!
//! Faults (drop, corrupt) are drawn per byte from a seeded generator that is separate
//! from the jitter generator, so the fault pattern depends only on the byte sequence and
//! the seed, never on how the bytes happened to be batched.
//!
//! Nothing here busy-waits: the host reader and the device thread sleep on condition
//! variables until the next release time, the caller's deadline, or a state change.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serialist_core::{
    ControlLine, SerialConfig, Transport, TransportError, TransportReader, TransportWriter,
};

use crate::{DeviceOutput, LinkConfig, LinkStats, SimDevice};

/// How often a paced wire hands a packet to the reader, like a USB-serial adapter's
/// poll interval. At 3 Mbaud that is about 300 bytes per packet.
pub const PACKET_INTERVAL: Duration = Duration::from_millis(1);

/// A paced device is not ticked while more than this much data is still waiting for
/// the wire, which bounds both memory and how long a baud change takes to show.
const PACED_LOOKAHEAD: Duration = Duration::from_millis(20);

/// A device is not ticked while this many bytes wait for the host to read them.
const MAX_BACKLOG: usize = 1 << 20;
const RESUME_BACKLOG: usize = MAX_BACKLOG / 2;

/// Upper bound on a single blocking wait, so an absurd timeout cannot overflow `Instant`.
const MAX_WAIT: Duration = Duration::from_secs(3600);

struct Packet {
    data: Vec<u8>,
    pos: usize,
    release_at: Instant,
}

/// One direction of the link.
struct Wire {
    packets: VecDeque<Packet>,
    /// Bytes in `packets` not yet taken by the receiving side.
    queued: usize,
    /// When the wire finishes transmitting everything scheduled so far.
    wire_free_at: Instant,
    last_release: Instant,
    fault_rng: StdRng,
    jitter_rng: StdRng,
}

/// Derive independent generator seeds from the one link seed (SplitMix64 finaliser).
fn mix_seed(seed: u64, stream: u64) -> u64 {
    let mut z = seed.wrapping_add(stream.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn probability(p: f64) -> f64 {
    if p.is_nan() || p <= 0.0 {
        0.0
    } else {
        p.min(1.0)
    }
}

/// Bytes per second the wire moves, or `None` when unpaced.
fn wire_rate(cfg: &LinkConfig) -> Option<f64> {
    if !cfg.pace_to_baud {
        return None;
    }
    let bps = cfg.serial.bytes_per_second();
    (bps.is_finite() && bps > 0.0).then_some(bps)
}

fn earliest(a: Option<Instant>, b: Instant) -> Option<Instant> {
    Some(a.map_or(b, |a| a.min(b)))
}

impl Wire {
    fn new(now: Instant, seed: u64, stream: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            queued: 0,
            wire_free_at: now,
            last_release: now,
            fault_rng: StdRng::seed_from_u64(mix_seed(seed, stream)),
            jitter_rng: StdRng::seed_from_u64(mix_seed(seed, stream + 1)),
        }
    }

    /// Schedule `bytes`, offered to the wire at `now`.
    fn push(&mut self, bytes: &[u8], now: Instant, cfg: &LinkConfig, stats: &mut LinkStats) {
        if bytes.is_empty() {
            return;
        }
        let max_chunk = cfg.max_chunk.max(1);
        let rate = wire_rate(cfg);
        let (start, piece_len) = match rate {
            Some(bps) => {
                let per_interval = (bps * PACKET_INTERVAL.as_secs_f64()).ceil() as usize;
                (self.wire_free_at.max(now), per_interval.clamp(1, max_chunk))
            }
            None => (now, max_chunk),
        };
        let mut offset = 0usize;
        for piece in bytes.chunks(piece_len) {
            offset += piece.len();
            let done = match rate {
                Some(bps) => start + Duration::from_secs_f64(offset as f64 / bps),
                None => now,
            };
            let jitter = self.jitter(cfg.jitter);
            let release = (done + cfg.latency + jitter).max(self.last_release);
            self.last_release = release;
            let data = self.apply_faults(piece, cfg, stats);
            if !data.is_empty() {
                self.queued += data.len();
                self.packets.push_back(Packet {
                    data,
                    pos: 0,
                    release_at: release,
                });
            }
        }
        self.wire_free_at = match rate {
            Some(bps) => start + Duration::from_secs_f64(bytes.len() as f64 / bps),
            None => now,
        };
    }

    fn jitter(&mut self, max: Duration) -> Duration {
        if max.is_zero() {
            return Duration::ZERO;
        }
        let max_ns = u64::try_from(max.as_nanos()).unwrap_or(u64::MAX);
        Duration::from_nanos(self.jitter_rng.random_range(0..=max_ns))
    }

    fn apply_faults(&mut self, bytes: &[u8], cfg: &LinkConfig, stats: &mut LinkStats) -> Vec<u8> {
        let drop_p = probability(cfg.drop_probability);
        let corrupt_p = probability(cfg.corrupt_probability);
        if drop_p <= 0.0 && corrupt_p <= 0.0 {
            return bytes.to_vec();
        }
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            if drop_p > 0.0 && self.fault_rng.random_bool(drop_p) {
                stats.dropped_bytes += 1;
                continue;
            }
            if corrupt_p > 0.0 && self.fault_rng.random_bool(corrupt_p) {
                // XOR with a non-zero mask, so a corrupted byte always differs.
                out.push(b ^ self.fault_rng.random_range(1..=u8::MAX));
                stats.corrupted_bytes += 1;
            } else {
                out.push(b);
            }
        }
        out
    }

    fn next_release(&self) -> Option<Instant> {
        self.packets.front().map(|p| p.release_at)
    }

    /// Copy released bytes into `buf`, merging packets, never past `buf.len()`.
    fn read_ready(&mut self, now: Instant, buf: &mut [u8]) -> usize {
        let mut n = 0;
        while n < buf.len() {
            let Some(front) = self.packets.front_mut() else {
                break;
            };
            if front.release_at > now {
                break;
            }
            let avail = &front.data[front.pos..];
            let k = avail.len().min(buf.len() - n);
            buf[n..n + k].copy_from_slice(&avail[..k]);
            front.pos += k;
            n += k;
            if front.pos == front.data.len() {
                self.packets.pop_front();
            }
        }
        self.queued -= n;
        n
    }

    /// Take released packets whole, one entry per packet.
    fn take_ready_packets(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while self.packets.front().is_some_and(|p| p.release_at <= now) {
            if let Some(mut packet) = self.packets.pop_front() {
                if packet.pos > 0 {
                    packet.data.drain(..packet.pos);
                }
                self.queued -= packet.data.len();
                out.push(packet.data);
            }
        }
        out
    }

    fn clear(&mut self) {
        self.packets.clear();
        self.queued = 0;
    }
}

enum Backlog {
    Clear,
    UntilDrained,
    Until(Instant),
}

struct LinkState {
    cfg: LinkConfig,
    to_host: Wire,
    to_device: Wire,
    unplugged: bool,
    reader_open: bool,
    writer_open: bool,
    device_running: bool,
    device_waiting_for_drain: bool,
    dtr: bool,
    rts: bool,
    controls: VecDeque<(ControlLine, bool)>,
    stats: LinkStats,
}

impl LinkState {
    fn host_gone(&self) -> bool {
        !self.reader_open && !self.writer_open
    }

    /// Whether the device may produce more output now.
    fn backlog(&self, now: Instant) -> Backlog {
        if self.to_host.queued >= MAX_BACKLOG {
            return Backlog::UntilDrained;
        }
        if wire_rate(&self.cfg).is_some()
            && let Some(resume) = self.to_host.wire_free_at.checked_sub(PACED_LOOKAHEAD)
            && resume > now
        {
            return Backlog::Until(resume);
        }
        Backlog::Clear
    }

    fn mark_unplugged(&mut self) {
        self.unplugged = true;
        self.to_device.clear();
        self.controls.clear();
    }
}

struct Shared {
    name: String,
    state: Mutex<LinkState>,
    /// The host reader waits here.
    host_cv: Condvar,
    /// The device thread waits here.
    device_cv: Condvar,
    /// `LinkHandle::wait_for_device_exit` waits here.
    exit_cv: Condvar,
}

impl Shared {
    fn unplug(&self) {
        self.state.lock().mark_unplugged();
        self.host_cv.notify_all();
        self.device_cv.notify_all();
    }
}

/// Entry point for building links. See the module docs for the timing model.
pub struct VirtualLink;

impl VirtualLink {
    /// Start `device` on its own thread and return the host side of the link plus a
    /// handle for unplugging it and reading its counters.
    ///
    /// The device thread exits when the link is unplugged (from the handle or by the
    /// device itself) or when the host drops both the reader and the writer.
    pub fn connect(device: Box<dyn SimDevice>, cfg: LinkConfig) -> (Transport, LinkHandle) {
        let now = Instant::now();
        let name = device.name().to_owned();
        let description = format!("virtual:{name} @ {}", cfg.serial.summary());
        let seed = cfg.seed;
        let shared = Arc::new(Shared {
            name,
            state: Mutex::new(LinkState {
                cfg,
                to_host: Wire::new(now, seed, 0),
                to_device: Wire::new(now, seed, 2),
                unplugged: false,
                reader_open: true,
                writer_open: true,
                device_running: true,
                device_waiting_for_drain: false,
                // A POSIX open raises DTR and RTS, so a fresh link starts with both asserted.
                dtr: true,
                rts: true,
                controls: VecDeque::new(),
                stats: LinkStats::default(),
            }),
            host_cv: Condvar::new(),
            device_cv: Condvar::new(),
            exit_cv: Condvar::new(),
        });

        let thread_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name(format!("serialist-sim-{}", shared.name))
            .spawn(move || run_device(&thread_shared, device));
        if let Err(err) = spawned {
            tracing::error!(%err, device = %shared.name, "could not start the simulated device thread");
            let mut st = shared.state.lock();
            st.mark_unplugged();
            st.device_running = false;
        }

        let transport = Transport {
            reader: Box::new(VirtualReader {
                shared: Arc::clone(&shared),
            }),
            writer: Box::new(VirtualWriter {
                shared: Arc::clone(&shared),
            }),
            description,
        };
        (transport, LinkHandle { shared })
    }
}

/// Test-side control over one virtual link. Cheap to clone.
#[derive(Clone)]
pub struct LinkHandle {
    shared: Arc<Shared>,
}

impl LinkHandle {
    pub fn device_name(&self) -> &str {
        &self.shared.name
    }

    /// Pull the cable. The host reader still returns bytes already queued on the link,
    /// then `Err(TransportError::Disconnected)`; writes fail at once. Idempotent.
    pub fn unplug(&self) {
        self.shared.unplug();
    }

    pub fn is_unplugged(&self) -> bool {
        self.shared.state.lock().unplugged
    }

    pub fn stats(&self) -> LinkStats {
        self.shared.state.lock().stats
    }

    pub fn link_config(&self) -> LinkConfig {
        self.shared.state.lock().cfg.clone()
    }

    /// Change how the link behaves from now on. Bytes already scheduled keep their
    /// timing. `seed` only takes effect when a link is created.
    pub fn set_link_config(&self, cfg: LinkConfig) {
        self.shared.state.lock().cfg = cfg;
        self.shared.host_cv.notify_all();
        self.shared.device_cv.notify_all();
    }

    /// The level the host last set on `line`. Both start asserted.
    pub fn control_line(&self, line: ControlLine) -> bool {
        let st = self.shared.state.lock();
        match line {
            ControlLine::Dtr => st.dtr,
            ControlLine::Rts => st.rts,
        }
    }

    /// Bytes the device has sent that the host has not read yet.
    pub fn queued_to_host(&self) -> usize {
        self.shared.state.lock().to_host.queued
    }

    pub fn is_device_running(&self) -> bool {
        self.shared.state.lock().device_running
    }

    /// Wait for the device thread to finish. Returns whether it did within `timeout`.
    pub fn wait_for_device_exit(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout.min(MAX_WAIT);
        let mut st = self.shared.state.lock();
        while st.device_running {
            if self
                .shared
                .exit_cv
                .wait_until(&mut st, deadline)
                .timed_out()
            {
                return !st.device_running;
            }
        }
        true
    }
}

impl fmt::Debug for LinkHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.shared.state.lock();
        f.debug_struct("LinkHandle")
            .field("device", &self.shared.name)
            .field("unplugged", &st.unplugged)
            .field("stats", &st.stats)
            .finish()
    }
}

struct VirtualReader {
    shared: Arc<Shared>,
}

impl TransportReader for VirtualReader {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        let shared = &*self.shared;
        let deadline = Instant::now() + timeout.min(MAX_WAIT);
        let mut st = shared.state.lock();
        st.stats.host_read_calls += 1;
        loop {
            let now = Instant::now();
            let limit = buf.len().min(st.cfg.max_chunk.max(1));
            let n = st.to_host.read_ready(now, &mut buf[..limit]);
            if n > 0 {
                if st.device_waiting_for_drain && st.to_host.queued < RESUME_BACKLOG {
                    st.device_waiting_for_drain = false;
                    shared.device_cv.notify_one();
                }
                return Ok(n);
            }
            if st.unplugged && st.to_host.packets.is_empty() {
                return Err(TransportError::Disconnected);
            }
            if now >= deadline || limit == 0 {
                return Ok(0);
            }
            let wake = st
                .to_host
                .next_release()
                .map_or(deadline, |t| t.min(deadline));
            shared.host_cv.wait_until(&mut st, wake);
        }
    }
}

impl Drop for VirtualReader {
    fn drop(&mut self) {
        self.shared.state.lock().reader_open = false;
        self.shared.device_cv.notify_all();
    }
}

struct VirtualWriter {
    shared: Arc<Shared>,
}

impl TransportWriter for VirtualWriter {
    /// Schedules the bytes on the wire and returns without waiting for them to be sent,
    /// like a write into a large OS buffer. The device sees them at the paced rate.
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let mut guard = self.shared.state.lock();
        if guard.unplugged {
            return Err(TransportError::Disconnected);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let st = &mut *guard;
        st.stats.host_to_device_bytes += bytes.len() as u64;
        st.to_device
            .push(bytes, Instant::now(), &st.cfg, &mut st.stats);
        self.shared.device_cv.notify_one();
        Ok(())
    }

    fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError> {
        let mut st = self.shared.state.lock();
        if st.unplugged {
            return Err(TransportError::Disconnected);
        }
        match line {
            ControlLine::Dtr => st.dtr = asserted,
            ControlLine::Rts => st.rts = asserted,
        }
        st.controls.push_back((line, asserted));
        self.shared.device_cv.notify_one();
        Ok(())
    }

    /// Changes the pacing rate live for both directions; bytes already on the wire keep
    /// their schedule.
    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
        if config.baud == 0 {
            return Err(TransportError::Config(
                "baud must be greater than zero".into(),
            ));
        }
        let mut st = self.shared.state.lock();
        if st.unplugged {
            return Err(TransportError::Disconnected);
        }
        st.cfg.serial = config.clone();
        self.shared.device_cv.notify_one();
        Ok(())
    }
}

impl Drop for VirtualWriter {
    fn drop(&mut self) {
        self.shared.state.lock().writer_open = false;
        self.shared.device_cv.notify_all();
    }
}

/// Device output buffered during one callback, handed to the wire afterwards so device
/// code never runs under the link lock.
#[derive(Default)]
struct Outbox {
    data: Vec<u8>,
    disconnect: bool,
}

impl DeviceOutput for Outbox {
    fn send(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }

    fn disconnect(&mut self) {
        self.disconnect = true;
    }
}

impl Outbox {
    /// Returns false once the link is down and the device thread should stop.
    fn flush(&mut self, shared: &Shared) -> bool {
        if self.data.is_empty() && !self.disconnect {
            return true;
        }
        let mut guard = shared.state.lock();
        if guard.unplugged {
            self.data.clear();
            return false;
        }
        if !self.data.is_empty() {
            let st = &mut *guard;
            st.stats.device_to_host_bytes += self.data.len() as u64;
            st.to_host
                .push(&self.data, Instant::now(), &st.cfg, &mut st.stats);
            self.data.clear();
            shared.host_cv.notify_one();
        }
        if self.disconnect {
            guard.mark_unplugged();
            drop(guard);
            shared.host_cv.notify_all();
            return false;
        }
        true
    }
}

struct Work {
    controls: Vec<(ControlLine, bool)>,
    incoming: Vec<Vec<u8>>,
    tick: Option<Instant>,
}

/// Marks the link down when the device thread ends, however it ends (including a panic
/// in device code), so the host sees `Disconnected` instead of waiting forever.
struct DeviceExit<'a>(&'a Shared);

impl Drop for DeviceExit<'_> {
    fn drop(&mut self) {
        let shared = self.0;
        {
            let mut st = shared.state.lock();
            st.device_running = false;
            st.mark_unplugged();
        }
        if thread::panicking() {
            tracing::error!(device = %shared.name, "simulated device panicked; link unplugged");
        }
        shared.host_cv.notify_all();
        shared.device_cv.notify_all();
        shared.exit_cv.notify_all();
    }
}

fn run_device(shared: &Shared, mut device: Box<dyn SimDevice>) {
    let _exit = DeviceExit(shared);
    let mut out = Outbox::default();
    device.on_connect(&mut out);
    if !out.flush(shared) {
        return;
    }
    let mut next_tick = Some(Instant::now());
    while let Some(work) = wait_for_work(shared, next_tick) {
        for (line, asserted) in work.controls {
            device.on_control(line, asserted, &mut out);
        }
        for chunk in &work.incoming {
            device.on_receive(chunk, &mut out);
        }
        if work.tick.is_some() {
            next_tick = device.on_tick(Instant::now(), &mut out);
        } else if !work.incoming.is_empty() && next_tick.is_none() {
            // A sleeping device gets one tick after new data so it can schedule itself.
            next_tick = Some(Instant::now());
        }
        if !out.flush(shared) {
            return;
        }
    }
}

/// Sleep until the device has something to do. `None` means the thread should exit.
fn wait_for_work(shared: &Shared, next_tick: Option<Instant>) -> Option<Work> {
    let mut st = shared.state.lock();
    loop {
        if st.unplugged || st.host_gone() {
            return None;
        }
        let now = Instant::now();
        let mut wake = st.to_device.next_release();
        let mut tick = None;
        match next_tick {
            Some(t) if t <= now => match st.backlog(now) {
                Backlog::Clear => tick = Some(now),
                Backlog::Until(resume) => wake = earliest(wake, resume),
                Backlog::UntilDrained => st.device_waiting_for_drain = true,
            },
            Some(t) => wake = earliest(wake, t),
            None => {}
        }
        let incoming = st.to_device.take_ready_packets(now);
        let controls: Vec<_> = st.controls.drain(..).collect();
        if tick.is_some() || !incoming.is_empty() || !controls.is_empty() {
            return Some(Work {
                controls,
                incoming,
                tick,
            });
        }
        match wake {
            Some(t) => {
                shared.device_cv.wait_until(&mut st, t);
            }
            None => shared.device_cv.wait(&mut st),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paced(baud: u32) -> LinkConfig {
        LinkConfig {
            serial: SerialConfig {
                baud,
                ..SerialConfig::default()
            },
            latency: Duration::ZERO,
            ..LinkConfig::default()
        }
    }

    #[test]
    fn paced_schedule_is_exact() {
        // 1 Mbaud 8N1 is 100_000 bytes/s: 100 bytes per 1 ms packet.
        let cfg = paced(1_000_000);
        let t0 = Instant::now();
        let mut wire = Wire::new(t0, 0, 0);
        let mut stats = LinkStats::default();
        wire.push(&[0u8; 10_000], t0, &cfg, &mut stats);
        assert_eq!(wire.packets.len(), 100);
        for (i, p) in wire.packets.iter().enumerate() {
            assert_eq!(p.data.len(), 100);
            let expected = Duration::from_micros(1000 * (i as u64 + 1));
            let got = p.release_at - t0;
            let err = got.abs_diff(expected);
            assert!(
                err < Duration::from_micros(1),
                "packet {i}: {got:?} vs {expected:?}"
            );
        }
        assert_eq!(wire.wire_free_at - t0, Duration::from_millis(100));

        // A second batch queues behind the first.
        wire.push(&[0u8; 50], t0, &cfg, &mut stats);
        let last = wire.packets.back().unwrap();
        assert_eq!(last.release_at - t0, Duration::from_micros(100_500));
    }

    #[test]
    fn packets_respect_max_chunk_and_latency() {
        let mut cfg = LinkConfig::unpaced();
        cfg.max_chunk = 7;
        cfg.latency = Duration::from_millis(3);
        let t0 = Instant::now();
        let mut wire = Wire::new(t0, 0, 0);
        wire.push(&[1u8; 20], t0, &cfg, &mut LinkStats::default());
        let sizes: Vec<_> = wire.packets.iter().map(|p| p.data.len()).collect();
        assert_eq!(sizes, [7, 7, 6]);
        assert!(
            wire.packets
                .iter()
                .all(|p| p.release_at == t0 + Duration::from_millis(3))
        );
    }

    #[test]
    fn jitter_never_reorders() {
        let mut cfg = LinkConfig::unpaced();
        cfg.max_chunk = 1;
        cfg.jitter = Duration::from_millis(5);
        let t0 = Instant::now();
        let mut wire = Wire::new(t0, 7, 0);
        let bytes: Vec<u8> = (0..=255).collect();
        wire.push(&bytes, t0, &cfg, &mut LinkStats::default());
        let releases: Vec<_> = wire.packets.iter().map(|p| p.release_at).collect();
        assert!(releases.windows(2).all(|w| w[0] <= w[1]));
        let mut out = vec![0u8; 256];
        let n = wire.read_ready(t0 + Duration::from_millis(10), &mut out);
        assert_eq!(n, 256);
        assert_eq!(out, bytes);
    }

    #[test]
    fn faults_are_counted_and_deterministic() {
        let cfg = LinkConfig {
            drop_probability: 0.1,
            corrupt_probability: 0.1,
            seed: 42,
            ..LinkConfig::unpaced()
        };
        // Batching differs between runs; the fault pattern must not.
        let run = |batch: usize| {
            let t0 = Instant::now();
            let mut wire = Wire::new(t0, cfg.seed, 0);
            let mut stats = LinkStats::default();
            for chunk in [0u8; 10_000].chunks(batch) {
                wire.push(chunk, t0, &cfg, &mut stats);
            }
            let mut out = vec![0u8; 10_000];
            let n = wire.read_ready(t0 + Duration::from_secs(1), &mut out);
            out.truncate(n);
            (out, stats)
        };
        let (a, stats) = run(10_000);
        let (b, stats_b) = run(333);
        assert_eq!(a, b);
        assert_eq!(stats, stats_b);
        assert_eq!(a.len() as u64, 10_000 - stats.dropped_bytes);
        assert_eq!(
            a.iter().filter(|&&b| b != 0).count() as u64,
            stats.corrupted_bytes
        );
        assert!(stats.dropped_bytes > 800 && stats.dropped_bytes < 1200);
    }
}
