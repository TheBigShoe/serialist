//! Where the characters of a terminal screen's line sit, in cells.
//!
//! The terminal element lays a line out one character per cell, and selection, search
//! and hit testing count columns in characters. A terminal screen's line is not like
//! that: a wide character (CJK, most emoji) fills two cells and a combining mark none,
//! because that is where the device put them (see `serialist_vt::cell_width`). For such a
//! line the element keeps a [`CellMap`] and turns character columns into cells to draw
//! and cells into character columns to hit. Selections and search matches stay in
//! characters, so the text they cover, and what copy gives, are unchanged.
//!
//! A line whose characters are each one cell has no map, and the two counts are the same:
//! every line of the scrollback store's log, and most of a screen's.

/// The first cell of each character of a line, and the cell after its last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellMap {
    /// `starts[i]` is the cell character `i` begins in; one more entry for the end.
    starts: Vec<usize>,
}

impl CellMap {
    /// The map of `text` with each character `width(c)` cells wide, or `None` when every
    /// character is one cell.
    pub fn of(text: &str, width: impl Fn(char) -> usize) -> Option<Self> {
        if text.chars().all(|c| width(c) == 1) {
            return None;
        }
        let mut starts = Vec::with_capacity(text.len() + 1);
        let mut cell = 0;
        for c in text.chars() {
            starts.push(cell);
            cell += width(c);
        }
        starts.push(cell);
        Some(Self { starts })
    }

    /// Characters in the line.
    pub fn chars(&self) -> usize {
        self.starts.len() - 1
    }

    /// Cells the line takes.
    pub fn cells(&self) -> usize {
        self.starts[self.chars()]
    }

    /// The cell character column `column` begins in; past the end, as many cells past
    /// the last as columns past it (a selection's line break is one).
    pub fn cell_of(&self, column: usize) -> usize {
        match self.starts.get(column) {
            Some(&cell) => cell,
            None => self.cells() + (column - self.chars()),
        }
    }

    /// The caret column at the boundary before cell `cell`: after any combining marks of
    /// the cell before, and in front of a wide character the cell is the second half of.
    pub fn caret_at(&self, cell: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= cell)
            .saturating_sub(1)
    }

    /// The character column of the character covering cell `cell`, or the last one past
    /// the end.
    pub fn char_at(&self, cell: usize) -> usize {
        self.starts[..self.chars()]
            .partition_point(|&start| start <= cell)
            .saturating_sub(1)
    }
}

/// `column` in cells, through `map` when there is one.
pub fn cell_of(map: Option<&CellMap>, column: usize) -> usize {
    map.map_or(column, |map| map.cell_of(column))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn width(c: char) -> usize {
        serialist_vt::cell_width(c)
    }

    #[test]
    fn narrow_text_has_no_map() {
        assert_eq!(CellMap::of("plain ─│ text", width), None);
        assert_eq!(CellMap::of("", width), None);
        assert_eq!(cell_of(None, 7), 7);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        // a 日 b 本 c: cells 0, 1-2, 3, 4-5, 6.
        let map = CellMap::of("a日b本c", width).expect("a map");
        assert_eq!((map.chars(), map.cells()), (5, 7));
        assert_eq!(
            (0..=5)
                .map(|column| map.cell_of(column))
                .collect::<Vec<_>>(),
            [0, 1, 3, 4, 6, 7]
        );
        // Past the end: the line break and beyond.
        assert_eq!(map.cell_of(6), 8);
        // The cell under the pointer: both halves of a wide character are that character.
        assert_eq!(
            (0..8).map(|cell| map.char_at(cell)).collect::<Vec<_>>(),
            [0, 1, 1, 2, 3, 3, 4, 4]
        );
        // A caret in the middle of a wide character goes in front of it.
        assert_eq!(
            (0..9).map(|cell| map.caret_at(cell)).collect::<Vec<_>>(),
            [0, 1, 1, 2, 3, 3, 4, 5, 5]
        );
    }

    #[test]
    fn combining_marks_take_no_cell() {
        // e + U+0301, then x: two cells, three characters.
        let map = CellMap::of("e\u{301}x", width).expect("a map");
        assert_eq!((map.chars(), map.cells()), (3, 2));
        assert_eq!(map.cell_of(2), 1, "x is in the second cell");
        assert_eq!(map.char_at(0), 0);
        assert_eq!(map.char_at(1), 2);
        // The caret between the cells is after the mark: a selection never splits it
        // from its base.
        assert_eq!(map.caret_at(1), 2);
        assert_eq!(map.caret_at(2), 3);
    }
}
