//! Recording: every received chunk appended to a raw file, exactly as the transport
//! delivered it, and a timing sidecar next to it saying when each chunk arrived.
//!
//! The raw file is what the Record toggle has always written and its bytes never change.
//! `<file>.timing` (see [`serialist_core::capture`] for the format) holds one `rx` record
//! per chunk, with the instant the session's reader thread received it, plus `connect`
//! and `disconnect` records, so a replay can reproduce the pacing and the chunk
//! boundaries. Both files are buffered, flushed together on the same cadence and synced
//! once at the end; a chunk costs two buffered appends and no I/O.
//!
//! The store owns raw retention (raw export reads its pages); recording is the one
//! consumer that must see every byte forever, so it runs on the ingest thread as a
//! [`ChunkSink`]. [`RecordingSink`] is installed when the session's ingest thread is
//! spawned and looks up the active [`Recorder`] in a [`RecordingSlot`] per chunk, so a
//! recording starts and stops mid-session without respawning anything, and file I/O
//! never runs on the main thread.

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::{ChunkSink, TimingWriter, timing_path};

/// A recording reaches the disk at least this often while data flows.
pub const RECORD_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Counters a recording shares with the UI, readable without touching the file.
#[derive(Debug, Default)]
pub struct RecorderStats {
    bytes: AtomicU64,
    error: Mutex<Option<String>>,
}

impl RecorderStats {
    /// Bytes handed to the raw file so far (buffered or written).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The first write error, from either file, after which the recording stops
    /// writing both.
    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }
}

/// Appends raw chunks to a file, and their arrival times to its `.timing` sidecar,
/// through buffers, flushing both on a timer and when finished.
pub struct Recorder {
    file: BufWriter<File>,
    timing: TimingWriter<BufWriter<File>>,
    path: PathBuf,
    timing_path: PathBuf,
    /// The instant sidecar times count from: when the recording was created.
    origin: Instant,
    stats: Arc<RecorderStats>,
    last_flush: Instant,
    failed: bool,
}

