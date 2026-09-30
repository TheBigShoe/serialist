//! Decoded frames: a bounded store the ingest thread fills and any thread reads, and the
//! [`ChunkSink`] that runs a [`Codec`] on the ingest thread.
//!
//! # Threads
//!
//! [`FrameStore`] is the writer, owned by one thread (inside a [`CodecSink`], the ingest
//! thread). [`FrameStoreReader`] (`Clone + Send + Sync`) hands out [`FrameSnapshot`]s to
//! any thread. Frames live in fixed blocks of write-once slots; after each batch the
//! writer publishes an immutable record (the id range and an `Arc` directory of blocks)
//! by swapping one `Arc` under a mutex, and a snapshot clones that `Arc`. The lock is
//! held for a pointer swap, never while decoding, so neither side waits on the other.
//! A snapshot keeps the blocks it can reach alive, so eviction never invalidates it.
//!
//! # Bounds
//!
//! The store keeps the newest [`FrameStoreConfig::capacity`] frames (default
//! [`DEFAULT_FRAME_CAPACITY`], 100 000), like the scrollback's ring: older frames are
//! evicted, a block at a time once every frame in it is past the window. [`FrameId`]s
//! increase by one per frame and are never reused, so an id below
//! [`FrameSnapshot::first`] names an evicted frame.
//!
//! # Waking the UI
//!
//! The same doorbell protocol as the ingest thread's (see [`crate::ingest`]): after a
//! batch is published the sink calls its waker only if the dirty flag was clear, and the
//! UI calls [`FrameStoreReader::acknowledge`] *before* it takes a snapshot, so a
//! publication is never missed and there is at most one wake per acknowledge.
//!
//! # Offsets
//!
//! A [`CodecSink`] tracks the stream offset of each chunk from the lengths of the chunks
//! before it, so frames carry the same raw offsets as the scrollback store. Offset zero
//! is the first byte the sink sees, which is the ingest start: for a fresh store that is
//! exactly the store's offset. A sink attached to a store that already holds bytes (a
//! reconnect keeping its scrollback) starts at the store's end instead, with
//! [`CodecSink::starting_at`]`(store.stats().raw_len)`.

use std::collections::VecDeque;
use std::fmt;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use parking_lot::Mutex;

use crate::codec::{Codec, Frame, Severity};
use crate::ingest::ChunkSink;

/// Frames a store keeps by default.
pub const DEFAULT_FRAME_CAPACITY: usize = 100_000;

/// Frames per block: the unit of allocation and eviction.
const BLOCK: u64 = 512;

/// Identity of a frame in one store. Increases by one per frame and is never reused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FrameId(pub u64);

impl FrameId {
    pub const ZERO: FrameId = FrameId(0);

    pub fn next(self) -> Self {
        FrameId(self.0 + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStoreConfig {
    /// Frames kept before the oldest are evicted. At least one.
    pub capacity: usize,
}

impl Default for FrameStoreConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_FRAME_CAPACITY,
        }
    }
}

/// Counters describing a store at one publication.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// The oldest retained frame; also how many were evicted.
    pub first: FrameId,
    /// One past the newest frame; also how many were ever stored.
    pub end: FrameId,
    pub capacity: usize,
}

impl FrameStats {
    /// Retained frames.
    pub fn count(&self) -> usize {
        (self.end.0 - self.first.0) as usize
    }
}

/// Write-once slots for `BLOCK` consecutive frames.
struct Block {
    slots: Box<[OnceLock<Frame>]>,
}

impl Block {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            slots: (0..BLOCK).map(|_| OnceLock::new()).collect(),
        })
    }
}

/// An immutable view shared by snapshots.
struct Published {
    first: u64,
    end: u64,
    /// Index (id / BLOCK) of `blocks[0]`.
    first_block: u64,
    blocks: Arc<[Arc<Block>]>,
    capacity: usize,
}

impl Published {
    fn stats(&self) -> FrameStats {
        FrameStats {
            first: FrameId(self.first),
            end: FrameId(self.end),
            capacity: self.capacity,
        }
    }
}

struct Shared {
    current: Mutex<Arc<Published>>,
    dirty: AtomicBool,
}

