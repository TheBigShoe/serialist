//! Turning one row of Alacritty's grid into a [`StyledLine`].
//!
//! A row is `columns` cells. The line's text is the cells in order with trailing blank
//! cells dropped, and its runs merge neighbouring cells of the same style:
//!
//! - A wide character (CJK, most emoji) fills two cells: the character, then a spacer.
//!   The text has the character once and the spacer is skipped, so from that point on the
//!   text is one character shorter than the row is wide. The terminal element draws one
//!   character per column, so text after a wide character sits one column left of where
//!   the device put it. That is a known limitation, not handled here.
//! - The spacer Alacritty leaves in the last column when a wide character did not fit
//!   (it wraps to the next row) is a blank column, and shows as a space.
//! - Combining marks and other zero-width characters are stored with the cell they
//!   follow and are appended right after its character, so `e` + U+0301 stays one column.
//! - A tab leaves `\t` in the first cell it skipped (Alacritty keeps it for copying); the
//!   text shows a space. Any other control character shows as a space too.
//! - A cell is blank (and dropped when trailing) if it holds a space in the default
//!   background with no inverse, underline or strikethrough: nothing would be drawn.

use std::ops::Range;
use std::time::Instant;

use alacritty_terminal::grid::Row;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color as VtColor, NamedColor};
use serialist_core::{Color, Direction, LineId, Style, StyleFlags, StyleRun, StyledLine};

/// Flags that make a space visible.
const VISIBLE_ON_SPACE: Flags = Flags::INVERSE
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::STRIKEOUT);

/// Where and when a line is captured, for the fields of [`StyledLine`] a grid row has no
/// opinion on.
#[derive(Clone, Debug)]
pub(crate) struct Stamp {
    pub received_at: Instant,
    pub raw: Range<u64>,
    pub complete: bool,
}

/// The text and runs of `row`. See the module docs for the rules.
pub(crate) fn row_content(row: &Row<Cell>) -> (String, Vec<StyleRun>) {
    let cells = &row[..];
    let end = cells
        .iter()
        .rposition(|cell| !is_blank(cell))
        .map_or(0, |i| i + 1);
    let mut text = String::with_capacity(end);
    let mut runs: Vec<StyleRun> = Vec::new();
    for cell in &cells[..end] {
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let start = text.len();
        if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            text.push(' ');
        } else {
            text.push(printable(cell.c));
            if let Some(marks) = cell.zerowidth() {
                text.extend(marks.iter().filter(|c| !c.is_control()));
            }
        }
        let len = text.len() - start;
        let style = style_of(cell);
        match runs.last_mut() {
            Some(run) if run.style == style => run.len += len,
            _ => runs.push(StyleRun { len, style }),
        }
    }
    (text, runs)
}

/// `row` as a received line with id `id`.
pub(crate) fn build_line(row: &Row<Cell>, id: LineId, stamp: Stamp) -> StyledLine {
    let (text, runs) = row_content(row);
    StyledLine {
        id,
        text,
        runs,
        direction: Direction::Rx,
        received_at: stamp.received_at,
        raw: stamp.raw,
        complete: stamp.complete,
    }
}

/// Whether two lines would draw the same: same text, same runs. Ids, times and the
/// `complete` flag are not compared.
pub(crate) fn same_content(a: &StyledLine, b: &StyledLine) -> bool {
    a.text == b.text && a.runs == b.runs
}

fn is_blank(cell: &Cell) -> bool {
    (cell.c == ' ' || cell.c == '\t')
        && cell.bg == VtColor::Named(NamedColor::Background)
        && !cell.flags.intersects(VISIBLE_ON_SPACE)
        && cell.zerowidth().is_none_or(<[char]>::is_empty)
}

fn printable(c: char) -> char {
    if c.is_control() { ' ' } else { c }
}

fn style_of(cell: &Cell) -> Style {
    let f = cell.flags;
    let mut flags = StyleFlags::NONE;
    for (from, to) in [
        (Flags::BOLD, StyleFlags::BOLD),
        (Flags::DIM, StyleFlags::DIM),
        (Flags::ITALIC, StyleFlags::ITALIC),
        (Flags::INVERSE, StyleFlags::INVERSE),
        (Flags::STRIKEOUT, StyleFlags::STRIKETHROUGH),
        (Flags::HIDDEN, StyleFlags::HIDDEN),
    ] {
        if f.contains(from) {
            flags.insert(to);
        }
    }
    // Double, curly, dotted and dashed underlines all draw as the one underline.
    if f.intersects(Flags::ALL_UNDERLINES) {
        flags.insert(StyleFlags::UNDERLINE);
    }
    Style {
        fg: color(cell.fg),
        bg: color(cell.bg),
        flags,
    }
}

