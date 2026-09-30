//! Text selection over line ids, so a selection survives new lines arriving and old
//! ones being evicted.
//!
//! A [`SelectionPoint`] is a caret position: a line and a column counted in chars
//! (cells), between characters, from `0` to the line's length. Columns past the end of
//! a line mean "the end of the line".

use std::ops::Range;

use serialist_core::{LineId, LineSource};

use crate::terminal::layout::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SelectionPoint {
    pub line: LineId,
    pub column: usize,
}

impl SelectionPoint {
    pub fn new(line: LineId, column: usize) -> Self {
        Self { line, column }
    }

    /// The end of `line`, whatever its length.
    pub fn end_of(line: LineId) -> Self {
        Self::new(line, usize::MAX)
    }
}

/// What a drag extends by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SelectionMode {
    #[default]
    Character,
    /// Double-click: whole words.
    Word,
    /// Triple-click: whole lines.
    Line,
}

/// A selection as the user made it: `anchor` is where the drag started, `head` where
/// it is now, in either order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: SelectionPoint,
    pub head: SelectionPoint,
    pub mode: SelectionMode,
    /// For word and line mode, the unit under the anchor, which stays selected while
    /// the head moves to either side of it.
    pub anchor_end: SelectionPoint,
}

impl Selection {
    /// A plain selection from `anchor` to `head`.
    pub fn new(anchor: SelectionPoint, head: SelectionPoint) -> Self {
        Self {
            anchor,
            head,
            mode: SelectionMode::Character,
            anchor_end: anchor,
        }
    }

    /// A word or line selection whose anchor unit spans `unit`.
    pub fn unit(unit: Range<SelectionPoint>, mode: SelectionMode) -> Self {
        Self {
            anchor: unit.start,
            head: unit.end,
            mode,
            anchor_end: unit.end,
        }
    }

    /// Everything from `first` through the end of the line before `end`.
    pub fn all(span: Span) -> Option<Self> {
        if span.is_empty() {
            return None;
        }
        Some(Self::unit(
            SelectionPoint::new(span.first, 0)..SelectionPoint::end_of(LineId(span.end.0 - 1)),
            SelectionMode::Line,
        ))
    }

    /// Move the head to `point`, widened to the unit under it in word and line mode.
    /// `unit_at` gives the word or line around a point.
    pub fn extend_to(
        &mut self,
        point: SelectionPoint,
        unit_at: impl FnOnce(SelectionPoint) -> Range<SelectionPoint>,
    ) {
        match self.mode {
            SelectionMode::Character => self.head = point,
            SelectionMode::Word | SelectionMode::Line => {
                let unit = unit_at(point);
                if unit.start < self.anchor {
                    self.head = unit.start;
                } else {
                    self.head = unit.end.max(self.anchor_end);
                }
            }
        }
    }

    /// The selected range in reading order: the anchor unit and the head, whichever
    /// comes first.
    pub fn range(&self) -> Range<SelectionPoint> {
        let (low, high) = if self.head < self.anchor {
            (self.head, self.anchor_end.max(self.head))
        } else {
            (self.anchor, self.head.max(self.anchor_end))
        };
        low..high
    }

    pub fn is_empty(&self) -> bool {
        let range = self.range();
        range.start == range.end
    }

    /// The range clipped to `span`: a start in an evicted line moves to the start of
    /// the first retained line. `None` if nothing selected is still retained.
    pub fn clipped(&self, span: Span) -> Option<Range<SelectionPoint>> {
        let mut range = self.range();
        if span.is_empty() || range.end.line < span.first || range.start.line >= span.end {
            return None;
        }
        if range.start.line < span.first {
            range.start = SelectionPoint::new(span.first, 0);
        }
        if range.end.line >= span.end {
            range.end = SelectionPoint::end_of(LineId(span.end.0 - 1));
        }
        Some(range)
    }

    /// The columns of `line` that are selected, if any: `start..end` in chars, with
    /// `end == usize::MAX` meaning through the end of the line and past it (the line
    /// break is part of the selection).
    pub fn columns_on(&self, line: LineId, span: Span) -> Option<Range<usize>> {
        let range = self.clipped(span)?;
        if line < range.start.line || line > range.end.line {
            return None;
        }
        let start = if line == range.start.line {
            range.start.column
        } else {
            0
        };
        let end = if line == range.end.line {
            range.end.column
        } else {
            usize::MAX
        };
        (start < end).then_some(start..end)
    }

