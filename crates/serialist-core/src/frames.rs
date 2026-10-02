//! Decoded frames: a bounded store the ingest thread fills and any thread reads, and the
//! [`ChunkSink`] that runs a [`Codec`] on the ingest thread.
//!
//! A codec is not `Send` (a Lua codec holds a VM that must stay on one thread), so it is
//! made on the thread that runs it: build the [`CodecSink`] inside
//! [`Ingest::spawn_with`](crate::Ingest::spawn_with)'s closure, from a
//! [`CodecFactory`] (which is `Send + Sync` and crosses threads freely).
//!
//! ```no_run
//! use std::sync::Arc;
//! use serialist_core::{
//!     ChunkSink, CodecFactory, CodecSink, FrameStore, Ingest, Session, Store,
//! };
//!
//! fn start(session: &Session, factory: Arc<dyn CodecFactory>) {
//!     let frames = FrameStore::default();
//!     let decoded = frames.reader(); // for the Decoded panel
//!     let ingest = Ingest::spawn_with(
//!         session.events(),
//!         Store::default(),
//!         Box::new(move || -> Vec<Box<dyn ChunkSink>> {
//!             vec![Box::new(CodecSink::from_factory(factory, frames, None))]
//!         }),
//!         Box::new(|| {}),
//!     );
//! #   let _ = (decoded, ingest);
//! }
//! ```
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
//! [`DEFAULT_FRAME_CAPACITY`], 100 000) within a byte budget
//! ([`FrameStoreConfig::budget`], default [`DEFAULT_FRAME_BUDGET`], 64 MiB), like the
//! scrollback store: older frames are evicted a block at a time, once every frame in the
//! block is past the count window or while the blocks together exceed the budget. A
//! frame's size is what it holds (its summary, its field names and values, a block's
//! slot), so a codec that emits large frames keeps fewer of them, and the store's memory
//! stays near the budget whatever a plugin returns. A block closes early once it holds
//! [`BLOCK_BYTES`] of frames, so eviction by bytes moves in steps of about that size;
//! the newest block is never evicted, so one frame larger than the budget is kept until
//! the next frame arrives. [`FrameStats::memory`] reports the bytes held. [`FrameId`]s
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
use smol_str::SmolStr;

use crate::codec::{Codec, CodecFactory, Frame, Severity, Value};
use crate::ingest::ChunkSink;

/// Frames a store keeps by default.
pub const DEFAULT_FRAME_CAPACITY: usize = 100_000;

/// Bytes of frames a store keeps by default: 64 MiB, room for the default capacity of
/// ordinary frames (a few hundred bytes each) and a bound for any other kind.
pub const DEFAULT_FRAME_BUDGET: usize = 64 << 20;

/// Frames per block: the unit of allocation and eviction.
const BLOCK: u64 = 512;

/// Bytes of frames at which a block closes before it has [`BLOCK`] of them, so that the
/// budget evicts in steps of about this size. Also the least budget a store accepts.
pub const BLOCK_BYTES: usize = 1 << 20;

/// What a block's slots cost before any frame is in them.
const BLOCK_OVERHEAD: usize = BLOCK as usize * std::mem::size_of::<OnceLock<Frame>>();

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
    /// Bytes of frames kept before the oldest are evicted, counting what each frame
    /// holds and the slots of the blocks they sit in. Values below [`BLOCK_BYTES`] are
    /// raised to it.
    pub budget: usize,
}

impl Default for FrameStoreConfig {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_FRAME_CAPACITY,
            budget: DEFAULT_FRAME_BUDGET,
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
    /// Bytes the retained blocks hold: their slots and what their frames own.
    pub memory: usize,
    pub budget: usize,
}

impl FrameStats {
    /// Retained frames.
    pub fn count(&self) -> usize {
        (self.end.0 - self.first.0) as usize
    }
}

/// Write-once slots for up to `BLOCK` consecutive frames, the first with id `first`.
struct Block {
    first: u64,
    slots: Box<[OnceLock<Frame>]>,
}

