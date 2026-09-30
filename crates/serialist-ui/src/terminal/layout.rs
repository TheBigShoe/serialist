//! Scroll position and visible-range math, without GPUI.
//!
//! The viewport is anchored at a line, not at a pixel offset from the top of the
//! scrollback: [`ScrollPosition`] names the top line, the wrap row inside it, and how
//! many pixels of that row are scrolled out above the top edge. An anchor survives
//! appends and eviction (ids are stable), and it means nothing here ever needs the
//! total height of the scrollback.
//!
//! **Cost.** Unwrapped, every line is one row, so every function is O(1) arithmetic
//! plus O(visible rows) to list them, whatever the line count. Wrapped, rows per line
//! come from a [`RowCounter`] backed by the wrap-count cache, and the functions walk
//! lines from the anchor: `visible_rows` and `bottom` visit only the lines on screen,
//! and `scroll_by` visits the lines scrolled past, so a wheel step or a page costs
//! O(rows moved + rows visible) and never O(line count). Jumps that name a line
//! directly (home, end, the scrollbar) cost the same as drawing one screen. The
//! scrollbar in wrapped mode is therefore positioned by line index, not by row,
//! which is exact for unwrapped text and a close estimate for wrapped text.

use std::ops::Range;

use serialist_core::LineId;

/// Rows a line occupies at the current width.
pub trait RowCounter {
    fn rows(&mut self, line: LineId) -> u32;

    /// `true` when every line is one row, which turns walks into arithmetic.
    fn is_uniform(&self) -> bool {
        false
    }
}

/// Unwrapped text: one row per line.
pub struct OneRowEach;

impl RowCounter for OneRowEach {
    fn rows(&mut self, _: LineId) -> u32 {
        1
    }

    fn is_uniform(&self) -> bool {
        true
    }
}

impl<F: FnMut(LineId) -> u32> RowCounter for F {
    fn rows(&mut self, line: LineId) -> u32 {
        self(line).max(1)
    }
}

/// Rows a line of `chars` cells takes at `columns` cells per row; at least one.
pub fn wrap_rows(chars: usize, columns: usize) -> u32 {
    if columns == 0 {
        return 1;
    }
    chars.div_ceil(columns).max(1) as u32
}

/// The top of the viewport.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollPosition {
    pub line: LineId,
    /// Wrap row within `line`; 0 when unwrapped.
    pub row: u32,
    /// Pixels of that row above the top edge, in `0.0..row_height`.
    pub pixel: f32,
}

impl ScrollPosition {
    pub fn at(line: LineId) -> Self {
        Self {
            line,
            row: 0,
            pixel: 0.0,
        }
    }

    fn key(&self) -> (LineId, u32, f32) {
        (self.line, self.row, self.pixel)
    }

    fn is_after(&self, other: &ScrollPosition) -> bool {
        let (a, b) = (self.key(), other.key());
        (a.0, a.1) > (b.0, b.1) || ((a.0, a.1) == (b.0, b.1) && a.2 > b.2 + 0.01)
    }
}

/// The lines being displayed: retained lines, cut at the frozen end while paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub first: LineId,
    pub end: LineId,
}

impl Span {
    pub fn new(first: LineId, end: LineId) -> Self {
        Self {
            first,
            end: end.max(first),
        }
    }