    /// The selected text, lines joined with `\n`. Reads only the selected lines, so
    /// selecting everything in a large scrollback is a background job (see
    /// `TerminalView::copy`).
    pub fn text(&self, source: &dyn LineSource, span: Span) -> String {
        let Some(range) = self.clipped(span) else {
            return String::new();
        };
        let mut out = String::new();
        let mut lines = Vec::new();
        // Fetch in slabs so a huge selection does not hold every line at once.
        let mut next = range.start.line;
        let last = range.end.line;
        while next <= last {
            let slab_end = next.offset(4096).min(last.next());
            lines.clear();
            source.lines(next..slab_end, &mut lines);
            for line in &lines {
                if line.id > range.start.line {
                    out.push('\n');
                }
                let start = if line.id == range.start.line {
                    range.start.column
                } else {
                    0
                };
                let end = if line.id == range.end.line {
                    range.end.column
                } else {
                    usize::MAX
                };
                out.push_str(char_slice(&line.text, start..end));
            }
            next = slab_end;
        }
        out
    }
}

/// `text[columns]` in chars, clamped to the text.
pub fn char_slice(text: &str, columns: Range<usize>) -> &str {
    let start = byte_of_column(text, columns.start);
    let end = byte_of_column(text, columns.end).max(start);
    &text[start..end]
}

/// Byte offset of char column `column`, or the text's length past its end.
pub fn byte_of_column(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Char column of byte offset `byte`; a byte inside a character counts that character.
pub fn column_of_byte(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    text.char_indices().take_while(|(ix, _)| *ix < byte).count()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Space,
    Word,
    Other,
}

fn class(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':') {
        // Paths, hex values, versions and timestamps select as one word.
        CharClass::Word
    } else {
        CharClass::Other
    }
}

