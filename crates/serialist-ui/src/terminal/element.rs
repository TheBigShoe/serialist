//! The terminal element: a bespoke GPUI [`Element`] that draws a [`LineSource`].
//!
//! Modeled on the shape of Zed's `TerminalElement` (the pattern, not its code): layout
//! work happens in `prepaint`, drawing and input registration in `paint`.
//!
//! **Prepaint** resolves the font's metrics, fixes the scroll position against this
//! frame's lines (see [`layout`](super::layout) for what that costs), fetches the lines
//! on screen in one call, and turns each into shaped rows. Shaped lines live in an LRU
//! keyed by line id and validated by a hash of the line's text and runs plus the wrap
//! width, so scrolling back over text already seen shapes nothing. Glyphs are placed
//! on a fixed cell grid (`force_width`), which makes columns, selection and wrapping
//! exact arithmetic. It then lays out backgrounds, search matches, the selection and
//! the timestamp gutter as plain rectangles and short lines.
//!
//! **Paint** clips to the bounds, fills the background, paints run backgrounds, search
//! highlights and the selection under the text, the shaped rows, then the gutter, and
//! registers mouse handlers for selection and scrolling.
//!
//! Nothing in either phase depends on the number of lines retained.
//!
//! # A terminal screen
//!
//! In VT mode the source is a terminal screen's snapshot and the element gets
//! [`ScreenInputs`]. The grid is already wrapped, so nothing wraps; the viewport is cut to
//! whole rows, so following the tail keeps the screen's top row at the element's top (the
//! scrollback above it scrolls into view like any other lines); the cursor is drawn from
//! the snapshot's [`CursorState`]; and each frame the element works out how many cells
//! fit (columns from the text area's width, rows from its height, over the cell
//! metrics) and, when that differs from the screen's size, asks for a resize through
//! [`ScreenInputs::resize`]. The request runs after the frame (`App::defer`, since
//! nothing can be notified mid-draw), once per size, so a window being dragged costs at
//! most one resize a frame.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use std::cell::Cell;

use serialist_core::{Direction, LineId, LineSource, SearchMatch, Style, StyledLine};
use serialist_vt::{CursorShape, CursorState};

use crate::config::Config;
use crate::prelude::*;
use crate::terminal::cache::Lru;
use crate::terminal::layout::{self, OneRowEach, RowCounter, Span, Viewport, wrap_rows};
use crate::terminal::palette::{ResolvedStyle, TerminalPalette};
use crate::terminal::scroll::TerminalScrollHandle;
use crate::terminal::selection::{Selection, SelectionPoint, column_of_byte};
use crate::terminal::stats::{FrameSample, FrameStats};
use crate::terminal::timestamps::{Clock, TimestampMode};
use crate::terminal::view::TerminalView;

/// Space between the element's left edge and the gutter or the text.
pub const PADDING_LEFT: Pixels = px(8.);
/// Room on the right for the scrollbar thumb, so it never covers text.
pub const PADDING_RIGHT: Pixels = px(14.);
/// Line height as a multiple of the font size, when the font's own ascent and descent
/// are tighter. Zed calls 1.3 "standard"; settings choose another.
pub const LINE_HEIGHT: f32 = crate::fonts::STANDARD_LINE_HEIGHT;

/// Shaped lines kept across frames: about forty screens of text.
const SHAPED_LINES: usize = 2048;
/// Wrap counts kept for lines scrolled past but not necessarily on screen.
const WRAP_COUNTS: usize = 64 * 1024;

/// The thickness of an underline or bar cursor.
pub const CURSOR_THICKNESS: Pixels = px(2.);

/// The grid every line is drawn on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub cell_width: Pixels,
    pub row_height: Pixels,
}

impl CellMetrics {
    /// The grid for `font` at `font_size`, rows `line_height` times the size apart
    /// unless the font's own ascent and descent need more.
    pub fn measure(font: &Font, font_size: Pixels, line_height: f32, window: &Window) -> Self {
        let text_system = window.text_system();
        let font_id = text_system.resolve_font(font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);
        let ascent = text_system.ascent(font_id, font_size);
        // GPUI reports the descent below the baseline as a negative number.
        let descent = text_system.descent(font_id, font_size).abs();
        let row_height = (ascent + descent).max(font_size * line_height).ceil();
        Self {
            cell_width,
            row_height,
        }
    }
}

/// What identifies a shaped line's content: if the store hands back a line with the
/// same id but different text or runs (an incomplete line that grew), it reshapes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryKey {
    content: u64,
    /// Wrap width in cells, 0 when unwrapped.
    columns: usize,
}

impl EntryKey {
    fn of(line: &StyledLine, columns: usize) -> Self {
        let mut hasher = DefaultHasher::new();
        line.text.hash(&mut hasher);
        line.runs.hash(&mut hasher);
        line.direction.hash(&mut hasher);
        Self {
            content: hasher.finish(),
            columns,
        }
    }
}

/// One wrap row of a shaped line.
#[derive(Clone, Debug)]
struct ShapedRow {
    shaped: ShapedLine,
    /// Column of the row's first cell within the line.
    start: usize,
    /// Cells in the row.
    len: usize,
    /// Run backgrounds, in columns relative to the row.
    backgrounds: Vec<(Range<usize>, Hsla)>,
}

