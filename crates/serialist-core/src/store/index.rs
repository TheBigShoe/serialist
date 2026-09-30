//! The line index: fixed-size blocks of compact per-line entries, and the encoding of
//! decoded line text.
//!
//! Block `k` holds lines `k * BLOCK_LINES .. (k + 1) * BLOCK_LINES`, so finding a line
//! is a division. Per line a block stores 5 bytes: a `u32` start offset relative to the
//! block's base raw offset, and a flag byte. Timestamps are stored once per change
//! (usually once per chunk), and decoded text only for the lines that need it.

use std::sync::Arc;

use super::buf::{AppendBuf, AppendWriter};
use crate::text::{Color, Direction, Style, StyleFlags, StyleRun};

/// Lines per block. Also bounds a block's raw span: `BLOCK_LINES` lines of at most
/// `MAX_LINE_BYTES_LIMIT` bytes each keep every start offset within a `u32`.
pub(crate) const BLOCK_LINES: usize = 4096;
const _: () =
    assert!((BLOCK_LINES as u64 - 1) * crate::ansi::MAX_LINE_BYTES_LIMIT as u64 <= u32::MAX as u64);

/// Per-line flag byte: direction, completeness, text storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LineFlags(pub u8);

impl LineFlags {
    const DIR_MASK: u8 = 0b11;
    const COMPLETE: u8 = 1 << 2;
    const DECODED: u8 = 1 << 3;
    const LEAD_SHIFT: u8 = 4;
    const TRAIL_SHIFT: u8 = 6;

    /// A line whose text is stored decoded.
    pub fn decoded(direction: Direction, complete: bool) -> Self {
        Self(dir_bits(direction) | if complete { Self::COMPLETE } else { 0 } | Self::DECODED)
    }

    /// A received line whose text is its raw bytes minus `lead` leading and `trail`
    /// trailing bytes (CRs and the LF). Both are at most 3.
    pub fn plain(complete: bool, lead: u8, trail: u8) -> Self {
        debug_assert!(lead <= 3 && trail <= 3);
        Self(
            dir_bits(Direction::Rx)
                | if complete { Self::COMPLETE } else { 0 }
                | (lead << Self::LEAD_SHIFT)
                | (trail << Self::TRAIL_SHIFT),
        )
    }

    pub fn direction(self) -> Direction {
        match self.0 & Self::DIR_MASK {
            0 => Direction::Rx,
            1 => Direction::Tx,
            _ => Direction::Notice,
        }
    }

    pub fn complete(self) -> bool {
        self.0 & Self::COMPLETE != 0
    }

    pub fn is_decoded(self) -> bool {
        self.0 & Self::DECODED != 0
    }

    pub fn lead(self) -> u64 {
        u64::from((self.0 >> Self::LEAD_SHIFT) & 0b11)
    }

    pub fn trail(self) -> u64 {
        u64::from((self.0 >> Self::TRAIL_SHIFT) & 0b11)
    }

    /// A received line whose text can be read straight from the raw pages.
    pub fn is_plain_rx(self) -> bool {
        !self.is_decoded() && self.direction() == Direction::Rx
    }
}

fn dir_bits(direction: Direction) -> u8 {
    match direction {
        Direction::Rx => 0,
        Direction::Tx => 1,
        Direction::Notice => 2,
    }
}

/// The time of line `local` and of every following line up to the next mark, as
/// nanoseconds since the store's epoch. Three `u32`s keep it at 12 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub local: u32,
    ns_lo: u32,
    ns_hi: u32,
}

impl Mark {
    pub fn new(local: u32, ns: u64) -> Self {
        Self {
            local,
            ns_lo: ns as u32,
            ns_hi: (ns >> 32) as u32,
        }
    }

    pub fn ns(self) -> u64 {
        u64::from(self.ns_lo) | (u64::from(self.ns_hi) << 32)
    }
}

/// Where the decoded text of line `local` lives: text page `page` (a sequence number,
/// truncated), byte `off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DecRef {
    pub local: u32,
    pub page: u32,
    pub off: u32,
}

/// One block of the line index. Shared with snapshots; only the store's
/// [`BlockWriter`] appends to it.
#[derive(Debug)]
pub(crate) struct Block {
    /// Line id of the block's first slot.
    pub first: u64,
    /// Raw offset that `starts` are relative to: the start of the block's first line.
    pub base_raw: u64,
    pub starts: Arc<AppendBuf<u32>>,
    pub flags: Arc<AppendBuf<u8>>,
    /// Sorted by `local`; the first line always has one.
    pub marks: Arc<AppendBuf<Mark>>,
    /// Sorted by `local`; one per decoded line.
    pub decs: Arc<AppendBuf<DecRef>>,
}

impl Block {
    /// Heap bytes: the four arrays at their capacity plus the block itself.
    pub fn heap_bytes(&self) -> usize {
        self.starts.heap_bytes()
            + self.flags.heap_bytes()
            + self.marks.heap_bytes()
            + self.decs.heap_bytes()
            + BLOCK_OVERHEAD
    }

