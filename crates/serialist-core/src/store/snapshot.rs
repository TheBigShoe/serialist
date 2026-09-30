//! Consistent read-only views of the store.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::buf::AppendBuf;
use super::index::{self, Block, DecRef, LineFlags};
use super::{B, LocalTail, P, PAGE_SIZE, Published, StoreStats, Tail, TailText};
use crate::text::{Direction, Epoch, LineId, LineSource, Style, StyleRun, StyledLine};

/// A consistent view of the store: a fixed line range and raw range, with the pages and
/// index blocks it needs held alive. Cheap to take and to clone; it never blocks the
/// writer and is never invalidated by eviction.
#[derive(Clone)]
pub struct Snapshot {
    pub(super) p: Arc<Published>,
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("lines", &(self.p.first_line..self.p.end_line))
            .field("raw", &(self.p.raw_start..self.p.raw_len))
            .finish()
    }
}

impl Snapshot {
    pub(super) fn new(p: Arc<Published>) -> Self {
        Self { p }
    }

    /// The retained raw bytes: `raw_start..raw_len` in stream offsets.
    pub fn raw_range(&self) -> Range<u64> {
        self.p.raw_start..self.p.raw_len
    }

    /// The raw bytes in `range` (stream offsets), clipped to what is retained, as page
    /// slices in order. Concatenated they are exactly the bytes received.
    pub fn raw(&self, range: Range<u64>) -> RawIter<'_> {
        let start = range.start.max(self.p.raw_start);
        let end = range.end.min(self.p.raw_len);
        RawIter::new(&self.p.pages, self.p.first_page, start..end.max(start))
    }

    pub fn stats(&self) -> StoreStats {
        self.p.stats
    }

    /// One past the last line in the index. Lines from here on are still open and may
    /// change in a later snapshot: the received line in progress, if there is one, and
    /// then the local lines typed in line that have not entered the index (see
    /// [`Store::append_local_inline`](super::Store::append_local_inline)). Lines before
    /// it never change. Equal to [`LineSource::end`] when nothing is open.
    pub fn committed_end(&self) -> LineId {
        LineId(self.p.committed_end)
    }

    /// Whether `other` is the same publication as this snapshot: nothing was published
    /// between the two. Cheaper than, and stricter than, comparing [`StoreStats`], which
    /// cannot see a line being typed change its text without changing its length.
    pub fn is_same_publication(&self, other: &Snapshot) -> bool {
        Arc::ptr_eq(&self.p, &other.p)
    }

    /// The text of line `id` as bytes, borrowed where possible (plain lines inside one
    /// page, decoded lines) and gathered into `scratch` otherwise.
    pub(super) fn line_text<'a>(&'a self, id: u64, scratch: &'a mut Vec<u8>) -> Option<&'a [u8]> {
        let p = &*self.p;
        if id < p.first_line || id >= p.end_line {
            return None;
        }
        if id >= p.committed_end {
            return Some(match p.open_line(id)? {
                OpenLine::Received(tail) => match &tail.text {
                    TailText::Plain { lead, len } => {
                        let start = tail.start + lead;
                        gather(p, start..start + *len as u64, scratch)
                    }
                    TailText::Decoded(d) => d.0.as_bytes(),
                },
                OpenLine::Typed(local) => local.text.as_bytes(),
            });
        }
        let (block, local) = p.block(id);
        let flags = block.flags(local);
        if flags.is_decoded() {
            return Some(index::record_text(p.record(block.dec(local)?)?));
        }
        let start = block.start(local);
        let end = p.raw_end_of(id);
        Some(gather(
            p,
            start + flags.lead()..end - flags.trail(),
            scratch,
        ))
    }
}

/// Raw bytes `range`, borrowed if they sit in one page, else copied into `scratch`.
fn gather<'a>(p: &'a Published, range: Range<u64>, scratch: &'a mut Vec<u8>) -> &'a [u8] {
    if range.is_empty() {
        return &[];
    }
    if range.start / P == (range.end - 1) / P {
        return p.page_slice(range);
    }
    scratch.clear();
    for slice in RawIter::new(&p.pages, p.first_page, range) {
        scratch.extend_from_slice(slice);
    }
    scratch
}

