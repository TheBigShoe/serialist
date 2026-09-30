//! A hex dump of the retained raw bytes as a [`LineSource`].

use std::fmt::Write as _;
use std::ops::Range;

use super::snapshot::Snapshot;
use crate::text::{
    Color, Direction, Epoch, LineId, LineSource, Style, StyleFlags, StyleRun, StyledLine,
};

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
        let p = &self.snap.p;
        Some(StyledLine {
            id,
            text,
            runs,
            direction: Direction::Rx,
            received_at: p.instant(p.ns_at_raw(start)),
            raw: start..end,
            complete: end == full.end,
        })
    }

    fn epoch(&self) -> Epoch {
        self.snap.epoch()
    }
}
