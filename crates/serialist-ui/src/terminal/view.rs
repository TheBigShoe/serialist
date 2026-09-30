//! The terminal view: the entity that owns what the terminal shows and how, and renders
//! the search bar, the [`TerminalElement`], the scrollbar and the overlays.
//!
//! The view never copies lines. It holds `Arc`s to the sources and draws whatever they
//! say is retained; pause is a frozen end id, hex view is a second source, selection is
//! a pair of line ids and columns, and search results are line ids and byte ranges.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serialist_core::{LineId, LineSource, SearchMatch, Searcher};

use crate::actions::{
    self, CycleTimestamps, DismissSearch, JumpToBottom, PageDown, PageUp, ScrollToTop, Search,
    SearchNext, SearchPrevious, SelectAll, ToggleFrameStats, ToggleHexView, ToggleWrap, context,
};
use crate::config::Config;
use crate::fonts::TerminalFont;
use crate::prelude::*;
use crate::terminal::element::{
    CellMetrics, Highlights, Hit, ShapeCache, TerminalElement, TerminalInputs,
};
use crate::terminal::layout::Span;
use crate::terminal::palette::TerminalPalette;
use crate::terminal::scroll::TerminalScrollHandle;
use crate::terminal::search::{MAX_MATCHES, SearchResults};
use crate::terminal::selection::{Selection, SelectionMode, SelectionPoint, word_at};
use crate::terminal::stats::{FrameStats, FrameSummary};
use crate::terminal::timestamps::{Clock, TimestampMode, TimestampModeExt};

/// Which source is on screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    #[default]
    Text,
    /// The hex source, when one is set.
    Hex,
}

/// Marks kept at most: the newest command responses.
pub const MAX_MARKS: usize = 256;

/// Where each source stood when the view was paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Frozen {
    text: LineId,
    hex: Option<LineId>,
}

struct InFlight {
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

struct SearchBar {
    input: Entity<InputState>,
    open: bool,
    results: SearchResults,
    in_flight: Option<InFlight>,
    generation: u64,
    /// End of the displayed lines the matches account for; lines from here on (and the
    /// last one before, which may still have been arriving) are not searched yet.
    covered: LineId,
    /// Lines arrived while a search was in flight; search them when it finishes.
    stale: bool,
}

/// What a search run is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchKind {
    /// A new query: everything displayed, newest first, then reveal the active match.
    New,
    /// The same query over everything displayed again, keeping the active match.
    Rescan,
    /// The same query over the lines from `from` on, merged into the matches.
    Extend { from: LineId },
}

pub struct TerminalView {
    text_source: Arc<dyn LineSource>,
    text_searcher: Option<Arc<dyn Searcher>>,
    hex_source: Option<Arc<dyn LineSource>>,
    hex_searcher: Option<Arc<dyn Searcher>>,
    display: DisplayMode,
    frozen: Option<Frozen>,
    scroll: TerminalScrollHandle,
    selection: Option<Selection>,
    selecting: bool,
    wrap: bool,
    timestamps: TimestampMode,
    palette: Rc<TerminalPalette>,
    font: TerminalFont,
    /// Bumped whenever what the element draws from changes wholesale, dropping its
    /// caches.
    generation: u64,
    clock: Clock,
    cache: Rc<RefCell<ShapeCache>>,
    stats: Rc<RefCell<FrameStats>>,
    show_frame_stats: bool,
    search: SearchBar,
    /// Inline mode: the key context is `TerminalInline` and keys go to the port.
    inline: bool,