impl Recorder {
    /// Create (or truncate) the raw file at `path` and its sidecar next to it.
    ///
    /// With `Some(description)`, the session was already connected when recording
    /// started, so the sidecar opens with a `connect` record at time zero naming the
    /// link. If the sidecar cannot be created the raw file is removed again and the
    /// sidecar's error returned: a recording is both files or neither. The sidecar's
    /// header (and that `connect` record) are flushed at once, so it parses from the
    /// moment it exists, however soon the recording ends badly.
    pub fn create(path: &Path, description: Option<&str>) -> io::Result<Self> {
        let origin = Instant::now();
        let file = File::create(path)?;
        let timing_path = timing_path(path);
        let mut timing = match TimingWriter::create(&timing_path, origin) {
            Ok(timing) => timing,
            Err(error) => {
                drop(file);
                let _ = fs::remove_file(path);
                return Err(error);
            }
        };
        let started = description
            .map_or(Ok(()), |description| timing.connect(origin, description))
            .and_then(|()| timing.flush());
        if let Err(error) = started {
            drop((file, timing));
            let _ = fs::remove_file(path);
            let _ = fs::remove_file(&timing_path);
            return Err(error);
        }
        Ok(Self {
            file: BufWriter::with_capacity(256 * 1024, file),
            timing,
            path: path.to_owned(),
            timing_path,
            origin,
            stats: Arc::default(),
            last_flush: origin,
            failed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the sidecar is: [`timing_path`] of [`path`](Self::path).
    pub fn timing_path(&self) -> &Path {
        &self.timing_path
    }

    /// The instant the sidecar's times count from. A chunk that arrived `d` after it is
    /// recorded at `d`; tests build the `at` they pass from it.
    pub fn origin(&self) -> Instant {
        self.origin
    }

    pub fn stats(&self) -> Arc<RecorderStats> {
        self.stats.clone()
    }

    /// Append `bytes`, received at `at`, to the raw file and note the chunk in the
    /// sidecar. The first error from either file ends the recording.
    pub fn write(&mut self, bytes: &[u8], at: Instant) {
        if self.failed || bytes.is_empty() {
            return;
        }
        if let Err(error) = self.file.write_all(bytes) {
            return self.fail(&error, false);
        }
        self.stats
            .bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        if let Err(error) = self.timing.rx(at, bytes.len()) {
            self.fail(&error, true);
        }
    }

    /// Note in the sidecar that the link came up at `at`. Does nothing after a failure.
    pub fn connect(&mut self, at: Instant, description: &str) {
        if self.failed {
            return;
        }
        if let Err(error) = self.timing.connect(at, description) {
            self.fail(&error, true);
        }
    }

    /// Note in the sidecar that the link went down at `at`. Does nothing after a failure.
    pub fn disconnect(&mut self, at: Instant) {
        if self.failed {
            return;
        }
        if let Err(error) = self.timing.disconnect(at) {
            self.fail(&error, true);
        }
    }

    /// Flush both files if the last flush was at least [`RECORD_FLUSH_INTERVAL`] before
    /// `now`.
    pub fn tick(&mut self, now: Instant) {
        if self.failed || now.saturating_duration_since(self.last_flush) < RECORD_FLUSH_INTERVAL {
            return;
        }
        self.last_flush = now;
        self.flush_files();
    }

    /// Flush both files now, whatever the interval.
    pub fn flush(&mut self) {
        if self.failed {
            return;
        }
        self.last_flush = Instant::now();
        self.flush_files();
    }

    /// Final flush and sync of both files. Returns the raw bytes recorded, or the first
    /// error.
    pub fn finish(mut self) -> Result<u64, String> {
        if !self.failed {
            let raw = self
                .file
                .flush()
                .and_then(|()| self.file.get_ref().sync_all());
            if let Err(error) = raw {
                self.fail(&error, false);
            }
            let timing = self
                .timing
                .flush()
                .and_then(|()| self.timing.get_ref().get_ref().sync_all());
            if let Err(error) = timing {
                self.fail(&error, true);
            }
        }
        match self.stats.error() {
            Some(error) => Err(error),
            None => Ok(self.stats.bytes()),
        }
    }

    fn flush_files(&mut self) {
        if let Err(error) = self.file.flush() {
            return self.fail(&error, false);
        }
        if let Err(error) = self.timing.flush() {
            self.fail(&error, true);
        }
    }

    /// Stop both files after `error`, from the sidecar when `sidecar`. Only the first
    /// error is kept for the UI.
    fn fail(&mut self, error: &io::Error, sidecar: bool) {
        self.failed = true;
        let (path, message) = if sidecar {
            (&self.timing_path, format!("timing file: {error}"))
        } else {
            (&self.path, error.to_string())
        };
        tracing::warn!(path = %path.display(), %error, "recording failed");
        self.stats.error.lock().get_or_insert(message);
    }
}

/// Where the recording sink finds the active recorder, if any.
///
/// Only background threads lock it (the ingest thread per chunk, and the start, stop
/// and idle-flush tasks), so the main thread never waits on a file write. Each
/// recording carries an id so a stop can only ever take the recording it was meant for.
#[derive(Clone, Default)]
pub struct RecordingSlot {
    active: Arc<Mutex<Option<(u64, Recorder)>>>,
}

impl RecordingSlot {
    /// Make `recorder` the active one. Returns any recorder it displaced.
    pub fn install(&self, id: u64, recorder: Recorder) -> Option<Recorder> {
        self.active
            .lock()
            .replace((id, recorder))
            .map(|(_, previous)| previous)
    }

    /// Take the recorder with this id out of the slot, if it is still there.
    pub fn take(&self, id: u64) -> Option<Recorder> {
        let mut active = self.active.lock();
        match &*active {
            Some((current, _)) if *current == id => active.take().map(|(_, r)| r),
            _ => None,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.active.lock().is_some()
    }

    /// Append every chunk (its bytes and the instant it was received) in order, then
    /// flush if one is due at `now`.
    pub fn record<'a>(&self, chunks: impl IntoIterator<Item = (&'a [u8], Instant)>, now: Instant) {
        let mut active = self.active.lock();
        if let Some((_, recorder)) = active.as_mut() {
            for (bytes, at) in chunks {
                recorder.write(bytes, at);
            }
            recorder.tick(now);
        }
    }

    /// The link came up at `at`: note it in the sidecar of the active recording.
    pub fn connect(&self, at: Instant, description: &str) {
        if let Some((_, recorder)) = self.active.lock().as_mut() {
            recorder.connect(at, description);
        }
    }

    /// The link went down at `at`: note it in the sidecar of the active recording.
    pub fn disconnect(&self, at: Instant) {
        if let Some((_, recorder)) = self.active.lock().as_mut() {
            recorder.disconnect(at);
        }
    }

    /// Flush if one is due. The recording sink calls this from the ingest thread when
    /// the stream goes quiet ([`ChunkSink::on_idle`]), so the flush cadence holds when
    /// data stops and no chunk calls the sink.
    pub fn tick(&self, now: Instant) {
        self.record(std::iter::empty::<(&[u8], Instant)>(), now);
    }

    /// Flush now.
    pub fn flush(&self) {
        if let Some((_, recorder)) = self.active.lock().as_mut() {
            recorder.flush();
        }
    }
}

/// The [`ChunkSink`] a session's ingest thread writes recordings through: whatever
/// recorder the slot holds gets every chunk, unchanged and in order, with the instant
/// the session received it; with none installed a chunk costs one uncontended lock.
#[derive(Clone, Default)]
pub struct RecordingSink {
    slot: RecordingSlot,
}

impl RecordingSink {
    pub fn new(slot: RecordingSlot) -> Self {
        Self { slot }
    }
}

impl ChunkSink for RecordingSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        self.slot.record([(bytes, at)], Instant::now());
    }