    pub fn len(&self) -> usize {
        (self.end.0 - self.first.0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.first
    }

    pub fn contains(&self, line: LineId) -> bool {
        self.first <= line && line < self.end
    }

    pub fn range(&self) -> Range<LineId> {
        self.first..self.end
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub height: f32,
    pub row_height: f32,
}

impl Viewport {
    /// Whole rows that fit, and one more for a partly visible row at the bottom.
    pub fn row_capacity(&self) -> usize {
        if self.row_height <= 0.0 {
            return 0;
        }
        (self.height / self.row_height).ceil().max(0.0) as usize + 1
    }
}

/// Scrolled all the way up.
pub fn top(span: Span) -> ScrollPosition {
    ScrollPosition::at(span.first)
}

/// Scrolled all the way down: the last row's bottom edge on the viewport's bottom
/// edge, or the top when everything fits. Visits only the lines on screen.
pub fn bottom(span: Span, viewport: Viewport, rows: &mut dyn RowCounter) -> ScrollPosition {
    let row_height = viewport.row_height;
    if span.is_empty() || row_height <= 0.0 {
        return top(span);
    }
    if rows.is_uniform() {
        let total = span.len() as f64 * row_height as f64;
        let offset = (total - viewport.height as f64).max(0.0);
        return from_uniform_offset(span, row_height, offset);
    }
    let mut needed = viewport.height;
    let mut line = span.end;
    while line > span.first {
        line = LineId(line.0 - 1);
        let height = rows.rows(line) as f32 * row_height;
        if height >= needed {
            return within(line, height - needed, row_height);
        }
        needed -= height;
    }
    top(span)
}

/// A position `offset` pixels below the top of `line`.
fn within(line: LineId, offset: f32, row_height: f32) -> ScrollPosition {
    let row = (offset / row_height).floor().max(0.0);
    ScrollPosition {
        line,
        row: row as u32,
        pixel: (offset - row * row_height).clamp(0.0, row_height),
    }
}

fn from_uniform_offset(span: Span, row_height: f32, offset: f64) -> ScrollPosition {
    let rows = (offset / row_height as f64).floor();
    let index = (rows as u64).min(span.len().saturating_sub(1) as u64);
    ScrollPosition {
        line: span.first.offset(index as usize),
        row: 0,
        pixel: (offset - index as f64 * row_height as f64).clamp(0.0, row_height as f64) as f32,
    }
}

/// Bring a stored position back inside the span: an evicted anchor moves to the first
/// retained line, one past the end or past the bottom moves to the bottom, and a wrap
/// row that no longer exists (the width grew) moves to the line's last row.
pub fn clamp(
    position: ScrollPosition,
    span: Span,
    viewport: Viewport,
    rows: &mut dyn RowCounter,
) -> ScrollPosition {
    if span.is_empty() || position.line < span.first {
        return top(span);
    }
    let bottom = bottom(span, viewport, rows);
    if position.line >= span.end {
        return bottom;
    }
    let max_row = rows.rows(position.line).saturating_sub(1);
    let position = ScrollPosition {
        line: position.line,
        row: position.row.min(max_row),
        pixel: position.pixel.clamp(0.0, viewport.row_height.max(0.0)),
    };
    if position.is_after(&bottom) {
        bottom
    } else {
        position
    }
}

/// Whether `position` is scrolled all the way down.
pub fn is_at_bottom(
    position: ScrollPosition,
    span: Span,
    viewport: Viewport,
    rows: &mut dyn RowCounter,
) -> bool {
    !bottom(span, viewport, rows).is_after(&position)
}

/// Scroll by `delta` pixels; positive moves toward older lines (up), as a wheel
/// moving content down does. Clamped to the top and the bottom.
pub fn scroll_by(
    position: ScrollPosition,
    delta: f32,
    span: Span,
    viewport: Viewport,
    rows: &mut dyn RowCounter,
) -> ScrollPosition {
    let row_height = viewport.row_height;
    let position = clamp(position, span, viewport, rows);
    if span.is_empty() || row_height <= 0.0 || delta == 0.0 {
        return position;
    }
    if rows.is_uniform() {
        let current =
            (position.line.0 - span.first.0) as f64 * row_height as f64 + position.pixel as f64;
        let total = span.len() as f64 * row_height as f64;
        let max = (total - viewport.height as f64).max(0.0);
        let offset = (current - delta as f64).clamp(0.0, max);
        return from_uniform_offset(span, row_height, offset);
    }
    let mut line = position.line;
    let mut offset = position.row as f32 * row_height + position.pixel;
    if delta > 0.0 {
        let mut remaining = delta;
        loop {
            if remaining <= offset {
                offset -= remaining;
                break;
            }
            remaining -= offset;
            if line <= span.first {
                offset = 0.0;
                break;
            }
            line = LineId(line.0 - 1);
            offset = rows.rows(line) as f32 * row_height;
        }
        within(line, offset, row_height)
    } else {
        let bottom = bottom(span, viewport, rows);
        let mut remaining = -delta;
        loop {
            // Nothing below the bottom position is ever the top of the viewport, so
            // the walk stops there however large the delta.
            if line >= bottom.line {
                break;
            }
            let height = rows.rows(line) as f32 * row_height;
            if offset + remaining < height {
                offset += remaining;
                remaining = 0.0;
                break;
            }
            remaining -= height - offset;
            offset = 0.0;
            line = line.next();
        }
        let moved = within(line, offset + remaining, row_height);
        if moved.is_after(&bottom) {
            bottom
        } else {
            moved
        }
    }
}

/// One visual row on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisibleRow {
    pub line: LineId,
    /// Wrap row within the line.
    pub row: u32,
    /// Rows the whole line takes.
    pub line_rows: u32,
    /// Top of the row relative to the viewport's top edge; the first may be negative.
    pub y: f32,
}

/// The rows to draw, top to bottom, starting at `position`. Visits only these rows.
pub fn visible_rows(
    position: ScrollPosition,
    span: Span,
    viewport: Viewport,
    rows: &mut dyn RowCounter,
) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    if span.is_empty() || viewport.row_height <= 0.0 || position.line < span.first {
        return out;
    }
    let mut y = -position.pixel;
    let mut line = position.line;
    let mut row = position.row;
    while line < span.end && y < viewport.height {
        let line_rows = rows.rows(line);
        while row < line_rows && y < viewport.height {
            out.push(VisibleRow {
                line,
                row,
                line_rows,
                y,
            });
            y += viewport.row_height;
            row += 1;
        }
        line = line.next();
        row = 0;
    }
    out
}

