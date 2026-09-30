//! The scrollback store: raw bytes in pages, a compact line index, and cheap consistent
//! snapshots for the UI.
//!
//! # Threads
//!
//! [`Store`] is owned by one thread (the ingest thread) and is the only writer.
//! [`StoreReader`] (`Clone + Send + Sync`) hands out [`Snapshot`]s to any thread. After
//! every append the store publishes an immutable `Published` record (line and byte
//! bounds plus `Arc`s of three small directories) by swapping one `Arc` under a mutex;
//! a snapshot clones that `Arc` under the same mutex. The lock is held for a pointer
//! swap or an increment, never for parsing, copying or I/O, so neither side waits on
//! the other for more than a few hundred nanoseconds.
//!
//! Data never moves after it is written. Raw pages, index arrays and decoded-text pages
//! are append-only buffers (`buf::AppendBuf`): the writer fills slots past the published
//! length and readers only look below it, so the page being filled is readable while it
//! fills, without a lock or a copy. A snapshot holds `Arc`s to everything it can reach,
//! so eviction never invalidates it.
//!
//! # Memory model
//!
//! Everything below counts against one byte budget (default 256 MiB,
//! [`StoreConfig::budget`]); [`StoreStats::memory`] reports the total.
//!
//! | Part | Cost |
//! |------|------|
//! | Raw pages | 64 KiB each, allocated whole. Retained raw bytes plus under 64 KiB. |
//! | Line entries | 5 bytes per line: a `u32` offset from its block's base and a flag byte, in blocks of 4096 lines. |
//! | Timestamps | 12 bytes per change of arrival time, so about once per chunk, not per line. |
//! | Decoded text | Only for lines the parser transformed and for local lines: a 12-byte reference, then the text, two separator bytes and its runs (about 5 bytes per run) in 64 KiB text pages. |
//! | Open block | 116 KiB while being filled; compacted to its used size when sealed. |
//! | Line in progress | The parser's line buffer; a published copy only if the line is not plain. |
//!
//! A plain line (printable UTF-8 in the default style, ended by LF or CRLF, possibly
//! preceded by CRs) stores no text at all: [`Snapshot`] reads it back from the raw page.
//! Lines with escape sequences, overwrites, tabs or invalid UTF-8 cost roughly their
//! decoded size again. Measured overhead above the raw bytes, 32 MiB of each firehose
//! content in 4 KiB chunks:
//!
//! | Content | Bytes per line | Overhead |
//! |---------|----------------|----------|
//! | `Text` (plain logs, CRLF) | 76 | 7.3% |
//! | Short lines (the gate test) | 35 | 15% |
//! | `LongLines` | 2322 | 0.9% |
//! | `Mixed` | 710 | 12.8% |
//! | `MixedEol` (bare CRs overwrite) | 100 | 32% |
//! | `Ansi` (SGR on every word) | 155 | 98% |
//! | `Binary` (invalid UTF-8 becomes 3-byte U+FFFD) | 264 | 122% |
//!
//! The budget bounds the total whatever the content: a heavier stream simply retains
//! fewer raw bytes.
//!
//! When an append takes the total over budget, the oldest whole page is evicted with
//! every line that starts in it, and blocks and text pages no longer referenced by a
//! retained line are freed. Line ids are never reused; [`LineSource::first_line`]
//! advances. The page holding the start of the line in progress is never evicted, so a
//! retained line always has all of its raw bytes. If local lines alone exceed the budget
//! the oldest lines are evicted without touching raw bytes.
//!
//! Snapshots are not counted: one held across evictions (a paused view) keeps the pages
//! it references alive until it is dropped.
//!
//! [`LineSource::first_line`]: crate::text::LineSource::first_line

mod buf;
mod export;
mod hex;
mod index;
mod search;
mod snapshot;

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;

use crate::ansi::{
    AnsiParser, DEFAULT_MAX_LINE_BYTES, MAX_LINE_BYTES_LIMIT, MAX_RUNS, MIN_LINE_BYTES,
    MOTION_WIDTH, ParsedLine, TAB_WIDTH,
};
use crate::text::{Direction, Epoch, LineId, Style, StyleRun};