    /// The link (re)connected while a recording runs: the sidecar notes it.
    fn on_connect(&mut self, description: &str) {
        self.slot.connect(Instant::now(), description);
    }

    /// The stream went quiet: flush a recording that is due, so what arrived last
    /// reaches the disk without a chunk to carry it.
    fn on_idle(&mut self, now: Instant) {
        self.slot.tick(now);
    }

    /// Nothing more will arrive: note it in the sidecar and get what is buffered onto
    /// the disk. The session view then stops the recording with a final flush and sync.
    fn on_disconnect(&mut self) {
        self.slot.disconnect(Instant::now());
        self.slot.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serialist_core::{ScheduledChunk, Timing, TimingRecord};

    use super::*;
    use crate::test_support::TestDir;

    const MS: Duration = Duration::from_millis(1);

    fn rx(at_us: u64, offset: u64, len: u64) -> (Duration, u64, u64) {
        (Duration::from_micros(at_us), offset, len)
    }

    #[test]
    fn recorder_flushes_on_its_interval_and_on_finish() {
        let dir = TestDir::new("recorder-flush");
        let path = dir.path().join("capture.bin");
        let mut recorder = Recorder::create(&path, None).unwrap();
        let start = Instant::now();
        recorder.write(b"hello ", start);
        recorder.tick(start);
        assert_eq!(std::fs::read(&path).unwrap(), b"", "still buffered");

        recorder.tick(start + RECORD_FLUSH_INTERVAL + MS);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello ");

        recorder.write(b"world", start);
        let stats = recorder.stats();
        assert_eq!(recorder.finish(), Ok(11));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
        assert_eq!(stats.bytes(), 11);
    }

    #[test]
    fn a_stop_takes_only_its_own_recording() {
        let dir = TestDir::new("recorder-slot");
        let slot = RecordingSlot::default();
        assert!(
            slot.install(1, Recorder::create(&dir.path().join("a"), None).unwrap())
                .is_none()
        );
        assert!(
            slot.take(2).is_none(),
            "a stale stop leaves the recording alone"
        );
        assert!(slot.is_recording());

        let now = Instant::now();
        slot.record([(&b"ab"[..], now), (&b"cd"[..], now)], now);
        let recorder = slot.take(1).expect("the active recording");
        assert!(!slot.is_recording());
        assert_eq!(recorder.finish(), Ok(4));
        assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"abcd");

        // With nothing installed, recording is a no-op.
        slot.record([(&b"lost"[..], now)], now);
        slot.connect(now, "nobody listens");
        slot.disconnect(now);
    }