impl Block {
    fn new(first: u64) -> Arc<Self> {
        Arc::new(Self {
            first,
            slots: (0..BLOCK).map(|_| OnceLock::new()).collect(),
        })
    }
}

/// Bytes a frame owns beyond its slot: its summary, its field names and values.
fn frame_heap_bytes(frame: &Frame) -> usize {
    fn name(s: &SmolStr) -> usize {
        if s.is_heap_allocated() { s.len() } else { 0 }
    }
    fn value(v: &Value) -> usize {
        match v {
            Value::Str(s) => s.capacity(),
            Value::Bytes(b) => b.capacity(),
            Value::List(items) => {
                items.capacity() * std::mem::size_of::<Value>()
                    + items.iter().map(value).sum::<usize>()
            }
            Value::Bool(_) | Value::Int(_) | Value::UInt(_) | Value::Float(_) => 0,
        }
    }
    name(&frame.kind)
        + frame.summary.capacity()
        + frame.fields.capacity() * std::mem::size_of::<(SmolStr, Value)>()
        + frame
            .fields
            .iter()
            .map(|(n, v)| name(n) + value(v))
            .sum::<usize>()
}

/// An immutable view shared by snapshots.
struct Published {
    first: u64,
    end: u64,
    /// In id order; a block's `first` says which ids it holds.
    blocks: Arc<[Arc<Block>]>,
    capacity: usize,
    memory: usize,
    budget: usize,
}

impl Published {
    fn stats(&self) -> FrameStats {
        FrameStats {
            first: FrameId(self.first),
            end: FrameId(self.end),
            capacity: self.capacity,
            memory: self.memory,
            budget: self.budget,
        }
    }

    /// The frame `id`, if it is retained.
    fn get(&self, id: u64) -> Option<&Frame> {
        if id < self.first || id >= self.end {
            return None;
        }
        // The last block that starts at or before `id`.
        let i = self
            .blocks
            .partition_point(|b| b.first <= id)
            .checked_sub(1)?;
        let block = &self.blocks[i];
        block.slots.get((id - block.first) as usize)?.get()
    }
}

struct Shared {
    current: Mutex<Arc<Published>>,
    dirty: AtomicBool,
}

/// The writer side: owned by the thread that decodes. See the module docs.
pub struct FrameStore {
    capacity: usize,
    budget: usize,
    shared: Arc<Shared>,
    blocks: VecDeque<Arc<Block>>,
    /// Bytes each block holds, in step with `blocks`.
    block_bytes: VecDeque<usize>,
    /// Frames in the newest block.
    block_len: usize,
    /// Bytes all blocks hold.
    memory: usize,
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
            .field("budget", &self.budget)
            .field("memory", &self.memory)
            .field("first", &self.first)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

impl FrameStore {
    pub fn new(config: FrameStoreConfig) -> Self {
        let capacity = config.capacity.max(1);
        let budget = config.budget.max(BLOCK_BYTES);
        let dir: Arc<[Arc<Block>]> = Arc::new([]);
        Self {
            capacity,
            budget,
            shared: Arc::new(Shared {
                current: Mutex::new(Arc::new(Published {
                    first: 0,
                    end: 0,
                    blocks: Arc::clone(&dir),
                    capacity,
                    memory: 0,
                    budget,
                })),
                dirty: AtomicBool::new(false),
            }),
            blocks: VecDeque::new(),
            block_bytes: VecDeque::new(),
            block_len: 0,
            memory: 0,
            first: 0,
            end: 0,
            dir,
            dir_dirty: false,
        }
    }

    /// A store keeping at most `capacity` frames within the default budget.
    pub fn with_capacity(capacity: usize) -> Self {
        Self::new(FrameStoreConfig {
            capacity,
            ..FrameStoreConfig::default()
        })
    }

