//! The in-process virtual serial link: a host side that implements the transport traits
//! and a device thread that drives a [`SimDevice`].
//!
//! Timing model. Each direction is a wire with a release schedule. Bytes offered to a
//! paced wire queue behind whatever is still being transmitted and finish at exactly
//! `serial.bytes_per_second()`. The wire groups them into packets holding one
//! [`PACKET_INTERVAL`] of data (rounded up to whole bytes, and never more than
//! `max_chunk`), like a USB adapter that flushes its FIFO once per poll. A packet becomes
//! readable when its last byte has finished on the wire, plus `latency`, plus a seeded
//! `jitter` sample. Release times never go backwards, so bytes stay in order however
//! large the jitter. An unpaced wire skips the transmission time but keeps latency,
//! jitter and chunking.
//!
//! Finish times and packet boundaries are counted in bytes from the start of the current
//! run (the wire has been busy without a break since then, at one rate), not from each
//! batch of bytes offered to it. So on a wire that is kept busy, byte `n` of the run
//! finishes at exactly `run_start + (n + 1) / rate`, and the host sees the same packets
//! at the same times however the bytes were batched.
//!
//! Device output first lands in the device's transmit FIFO. The device thread moves it
//! onto a paced wire only as far as [`PACED_LOOKAHEAD`] ahead of now, in whole packets
//! (onto an unpaced wire, only while fewer than [`HOST_BACKLOG_LIMIT`] bytes wait
//! unread), and the device is not ticked again until its FIFO is empty. So however much
//! one callback sends, at most one look-ahead of it is committed to the wire: a baud
//! change shows within one look-ahead, and pulling the cable loses what is in flight
//! instead of delaying the disconnect until it drains.
//!
//! Time comes from the link's [`Clock`]: [`VirtualLink::connect`] uses real time, and
//! [`VirtualLink::connect_with_clock`] takes any clock, such as a
//! [`ManualClock`](crate::ManualClock) that a test moves by hand.
//!
//! Faults (drop, corrupt) are drawn per byte from a seeded generator that is separate
//! from the jitter generator, so the fault pattern depends only on the byte sequence and
//! the seed, never on how the bytes happened to be batched.
//!
//! Nothing here busy-waits: the host reader and the device thread sleep through
//! [`Clock::wait`] until the next release time, the caller's deadline, or a state change.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, MutexGuard};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serialist_core::{
    ControlLine, SerialConfig, Transport, TransportError, TransportReader, TransportWriter,
};

use crate::{Clock, DeviceOutput, LinkConfig, LinkStats, SimDevice, SystemClock, Wakeup};

/// How often a paced wire hands a packet to the reader, like a USB-serial adapter's
/// poll interval. At 3 Mbaud that is about 300 bytes per packet.
pub const PACKET_INTERVAL: Duration = Duration::from_millis(1);

/// The most wire time a paced link commits ahead of now (longer only at baud rates so
/// slow that four bytes take longer). Bounds how long a baud change takes to show.
///
/// The device thread refills the wire once a quarter of this is free, which leaves
/// 24 ms of margin: more than one 15.6 ms Windows timer tick, so a late wake-up never
/// lets the wire go idle.
pub const PACED_LOOKAHEAD: Duration = Duration::from_millis(32);

/// Device output waiting unread on the host side beyond which the device is held back.
/// With the device's FIFO (at most one callback's output) this bounds link memory.
pub const HOST_BACKLOG_LIMIT: usize = 1 << 20;
const RESUME_BACKLOG: usize = HOST_BACKLOG_LIMIT / 2;

/// Upper bound on a single blocking wait, so an absurd timeout cannot overflow `Instant`.
const MAX_WAIT: Duration = Duration::from_secs(3600);

/// Compact the device FIFO once this much of its front has been sent.
const FIFO_COMPACT: usize = 64 * 1024;