    #[test]
    fn the_sink_records_through_whatever_the_slot_holds() {
        let dir = TestDir::new("recorder-sink");
        let slot = RecordingSlot::default();
        let mut sink = RecordingSink::new(slot.clone());
        let now = Instant::now();
        sink.on_chunk(b"before ", now);

        let path = dir.path().join("mid.bin");
        slot.install(7, Recorder::create(&path, None).unwrap());
        sink.on_chunk(b"\x00during\r\n", now);
        sink.on_chunk(b"\xff", now);
        assert_eq!(fs::read(&path).unwrap(), b"", "buffered");
        sink.on_disconnect();
        assert_eq!(
            fs::read(&path).unwrap(),
            b"\x00during\r\n\xff",
            "disconnect flushes, byte for byte"
        );

        let recorder = slot.take(7).unwrap();
        sink.on_chunk(b"after", now);
        assert_eq!(recorder.finish(), Ok(10));
        assert_eq!(fs::read(&path).unwrap(), b"\x00during\r\n\xff");
    }

    #[test]
    fn an_idle_tick_flushes_once_the_interval_passed() {
        let dir = TestDir::new("recorder-tick");
        let slot = RecordingSlot::default();
        let path = dir.path().join("idle.bin");
        slot.install(1, Recorder::create(&path, None).unwrap());
        let start = Instant::now();
        slot.record([(&b"quiet"[..], start)], start);
        slot.tick(start);
        assert_eq!(fs::read(&path).unwrap(), b"");
        slot.tick(start + RECORD_FLUSH_INTERVAL + MS);
        assert_eq!(fs::read(&path).unwrap(), b"quiet");
    }

    #[test]
    fn the_sidecar_records_every_chunk_at_its_arrival_time() {
        let dir = TestDir::new("recorder-timing");
        let path = dir.path().join("capture.bin");
        let mut recorder = Recorder::create(&path, None).unwrap();
        let origin = recorder.origin();
        let sidecar = recorder.timing_path().to_owned();
        assert_eq!(sidecar, dir.path().join("capture.bin.timing"));

        recorder.write(b"abc", origin + MS);
        recorder.write(b"d", origin + 5 * MS);
        recorder.write(b"efgh", origin + 5 * MS);
        assert_eq!(recorder.finish(), Ok(8));

        let timing = Timing::read_file(&sidecar).unwrap();
        assert!(!timing.truncated);
        assert_eq!(
            timing.rx().collect::<Vec<_>>(),
            [rx(1000, 0, 3), rx(5000, 3, 1), rx(5000, 4, 4)]
        );
        let raw_len = fs::metadata(&path).unwrap().len();
        assert_eq!(raw_len, 8);
        let chunk = |at_us: u64, offset: u64, len: u64| ScheduledChunk {
            at: Duration::from_micros(at_us),
            offset,
            len,
        };
        assert_eq!(
            timing.schedule(raw_len),
            [chunk(0, 0, 3), chunk(4000, 3, 1), chunk(4000, 4, 4)],
            "the schedule covers the raw file exactly, in the chunks that arrived"
        );
    }