    /// Highlighted text lines, such as a saved command's matched response. Sorted by
    /// line, then start; drawn like search matches whether or not search is open.
    marks: Arc<Vec<SearchMatch>>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl TerminalView {
    pub fn new(source: Arc<dyn LineSource>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search (regex)"));
        let input_events =
            cx.subscribe_in(&input, window, |this, input, event, _, cx| match event {
                InputEvent::Change => {
                    let query = input.read(cx).value().to_string();
                    this.run_search(query, cx);
                }
                InputEvent::PressEnter { shift: false, .. } => this.search_next(cx),
                InputEvent::PressEnter { shift: true, .. } => this.search_previous(cx),
                InputEvent::Focus | InputEvent::Blur => {}
            });
        let clock = Clock::local(source.epoch());
        // The font and colors follow the configuration, when the app has one.
        let (font, palette) = match cx.try_global::<Config>() {
            Some(config) => (config.terminal_font().clone(), config.palette().clone()),
            None => (TerminalFont::default(), TerminalPalette::default()),
        };
        let config_changes = cx.observe_global::<Config>(|this, cx| {
            let config = cx.global::<Config>();
            let (font, palette) = (config.terminal_font().clone(), config.palette().clone());
            this.set_font(font, cx);
            // A reload about something else keeps the shaped lines.
            if *this.palette != palette {
                this.set_palette(palette, cx);
            }
        });
        Self {
            text_source: source,
            text_searcher: None,
            hex_source: None,
            hex_searcher: None,
            display: DisplayMode::Text,
            frozen: None,
            scroll: TerminalScrollHandle::new(),
            selection: None,
            selecting: false,
            wrap: false,
            timestamps: TimestampMode::Off,
            palette: Rc::new(palette),
            font,
            generation: 1,
            clock,
            cache: Rc::default(),
            stats: Rc::default(),
            show_frame_stats: false,
            search: SearchBar {
                input,
                open: false,
                results: SearchResults::default(),
                in_flight: None,
                generation: 0,
                covered: LineId::ZERO,
                stale: false,
            },
            inline: false,

            marks: Arc::default(),
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![input_events, config_changes],
        }
    }

    // --- Sources -------------------------------------------------------------------

    /// The source on screen: the text source, or the hex source in hex view.
    pub fn source(&self) -> &Arc<dyn LineSource> {
        match (self.display, &self.hex_source) {
            (DisplayMode::Hex, Some(hex)) => hex,
            _ => &self.text_source,
        }
    }

    fn searcher(&self) -> Option<&Arc<dyn Searcher>> {
        match self.display {
            DisplayMode::Text => self.text_searcher.as_ref(),
            DisplayMode::Hex => self.hex_searcher.as_ref(),
        }
    }

    /// Swap in a newer view of the same stream, such as the store's next snapshot. Line
    /// ids keep their meaning across it, so the scroll position, the selection, a pause
    /// and the search results all stand; call [`Self::lines_appended`] after to repaint
    /// and bring an open search up to date. Hex sources are optional as in
    /// [`Self::set_hex_source`]; losing the hex source in hex view goes back to text.
    pub fn update_sources(
        &mut self,
        text: Arc<dyn LineSource>,
        text_searcher: Option<Arc<dyn Searcher>>,
        hex: Option<Arc<dyn LineSource>>,
        hex_searcher: Option<Arc<dyn Searcher>>,
        cx: &mut Context<Self>,
    ) {
        self.text_source = text;
        self.text_searcher = text_searcher;
        let lost_hex = hex.is_none() && self.display == DisplayMode::Hex;
        self.hex_source = hex;
        self.hex_searcher = hex_searcher;
        if lost_hex {
            self.display = DisplayMode::Text;
            self.source_changed(cx);
        }
    }

    /// Show a different text source, such as a new session's store.
    pub fn set_source(&mut self, source: Arc<dyn LineSource>, cx: &mut Context<Self>) {
        self.clock = Clock::local(source.epoch());
        self.text_source = source;
        if self.display == DisplayMode::Text {
            self.source_changed(cx);
        }
    }

    pub fn set_searcher(&mut self, searcher: Option<Arc<dyn Searcher>>, cx: &mut Context<Self>) {
        self.text_searcher = searcher;
        cx.notify();
    }

    /// The hex dump of the same session, which the store provides.
    pub fn set_hex_source(
        &mut self,
        source: Option<Arc<dyn LineSource>>,
        searcher: Option<Arc<dyn Searcher>>,
        cx: &mut Context<Self>,
    ) {
        self.hex_source = source;
        self.hex_searcher = searcher;
        if self.hex_source.is_none() && self.display == DisplayMode::Hex {
            self.display = DisplayMode::Text;
            self.source_changed(cx);
        } else if self.display == DisplayMode::Hex {
            self.source_changed(cx);
        }
        cx.notify();
    }

    pub fn display_mode(&self) -> DisplayMode {
        self.display
    }

    /// Show text or hex. Hex needs a hex source; without one this stays on text.
    pub fn set_display_mode(&mut self, mode: DisplayMode, cx: &mut Context<Self>) {
        if mode != self.display {
            self.toggle_hex(cx);
        }
    }

    pub fn has_hex_source(&self) -> bool {
        self.hex_source.is_some()
    }

    /// Swap between text and hex. Ids mean different things in each, so the selection
    /// goes, the view follows the tail again, and an open search reruns.
    pub fn toggle_hex(&mut self, cx: &mut Context<Self>) {
        if self.hex_source.is_none() {
            return;
        }
        self.display = match self.display {
            DisplayMode::Text => DisplayMode::Hex,
            DisplayMode::Hex => DisplayMode::Text,
        };
        self.source_changed(cx);
    }

    fn source_changed(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self.selection = None;
        self.selecting = false;
        self.scroll.reset();
        if self.search.open {
            let query = self.search.results.query.clone();
            self.run_search(query, cx);
        } else {
            self.cancel_search();
            self.search.results.clear();
        }
        cx.notify();
    }

    /// The source grew: text lines `changed` are new, or were rewritten (the line that
    /// was still arriving). Repaints; following the tail happens in layout, against the
    /// source's new end. An open search takes in the new lines (see
    /// [`Self::refresh_search`]).
    pub fn lines_appended(&mut self, changed: std::ops::Range<LineId>, cx: &mut Context<Self>) {
        if self.search.open && !changed.is_empty() {
            let from = (self.display == DisplayMode::Text).then_some(changed.start);
            self.refresh_search(from, cx);
        }
        cx.notify();
    }

    // --- Display settings ------------------------------------------------------------

    pub fn palette(&self) -> &TerminalPalette {
        &self.palette
    }

    /// Draw with `palette`, dropping every shaped line (their runs carry colors), even
    /// when it equals the current one.
    pub fn set_palette(&mut self, palette: TerminalPalette, cx: &mut Context<Self>) {
        self.palette = Rc::new(palette);
        self.generation += 1;
        cx.notify();
    }

    /// The font the terminal draws with.
    pub fn font(&self) -> &TerminalFont {
        &self.font
    }

    /// Draw with `font` from the next frame on. The element measures the new grid and
    /// drops every shaped line when the font, size or line height differ from the last
    /// frame's.
    pub fn set_font(&mut self, font: TerminalFont, cx: &mut Context<Self>) {
        if self.font != font {
            self.font = font;
            cx.notify();
        }
    }

    pub fn wrap(&self) -> bool {
        self.wrap
    }

    pub fn set_wrap(&mut self, wrap: bool, cx: &mut Context<Self>) {
        if self.wrap != wrap {
            self.wrap = wrap;
            cx.notify();
        }
    }

    pub fn toggle_wrap(&mut self, cx: &mut Context<Self>) {
        self.set_wrap(!self.wrap, cx);
    }

    pub fn timestamps(&self) -> TimestampMode {
        self.timestamps
    }

    pub fn set_timestamps(&mut self, mode: TimestampMode, cx: &mut Context<Self>) {
        self.timestamps = mode;
        cx.notify();
    }

    pub fn cycle_timestamps(&mut self, cx: &mut Context<Self>) {
        self.set_timestamps(self.timestamps.next(), cx);
    }

    pub fn shows_frame_stats(&self) -> bool {
        self.show_frame_stats
    }

    pub fn toggle_frame_stats(&mut self, cx: &mut Context<Self>) {
        self.show_frame_stats = !self.show_frame_stats;
        cx.notify();
    }

    /// Costs of the last 60 frames the element drew.
    pub fn frame_summary(&self) -> FrameSummary {
        self.stats.borrow().summary()
    }

    pub fn frame_stats(&self) -> std::cell::Ref<'_, FrameStats> {
        self.stats.borrow()
    }