/// `t + d`, saturating far in the future instead of panicking on overflow.
pub(crate) fn later(t: Instant, d: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(100 * 365 * 24 * 3600);
    t.checked_add(d).or_else(|| t.checked_add(FAR)).unwrap_or(t)
}

/// Look-ahead and refill threshold for a wire moving `bps` bytes per second.
fn paced_window(bps: f64) -> (Duration, Duration) {
    let four_bytes = Duration::try_from_secs_f64(4.0 / bps).unwrap_or(MAX_WAIT);
    let lookahead = PACED_LOOKAHEAD.max(four_bytes.min(MAX_WAIT));
    (lookahead, lookahead / 4)
}

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
    /// The current run on a paced wire: busy without a break since `run_start`, at
    /// `run_bps`, with `run_bytes` scheduled. Zero `run_bps` means no run.
    run_start: Instant,
    run_bytes: usize,
    run_bps: f64,
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

/// Time for `bytes` at `bps`, saturating instead of panicking on absurd values.
fn wire_time(bytes: usize, bps: f64) -> Duration {
    Duration::try_from_secs_f64(bytes as f64 / bps).unwrap_or(Duration::MAX)
}

/// Whole bytes that finish within `d` at `bps`. The slack (one nanosecond, and a
/// millionth of a byte) keeps nanosecond and float rounding from losing a byte that
/// finishes exactly at the end of `d`.
fn bytes_in(d: Duration, bps: f64) -> usize {
    ((d.as_secs_f64() + 1e-9) * bps + 1e-6) as usize
}

/// Bytes in one packet of a wire moving `bps` bytes per second.
fn packet_len(bps: f64, max_chunk: usize) -> usize {
    ((bps * PACKET_INTERVAL.as_secs_f64()).ceil() as usize).clamp(1, max_chunk.max(1))
}

impl Wire {
    fn new(now: Instant, seed: u64, stream: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            queued: 0,
            wire_free_at: now,
            run_start: now,
            run_bytes: 0,
            run_bps: 0.0,
            last_release: now,
            fault_rng: StdRng::seed_from_u64(mix_seed(seed, stream)),
            jitter_rng: StdRng::seed_from_u64(mix_seed(seed, stream + 1)),
        }
    }

    /// Where bytes offered at `now` to a wire moving `bps` start: the start of the
    /// current run and the bytes already scheduled in it, or a new run from when the wire
    /// is free (now, if it is idle).
    fn run_at(&self, now: Instant, bps: f64) -> (Instant, usize) {
        if self.run_bps == bps && self.wire_free_at >= now {
            (self.run_start, self.run_bytes)
        } else {
            (self.wire_free_at.max(now), 0)
        }
    }

    /// Schedule `bytes`, offered to the wire at `now`.
    fn push(&mut self, bytes: &[u8], now: Instant, cfg: &LinkConfig, stats: &mut LinkStats) {
        if bytes.is_empty() {
            return;
        }
        let max_chunk = cfg.max_chunk.max(1);
        match wire_rate(cfg) {
            Some(bps) => {
                let packet = packet_len(bps, max_chunk);
                let (run_start, base) = self.run_at(now, bps);
                let end = base + bytes.len();
                let mut at = base;
                while at < end {
                    // Packets end on multiples of `packet` counted from the run's start.
                    let next = (at / packet + 1).saturating_mul(packet).min(end);
                    let done = later(run_start, wire_time(next, bps));
                    self.schedule(&bytes[at - base..next - base], done, cfg, stats);
                    at = next;
                }
                self.run_start = run_start;
                self.run_bytes = end;
                self.run_bps = bps;
                self.wire_free_at = later(run_start, wire_time(end, bps));
            }
            None => {
                for piece in bytes.chunks(max_chunk) {
                    self.schedule(piece, now, cfg, stats);
                }
                self.run_bps = 0.0;
                self.wire_free_at = now;
            }
        }
    }

    /// Queue one packet whose last byte finishes on the wire at `done`.
    fn schedule(&mut self, piece: &[u8], done: Instant, cfg: &LinkConfig, stats: &mut LinkStats) {
        let jitter = self.jitter(cfg.jitter.min(MAX_WAIT));
        let release = later(done, cfg.latency.min(MAX_WAIT) + jitter).max(self.last_release);
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

    /// Drop every packet not yet released at `now`; returns the bytes dropped. Release
    /// times never decrease, so those packets are a suffix of the queue.
    fn drop_unreleased(&mut self, now: Instant) -> usize {
        let mut dropped = 0;
        while self.packets.back().is_some_and(|p| p.release_at > now) {
            if let Some(p) = self.packets.pop_back() {
                dropped += p.data.len() - p.pos;
            }
        }
        self.queued -= dropped;
        dropped
    }

    fn clear(&mut self) {
        self.packets.clear();
        self.queued = 0;
    }
}