    #[test]
    fn connect_and_disconnect_are_recorded() {
        let dir = TestDir::new("recorder-link");
        let path = dir.path().join("link.bin");
        let slot = RecordingSlot::default();
        let mut sink = RecordingSink::new(slot.clone());
        let recorder = Recorder::create(&path, None).unwrap();
        let origin = recorder.origin();
        let sidecar = recorder.timing_path().to_owned();
        slot.install(1, recorder);

        // The sink stamps a connect and a disconnect with the time it hears them, so the
        // chunks arrive far enough ahead that the order of the records is certain.
        let later = Duration::from_secs(60);
        sink.on_connect("virtual:x @ 115200 8N1");
        sink.on_chunk(b"ab", origin + later);
        sink.on_chunk(b"cde", origin + later + 5 * MS);
        sink.on_disconnect();
        let timing = Timing::read_file(&sidecar).unwrap();
        assert_eq!(
            timing.records.len(),
            4,
            "a disconnect flushes the sidecar too: {timing:?}"
        );

        assert_eq!(slot.take(1).unwrap().finish(), Ok(5));
        let timing = Timing::read_file(&sidecar).unwrap();
        assert!(!timing.truncated);
        assert!(
            matches!(
                timing.records.as_slice(),
                [
                    TimingRecord::Connect { at: up, description },
                    TimingRecord::Rx { at: first, offset: 0, len: 2 },
                    TimingRecord::Rx { at: second, offset: 2, len: 3 },
                    TimingRecord::Disconnect { at: down },
                ] if description == "virtual:x @ 115200 8N1"
                    && *up <= *first
                    && *first == later
                    && *second == later + 5 * MS
                    && *down >= *second
            ),
            "{timing:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"abcde");
    }

    #[test]
    fn a_recording_started_mid_session_names_the_link() {
        let dir = TestDir::new("recorder-mid");
        let path = dir.path().join("mid.bin");
        let mut recorder = Recorder::create(&path, Some("tcp:h:1")).unwrap();
        let origin = recorder.origin();
        let sidecar = recorder.timing_path().to_owned();
        let on_disk = Timing::read_file(&sidecar).unwrap();
        assert_eq!(
            on_disk.records,
            [TimingRecord::Connect {
                at: Duration::ZERO,
                description: "tcp:h:1".into()
            }],
            "the sidecar is parseable from the moment it exists"
        );

        recorder.write(b"x", origin + 3 * MS);
        assert_eq!(recorder.finish(), Ok(1));
        let timing = Timing::read_file(&sidecar).unwrap();
        assert_eq!(
            timing.records,
            [
                TimingRecord::Connect {
                    at: Duration::ZERO,
                    description: "tcp:h:1".into()
                },
                TimingRecord::Rx {
                    at: 3 * MS,
                    offset: 0,
                    len: 1
                },
            ]
        );
    }

    #[test]
    fn both_files_flush_on_the_interval() {
        let dir = TestDir::new("recorder-both");
        let path = dir.path().join("both.bin");
        let mut recorder = Recorder::create(&path, None).unwrap();
        let sidecar = recorder.timing_path().to_owned();
        let start = recorder.origin();
        recorder.write(b"hello ", start);
        recorder.tick(start);
        assert_eq!(fs::read(&path).unwrap(), b"", "raw still buffered");
        assert_eq!(
            Timing::read_file(&sidecar).unwrap().records,
            [],
            "and so is the new record"
        );

        recorder.tick(start + RECORD_FLUSH_INTERVAL + MS);
        assert_eq!(fs::read(&path).unwrap(), b"hello ");
        assert_eq!(
            Timing::read_file(&sidecar)
                .unwrap()
                .rx()
                .collect::<Vec<_>>(),
            [rx(0, 0, 6)],
            "both reach the disk together"
        );
        assert_eq!(recorder.finish(), Ok(6));
    }

    #[test]
    fn no_sidecar_means_no_recording() {
        let dir = TestDir::new("recorder-no-sidecar");
        let path = dir.path().join("blocked.bin");
        fs::create_dir(dir.path().join("blocked.bin.timing")).unwrap();
        assert!(Recorder::create(&path, None).is_err());
        assert!(!path.exists(), "the raw file is not left behind");
        assert!(Recorder::create(&path, Some("tcp:h:1")).is_err());
        assert!(!path.exists());
    }
}