use buf::{AppendBuf, AppendWriter};
use index::{BLOCK_LINES, Block, BlockWriter, LineFlags};

pub use export::{TextOptions, Timestamps, format_utc};
pub use hex::{HexStyles, HexView};
pub use search::smart_case_insensitive;
pub use snapshot::{RawIter, Snapshot};

/// Size of a raw page.
pub const PAGE_SIZE: usize = 64 * 1024;
/// The default memory budget per store.
pub const DEFAULT_BUDGET: usize = 256 * 1024 * 1024;
/// Size of a page of decoded text (a larger record gets a page of its own size).
const TEXT_PAGE_SIZE: usize = 64 * 1024;
const B: u64 = BLOCK_LINES as u64;
const P: u64 = PAGE_SIZE as u64;
/// Bookkeeping not otherwise counted: the published record, the store struct, slack.
const FIXED_OVERHEAD: usize = 4096;
/// History the minimum budget keeps room for beyond the unevictable parts.
const HISTORY_HEADROOM: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// Upper bound on the store's memory, raw pages and index together. Values below
    /// [`StoreConfig::min_budget`] are raised to it.
    pub budget: usize,
    /// A received line with this many raw bytes and no LF is ended early.
    pub max_line_bytes: usize,
    /// Time zero for timestamps. `None` means when the store is created.
    pub epoch: Option<Epoch>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            budget: DEFAULT_BUDGET,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            epoch: None,
        }
    }
}

impl StoreConfig {
    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget,
            ..Self::default()
        }
    }

    /// The smallest budget that covers everything eviction cannot free, at its worst,
    /// with a megabyte of history to spare: the parser holding the widest possible line
    /// in progress, a published copy of that line, the pages holding it, the open index
    /// block, the open text page and the record buffer. With at least this much, the
    /// store is back within budget after every append whatever the input. About 3 MiB
    /// at the default 64 KiB line limit.
    pub fn min_budget(&self) -> usize {
        let line = self
            .max_line_bytes
            .clamp(MIN_LINE_BYTES, MAX_LINE_BYTES_LIMIT);
        let cols = line + MOTION_WIDTH;
        let parser = AnsiParser::worst_case_heap(line);
        let tail_copy = 4 * cols + MAX_RUNS * size_of::<StyleRun>() + 64;
        let line_pages = (line / PAGE_SIZE + 2) * PAGE_SIZE;
        parser
            + tail_copy
            + line_pages
            + index::OPEN_BLOCK_BYTES
            + 2 * TEXT_PAGE_SIZE
            + FIXED_OVERHEAD
            + HISTORY_HEADROOM
    }
}

/// What one append did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppendReport {
    /// Lines created or changed: from the line that was in progress before (if any)
    /// through the newest line.
    pub changed: Range<LineId>,
    /// How many line ids are new.
    pub new_lines: usize,
    /// The newest line is still waiting for its LF.
    pub incomplete: bool,
    /// Lines evicted to stay within budget.
    pub evicted_lines: usize,
    /// Raw bytes evicted to stay within budget.
    pub evicted_bytes: u64,
}

/// Counters describing a store at one publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreStats {
    pub first_line: LineId,
    pub end_line: LineId,
    /// Offset of the oldest retained raw byte; also the raw bytes evicted so far.
    pub raw_start: u64,
    /// Raw bytes ever received.
    pub raw_len: u64,
    /// Bytes the store accounts for (see the module's memory model).
    pub memory: usize,
    pub budget: usize,
    pub pages: usize,
    pub blocks: usize,
    pub text_pages: usize,
    /// Bytes of retained decoded-text pages.
    pub text_bytes: usize,
    pub evicted_lines: u64,
}

impl StoreStats {
    /// Retained lines.
    pub fn lines(&self) -> usize {
        (self.end_line.0 - self.first_line.0) as usize
    }

    /// Retained raw bytes.
    pub fn retained_bytes(&self) -> u64 {
        self.raw_len - self.raw_start
    }
}