impl Published {
    /// The block holding committed line `id` and the line's index in it.
    pub(super) fn block(&self, id: u64) -> (&Block, usize) {
        let first_block = self.blocks[0].first / B;
        let block = &self.blocks[(id / B - first_block) as usize];
        (block, (id - block.first) as usize)
    }

    pub(super) fn start_of(&self, id: u64) -> u64 {
        if id >= self.committed_end {
            return self.committed_raw_end;
        }
        let (block, local) = self.block(id);
        block.start(local)
    }

    /// Where received line `id` ends: the next line's start, or where the line in
    /// progress starts.
    pub(super) fn raw_end_of(&self, id: u64) -> u64 {
        if id + 1 < self.committed_end {
            self.start_of(id + 1)
        } else {
            self.committed_raw_end
        }
    }

    pub(super) fn record(&self, dec: DecRef) -> Option<&[u8]> {
        let index = dec.page.wrapping_sub(self.first_text_seq as u32) as usize;
        let page = self.text_pages.get(index)?;
        page.as_slice().get(dec.off as usize..)
    }

    /// Raw bytes `range`, which must lie in one retained page.
    pub(super) fn page_slice(&self, range: Range<u64>) -> &[u8] {
        let page = &self.pages[(range.start / P - self.first_page) as usize];
        let base = (range.start / P) * P;
        &page.as_slice()[(range.start - base) as usize..(range.end - base) as usize]
    }

    pub(super) fn instant(&self, ns: u64) -> Instant {
        self.epoch.instant + Duration::from_nanos(ns)
    }

    /// Arrival time of the line holding raw offset `offset`, as ns since the epoch.
    pub(super) fn ns_at_raw(&self, offset: u64) -> u64 {
        if offset >= self.committed_raw_end
            && let Some(tail) = &self.tail
        {
            return tail.ns;
        }
        let committed: &[Arc<Block>] = &self.blocks;
        if committed.is_empty() || self.committed_end == 0 {
            return 0;
        }
        let bi = committed
            .partition_point(|b| b.base_raw <= offset && b.first < self.committed_end)
            .saturating_sub(1);
        let block = &committed[bi];
        let n = (self.committed_end - block.first).min(B) as usize;
        let starts = &block.starts.as_slice()[..n.min(block.starts.as_slice().len())];
        let local = starts
            .partition_point(|&s| block.base_raw + u64::from(s) <= offset)
            .saturating_sub(1);
        block.ns(local.min(n.saturating_sub(1)))
    }

    /// The open line with id `id`, which is at or past `committed_end`: the received line
    /// in progress, then the typed lines.
    pub(super) fn open_line(&self, id: u64) -> Option<OpenLine<'_>> {
        let k = usize::try_from(id.checked_sub(self.committed_end)?).ok()?;
        match &self.tail {
            Some(tail) if k == 0 => Some(OpenLine::Received(tail)),
            Some(_) => self.local.get(k - 1).map(OpenLine::Typed),
            None => self.local.get(k).map(OpenLine::Typed),
        }
    }

    fn open_styled_line(&self, id: LineId) -> Option<StyledLine> {
        match self.open_line(id.0)? {
            OpenLine::Received(tail) => Some(self.tail_line(id, tail)),
            OpenLine::Typed(local) => {
                let text = local.text.to_string();
                let runs = default_runs(text.len());
                Some(StyledLine {
                    id,
                    text,
                    runs,
                    direction: local.direction,
                    received_at: self.instant(local.ns),
                    // Local lines have no bytes of their own; this is where they sit.
                    raw: self.raw_len..self.raw_len,
                    complete: local.complete,
                })
            }
        }
    }

    fn tail_line(&self, id: LineId, tail: &Tail) -> StyledLine {
        let (text, runs) = match &tail.text {
            TailText::Plain { lead, len } => {
                let start = tail.start + lead;
                let text = read_string(self, start..start + *len as u64);
                let runs = default_runs(text.len());
                (text, runs)
            }
            TailText::Decoded(d) => (d.0.clone(), d.1.clone()),
        };
        StyledLine {
            id,
            text,
            runs,
            direction: Direction::Rx,
            received_at: self.instant(tail.ns),
            raw: tail.start..self.raw_len,
            complete: false,
        }
    }
}

