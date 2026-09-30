//! A hex dump of the retained raw bytes as a [`LineSource`] and [`Searcher`].

use std::fmt::Write as _;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use super::search::{Query, drive, note_scanned};
use super::snapshot::Snapshot;
use crate::text::{
    Color, Direction, Epoch, LineId, LineSource, SearchMatch, Searcher, Style, StyleFlags,
    StyleRun, StyledLine,
};

/// Rows per window when a hex search runs backward. A row costs about a microsecond to
/// render, so a window is about a millisecond of work between checks of `cancel`.
const BACKWARD_WINDOW: u64 = 1024;

/// Rows between checks of `cancel` while scanning forward.
const CANCEL_STRIDE: usize = 64;

/// Styles for the three columns of a hex row. The three must differ so the runs stay
/// separate for the element to color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HexStyles {
    /// The offset column and the two spaces after it.
    pub offset: Style,
    /// The hex bytes and the padding after them.
    pub hex: Style,
    /// The `|ASCII|` column.
    pub ascii: Style,
}

impl Default for HexStyles {
    fn default() -> Self {
        Self {
            offset: Style {
                flags: StyleFlags::DIM,
                ..Style::default()
            },
            hex: Style::default(),
            ascii: Style {
                fg: Color::Ansi(6),
                ..Style::default()
            },
        }
    }
}

/// Rows of `bytes_per_row` raw bytes, like `hexdump -C`:
///
/// ```text
/// 00000000  48 65 6c 6c 6f 0d 0a 00  01 02                    |Hello.....|
/// ```
///
/// Row `n` covers stream offsets `n * bytes_per_row ..`, so row ids are stable as bytes
/// arrive and are evicted. Each row is one [`StyledLine`] with three runs (offset, hex,
/// ASCII); `raw` is the row's byte range and `received_at` the arrival time of the line
/// holding its first byte. A row clipped by eviction shows blanks for the missing bytes.
#[derive(Clone, Debug)]
pub struct HexView {
    snap: Snapshot,
    bytes_per_row: usize,
    styles: HexStyles,
}

impl Snapshot {
    /// A hex view of this snapshot's raw bytes, `bytes_per_row` (1 to 256) per row.
    pub fn hex_view(&self, bytes_per_row: usize) -> HexView {
        self.hex_view_styled(bytes_per_row, HexStyles::default())
    }

    pub fn hex_view_styled(&self, bytes_per_row: usize, styles: HexStyles) -> HexView {
        HexView {
            snap: self.clone(),
            bytes_per_row: bytes_per_row.clamp(1, 256),
            styles,
        }
    }
}

/// One rendered row: what [`LineSource::line`] returns minus the identity and the time.
struct Row {
    text: String,
    runs: Vec<StyleRun>,
    /// The retained stream offsets the row shows.
    raw: Range<u64>,
    complete: bool,
}

impl HexView {
    pub fn bytes_per_row(&self) -> usize {
        self.bytes_per_row
    }

    /// The row holding stream offset `offset`.
    pub fn row_of(&self, offset: u64) -> LineId {
        LineId(offset / self.bytes_per_row as u64)
    }

    /// Stream offsets row `row` covers, before clipping.
    pub fn row_range(&self, row: LineId) -> Range<u64> {
        let bpr = self.bytes_per_row as u64;
        row.0 * bpr..(row.0 + 1) * bpr
    }

    /// Row `id` rendered, without the arrival time (a search does not need it).
    fn row(&self, id: LineId) -> Option<Row> {
        let retained = self.snap.raw_range();
        let full = self.row_range(id);
        let start = full.start.max(retained.start);
        let end = full.end.min(retained.end);
        if start >= end {
            return None;
        }
        let mut bytes = Vec::with_capacity(self.bytes_per_row);
        for slice in self.snap.raw(start..end) {
            bytes.extend_from_slice(slice);
        }
        let (text, runs) = self.render(id, &bytes, (start - full.start) as usize);
        Some(Row {
            text,
            runs,
            raw: start..end,
            complete: end == full.end,
        })
    }

