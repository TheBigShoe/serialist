//! The file-replay transport: `replay:<path>` ids play a recorded capture (see
//! [`crate::capture`]) back into a session as if a device were sending it.
//!
//! # Pacing
//!
//! The options in effect are the id's own (`?speed=…&end=…`), else the factory's
//! [`defaults`](ReplayTransportFactory::defaults) (the app's settings), else
//! [`ReplayOptions::default`] (1x, disconnect at the end).
//!
//! - **With a timing sidecar** the reader follows [`Timing::schedule`](crate::capture::Timing::schedule):
//!   chunk `i` is due `speed.scale(chunk.at)` after the open, measured on the factory's
//!   [`Clock`]. Each read returns at most one recorded chunk (split only when it is
//!   larger than the read buffer, the rest due at once), so chunk boundaries replay
//!   exactly. A reader that falls behind gets the overdue chunks back to back; the
//!   schedule is anchored to the open, so lateness never accumulates.
//! - **Without a sidecar** bytes go out at the session's baud rate times the speed
//!   factor (`SerialConfig::bytes_per_second`). That rate is a budget which starts at
//!   the open and grows with time whether or not anyone is reading. A read delivers
//!   what the budget allows, but only once a whole batch is allowed: a batch is what the
//!   rate delivers in [`REPLAY_PACE_TICK`] (at least one byte), or what is left of the
//!   file or of the read buffer if that is less. So a reader that keeps up gets a chunk
//!   every tick or so, and one that fell behind catches up in a single read. A
//!   `reconfigure` with a new baud changes the rate at the instant of the reconfigure,
//!   not at the reader's next read: the budget earned so far is kept, and later time
//!   earns at the new rate.
//! - **`speed=max`** ignores time either way: every read returns the next chunk (with
//!   a sidecar) or a full buffer, or what is left of the file (without) at once.
//! - A read waits on the clock for at most its timeout and returns `Ok(0)` if nothing is
//!   due by then, so the session's stop flag is polled as usual and nothing spins.
//!
//! The raw file is read front to back through a buffer and never loaded whole. Its
//! length is taken at the open; a capture that grows afterwards plays only what was
//! there, and one that shrinks fails the read with [`TransportError::Io`].
//!
//! # End of the capture
//!
//! [`ReplayEnd::Disconnect`]: the read after the last byte fails with
//! [`TransportError::Disconnected`]. [`ReplayEnd::Hold`]: reads keep returning `Ok(0)`
//! after waiting their timeout, until the session closes.
//!
//! # Everything else
//!
//! - Writes are accepted and discarded (a capture has nobody to answer), so typing or
//!   a script's `port:write` against a replay is harmless.
//! - DTR, RTS and reconfigure are accepted (reconfigure only matters without a
//!   sidecar, above); break is `Unsupported`.
//! - Open fails with `NotFound` if the raw file does not exist, `Config` for a
//!   malformed id or a sidecar that does not parse (the message names the port), and
//!   `Io` otherwise (a directory, say). A missing sidecar is not an error.
//! - Reconnect opens the capture again and plays it from the start.
//! - The description is `replay:<file name> (<speed>[, no timing])`, for example
//!   `replay:boot.bin (1x)` or `replay:dump.bin (4x, no timing)`.

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::address::{ReplayAddress, ReplayEnd, ReplaySpeed};
use crate::capture::{ScheduledChunk, Timing, TimingError};
use crate::clock::{Clock, SystemClock, Wakeup};
use crate::config::SerialConfig;
use crate::port::PortId;
use crate::transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};

/// How much of the line rate one batch holds when a capture without a timing sidecar is
/// paced at the baud rate: a read delivers once the rate has earned this much time's
/// worth of bytes (at least one byte), so chunks are not a byte each.
pub const REPLAY_PACE_TICK: Duration = Duration::from_millis(10);

/// Read-ahead on the raw file. Chunks are small, so most reads are served from memory.
const FILE_BUFFER: usize = 64 * 1024;

/// How a capture plays back. See the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReplayOptions {
    pub speed: ReplaySpeed,
    pub end: ReplayEnd,
}

/// Opens `replay:<path>` ids.
///
/// The defaults are shared by every open and can change at any time (the app updates
/// them when the settings change); a replay already open keeps the options it opened
/// with.
pub struct ReplayTransportFactory {
    clock: Arc<dyn Clock>,
    defaults: Mutex<ReplayOptions>,
}