#[derive(Clone, Debug)]
struct ShapedEntry {
    key: EntryKey,
    rows: Vec<ShapedRow>,
}

/// Everything the element keeps between frames. Owned by the view, borrowed by the
/// element in prepaint.
pub struct ShapeCache {
    generation: u64,
    /// The font, size and line height (as bits) the metrics and shaped lines are for.
    style: Option<(Font, Pixels, u32)>,
    metrics: Option<CellMetrics>,
    lines: Lru<LineId, ShapedEntry>,
    wraps: Lru<LineId, u32>,
    wrap_columns: usize,
    /// Columns and rows of text that fit the element, as of the last frame.
    grid: Option<(usize, usize)>,
}

impl Default for ShapeCache {
    fn default() -> Self {
        Self {
            generation: 0,
            style: None,
            metrics: None,
            lines: Lru::new(SHAPED_LINES),
            wraps: Lru::new(WRAP_COUNTS),
            wrap_columns: 0,
            grid: None,
        }
    }
}

impl ShapeCache {
    /// Drop everything when the source, palette or font changed, and return the grid.
    fn prepare(
        &mut self,
        generation: u64,
        font: &Font,
        font_size: Pixels,
        line_height: f32,
        window: &Window,
    ) -> CellMetrics {
        if generation != self.generation {
            self.generation = generation;
            self.lines.clear();
            self.wraps.clear();
        }
        let style = (font.clone(), font_size, line_height.to_bits());
        if self.style.as_ref() != Some(&style) || self.metrics.is_none() {
            self.metrics = Some(CellMetrics::measure(font, font_size, line_height, window));
            self.style = Some(style);
            self.lines.clear();
            self.wraps.clear();
        }
        self.metrics.expect("measured above")
    }

    /// Wrap counts are only good for the width they were counted at.
    fn set_wrap_columns(&mut self, columns: usize) {
        if columns != self.wrap_columns {
            self.wrap_columns = columns;
            self.wraps.clear();
        }
    }

    pub fn shaped_len(&self) -> usize {
        self.lines.len()
    }

    /// The grid of the last frame, once one was drawn.
    pub fn metrics(&self) -> Option<CellMetrics> {
        self.metrics
    }

    /// The columns and rows of text that fit the element, as of the last frame: what a
    /// terminal screen is sized to.
    pub fn grid(&self) -> Option<(usize, usize)> {
        self.grid
    }
}

/// Lines fetched for one frame.
#[derive(Default)]
struct FrameLines {
    lines: HashMap<LineId, StyledLine>,
    fetched: usize,
    scratch: Vec<StyledLine>,
}

impl FrameLines {
    fn fetch(&mut self, source: &dyn LineSource, range: Range<LineId>) {
        if range.start >= range.end {
            return;
        }
        self.scratch.clear();
        source.lines(range, &mut self.scratch);
        self.fetched += self.scratch.len();
        for line in self.scratch.drain(..) {
            self.lines.insert(line.id, line);
        }
    }

    /// Fetch whatever part of `range` is not here yet, in contiguous pieces.
    fn ensure(&mut self, source: &dyn LineSource, range: Range<LineId>) {
        let mut id = range.start;
        while id < range.end {
            if self.lines.contains_key(&id) {
                id = id.next();
                continue;
            }
            let start = id;
            while id < range.end && !self.lines.contains_key(&id) {
                id = id.next();
            }
            self.fetch(source, start..id);
        }
    }

    fn get_or_fetch(&mut self, source: &dyn LineSource, id: LineId) -> Option<&StyledLine> {
        if !self.lines.contains_key(&id) {
            let line = source.line(id)?;
            self.fetched += 1;
            self.lines.insert(id, line);
        }
        self.lines.get(&id)
    }
}

/// Rows per line at the current width, from the wrap-count cache or, on a miss, the
/// line's length. Only complete lines are cached: the last line may still grow.
struct WrapCounter<'a> {
    wraps: &'a mut Lru<LineId, u32>,
    frame: &'a mut FrameLines,
    source: &'a dyn LineSource,
    columns: usize,
}

impl RowCounter for WrapCounter<'_> {
    fn rows(&mut self, line: LineId) -> u32 {
        if let Some(rows) = self.wraps.get(&line).copied() {
            return rows;
        }
        let Some(styled) = self.frame.get_or_fetch(self.source, line) else {
            return 1;
        };
        let rows = wrap_rows(styled.text.chars().count(), self.columns);
        if styled.complete {
            self.wraps.insert(line, rows);
        }
        rows
    }
}

/// Search highlights handed to the element.
#[derive(Clone, Debug, Default)]
pub struct Highlights {
    /// Sorted by line, then start.
    pub matches: Arc<Vec<SearchMatch>>,
    pub active: Option<usize>,
}

/// One visual row for hit testing.
#[derive(Clone, Copy, Debug)]
pub struct HitRow {
    pub line: LineId,
    /// Top of the row relative to the element's top.
    pub y: f32,
    pub start: usize,
    pub len: usize,
    /// The line's last wrap row.
    pub last: bool,
}

/// Where the last frame put things, for turning pointer positions into columns.
#[derive(Clone, Debug)]
pub struct HitMap {
    pub bounds: Bounds<Pixels>,
    /// x of column 0 after horizontal scrolling.
    pub column_origin: Pixels,
    pub cell_width: f32,
    pub row_height: f32,
    pub rows: Vec<HitRow>,
}