/// Whether the host-bound wire can take more device output.
enum Output {
    Ready,
    /// Wait until this time (a paced wire has refill room then).
    Until(Instant),
    /// Wait for the host to read ([`HOST_BACKLOG_LIMIT`] reached).
    UntilDrained,
}

struct LinkState {
    cfg: LinkConfig,
    to_host: Wire,
    to_device: Wire,
    /// Device output not yet on the wire: the device's transmit FIFO. Bytes before
    /// `tx_pos` have been sent.
    tx_fifo: Vec<u8>,
    tx_pos: usize,
    unplugged: bool,
    /// The device asked to disconnect. The link goes down once its FIFO has drained.
    hanging_up: bool,
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

    /// The device is gone or going: host writes and control changes fail.
    fn device_gone(&self) -> bool {
        self.unplugged || self.hanging_up
    }

    fn tx_pending(&self) -> usize {
        self.tx_fifo.len() - self.tx_pos
    }

    fn output(&self, now: Instant) -> Output {
        if self.to_host.queued >= HOST_BACKLOG_LIMIT {
            return Output::UntilDrained;
        }
        if let Some(bps) = wire_rate(&self.cfg) {
            let (lookahead, refill) = paced_window(bps);
            // Room for at least `refill` of wire time opens at `resume`.
            if let Some(resume) = self.to_host.wire_free_at.checked_sub(lookahead - refill)
                && resume > now
            {
                return Output::Until(resume);
            }
        }
        Output::Ready
    }

    /// Move device output from the FIFO onto the wire, as far as the look-ahead (paced)
    /// or the host backlog limit allows. Returns the bytes moved.
    fn pump(&mut self, now: Instant) -> usize {
        let pending = self.tx_pending();
        if pending == 0 {
            return 0;
        }
        let backlog_room = HOST_BACKLOG_LIMIT.saturating_sub(self.to_host.queued);
        let n = match wire_rate(&self.cfg) {
            Some(bps) => {
                let (lookahead, _) = paced_window(bps);
                let (run_start, base) = self.to_host.run_at(now, bps);
                let horizon = later(now, lookahead);
                let fits = bytes_in(horizon.saturating_duration_since(run_start), bps)
                    .saturating_sub(base);
                let mut n = pending.min(fits).min(backlog_room);
                if n < pending {
                    // Whole packets only (the FIFO's tail excepted), so a packet's
                    // contents and release time do not depend on when this runs.
                    let packet = packet_len(bps, self.cfg.max_chunk);
                    n = ((base + n) / packet * packet).saturating_sub(base);
                }
                // An idle wire always takes at least one byte.
                if n == 0 && base == 0 && run_start <= now && backlog_room > 0 {
                    n = 1;
                }
                n
            }
            None => pending.min(backlog_room),
        };
        if n == 0 {
            return 0;
        }
        let Self {
            tx_fifo,
            tx_pos,
            to_host,
            cfg,
            stats,
            ..
        } = self;
        stats.device_to_host_bytes += n as u64;
        to_host.push(&tx_fifo[*tx_pos..*tx_pos + n], now, cfg, stats);
        *tx_pos += n;
        if *tx_pos == tx_fifo.len() {
            tx_fifo.clear();
            *tx_pos = 0;
        } else if *tx_pos >= FIFO_COMPACT && *tx_pos * 2 >= tx_fifo.len() {
            tx_fifo.drain(..*tx_pos);
            *tx_pos = 0;
        }
        n
    }

