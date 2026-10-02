//! Recording a simulated device the way the app's Record toggle records a real one: the
//! raw bytes exactly as the host received them, plus the timing sidecar that says when
//! each chunk arrived (see `serialist_core::capture` for both files). The result is a
//! capture the replay transport plays back, with no hardware involved.
//!
//! [`record_device`] runs a device on a virtual link on its own [`ManualClock`], so the
//! recording is a pure function of its arguments: two runs are byte-identical, which is
//! what lets a capture be committed as a test fixture and regenerated on demand.
//! [`CaptureRecorder`] is the in-memory recorder underneath; it also implements
//! [`ChunkSink`], so it can sit on an ingest thread next to the real sinks.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serialist_core::{ChunkSink, Timing, TimingError, TimingWriter, Transport, timing_path};

use crate::{LinkConfig, LinkHandle, ManualClock, SimDevice, VirtualLink};

/// The largest read the recorder asks the link for. The link's own `max_chunk` is lower
/// by default, so this only bounds what one read could return.
const READ_BUFFER: usize = 64 * 1024;

/// How long [`settle_device`] waits, in real time, before it fails.
const SETTLE_LIMIT: Duration = Duration::from_secs(10);

/// A capture in memory: the raw file's bytes and the sidecar's text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    /// Every byte the host received, back to back.
    pub raw: Vec<u8>,
    /// The timing sidecar, UTF-8 text with LF line ends.
    pub timing: Vec<u8>,
}

impl Capture {
    /// Write the raw file to `raw_path` and the sidecar to [`timing_path`]`(raw_path)`,
    /// replacing both.
    pub fn write_to(&self, raw_path: &Path) -> io::Result<()> {
        std::fs::write(raw_path, &self.raw)?;
        std::fs::write(timing_path(raw_path), &self.timing)
    }

    /// Read both files back. The sidecar must exist (a missing one is an `io::Error` of
    /// kind `NotFound`): a capture read here is one this module wrote.
    pub fn read_from(raw_path: &Path) -> io::Result<Self> {
        Ok(Self {
            raw: std::fs::read(raw_path)?,
            timing: std::fs::read(timing_path(raw_path))?,
        })
    }

    /// The sidecar, parsed.
    pub fn parse_timing(&self) -> Result<Timing, TimingError> {
        Timing::read(self.timing.as_slice())
    }
}

/// Records what a host receives into memory, the way the app's Record toggle records it
/// into a file: raw bytes in one buffer, one sidecar line per chunk in another. Times
/// are counted from the `origin` given at creation.
pub struct CaptureRecorder {
    raw: Vec<u8>,
    timing: TimingWriter<Vec<u8>>,
}

impl CaptureRecorder {
    /// Start a recording at `origin`.
    pub fn new(origin: Instant) -> Self {
        Self {
            raw: Vec::new(),
            timing: TimingWriter::new(Vec::new(), origin).expect("writing to a Vec cannot fail"),
        }
    }

    /// A chunk of `bytes` the host received at `at`, kept exactly as delivered. An empty
    /// chunk records nothing.
    pub fn chunk(&mut self, bytes: &[u8], at: Instant) {
        self.raw.extend_from_slice(bytes);
        self.timing
            .rx(at, bytes.len())
            .expect("writing to a Vec cannot fail");
    }

    /// The link came up at `at`; `description` is the transport's own name for it.
    pub fn connect(&mut self, at: Instant, description: &str) {
        self.timing
            .connect(at, description)
            .expect("writing to a Vec cannot fail");
    }

    /// The link went down at `at`.
    pub fn disconnect(&mut self, at: Instant) {
        self.timing
            .disconnect(at)
            .expect("writing to a Vec cannot fail");
    }

    /// End the recording and hand over both files' contents.
    pub fn finish(self) -> Capture {
        Capture {
            raw: self.raw,
            timing: self.timing.into_inner(),
        }
    }
}

impl ChunkSink for CaptureRecorder {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        self.chunk(bytes, at);
    }

    fn on_disconnect(&mut self) {
        self.disconnect(Instant::now());
    }

    fn on_connect(&mut self, description: &str) {
        self.connect(Instant::now(), description);
    }
}