/// A pointer position in text terms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The caret position nearest the pointer, for drag selection.
    pub caret: SelectionPoint,
    /// The cell under the pointer, for word and line selection.
    pub cell: SelectionPoint,
}

impl HitMap {
    pub fn hit(&self, position: Point<Pixels>) -> Option<Hit> {
        let first = self.rows.first()?;
        let last = self.rows.last()?;
        let y = f32::from(position.y - self.bounds.top());
        let x = f32::from(position.x - self.column_origin).max(0.0) / self.cell_width;
        let caret_in = |row: &HitRow| {
            let column = (x.round() as usize).min(row.len);
            SelectionPoint::new(row.line, row.start + column)
        };
        let cell_in = |row: &HitRow| {
            let column = (x.floor() as usize).min(row.len.saturating_sub(1));
            SelectionPoint::new(row.line, row.start + column)
        };
        if y < first.y {
            let caret = SelectionPoint::new(first.line, first.start);
            return Some(Hit { caret, cell: caret });
        }
        if y >= last.y + self.row_height {
            let caret = SelectionPoint::new(last.line, last.start + last.len);
            return Some(Hit {
                caret,
                cell: cell_in(last),
            });
        }
        let row = self
            .rows
            .iter()
            .rev()
            .find(|row| row.y <= y)
            .unwrap_or(first);
        Some(Hit {
            caret: caret_in(row),
            cell: cell_in(row),
        })
    }
}

/// Asks for a terminal screen of `columns` by `rows` cells.
pub type ResizeScreen = Rc<dyn Fn(usize, usize, &mut App)>;

/// A terminal screen on display (VT mode): see the module docs.
#[derive(Clone)]
pub struct ScreenInputs {
    /// The screen's size in cells, as its snapshot says.
    pub columns: usize,
    pub rows: usize,
    /// The cursor to draw: `None` while the device hides it, or a blink has it off.
    pub cursor: Option<CursorState>,
    /// Asked, after the frame, for the screen to be `columns` by `rows` when that is what
    /// fits the element and not the screen's size.
    pub resize: ResizeScreen,
    /// The size last asked for, so a size is asked for once.
    pub requested: Rc<Cell<Option<(usize, usize)>>>,
}

/// What the view hands the element each frame.
pub struct TerminalInputs {
    pub view: Entity<TerminalView>,
    pub source: Arc<dyn LineSource>,
    /// Pause: draw `first_line()..frozen_end` and follow that end instead of the live one.
    pub frozen_end: Option<LineId>,
    pub scroll: TerminalScrollHandle,
    pub cache: Rc<RefCell<ShapeCache>>,
    pub stats: Rc<RefCell<FrameStats>>,
    pub palette: Rc<TerminalPalette>,
    /// Bumped by the view whenever the source or palette changes, dropping the caches.
    pub generation: u64,
    pub font: Font,
    pub font_size: Pixels,
    /// Row pitch as a multiple of `font_size`.
    pub line_height: f32,
    pub wrap: bool,
    pub timestamps: TimestampMode,
    pub clock: Clock,
    pub selection: Option<Selection>,
    pub highlights: Highlights,
    pub focus: FocusHandle,
    /// Set when the source is a terminal screen (VT mode).
    pub screen: Option<ScreenInputs>,
}

pub struct TerminalElement {
    inputs: TerminalInputs,
}

impl TerminalElement {
    pub fn new(inputs: TerminalInputs) -> Self {
        Self { inputs }
    }
}

struct PaintRow {
    origin: Point<Pixels>,
    shaped: ShapedLine,
}

/// Prepaint's output.
pub struct TerminalLayout {
    hitbox: Hitbox,
    text_clip: Bounds<Pixels>,
    background: Hsla,
    row_height: Pixels,
    rects: Vec<(Bounds<Pixels>, Hsla)>,
    rows: Vec<PaintRow>,
    gutter: Vec<PaintRow>,
    /// The cursor, painted over the text, and the character under a block cursor again
    /// in the background color, over the block.
    cursor: Vec<PaintQuad>,
    cursor_glyph: Option<PaintRow>,
    hit_map: Rc<HitMap>,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A line's runs as byte ranges covering exactly its text, on char boundaries. Text
/// past the runs takes the default style, so a malformed line still draws.
fn normalized_runs(line: &StyledLine) -> Vec<(Range<usize>, Style)> {
    let text = &line.text;
    let mut out = Vec::with_capacity(line.runs.len() + 1);
    let mut at = 0;
    for run in &line.runs {
        if at >= text.len() {
            break;
        }
        let mut end = (at + run.len).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        if end > at {
            out.push((at..end, run.style));
        }
        at = end;
    }
    if at < text.len() {
        out.push((at..text.len(), Style::default()));
    }
    out
}

/// A line's runs as byte ranges with the colors and decorations they are drawn in. The
/// glyphs of a `CONTROL` run (the store's placeholders for control bytes, when
/// `display.show_control_chars` is on) come out in the palette's dim color, and a decoded
/// frame's summary line (a notice starting with
/// [`DECODED_MARK`](crate::codecs::DECODED_MARK)) in the plugin color.
fn resolved_runs(
    line: &StyledLine,
    palette: &TerminalPalette,
) -> Vec<(Range<usize>, ResolvedStyle)> {
    let decoded =
        line.direction == Direction::Notice && crate::codecs::is_decoded_summary(&line.text);
    let resolve = |style: &Style| {
        if decoded {
            palette.resolve_decoded(style)
        } else {
            palette.resolve(style, line.direction)
        }
    };
    let mut resolved: Vec<(Style, ResolvedStyle)> = Vec::new();
    normalized_runs(line)
        .into_iter()
        .map(|(range, run_style)| {
            let found = resolved
                .iter()
                .find(|(s, _)| *s == run_style)
                .map(|(_, r)| *r);
            let resolved_style = found.unwrap_or_else(|| {
                let r = resolve(&run_style);
                resolved.push((run_style, r));
                r
            });
            (range, resolved_style)
        })
        .collect()
}

struct ShapeStyle<'a> {
    font: &'a Font,
    font_size: Pixels,
    cell_width: Pixels,
    palette: &'a TerminalPalette,
}