/// An immutable view of the store at one moment, shared by snapshots.
pub(crate) struct Published {
    epoch: Epoch,
    first_line: u64,
    /// One past the last line in the index; the line in progress, if any, has this id.
    committed_end: u64,
    end_line: u64,
    /// Where the line after the committed ones starts.
    committed_raw_end: u64,
    raw_start: u64,
    raw_len: u64,
    pages: Arc<[Arc<AppendBuf<u8>>]>,
    first_page: u64,
    blocks: Arc<[Arc<Block>]>,
    text_pages: Arc<[Arc<AppendBuf<u8>>]>,
    first_text_seq: u64,
    tail: Option<Tail>,
    stats: StoreStats,
}

/// The line in progress as published.
#[derive(Clone, Debug)]
pub(crate) struct Tail {
    start: u64,
    ns: u64,
    text: TailText,
}

#[derive(Clone, Debug)]
pub(crate) enum TailText {
    /// The text is raw bytes `start + lead .. start + lead + len`.
    Plain {
        lead: u64,
        len: usize,
    },
    Decoded(Arc<(String, Vec<StyleRun>)>),
}

impl TailText {
    fn heap_bytes(&self) -> usize {
        match self {
            TailText::Plain { .. } => 0,
            TailText::Decoded(d) => d.0.len() + d.1.len() * size_of::<StyleRun>() + 64,
        }
    }
}

struct Shared {
    current: Mutex<Arc<Published>>,
}

/// A handle for readers on other threads. Cheap to clone.
#[derive(Clone)]
pub struct StoreReader {
    shared: Arc<Shared>,
}

impl StoreReader {
    /// A consistent view of the store now: a fixed line range and raw range. Costs one
    /// uncontended lock and one `Arc` clone; no page data is copied.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot::new(Arc::clone(&self.shared.current.lock()))
    }

    pub fn stats(&self) -> StoreStats {
        self.shared.current.lock().stats
    }
}

impl fmt::Debug for StoreReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreReader")
            .field("stats", &self.stats())
            .finish()
    }
}

/// Where the decoded text of local and transformed lines is written.
struct TextPage {
    buf: Arc<AppendBuf<u8>>,
    /// The newest line with a record here; the page can go once it is evicted.
    last_line: u64,
}

/// Incremental check that the line in progress is still plain, so its text can be
/// borrowed from the raw pages instead of copied.
#[derive(Clone, Copy, Debug)]
struct TailCheck {
    plain: bool,
    lead: Option<u8>,
    /// Bytes of the line's text already confirmed equal to its raw bytes.
    verified: usize,
}

impl TailCheck {
    fn new() -> Self {
        Self {
            plain: true,
            lead: None,
            verified: 0,
        }
    }
}

/// The scrollback store. See the module docs.
pub struct Store {
    budget: usize,
    parser: AnsiParser,
    w: Writer,
    shared: Arc<Shared>,
}

/// Everything the parser callback needs, split from the parser so both can be borrowed.
struct Writer {
    epoch: Epoch,
    // Raw pages: page `first_page + i` covers `(first_page + i) * PAGE_SIZE ..`.
    pages: Vec<Arc<AppendBuf<u8>>>,
    open_page: Option<AppendWriter<u8>>,
    first_page: u64,
    raw_start: u64,
    raw_len: u64,
    // Line index.
    blocks: Vec<Arc<Block>>,
    open_block: Option<BlockWriter>,
    sealed_block_bytes: usize,
    first_line: u64,
    next_line: u64,
    /// Raw offset where the line in progress starts.
    line_start: u64,
    /// Arrival time of the line in progress's first byte.
    line_ns: u64,
    chunk_ns: u64,
    tail_check: TailCheck,
    // Decoded text.
    text_pages: Vec<TextPage>,
    open_text: Option<AppendWriter<u8>>,
    first_text_seq: u64,
    next_text_seq: u64,
    text_bytes: usize,
    record: Vec<u8>,
    // Published directories, rebuilt only when a page, block or text page comes or goes.
    dirs_dirty: bool,
    page_dir: Arc<[Arc<AppendBuf<u8>>]>,
    block_dir: Arc<[Arc<Block>]>,
    text_dir: Arc<[Arc<AppendBuf<u8>>]>,
    tail_bytes: usize,
    evicted_lines: u64,
}