    /// How many shaped lines the element's cache holds.
    pub fn shaped_lines_cached(&self) -> usize {
        self.cache.borrow().shaped_len()
    }

    /// The cell grid of the last frame, once one was drawn.
    pub fn cell_metrics(&self) -> Option<CellMetrics> {
        self.cache.borrow().metrics()
    }

    // --- Inline mode and marks ---------------------------------------------------------

    pub fn is_inline(&self) -> bool {
        self.inline
    }

    /// Take keys for the port (`TerminalInline`) or for the terminal's own bindings
    /// (`Terminal`).
    pub fn set_inline(&mut self, inline: bool, cx: &mut Context<Self>) {
        if self.inline != inline {
            self.inline = inline;
            cx.notify();
        }
    }

    /// The highlighted text ranges, sorted.
    pub fn marks(&self) -> &Arc<Vec<SearchMatch>> {
        &self.marks
    }

    /// Highlight `range` of text line `line`, keeping the newest [`MAX_MARKS`].
    pub fn add_mark(&mut self, mark: SearchMatch, cx: &mut Context<Self>) {
        let marks = Arc::make_mut(&mut self.marks);
        let at =
            marks.partition_point(|m| (m.line, m.range.start) <= (mark.line, mark.range.start));
        marks.insert(at, mark);
        if marks.len() > MAX_MARKS {
            marks.remove(0);
        }
        cx.notify();
    }