    /// Bytes the blocks hold now, published or not.
    pub fn memory(&self) -> usize {
        self.memory
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
        let bytes = frame_heap_bytes(&frame);
        // A new block when there is none, the newest is full, or the frame would take
        // it past BLOCK_BYTES (a block always takes its first frame, whatever its size).
        let open = self.block_len > 0
            && self.block_len < BLOCK as usize
            && self
                .block_bytes
                .back()
                .is_some_and(|b| b + bytes <= BLOCK_BYTES);
        if !open {
            self.blocks.push_back(Block::new(id));
            self.block_bytes.push_back(BLOCK_OVERHEAD);
            self.block_len = 0;
            self.memory += BLOCK_OVERHEAD;
            self.dir_dirty = true;
        }
        let block = self.blocks.back().expect("a block for the new frame");
        // A fresh slot: `end` only moves forward, so no slot is set twice.
        let _ = block.slots[self.block_len].set(frame);
        self.block_len += 1;
        *self.block_bytes.back_mut().expect("the new frame's block") += bytes;
        self.memory += bytes;
        self.end += 1;
        self.first = self
            .first
            .max(self.end.saturating_sub(self.capacity as u64));
        // Over budget: the oldest blocks go, all but the newest.
        while self.memory > self.budget && self.blocks.len() > 1 {
            self.evict_oldest_block();
        }
        // Blocks whose every frame is past the count window go too.
        while self.blocks.len() > 1 && self.blocks[1].first <= self.first {
            self.evict_oldest_block();
        }
        FrameId(id)
    }

    /// Drop the oldest block, moving `first` past it. Never the last block.
    fn evict_oldest_block(&mut self) {
        debug_assert!(self.blocks.len() > 1);
        self.blocks.pop_front();
        let bytes = self
            .block_bytes
            .pop_front()
            .expect("the evicted block's bytes");
        self.memory -= bytes;
        let next = self
            .blocks
            .front()
            .expect("the block after the evicted one");
        self.first = self.first.max(next.first);
        self.dir_dirty = true;
    }