impl Default for Store {
    fn default() -> Self {
        Self::new(StoreConfig::default())
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("budget", &self.budget)
            .field("first_line", &self.w.first_line)
            .field("next_line", &self.w.next_line)
            .field("raw_start", &self.w.raw_start)
            .field("raw_len", &self.w.raw_len)
            .finish_non_exhaustive()
    }
}

impl Store {
    pub fn new(config: StoreConfig) -> Self {
        let budget = config.budget.max(config.min_budget());
        let parser = AnsiParser::with_max_line_bytes(config.max_line_bytes);
        let epoch = config.epoch.unwrap_or_else(Epoch::now);
        let w = Writer {
            epoch,
            pages: Vec::new(),
            open_page: None,
            first_page: 0,
            raw_start: 0,
            raw_len: 0,
            blocks: Vec::new(),
            open_block: None,
            sealed_block_bytes: 0,
            first_line: 0,
            next_line: 0,
            line_start: 0,
            line_ns: 0,
            chunk_ns: 0,
            tail_check: TailCheck::new(),
            text_pages: Vec::new(),
            open_text: None,
            first_text_seq: 0,
            next_text_seq: 0,
            text_bytes: 0,
            record: Vec::new(),
            dirs_dirty: false,
            page_dir: Arc::new([]),
            block_dir: Arc::new([]),
            text_dir: Arc::new([]),
            tail_bytes: 0,
            evicted_lines: 0,
        };
        let mut store = Self {
            budget,
            parser,
            w,
            shared: Arc::new(Shared {
                current: Mutex::new(Arc::new(Published::empty(epoch, budget))),
            }),
        };
        store.publish();
        store
    }

    /// A store with the default configuration and this budget.
    pub fn with_budget(budget: usize) -> Self {
        Self::new(StoreConfig::with_budget(budget))
    }