    /// Accept one callback's worth of device output into the FIFO.
    fn enqueue(&mut self, data: &mut Vec<u8>) {
        if self.tx_pending() == 0 {
            // Swap instead of copying; `data` gets the empty FIFO's allocation back.
            std::mem::swap(&mut self.tx_fifo, data);
            self.tx_pos = 0;
            data.clear();
        } else {
            self.tx_fifo.extend_from_slice(data);
            data.clear();
        }
    }

    /// Pull the cable. Bytes already released to the host (in its driver's buffer, as it
    /// were) stay readable; everything still in flight or in the device's FIFO is lost.
    fn pull_cable(&mut self, now: Instant) {
        self.unplugged = true;
        self.hanging_up = false;
        let lost = self.to_host.drop_unreleased(now);
        self.stats.lost_on_unplug += lost as u64;
        self.tx_fifo.clear();
        self.tx_pos = 0;
        self.to_device.clear();
        self.controls.clear();
    }

    /// Finish a device-initiated disconnect: its output is all on the wire, which keeps
    /// delivering at its own pace before the host sees `Disconnected`.
    fn finish_hang_up(&mut self) {
        self.unplugged = true;
        self.hanging_up = false;
        self.to_device.clear();
        self.controls.clear();
    }
}

struct Shared {
    name: String,
    clock: Arc<dyn Clock>,
    state: Mutex<LinkState>,
    /// Wakes the host reader.
    host: Arc<Wakeup>,
    /// Wakes the device thread.
    device: Arc<Wakeup>,
    /// Wakes `LinkHandle::wait_for_device_exit`.
    exit: Arc<Wakeup>,
}

impl Shared {
    fn unplug(&self) {
        self.state.lock().pull_cable(self.clock.now());
        self.host.notify();
        self.device.notify();
    }

    /// Sleep with the link unlocked until `wakeup` is notified or the clock reaches
    /// `deadline`. Call it right after checking, under `st`, that there is nothing to do.
    fn sleep(
        &self,
        st: &mut MutexGuard<'_, LinkState>,
        wakeup: &Arc<Wakeup>,
        deadline: Option<Instant>,
    ) {
        let seen = wakeup.epoch();
        MutexGuard::unlocked(st, || self.clock.wait(wakeup, seen, deadline));
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
        Self::connect_with_clock(device, cfg, Arc::new(SystemClock))
    }