impl ReplayTransportFactory {
    /// Paced in real time.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Paced on `clock`; tests pass a `serialist_sim::ManualClock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            defaults: Mutex::new(ReplayOptions::default()),
        }
    }

    /// The clock replays are paced on.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The options an id without its own get.
    pub fn defaults(&self) -> ReplayOptions {
        *self.defaults.lock()
    }

    pub fn set_defaults(&self, options: ReplayOptions) {
        *self.defaults.lock() = options;
    }

    /// The options a replay of `address` opens with: its own, else the defaults.
    pub fn options_for(&self, address: &ReplayAddress) -> ReplayOptions {
        let defaults = self.defaults();
        ReplayOptions {
            speed: address.speed.unwrap_or(defaults.speed),
            end: address.end.unwrap_or(defaults.end),
        }
    }
}

impl Default for ReplayTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ReplayTransportFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayTransportFactory")
            .field("defaults", &self.defaults())
            .finish_non_exhaustive()
    }
}

impl TransportFactory for ReplayTransportFactory {
    fn open(&self, port: &PortId, config: &SerialConfig) -> Result<Transport, TransportError> {
        let address = ReplayAddress::from_port_id(port)
            .map_err(|err| TransportError::Config(err.to_string()))?;
        let options = self.options_for(&address);

        // Metadata before the open: Windows refuses to open a directory with
        // PermissionDenied, while Unix opens it and fails on the first read, so the
        // directory check has to come first to answer the same way everywhere.
        let metadata = std::fs::metadata(&address.path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => TransportError::NotFound(port.clone()),
            _ => io_error(port, &err),
        })?;
        if metadata.is_dir() {
            return Err(TransportError::Io(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{port}: a replay port id names a capture file, not a directory"),
            )));
        }
        let file = File::open(&address.path).map_err(|err| match err.kind() {
            io::ErrorKind::NotFound => TransportError::NotFound(port.clone()),
            _ => io_error(port, &err),
        })?;
        let raw_len = metadata.len();

        let schedule = match Timing::read_file(&address.timing_path()) {
            Ok(timing) => Some(timing.schedule(raw_len)),
            Err(TimingError::Io(err)) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(TransportError::Config(format!("{port}: {err}"))),
        };

        let start = self.clock.now();
        let name = address.path.file_name().map_or_else(
            || address.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let tail = if schedule.is_some() {
            ""
        } else {
            ", no timing"
        };
        let description = format!("replay:{name} ({}{tail})", options.speed);
        tracing::debug!(%port, ?options, raw_len, timed = schedule.is_some(), "opened replay");

        let pacing = Arc::new(Mutex::new(Pacing {
            serial: config.clone(),
            changed_at: start,
            generation: 0,
        }));
        let plan = match schedule {
            Some(chunks) => Plan::Scheduled(Scheduled {
                chunks,
                next: 0,
                sent: 0,
            }),
            None => {
                let rate = options
                    .speed
                    .factor()
                    .map_or(0.0, |factor| config.bytes_per_second() * factor);
                Plan::Paced(Paced {
                    raw_len,
                    sent: 0,
                    anchor_time: start,
                    anchor_sent: 0,
                    generation: 0,
                    rate,
                })
            }
        };

        Ok(Transport {
            reader: Box::new(ReplayReader {
                file: BufReader::with_capacity(FILE_BUFFER, file),
                clock: Arc::clone(&self.clock),
                wakeup: Arc::new(Wakeup::new()),
                start,
                speed: options.speed,
                end: options.end,
                plan,
                pacing: Arc::clone(&pacing),
            }),
            writer: Box::new(ReplayWriter {
                clock: Arc::clone(&self.clock),
                pacing,
            }),
            description,
        })
    }
}

/// An I/O error from opening `port`, with the port named in its message.
fn io_error(port: &PortId, err: &io::Error) -> TransportError {
    TransportError::Io(io::Error::new(err.kind(), format!("{port}: {err}")))
}

/// `t + d`, saturating far in the future instead of panicking on overflow.
fn later(t: Instant, d: Duration) -> Instant {
    const FAR: Duration = Duration::from_secs(100 * 365 * 24 * 3600);
    t.checked_add(d).or_else(|| t.checked_add(FAR)).unwrap_or(t)
}

/// What the writer tells the reader about the line settings. Only a capture without a
/// timing sidecar uses them: its pace is the baud rate.
struct Pacing {
    serial: SerialConfig,
    /// When the writer last applied `serial`, on the factory's clock.
    changed_at: Instant,
    /// Moves on with every `reconfigure`, so the reader notices. If the writer
    /// reconfigures twice before the reader looks, the reader sees only the last one:
    /// the time in between is paced at the old rate, an error of one read interval.
    generation: u64,
}