/// A line that is not in the index yet.
pub(super) enum OpenLine<'a> {
    /// The received line in progress.
    Received(&'a Tail),
    /// A local line typed in line.
    Typed(&'a LocalTail),
}

fn default_runs(len: usize) -> Vec<StyleRun> {
    if len == 0 {
        Vec::new()
    } else {
        vec![StyleRun {
            len,
            style: Style::default(),
        }]
    }
}

fn read_string(p: &Published, range: Range<u64>) -> String {
    let mut bytes = Vec::with_capacity((range.end - range.start) as usize);
    for slice in RawIter::new(&p.pages, p.first_page, range) {
        bytes.extend_from_slice(slice);
    }
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

impl LineSource for Snapshot {
    fn first_line(&self) -> LineId {
        LineId(self.p.first_line)
    }

    fn line_count(&self) -> usize {
        (self.p.end_line - self.p.first_line) as usize
    }

    fn end(&self) -> LineId {
        LineId(self.p.end_line)
    }

    /// O(1): a division to find the block, then array reads (and a binary search over
    /// the block's few time marks and decoded lines).
    fn line(&self, id: LineId) -> Option<StyledLine> {
        let p = &*self.p;
        let n = id.0;
        if n < p.first_line || n >= p.end_line {
            return None;
        }
        if n >= p.committed_end {
            return p.open_styled_line(id);
        }
        let (block, local) = p.block(n);
        let flags: LineFlags = block.flags(local);
        let start = block.start(local);
        let direction = flags.direction();
        let raw = if direction == Direction::Rx {
            start..p.raw_end_of(n)
        } else {
            start..start
        };
        let (text, runs) = if flags.is_decoded() {
            let mut text = String::new();
            let mut runs = Vec::new();
            if let Some(record) = block.dec(local).and_then(|d| p.record(d)) {
                index::decode_record(record, &mut text, &mut runs);
            }
            (text, runs)
        } else {
            let text = read_string(p, raw.start + flags.lead()..raw.end - flags.trail());
            let runs = default_runs(text.len());
            (text, runs)
        };
        Some(StyledLine {
            id,
            text,
            runs,
            direction,
            received_at: p.instant(block.ns(local)),
            raw,
            complete: flags.complete(),
        })
    }

    fn epoch(&self) -> Epoch {
        self.p.epoch
    }
}

/// Page slices of a raw byte range, in order.
pub struct RawIter<'a> {
    pages: &'a [Arc<AppendBuf<u8>>],
    first_page: u64,
    pos: u64,
    end: u64,
}

impl<'a> RawIter<'a> {
    pub(super) fn new(pages: &'a [Arc<AppendBuf<u8>>], first_page: u64, range: Range<u64>) -> Self {
        Self {
            pages,
            first_page,
            pos: range.start,
            end: range.end,
        }
    }
}

impl<'a> Iterator for RawIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.end {
            return None;
        }
        let page_no = self.pos / P;
        let page = self
            .pages
            .get(page_no.checked_sub(self.first_page)? as usize)?;
        let base = page_no * P;
        let data = page.as_slice();
        let from = (self.pos - base) as usize;
        let to = ((self.end - base) as usize).min(PAGE_SIZE).min(data.len());
        if from >= to {
            self.pos = self.end;
            return None;
        }
        self.pos = base + to as u64;
        Some(&data[from..to])
    }
}

impl fmt::Debug for RawIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RawIter")
            .field("pos", &self.pos)
            .field("end", &self.end)
            .finish()
    }
}