fn text_run(font: &Font, len: usize, style: &ResolvedStyle) -> TextRun {
    let mut font = font.clone();
    if style.bold {
        font.weight = FontWeight::BOLD;
    }
    if style.italic {
        font.style = FontStyle::Italic;
    }
    TextRun {
        len,
        font,
        color: style.foreground,
        background_color: None,
        underline: style.underline.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(style.foreground),
            wavy: false,
        }),
        strikethrough: style.strikethrough.then_some(StrikethroughStyle {
            thickness: px(1.),
            color: Some(style.foreground),
        }),
    }
}

/// Shape a line into its wrap rows (one row when `columns` is 0).
fn shape_entry(
    line: &StyledLine,
    key: EntryKey,
    style: &ShapeStyle<'_>,
    window: &Window,
) -> ShapedEntry {
    let text = line.text.as_str();
    // Byte offset of every char, plus the end.
    let mut starts: Vec<usize> = text.char_indices().map(|(ix, _)| ix).collect();
    let chars = starts.len();
    starts.push(text.len());
    let column_of = |byte: usize| starts.partition_point(|&start| start < byte);

    let runs = resolved_runs(line, style.palette);

    let per_row = if key.columns == 0 {
        chars.max(1)
    } else {
        key.columns
    };
    let row_count = wrap_rows(chars, per_row) as usize;
    let mut rows = Vec::with_capacity(row_count);
    for row in 0..row_count {
        let first_col = row * per_row;
        let last_col = (first_col + per_row).min(chars);
        let bytes = starts[first_col.min(chars)]..starts[last_col];
        let mut text_runs = Vec::new();
        let mut backgrounds = Vec::new();
        for (range, resolved_style) in &runs {
            let start = range.start.max(bytes.start);
            let end = range.end.min(bytes.end);
            if start >= end {
                continue;
            }
            text_runs.push(text_run(style.font, end - start, resolved_style));
            if let Some(background) = resolved_style.background {
                backgrounds.push((
                    column_of(start) - first_col..column_of(end) - first_col,
                    background,
                ));
            }
        }
        let shaped = if bytes.is_empty() {
            ShapedLine::default()
        } else {
            window.text_system().shape_line(
                SharedString::from(text[bytes].to_owned()),
                style.font_size,
                &text_runs,
                Some(style.cell_width),
            )
        };
        rows.push(ShapedRow {
            shaped,
            start: first_col,
            len: last_col.saturating_sub(first_col),
            backgrounds,
        });
    }
    ShapedEntry { key, rows }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = TerminalLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = crate::prelude::Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> TerminalLayout {
        let started = Instant::now();
        let inputs = &self.inputs;
        // The format of absolute stamps follows the settings, live: read here every
        // frame, so a saved change shows on the next one. Without a configuration (a
        // test window) the stamps take the default format.
        let timestamp_format = (inputs.timestamps == TimestampMode::Absolute)
            .then(|| cx.try_global::<Config>())
            .flatten()
            .map(|config| config.timestamp_format().to_owned());
        let timestamp_format = timestamp_format.as_deref();
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        inputs.scroll.set_bounds(bounds);

        let mut cache = inputs.cache.borrow_mut();
        let metrics = cache.prepare(
            inputs.generation,
            &inputs.font,
            inputs.font_size,
            inputs.line_height,
            window,
        );
        let (cell_width, row_height) = (metrics.cell_width, metrics.row_height);
        let gutter_cells = inputs.clock.width(inputs.timestamps, timestamp_format);
        let gutter_width = if gutter_cells > 0 {
            cell_width * (gutter_cells + 1) as f32
        } else {
            px(0.)
        };
        let text_left = bounds.left() + PADDING_LEFT + gutter_width;
        let text_right = (bounds.right() - PADDING_RIGHT).max(text_left + cell_width);
        let text_width = text_right - text_left;
        let screen = inputs.screen.as_ref();
        // What fits: a screen is sized to it, and its grid is already wrapped.
        let grid = (
            (f32::from(text_width) / f32::from(cell_width))
                .floor()
                .max(1.0) as usize,
            (f32::from(bounds.size.height) / f32::from(row_height))
                .floor()
                .max(1.0) as usize,
        );
        cache.grid = Some(grid);
        if let Some(screen) = screen
            && grid != (screen.columns, screen.rows)
            && screen.requested.get() != Some(grid)
        {
            screen.requested.set(Some(grid));
            let resize = screen.resize.clone();
            cx.defer(move |cx| resize(grid.0, grid.1, cx));
        }
        let wrap = inputs.wrap && screen.is_none();
        let columns = if wrap {
            (f32::from(text_width) / f32::from(cell_width))
                .floor()
                .max(1.0) as usize
        } else {
            0
        };
        cache.set_wrap_columns(columns);

        let source = inputs.source.as_ref();
        let first = source.first_line();
        let live_end = source.end();
        let end = inputs
            .frozen_end
            .map_or(live_end, |frozen| frozen.min(live_end));
        let span = Span::new(first, end);
        // A screen's rows are whole: the grid's top row sits on the element's top edge
        // when following, and the part row left over at the bottom stays blank.
        let height = match screen {
            Some(_) => grid.1 as f32 * f32::from(row_height),
            None => f32::from(bounds.size.height),
        };
        let viewport = Viewport {
            height,
            row_height: f32::from(row_height),
        };

        // One fetch covers the screen around where the view was: every line is at
        // least one row, so a screen's worth of lines from the anchor is enough.
        let mut frame = FrameLines::default();
        let window_range = if inputs.scroll.is_following() {
            let lines = viewport.row_capacity();
            LineId(end.0.saturating_sub(lines as u64).max(first.0))..end
        } else {
            layout::fetch_window(inputs.scroll.position(), span, viewport)
        };
        frame.fetch(source, window_range);

        let visible = {
            let cache = &mut *cache;
            if wrap {
                let mut rows = WrapCounter {
                    wraps: &mut cache.wraps,
                    frame: &mut frame,
                    source,
                    columns,
                };
                let position = inputs.scroll.resolve(span, viewport, &mut rows);
                layout::visible_rows(position, span, viewport, &mut rows)
            } else {
                let position = inputs.scroll.resolve(span, viewport, &mut OneRowEach);
                layout::visible_rows(position, span, viewport, &mut OneRowEach)
            }
        };
        if let (Some(top), Some(bottom)) = (visible.first(), visible.last()) {
            frame.ensure(source, top.line..bottom.line.next());
            if inputs.timestamps == TimestampMode::Delta && top.line > first {
                frame.ensure(source, LineId(top.line.0 - 1)..top.line);
            }
        }

        // Shape, or find in the cache, each visible line once.
        let shape_style = ShapeStyle {
            font: &inputs.font,
            font_size: inputs.font_size,
            cell_width,
            palette: &inputs.palette,
        };
        let mut sample = FrameSample::default();
        let mut placed: Vec<(usize, ShapedRow)> = Vec::with_capacity(visible.len());
        let mut widest = px(0.);
        let mut ix = 0;
        while ix < visible.len() {
            let id = visible[ix].line;
            let mut next = ix;
            while next < visible.len() && visible[next].line == id {
                next += 1;
            }
            if let Some(line) = frame.lines.get(&id) {
                let key = EntryKey::of(line, columns);
                if cache.lines.peek(&id).is_some_and(|entry| entry.key == key) {
                    sample.cache_hits += 1;
                } else {
                    sample.lines_shaped += 1;
                    let entry = shape_entry(line, key, &shape_style, window);
                    cache.lines.insert(id, entry);
                }
                let entry = cache.lines.get(&id).expect("just checked or inserted");
                for (visible_ix, row) in visible.iter().enumerate().take(next).skip(ix) {
                    if let Some(shaped) = entry.rows.get(row.row as usize) {
                        widest = widest.max(shaped.shaped.width());
                        placed.push((visible_ix, shaped.clone()));
                    }
                }
            }
            ix = next;
        }
        drop(cache);

        let horizontal = if wrap {
            inputs.scroll.reset_horizontal();
            0.0
        } else {
            inputs
                .scroll
                .resolve_horizontal(f32::from(widest + cell_width), f32::from(text_width))
        };
        let column_origin = text_left - px(horizontal);
        let x_of = |column: usize| column_origin + cell_width * column as f32;
        let palette = &inputs.palette;

        let mut rects = Vec::new();
        let mut rows = Vec::with_capacity(placed.len());
        let mut hit_rows = Vec::with_capacity(placed.len());
        // Search matches on the visible lines: a binary search to the first, then in
        // order, so the cost is the matches on screen, not the matches in total.
        let matches = &inputs.highlights.matches;
        let top_line = visible.first().map(|row| row.line);
        let mut match_ix = top_line.map_or(matches.len(), |top| {
            matches.partition_point(|m| m.line < top)
        });

        for (visible_ix, shaped) in &placed {
            let row = visible[*visible_ix];
            let y = bounds.top() + px(row.y);
            let row_bounds = |from: usize, to: usize| {
                Bounds::from_corners(
                    point(x_of(from - shaped.start), y),
                    point(x_of(to - shaped.start), y + row_height),
                )
            };
            let row_end = shaped.start + shaped.len;
            let last = row.row + 1 == row.line_rows;

            for (columns, color) in &shaped.backgrounds {
                rects.push((
                    row_bounds(shaped.start + columns.start, shaped.start + columns.end),
                    *color,
                ));
            }

            let text = frame.lines.get(&row.line).map(|l| l.text.as_str());
            while match_ix < matches.len() && matches[match_ix].line < row.line {
                match_ix += 1;
            }
            let mut m = match_ix;
            while let (Some(found), Some(text)) = (matches.get(m), text) {
                if found.line != row.line {
                    break;
                }
                let from = column_of_byte(text, found.range.start).max(shaped.start);
                let to = column_of_byte(text, found.range.end).min(row_end);
                if from < to {
                    let color = if inputs.highlights.active == Some(m) {
                        palette.active_match
                    } else {
                        palette.search_match
                    };
                    rects.push((row_bounds(from, to), color));
                }
                m += 1;
            }

            if let Some(selection) = &inputs.selection
                && let Some(selected) = selection.columns_on(row.line, span)
            {
                let from = selected.start.max(shaped.start);
                // A selection running past the line's end takes its break too: one
                // extra cell on the last row.
                let to = if selected.end == usize::MAX {
                    if last { row_end + 1 } else { row_end }
                } else {
                    selected.end.min(row_end)
                };
                if from < to {
                    rects.push((row_bounds(from, to), palette.selection));
                }
            }

            rows.push(PaintRow {
                origin: point(column_origin, y),
                shaped: shaped.shaped.clone(),
            });
            hit_rows.push(HitRow {
                line: row.line,
                y: row.y,
                start: shaped.start,
                len: shaped.len,
                last,
            });
        }

        // The gutter: one stamp per line, on its first row.
        let mut gutter = Vec::new();
        if inputs.timestamps != TimestampMode::Off {
            let text_system = window.text_system();
            for row in visible.iter().filter(|row| row.row == 0) {
                let Some(line) = frame.lines.get(&row.line) else {
                    continue;
                };
                let previous = (row.line > first)
                    .then(|| frame.lines.get(&LineId(row.line.0 - 1)))
                    .flatten()
                    .map(|line| line.received_at);
                let Some(stamp) = inputs.clock.format(
                    inputs.timestamps,
                    timestamp_format,
                    line.received_at,
                    previous,
                ) else {
                    continue;
                };
                let run = TextRun {
                    len: stamp.len(),
                    font: inputs.font.clone(),
                    color: palette.dim_foreground,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = text_system.shape_line(
                    stamp.into(),
                    inputs.font_size,
                    &[run],
                    Some(cell_width),
                );
                gutter.push(PaintRow {
                    origin: point(bounds.left() + PADDING_LEFT, bounds.top() + px(row.y)),
                    shaped,
                });
            }
        }

        // The cursor, on its row if that row is on screen.
        let mut cursor = Vec::new();
        let mut cursor_glyph = None;
        if let Some(state) = screen.and_then(|screen| screen.cursor)
            && let Some(row) = visible.iter().find(|row| row.line == state.line)
        {
            let origin = point(x_of(state.column), bounds.top() + px(row.y));
            let cell = Bounds::new(origin, size(cell_width, row_height));
            let color = palette.cursor;
            let focused = inputs.focus.is_focused(window);
            match state.shape {
                // Without the keyboard, the cursor is an outline whatever its shape.
                _ if !focused => cursor.push(outline(cell, color, BorderStyle::Solid)),
                CursorShape::HollowBlock => {
                    cursor.push(outline(cell, color, BorderStyle::Solid));
                }
                CursorShape::Underline => cursor.push(fill(
                    Bounds::new(
                        point(origin.x, origin.y + row_height - CURSOR_THICKNESS),
                        size(cell_width, CURSOR_THICKNESS),
                    ),
                    color,
                )),
                CursorShape::Beam => cursor.push(fill(
                    Bounds::new(origin, size(CURSOR_THICKNESS, row_height)),
                    color,
                )),
                CursorShape::Block => {
                    cursor.push(fill(cell, color));
                    let under = frame
                        .lines
                        .get(&state.line)
                        .and_then(|line| line.text.chars().nth(state.column))
                        .filter(|c| !c.is_whitespace());
                    if let Some(c) = under {
                        let text = c.to_string();
                        let run = TextRun {
                            len: text.len(),
                            font: inputs.font.clone(),
                            color: palette.background,
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shaped = window.text_system().shape_line(
                            text.into(),
                            inputs.font_size,
                            &[run],
                            Some(cell_width),
                        );
                        cursor_glyph = Some(PaintRow { origin, shaped });
                    }
                }
            }
        }

        sample.lines_fetched = frame.fetched;
        sample.prepaint = started.elapsed();
        inputs.stats.borrow_mut().record(sample);

        TerminalLayout {
            hitbox,
            text_clip: Bounds::from_corners(
                point(text_left, bounds.top()),
                point(bounds.right(), bounds.bottom()),
            ),
            background: palette.background,
            row_height,
            rects,
            rows,
            gutter,
            cursor,
            cursor_glyph,
            hit_map: Rc::new(HitMap {
                bounds,
                column_origin,
                cell_width: f32::from(cell_width),
                row_height: f32::from(row_height),
                rows: hit_rows,
            }),
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        layout: &mut TerminalLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let started = Instant::now();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, layout.background));
            window.with_content_mask(
                Some(ContentMask {
                    bounds: layout.text_clip,
                }),
                |window| {
                    for (rect, color) in &layout.rects {
                        window.paint_quad(fill(*rect, *color));
                    }
                    for row in &layout.rows {
                        row.shaped
                            .paint(
                                row.origin,
                                layout.row_height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            )
                            .ok();
                    }
                    // The cursor covers its cell's glyph, and a block cursor then shows
                    // the glyph again in the background color.
                    for quad in layout.cursor.drain(..) {
                        window.paint_quad(quad);
                    }
                    if let Some(glyph) = &layout.cursor_glyph {
                        glyph
                            .shaped
                            .paint(
                                glyph.origin,
                                layout.row_height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            )
                            .ok();
                    }
                },
            );
            for row in &layout.gutter {
                row.shaped
                    .paint(
                        row.origin,
                        layout.row_height,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
            }
        });
        window.set_cursor_style(CursorStyle::IBeam, &layout.hitbox);
        self.register_mouse_handlers(bounds, layout, window);
        self.inputs
            .stats
            .borrow_mut()
            .record_paint(started.elapsed());
    }
}