/// What the reader finds when it looks at its plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// This many bytes (at least one) can go out now.
    Now(usize),
    /// Nothing before this instant, which is later than `now`.
    At(Instant),
    /// Nothing ever, a line rate of zero: wait out the timeout.
    Never,
    /// Every byte has been delivered.
    Finished,
}

enum Plan {
    Scheduled(Scheduled),
    Paced(Paced),
}

/// The chunks of a capture with a sidecar, in order.
struct Scheduled {
    chunks: Vec<ScheduledChunk>,
    /// The chunk being delivered.
    next: usize,
    /// How much of it has gone out already.
    sent: u64,
}

impl Scheduled {
    fn poll(&self, start: Instant, speed: ReplaySpeed, now: Instant, cap: usize) -> Due {
        let Some(chunk) = self.chunks.get(self.next) else {
            return Due::Finished;
        };
        // The rest of a split chunk was due when its first part went out, so it never
        // waits again.
        let due = speed
            .scale(chunk.at)
            .map_or(start, |after| later(start, after));
        if now < due {
            return Due::At(due);
        }
        let left = chunk.len - self.sent;
        Due::Now(left.min(cap as u64) as usize)
    }

    fn advance(&mut self, n: usize) {
        self.sent += n as u64;
        if self.sent >= self.chunks[self.next].len {
            self.next += 1;
            self.sent = 0;
        }
    }
}

/// A capture without a sidecar, paced at the line rate.
///
/// `allowed = anchor_sent + floor((now - anchor_time) * rate)` is how many bytes the
/// pace has earned. The anchor moves forward when the rate changes, so each segment of
/// time is earned at the rate in force during it.
struct Paced {
    raw_len: u64,
    sent: u64,
    anchor_time: Instant,
    anchor_sent: u64,
    /// The `Pacing::generation` the rate below is for.
    generation: u64,
    /// Bytes per second at the current line settings and speed.
    rate: f64,
}

impl Paced {
    fn poll(
        &mut self,
        pacing: &Mutex<Pacing>,
        factor: Option<f64>,
        now: Instant,
        cap: usize,
    ) -> Due {
        let remaining = self.raw_len - self.sent;
        if remaining == 0 {
            return Due::Finished;
        }
        let cap = remaining.min(cap as u64);
        let Some(factor) = factor else {
            return Due::Now(cap as usize);
        };

        self.follow(pacing, factor);
        if !self.rate.is_finite() || self.rate <= 0.0 {
            return Due::Never;
        }
        let batch = ((self.rate * REPLAY_PACE_TICK.as_secs_f64()).floor() as u64).max(1);
        let needed = cap.min(batch);
        let allowed = self.anchor_sent.saturating_add(budget(
            now.saturating_duration_since(self.anchor_time),
            self.rate,
        ));
        let available = allowed.saturating_sub(self.sent);
        if available >= needed {
            return Due::Now(available.min(cap) as usize);
        }
        let until = time_to_earn(
            (self.sent + needed).saturating_sub(self.anchor_sent),
            self.rate,
        );
        // The estimate rounds up to the nanosecond, so it is never early; the floor
        // only guarantees that a wait always ends in the future, whatever the
        // arithmetic did.
        Due::At(later(self.anchor_time, until).max(later(now, Duration::from_nanos(1))))
    }

    /// Pick up a reconfigure the writer made: close the segment at the rate it was
    /// earned at, up to the instant of the change, and start earning at the new rate.
    fn follow(&mut self, pacing: &Mutex<Pacing>, factor: f64) {
        let pacing = pacing.lock();
        if pacing.generation == self.generation {
            return;
        }
        let changed_at = pacing.changed_at.max(self.anchor_time);
        let earned = budget(changed_at - self.anchor_time, self.rate);
        self.anchor_sent = self.anchor_sent.saturating_add(earned);
        self.anchor_time = changed_at;
        self.generation = pacing.generation;
        self.rate = pacing.serial.bytes_per_second() * factor;
    }

    fn advance(&mut self, n: usize) {
        self.sent += n as u64;
    }
}

/// Whole bytes `rate` bytes per second earns in `elapsed`. Multiplies before dividing,
/// so a whole number of bytes comes out whole.
fn budget(elapsed: Duration, rate: f64) -> u64 {
    (elapsed.as_nanos() as f64 * rate / 1e9).floor() as u64
}