    pub fn start(&self, local: usize) -> u64 {
        self.base_raw + u64::from(self.starts.as_slice()[local])
    }

    pub fn flags(&self, local: usize) -> LineFlags {
        LineFlags(self.flags.as_slice()[local])
    }

    pub fn ns(&self, local: usize) -> u64 {
        let marks = self.marks.as_slice();
        let i = marks.partition_point(|m| m.local as usize <= local);
        marks[i.saturating_sub(1)].ns()
    }

    pub fn dec(&self, local: usize) -> Option<DecRef> {
        let decs = self.decs.as_slice();
        decs.binary_search_by_key(&(local as u32), |d| d.local)
            .ok()
            .map(|i| decs[i])
    }
}

/// The block struct, its `Arc` and the five `Arc`s it holds, roughly.
pub(crate) const BLOCK_OVERHEAD: usize = 64 + 5 * 32;

/// Appends lines to the open block.
#[derive(Debug)]
pub(crate) struct BlockWriter {
    pub block: Arc<Block>,
    starts: AppendWriter<u32>,
    flags: AppendWriter<u8>,
    marks: AppendWriter<Mark>,
    decs: AppendWriter<DecRef>,
    last_ns: u64,
}

impl BlockWriter {
    pub fn new(first: u64, base_raw: u64) -> Self {
        let (starts_buf, starts) = AppendBuf::new(BLOCK_LINES);
        let (flags_buf, flags) = AppendBuf::new(BLOCK_LINES);
        let (marks_buf, marks) = AppendBuf::new(BLOCK_LINES);
        let (decs_buf, decs) = AppendBuf::new(BLOCK_LINES);
        Self {
            block: Arc::new(Block {
                first,
                base_raw,
                starts: starts_buf,
                flags: flags_buf,
                marks: marks_buf,
                decs: decs_buf,
            }),
            starts,
            flags,
            marks,
            decs,
            last_ns: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.starts.len()
    }

    pub fn is_full(&self) -> bool {
        self.starts.is_full()
    }

    /// Append a line. `dec` is where its decoded text lives, if it has any.
    pub fn push(&mut self, start: u64, flags: LineFlags, ns: u64, dec: Option<(u32, u32)>) {
        let local = self.len() as u32;
        let rel = start - self.block.base_raw;
        debug_assert!(rel <= u64::from(u32::MAX));
        if local == 0 || ns != self.last_ns {
            self.marks.push(Mark::new(local, ns));
            self.last_ns = ns;
        }
        if let Some((page, off)) = dec {
            self.decs.push(DecRef { local, page, off });
        }
        self.flags.push(flags.0);
        // Last: a reader that can see the start sees the rest of the entry.
        self.starts.push(rel as u32);
    }

    /// A right-sized immutable copy of this block, for when it is full.
    pub fn seal(&self) -> Arc<Block> {
        Arc::new(Block {
            first: self.block.first,
            base_raw: self.block.base_raw,
            starts: AppendBuf::frozen(self.starts.written()),
            flags: AppendBuf::frozen(self.flags.written()),
            marks: AppendBuf::frozen(self.marks.written()),
            decs: AppendBuf::frozen(self.decs.written()),
        })
    }
}

// Decoded text records, laid out so a text page is also a search haystack:
//
// page   := '\n' record*
// record := text '\n' varint(run_count) run* '\n'
// run    := varint(len) color(fg) color(bg) flags:u8
// color  := 0 | 1 n | 2 n | 3 r g b          (Default | Ansi | Indexed | Rgb)
//
// A `DecRef` points at the text. Line text never contains '\n', so every text in a page
// is preceded and followed by '\n' and a multi-line regex sees its ends as line ends.
// A run count of zero with non-empty text means one default-style run over the text.

/// Written at the start of every text page.
pub(crate) const TEXT_PAGE_HEADER: &[u8] = b"\n";

pub(crate) fn encode_record(out: &mut Vec<u8>, text: &str, runs: &[StyleRun]) {
    debug_assert!(!text.contains('\n'));
    out.clear();
    out.extend_from_slice(text.as_bytes());
    out.push(b'\n');
    let single_default = runs.len() == 1 && runs[0].style == Style::default();
    if runs.is_empty() || single_default {
        put_varint(out, 0);
    } else {
        put_varint(out, runs.len() as u64);
        for run in runs {
            put_varint(out, run.len as u64);
            put_color(out, run.style.fg);
            put_color(out, run.style.bg);
            out.push(run.style.flags.0);
        }
    }
    out.push(b'\n');
}

/// The text of the record at the start of `bytes`.
pub(crate) fn record_text(bytes: &[u8]) -> &[u8] {
    let end = memchr::memchr(b'\n', bytes).unwrap_or(bytes.len());
    &bytes[..end]
}

/// Decode the record at the start of `bytes` into `text` and `runs`.
pub(crate) fn decode_record(bytes: &[u8], text: &mut String, runs: &mut Vec<StyleRun>) {
    let raw_text = record_text(bytes);
    let len = raw_text.len();
    text.push_str(&String::from_utf8_lossy(raw_text));
    let mut pos = len + 1;
    let count = get_varint(bytes, &mut pos) as usize;
    if count == 0 {
        if len > 0 {
            runs.push(StyleRun {
                len,
                style: Style::default(),
            });
        }
        return;
    }
    runs.reserve(count);
    for _ in 0..count {
        let len = get_varint(bytes, &mut pos) as usize;
        let fg = get_color(bytes, &mut pos);
        let bg = get_color(bytes, &mut pos);
        let flags = StyleFlags(bytes[pos]);
        pos += 1;
        runs.push(StyleRun {
            len,
            style: Style { fg, bg, flags },
        });
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &[u8], pos: &mut usize) -> u64 {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = bytes[*pos];
        *pos += 1;
        v |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return v;
        }
        shift += 7;
    }
}