/// The writer side: owned by the thread that decodes. See the module docs.
pub struct FrameStore {
    capacity: usize,
    shared: Arc<Shared>,
    blocks: VecDeque<Arc<Block>>,
    first_block: u64,
    first: u64,
    end: u64,
    /// The published block directory, rebuilt only when a block comes or goes.
    dir: Arc<[Arc<Block>]>,
    dir_dirty: bool,
}

impl Default for FrameStore {
    fn default() -> Self {
        Self::new(FrameStoreConfig::default())
    }
}

impl fmt::Debug for FrameStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameStore")
            .field("capacity", &self.capacity)
            .field("first", &self.first)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl FrameStore {
    pub fn new(config: FrameStoreConfig) -> Self {
        let capacity = config.capacity.max(1);
        let dir: Arc<[Arc<Block>]> = Arc::new([]);
        Self {
            capacity,
            shared: Arc::new(Shared {
                current: Mutex::new(Arc::new(Published {
                    first: 0,
                    end: 0,
                    first_block: 0,
                    blocks: Arc::clone(&dir),
                    capacity,
                })),
                dirty: AtomicBool::new(false),
            }),
            blocks: VecDeque::new(),
            first_block: 0,
            first: 0,
            end: 0,
            dir,
            dir_dirty: false,
        }
    }

    /// A store keeping at most `capacity` frames.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::new(FrameStoreConfig { capacity })
    }

    pub fn reader(&self) -> FrameStoreReader {
        FrameStoreReader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Same as `self.reader().snapshot()`: everything published so far.
    pub fn snapshot(&self) -> FrameSnapshot {
        self.reader().snapshot()
    }

    /// One past the newest frame stored, published or not.
    pub fn end(&self) -> FrameId {
        FrameId(self.end)
    }

    /// Store one frame and publish it.
    pub fn push(&mut self, frame: Frame) -> FrameId {
        let id = self.append(frame);
        self.publish();
        id
    }

    /// Store frames and publish them together. Returns the ids they got.
    pub fn extend(&mut self, frames: impl IntoIterator<Item = Frame>) -> Range<FrameId> {
        let start = FrameId(self.end);
        for frame in frames {
            self.append(frame);
        }
        if self.end != start.0 {
            self.publish();
        }
        start..FrameId(self.end)
    }

    fn append(&mut self, frame: Frame) -> FrameId {
        let id = self.end;
        let slot = (id % BLOCK) as usize;
        if slot == 0 {
            self.blocks.push_back(Block::new());
            self.dir_dirty = true;
        }
        let block = self.blocks.back().expect("a block for the new frame");
        // A fresh slot: `end` only moves forward, so no slot is set twice.
        let _ = block.slots[slot].set(frame);
        self.end += 1;
        self.first = self.end.saturating_sub(self.capacity as u64);
        while (self.first_block + 1) * BLOCK <= self.first {
            self.blocks.pop_front();
            self.first_block += 1;
            self.dir_dirty = true;
        }
        FrameId(id)
    }

    fn publish(&mut self) {
        if self.dir_dirty {
            self.dir = self.blocks.iter().cloned().collect();
            self.dir_dirty = false;
        }
        let published = Arc::new(Published {
            first: self.first,
            end: self.end,
            first_block: self.first_block,
            blocks: Arc::clone(&self.dir),
            capacity: self.capacity,
        });
        *self.shared.current.lock() = published;
    }

    /// Mark the store dirty. Returns whether it was clean, which is when a waker should
    /// be called (see the module docs).
    fn mark_dirty(&self) -> bool {
        !self.shared.dirty.swap(true, Ordering::AcqRel)
    }
}

/// A handle for readers on any thread. Cheap to clone.
#[derive(Clone)]
pub struct FrameStoreReader {
    shared: Arc<Shared>,
}

impl FrameStoreReader {
    /// A consistent view now. One uncontended lock and one `Arc` clone; no frame is copied.
    pub fn snapshot(&self) -> FrameSnapshot {
        FrameSnapshot {
            p: Arc::clone(&self.shared.current.lock()),
        }
    }