impl TerminalElement {
    fn register_mouse_handlers(
        &self,
        bounds: Bounds<Pixels>,
        layout: &TerminalLayout,
        window: &mut Window,
    ) {
        let row_height = layout.row_height;
        let wrap = self.inputs.wrap && self.inputs.screen.is_none();

        window.on_mouse_event({
            let view = self.inputs.view.clone();
            let hitbox = layout.hitbox.clone();
            let hit_map = layout.hit_map.clone();
            let focus = self.inputs.focus.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble()
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                window.focus(&focus, cx);
                let Some(hit) = hit_map.hit(event.position) else {
                    return;
                };
                window.capture_pointer(hitbox.id);
                let (clicks, extend) = (event.click_count, event.modifiers.shift);
                view.update(cx, |view, cx| view.mouse_down(hit, clicks, extend, cx));
                cx.stop_propagation();
            }
        });

        window.on_mouse_event({
            let view = self.inputs.view.clone();
            let hit_map = layout.hit_map.clone();
            let scroll = self.inputs.scroll.clone();
            move |event: &MouseMoveEvent, phase, _window, cx| {
                if !phase.bubble()
                    || event.pressed_button != Some(MouseButton::Left)
                    || !view.read(cx).is_selecting()
                {
                    return;
                }
                // Dragging past an edge scrolls, a row per move event.
                if event.position.y < bounds.top() {
                    scroll.scroll_by(f32::from(row_height));
                    cx.notify(view.entity_id());
                } else if event.position.y > bounds.bottom() {
                    scroll.scroll_by(-f32::from(row_height));
                    cx.notify(view.entity_id());
                }
                if let Some(hit) = hit_map.hit(event.position) {
                    view.update(cx, |view, cx| view.mouse_drag(hit, cx));
                }
            }
        });

