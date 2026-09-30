//! Scroll state shared by the terminal view, its element and the scrollbar.
//!
//! It is a cloneable handle around shared state, the same shape as GPUI's own
//! `ScrollHandle`, because gpui-kit's scrollbar sets offsets through a `&self` trait
//! method with no context to update an entity through. Input (wheel, keys, the
//! scrollbar, search reveals) only records a request; the element resolves requests
//! against the current lines and width in prepaint, where it has the wrap counts, so
//! nothing here walks the scrollback.

use std::cell::RefCell;
use std::rc::Rc;

use serialist_core::LineId;

use crate::prelude::*;
use crate::terminal::layout::{
    self, RowCounter, ScrollPosition, ScrollbarPosition, Span, Viewport,
};

/// Taller content than this is reported to the scrollbar scaled down; only the thumb's
/// proportions matter, and f32 pixels lose precision past about 2^24.
const MAX_REPORTED_CONTENT: f32 = 1.0e7;

#[derive(Clone, Debug)]
pub struct ScrollState {
    /// Top of the viewport, as of the last layout.
    pub position: ScrollPosition,
    /// Keep the newest line in view as lines arrive.
    pub follow_tail: bool,
    /// Pixels scrolled right, unwrapped mode only.
    pub horizontal: f32,
    pending_delta: f32,
    pending_horizontal: f32,
    pending_line: Option<f64>,
    pending_reveal: Option<LineId>,
    // What the last layout saw, for key actions and the scrollbar.
    pub bounds: Bounds<Pixels>,
    pub viewport: Viewport,
    pub span: Span,
    pub scrollbar: ScrollbarPosition,
    pub max_horizontal: f32,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            position: ScrollPosition::at(LineId::ZERO),
            follow_tail: true,
            horizontal: 0.0,
            pending_delta: 0.0,
            pending_horizontal: 0.0,
            pending_line: None,
            pending_reveal: None,
            bounds: Bounds::default(),
            viewport: Viewport {
                height: 0.0,
                row_height: 0.0,
            },
            span: Span::new(LineId::ZERO, LineId::ZERO),
            scrollbar: ScrollbarPosition {
                fraction: 0.0,
                visible: 1.0,
                scrollable: false,
            },
            max_horizontal: 0.0,
        }
    }
}

#[derive(Clone, Default)]
pub struct TerminalScrollHandle(Rc<RefCell<ScrollState>>);