    /// Search matches and marks together, for the element. The active search match
    /// stays active.
    fn highlights(&self) -> Highlights {
        let search = self.search.open.then_some(&self.search.results);
        // Mark line ids are text line ids.
        let marks =
            (self.display == DisplayMode::Text && !self.marks.is_empty()).then_some(&self.marks);
        match (search, marks) {
            (None, None) => Highlights::default(),
            (Some(results), None) => Highlights {
                matches: results.matches.clone(),
                active: results.active,
            },
            (None, Some(marks)) => Highlights {
                matches: marks.clone(),
                active: None,
            },
            (Some(results), Some(marks)) => {
                let mut merged = Vec::with_capacity(results.matches.len() + marks.len());
                let mut active = None;
                let (mut a, mut b) = (0, 0);
                let key = |m: &SearchMatch| (m.line, m.range.start);
                while a < results.matches.len() || b < marks.len() {
                    let take_search = match (results.matches.get(a), marks.get(b)) {
                        (Some(found), Some(mark)) => key(found) <= key(mark),
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if take_search {
                        if results.active == Some(a) {
                            active = Some(merged.len());
                        }
                        merged.push(results.matches[a].clone());
                        a += 1;
                    } else {
                        merged.push(marks[b].clone());
                        b += 1;
                    }
                }
                Highlights {
                    matches: Arc::new(merged),
                    active,
                }
            }
        }
    }

    // --- Scrolling -------------------------------------------------------------------

    pub fn scroll_handle(&self) -> &TerminalScrollHandle {
        &self.scroll
    }

    pub fn is_following_tail(&self) -> bool {
        self.scroll.is_following()
    }

    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    pub fn scroll_to_top(&mut self, cx: &mut Context<Self>) {
        self.scroll.scroll_to_top();
        cx.notify();
    }

    pub fn page_up(&mut self, cx: &mut Context<Self>) {
        self.scroll.page_up();
        cx.notify();
    }

    pub fn page_down(&mut self, cx: &mut Context<Self>) {
        self.scroll.page_down();
        cx.notify();
    }

    /// Scroll by pixels; positive moves toward older lines.
    pub fn scroll_by(&mut self, delta: Pixels, cx: &mut Context<Self>) {
        self.scroll.scroll_by(f32::from(delta));
        cx.notify();
    }

    // --- Pause -----------------------------------------------------------------------

    /// Freeze the view at the lines retained now. The sources keep growing; the view
    /// draws `first_line()..frozen_end` and follows the frozen end, so nothing on screen
    /// moves except when eviction takes the oldest lines. Returns false if already
    /// paused.
    pub fn pause(&mut self, cx: &mut Context<Self>) -> bool {
        if self.frozen.is_some() {
            return false;
        }
        self.frozen = Some(Frozen {
            text: self.text_source.end(),
            hex: self.hex_source.as_ref().map(|hex| hex.end()),
        });
        // Matches found past the frozen end are not on display any more.
        let span = self.displayed_span();
        self.search.results.retain_lines(span.range());
        self.search.covered = self.search.covered.min(span.end);
        cx.notify();
        true
    }

    /// Show the live tail again. Returns false if not paused.
    pub fn resume(&mut self, cx: &mut Context<Self>) -> bool {
        if self.frozen.take().is_none() {
            return false;
        }
        self.scroll.scroll_to_bottom();
        // What arrived while paused is on display again.
        self.refresh_search(None, cx);
        cx.notify();
        true
    }

    pub fn is_paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// The end the element draws to while paused, for the source on screen.
    pub fn frozen_end(&self) -> Option<LineId> {
        let frozen = self.frozen?;
        match (self.display, &self.hex_source) {
            (DisplayMode::Hex, Some(_)) => frozen.hex,
            _ => Some(frozen.text),
        }
    }

    /// Lines appended to the text source since the pause.
    pub fn lines_since_pause(&self) -> Option<u64> {
        let frozen = self.frozen?;
        Some(self.text_source.end().0.saturating_sub(frozen.text.0))
    }

    /// The lines on display: retained, cut at the frozen end while paused. An export
    /// reads these from the source on a background thread.
    pub fn displayed_span(&self) -> Span {
        let source = self.source();
        let end = source.end();
        Span::new(
            source.first_line(),
            self.frozen_end().map_or(end, |frozen| frozen.min(end)),
        )
    }

    // --- Selection -------------------------------------------------------------------

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    pub fn set_selection(&mut self, selection: Option<Selection>, cx: &mut Context<Self>) {
        self.selection = selection;
        cx.notify();
    }

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.selection = Selection::all(self.displayed_span());
        cx.notify();
    }