    pub fn reader(&self) -> StoreReader {
        StoreReader {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Same as `self.reader().snapshot()`.
    pub fn snapshot(&self) -> Snapshot {
        self.reader().snapshot()
    }

    pub fn stats(&self) -> StoreStats {
        self.shared.current.lock().stats
    }

    pub fn epoch(&self) -> Epoch {
        self.w.epoch
    }

    /// The budget in effect (the configured one, raised to the minimum).
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// One past the newest line, counting the line in progress.
    pub fn end(&self) -> LineId {
        LineId(self.w.next_line + u64::from(self.parser.current().is_some()))
    }

    /// Append one received chunk that arrived at `at`: store the bytes, parse them once,
    /// index the lines that ended and publish. O(chunk) plus amortised O(1) per line.
    pub fn append(&mut self, bytes: &[u8], at: Instant) -> AppendReport {
        let old_next = self.w.next_line;
        let old_end = self.end().0;
        if bytes.is_empty() {
            return AppendReport {
                changed: LineId(old_end)..LineId(old_end),
                new_lines: 0,
                incomplete: self.parser.current().is_some(),
                evicted_lines: 0,
                evicted_bytes: 0,
            };
        }
        let ns = self.w.ns(at);
        self.w.chunk_ns = ns;
        if self.parser.current().is_none() {
            self.w.line_ns = ns;
        }
        self.w.write_raw(bytes);
        let w = &mut self.w;
        self.parser.feed(bytes, |line| w.commit_rx(line));
        if let Some(line) = self.parser.current() {
            self.w.check_tail(line);
        }
        // Count the copy of the line in progress this append will publish, so eviction
        // makes room for it.
        self.w.tail_bytes = self
            .parser
            .current()
            .map_or(0, |line| self.w.tail_heap(line));
        let (evicted_lines, evicted_bytes) = self.evict();
        self.publish();
        let end = self.end().0;
        AppendReport {
            changed: LineId(old_next.max(self.w.first_line))..LineId(end),
            new_lines: (end - old_end) as usize,
            incomplete: self.parser.current().is_some(),
            evicted_lines,
            evicted_bytes,
        }
    }

    /// Insert lines that are not part of the received stream (a sent-command echo, a
    /// connect notice), stamped now. See [`Store::append_local_at`].
    pub fn append_local(&mut self, text: &str, direction: Direction) -> Range<LineId> {
        self.append_local_at(text, direction, Instant::now())
    }

    /// Insert local lines stamped `at`. `text` is split at `\n`; CRs and other control
    /// characters are dropped and tabs expanded. A received line still waiting for its
    /// LF is ended first (marked incomplete), so later bytes start a new line after the
    /// local ones. Local lines have an empty raw range positioned at the current end of
    /// the raw stream. `Direction::Rx` is treated as `Notice`.
    pub fn append_local_at(
        &mut self,
        text: &str,
        direction: Direction,
        at: Instant,
    ) -> Range<LineId> {
        let direction = match direction {
            Direction::Rx => Direction::Notice,
            other => other,
        };
        let w = &mut self.w;
        self.parser.break_line(|line| w.commit_rx(line));
        let ns = w.ns(at);
        let first = w.next_line;
        for piece in local_lines(text) {
            let runs = [StyleRun {
                len: piece.len(),
                style: Style::default(),
            }];
            let runs = if piece.is_empty() { &[][..] } else { &runs[..] };
            let dec = w.write_record(&piece, runs);
            let start = w.line_start;
            w.push_line(start, LineFlags::decoded(direction, true), ns, Some(dec));
        }
        let end = w.next_line;
        w.tail_bytes = 0;
        self.evict();
        self.publish();
        LineId(first.max(self.w.first_line))..LineId(end)
    }

    /// Bytes the store accounts for right now.
    fn memory(&self) -> usize {
        let w = &self.w;
        let dirs = 2 * size_of::<usize>() * (w.pages.len() + w.blocks.len() + w.text_pages.len());
        w.pages.len() * PAGE_SIZE
            + w.sealed_block_bytes
            + w.open_block.as_ref().map_or(0, |b| b.block.heap_bytes())
            + w.text_bytes
            + w.record.capacity()
            + w.tail_bytes
            + self.parser.heap_bytes()
            + dirs
            + FIXED_OVERHEAD
    }

    /// Evict oldest-first until within budget. Returns lines and raw bytes evicted.
    fn evict(&mut self) -> (usize, u64) {
        let (mut lines, mut bytes) = (0u64, 0u64);
        while self.memory() > self.budget {
            let w = &mut self.w;
            let keep_page = w.line_start / P;
            let before = w.first_line;
            if !w.pages.is_empty() && w.first_page < keep_page {
                w.pages.remove(0);
                w.first_page += 1;
                let new_start = w.first_page * P;
                bytes += new_start - w.raw_start;
                w.raw_start = new_start;
                if w.pages.is_empty() {
                    w.open_page = None;
                }
                while w.first_line < w.next_line && w.start_of(w.first_line) < new_start {
                    w.first_line += 1;
                }
            } else if w.first_line < w.next_line {
                w.first_line = ((w.first_line / B + 1) * B).min(w.next_line);
            } else {
                break;
            }
            lines += w.first_line - before;
            w.drop_unreferenced();
            w.dirs_dirty = true;
        }
        self.w.evicted_lines += lines;
        (lines as usize, bytes)
    }

    fn publish(&mut self) {
        let tail = self.parser.current().map(|line| self.w.tail(line));
        self.w.tail_bytes = tail.as_ref().map_or(0, |t| t.text.heap_bytes());
        let memory = self.memory();
        let w = &mut self.w;
        if w.dirs_dirty {
            w.page_dir = w.pages.iter().cloned().collect();
            w.block_dir = w.blocks.iter().cloned().collect();
            w.text_dir = w.text_pages.iter().map(|t| Arc::clone(&t.buf)).collect();
            w.dirs_dirty = false;
        }
        let end_line = w.next_line + u64::from(tail.is_some());
        let published = Published {
            epoch: w.epoch,
            first_line: w.first_line,
            committed_end: w.next_line,
            end_line,
            committed_raw_end: w.line_start,
            raw_start: w.raw_start,
            raw_len: w.raw_len,
            pages: Arc::clone(&w.page_dir),
            first_page: w.first_page,
            blocks: Arc::clone(&w.block_dir),
            text_pages: Arc::clone(&w.text_dir),
            first_text_seq: w.first_text_seq,
            tail,
            stats: StoreStats {
                first_line: LineId(w.first_line),
                end_line: LineId(end_line),
                raw_start: w.raw_start,
                raw_len: w.raw_len,
                memory,
                budget: self.budget,
                pages: w.pages.len(),
                blocks: w.blocks.len(),
                text_pages: w.text_pages.len(),
                text_bytes: w.text_bytes,
                evicted_lines: w.evicted_lines,
            },
        };
        let old = std::mem::replace(&mut *self.shared.current.lock(), Arc::new(published));
        // Dropped outside the lock: it may free a directory.
        drop(old);
    }
}

impl Writer {
    fn ns(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch.instant).as_nanos() as u64
    }

    fn write_raw(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.open_page.as_ref().is_none_or(AppendWriter::is_full) {
                let (buf, writer) = AppendBuf::new(PAGE_SIZE);
                if self.pages.is_empty() {
                    self.first_page = self.raw_len / P;
                    self.raw_start = self.raw_len;
                }
                self.pages.push(buf);
                self.open_page = Some(writer);
                self.dirs_dirty = true;
            }
            let page = self.open_page.as_mut().expect("just opened");
            let n = page.extend(bytes);
            self.raw_len += n as u64;
            bytes = &bytes[n..];
        }
    }