    /// [`VirtualLink::connect`] on `clock`: the release schedule, latency, jitter, read
    /// timeouts, device ticks and unplug timing all run on its time. Pass a
    /// [`ManualClock`](crate::ManualClock) to make a test's timing exact; its docs
    /// describe how to drive one.
    pub fn connect_with_clock(
        device: Box<dyn SimDevice>,
        cfg: LinkConfig,
        clock: Arc<dyn Clock>,
    ) -> (Transport, LinkHandle) {
        let now = clock.now();
        let name = device.name().to_owned();
        let description = format!("virtual:{name} @ {}", cfg.serial.summary());
        let seed = cfg.seed;
        let shared = Arc::new(Shared {
            name,
            clock,
            state: Mutex::new(LinkState {
                cfg,
                to_host: Wire::new(now, seed, 0),
                to_device: Wire::new(now, seed, 2),
                tx_fifo: Vec::new(),
                tx_pos: 0,
                unplugged: false,
                hanging_up: false,
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
            host: Arc::new(Wakeup::new()),
            device: Arc::new(Wakeup::new()),
            exit: Arc::new(Wakeup::new()),
        });

        let thread_shared = Arc::clone(&shared);
        let spawned = thread::Builder::new()
            .name(format!("serialist-sim-{}", shared.name))
            .spawn(move || run_device(&thread_shared, device));
        if let Err(err) = spawned {
            tracing::error!(%err, device = %shared.name, "could not start the simulated device thread");
            let mut st = shared.state.lock();
            st.pull_cable(shared.clock.now());
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

    /// Pull the cable. Bytes already released to the host are still returned by `read`,
    /// then `Err(TransportError::Disconnected)`, so the host sees the disconnect at once
    /// rather than after the queue drains. Bytes still in flight or in the device's FIFO
    /// are lost and counted in [`LinkStats::lost_on_unplug`]. Writes fail from now on.
    /// Idempotent.
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
        self.shared.host.notify();
        self.shared.device.notify();
    }

    /// The level the host last set on `line`. Both start asserted.
    pub fn control_line(&self, line: ControlLine) -> bool {
        let st = self.shared.state.lock();
        match line {
            ControlLine::Dtr => st.dtr,
            ControlLine::Rts => st.rts,
        }
    }

    /// Bytes the device has sent that the host has not read yet, including those still
    /// in the device's transmit FIFO. Bounded by [`HOST_BACKLOG_LIMIT`] plus one device
    /// callback's output.
    pub fn queued_to_host(&self) -> usize {
        let st = self.shared.state.lock();
        st.to_host.queued + st.tx_pending()
    }

    pub fn is_device_running(&self) -> bool {
        self.shared.state.lock().device_running
    }

    /// Wait for the device thread to finish. Returns whether it did within `timeout`,
    /// which runs on the link's clock.
    pub fn wait_for_device_exit(&self, timeout: Duration) -> bool {
        let shared = &*self.shared;
        let deadline = later(shared.clock.now(), timeout.min(MAX_WAIT));
        let mut st = shared.state.lock();
        loop {
            if !st.device_running {
                return true;
            }
            if shared.clock.now() >= deadline {
                return false;
            }
            shared.sleep(&mut st, &shared.exit, Some(deadline));
        }
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
        let deadline = later(shared.clock.now(), timeout.min(MAX_WAIT));
        let mut st = shared.state.lock();
        st.stats.host_read_calls += 1;
        loop {
            let now = shared.clock.now();
            let limit = buf.len().min(st.cfg.max_chunk.max(1));
            let n = st.to_host.read_ready(now, &mut buf[..limit]);
            if n > 0 {
                if st.device_waiting_for_drain && st.to_host.queued < RESUME_BACKLOG {
                    st.device_waiting_for_drain = false;
                    shared.device.notify();
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
            shared.sleep(&mut st, &shared.host, Some(wake));
        }
    }
}

impl Drop for VirtualReader {
    fn drop(&mut self) {
        self.shared.state.lock().reader_open = false;
        self.shared.device.notify();
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
        if guard.device_gone() {
            return Err(TransportError::Disconnected);
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let st = &mut *guard;
        st.stats.host_to_device_bytes += bytes.len() as u64;
        st.to_device
            .push(bytes, self.shared.clock.now(), &st.cfg, &mut st.stats);
        self.shared.device.notify();
        Ok(())
    }

    fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError> {
        let mut st = self.shared.state.lock();
        if st.device_gone() {
            return Err(TransportError::Disconnected);
        }
        match line {
            ControlLine::Dtr => st.dtr = asserted,
            ControlLine::Rts => st.rts = asserted,
        }
        st.controls.push_back((line, asserted));
        self.shared.device.notify();
        Ok(())
    }

    /// Changes the pacing rate live for both directions. Bytes already on the wire keep
    /// their schedule, so the new rate shows within one [`PACED_LOOKAHEAD`].
    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
        if config.baud == 0 {
            return Err(TransportError::Config(
                "baud must be greater than zero".into(),
            ));
        }
        let mut st = self.shared.state.lock();
        if st.device_gone() {
            return Err(TransportError::Disconnected);
        }
        st.cfg.serial = config.clone();
        self.shared.device.notify();
        Ok(())
    }
}

impl Drop for VirtualWriter {
    fn drop(&mut self) {
        self.shared.state.lock().writer_open = false;
        self.shared.device.notify();
    }
}

/// Device output buffered during one callback, handed to the link afterwards so device
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
    /// Move this callback's output into the device FIFO and start it onto the wire.
    /// Returns false once the link is down and the device thread should stop.
    fn flush(&mut self, shared: &Shared) -> bool {
        if self.data.is_empty() && !self.disconnect {
            return true;
        }
        let mut st = shared.state.lock();
        if st.device_gone() {
            self.data.clear();
            return false;
        }
        st.enqueue(&mut self.data);
        if std::mem::take(&mut self.disconnect) {
            st.hanging_up = true;
        }
        if st.pump(shared.clock.now()) > 0 {
            shared.host.notify();
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
            // A finished hang-up keeps its in-flight bytes; any other exit pulls the cable.
            if !st.unplugged {
                st.pull_cable(shared.clock.now());
            }
        }
        if thread::panicking() {
            tracing::error!(device = %shared.name, "simulated device panicked; link unplugged");
        }
        shared.host.notify();
        shared.device.notify();
        shared.exit.notify();
    }
}

fn run_device(shared: &Shared, mut device: Box<dyn SimDevice>) {
    let _exit = DeviceExit(shared);
    let mut out = Outbox::default();
    device.on_connect(&mut out);
    if !out.flush(shared) {
        return;
    }
    let mut next_tick = Some(shared.clock.now());
    while let Some(work) = wait_for_work(shared, next_tick) {
        for (line, asserted) in work.controls {
            device.on_control(line, asserted, &mut out);
        }
        for chunk in &work.incoming {
            device.on_receive(chunk, &mut out);
        }
        if work.tick.is_some() {
            next_tick = device.on_tick(shared.clock.now(), &mut out);
        } else if !work.incoming.is_empty() && next_tick.is_none() {
            // A sleeping device gets one tick after new data so it can schedule itself.
            next_tick = Some(shared.clock.now());
        }
        if !out.flush(shared) {
            return;
        }
    }
}

/// Sleep until the device has something to do, keeping its FIFO flowing onto the wire
/// meanwhile. `None` means the thread should exit.
fn wait_for_work(shared: &Shared, next_tick: Option<Instant>) -> Option<Work> {
    let mut st = shared.state.lock();
    loop {
        if st.unplugged || st.host_gone() {
            return None;
        }
        let now = shared.clock.now();
        if st.pump(now) > 0 {
            shared.host.notify();
        }
        if st.hanging_up && st.tx_pending() == 0 {
            st.finish_hang_up();
            shared.host.notify();
            return None;
        }

        let tick_due = next_tick.is_some_and(|t| t <= now);
        let mut wake = None;
        let mut may_tick = false;
        if st.tx_pending() > 0 || tick_due {
            match st.output(now) {
                Output::Ready if st.tx_pending() == 0 => may_tick = true,
                // The FIFO is non-empty although the wire has room: float rounding in
                // `pump`. Retry shortly rather than spin.
                Output::Ready => wake = earliest(wake, later(now, PACKET_INTERVAL)),
                Output::Until(t) => wake = earliest(wake, t),
                Output::UntilDrained => st.device_waiting_for_drain = true,
            }
        }

        // A device that is hanging up only drains its FIFO; it takes no input or ticks.
        if !st.hanging_up {
            if let Some(t) = next_tick
                && t > now
            {
                wake = earliest(wake, t);
            }
            let tick = (tick_due && may_tick).then_some(now);
            let incoming = st.to_device.take_ready_packets(now);
            if let Some(t) = st.to_device.next_release() {
                wake = earliest(wake, t);
            }
            let controls: Vec<_> = st.controls.drain(..).collect();
            if tick.is_some() || !incoming.is_empty() || !controls.is_empty() {
                return Some(Work {
                    controls,
                    incoming,
                    tick,
                });
            }
        }

        shared.sleep(&mut st, &shared.device, wake);
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

    fn state(cfg: LinkConfig, now: Instant) -> LinkState {
        LinkState {
            to_host: Wire::new(now, cfg.seed, 0),
            to_device: Wire::new(now, cfg.seed, 2),
            cfg,
            tx_fifo: Vec::new(),
            tx_pos: 0,
            unplugged: false,
            hanging_up: false,
            reader_open: true,
            writer_open: true,
            device_running: true,
            device_waiting_for_drain: false,
            dtr: true,
            rts: true,
            controls: VecDeque::new(),
            stats: LinkStats::default(),
        }
    }

    #[test]
    fn paced_schedule_is_exact() {
        // 1 Mbaud 8N1 is 100_000 bytes/s: 100 bytes per 1 ms packet.
        let cfg = paced(1_000_000);
        let t0 = SystemClock.now();
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

    /// Each packet's size and release time, for comparing schedules.
    fn schedule(wire: &Wire) -> Vec<(usize, Instant)> {
        wire.packets
            .iter()
            .map(|p| (p.data.len(), p.release_at))
            .collect()
    }

    #[test]
    fn packets_do_not_depend_on_how_bytes_were_batched() {
        // 115_200 baud 8N1 is 11_520 bytes/s: 12-byte packets of 1.04 ms each.
        let cfg = paced(115_200);
        let t0 = SystemClock.now();
        let run = |batches: &[usize]| {
            let mut wire = Wire::new(t0, 0, 0);
            let mut stats = LinkStats::default();
            let mut now = t0;
            for &n in batches {
                wire.push(&vec![0u8; n], now, &cfg, &mut stats);
                // Offered again while the wire is still busy.
                now += Duration::from_millis(3);
            }
            wire
        };
        let whole = run(&[1_200]);
        assert_eq!(whole.packets.len(), 100);
        assert_eq!(schedule(&run(&[360, 96, 744])), schedule(&whole));
        // A batch that ends mid-packet splits that one packet in two; every packet still
        // ends where it would have.
        let split = run(&[368, 88, 744]);
        assert_eq!(split.packets.len(), 101);
        let ends: Vec<_> = split.packets.iter().map(|p| p.release_at).collect();
        assert!(whole.packets.iter().all(|p| ends.contains(&p.release_at)));
        assert_eq!(split.wire_free_at, whole.wire_free_at);
    }

    #[test]
    fn pumping_at_any_times_gives_the_same_packets() {
        let t0 = SystemClock.now();
        let pumped = |at_ms: &[u64]| {
            let mut st = state(paced(115_200), t0);
            st.enqueue(&mut vec![0u8; 16 * 1024]);
            for &ms in at_ms {
                st.pump(t0 + Duration::from_millis(ms));
            }
            st
        };
        let a = pumped(&[0, 8, 16, 24, 32, 40]);
        let b = pumped(&[0, 1, 2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 40]);
        // Both refilled to the same horizon, in whole packets.
        assert_eq!(a.stats.device_to_host_bytes, b.stats.device_to_host_bytes);
        assert_eq!(a.stats.device_to_host_bytes % 12, 0);
        assert_eq!(schedule(&a.to_host), schedule(&b.to_host));
    }

    #[test]
    fn a_rate_change_starts_a_new_run_where_the_old_one_ends() {
        let t0 = SystemClock.now();
        let mut st = state(paced(1_000_000), t0);
        st.enqueue(&mut vec![0u8; 50_000]);
        assert_eq!(st.pump(t0), 3_200);
        st.cfg.serial.baud = 500_000;
        // The wire is full to the horizon; the new rate starts when it frees up.
        assert_eq!(st.pump(t0), 0);
        assert_eq!(st.pump(t0 + Duration::from_millis(4)), 200);
        let last = st.to_host.packets.back().unwrap();
        assert_eq!(last.data.len(), 50);
        assert_eq!(last.release_at - t0, Duration::from_millis(36));
    }

    #[test]
    fn pump_commits_at_most_one_lookahead() {
        // 1 Mbaud: 100 bytes/ms, so 32 ms of look-ahead is 3_200 bytes.
        let t0 = SystemClock.now();
        let mut st = state(paced(1_000_000), t0);
        st.enqueue(&mut vec![7u8; 50_000]);
        assert_eq!(st.pump(t0), 3_200);
        assert_eq!(st.tx_pending(), 46_800);
        assert_eq!(st.stats.device_to_host_bytes, 3_200);
        // Nothing more fits right now; the device thread refills once a quarter of the
        // look-ahead has gone out, topping the wire back up to the horizon.
        assert_eq!(st.pump(t0), 0);
        assert!(matches!(st.output(t0), Output::Until(t) if t - t0 == Duration::from_millis(8)));
        assert_eq!(st.pump(t0 + Duration::from_millis(8)), 800);
        assert_eq!(st.to_host.wire_free_at - t0, Duration::from_millis(40));
    }

    #[test]
    fn slow_links_still_commit_whole_bytes() {
        // 300 baud 8N1 is 30 bytes/s; four bytes take longer than the usual look-ahead.
        let t0 = SystemClock.now();
        let mut st = state(paced(300), t0);
        st.enqueue(&mut vec![1u8; 100]);
        assert_eq!(st.pump(t0), 4);
        let Output::Until(resume) = st.output(t0) else {
            panic!("wire should be busy");
        };
        // Refill after one byte time, before the wire goes idle.
        assert!(resume < st.to_host.wire_free_at);
        assert_eq!(st.pump(resume), 1);
    }

    #[test]
    fn pulling_the_cable_keeps_released_bytes_only() {
        let t0 = SystemClock.now();
        let mut st = state(paced(1_000_000), t0);
        st.enqueue(&mut vec![0u8; 50_000]);
        st.pump(t0);
        // At 10 ms, 1_000 bytes are released and 2_200 are still in flight.
        st.pull_cable(t0 + Duration::from_millis(10));
        assert_eq!(st.to_host.queued, 1_000);
        assert_eq!(st.stats.lost_on_unplug, 2_200);
        assert_eq!(st.tx_pending(), 0);
        assert!(st.unplugged);
    }

    #[test]
    fn unpaced_pump_respects_the_backlog_limit() {
        let t0 = SystemClock.now();
        let mut st = state(LinkConfig::unpaced(), t0);
        st.enqueue(&mut vec![0u8; HOST_BACKLOG_LIMIT + 5]);
        assert_eq!(st.pump(t0), HOST_BACKLOG_LIMIT);
        assert!(matches!(st.output(t0), Output::UntilDrained));
        assert_eq!(st.tx_pending(), 5);
    }

    #[test]
    fn absurd_durations_saturate() {
        let t0 = SystemClock.now();
        assert!(later(t0, Duration::MAX) > t0);
        let cfg = LinkConfig {
            latency: Duration::MAX,
            jitter: Duration::MAX,
            ..paced(1)
        };
        let mut wire = Wire::new(t0, 0, 0);
        wire.push(&[0u8; 1000], t0, &cfg, &mut LinkStats::default());
        assert!(wire.next_release().unwrap() > t0);
    }

    #[test]
    fn packets_respect_max_chunk_and_latency() {
        let mut cfg = LinkConfig::unpaced();
        cfg.max_chunk = 7;
        cfg.latency = Duration::from_millis(3);
        let t0 = SystemClock.now();
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
        let t0 = SystemClock.now();
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
            let t0 = SystemClock.now();
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