impl TerminalScrollHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of the state as of the last layout plus pending requests.
    pub fn state(&self) -> ScrollState {
        self.0.borrow().clone()
    }

    pub fn is_following(&self) -> bool {
        self.0.borrow().follow_tail
    }

    pub fn position(&self) -> ScrollPosition {
        self.0.borrow().position
    }

    /// Scroll by pixels; positive moves up toward older lines, which stops following.
    pub fn scroll_by(&self, delta: f32) {
        let mut state = self.0.borrow_mut();
        state.pending_delta += delta;
        if delta > 0.0 {
            state.follow_tail = false;
        }
    }

    /// Scroll right by pixels (negative scrolls left); ignored while wrapping.
    pub fn scroll_horizontally(&self, delta: f32) {
        self.0.borrow_mut().pending_horizontal += delta;
    }

    /// A page up, keeping one row of the old page for context.
    pub fn page_up(&self) {
        let page = self.page();
        self.scroll_by(page);
    }

    pub fn page_down(&self) {
        let page = self.page();
        self.scroll_by(-page);
    }

    fn page(&self) -> f32 {
        let state = self.0.borrow();
        (state.viewport.height - state.viewport.row_height).max(state.viewport.row_height)
    }

    pub fn scroll_to_top(&self) {
        let mut state = self.0.borrow_mut();
        state.follow_tail = false;
        state.pending_delta = 0.0;
        state.pending_line = Some(0.0);
    }

    /// Jump to the newest line and follow it again.
    pub fn scroll_to_bottom(&self) {
        let mut state = self.0.borrow_mut();
        state.follow_tail = true;
        state.pending_delta = 0.0;
        state.pending_line = None;
        state.pending_reveal = None;
    }

    /// Bring `line` into view if it is not already, as search does for its active
    /// match. Stops following.
    pub fn reveal(&self, line: LineId) {
        let mut state = self.0.borrow_mut();
        state.pending_reveal = Some(line);
    }

    /// Prepaint: apply pending requests against the lines and viewport of this frame
    /// and return where the viewport's top is. Costs what the layout functions it
    /// calls cost: O(rows moved + rows visible).
    pub(crate) fn resolve(
        &self,
        span: Span,
        viewport: Viewport,
        rows: &mut dyn RowCounter,
    ) -> ScrollPosition {
        let mut state = self.0.borrow_mut();
        let mut reached_bottom_counts = false;
        if let Some(line) = state.pending_line.take() {
            state.position = layout::position_at_line(line, span, viewport, rows);
            reached_bottom_counts = true;
        }
        if let Some(line) = state.pending_reveal.take()
            && span.contains(line)
        {
            let current = layout::clamp(state.position, span, viewport, rows);
            let shown = layout::visible_rows(current, span, viewport, rows);
            // Fully visible means some row of it is on screen with room below.
            let on_screen = shown.iter().any(|row| {
                row.line == line && row.y >= 0.0 && row.y + viewport.row_height <= viewport.height
            });
            if !on_screen {
                // Put it a third of the way down, so what led up to it shows too.
                let at = ScrollPosition::at(line);
                state.position = layout::scroll_by(at, viewport.height / 3.0, span, viewport, rows);
            } else {
                state.position = current;
            }
            // Either way, hold it there while new lines arrive.
            state.follow_tail = false;
        }
        let delta = std::mem::take(&mut state.pending_delta);
        if delta != 0.0 {
            let base = if state.follow_tail {
                layout::bottom(span, viewport, rows)
            } else {
                state.position
            };
            if delta > 0.0 {
                state.follow_tail = false;
            } else {
                reached_bottom_counts = true;
            }
            state.position = layout::scroll_by(base, delta, span, viewport, rows);
        }
        state.position = if state.follow_tail {
            layout::bottom(span, viewport, rows)
        } else {
            layout::clamp(state.position, span, viewport, rows)
        };
        let at_top = state.position == layout::top(span);
        let at_bottom = layout::is_at_bottom(state.position, span, viewport, rows);
        // Scrolling down onto the bottom follows again; so does content that fits.
        if at_bottom && (reached_bottom_counts || at_top) {
            state.follow_tail = true;
        }
        let position_rows = if span.contains(state.position.line) {
            rows.rows(state.position.line)
        } else {
            1
        };
        state.scrollbar = layout::scrollbar_position(
            state.position,
            position_rows,
            span,
            viewport,
            at_top,
            at_bottom,
        );
        state.span = span;
        state.viewport = viewport;
        state.position
    }

    /// Prepaint, unwrapped mode: apply horizontal requests now that the widest visible
    /// line is known. Returns the horizontal offset.
    pub(crate) fn resolve_horizontal(&self, content_width: f32, visible_width: f32) -> f32 {
        let mut state = self.0.borrow_mut();
        let max = (content_width - visible_width).max(0.0);
        let pending = std::mem::take(&mut state.pending_horizontal);
        state.max_horizontal = max;
        state.horizontal = (state.horizontal + pending).clamp(0.0, max);
        state.horizontal
    }

    /// Wrapped mode has nothing to scroll sideways.
    pub(crate) fn reset_horizontal(&self) {
        let mut state = self.0.borrow_mut();
        state.pending_horizontal = 0.0;
        state.horizontal = 0.0;
        state.max_horizontal = 0.0;
    }

    pub(crate) fn set_bounds(&self, bounds: Bounds<Pixels>) {
        self.0.borrow_mut().bounds = bounds;
    }

    /// Forget the position, as when the source is swapped for another whose ids mean
    /// something else.
    pub(crate) fn reset(&self) {
        let mut state = self.0.borrow_mut();
        let bounds = state.bounds;
        *state = ScrollState {
            bounds,
            ..ScrollState::default()
        };
    }

    fn content_height(&self) -> f32 {
        let state = self.0.borrow();
        let viewport = state.bounds.size.height;
        if !state.scrollbar.scrollable || state.scrollbar.visible <= 0.0 {
            return f32::from(viewport);
        }
        (f32::from(viewport) / state.scrollbar.visible as f32).min(MAX_REPORTED_CONTENT)
    }
}