    fn raw(&self, range: Range<u64>) -> RawIter<'_> {
        RawIter::new(&self.pages, self.first_page, range)
    }

    fn raw_eq(&self, at: u64, expected: &[u8]) -> bool {
        let mut rest = expected;
        for slice in self.raw(at..at + expected.len() as u64) {
            if !rest.starts_with(slice) {
                return false;
            }
            rest = &rest[slice.len()..];
        }
        rest.is_empty()
    }

    /// Raw bytes `range`, at most `max` of them, as a small array.
    fn raw_prefix(&self, range: Range<u64>, max: usize) -> ([u8; 8], usize) {
        let mut out = [0u8; 8];
        let mut n = 0;
        let end = range.end.min(range.start + max.min(8) as u64);
        for slice in self.raw(range.start..end) {
            out[n..n + slice.len()].copy_from_slice(slice);
            n += slice.len();
        }
        (out, n)
    }

    fn start_of(&self, id: u64) -> u64 {
        let block = &self.blocks[(id / B - self.blocks[0].first / B) as usize];
        block.start((id - block.first) as usize)
    }

    /// Confirm the plain prefix of the line in progress, so it can be published without
    /// a copy.
    fn check_tail(&mut self, line: ParsedLine<'_>) {
        if !line.simple {
            self.tail_check.plain = false;
        }
        if !self.tail_check.plain || line.text.is_empty() {
            return;
        }
        let start = self.line_start;
        let lead = match self.tail_check.lead {
            Some(lead) => lead,
            None => {
                let (head, n) = self.raw_prefix(start..self.raw_len, 4);
                let lead = head[..n].iter().take_while(|&&b| b == b'\r').count() as u8;
                self.tail_check.lead = Some(lead);
                lead
            }
        };
        let verified = self.tail_check.verified;
        let at = start + u64::from(lead) + verified as u64;
        let ok = lead <= 3
            && at + (line.text.len() - verified) as u64 <= self.raw_len
            && self.raw_eq(at, &line.text.as_bytes()[verified..]);
        if ok {
            self.tail_check.verified = line.text.len();
        } else {
            self.tail_check.plain = false;
        }
    }

    /// If the line from `start` to `end` is plain, its lead and trail byte counts.
    fn plain_layout(&mut self, start: u64, end: u64, line: &ParsedLine<'_>) -> Option<(u8, u8)> {
        if !line.simple {
            return None;
        }
        let text = line.text;
        let raw_len = end - start;
        if text.is_empty() {
            // Only CRs and the LF: split them into up to 3 leading and 3 trailing.
            if raw_len > 6 {
                return None;
            }
            let (bytes, n) = self.raw_prefix(start..end, 6);
            let all_eol = bytes[..n].iter().all(|&b| b == b'\r' || b == b'\n');
            let lead = raw_len.saturating_sub(3).min(3) as u8;
            let trail = (raw_len - u64::from(lead)) as u8;
            return all_eol.then_some((lead, trail));
        }
        self.check_tail(*line);
        if !self.tail_check.plain {
            return None;
        }
        let lead = u64::from(self.tail_check.lead?);
        let text_end = start + lead + text.len() as u64;
        let trail = end.checked_sub(text_end)?;
        if trail > 3 {
            return None;
        }
        let (bytes, n) = self.raw_prefix(text_end..end, 3);
        bytes[..n]
            .iter()
            .all(|&b| b == b'\r' || b == b'\n')
            .then_some((lead as u8, trail as u8))
    }

    /// Index a received line the parser ended.
    fn commit_rx(&mut self, line: ParsedLine<'_>) {
        let start = self.line_start;
        let end = start + line.raw_len as u64;
        let ns = self.line_ns;
        match self.plain_layout(start, end, &line) {
            Some((lead, trail)) => {
                self.push_line(
                    start,
                    LineFlags::plain(line.complete, lead, trail),
                    ns,
                    None,
                );
            }
            None => {
                let dec = self.write_record(line.text, line.runs);
                self.push_line(
                    start,
                    LineFlags::decoded(Direction::Rx, line.complete),
                    ns,
                    Some(dec),
                );
            }
        }
        self.line_start = end;
        self.line_ns = self.chunk_ns;
        self.tail_check = TailCheck::new();
    }

    /// Index a line. A block is sealed the moment it fills, so the open block is never
    /// full: every block that eviction can drain is a sealed one.
    fn push_line(&mut self, start: u64, flags: LineFlags, ns: u64, dec: Option<(u32, u32)>) {
        let open = match &mut self.open_block {
            Some(open) => open,
            None => {
                debug_assert_eq!(self.next_line % B, 0);
                let open = BlockWriter::new(self.next_line, start);
                self.blocks.push(Arc::clone(&open.block));
                self.dirs_dirty = true;
                self.open_block.insert(open)
            }
        };
        open.push(start, flags, ns, dec);
        self.next_line += 1;
        if open.is_full() {
            let sealed = open.seal();
            self.sealed_block_bytes += sealed.heap_bytes();
            let last = self.blocks.last_mut().expect("the open block is listed");
            debug_assert!(Arc::ptr_eq(last, &open.block));
            *last = sealed;
            self.open_block = None;
            self.dirs_dirty = true;
        }
    }

    /// Write the decoded text of the line about to be pushed. Returns its location.
    fn write_record(&mut self, text: &str, runs: &[StyleRun]) -> (u32, u32) {
        index::encode_record(&mut self.record, text, runs);
        let len = self.record.len();
        if self.open_text.as_ref().is_none_or(|w| w.remaining() < len) {
            let cap = TEXT_PAGE_SIZE.max(len + index::TEXT_PAGE_HEADER.len());
            let (buf, mut writer) = AppendBuf::new(cap);
            writer.extend(index::TEXT_PAGE_HEADER);
            if self.text_pages.is_empty() {
                self.first_text_seq = self.next_text_seq;
            }
            self.next_text_seq += 1;
            self.text_bytes += buf.heap_bytes();
            self.text_pages.push(TextPage {
                buf,
                last_line: self.next_line,
            });
            self.open_text = Some(writer);
            self.dirs_dirty = true;
        }
        let writer = self.open_text.as_mut().expect("just opened");
        let off = writer.len();
        writer.extend(&self.record);
        if self.record.capacity() > TEXT_PAGE_SIZE {
            // One huge line must not pin its encoding buffer against the budget.
            self.record = Vec::new();
        }
        let page = self.text_pages.last_mut().expect("open page is listed");
        page.last_line = self.next_line;
        ((self.next_text_seq - 1) as u32, off as u32)
    }

    /// Free blocks and text pages that hold no retained line.
    fn drop_unreferenced(&mut self) {
        // The open block is never full (see `push_line`), so it never qualifies; the
        // check keeps it that way should that ever change.
        let open_first = self.open_block.as_ref().map(|b| b.block.first);
        let dead_blocks = self
            .blocks
            .iter()
            .take_while(|b| b.first + B <= self.first_line && Some(b.first) != open_first)
            .count();
        for block in self.blocks.drain(..dead_blocks) {
            self.sealed_block_bytes -= block.heap_bytes();
        }
        let dead_text = self
            .text_pages
            .iter()
            .take_while(|t| t.last_line < self.first_line)
            .count();
        for page in self.text_pages.drain(..dead_text) {
            self.text_bytes -= page.buf.heap_bytes();
        }
        self.first_text_seq += dead_text as u64;
        if self.text_pages.is_empty() {
            self.open_text = None;
        }
    }

    /// The raw lead of the line in progress if its text can be read from the raw pages;
    /// `None` if it must be published as a decoded copy.
    fn plain_tail_lead(&self, line: ParsedLine<'_>) -> Option<u64> {
        if !(self.tail_check.plain && line.simple) {
            return None;
        }
        if line.text.is_empty() {
            return Some(0);
        }
        self.tail_check
            .lead
            .filter(|_| self.tail_check.verified == line.text.len())
            .map(u64::from)
    }

    /// Heap the published copy of the line in progress will take (see `TailText`).
    fn tail_heap(&self, line: ParsedLine<'_>) -> usize {
        match self.plain_tail_lead(line) {
            Some(_) => 0,
            None => line.text.len() + size_of_val(line.runs) + 64,
        }
    }

    fn tail(&self, line: ParsedLine<'_>) -> Tail {
        let text = match self.plain_tail_lead(line) {
            Some(lead) => TailText::Plain {
                lead,
                len: line.text.len(),
            },
            None => TailText::Decoded(Arc::new((line.text.to_owned(), line.runs.to_vec()))),
        };
        Tail {
            start: self.line_start,
            ns: self.line_ns,
            text,
        }
    }
}