    /// The selected text, read now. For a large selection prefer [`Self::copy`], which
    /// reads on the background executor.
    pub fn selection_text(&self) -> Option<String> {
        let selection = self.selection?;
        Some(selection.text(self.source().as_ref(), self.displayed_span()))
    }

    /// Copy the selection to the clipboard. The text is gathered on the background
    /// executor, so copying a million selected lines does not stall the UI.
    pub fn copy(&mut self, cx: &mut Context<Self>) -> Task<()> {
        let Some(selection) = self.selection else {
            return Task::ready(());
        };
        let source = self.source().clone();
        let span = self.displayed_span();
        cx.spawn(async move |_, cx| {
            let text = cx
                .background_spawn(async move { selection.text(source.as_ref(), span) })
                .await;
            cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
        })
    }

    pub(crate) fn is_selecting(&self) -> bool {
        self.selecting
    }

    fn word_unit(&self, point: SelectionPoint) -> std::ops::Range<SelectionPoint> {
        let text = self
            .source()
            .line(point.line)
            .map(|line| line.text)
            .unwrap_or_default();
        let word = word_at(&text, point.column);
        SelectionPoint::new(point.line, word.start)..SelectionPoint::new(point.line, word.end)
    }

    fn line_unit(line: LineId) -> std::ops::Range<SelectionPoint> {
        SelectionPoint::new(line, 0)..SelectionPoint::end_of(line)
    }

    pub(crate) fn mouse_down(
        &mut self,
        hit: Hit,
        clicks: usize,
        extend: bool,
        cx: &mut Context<Self>,
    ) {
        self.selecting = true;
        self.selection = match clicks {
            0 | 1 => match self.selection {
                Some(mut selection) if extend => {
                    selection.extend_to(hit.caret, |point| point..point);
                    Some(selection)
                }
                _ => Some(Selection::new(hit.caret, hit.caret)),
            },
            2 => Some(Selection::unit(
                self.word_unit(hit.cell),
                SelectionMode::Word,
            )),
            _ => Some(Selection::unit(
                Self::line_unit(hit.cell.line),
                SelectionMode::Line,
            )),
        };
        cx.notify();
    }

    pub(crate) fn mouse_drag(&mut self, hit: Hit, cx: &mut Context<Self>) {
        let Some(mode) = self.selection.map(|selection| selection.mode) else {
            return;
        };
        let (point, unit) = match mode {
            SelectionMode::Character => (hit.caret, hit.caret..hit.caret),
            SelectionMode::Word => (hit.cell, self.word_unit(hit.cell)),
            SelectionMode::Line => (hit.cell, Self::line_unit(hit.cell.line)),
        };
        if let Some(selection) = &mut self.selection {
            let before = *selection;
            selection.extend_to(point, |_| unit);
            if *selection != before {
                cx.notify();
            }
        }
    }