fn put_color(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Default => out.push(0),
        Color::Ansi(n) => out.extend_from_slice(&[1, n]),
        Color::Indexed(n) => out.extend_from_slice(&[2, n]),
        Color::Rgb(r, g, b) => out.extend_from_slice(&[3, r, g, b]),
    }
}

fn get_color(bytes: &[u8], pos: &mut usize) -> Color {
    let tag = bytes[*pos];
    *pos += 1;
    let take = |pos: &mut usize| {
        let b = bytes[*pos];
        *pos += 1;
        b
    };
    match tag {
        1 => Color::Ansi(take(pos)),
        2 => Color::Indexed(take(pos)),
        3 => {
            let r = take(pos);
            let g = take(pos);
            let b = take(pos);
            Color::Rgb(r, g, b)
        }
        _ => Color::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        let styles = [
            Style::default(),
            Style {
                fg: Color::Ansi(3),
                bg: Color::Rgb(1, 2, 3),
                flags: StyleFlags::BOLD,
            },
            Style {
                fg: Color::Indexed(200),
                ..Style::default()
            },
        ];
        let text = "a".repeat(300) + "é€";
        let runs = vec![
            StyleRun {
                len: 100,
                style: styles[1],
            },
            StyleRun {
                len: 200,
                style: styles[0],
            },
            StyleRun {
                len: 5,
                style: styles[2],
            },
        ];
        let mut buf = Vec::new();
        for (text, runs) in [
            (text.as_str(), runs.as_slice()),
            (
                "plain",
                &[StyleRun {
                    len: 5,
                    style: styles[0],
                }][..],
            ),
            ("", &[][..]),
        ] {
            encode_record(&mut buf, text, runs);
            let (mut t, mut r) = (String::new(), Vec::new());
            decode_record(&buf, &mut t, &mut r);
            assert_eq!(t, text);
            assert_eq!(r, runs);
            assert_eq!(record_text(&buf), text.as_bytes());
        }
    }

    #[test]
    fn flags_round_trip() {
        let f = LineFlags::plain(true, 2, 3);
        assert_eq!(f.direction(), Direction::Rx);
        assert!(f.complete() && !f.is_decoded() && f.is_plain_rx());
        assert_eq!((f.lead(), f.trail()), (2, 3));
        let f = LineFlags::decoded(Direction::Notice, false);
        assert_eq!(f.direction(), Direction::Notice);
        assert!(!f.complete() && f.is_decoded() && !f.is_plain_rx());
        assert_eq!(
            LineFlags::decoded(Direction::Tx, true).direction(),
            Direction::Tx
        );
    }

    #[test]
    fn marks_and_decs() {
        let mut w = BlockWriter::new(0, 100);
        w.push(100, LineFlags::plain(true, 0, 1), 5, None);
        w.push(110, LineFlags::plain(true, 0, 1), 5, None);
        w.push(
            120,
            LineFlags::decoded(Direction::Rx, true),
            9,
            Some((0, 0)),
        );
        w.push(
            120,
            LineFlags::decoded(Direction::Tx, true),
            9,
            Some((0, 7)),
        );
        let b = &w.block;
        assert_eq!(b.marks.as_slice().len(), 2);
        assert_eq!((b.ns(0), b.ns(1), b.ns(2), b.ns(3)), (5, 5, 9, 9));
        assert_eq!(b.dec(1), None);
        assert_eq!(b.dec(3).map(|d| d.off), Some(7));
        assert_eq!(b.start(2), 120);
        let sealed = w.seal();
        assert_eq!(sealed.starts.as_slice(), b.starts.as_slice());
        assert!(sealed.heap_bytes() < b.heap_bytes());
        assert_eq!(Mark::new(0, u64::MAX - 3).ns(), u64::MAX - 3);
    }
}
