//! Shared text model for milestone 1: styled lines as the store produces them and the
//! terminal element consumes them. Both sides code against [`LineSource`], so the element
//! can be built and tested against an in-memory double before the store exists.

use std::ops::Range;
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// Identity of a line in a session. Ids increase by one per line, are never reused, and
/// survive eviction: an id below `LineSource::first_line()` names a line that was evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LineId(pub u64);

impl LineId {
    pub const ZERO: LineId = LineId(0);

    pub fn next(self) -> Self {
        LineId(self.0 + 1)
    }

    pub fn offset(self, n: usize) -> Self {
        LineId(self.0 + n as u64)
    }
}

/// A terminal color. `Ansi` is 0..=15 (the 16 theme colors), `Indexed` is the 256-color
/// cube and greys (16..=255), `Rgb` is truecolor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Color {
    #[default]
    Default,
    Ansi(u8),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StyleFlags(pub u8);

impl StyleFlags {
    pub const NONE: StyleFlags = StyleFlags(0);
    pub const BOLD: StyleFlags = StyleFlags(1 << 0);
    pub const DIM: StyleFlags = StyleFlags(1 << 1);
    pub const ITALIC: StyleFlags = StyleFlags(1 << 2);
    pub const UNDERLINE: StyleFlags = StyleFlags(1 << 3);
    pub const INVERSE: StyleFlags = StyleFlags(1 << 4);
    pub const STRIKETHROUGH: StyleFlags = StyleFlags(1 << 5);
    pub const HIDDEN: StyleFlags = StyleFlags(1 << 6);

    pub fn contains(self, other: StyleFlags) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: StyleFlags) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: StyleFlags) {
        self.0 &= !other.0;
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub flags: StyleFlags,
}

/// A run of `len` bytes of a line's `text` drawn with one style. Runs cover the text
/// exactly, in order, with no zero-length runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StyleRun {
    pub len: usize,
    pub style: Style,
}

/// Where a line came from. `Tx` lines are local echoes of what was sent and are not part
/// of the raw received stream; `Notice` lines are app messages (connected, timeouts).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Rx,
    Tx,
    Notice,
}

/// One displayable line. `text` is printable content only: control characters and escape
/// sequences have been applied or removed, tabs expanded, invalid UTF-8 replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledLine {
    pub id: LineId,
    pub text: String,
    pub runs: Vec<StyleRun>,
    pub direction: Direction,
    /// When the first byte of the line arrived.
    pub received_at: Instant,
    /// Byte offsets of this line in the raw received stream (empty for `Tx`/`Notice`).
    pub raw: Range<u64>,
    /// `true` once a line feed ended the line; the last line of a burst may be incomplete.
    pub complete: bool,
}

/// Maps monotonic instants to wall-clock time for timestamp display and export.
#[derive(Clone, Copy, Debug)]
pub struct Epoch {
    pub instant: Instant,
    pub wall: SystemTime,
}

impl Epoch {
    pub fn now() -> Self {
        Self {
            instant: Instant::now(),
            wall: SystemTime::now(),
        }
    }

    pub fn wall_time(&self, at: Instant) -> SystemTime {
        self.wall + at.saturating_duration_since(self.instant)
    }
}

/// What the terminal element reads. Implemented by store snapshots, hex views and test
/// doubles. Every method is cheap enough to call once per frame; `line` is O(1) or O(log n).
pub trait LineSource: Send + Sync {
    /// Oldest retained line. Everything below it was evicted.
    fn first_line(&self) -> LineId;

    fn line_count(&self) -> usize;

    /// One past the newest line.
    fn end(&self) -> LineId {
        self.first_line().offset(self.line_count())
    }

    fn line(&self, id: LineId) -> Option<StyledLine>;

    /// Lines in `range` clipped to what is retained, appended to `out` in order.
    fn lines(&self, range: Range<LineId>, out: &mut Vec<StyledLine>) {
        let start = range.start.max(self.first_line());
        let end = range.end.min(self.end());
        let mut id = start;
        while id < end {
            if let Some(line) = self.line(id) {
                out.push(line);
            }
            id = id.next();
        }
    }

    fn epoch(&self) -> Epoch;
}

/// A hit from a scrollback search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    pub line: LineId,
    /// Byte range within that line's `text`.
    pub range: Range<usize>,
}

/// Searches line text. Implemented by store snapshots; `cancel` is polled so a long
/// search on a background thread stops when the user edits the query.
pub trait Searcher: Send + Sync {
    /// Up to `limit` matches starting at `from`, forward or backward. `pattern` is a regex;
    /// an invalid pattern returns `Err` with the regex crate's message.
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Vec<SearchMatch>, String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_compose() {
        let mut f = StyleFlags::BOLD;
        f.insert(StyleFlags::UNDERLINE);
        assert!(f.contains(StyleFlags::BOLD));
        assert!(f.contains(StyleFlags::UNDERLINE));
        f.remove(StyleFlags::BOLD);
        assert!(!f.contains(StyleFlags::BOLD));
        assert!(!f.is_empty());
    }

    #[test]
    fn line_ids_are_ordered() {
        assert!(LineId(3) < LineId(4));
        assert_eq!(LineId(3).next(), LineId(4));
        assert_eq!(LineId(3).offset(2), LineId(5));
    }
}