/// Alacritty's color as the theme-relative [`Color`] the element resolves.
///
/// The 16 named colors are [`Color::Ansi`], and so are indices 0 to 15 of the 256-color
/// palette (`CSI 38;5;n m` with a small `n` means the same theme colors). The default
/// foreground and background, the cursor color and the bright and dim foregrounds are
/// [`Color::Default`]; the element picks the foreground or background default by where
/// the color is used. Alacritty only produces the dim named colors when rendering, never
/// in a cell, but they map to their normal color for completeness.
pub(crate) fn color(color: VtColor) -> Color {
    match color {
        VtColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        VtColor::Indexed(n) if n < 16 => Color::Ansi(n),
        VtColor::Indexed(n) => Color::Indexed(n),
        VtColor::Named(named) => {
            let n = named as usize;
            let dim_black = NamedColor::DimBlack as usize;
            match n {
                0..=15 => Color::Ansi(n as u8),
                _ if (dim_black..dim_black + 8).contains(&n) => Color::Ansi((n - dim_black) as u8),
                _ => Color::Default,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::vte::ansi::Rgb;

    use super::*;

    fn row(cells: Vec<Cell>) -> Row<Cell> {
        let occ = cells.len();
        Row::from_vec(cells, occ)
    }

    fn cell(c: char) -> Cell {
        Cell {
            c,
            ..Cell::default()
        }
    }

    #[test]
    fn colors_map_to_theme_slots() {
        assert_eq!(color(VtColor::Named(NamedColor::Red)), Color::Ansi(1));
        assert_eq!(
            color(VtColor::Named(NamedColor::BrightWhite)),
            Color::Ansi(15)
        );
        assert_eq!(
            color(VtColor::Named(NamedColor::Foreground)),
            Color::Default
        );
        assert_eq!(
            color(VtColor::Named(NamedColor::Background)),
            Color::Default
        );
        assert_eq!(color(VtColor::Named(NamedColor::Cursor)), Color::Default);
        assert_eq!(color(VtColor::Named(NamedColor::DimGreen)), Color::Ansi(2));
        assert_eq!(
            color(VtColor::Named(NamedColor::DimForeground)),
            Color::Default
        );
        assert_eq!(color(VtColor::Indexed(3)), Color::Ansi(3));
        assert_eq!(color(VtColor::Indexed(200)), Color::Indexed(200));
        assert_eq!(
            color(VtColor::Spec(Rgb { r: 1, g: 2, b: 3 })),
            Color::Rgb(1, 2, 3)
        );
    }

    #[test]
    fn trailing_blanks_go_but_visible_spaces_stay() {
        let mut cells = vec![cell('a'), cell(' '), cell('b'), cell(' '), cell(' ')];
        let (text, runs) = row_content(&row(cells.clone()));
        assert_eq!(text, "a b");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len, 3);

        // An inverse space at the end is drawn, so it is kept, in its own run.
        cells[4].flags.insert(Flags::INVERSE);
        let (text, runs) = row_content(&row(cells));
        assert_eq!(text, "a b  ");
        assert_eq!(
            runs.iter().map(|r| r.len).collect::<Vec<_>>(),
            [4, 1],
            "{runs:?}"
        );
        assert!(runs[1].style.flags.contains(StyleFlags::INVERSE));
    }

    #[test]
    fn an_empty_row_is_an_empty_line() {
        let (text, runs) = row_content(&row(vec![Cell::default(); 8]));
        assert_eq!(text, "");
        assert!(runs.is_empty());
    }

    #[test]
    fn wide_characters_appear_once_and_marks_follow_their_base() {
        let mut wide = cell('日');
        wide.flags.insert(Flags::WIDE_CHAR);
        let mut spacer = cell(' ');
        spacer.flags.insert(Flags::WIDE_CHAR_SPACER);
        let mut e = cell('e');
        e.push_zerowidth('\u{301}');
        let (text, runs) = row_content(&row(vec![wide, spacer, e, cell('x')]));
        assert_eq!(text, "日e\u{301}x");
        assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    }

    #[test]
    fn tabs_and_controls_show_as_spaces() {
        let (text, _) = row_content(&row(vec![cell('\t'), cell(' '), cell('x'), cell('\u{7}')]));
        assert_eq!(text, "  x ");
    }

    #[test]
    fn all_underline_kinds_are_one_underline() {
        let mut c = cell('u');
        c.flags.insert(Flags::UNDERCURL);
        let (_, runs) = row_content(&row(vec![c]));
        assert_eq!(runs[0].style.flags, StyleFlags::UNDERLINE);
    }
}
