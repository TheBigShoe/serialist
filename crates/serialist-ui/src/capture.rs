//! Raw recording: every received chunk appended to a file, exactly as the transport
//! delivered it.
//!
//! The store owns raw retention (raw export reads its pages); recording is the one
//! consumer that must see every byte forever, so it runs on the ingest thread as a
//! [`ChunkSink`]. [`RecordingSink`] is installed when the session's ingest thread is
//! spawned and looks up the active [`Recorder`] in a [`RecordingSlot`] per chunk, so a
//! recording starts and stops mid-session without respawning anything, and file I/O
//! never runs on the main thread.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::ChunkSink;

/// A recording reaches the disk at least this often while data flows.
pub const RECORD_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Counters a recording shares with the UI, readable without touching the file.
#[derive(Debug, Default)]
pub struct RecorderStats {
    bytes: AtomicU64,
    error: Mutex<Option<String>>,
}

impl RecorderStats {
    /// Bytes handed to the file so far (buffered or written).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The first write error, after which the recording stops writing.
    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }
}

/// Appends raw chunks to a file through a buffer, flushing on a timer and when finished.
pub struct Recorder {
    file: BufWriter<File>,
    path: PathBuf,
    stats: Arc<RecorderStats>,
    last_flush: Instant,
    failed: bool,
}

impl Recorder {
    /// Create (or truncate) the file at `path`.
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: BufWriter::with_capacity(256 * 1024, File::create(path)?),
            path: path.to_owned(),
            stats: Arc::default(),
            last_flush: Instant::now(),
            failed: false,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn stats(&self) -> Arc<RecorderStats> {
        self.stats.clone()
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.failed || bytes.is_empty() {
            return;
        }
        match self.file.write_all(bytes) {
            Ok(()) => {
                self.stats
                    .bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
            Err(error) => self.fail(&error),
        }
    }

    /// Flush if the last flush was at least [`RECORD_FLUSH_INTERVAL`] before `now`.
    pub fn tick(&mut self, now: Instant) {
        if self.failed || now.saturating_duration_since(self.last_flush) < RECORD_FLUSH_INTERVAL {
            return;
        }
        self.last_flush = now;
        if let Err(error) = self.file.flush() {
            self.fail(&error);
        }
    }

    /// Flush now, whatever the interval.
    pub fn flush(&mut self) {
        if self.failed {
            return;
        }
        self.last_flush = Instant::now();
        if let Err(error) = self.file.flush() {
            self.fail(&error);
        }
    }

    /// Final flush and sync. Returns the bytes recorded, or the first error.
    pub fn finish(mut self) -> Result<u64, String> {
        if !self.failed {
            let result = self
                .file
                .flush()
                .and_then(|()| self.file.get_ref().sync_all());
            if let Err(error) = result {
                self.fail(&error);
            }
        }
        match self.stats.error() {
            Some(error) => Err(error),
            None => Ok(self.stats.bytes()),
        }
    }

    fn fail(&mut self, error: &io::Error) {
        self.failed = true;
        tracing::warn!(path = %self.path.display(), %error, "recording failed");
        self.stats
            .error
            .lock()
            .get_or_insert_with(|| error.to_string());
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

    /// Append every chunk in order, then flush if one is due.
    pub fn record<'a>(&self, chunks: impl IntoIterator<Item = &'a [u8]>, now: Instant) {
        let mut active = self.active.lock();
        if let Some((_, recorder)) = active.as_mut() {
            for chunk in chunks {
                recorder.write(chunk);
            }
            recorder.tick(now);
        }
    }

    /// Flush if one is due. The recording sink calls this from the ingest thread when
    /// the stream goes quiet ([`ChunkSink::on_idle`]), so the flush cadence holds when
    /// data stops and no chunk calls the sink.
    pub fn tick(&self, now: Instant) {
        self.record(std::iter::empty(), now);
    }

    /// Flush now.
    pub fn flush(&self) {
        if let Some((_, recorder)) = self.active.lock().as_mut() {
            recorder.flush();
        }
    }
}

/// The [`ChunkSink`] a session's ingest thread writes recordings through: whatever
/// recorder the slot holds gets every chunk, unchanged and in order; with none
/// installed a chunk costs one uncontended lock.
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
    fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
        self.slot.record([bytes], Instant::now());
    }

    /// The stream went quiet: flush a recording that is due, so what arrived last
    /// reaches the disk without a chunk to carry it.
    fn on_idle(&mut self, now: Instant) {
        self.slot.tick(now);
    }

    /// Nothing more will arrive: get what is buffered onto the disk. The session view
    /// then stops the recording with a final flush and sync.
    fn on_disconnect(&mut self) {
        self.slot.flush();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn recorder_flushes_on_its_interval_and_on_finish() {
        let dir = TestDir::new("recorder-flush");
        let path = dir.path().join("capture.bin");
        let mut recorder = Recorder::create(&path).unwrap();
        let start = Instant::now();
        recorder.write(b"hello ");
        recorder.tick(start);
        assert_eq!(std::fs::read(&path).unwrap(), b"", "still buffered");

        recorder.tick(start + RECORD_FLUSH_INTERVAL + Duration::from_millis(1));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello ");

        recorder.write(b"world");
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
            slot.install(1, Recorder::create(&dir.path().join("a")).unwrap())
                .is_none()
        );
        assert!(
            slot.take(2).is_none(),
            "a stale stop leaves the recording alone"
        );
        assert!(slot.is_recording());

        slot.record([&b"ab"[..], b"cd"], Instant::now());
        let recorder = slot.take(1).expect("the active recording");
        assert!(!slot.is_recording());
        assert_eq!(recorder.finish(), Ok(4));
        assert_eq!(std::fs::read(dir.path().join("a")).unwrap(), b"abcd");

        // With nothing installed, recording is a no-op.
        slot.record([&b"lost"[..]], Instant::now());
    }

    #[test]
    fn the_sink_records_through_whatever_the_slot_holds() {
        let dir = TestDir::new("recorder-sink");
        let slot = RecordingSlot::default();
        let mut sink = RecordingSink::new(slot.clone());
        let now = Instant::now();
        sink.on_chunk(b"before ", now);

        let path = dir.path().join("mid.bin");
        slot.install(7, Recorder::create(&path).unwrap());
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
        slot.install(1, Recorder::create(&path).unwrap());
        let start = Instant::now();
        slot.record([&b"quiet"[..]], start);
        slot.tick(start);
        assert_eq!(fs::read(&path).unwrap(), b"");
        slot.tick(start + RECORD_FLUSH_INTERVAL + Duration::from_millis(1));
        assert_eq!(fs::read(&path).unwrap(), b"quiet");
    }
}