/// The lines [`visible_rows`] would touch from `position`, without counting rows:
/// every line is at least one row, so this many lines from the anchor always cover the
/// screen. Used to fetch everything a frame needs in one call.
pub fn fetch_window(position: ScrollPosition, span: Span, viewport: Viewport) -> Range<LineId> {
    let start = position.line.max(span.first).min(span.end);
    let end = start.offset(viewport.row_capacity()).min(span.end);
    start..end
}

/// Where the scrollbar thumb sits: `offset` in `0.0..=1.0` of the travel, and the
/// share of the content that is visible. Line-based (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarPosition {
    pub fraction: f64,
    pub visible: f64,
    /// Whether there is anything to scroll at all.
    pub scrollable: bool,
}

pub fn scrollbar_position(
    position: ScrollPosition,
    position_rows: u32,
    span: Span,
    viewport: Viewport,
    at_top: bool,
    at_bottom: bool,
) -> ScrollbarPosition {
    let visible_rows = if viewport.row_height > 0.0 {
        viewport.height as f64 / viewport.row_height as f64
    } else {
        0.0
    };
    let lines = span.len() as f64;
    let scrollable = !(at_top && at_bottom);
    if !scrollable {
        return ScrollbarPosition {
            fraction: 0.0,
            visible: 1.0,
            scrollable,
        };
    }
    // Wrapped text can scroll with fewer lines than rows on screen; give the thumb
    // something to travel.
    let travel = (lines - visible_rows).max(1.0);
    let fraction = if at_bottom {
        1.0
    } else if at_top {
        0.0
    } else {
        let into_line = (position.row as f64 * viewport.row_height as f64 + position.pixel as f64)
            / (position_rows.max(1) as f64 * viewport.row_height as f64);
        (((position.line.0 - span.first.0) as f64 + into_line) / travel).clamp(0.0, 1.0)
    };
    ScrollbarPosition {
        fraction,
        visible: (visible_rows / (travel + visible_rows)).clamp(0.0, 1.0),
        scrollable,
    }
}

/// The inverse of [`scrollbar_position`]: the top line, as a fractional line index from
/// `span.first`, for a thumb dragged to `fraction`.
pub fn line_for_scrollbar(fraction: f64, span: Span, viewport: Viewport) -> f64 {
    let visible_rows = if viewport.row_height > 0.0 {
        viewport.height as f64 / viewport.row_height as f64
    } else {
        0.0
    };
    let travel = (span.len() as f64 - visible_rows).max(1.0);
    fraction.clamp(0.0, 1.0) * travel
}