/// The shortest time, rounded up to the nanosecond, in which `rate` bytes per second
/// earns `bytes`.
fn time_to_earn(bytes: u64, rate: f64) -> Duration {
    let nanos = (bytes as f64 * 1e9 / rate).ceil();
    if nanos < u64::MAX as f64 {
        Duration::from_nanos(nanos as u64)
    } else {
        Duration::MAX
    }
}

struct ReplayReader {
    file: BufReader<File>,
    clock: Arc<dyn Clock>,
    /// Waited on, never notified: the reader waits for time alone.
    wakeup: Arc<Wakeup>,
    /// When the replay opened, on `clock`. Every schedule is anchored to it.
    start: Instant,
    speed: ReplaySpeed,
    end: ReplayEnd,
    plan: Plan,
    pacing: Arc<Mutex<Pacing>>,
}

impl ReplayReader {
    fn poll(&mut self, now: Instant, cap: usize) -> Due {
        match &mut self.plan {
            Plan::Scheduled(schedule) => schedule.poll(self.start, self.speed, now, cap),
            Plan::Paced(paced) => paced.poll(&self.pacing, self.speed.factor(), now, cap),
        }
    }

    fn advance(&mut self, n: usize) {
        match &mut self.plan {
            Plan::Scheduled(schedule) => schedule.advance(n),
            Plan::Paced(paced) => paced.advance(n),
        }
    }
}

impl TransportReader for ReplayReader {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = later(self.clock.now(), timeout);
        loop {
            // Waits can end early, so the time is read afresh on every pass.
            let now = self.clock.now();
            let wake = match self.poll(now, buf.len()) {
                Due::Now(n) => {
                    self.file.read_exact(&mut buf[..n])?;
                    self.advance(n);
                    return Ok(n);
                }
                Due::Finished if self.end == ReplayEnd::Disconnect => {
                    return Err(TransportError::Disconnected);
                }
                Due::Finished | Due::Never => deadline,
                Due::At(due) => due.min(deadline),
            };
            if now >= deadline {
                return Ok(0);
            }
            self.clock
                .wait(&self.wakeup, self.wakeup.epoch(), Some(wake));
        }
    }
}

/// Accepts everything and does nothing, except remember the line settings.
struct ReplayWriter {
    clock: Arc<dyn Clock>,
    pacing: Arc<Mutex<Pacing>>,
}