    fn publish(&mut self) {
        if self.dir_dirty {
            self.dir = self.blocks.iter().cloned().collect();
            self.dir_dirty = false;
        }
        let published = Arc::new(Published {
            first: self.first,
            end: self.end,
            blocks: Arc::clone(&self.dir),
            capacity: self.capacity,
            memory: self.memory,
            budget: self.budget,
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
        self.p.get(id.0)
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

/// Kind of the frame a [`CodecSink`] records when its codec cannot be made or panics.
pub const CODEC_ERROR_KIND: &str = "codec_error";

/// The sink's codec, made on first use when it comes from a factory.
enum Slot {
    Pending(Arc<dyn CodecFactory>),
    Ready(Box<dyn Codec>),
    /// The codec could not be made, or panicked again while resetting: decoding stopped.
    Broken,
}

/// Runs a codec over every received chunk on the ingest thread and records the frames.
///
/// Build it where it runs, inside [`Ingest::spawn_with`](crate::Ingest::spawn_with)'s
/// closure (see the module docs); a codec is not `Send`, so neither is the sink. Each
/// chunk costs one `decode` call and, if frames came out, one publication and at most
/// one wake. The codec is reset when the session disconnects, so a partial frame never
/// joins bytes from a later connection.
///
/// Nothing a codec does takes the ingest thread with it. A factory that fails to make
/// the codec gets a [`CODEC_ERROR_KIND`] frame (`Severity::Error`) over the first chunk,
/// and the sink decodes nothing more. A codec that panics gets the same kind of frame
/// over the chunk it panicked on and is reset; if the reset panics too, the sink stops
/// decoding.
pub struct CodecSink {
    codec: Slot,
    store: FrameStore,
    waker: Option<Box<dyn Fn() + Send>>,
    offset: u64,
    scratch: Vec<Frame>,
}

impl fmt::Debug for CodecSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let codec = match &self.codec {
            Slot::Pending(_) => "pending",
            Slot::Ready(_) => "ready",
            Slot::Broken => "broken",
        };
        f.debug_struct("CodecSink")
            .field("codec", &codec)
            .field("offset", &self.offset)
            .field("store", &self.store)
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
        Self::with_slot(Slot::Ready(codec), store, waker)
    }

    /// Decode with a codec `factory` makes on the first chunk, on the thread that runs
    /// the sink, so the codec never crosses threads. Otherwise the same as
    /// [`new`](Self::new).
    pub fn from_factory(
        factory: Arc<dyn CodecFactory>,
        store: FrameStore,
        waker: Option<Box<dyn Fn() + Send>>,
    ) -> Self {
        Self::with_slot(Slot::Pending(factory), store, waker)
    }

    fn with_slot(codec: Slot, store: FrameStore, waker: Option<Box<dyn Fn() + Send>>) -> Self {
        Self {
            codec,
            store,
            waker,
            offset: 0,
            scratch: Vec::new(),
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

fn error_frame(raw: Range<u64>, at: Instant, message: String, summary: String) -> Frame {
    Frame::new(CODEC_ERROR_KIND, raw, at)
        .with_field("error", message)
        .with_severity(Severity::Error)
        .with_summary(summary)
}

impl ChunkSink for CodecSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        let offset = self.offset;
        self.offset += bytes.len() as u64;
        if let Slot::Pending(factory) = &self.codec {
            let failure = match catch_unwind(AssertUnwindSafe(|| factory.create())) {
                Ok(Ok(codec)) => {
                    self.codec = Slot::Ready(codec);
                    None
                }
                Ok(Err(err)) => Some(err.to_string()),
                Err(payload) => Some(panic_message(payload.as_ref())),
            };
            if let Some(message) = failure {
                tracing::error!(%message, "the codec could not be made; decoding stops");
                let summary = format!("the codec could not be made: {message}");
                self.scratch
                    .push(error_frame(offset..self.offset, at, message, summary));
                self.codec = Slot::Broken;
                self.record();
                return;
            }
        }
        let Slot::Ready(codec) = &mut self.codec else {
            return;
        };
        let scratch = &mut self.scratch;
        let decoded = catch_unwind(AssertUnwindSafe(|| {
            codec.decode(bytes, at, offset, scratch);
        }));
        if let Err(payload) = decoded {
            let message = panic_message(payload.as_ref());
            tracing::error!(%message, "a codec panicked while decoding");
            let summary = format!("the codec panicked: {message}");
            self.scratch
                .push(error_frame(offset..self.offset, at, message, summary));
            self.reset_codec();
        }
        self.record();
    }

    fn on_disconnect(&mut self) {
        self.reset_codec();
    }
}

impl CodecSink {
    /// Reset a codec in use; one that panics doing so is dropped and decoding stops.
    fn reset_codec(&mut self) {
        let Slot::Ready(codec) = &mut self.codec else {
            return;
        };
        if catch_unwind(AssertUnwindSafe(|| codec.reset())).is_err() {
            tracing::error!("the codec panicked while resetting; decoding stops");
            self.codec = Slot::Broken;
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

    /// A frame of `n` carrying `bytes` of payload.
    fn heavy(n: u64, bytes: usize) -> Frame {
        frame(n).with_field("payload", Value::Bytes(vec![0xAA; bytes]))
    }

    /// Every retained id reads back as the frame pushed under it.
    fn check_readable(snap: &FrameSnapshot) {
        for (id, frame) in snap.frames() {
            assert_eq!(frame.field("n").and_then(|v| v.as_u64()), Some(id.0));
        }
        assert_eq!(snap.frames().count(), snap.count());
        assert!(snap.get(FrameId(snap.first().0.wrapping_sub(1))).is_none());
        assert!(snap.get(snap.end()).is_none());
    }

    #[test]
    fn the_budget_evicts_the_oldest_frames_by_bytes() {
        let budget = 4 * BLOCK_BYTES;
        let mut store = FrameStore::new(FrameStoreConfig {
            capacity: 100_000,
            budget,
        });
        let reader = store.reader();
        // 2000 frames of 10 KiB: 20 MiB, five times the budget.
        let held = {
            store.extend((0..100).map(|n| heavy(n, 10 << 10)));
            reader.snapshot()
        };
        store.extend((100..2000).map(|n| heavy(n, 10 << 10)));
        let snap = reader.snapshot();
        let stats = snap.stats();
        assert_eq!(stats.budget, budget);
        assert!(
            stats.memory <= budget,
            "{} bytes held against a budget of {budget}",
            stats.memory
        );
        assert_eq!(stats.memory, store.memory());
        // Near the budget, not far under it: at most one block's worth of slack.
        assert!(stats.memory > budget - BLOCK_BYTES - BLOCK_OVERHEAD);
        assert_eq!(snap.end(), FrameId(2000));
        assert!(snap.first() > FrameId(1500), "{:?}", snap.first());
        assert!(snap.count() < 400);
        check_readable(&snap);
        // An old snapshot still reads what it saw, evicted or not.
        assert_eq!(held.count(), 100);
        check_readable(&held);
        assert!(store.blocks.len() <= 5, "{} blocks", store.blocks.len());
    }

    #[test]
    fn a_block_closes_early_under_heavy_frames_and_lookups_still_work() {
        let mut store = FrameStore::default();
        let reader = store.reader();
        // 300 KiB frames: three to a block, not 512. Then small ones fill blocks again.
        store.extend((0..10).map(|n| heavy(n, 300 << 10)));
        assert_eq!(store.blocks.len(), 4, "three heavy frames per block");
        store.extend((10..1000).map(frame));
        assert!(store.blocks.len() <= 4 + 2, "{} blocks", store.blocks.len());
        let snap = reader.snapshot();
        assert_eq!((snap.first(), snap.end()), (FrameId(0), FrameId(1000)));
        check_readable(&snap);
        assert_eq!(snap.get(FrameId(9)).unwrap().raw, 9..10);
        assert_eq!(snap.get(FrameId(10)).unwrap().raw, 10..11);
    }

    #[test]
    fn a_frame_larger_than_the_budget_is_kept_until_the_next_arrives() {
        let mut store = FrameStore::new(FrameStoreConfig {
            capacity: 100_000,
            budget: 0, // raised to BLOCK_BYTES
        });
        let reader = store.reader();
        store.extend((0..3).map(frame));
        store.push(heavy(3, 3 * BLOCK_BYTES));
        let snap = reader.snapshot();
        assert_eq!((snap.first(), snap.end()), (FrameId(3), FrameId(4)));
        assert!(snap.stats().memory > 3 * BLOCK_BYTES);
        check_readable(&snap);
        store.push(frame(4));
        let snap = reader.snapshot();
        assert_eq!((snap.first(), snap.end()), (FrameId(4), FrameId(5)));
        assert!(snap.stats().memory <= BLOCK_BYTES);
        check_readable(&snap);
    }

    #[test]
    fn random_frame_sizes_keep_memory_within_the_budget_and_ids_consistent() {
        // A small xorshift so the test needs no dependency and replays the same way.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..6 {
            let capacity = [1, 7, 100, 1000, 100_000, 600][round];
            let budget = [
                0,
                BLOCK_BYTES,
                3 * BLOCK_BYTES,
                8 * BLOCK_BYTES,
                BLOCK_BYTES,
                2 * BLOCK_BYTES,
            ][round];
            let mut store = FrameStore::new(FrameStoreConfig { capacity, budget });
            let reader = store.reader();
            let mut largest = 0;
            let mut last_end = 0;
            for n in 0..3000u64 {
                let bytes = match next() % 10 {
                    0 => (next() % (2 << 20)) as usize, // up to 2 MiB
                    1..=3 => (next() % (64 << 10)) as usize,
                    _ => (next() % 256) as usize,
                };
                largest = largest.max(bytes);
                store.push(heavy(n, bytes));
                if next() % 50 == 0 {
                    let snap = reader.snapshot();
                    let stats = snap.stats();
                    assert_eq!(stats.end, FrameId(n + 1));
                    assert!(stats.end.0 >= last_end);
                    last_end = stats.end.0;
                    assert!(snap.count() <= capacity, "{} > {capacity}", snap.count());
                    let slack = BLOCK_BYTES + BLOCK_OVERHEAD + largest;
                    assert!(
                        stats.memory <= stats.budget + slack,
                        "round {round}: {} bytes held, budget {}, slack {slack}",
                        stats.memory,
                        stats.budget
                    );
                    check_readable(&snap);
                }
            }
        }
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
        assert_eq!(kinds, ["chunk", CODEC_ERROR_KIND, "chunk"]);
        let error = snap.get(FrameId(1)).unwrap();
        assert_eq!(error.raw, 2..5);
        assert_eq!(error.severity, Severity::Error);
        assert_eq!(
            error.field("error").and_then(|v| v.as_str()),
            Some("bad byte")
        );
        assert_eq!(resets.load(Ordering::Relaxed), 1);
    }

    /// Counts the codecs it makes, and the thread each was made on.
    struct Counting {
        made: Arc<AtomicUsize>,
        threads: Arc<std::sync::Mutex<Vec<std::thread::ThreadId>>>,
        fail: bool,
    }

    impl CodecFactory for Counting {
        fn info(&self) -> CodecInfo {
            CodecInfo::default()
        }

        fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
            if self.fail {
                return Err(CodecError::Internal("no plugin today".into()));
            }
            self.made.fetch_add(1, Ordering::Relaxed);
            self.threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            Ok(Box::new(PerChunk {
                resets: Arc::new(AtomicUsize::new(0)),
                panic_on: None,
            }))
        }
    }

    #[test]
    fn a_factory_makes_the_codec_on_the_first_chunk_on_the_sink_thread() {
        let made = Arc::new(AtomicUsize::new(0));
        let threads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let factory: Arc<dyn CodecFactory> = Arc::new(Counting {
            made: Arc::clone(&made),
            threads: Arc::clone(&threads),
            fail: false,
        });
        let store = FrameStore::default();
        let reader = store.reader();
        // Only the factory and the store cross to the thread; the codec is made there.
        let sink_thread = std::thread::spawn(move || {
            let mut sink = CodecSink::from_factory(factory, store, None).starting_at(10);
            sink.on_disconnect();
            sink.on_chunk(b"ab", Instant::now());
            sink.on_chunk(b"c", Instant::now());
            std::thread::current().id()
        })
        .join()
        .unwrap();
        assert_eq!(made.load(Ordering::Relaxed), 1);
        assert_eq!(*threads.lock().unwrap(), [sink_thread]);
        let ranges: Vec<_> = reader
            .snapshot()
            .frames()
            .map(|(_, f)| f.raw.clone())
            .collect();
        assert_eq!(ranges, [10..12, 12..13]);
    }

    #[test]
    fn a_factory_that_fails_leaves_one_error_frame() {
        let factory: Arc<dyn CodecFactory> = Arc::new(Counting {
            made: Arc::new(AtomicUsize::new(0)),
            threads: Arc::default(),
            fail: true,
        });
        let mut sink = CodecSink::from_factory(factory, FrameStore::default(), None);
        let reader = sink.reader();
        sink.on_chunk(b"abc", Instant::now());
        sink.on_chunk(b"def", Instant::now());
        sink.on_disconnect();
        let snap = reader.snapshot();
        assert_eq!(snap.count(), 1);
        let error = snap.get(FrameId(0)).unwrap();
        assert_eq!(error.kind, CODEC_ERROR_KIND);
        assert_eq!(error.raw, 0..3);
        assert_eq!(
            error.field("error").and_then(|v| v.as_str()),
            Some("no plugin today")
        );
        assert_eq!(sink.offset(), 6);
    }
}
