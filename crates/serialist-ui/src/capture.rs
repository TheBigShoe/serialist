//! Raw capture: the received bytes exactly as the transport delivered them, for raw
//! export and recording.
//!
//! A stand-in for the milestone 1 page store, which will own raw retention. Until then
//! [`RawRing`] keeps the transport's own `Arc<[u8]>` chunks (no copy) under a byte cap,
//! and [`Recorder`] appends chunks to a file from the drain worker, so file I/O never
//! runs on the main thread.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Raw bytes kept per session before the oldest chunks are dropped.
pub const DEFAULT_RAW_CAPACITY: usize = 64 * 1024 * 1024;

/// A recording reaches the disk at least this often while data flows.
pub const RECORD_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// The newest received chunks, up to a byte cap.
///
/// Eviction is by whole chunks, oldest first. The newest chunk is always kept, so a
/// single chunk larger than the cap (not possible with the session's 64 KiB reads)
/// cannot empty the ring.
#[derive(Clone, Debug)]
pub struct RawRing {
    chunks: VecDeque<Arc<[u8]>>,
    capacity: usize,
    retained: usize,
    /// Stream offset of the first retained byte, which is also the bytes evicted so far.
    oldest_offset: u64,
}

impl Default for RawRing {
    fn default() -> Self {
        Self::new(DEFAULT_RAW_CAPACITY)
    }
}

impl RawRing {
    pub fn new(capacity: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            capacity,
            retained: 0,
            oldest_offset: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn push(&mut self, chunk: Arc<[u8]>) {
        if chunk.is_empty() {
            return;
        }
        self.retained += chunk.len();
        self.chunks.push_back(chunk);
        while self.retained > self.capacity && self.chunks.len() > 1 {
            if let Some(oldest) = self.chunks.pop_front() {
                self.retained -= oldest.len();
                self.oldest_offset += oldest.len() as u64;
            }
        }
    }

    /// Bytes currently held.
    pub fn retained_bytes(&self) -> usize {
        self.retained
    }

    /// Stream offset of the oldest byte still held.
    pub fn oldest_offset(&self) -> u64 {
        self.oldest_offset
    }

    /// Bytes dropped from the front to honour the cap.
    pub fn evicted_bytes(&self) -> u64 {
        self.oldest_offset
    }

    /// Stream offset just past the newest byte: every byte ever pushed.
    pub fn end_offset(&self) -> u64 {
        self.oldest_offset + self.retained as u64
    }

    pub fn chunks(&self) -> impl Iterator<Item = &Arc<[u8]>> + '_ {
        self.chunks.iter()
    }

    /// The retained chunks, sharing their storage: cheap enough to take on the main
    /// thread and hand to a background export.
    pub fn snapshot(&self) -> Vec<Arc<[u8]>> {
        self.chunks.iter().cloned().collect()
    }
}

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

/// Where the drain worker finds the active recorder, if any.
///
/// Only background threads lock it (the drain worker per batch, and start and stop
/// tasks), so the main thread never waits on a file write. Each recording carries an
/// id so a stop can only ever take the recording it was meant for.
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

    /// The drain path: append every chunk in order, then flush if one is due. Called on
    /// every wake, including idle ones, so the flush cadence holds when data stops.
    pub fn record<'a>(&self, chunks: impl IntoIterator<Item = &'a [u8]>, now: Instant) {
        let mut active = self.active.lock();
        if let Some((_, recorder)) = active.as_mut() {
            for chunk in chunks {
                recorder.write(chunk);
            }
            recorder.tick(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    fn ring_bytes(ring: &RawRing) -> Vec<u8> {
        ring.chunks().flat_map(|c| c.iter().copied()).collect()
    }

    #[test]
    fn ring_evicts_the_oldest_chunks_and_counts_them() {
        let mut ring = RawRing::new(10);
        for chunk in [&b"abcd"[..], b"efgh", b"ijkl", b"mn"] {
            ring.push(Arc::from(chunk));
        }
        // 14 bytes pushed; dropping "abcd" leaves exactly 10.
        assert_eq!(ring_bytes(&ring), b"efghijklmn");
        assert_eq!(ring.retained_bytes(), 10);
        assert_eq!(ring.evicted_bytes(), 4);
        assert_eq!(ring.oldest_offset(), 4);
        assert_eq!(ring.end_offset(), 14);

        ring.push(Arc::from(&b"o"[..]));
        assert_eq!(ring_bytes(&ring), b"ijklmno");
        assert_eq!(ring.evicted_bytes(), 8);
        assert_eq!(ring.end_offset(), 15);
    }

    #[test]
    fn ring_keeps_the_newest_bytes_of_a_long_stream() {
        let mut ring = RawRing::new(1000);
        let stream: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        for chunk in stream.chunks(37) {
            ring.push(Arc::from(chunk));
        }
        let kept = ring_bytes(&ring);
        assert!(kept.len() <= 1000 && kept.len() > 1000 - 37);
        assert_eq!(kept, stream[stream.len() - kept.len()..]);
        assert_eq!(ring.evicted_bytes() as usize, stream.len() - kept.len());
    }

    #[test]
    fn ring_never_drops_the_newest_chunk_and_ignores_empty_ones() {
        let mut ring = RawRing::new(4);
        ring.push(Arc::from(&b""[..]));
        assert_eq!(ring.chunks().count(), 0);
        ring.push(Arc::from(&b"123456"[..]));
        assert_eq!(ring_bytes(&ring), b"123456");
        ring.push(Arc::from(&b"78"[..]));
        assert_eq!(ring_bytes(&ring), b"78");
        assert_eq!(ring.evicted_bytes(), 6);
    }

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
}