/// A position `line_index` lines (fractional) below `span.first`.
pub fn position_at_line(
    line_index: f64,
    span: Span,
    viewport: Viewport,
    rows: &mut dyn RowCounter,
) -> ScrollPosition {
    if span.is_empty() {
        return top(span);
    }
    let whole = (line_index.max(0.0).floor() as usize).min(span.len() - 1);
    let line = span.first.offset(whole);
    let fraction = (line_index - whole as f64).clamp(0.0, 1.0) as f32;
    let height = rows.rows(line) as f32 * viewport.row_height;
    clamp(
        within(line, fraction * height, viewport.row_height),
        span,
        viewport,
        rows,
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const ROW: f32 = 20.0;

    fn viewport(rows: f32) -> Viewport {
        Viewport {
            height: rows * ROW,
            row_height: ROW,
        }
    }

    fn span(first: u64, end: u64) -> Span {
        Span::new(LineId(first), LineId(end))
    }

    fn pos(line: u64, row: u32, pixel: f32) -> ScrollPosition {
        ScrollPosition {
            line: LineId(line),
            row,
            pixel,
        }
    }

    fn lines_of(rows: &[VisibleRow]) -> Vec<(u64, u32)> {
        rows.iter().map(|r| (r.line.0, r.row)).collect()
    }

    /// A wrapped layout where line `n` takes `1 + n % 3` rows, counting visits.
    struct Wrapped {
        visits: HashMap<LineId, usize>,
    }

    impl Wrapped {
        fn new() -> Self {
            Self {
                visits: HashMap::new(),
            }
        }

        fn visited(&self) -> usize {
            self.visits.values().sum()
        }
    }

    impl RowCounter for Wrapped {
        fn rows(&mut self, line: LineId) -> u32 {
            *self.visits.entry(line).or_default() += 1;
            1 + (line.0 % 3) as u32
        }
    }

    #[test]
    fn wrap_rows_rounds_up_and_never_returns_zero() {
        assert_eq!(wrap_rows(0, 80), 1);
        assert_eq!(wrap_rows(80, 80), 1);
        assert_eq!(wrap_rows(81, 80), 2);
        assert_eq!(wrap_rows(240, 80), 3);
        assert_eq!(wrap_rows(10, 0), 1, "a zero-width viewport does not divide");
    }

    #[test]
    fn unwrapped_top_bottom_and_middle() {
        let span = span(0, 1000);
        let vp = viewport(10.5);
        let rows = &mut OneRowEach;

        let top = top(span);
        let visible = visible_rows(top, span, vp, rows);
        assert_eq!(visible.len(), 11, "ten whole rows and half of the eleventh");
        assert_eq!(visible[0].y, 0.0);
        assert_eq!(visible[0].line, LineId(0));

        let bottom = bottom(span, vp, rows);
        assert_eq!(
            bottom,
            pos(989, 0, 10.0),
            "last row flush with the bottom edge"
        );
        let visible = visible_rows(bottom, span, vp, rows);
        assert_eq!(visible.first().unwrap().line, LineId(989));
        assert_eq!(visible.last().unwrap().line, LineId(999));
        let last = visible.last().unwrap();
        assert_eq!(last.y + ROW, vp.height, "bottom edge aligned");
        assert!(is_at_bottom(bottom, span, vp, rows));
        assert!(!is_at_bottom(top, span, vp, rows));

        // Scrolling moves by pixels and clamps at both ends.
        let moved = scroll_by(bottom, 35.0, span, vp, rows);
        assert_eq!(moved, pos(987, 0, 15.0));
        assert_eq!(scroll_by(top, 5.0, span, vp, rows), top);
        assert_eq!(scroll_by(moved, -1e9, span, vp, rows), bottom);
        assert_eq!(scroll_by(bottom, 1e9, span, vp, rows), top);
    }

    #[test]
    fn unwrapped_cost_does_not_depend_on_line_count() {
        // Ten million lines: bottom and a huge scroll are arithmetic, and listing the
        // rows visits only the rows on screen.
        let span = span(5, 10_000_005);
        let vp = viewport(40.0);
        let mut counted = 0usize;
        let mut rows = |_: LineId| {
            counted += 1;
            1
        };
        let bottom_pos = bottom(span, vp, &mut OneRowEach);
        let visible = visible_rows(bottom_pos, span, vp, &mut rows);
        assert_eq!(visible.len(), 40);
        assert_eq!(counted, 40);
        let middle = scroll_by(bottom_pos, 5_000_000.0 * ROW, span, vp, &mut OneRowEach);
        assert_eq!(middle.line, LineId(10_000_005 - 40 - 5_000_000));
    }

    #[test]
    fn content_shorter_than_the_viewport_sits_at_the_top() {
        let span = span(0, 3);
        let vp = viewport(10.0);
        let rows = &mut OneRowEach;
        assert_eq!(bottom(span, vp, rows), top(span));
        assert!(is_at_bottom(top(span), span, vp, rows));
        assert_eq!(scroll_by(top(span), 50.0, span, vp, rows), top(span));
        assert_eq!(scroll_by(top(span), -50.0, span, vp, rows), top(span));

        let empty = Span::new(LineId(7), LineId(7));
        assert!(visible_rows(top(empty), empty, vp, rows).is_empty());
        assert_eq!(bottom(empty, vp, rows), ScrollPosition::at(LineId(7)));
    }

    #[test]
    fn eviction_moves_an_evicted_anchor_to_the_first_retained_line() {
        let vp = viewport(5.0);
        let rows = &mut OneRowEach;
        let anchored = pos(10, 0, 4.0);
        // Lines 0..12 evicted: the anchor is gone.
        let after = span(12, 500);
        assert_eq!(clamp(anchored, after, vp, rows), top(after));
        let visible = visible_rows(clamp(anchored, after, vp, rows), after, vp, rows);
        assert_eq!(visible[0].line, LineId(12));
        // Eviction below the anchor leaves it alone.
        let after = span(8, 500);
        assert_eq!(clamp(anchored, after, vp, rows), anchored);
        // New lines at the end leave it alone too.
        assert_eq!(clamp(anchored, span(8, 900), vp, rows), anchored);
        // Everything evicted and replaced: the bottom.
        let after = span(600, 700);
        assert_eq!(clamp(anchored, after, vp, rows), top(after));
        assert_eq!(
            clamp(pos(800, 0, 0.0), after, vp, rows),
            bottom(after, vp, rows)
        );
    }

    #[test]
    fn wrapped_rows_list_every_wrap_row_in_order() {
        // Line n takes 1 + n % 3 rows: 0→1, 1→2, 2→3, 3→1, 4→2 ...
        let span = span(0, 100);
        let vp = viewport(6.0);
        let mut rows = Wrapped::new();
        let visible = visible_rows(pos(1, 1, 0.0), span, vp, &mut rows);
        assert_eq!(
            lines_of(&visible),
            [(1, 1), (2, 0), (2, 1), (2, 2), (3, 0), (4, 0)]
        );
        assert_eq!(visible[1].line_rows, 3);
        assert_eq!(visible[5].y, 5.0 * ROW);
        assert_eq!(rows.visited(), 4, "only the lines on screen were counted");
    }

    #[test]
    fn wrapped_bottom_walks_back_only_across_the_screen() {
        let span = span(0, 1_000_000);
        let vp = viewport(5.5);
        let mut rows = Wrapped::new();
        let bottom_pos = bottom(span, vp, &mut rows);
        // Line 999_999 has 1 row, 999_998 has 3, 999_997 has 2: 6 rows cover 5.5.
        assert_eq!(bottom_pos, pos(999_997, 0, 10.0));
        assert!(rows.visited() <= 4);
        let visible = visible_rows(bottom_pos, span, vp, &mut rows);
        assert_eq!(
            lines_of(&visible),
            [
                (999_997, 0),
                (999_997, 1),
                (999_998, 0),
                (999_998, 1),
                (999_998, 2),
                (999_999, 0)
            ]
        );
        assert_eq!(visible.last().unwrap().y + ROW, vp.height);
    }

    #[test]
    fn wrapped_scrolling_crosses_wrap_rows_and_lines() {
        let span = span(0, 1_000_000);
        let vp = viewport(4.0);
        let mut rows = Wrapped::new();
        // Down one row at a time from the top: (0,0) (1,0) (1,1) (2,0) (2,1) (2,2) (3,0).
        let mut at = top(span);
        let mut seen = vec![(at.line.0, at.row)];
        for _ in 0..6 {
            at = scroll_by(at, -ROW, span, vp, &mut rows);
            seen.push((at.line.0, at.row));
        }
        assert_eq!(
            seen,
            [(0, 0), (1, 0), (1, 1), (2, 0), (2, 1), (2, 2), (3, 0)]
        );
        // And back up, with a pixel remainder on the way.
        let half = scroll_by(at, ROW * 1.5, span, vp, &mut rows);
        assert_eq!(half, pos(2, 1, 10.0));
        assert_eq!(scroll_by(half, 1e7, span, vp, &mut rows), top(span));

        // A page from far down visits about a page of lines, not the whole buffer.
        let mut rows = Wrapped::new();
        let deep = pos(500_000, 0, 0.0);
        let paged = scroll_by(deep, -vp.height, span, vp, &mut rows);
        assert!(paged.line > deep.line);
        assert!(
            rows.visited() < 64,
            "visited {} lines for one page",
            rows.visited()
        );
    }

    #[test]
    fn wrapped_scroll_down_stops_at_the_bottom() {
        let span = span(0, 50);
        let vp = viewport(4.0);
        let mut rows = Wrapped::new();
        let bottom_pos = bottom(span, vp, &mut rows);
        let way_down = scroll_by(top(span), -1e6, span, vp, &mut rows);
        assert_eq!(way_down, bottom_pos);
        assert!(is_at_bottom(way_down, span, vp, &mut rows));
    }

    #[test]
    fn wrapped_clamp_handles_a_width_change_and_eviction() {
        let vp = viewport(4.0);
        // The anchor was on row 5 of a line that now wraps into 3 rows.
        let mut rows = |_: LineId| 3;
        let clamped = clamp(pos(10, 5, 3.0), span(0, 100), vp, &mut rows);
        assert_eq!(clamped, pos(10, 2, 3.0));
        let mut rows = Wrapped::new();
        let after = span(20, 100);
        assert_eq!(clamp(pos(10, 1, 0.0), after, vp, &mut rows), top(after));
    }

    #[test]
    fn fetch_window_covers_every_visible_line() {
        let span = span(0, 1000);
        let vp = viewport(10.5);
        let mut rows = Wrapped::new();
        for start in [0u64, 7, 500, 990, 999] {
            let at = clamp(pos(start, 0, 0.0), span, vp, &mut rows);
            let window = fetch_window(at, span, vp);
            for row in visible_rows(at, span, vp, &mut rows) {
                assert!(
                    window.contains(&row.line),
                    "{:?} not in {window:?}",
                    row.line
                );
            }
            assert!(window.end.0 - window.start.0 <= vp.row_capacity() as u64);
        }
    }

    #[test]
    fn scrollbar_round_trips_by_line() {
        let span = span(100, 1100);
        let vp = viewport(10.0);
        let rows = &mut OneRowEach;
        let at_bottom = scrollbar_position(bottom(span, vp, rows), 1, span, vp, false, true);
        assert_eq!(at_bottom.fraction, 1.0);
        assert!((at_bottom.visible - 0.01).abs() < 1e-9);
        let at_top = scrollbar_position(top(span), 1, span, vp, true, false);
        assert_eq!(at_top.fraction, 0.0);

        let middle = pos(595, 0, 0.0);
        let bar = scrollbar_position(middle, 1, span, vp, false, false);
        assert!((bar.fraction - 0.5).abs() < 1e-9);
        let back = line_for_scrollbar(bar.fraction, span, vp);
        assert_eq!(position_at_line(back, span, vp, rows), middle);

        let fits = scrollbar_position(top(span), 1, span, vp, true, true);
        assert!(!fits.scrollable);
    }
}
