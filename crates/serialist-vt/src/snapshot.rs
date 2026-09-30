//! [`VtSnapshot`]: an immutable view of the screen and its scrollback that the terminal
//! element reads through [`LineSource`].

use std::ops::Range;
use std::sync::Arc;

use serialist_core::{Epoch, LineId, LineSource, StyledLine};

use crate::convert::same_content;
use crate::scrollback::ScrollbackView;

/// How the cursor is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    /// A vertical bar before the cell.
    Beam,
    /// An outlined block.
    HollowBlock,
}

/// Where the cursor is and how to draw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CursorState {
    /// The row the cursor is on, a visible row of the snapshot.
    pub line: LineId,
    /// The cursor's cell column, 0-based. After a wide character this is one more than
    /// its character index in the line's text (see the crate docs on wide characters).
    pub column: usize,
    pub shape: CursorShape,
    /// `false` when the device hid it (`CSI ? 25 l`, or a hidden cursor style).
    pub visible: bool,
    pub blinking: bool,
}

/// Terminal modes the app acts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct VtModes {
    /// The alternate screen is shown (`CSI ? 1049 h` and friends). Full-screen programs
    /// use it; the scrollback shown above it is the primary screen's.
    pub alternate_screen: bool,
    /// Wrap pasted text in `ESC [ 200 ~` and `ESC [ 201 ~` (`CSI ? 2004 h`).
    pub bracketed_paste: bool,
    /// Cursor keys send `ESC O A` instead of `ESC [ A` (DECCKM, `CSI ? 1 h`). See
    /// [`vt_key_bytes`](crate::vt_key_bytes).
    pub app_cursor_keys: bool,
    /// The keypad is in application mode (DECKPAM, `ESC =`).
    pub app_keypad: bool,
    /// The device asked for mouse reports (any of `CSI ? 1000/1002/1003 h`). Serialist
    /// does not send mouse reports yet; this is here so the UI can tell the user.
    pub mouse_reporting: bool,
}

/// The screen and scrollback at one moment. Cheap to clone (reference counts) and never
/// changes, so the UI can hold it while the ingest thread keeps feeding the screen.
///
/// Lines run from the oldest retained scrollback row to the bottom row of the screen;
/// see the crate docs for the id policy. Scrollback lines are `complete`; screen rows are
/// not, because the device can still rewrite them.
#[derive(Clone)]
pub struct VtSnapshot {
    pub(crate) scrollback: ScrollbackView,
    pub(crate) visible: Arc<[Arc<StyledLine>]>,
    pub(crate) columns: usize,
    pub(crate) cursor: CursorState,
    pub(crate) modes: VtModes,
    pub(crate) title: Option<Arc<str>>,
    pub(crate) generation: u64,
    pub(crate) epoch: Epoch,
}

impl VtSnapshot {
    /// Screen rows.
    pub fn viewport_rows(&self) -> usize {
        self.visible.len()
    }

    /// Screen columns.
    pub fn columns(&self) -> usize {
        self.columns
    }

    /// Scrollback rows retained.
    pub fn scrollback_lines(&self) -> usize {
        self.scrollback.len()
    }

    /// Id of the top screen row. Every id below it is scrollback (or evicted).
    pub fn first_visible(&self) -> LineId {
        LineId(self.scrollback.end())
    }

    /// Screen row `row` (0 is the top), if there is one.
    pub fn visible_line(&self, row: usize) -> Option<&StyledLine> {
        self.visible.get(row).map(|line| &**line)
    }

    /// The screen rows' text, top to bottom. Handy in tests and logs.
    pub fn screen_text(&self) -> Vec<&str> {
        self.visible.iter().map(|line| line.text.as_str()).collect()
    }

    /// The cursor. Always `Some` for a terminal screen; `None` is reserved for a source
    /// without one.
    pub fn cursor(&self) -> Option<CursorState> {
        Some(self.cursor)
    }

    pub fn modes(&self) -> VtModes {
        self.modes
    }

    /// The title the device set last, if it set one since the last reset.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Increases by one for every snapshot whose content (rows, cursor, modes or title)
    /// differs from the one before. Equal generations from one screen mean equal
    /// snapshots, so the UI can skip everything.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn get(&self, id: LineId) -> Option<&Arc<StyledLine>> {
        let first_visible = self.scrollback.end();
        if id.0 < first_visible {
            self.scrollback.get(id.0)
        } else {
            self.visible.get((id.0 - first_visible) as usize)
        }
    }

    /// The lines of this snapshot that may draw differently from the lines with the same
    /// ids in `previous` (an earlier snapshot of the same screen), including lines
    /// `previous` did not have. Empty (at [`LineSource::end`]) when nothing changed.
    ///
    /// Scrollback rows never change, so only the rows that were on screen in `previous`
    /// and the rows after them are compared: at most one screen of pointer compares, and
    /// text compares only where a row was rebuilt. Lines inside the range may still be
    /// unchanged (it is one range from the first change to the last), and the cursor,
    /// modes and title are not part of it. Rows `previous` had and this snapshot does
    /// not (the screen shrank) are not reported; the source's end says so.
    pub fn changed_since(&self, previous: &VtSnapshot) -> Range<LineId> {
        let end = self.end();
        let from = self.first_line().max(previous.first_visible());
        let overlap = end.min(previous.end());
        let mut first = None;
        let mut last = None;
        let mut id = from;
        while id < overlap {
            let now = self.get(id);
            let before = previous.get(id);
            let same = match (now, before) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b) || same_content(a, b),
                (None, None) => true,
                _ => false,
            };
            if !same {
                first.get_or_insert(id);
                last = Some(id);
            }
            id = id.next();
        }
        if end > overlap.max(from) {
            first.get_or_insert(overlap.max(from));
            last = Some(LineId(end.0 - 1));
        }
        match (first, last) {
            (Some(first), Some(last)) => first..last.next(),
            _ => end..end,
        }
    }
}

impl std::fmt::Debug for VtSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtSnapshot")
            .field("generation", &self.generation)
            .field("columns", &self.columns)
            .field("rows", &self.visible.len())
            .field(
                "scrollback",
                &(self.scrollback.first()..self.scrollback.end()),
            )
            .field("cursor", &self.cursor)
            .field("modes", &self.modes)
            .field("title", &self.title)
            .field("screen", &self.screen_text())
            .finish()
    }
}

impl LineSource for VtSnapshot {
    fn first_line(&self) -> LineId {
        LineId(self.scrollback.first())
    }

    fn line_count(&self) -> usize {
        self.scrollback.len() + self.visible.len()
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        self.get(id).map(|line| StyledLine::clone(line))
    }

    fn lines(&self, range: Range<LineId>, out: &mut Vec<StyledLine>) {
        let start = range.start.max(self.first_line());
        let end = range.end.min(self.end());
        let mut id = start;
        while id < end {
            if let Some(line) = self.get(id) {
                out.push(StyledLine::clone(line));
            }
            id = id.next();
        }
    }

    fn epoch(&self) -> Epoch {
        self.epoch
    }
}