    /// The reader has looked (or is about to take its snapshot): the next publication
    /// may wake it again.
    pub fn acknowledge(&self) {
        self.shared.dirty.store(false, Ordering::Release);
    }

    pub fn stats(&self) -> FrameStats {
        self.shared.current.lock().stats()
    }
}

impl fmt::Debug for FrameStoreReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameStoreReader")
            .field("stats", &self.stats())
            .finish()
    }
}

/// The frames of a store at one moment. Holds what it can reach alive; costs nothing to
/// keep while the store moves on.
#[derive(Clone)]
pub struct FrameSnapshot {
    p: Arc<Published>,
}

impl fmt::Debug for FrameSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameSnapshot")
            .field("stats", &self.p.stats())
            .finish()
    }
}

impl FrameSnapshot {
    /// The oldest retained frame.
    pub fn first(&self) -> FrameId {
        FrameId(self.p.first)
    }

    /// One past the newest frame.
    pub fn end(&self) -> FrameId {
        FrameId(self.p.end)
    }

    /// Retained frames.
    pub fn count(&self) -> usize {
        (self.p.end - self.p.first) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }

    pub fn stats(&self) -> FrameStats {
        self.p.stats()
    }

    /// The frame `id`, if it is retained.
    pub fn get(&self, id: FrameId) -> Option<&Frame> {
        let p = &*self.p;
        if id.0 < p.first || id.0 >= p.end {
            return None;
        }
        let block = p.blocks.get((id.0 / BLOCK - p.first_block) as usize)?;
        block.slots[(id.0 % BLOCK) as usize].get()
    }

    /// The retained frames in `range`, in order, with their ids.
    pub fn iter(&self, range: Range<FrameId>) -> impl Iterator<Item = (FrameId, &Frame)> + '_ {
        let start = range.start.0.max(self.p.first);
        let end = range.end.0.min(self.p.end).max(start);
        (start..end).filter_map(move |id| Some((FrameId(id), self.get(FrameId(id))?)))
    }

    /// Every retained frame, in order.
    pub fn frames(&self) -> impl Iterator<Item = (FrameId, &Frame)> + '_ {
        self.iter(self.first()..self.end())
    }

    /// The retained frames of one kind, in order.
    pub fn filter<'a>(&'a self, kind: &'a str) -> impl Iterator<Item = (FrameId, &'a Frame)> + 'a {
        self.frames().filter(move |(_, frame)| frame.kind == kind)
    }

    /// The newest frame.
    pub fn last(&self) -> Option<&Frame> {
        self.p
            .end
            .checked_sub(1)
            .and_then(|id| self.get(FrameId(id)))
    }
}

/// Kind of the frame a [`CodecSink`] records when its codec panics.
pub const CODEC_PANIC_KIND: &str = "codec_error";

/// Runs a codec over every received chunk on the ingest thread and records the frames.
///
/// Give it to [`Ingest::spawn`](crate::Ingest::spawn) with the other sinks; the ingest
/// thread is unchanged. Each chunk costs one `decode` call and, if frames came out, one
/// publication and at most one wake. The codec is reset when the session disconnects,
/// so a partial frame never joins bytes from a later connection.
///
/// A codec that panics does not take the ingest thread with it: the chunk gets a
/// [`CODEC_PANIC_KIND`] frame with `Severity::Error`, the codec is reset, and if the
/// reset panics too, the sink stops decoding.
pub struct CodecSink {
    codec: Box<dyn Codec>,
    store: FrameStore,
    waker: Option<Box<dyn Fn() + Send>>,
    offset: u64,
    scratch: Vec<Frame>,
    broken: bool,
}