/// Run `device` on a virtual link on a fresh [`ManualClock`] for `duration` of simulated
/// time, advancing `step` at a time, and record every chunk the host reads. A connect
/// record with the transport's description (`virtual:race @ 115200 8N1`) comes first. If
/// the device hangs up (or the link goes down) before `duration` is over, a disconnect
/// record ends the capture there; otherwise it simply stops, like a Record toggle
/// switched off on a live link.
///
/// The host reads with a zero timeout after each `advance`, once the device thread has
/// settled (asleep on the clock, as `ManualClock::settle(1)` waits for, or gone after a
/// hang-up), so the only thread waiting on the clock is the device thread (the `ManualClock` docs: the thread
/// that moves the clock must not be the one blocked in `read`). Everything the link has
/// released by then is read, in as many reads as it takes, so a chunk is what the link's
/// own packetising delivers at that instant and its time is the clock's, never the
/// machine's. The recording therefore depends on the arguments alone: the same device,
/// link and timings give the same bytes in both files, on any machine and under any
/// load.
///
/// # Panics
///
/// If `step` is zero.
pub fn record_device(
    device: Box<dyn SimDevice>,
    link: LinkConfig,
    duration: Duration,
    step: Duration,
) -> Capture {
    assert!(!step.is_zero(), "record_device needs a step above zero");
    let clock = Arc::new(ManualClock::new());
    let origin = clock.now();
    let (
        Transport {
            mut reader,
            writer,
            description,
        },
        handle,
    ) = VirtualLink::connect_with_clock(device, link, clock.clone());

    let mut recorder = CaptureRecorder::new(origin);
    recorder.connect(origin, &description);
    let mut buf = vec![0u8; READ_BUFFER];
    let mut elapsed = Duration::ZERO;
    // The device thread runs `on_connect` and its first tick before the clock moves.
    settle_device(&clock, &handle);
    'recording: loop {
        loop {
            match reader.read(&mut buf, Duration::ZERO) {
                Ok(0) => break,
                Ok(n) => recorder.chunk(&buf[..n], clock.now()),
                Err(_) => {
                    recorder.disconnect(clock.now());
                    break 'recording;
                }
            }
        }
        if elapsed >= duration {
            break;
        }
        let by = step.min(duration - elapsed);
        elapsed += by;
        clock.advance(by);
        settle_device(&clock, &handle);
    }
    // The device thread stops by itself once both host halves are gone.
    drop((reader, writer));
    recorder.finish()
}