impl ScrollbarHandle for TerminalScrollHandle {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.0.borrow().bounds
    }

    fn offset(&self) -> Point<Pixels> {
        let content = self.content_height();
        let state = self.0.borrow();
        let extent = (content - f32::from(state.bounds.size.height)).max(0.0);
        point(
            px(-state.horizontal),
            px(-(state.scrollbar.fraction as f32) * extent),
        )
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let content = self.content_height();
        let mut state = self.0.borrow_mut();
        let extent = content - f32::from(state.bounds.size.height);
        if extent <= 0.0 {
            return;
        }
        let fraction = (-f32::from(offset.y) / extent).clamp(0.0, 1.0) as f64;
        state.pending_delta = 0.0;
        if fraction >= 0.9999 {
            state.follow_tail = true;
            state.pending_line = None;
        } else {
            state.follow_tail = false;
            state.pending_line = Some(layout::line_for_scrollbar(
                fraction,
                state.span,
                state.viewport,
            ));
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        let width = self.0.borrow().bounds.size.width;
        size(width, px(self.content_height()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::layout::OneRowEach;

    const ROW: f32 = 20.0;

    fn setup() -> (TerminalScrollHandle, Span, Viewport) {
        let handle = TerminalScrollHandle::new();
        handle.set_bounds(Bounds {
            origin: Point::default(),
            size: size(px(400.), px(200.)),
        });
        let span = Span::new(LineId(0), LineId(1000));
        let viewport = Viewport {
            height: 200.0,
            row_height: ROW,
        };
        (handle, span, viewport)
    }

    #[test]
    fn follows_until_scrolled_up_and_again_after_jumping_down() {
        let (handle, span, viewport) = setup();
        let rows = &mut OneRowEach;
        assert!(handle.is_following());
        assert_eq!(handle.resolve(span, viewport, rows).line, LineId(990));
        // New lines: still at the bottom.
        let grown = Span::new(LineId(0), LineId(1010));
        assert_eq!(handle.resolve(grown, viewport, rows).line, LineId(1000));

        handle.scroll_by(3.0 * ROW);
        assert!(!handle.is_following());
        assert_eq!(handle.resolve(grown, viewport, rows).line, LineId(997));
        // More lines arrive; the view stays put.
        let grown = Span::new(LineId(0), LineId(1100));
        assert_eq!(handle.resolve(grown, viewport, rows).line, LineId(997));
        assert!(!handle.is_following());

        handle.scroll_to_bottom();
        assert_eq!(handle.resolve(grown, viewport, rows).line, LineId(1090));
        assert!(handle.is_following());
    }

    #[test]
    fn scrolling_back_down_to_the_bottom_follows_again() {
        let (handle, span, viewport) = setup();
        let rows = &mut OneRowEach;
        handle.resolve(span, viewport, rows);
        handle.scroll_by(2.0 * ROW);
        handle.resolve(span, viewport, rows);
        handle.scroll_by(-ROW);
        handle.resolve(span, viewport, rows);
        assert!(!handle.is_following(), "not there yet");
        handle.scroll_by(-5.0 * ROW);
        handle.resolve(span, viewport, rows);
        assert!(handle.is_following());
    }

    #[test]
    fn pages_home_and_the_scrollbar() {
        let (handle, span, viewport) = setup();
        let rows = &mut OneRowEach;
        handle.resolve(span, viewport, rows);
        handle.page_up();
        assert_eq!(handle.resolve(span, viewport, rows).line, LineId(981));
        handle.page_down();
        handle.resolve(span, viewport, rows);
        assert!(handle.is_following());
        handle.scroll_to_top();
        assert_eq!(handle.resolve(span, viewport, rows).line, LineId(0));
        assert!(!handle.is_following());
        assert_eq!(handle.offset().y, px(0.));

        // Drag the thumb to the middle, then to the end.
        let extent = f32::from(handle.content_size().height) - 200.0;
        handle.set_offset(point(px(0.), px(-extent / 2.0)));
        assert_eq!(handle.resolve(span, viewport, rows).line, LineId(495));
        assert!((f32::from(handle.offset().y) + extent / 2.0).abs() < 1.0);
        handle.set_offset(point(px(0.), px(-extent)));
        handle.resolve(span, viewport, rows);
        assert!(handle.is_following());
    }

    #[test]
    fn reveal_brings_a_line_into_view_only_when_needed() {
        let (handle, span, viewport) = setup();
        let rows = &mut OneRowEach;
        handle.scroll_to_top();
        handle.resolve(span, viewport, rows);
        handle.reveal(LineId(5));
        assert_eq!(
            handle.resolve(span, viewport, rows).line,
            LineId(0),
            "visible"
        );
        handle.reveal(LineId(500));
        let at = handle.resolve(span, viewport, rows);
        assert!(at.line < LineId(500) && at.line.offset(10) > LineId(500));
        assert!(!handle.is_following());
    }

    #[test]
    fn short_content_always_follows() {
        let (handle, _, viewport) = setup();
        let rows = &mut OneRowEach;
        let short = Span::new(LineId(0), LineId(3));
        handle.scroll_by(40.0);
        handle.resolve(short, viewport, rows);
        assert!(handle.is_following());
        assert!(!handle.state().scrollbar.scrollable);
    }

    #[test]
    fn horizontal_offsets_clamp_to_the_widest_line() {
        let (handle, _, _) = setup();
        handle.scroll_horizontally(500.0);
        assert_eq!(handle.resolve_horizontal(700.0, 400.0), 300.0);
        handle.scroll_horizontally(-1000.0);
        assert_eq!(handle.resolve_horizontal(700.0, 400.0), 0.0);
        handle.scroll_horizontally(50.0);
        assert_eq!(handle.resolve_horizontal(300.0, 400.0), 0.0, "fits");
    }
}