impl fmt::Debug for CodecSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodecSink")
            .field("offset", &self.offset)
            .field("store", &self.store)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl CodecSink {
    /// Decode with `codec` into `store`, calling `waker` (if any) when new frames are
    /// published and the reader has acknowledged the last wake. The waker runs on the
    /// ingest thread and must not block.
    pub fn new(
        codec: Box<dyn Codec>,
        store: FrameStore,
        waker: Option<Box<dyn Fn() + Send>>,
    ) -> Self {
        Self {
            codec,
            store,
            waker,
            offset: 0,
            scratch: Vec::new(),
            broken: false,
        }
    }

    /// Start counting stream offsets at `raw_offset` instead of zero: the store's
    /// `raw_len` when the scrollback already holds bytes (see the module docs).
    pub fn starting_at(mut self, raw_offset: u64) -> Self {
        self.offset = raw_offset;
        self
    }

    /// The stream offset the next chunk will start at.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn reader(&self) -> FrameStoreReader {
        self.store.reader()
    }

    fn record(&mut self) {
        if self.scratch.is_empty() {
            return;
        }
        self.store.extend(self.scratch.drain(..));
        if self.store.mark_dirty()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }
}

impl ChunkSink for CodecSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        let offset = self.offset;
        self.offset += bytes.len() as u64;
        if self.broken {
            return;
        }
        let codec = &mut self.codec;
        let scratch = &mut self.scratch;
        let decoded = catch_unwind(AssertUnwindSafe(|| {
            codec.decode(bytes, at, offset, scratch);
        }));
        if let Err(payload) = decoded {
            let message = panic_message(payload.as_ref());
            tracing::error!(%message, "a codec panicked while decoding");
            self.scratch.push(
                Frame::new(CODEC_PANIC_KIND, offset..self.offset, at)
                    .with_field("error", message.clone())
                    .with_severity(Severity::Error)
                    .with_summary(format!("the codec panicked: {message}")),
            );
            let codec = &mut self.codec;
            if catch_unwind(AssertUnwindSafe(|| codec.reset())).is_err() {
                tracing::error!("the codec panicked again while resetting; decoding stops");
                self.broken = true;
            }
        }
        self.record();
    }

    fn on_disconnect(&mut self) {
        if self.broken {
            return;
        }
        let codec = &mut self.codec;
        if catch_unwind(AssertUnwindSafe(|| codec.reset())).is_err() {
            tracing::error!("the codec panicked while resetting; decoding stops");
            self.broken = true;
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::codec::{CodecError, CodecInfo, EncodeRequest};

    fn frame(n: u64) -> Frame {
        Frame::new(
            if n.is_multiple_of(2) { "even" } else { "odd" },
            n..n + 1,
            Instant::now(),
        )
        .with_field("n", n)
    }

    #[test]
    fn frames_get_increasing_ids_and_snapshots_are_fixed() {
        let mut store = FrameStore::default();
        let reader = store.reader();
        assert!(reader.snapshot().is_empty());
        assert_eq!(store.push(frame(0)), FrameId(0));
        let before = reader.snapshot();
        let ids = store.extend((1..10).map(frame));
        assert_eq!(ids, FrameId(1)..FrameId(10));
        assert_eq!(before.count(), 1);
        let snap = reader.snapshot();
        assert_eq!(snap.count(), 10);
        assert_eq!((snap.first(), snap.end()), (FrameId(0), FrameId(10)));
        assert_eq!(snap.get(FrameId(3)).unwrap().raw, 3..4);
        assert!(snap.get(FrameId(10)).is_none());
        let odd: Vec<u64> = snap.filter("odd").map(|(id, _)| id.0).collect();
        assert_eq!(odd, [1, 3, 5, 7, 9]);
        let mid: Vec<u64> = snap
            .iter(FrameId(8)..FrameId(99))
            .map(|(id, _)| id.0)
            .collect();
        assert_eq!(mid, [8, 9]);
        assert_eq!(snap.last().unwrap().raw, 9..10);
    }

    #[test]
    fn the_oldest_frames_are_evicted_past_capacity() {
        let capacity = 1000;
        let mut store = FrameStore::with_capacity(capacity);
        let reader = store.reader();
        let held = {
            store.extend((0..600).map(frame));
            reader.snapshot()
        };
        for chunk in 0..10u64 {
            store.extend((600 + chunk * 500..600 + (chunk + 1) * 500).map(frame));
        }
        let snap = reader.snapshot();
        assert_eq!(snap.count(), capacity);
        assert_eq!(snap.end(), FrameId(5600));
        assert_eq!(snap.first(), FrameId(4600));
        assert!(snap.get(FrameId(4599)).is_none());
        assert_eq!(snap.get(FrameId(4600)).unwrap().raw, 4600..4601);
        assert_eq!(snap.frames().count(), capacity);
        // Whole blocks go: at most the retained frames plus one block are allocated.
        assert!(store.blocks.len() as u64 <= capacity as u64 / BLOCK + 2);
        // An old snapshot still reads what it saw.
        assert_eq!(held.get(FrameId(0)).unwrap().raw, 0..1);
        assert_eq!(held.count(), 600);
    }

    #[test]
    fn readers_on_other_threads_see_whole_frames() {
        let mut store = FrameStore::with_capacity(5000);
        let reader = store.reader();
        let watcher = std::thread::spawn(move || {
            let mut last = 0;
            while last < 20_000 {
                let snap = reader.snapshot();
                for (id, frame) in snap.frames() {
                    assert_eq!(frame.field("n").and_then(|v| v.as_u64()), Some(id.0));
                }
                assert!(snap.end().0 >= last);
                last = snap.end().0;
            }
        });
        for n in 0..20_000 {
            store.push(frame(n));
        }
        watcher.join().unwrap();
    }

    /// Emits one frame per chunk covering it, and counts resets.
    struct PerChunk {
        resets: Arc<AtomicUsize>,
        panic_on: Option<u8>,
    }

    impl Codec for PerChunk {
        fn describe(&self) -> CodecInfo {
            CodecInfo::default()
        }

        fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
            if self.panic_on.is_some_and(|b| chunk.contains(&b)) {
                panic!("bad byte");
            }
            out.push(Frame::new(
                "chunk",
                raw_offset..raw_offset + chunk.len() as u64,
                at,
            ));
        }

        fn encode(&mut self, _: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
            Ok(Vec::new())
        }

        fn reset(&mut self) {
            self.resets.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn the_sink_tracks_offsets_wakes_once_per_acknowledge_and_resets() {
        let resets = Arc::new(AtomicUsize::new(0));
        let wakes = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&wakes);
        let mut sink = CodecSink::new(
            Box::new(PerChunk {
                resets: Arc::clone(&resets),
                panic_on: None,
            }),
            FrameStore::default(),
            Some(Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
            })),
        )
        .starting_at(100);
        let reader = sink.reader();
        let at = Instant::now();
        sink.on_chunk(b"abc", at);
        sink.on_chunk(b"", at);
        sink.on_chunk(b"defg", at);
        assert_eq!(wakes.load(Ordering::Relaxed), 1, "no acknowledge yet");
        reader.acknowledge();
        sink.on_chunk(b"h", at);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert_eq!(sink.offset(), 108);
        let ranges: Vec<_> = reader
            .snapshot()
            .frames()
            .map(|(_, f)| f.raw.clone())
            .collect();
        assert_eq!(ranges, [100..103, 103..103, 103..107, 107..108]);
        sink.on_disconnect();
        assert_eq!(resets.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_panicking_codec_becomes_an_error_frame() {
        let resets = Arc::new(AtomicUsize::new(0));
        let mut sink = CodecSink::new(
            Box::new(PerChunk {
                resets: Arc::clone(&resets),
                panic_on: Some(b'!'),
            }),
            FrameStore::default(),
            None,
        );
        let reader = sink.reader();
        let at = Instant::now();
        sink.on_chunk(b"ok", at);
        sink.on_chunk(b"no!", at);
        sink.on_chunk(b"ok", at);
        let snap = reader.snapshot();
        let kinds: Vec<_> = snap.frames().map(|(_, f)| f.kind.to_string()).collect();
        assert_eq!(kinds, ["chunk", CODEC_PANIC_KIND, "chunk"]);
        let error = snap.get(FrameId(1)).unwrap();
        assert_eq!(error.raw, 2..5);
        assert_eq!(error.severity, Severity::Error);
        assert_eq!(
            error.field("error").and_then(|v| v.as_str()),
            Some("bad byte")
        );
        assert_eq!(resets.load(Ordering::Relaxed), 1);
    }
}