/// Split local text into display lines: at `\n`, CRs and other controls dropped, tabs
/// expanded. A trailing `\n` does not add an empty line; empty text is one empty line.
fn local_lines(text: &str) -> Vec<String> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    text.split('\n')
        .map(|piece| {
            let mut out = String::with_capacity(piece.len());
            let mut col = 0;
            for c in piece.chars() {
                if c == '\t' {
                    let next = (col / TAB_WIDTH + 1) * TAB_WIDTH;
                    out.extend(std::iter::repeat_n(' ', next - col));
                    col = next;
                } else if !c.is_control() {
                    out.push(c);
                    col += 1;
                }
            }
            out
        })
        .collect()
}

impl Published {
    fn empty(epoch: Epoch, budget: usize) -> Self {
        Self {
            epoch,
            first_line: 0,
            committed_end: 0,
            end_line: 0,
            committed_raw_end: 0,
            raw_start: 0,
            raw_len: 0,
            pages: Arc::new([]),
            first_page: 0,
            blocks: Arc::new([]),
            text_pages: Arc::new([]),
            first_text_seq: 0,
            tail: None,
            stats: StoreStats {
                first_line: LineId(0),
                end_line: LineId(0),
                raw_start: 0,
                raw_len: 0,
                memory: 0,
                budget,
                pages: 0,
                blocks: 0,
                text_pages: 0,
                text_bytes: 0,
                evicted_lines: 0,
            },
        }
    }
}

#[cfg(test)]
mod tests;