    pub(crate) fn mouse_up(&mut self, cx: &mut Context<Self>) {
        self.selecting = false;
        // A click without a drag places nothing worth keeping.
        if self.selection.is_some_and(|selection| selection.is_empty()) {
            self.selection = None;
        }
        cx.notify();
    }

    // --- Search ----------------------------------------------------------------------

    pub fn search_results(&self) -> &SearchResults {
        &self.search.results
    }

    pub fn is_search_open(&self) -> bool {
        self.search.open
    }

    pub fn search_input(&self) -> &Entity<InputState> {
        &self.search.input
    }

    /// Open the search bar and focus its field.
    pub fn deploy_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let reopening = !self.search.open;
        self.search.open = true;
        self.search
            .input
            .update(cx, |input, cx| input.focus(window, cx));
        // The field keeps its text while closed; bring its matches back.
        let query = self.search.input.read(cx).value().to_string();
        if reopening && !query.is_empty() {
            self.run_search(query, cx);
        }
        cx.notify();
    }

    /// Close the search bar: cancel a search in flight, drop the highlights, and give
    /// focus back to the terminal.
    pub fn dismiss_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search.open = false;
        self.cancel_search();
        self.search.results.clear();
        self.focus_handle.focus(window, cx);
        cx.notify();
    }

    fn cancel_search(&mut self) {
        if let Some(in_flight) = self.search.in_flight.take() {
            in_flight.cancel.store(true, Ordering::Relaxed);
        }
        self.search.results.pending = false;
        self.search.stale = false;
    }

    /// The cancel flag of the search in flight, if any.
    pub fn search_cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        self.search
            .in_flight
            .as_ref()
            .map(|in_flight| in_flight.cancel.clone())
    }

    /// Search for `query` on the background executor, newest lines first. A search
    /// still running for an older query is cancelled through its flag, so typing never
    /// waits on a search.
    pub fn run_search(&mut self, query: String, cx: &mut Context<Self>) {
        self.cancel_search();
        self.search.generation += 1;
        self.search.results.clear();
        self.search.results.query = query;
        self.start_search(SearchKind::New, cx);
    }

    /// Bring the open search up to date with the source: drop matches that left the
    /// display (evicted, or past a pause), then search what arrived since the matches
    /// were found, from `changed_from` (or the last line covered, which may have grown)
    /// on, and merge. Runs at most once per call and never cancels a search in flight:
    /// if one is running, this one waits for it, so a stream faster than the search
    /// cannot starve it. A search of new lines that hits [`MAX_MATCHES`] rescans
    /// everything displayed instead, to keep the newest matches.
    pub fn refresh_search(&mut self, changed_from: Option<LineId>, cx: &mut Context<Self>) {
        let search = &mut self.search;
        if !search.open || search.results.query.is_empty() || search.results.error.is_some() {
            return;
        }
        if search.in_flight.is_some() {
            search.stale = true;
            return;
        }
        let span = self.displayed_span();
        self.search.results.retain_lines(span.range());
        let covered = self.search.covered.min(span.end);
        if covered >= span.end && self.frozen.is_some() {
            // Paused, and everything on display was searched.
            return;
        }
        let mut from = LineId(covered.0.saturating_sub(1));
        if let Some(changed) = changed_from {
            from = from.min(changed);
        }
        let from = from.max(span.first);
        if from >= span.end {
            return;
        }
        self.search.generation += 1;
        self.start_search(SearchKind::Extend { from }, cx);
    }

    /// Run the current query on the background executor.
    fn start_search(&mut self, kind: SearchKind, cx: &mut Context<Self>) {
        let query = self.search.results.query.clone();
        let span = self.displayed_span();
        let Some(searcher) = self.searcher().cloned() else {
            cx.notify();
            return;
        };
        if query.is_empty() || span.is_empty() {
            self.search.covered = span.end;
            cx.notify();
            return;
        }
        let generation = self.search.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (from, backward) = match kind {
            SearchKind::New | SearchKind::Rescan => (LineId(span.end.0 - 1), true),
            SearchKind::Extend { from } => (from, false),
        };
        let task = cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move {
                    let found = searcher
                        .search(&query, from, backward, MAX_MATCHES, &flag)
                        .map(|mut found| {
                            // A forward search runs to the source's end; a paused view
                            // shows less.
                            found.retain(|m| m.line < span.end);
                            found
                        });
                    (found, flag.load(Ordering::Relaxed))
                })
                .await;
            this.update(cx, |view, cx| {
                view.search_finished(generation, kind, span, found, cx)
            })
            .ok();
        });
        self.search.results.pending = true;
        self.search.in_flight = Some(InFlight {
            cancel,
            _task: task,
        });
        cx.notify();
    }

    fn search_finished(
        &mut self,
        generation: u64,
        kind: SearchKind,
        span: Span,
        (found, cancelled): (Result<Vec<SearchMatch>, String>, bool),
        cx: &mut Context<Self>,
    ) {
        if generation != self.search.generation || cancelled {
            return;
        }
        self.search.in_flight = None;
        match (kind, found) {
            (_, Err(error)) => self.search.results.fail(error),
            (SearchKind::New, Ok(matches)) => {
                let top = self.scroll.position().line;
                self.search.results.finish(matches, top);
                self.search.covered = span.end;
                self.reveal_active();
            }
            (SearchKind::Rescan, Ok(matches)) => {
                self.search.results.rescanned(matches);
                self.search.covered = span.end;
            }
            (SearchKind::Extend { .. }, Ok(found)) if found.len() >= MAX_MATCHES => {
                self.search.generation += 1;
                self.start_search(SearchKind::Rescan, cx);
                return;
            }
            (SearchKind::Extend { from }, Ok(found)) => {
                self.search.results.extend(from, found);
                self.search.covered = span.end;
            }
        }
        if std::mem::take(&mut self.search.stale) {
            self.refresh_search(None, cx);
        }
        cx.notify();
    }

    fn reveal_active(&self) {
        if let Some(found) = self.search.results.active_match() {
            self.scroll.reveal(found.line);
        }
    }

    pub fn search_next(&mut self, cx: &mut Context<Self>) {
        if self.search.results.select_next().is_some() {
            self.reveal_active();
            cx.notify();
        }
    }

    pub fn search_previous(&mut self, cx: &mut Context<Self>) {
        if self.search.results.select_previous().is_some() {
            self.reveal_active();
            cx.notify();
        }
    }

    // --- Actions ---------------------------------------------------------------------

    fn copy_action(&mut self, _: &actions::Copy, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.is_some() {
            self.copy(cx).detach();
        } else {
            cx.propagate();
        }
    }

    fn select_all_action(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select_all(cx);
    }

    fn search_action(&mut self, _: &Search, window: &mut Window, cx: &mut Context<Self>) {
        self.deploy_search(window, cx);
    }

    fn dismiss_search_action(
        &mut self,
        _: &DismissSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.search.open {
            self.dismiss_search(window, cx);
        } else {
            cx.propagate();
        }
    }

    fn search_next_action(&mut self, _: &SearchNext, _: &mut Window, cx: &mut Context<Self>) {
        self.search_next(cx);
    }

    fn search_previous_action(
        &mut self,
        _: &SearchPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_previous(cx);
    }

    fn toggle_wrap_action(&mut self, _: &ToggleWrap, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_wrap(cx);
    }

    fn cycle_timestamps_action(
        &mut self,
        _: &CycleTimestamps,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cycle_timestamps(cx);
    }

    fn toggle_hex_action(&mut self, _: &ToggleHexView, _: &mut Window, cx: &mut Context<Self>) {
        self.toggle_hex(cx);
    }

    fn toggle_frame_stats_action(
        &mut self,
        _: &ToggleFrameStats,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_frame_stats(cx);
    }

    fn page_up_action(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.page_up(cx);
    }

    fn page_down_action(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.page_down(cx);
    }

    fn scroll_to_top_action(&mut self, _: &ScrollToTop, _: &mut Window, cx: &mut Context<Self>) {
        self.scroll_to_top(cx);
    }

    fn jump_to_bottom_action(&mut self, _: &JumpToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_bottom(cx);
    }

    // --- Rendering -------------------------------------------------------------------

    fn render_search_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let toolbar = Config::toolbar_background(cx);
        let theme = cx.theme();
        let results = &self.search.results;
        let label = results.count_label();
        let label_color = if results.error.is_some() {
            theme.danger
        } else {
            theme.muted_foreground
        };
        h_flex()
            .key_context(context::TERMINAL_SEARCH)
            .on_action(cx.listener(Self::dismiss_search_action))
            .flex_none()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .when_some(toolbar, |bar, background| bar.bg(background))
            .child(
                div().flex_1().child(
                    Input::new(&self.search.input)
                        .id("terminal-search-input")
                        .small(),
                ),
            )
            .child(
                div()
                    .id("terminal-search-count")
                    .min_w(px(72.))
                    .text_xs()
                    .text_color(label_color)
                    .child(label),
            )
            .child(
                Button::new("terminal-search-previous")
                    .label("↑")
                    .tooltip("Previous match (shift-enter)")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.search_previous(cx))),
            )
            .child(
                Button::new("terminal-search-next")
                    .label("↓")
                    .tooltip("Next match (enter)")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.search_next(cx))),
            )
            .child(
                Button::new("terminal-search-close")
                    .label("✕")
                    .tooltip("Close (escape)")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| this.dismiss_search(window, cx))),
            )
    }

    fn render_frame_stats(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let summary = self.frame_summary();
        v_flex()
            .id("terminal-frame-stats")
            .absolute()
            .top_2()
            .right_4()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(self.palette.background)
            .border_1()
            .border_color(theme.border)
            .font_family(theme.mono_font_family.clone())
            .text_xs()
            .text_color(theme.muted_foreground)
            .children(summary.lines())
    }
}