    fn render(&self, row: LineId, bytes: &[u8], skip: usize) -> (String, Vec<StyleRun>) {
        let bpr = self.bytes_per_row;
        let mut text = String::with_capacity(16 + bpr * 4);
        let _ = write!(text, "{:08x}  ", row.0 * bpr as u64);
        let offset_len = text.len();
        for i in 0..bpr {
            if i > 0 {
                text.push(' ');
                if i % 8 == 0 {
                    text.push(' ');
                }
            }
            match i.checked_sub(skip).and_then(|j| bytes.get(j)) {
                Some(b) => {
                    let _ = write!(text, "{b:02x}");
                }
                None => text.push_str("  "),
            }
        }
        text.push_str("  ");
        let hex_len = text.len() - offset_len;
        text.push('|');
        text.extend(std::iter::repeat_n(' ', skip));
        text.extend(bytes.iter().map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '.'
            }
        }));
        text.push('|');
        let ascii_len = text.len() - offset_len - hex_len;
        let runs = vec![
            StyleRun {
                len: offset_len,
                style: self.styles.offset,
            },
            StyleRun {
                len: hex_len,
                style: self.styles.hex,
            },
            StyleRun {
                len: ascii_len,
                style: self.styles.ascii,
            },
        ];
        (text, runs)
    }
}

impl LineSource for HexView {
    fn first_line(&self) -> LineId {
        self.row_of(self.snap.raw_range().start)
    }

    fn line_count(&self) -> usize {
        let raw = self.snap.raw_range();
        if raw.is_empty() {
            return 0;
        }
        let bpr = self.bytes_per_row as u64;
        (raw.end.div_ceil(bpr) - raw.start / bpr) as usize
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        let row = self.row(id)?;
        let p = &self.snap.p;
        Some(StyledLine {
            id,
            text: row.text,
            runs: row.runs,
            direction: Direction::Rx,
            received_at: p.instant(p.ns_at_raw(row.raw.start)),
            raw: row.raw,
            complete: row.complete,
        })
    }

    fn epoch(&self) -> Epoch {
        self.snap.epoch()
    }
}

impl HexView {
    /// [`Searcher::search`] restricted to the rows `range` (clipped to the retained rows),
    /// with the same guarantees as [`Snapshot::search_in`]: no match outside `range`, and
    /// no row outside it is rendered or scanned.
    pub fn search_in(
        &self,
        pattern: &str,
        range: Range<LineId>,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        let query = Query::per_line(pattern)?;
        let bounds = range.start.0.max(self.first_line().0)..range.end.0.min(self.end().0);
        Ok(drive(
            bounds,
            from.0,
            backward,
            limit,
            BACKWARD_WINDOW,
            cancel,
            |rows, limit, out| {
                for (n, id) in rows.enumerate() {
                    if out.len() >= limit
                        || (n % CANCEL_STRIDE == 0 && cancel.load(Ordering::Relaxed))
                    {
                        break;
                    }
                    note_scanned(1);
                    if let Some(row) = self.row(LineId(id)) {
                        query.match_line(row.text.as_bytes(), id, limit, out);
                    }
                }
            },
        ))
    }
}

/// Searches the text of the rows: the offset column (`00000010`), the hex column
/// (`0d 0a`, including the extra space between groups of eight) and the ASCII column
/// (`|OK..|`). The pattern is a regex with the store's smart-case rule, and the
/// direction, `from`, `limit` and `cancel` rules of [`Snapshot`]'s search, so the two
/// behave alike. A match lies within one row; `range` is a byte range in that row's
/// `text`, so it can be drawn over the row's runs. As for text lines, a pattern that can
/// match the empty string reports empty matches.
impl Searcher for HexView {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        self.search_in(
            pattern,
            self.first_line()..self.end(),
            from,
            backward,
            limit,
            cancel,
        )
    }
}