impl TransportWriter for ReplayWriter {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        tracing::trace!(len = bytes.len(), "replay discards a write");
        Ok(())
    }

    fn set_control(&mut self, _line: ControlLine, _asserted: bool) -> Result<(), TransportError> {
        Ok(())
    }

    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
        let at = self.clock.now();
        let mut pacing = self.pacing.lock();
        pacing.serial = config.clone();
        pacing.changed_at = at;
        pacing.generation = pacing.generation.wrapping_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::test_util::TempDir;

    const MS: Duration = Duration::from_millis(1);

    fn open(path: &Path, query: &str) -> Result<Transport, TransportError> {
        let id = PortId::new(format!("replay:{}{query}", path.display()));
        ReplayTransportFactory::new().open(&id, &SerialConfig::default())
    }

    fn open_err(path: &Path, query: &str) -> TransportError {
        match open(path, query) {
            Err(err) => err,
            Ok(transport) => panic!("opened as {}", transport.description),
        }
    }

    fn chunk(at_ms: u64, offset: u64, len: u64) -> ScheduledChunk {
        ScheduledChunk {
            at: Duration::from_millis(at_ms),
            offset,
            len,
        }
    }

    #[test]
    fn options_come_from_the_id_then_the_defaults() {
        let factory = ReplayTransportFactory::new();
        let bare = ReplayAddress::new("/c/boot.bin");
        assert_eq!(factory.options_for(&bare), ReplayOptions::default());

        factory.set_defaults(ReplayOptions {
            speed: ReplaySpeed::Max,
            end: ReplayEnd::Hold,
        });
        assert_eq!(factory.options_for(&bare).speed, ReplaySpeed::Max);
        let own = bare.with_speed(ReplaySpeed::Times(2.0));
        assert_eq!(
            factory.options_for(&own),
            ReplayOptions {
                speed: ReplaySpeed::Times(2.0),
                end: ReplayEnd::Hold
            }
        );
    }

    #[test]
    fn open_checks_the_id_then_the_file() {
        let dir = TempDir::new("replay-open");
        let missing = dir.path().join("missing.bin");
        assert!(
            matches!(open_err(&missing, ""), TransportError::NotFound(id)
                if id.as_str().ends_with("missing.bin"))
        );
        // The id is checked before the file is looked for.
        assert!(matches!(
            open_err(&missing, "?speed=fast"),
            TransportError::Config(_)
        ));
        match open_err(dir.path(), "") {
            TransportError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::IsADirectory),
            other => panic!("a directory is not a capture: {other:?}"),
        }
    }

    #[test]
    fn the_description_names_the_file_speed_and_sidecar() {
        let dir = TempDir::new("replay-description");
        let raw = dir.write("dump.bin", "abc");
        assert_eq!(
            open(&raw, "?speed=4x").unwrap().description,
            "replay:dump.bin (4x, no timing)"
        );
        dir.write("dump.bin.timing", "serialist-timing 1\nrx 0 0 3\n");
        assert_eq!(open(&raw, "").unwrap().description, "replay:dump.bin (1x)");
        assert_eq!(
            open(&raw, "?speed=0.5x").unwrap().description,
            "replay:dump.bin (0.5x)"
        );
        assert_eq!(
            open(&raw, "?speed=max").unwrap().description,
            "replay:dump.bin (max)"
        );
    }

    #[test]
    fn a_sidecar_that_does_not_parse_is_a_config_error_naming_the_port() {
        let dir = TempDir::new("replay-bad-sidecar");
        let raw = dir.write("boot.bin", "abc");
        dir.write("boot.bin.timing", "not a sidecar\n");
        match open_err(&raw, "") {
            TransportError::Config(message) => assert!(message.contains("boot.bin"), "{message}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn the_length_is_taken_at_the_open() {
        let dir = TempDir::new("replay-length");
        let raw = dir.write("grow.bin", "0123456789");
        let mut grows = open(&raw, "?speed=max").unwrap();
        fs::write(&raw, "0123456789 and more").unwrap();
        let mut buf = [0u8; 64];
        let n = grows.reader.read(&mut buf, Duration::ZERO).unwrap();
        assert_eq!(&buf[..n], b"0123456789", "growth after the open is ignored");
        assert!(matches!(
            grows.reader.read(&mut buf, Duration::ZERO),
            Err(TransportError::Disconnected)
        ));

        let raw = dir.write("shrink.bin", "0123456789");
        let mut shrinks = open(&raw, "?speed=max").unwrap();
        fs::write(&raw, "0123").unwrap();
        match shrinks.reader.read(&mut buf, Duration::ZERO) {
            Err(TransportError::Io(err)) => assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("a capture that shrank fails the read, got {other:?}"),
        }
    }

    #[test]
    fn a_rate_earns_whole_bytes_and_the_estimate_is_never_early() {
        assert_eq!(budget(100 * MS, 960.0), 96);
        assert_eq!(budget(Duration::ZERO, 960.0), 0);
        assert_eq!(budget(Duration::from_nanos(1), 960.0), 0);
        assert_eq!(time_to_earn(9, 960.0), Duration::from_nanos(9_375_000));
        for rate in [960.0, 11_520.0, 8_727.272_727, 46_080.0, 0.7, 1.0e7] {
            for bytes in [1u64, 2, 9, 100, 4096] {
                let wait = time_to_earn(bytes, rate);
                assert!(
                    budget(wait, rate) >= bytes,
                    "{bytes} bytes at {rate} B/s are earned by {wait:?}"
                );
            }
        }
        assert_eq!(time_to_earn(1, 1.0e-300), Duration::MAX);
    }

    #[test]
    fn a_schedule_splits_a_chunk_and_never_waits_for_the_rest() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let speed = ReplaySpeed::REALTIME;
        let mut plan = Scheduled {
            chunks: vec![chunk(0, 0, 10), chunk(10, 10, 3)],
            next: 0,
            sent: 0,
        };
        assert_eq!(plan.poll(start, speed, at(0), 4), Due::Now(4));
        plan.advance(4);
        assert_eq!(plan.poll(start, speed, at(0), 4), Due::Now(4));
        plan.advance(4);
        assert_eq!(plan.poll(start, speed, at(0), 4), Due::Now(2));
        plan.advance(2);
        assert_eq!(plan.poll(start, speed, at(9), 4), Due::At(at(10)));
        // A late reader gets the chunk, and the schedule does not move.
        assert_eq!(plan.poll(start, speed, at(500), 64), Due::Now(3));
        plan.advance(3);
        assert_eq!(plan.poll(start, speed, at(500), 64), Due::Finished);

        let plan = Scheduled {
            chunks: vec![chunk(10, 0, 3)],
            next: 0,
            sent: 0,
        };
        assert_eq!(
            plan.poll(start, ReplaySpeed::Times(2.0), at(0), 64),
            Due::At(at(5))
        );
        assert_eq!(
            plan.poll(start, ReplaySpeed::Max, at(0), 64),
            Due::Now(3),
            "max ignores the schedule"
        );
    }

    #[test]
    fn a_paced_replay_earns_a_batch_and_waits_for_the_next() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let nanos = |n: u64| start + Duration::from_nanos(n);
        let serial = SerialConfig {
            baud: 9600,
            ..SerialConfig::default()
        };
        let pacing = Mutex::new(Pacing {
            serial: serial.clone(),
            changed_at: start,
            generation: 0,
        });
        let mut paced = Paced {
            raw_len: 1000,
            sent: 0,
            anchor_time: start,
            anchor_sent: 0,
            generation: 0,
            rate: serial.bytes_per_second(),
        };
        let one = Some(1.0);
        // 960 B/s: a batch is 9 bytes, earned 9.375 ms in.
        assert_eq!(
            paced.poll(&pacing, one, at(0), 64),
            Due::At(nanos(9_375_000))
        );
        assert_eq!(
            paced.poll(&pacing, one, at(9), 64),
            Due::At(nanos(9_375_000))
        );
        assert_eq!(paced.poll(&pacing, one, at(10), 64), Due::Now(9));
        // A small buffer needs only its own size earned.
        assert_eq!(
            paced.poll(&pacing, one, at(0), 4),
            Due::At(nanos(4_166_667))
        );
        assert_eq!(paced.poll(&pacing, one, at(5), 4), Due::Now(4));
        // The budget grows whether or not it is read: all of it goes out at once.
        assert_eq!(paced.poll(&pacing, one, at(100), 200), Due::Now(96));
        paced.advance(96);
        assert_eq!(
            paced.poll(&pacing, one, at(100), 200),
            Due::At(nanos(109_375_000)),
            "the next batch of 9 is earned 105 bytes in"
        );
        // Four times as fast: 3840 B/s, a batch of 38 bytes.
        let mut fast = Paced {
            rate: serial.bytes_per_second() * 4.0,
            sent: 0,
            ..paced
        };
        assert_eq!(fast.poll(&pacing, Some(4.0), at(10), 1000), Due::Now(38));
        // Max never waits and never looks at the line settings.
        assert_eq!(fast.poll(&pacing, None, at(0), 1000), Due::Now(1000));
        assert_eq!(fast.poll(&pacing, None, at(0), 5000), Due::Now(1000));
    }

    #[test]
    fn a_baud_change_keeps_the_budget_earned_before_it() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let serial = SerialConfig {
            baud: 9600,
            ..SerialConfig::default()
        };
        let pacing = Mutex::new(Pacing {
            serial: serial.clone(),
            changed_at: start,
            generation: 0,
        });
        let mut paced = Paced {
            raw_len: 10_000,
            sent: 0,
            anchor_time: start,
            anchor_sent: 0,
            generation: 0,
            rate: serial.bytes_per_second(),
        };
        // The writer doubles the baud 100 ms in; the reader first looks 100 ms later.
        {
            let mut shared = pacing.lock();
            shared.serial.baud = 19_200;
            shared.changed_at = at(100);
            shared.generation = 1;
        }
        assert_eq!(
            paced.poll(&pacing, Some(1.0), at(200), 1000),
            Due::Now(288),
            "96 bytes at 960 B/s, then 192 at 1920 B/s"
        );
        assert_eq!((paced.anchor_sent, paced.anchor_time), (96, at(100)));
    }

    #[test]
    fn a_line_rate_of_zero_earns_nothing_and_does_not_panic() {
        let start = Instant::now();
        let serial = SerialConfig {
            baud: 0,
            ..SerialConfig::default()
        };
        let pacing = Mutex::new(Pacing {
            serial: serial.clone(),
            changed_at: start,
            generation: 0,
        });
        let mut paced = Paced {
            raw_len: 10,
            sent: 0,
            anchor_time: start,
            anchor_sent: 0,
            generation: 0,
            rate: serial.bytes_per_second(),
        };
        assert_eq!(
            paced.poll(&pacing, Some(1.0), start + 60 * MS, 10),
            Due::Never
        );
    }
}