impl Render for TerminalView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search_open = self.search.open;
        let highlights = self.highlights();
        let element = TerminalElement::new(TerminalInputs {
            view: cx.entity(),
            source: self.source().clone(),
            frozen_end: self.frozen_end(),
            scroll: self.scroll.clone(),
            cache: self.cache.clone(),
            stats: self.stats.clone(),
            palette: self.palette.clone(),
            generation: self.generation,
            font: self.font.font.clone(),
            font_size: self.font.size,
            line_height: self.font.line_height,
            wrap: self.wrap,
            timestamps: self.timestamps,
            clock: self.clock,
            selection: self.selection,
            highlights,
            focus: self.focus_handle.clone(),
        });
        let following = self.scroll.is_following();
        let search_bar = search_open.then(|| self.render_search_bar(cx));
        let frame_stats = self.show_frame_stats.then(|| self.render_frame_stats(cx));

        v_flex()
            .id("terminal")
            .key_context(if self.inline {
                context::TERMINAL_INLINE
            } else {
                context::TERMINAL
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::select_all_action))
            .on_action(cx.listener(Self::search_action))
            .on_action(cx.listener(Self::search_next_action))
            .on_action(cx.listener(Self::search_previous_action))
            .on_action(cx.listener(Self::toggle_wrap_action))
            .on_action(cx.listener(Self::cycle_timestamps_action))
            .on_action(cx.listener(Self::toggle_hex_action))
            .on_action(cx.listener(Self::toggle_frame_stats_action))
            .on_action(cx.listener(Self::page_up_action))
            .on_action(cx.listener(Self::page_down_action))
            .on_action(cx.listener(Self::scroll_to_top_action))
            .on_action(cx.listener(Self::jump_to_bottom_action))
            .size_full()
            .children(search_bar)
            .child(
                div()
                    .id("terminal-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(element)
                    .child(Scrollbar::vertical(&self.scroll).id("terminal-scrollbar"))
                    .children(frame_stats)
                    .when(!following, |area| {
                        area.child(
                            div().absolute().bottom_3().right_6().child(
                                Button::new("terminal-jump-to-bottom")
                                    .label(if self.frozen.is_some() {
                                        "Jump to paused end"
                                    } else {
                                        "Jump to bottom"
                                    })
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_to_bottom(cx);
                                    })),
                            ),
                        )
                    }),
            )
    }
}