/// The columns of the word at `column` (the character under it): a run of word
/// characters, a run of spaces, or a single other character. An empty line or a
/// column past the end selects nothing.
pub fn word_at(text: &str, column: usize) -> Range<usize> {
    let chars: Vec<char> = text.chars().collect();
    if column >= chars.len() {
        return chars.len()..chars.len();
    }
    let kind = class(chars[column]);
    if kind == CharClass::Other {
        return column..column + 1;
    }
    let mut start = column;
    while start > 0 && class(chars[start - 1]) == kind {
        start -= 1;
    }
    let mut end = column + 1;
    while end < chars.len() && class(chars[end]) == kind {
        end += 1;
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::double::MemoryLines;

    fn point(line: u64, column: usize) -> SelectionPoint {
        SelectionPoint::new(LineId(line), column)
    }

    fn source(lines: &[&str]) -> MemoryLines {
        let source = MemoryLines::new();
        for line in lines {
            source.push(line);
        }
        source
    }

    fn span_of(source: &MemoryLines) -> Span {
        Span::new(source.first_line(), source.end())
    }

    #[test]
    fn single_line() {
        let source = source(&["hello world", "second"]);
        let span = span_of(&source);
        let selection = Selection::new(point(0, 6), point(0, 11));
        assert_eq!(selection.text(&source, span), "world");
        let partial = Selection::new(point(0, 0), point(0, 4));
        assert_eq!(partial.text(&source, span), "hell");
        assert!(Selection::new(point(0, 3), point(0, 3)).is_empty());
        assert_eq!(
            Selection::new(point(0, 3), point(0, 3)).text(&source, span),
            ""
        );
    }

    #[test]
    fn multi_line() {
        let source = source(&["alpha", "bravo", "charlie", "delta"]);
        let span = span_of(&source);
        let selection = Selection::new(point(0, 2), point(2, 4));
        assert_eq!(selection.text(&source, span), "pha\nbravo\nchar");
        // A head past the end of its line takes the whole line, not its break.
        let to_end = Selection::new(point(1, 0), point(2, 99));
        assert_eq!(to_end.text(&source, span), "bravo\ncharlie");
        let to_next_line_start = Selection::new(point(1, 0), point(2, 0));
        assert_eq!(to_next_line_start.text(&source, span), "bravo\n");
    }

    #[test]
    fn reversed_drags_select_the_same_text() {
        let source = source(&["alpha", "bravo", "charlie"]);
        let span = span_of(&source);
        let forward = Selection::new(point(0, 2), point(2, 4));
        let backward = Selection::new(point(2, 4), point(0, 2));
        assert_eq!(forward.range(), backward.range());
        assert_eq!(backward.text(&source, span), "pha\nbravo\nchar");
        let same_line = Selection::new(point(1, 4), point(1, 1));
        assert_eq!(same_line.text(&source, span), "rav");
    }

    #[test]
    fn after_eviction_the_start_clamps_to_the_first_retained_line() {
        let source = source(&["zero", "one", "two", "three", "four"]);
        let selection = Selection::new(point(1, 2), point(3, 3));
        source.evict(2);
        let span = span_of(&source);
        assert_eq!(span.first, LineId(2));
        assert_eq!(selection.text(&source, span), "two\nthr");
        assert_eq!(selection.clipped(span), Some(point(2, 0)..point(3, 3)));
        // Entirely evicted: nothing.
        let gone = Selection::new(point(0, 0), point(1, 2));
        assert_eq!(gone.clipped(span), None);
        assert_eq!(gone.text(&source, span), "");
        // New lines after the selection change nothing.
        source.push("five");
        assert_eq!(selection.text(&source, span_of(&source)), "two\nthr");
    }

    #[test]
    fn a_frozen_end_cuts_the_selection() {
        let source = source(&["a", "b", "c", "d"]);
        let paused = Span::new(LineId(0), LineId(2));
        let selection = Selection::new(point(1, 0), point(3, 1));
        assert_eq!(selection.text(&source, paused), "b");
    }

    #[test]
    fn select_all_covers_every_retained_line() {
        let source = source(&["one", "two", "three"]);
        source.evict(1);
        let span = span_of(&source);
        let all = Selection::all(span).unwrap();
        assert_eq!(all.text(&source, span), "two\nthree");
        assert_eq!(Selection::all(Span::new(LineId(4), LineId(4))), None);
    }

    #[test]
    fn columns_on_each_line() {
        let span = Span::new(LineId(0), LineId(10));
        let selection = Selection::new(point(2, 3), point(4, 5));
        assert_eq!(selection.columns_on(LineId(1), span), None);
        assert_eq!(selection.columns_on(LineId(2), span), Some(3..usize::MAX));
        assert_eq!(selection.columns_on(LineId(3), span), Some(0..usize::MAX));
        assert_eq!(selection.columns_on(LineId(4), span), Some(0..5));
        assert_eq!(selection.columns_on(LineId(5), span), None);
    }

    #[test]
    fn words() {
        let text = "err=0x1F  at /dev/cu.usb, ok";
        assert_eq!(word_at(text, 0), 0..3);
        assert_eq!(word_at(text, 3), 3..4, "punctuation alone");
        assert_eq!(word_at(text, 5), 4..8, "hex value");
        assert_eq!(word_at(text, 8), 8..10, "run of spaces");
        assert_eq!(word_at(text, 14), 13..24, "a path is one word");
        assert_eq!(word_at(text, 99), 28..28);
        assert_eq!(word_at("", 0), 0..0);
        assert_eq!(word_at("héllo wörld", 8), 6..11, "columns are chars");
    }

    #[test]
    fn word_mode_extends_by_whole_words_on_both_sides() {
        let source = source(&["one two three four"]);
        let span = span_of(&source);
        let text = "one two three four";
        let unit = |p: SelectionPoint| {
            let w = word_at(text, p.column);
            point(0, w.start)..point(0, w.end)
        };
        let mut selection = Selection::unit(unit(point(0, 9)), SelectionMode::Word);
        assert_eq!(selection.text(&source, span), "three");
        selection.extend_to(point(0, 15), unit);
        assert_eq!(selection.text(&source, span), "three four");
        selection.extend_to(point(0, 5), unit);
        assert_eq!(selection.text(&source, span), "two three");
    }

    #[test]
    fn column_conversions() {
        let text = "aé€b";
        assert_eq!(byte_of_column(text, 0), 0);
        assert_eq!(byte_of_column(text, 2), 3);
        assert_eq!(byte_of_column(text, 3), 6);
        assert_eq!(byte_of_column(text, 9), 7);
        assert_eq!(column_of_byte(text, 3), 2);
        assert_eq!(
            column_of_byte(text, 4),
            3,
            "inside a char rounds up to the next"
        );
        assert_eq!(column_of_byte(text, 100), 4);
        assert_eq!(char_slice(text, 1..3), "é€");
        assert_eq!(char_slice(text, 3..usize::MAX), "b");
    }
}