        window.on_mouse_event({
            let view = self.inputs.view.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase.bubble()
                    && event.button == MouseButton::Left
                    && view.read(cx).is_selecting()
                {
                    view.update(cx, |view, cx| view.mouse_up(cx));
                }
            }
        });

        window.on_mouse_event({
            let view = self.inputs.view.clone();
            let hitbox = layout.hitbox.clone();
            let scroll = self.inputs.scroll.clone();
            move |event: &ScrollWheelEvent, phase, window, cx| {
                if !phase.bubble() || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let delta = event.delta.pixel_delta(row_height);
                let (mut dx, mut dy) = (f32::from(delta.x), f32::from(delta.y));
                // A plain wheel with shift held scrolls sideways.
                if event.modifiers.shift && dx == 0.0 {
                    (dx, dy) = (dy, 0.0);
                }
                if dy != 0.0 {
                    scroll.scroll_by(dy);
                }
                if dx != 0.0 && !wrap {
                    scroll.scroll_horizontally(-dx);
                }
                cx.notify(view.entity_id());
                cx.stop_propagation();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::{Color, Direction, StyleFlags, StyleRun};

    use super::*;

    fn line(text: &str, runs: Vec<StyleRun>) -> StyledLine {
        StyledLine {
            id: LineId(0),
            text: text.into(),
            runs,
            direction: Direction::Rx,
            received_at: Instant::now(),
            raw: 0..0,
            complete: true,
        }
    }

    fn styled(len: usize, fg: Color) -> StyleRun {
        StyleRun {
            len,
            style: Style {
                fg,
                bg: Color::Default,
                flags: StyleFlags::NONE,
            },
        }
    }

    #[test]
    fn runs_are_normalized_to_cover_the_text() {
        let exact = line(
            "abcdef",
            vec![styled(2, Color::Ansi(1)), styled(4, Color::Ansi(2))],
        );
        let runs = normalized_runs(&exact);
        assert_eq!(
            runs.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            [0..2, 2..6]
        );
        // Short runs: the rest in the default style.
        let short = line("abcdef", vec![styled(2, Color::Ansi(1))]);
        let runs = normalized_runs(&short);
        assert_eq!(runs[1], (2..6, Style::default()));
        // Long runs are cut; a run ending inside a char is moved to its end.
        let long = line(
            "aé",
            vec![styled(2, Color::Ansi(1)), styled(9, Color::Ansi(2))],
        );
        let runs = normalized_runs(&long);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 0..3);
        assert!(normalized_runs(&line("", vec![])).is_empty());
    }

    #[test]
    fn entry_keys_change_with_text_runs_and_width() {
        let a = line("abc", vec![styled(3, Color::Default)]);
        let mut b = a.clone();
        assert_eq!(EntryKey::of(&a, 0), EntryKey::of(&b, 0));
        b.text.push('d');
        assert_ne!(EntryKey::of(&a, 0), EntryKey::of(&b, 0));
        let mut c = a.clone();
        c.runs[0].style.fg = Color::Ansi(1);
        assert_ne!(EntryKey::of(&a, 0), EntryKey::of(&c, 0));
        assert_ne!(EntryKey::of(&a, 0), EntryKey::of(&a, 80));
        // The arrival time is not part of what is drawn.
        let mut d = a.clone();
        d.received_at += std::time::Duration::from_secs(1);
        assert_eq!(EntryKey::of(&a, 0), EntryKey::of(&d, 0));
    }

    /// `text` with `glyphs` glyph bytes at the end, in the runs the store produces.
    fn with_glyph_run(text: &str, glyphs: &str) -> StyledLine {
        let runs = vec![
            StyleRun {
                len: text.len(),
                style: Style::default(),
            },
            StyleRun {
                len: glyphs.len(),
                style: serialist_core::ansi::CONTROL_STYLE,
            },
        ];
        line(&format!("{text}{glyphs}"), runs)
    }

    #[test]
    fn a_control_glyph_run_is_painted_in_the_dim_color() {
        let palette = TerminalPalette::default();
        let styled = with_glyph_run("abc", "\u{240d}\u{240a}");
        let runs = resolved_runs(&styled, &palette);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].0, 0..3);
        assert_eq!(runs[0].1.foreground, palette.foreground);
        assert_eq!(runs[1].0, 3..9, "both glyphs are one run");
        assert_eq!(runs[1].1.foreground, palette.dim_foreground);
        assert_ne!(runs[0].1.foreground, runs[1].1.foreground);

        // What the shaper hands the text system for that run: its length and its color.
        let font = Font::default();
        let run = text_run(&font, 6, &runs[1].1);
        assert_eq!((run.len, run.color), (6, palette.dim_foreground));
        assert!(run.underline.is_none() && run.strikethrough.is_none());
        assert_eq!(run.font.weight, FontWeight::NORMAL, "not bold");

        // It is the palette's dim color, so a theme decides it.
        let themed = TerminalPalette {
            dim_foreground: Hsla::from(rgb(0x808040)),
            ..palette
        };
        let runs = resolved_runs(&styled, &themed);
        assert_eq!(runs[1].1.foreground, themed.dim_foreground);
    }

    #[test]
    fn glyph_runs_do_not_disturb_the_lines_own_styles() {
        let palette = TerminalPalette::default();
        let red = StyleRun {
            len: 3,
            style: Style {
                fg: Color::Ansi(1),
                ..Style::default()
            },
        };
        let glyph = StyleRun {
            len: 3,
            style: serialist_core::ansi::CONTROL_STYLE,
        };
        let styled = line("red\u{241b}", vec![red, glyph]);
        let runs = resolved_runs(&styled, &palette);
        assert_eq!(runs[0].1.foreground, palette.indexed(1));
        assert_eq!(runs[1].1.foreground, palette.dim_foreground);
        // A changed run (a different key) reshapes the line: the flag is part of it.
        let plain = line("red\u{241b}", vec![red, StyleRun { len: 3, ..red }]);
        assert_ne!(EntryKey::of(&styled, 0), EntryKey::of(&plain, 0));
    }

    #[test]
    fn hit_testing_maps_positions_to_carets_and_cells() {
        let map = HitMap {
            bounds: Bounds::new(point(px(0.), px(100.)), size(px(400.), px(60.))),
            column_origin: px(20.),
            cell_width: 10.0,
            row_height: 20.0,
            rows: vec![
                HitRow {
                    line: LineId(7),
                    y: -5.0,
                    start: 0,
                    len: 30,
                    last: true,
                },
                HitRow {
                    line: LineId(8),
                    y: 15.0,
                    start: 0,
                    len: 30,
                    last: false,
                },
                HitRow {
                    line: LineId(8),
                    y: 35.0,
                    start: 30,
                    len: 4,
                    last: true,
                },
            ],
        };
        let at = |x: f32, y: f32| map.hit(point(px(x), px(y))).unwrap();
        // Middle of cell 3 of line 7: caret rounds to 4, the cell is 3.
        let hit = at(20.0 + 36.0, 105.0);
        assert_eq!(hit.caret, SelectionPoint::new(LineId(7), 4));
        assert_eq!(hit.cell, SelectionPoint::new(LineId(7), 3));
        // Second wrap row of line 8 starts at column 30.
        let hit = at(20.0 + 11.0, 140.0);
        assert_eq!(hit.caret, SelectionPoint::new(LineId(8), 31));
        // Past the end of a short row: its end.
        let hit = at(390.0, 140.0);
        assert_eq!(hit.caret, SelectionPoint::new(LineId(8), 34));
        assert_eq!(hit.cell, SelectionPoint::new(LineId(8), 33));
        // Left of the text, above and below the rows.
        assert_eq!(at(0.0, 120.0).caret, SelectionPoint::new(LineId(8), 0));
        assert_eq!(at(50.0, 0.0).caret, SelectionPoint::new(LineId(7), 0));
        assert_eq!(at(50.0, 500.0).caret, SelectionPoint::new(LineId(8), 34));
    }
}