/// Wait, in real time, until the device thread has seen the clock's latest time and has
/// nothing left to do: asleep on the clock (what `ManualClock::settle(1)` waits for), or
/// gone. `settle(1)` alone would stall on a device that hung up, whose thread exits.
///
/// # Panics
///
/// After [`SETTLE_LIMIT`] of real time.
fn settle_device(clock: &ManualClock, handle: &LinkHandle) {
    let limit = Instant::now() + SETTLE_LIMIT;
    while handle.is_device_running() && clock.sleepers() == 0 {
        assert!(
            Instant::now() < limit,
            "the simulated device thread did not settle within {SETTLE_LIMIT:?}"
        );
        thread::sleep(Duration::from_micros(50));
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use serialist_core::TimingRecord;

    use super::*;
    use crate::{DeviceOutput, EchoDevice, RaceDevice, race_frame};

    const MS: Duration = Duration::from_millis(1);

    /// A directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "serialist-sim-capture-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("create the temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn race() -> Box<dyn SimDevice> {
        Box::new(RaceDevice::new().with_log_interval(Some(20 * MS)))
    }

    fn record_race() -> Capture {
        record_device(
            race(),
            LinkConfig::default(),
            Duration::from_millis(200),
            5 * MS,
        )
    }

    #[test]
    fn the_recorder_writes_both_files_in_step() {
        let origin = Instant::now();
        let mut recorder = CaptureRecorder::new(origin);
        recorder.connect(origin, "virtual:x @ 115200 8N1");
        recorder.chunk(b"abc", origin + MS);
        recorder.chunk(b"", origin + 2 * MS);
        recorder.chunk(b"defg", origin + 5 * MS);
        recorder.disconnect(origin + 9 * MS);
        let capture = recorder.finish();
        assert_eq!(capture.raw, b"abcdefg");
        let timing = capture.parse_timing().unwrap();
        assert!(!timing.truncated);
        assert_eq!(
            timing.records,
            [
                TimingRecord::Connect {
                    at: Duration::ZERO,
                    description: "virtual:x @ 115200 8N1".into()
                },
                TimingRecord::Rx {
                    at: MS,
                    offset: 0,
                    len: 3
                },
                TimingRecord::Rx {
                    at: 5 * MS,
                    offset: 3,
                    len: 4
                },
                TimingRecord::Disconnect { at: 9 * MS },
            ]
        );
    }

    #[test]
    fn the_recorder_is_a_chunk_sink() {
        let mut recorder = CaptureRecorder::new(Instant::now());
        let sink: &mut dyn ChunkSink = &mut recorder;
        sink.on_connect("virtual:x @ 9600 8N1");
        sink.on_chunk(b"hello", Instant::now());
        sink.on_disconnect();
        let capture = recorder.finish();
        assert_eq!(capture.raw, b"hello");
        let kinds: Vec<&str> = capture
            .parse_timing()
            .unwrap()
            .records
            .iter()
            .map(|record| match record {
                TimingRecord::Connect { .. } => "connect",
                TimingRecord::Rx { .. } => "rx",
                TimingRecord::Disconnect { .. } => "disconnect",
            })
            .collect();
        assert_eq!(kinds, ["connect", "rx", "disconnect"]);
    }

    #[test]
    fn a_capture_round_trips_through_its_files() {
        let dir = TempDir::new();
        let path = dir.0.join("cap.bin");
        let capture = record_race();
        capture.write_to(&path).unwrap();
        assert!(dir.0.join("cap.bin.timing").is_file());
        assert_eq!(Capture::read_from(&path).unwrap(), capture);
        let missing = dir.0.join("nothing.bin");
        assert_eq!(
            Capture::read_from(&missing).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn recording_a_device_is_deterministic() {
        let first = record_race();
        let second = record_race();
        assert_eq!(first.raw, second.raw);
        assert_eq!(first.timing, second.timing);
    }

    #[test]
    fn a_recording_holds_what_the_device_sent_when_it_sent_it() {
        let capture = record_race();
        let timing = capture.parse_timing().unwrap();
        assert!(!timing.truncated);

        // The connect record comes first, at the origin, with the link's description.
        assert_eq!(
            timing.records[0],
            TimingRecord::Connect {
                at: Duration::ZERO,
                description: "virtual:race @ 115200 8N1".into()
            }
        );
        // The raw file starts with the device's banner and holds its first log frame.
        assert!(
            capture
                .raw
                .starts_with(b"Airoha RACE simulator SIM-RACE 1.4.2\r\n")
        );
        let log_one = race_frame(
            crate::RACE_LOG,
            RaceDevice::LOG_CMD_ID,
            RaceDevice::log_text(1).as_bytes(),
        );
        assert!(
            capture
                .raw
                .windows(log_one.len())
                .any(|window| window == log_one)
        );

        // Chunks are contiguous, land on the clock's steps, and cover the raw file.
        let schedule = timing.schedule(capture.raw.len() as u64);
        let rx: Vec<_> = timing.rx().collect();
        assert!(rx.len() > 10, "{} chunks", rx.len());
        let mut next = 0;
        for (at, offset, len) in &rx {
            assert_eq!(*offset, next);
            next += len;
            assert_eq!(at.as_micros() % (5 * MS).as_micros(), 0, "{at:?}");
            assert!(*at <= Duration::from_millis(200));
        }
        assert_eq!(next, capture.raw.len() as u64);
        assert_eq!(schedule.len(), rx.len());
    }

    #[test]
    fn a_device_that_hangs_up_ends_the_capture_with_a_disconnect() {
        struct Goodbye;
        impl SimDevice for Goodbye {
            fn name(&self) -> &str {
                "goodbye"
            }
            fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
                out.send(b"bye\r\n");
                out.disconnect();
            }
            fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {}
        }
        let capture = record_device(
            Box::new(Goodbye),
            LinkConfig::default(),
            Duration::from_secs(1),
            5 * MS,
        );
        assert_eq!(capture.raw, b"bye\r\n");
        let timing = capture.parse_timing().unwrap();
        let [
            TimingRecord::Connect { .. },
            TimingRecord::Rx { len: 5, .. },
            TimingRecord::Disconnect { at },
        ] = timing.records.as_slice()
        else {
            panic!("{:?}", timing.records);
        };
        assert!(*at < Duration::from_secs(1), "{at:?}");
    }

    #[test]
    fn a_silent_device_records_only_the_connect() {
        let capture = record_device(
            Box::new(EchoDevice::new()),
            LinkConfig::default(),
            50 * MS,
            5 * MS,
        );
        assert!(capture.raw.is_empty());
        assert_eq!(capture.parse_timing().unwrap().records.len(), 1);
    }

    #[test]
    #[should_panic(expected = "step above zero")]
    fn a_zero_step_is_refused() {
        record_device(
            Box::new(EchoDevice::new()),
            LinkConfig::default(),
            MS,
            Duration::ZERO,
        );
    }
}
