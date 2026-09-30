//! Headless tests of the terminal view and element: real frames drawn in a test
//! window (GPUI's no-op text system shapes every glyph one em wide), real mouse and
//! key events, the in-memory doubles as the source.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serialist_core::{LineId, LineSource, SearchMatch, Searcher};

use crate::actions::keys;
use crate::prelude::*;
use crate::terminal::double::{HexLines, MemoryLines, SyntheticLines};
use crate::terminal::element::{CellMetrics, PADDING_LEFT};
use crate::terminal::layout::Viewport;
use crate::terminal::{DisplayMode, FrameSample, TerminalView, TimestampMode};
use crate::test_support::open_test_window;

/// gpui-kit's `Input` binds select-all to the platform primary modifier.
const INPUT_SELECT_ALL: &str = if cfg!(target_os = "macos") {
    "cmd-a"
} else {
    "ctrl-a"
};

fn open(
    cx: &mut TestAppContext,
    source: Arc<dyn LineSource>,
) -> (AnyWindowHandle, Entity<TerminalView>) {
    let (window, view) =
        open_test_window(cx, move |window, cx| TerminalView::new(source, window, cx));
    draw(cx, window);
    (window, view)
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn lines(texts: impl IntoIterator<Item = String>) -> Arc<MemoryLines> {
    let source = Arc::new(MemoryLines::new());
    for text in texts {
        source.push(&text);
    }
    source
}

fn numbered(count: usize) -> Arc<MemoryLines> {
    lines((0..count).map(|i| format!("line {i}")))
}

struct Geometry {
    bounds: Bounds<Pixels>,
    metrics: CellMetrics,
}

impl Geometry {
    fn of(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> Self {
        view.read_with(cx, |view, _| Self {
            bounds: view.scroll_handle().state().bounds,
            metrics: view.cell_metrics().expect("a frame was drawn"),
        })
    }

    /// The middle of cell `column` on screen row `row`, with no gutter.
    fn cell(&self, column: usize, row: usize) -> Point<Pixels> {
        point(
            self.bounds.left() + PADDING_LEFT + self.metrics.cell_width * (column as f32 + 0.5),
            self.bounds.top() + self.metrics.row_height * (row as f32 + 0.5),
        )
    }

    /// The boundary before cell `column` on screen row `row`.
    fn caret(&self, column: usize, row: usize) -> Point<Pixels> {
        point(
            self.bounds.left() + PADDING_LEFT + self.metrics.cell_width * column as f32 + px(1.),
            self.bounds.top() + self.metrics.row_height * (row as f32 + 0.5),
        )
    }

    fn rows(&self) -> usize {
        let viewport = Viewport {
            height: f32::from(self.bounds.size.height),
            row_height: f32::from(self.metrics.row_height),
        };
        viewport.row_capacity()
    }
}

fn send(cx: &mut TestAppContext, window: AnyWindowHandle, event: PlatformInput) {
    cx.update_window(window, |_, window, cx| {
        window.dispatch_event(event, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn mouse_down(at: Point<Pixels>, click_count: usize) -> PlatformInput {
    MouseDownEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::default(),
        click_count,
        first_mouse: false,
    }
    .to_platform_input()
}

fn mouse_move(at: Point<Pixels>) -> PlatformInput {
    MouseMoveEvent {
        position: at,
        pressed_button: Some(MouseButton::Left),
        modifiers: Modifiers::default(),
    }
    .to_platform_input()
}

fn mouse_up(at: Point<Pixels>, click_count: usize) -> PlatformInput {
    MouseUpEvent {
        button: MouseButton::Left,
        position: at,
        modifiers: Modifiers::default(),
        click_count,
    }
    .to_platform_input()
}

fn drag(cx: &mut TestAppContext, window: AnyWindowHandle, from: Point<Pixels>, to: Point<Pixels>) {
    send(cx, window, mouse_down(from, 1));
    send(cx, window, mouse_move(to));
    send(cx, window, mouse_up(to, 1));
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
}

fn focus(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<TerminalView>) {
    cx.update_window(window, |_, window, cx| {
        let handle = view.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

fn top_line(cx: &mut TestAppContext, view: &Entity<TerminalView>) -> LineId {
    view.read_with(cx, |view, _| view.scroll_handle().position().line)
}

#[gpui_test]
fn follows_the_tail_and_fetches_only_the_screen(cx: &mut TestAppContext) {
    let source = numbered(1000);
    let (window, view) = open(cx, source.clone());
    let geometry = Geometry::of(cx, &view);
    let rows = geometry.rows();

    let sample = view.read_with(cx, |view, _| view.frame_stats().last().unwrap());
    assert!(
        sample.lines_fetched <= rows,
        "fetched {} for {rows} rows",
        sample.lines_fetched
    );
    let top = top_line(cx, &view);
    assert!(top.0 > 1000 - rows as u64 && top.0 < 1000);
    assert!(view.read_with(cx, |view, _| view.is_following_tail()));

    // New lines arrive: the view follows them.
    for i in 1000..1010 {
        source.push(&format!("line {i}"));
    }
    view.update(cx, |view, cx| view.lines_appended(cx));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), top.offset(10));

    // Scrolled up, it stays put while more arrive.
    view.update(cx, |view, cx| {
        view.scroll_by(geometry.metrics.row_height * 5.0, cx)
    });
    draw(cx, window);
    let held = top_line(cx, &view);
    assert_eq!(held, top.offset(5));
    assert!(!view.read_with(cx, |view, _| view.is_following_tail()));
    source.push("line 1010");
    draw(cx, window);
    assert_eq!(top_line(cx, &view), held);

    // Jump to bottom follows again.
    focus(cx, window, &view);
    press(cx, window, "end");
    assert!(view.read_with(cx, |view, _| view.is_following_tail()));
    assert_eq!(top_line(cx, &view), top.offset(11));
}

#[gpui_test]
fn page_keys_home_and_end(cx: &mut TestAppContext) {
    let (window, view) = open(cx, numbered(1000));
    focus(cx, window, &view);
    let bottom = top_line(cx, &view);
    press(cx, window, "pageup");
    let paged = top_line(cx, &view);
    let rows = Geometry::of(cx, &view).rows() as u64;
    assert!(bottom.0 - paged.0 >= rows - 4 && bottom.0 - paged.0 <= rows);
    press(cx, window, "home");
    assert_eq!(top_line(cx, &view), LineId(0));
    press(cx, window, "pagedown");
    assert!(top_line(cx, &view) > LineId(0));
    press(cx, window, "end");
    assert_eq!(top_line(cx, &view), bottom);
    assert!(view.read_with(cx, |view, _| view.is_following_tail()));
}

#[gpui_test]
fn wheel_scrolling_detaches_and_reattaches(cx: &mut TestAppContext) {
    let (window, view) = open(cx, numbered(1000));
    let geometry = Geometry::of(cx, &view);
    let at = geometry.cell(3, 3);
    let wheel = |dy: f32| {
        ScrollWheelEvent {
            position: at,
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            modifiers: Modifiers::default(),
            touch_phase: TouchPhase::Moved,
        }
        .to_platform_input()
    };
    let bottom = top_line(cx, &view);
    let row = f32::from(geometry.metrics.row_height);
    send(cx, window, wheel(row * 10.0));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), LineId(bottom.0 - 10));
    assert!(!view.read_with(cx, |view, _| view.is_following_tail()));
    send(cx, window, wheel(-row * 4.0));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), LineId(bottom.0 - 6));
    send(cx, window, wheel(-row * 100.0));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), bottom);
    assert!(view.read_with(cx, |view, _| view.is_following_tail()));
}

#[gpui_test]
fn eviction_moves_an_evicted_top_to_the_first_retained_line(cx: &mut TestAppContext) {
    let source = numbered(1000);
    let (window, view) = open(cx, source.clone());
    view.update(cx, |view, cx| view.scroll_to_top(cx));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), LineId(0));
    let row = Geometry::of(cx, &view).metrics.row_height;
    view.update(cx, |view, cx| view.scroll_by(-row * 20.0, cx));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), LineId(20));
    // The store drops its oldest 50 lines: line 20 is gone.
    source.evict(50);
    draw(cx, window);
    assert_eq!(top_line(cx, &view), LineId(50));
    assert!(!view.read_with(cx, |view, _| view.is_following_tail()));
}

#[gpui_test]
fn drag_selects_across_lines_and_copy_puts_it_on_the_clipboard(cx: &mut TestAppContext) {
    let source = lines(["hello world".into(), "second line".into(), "third".into()]);
    let (window, view) = open(cx, source);
    let geometry = Geometry::of(cx, &view);

    drag(cx, window, geometry.caret(6, 0), geometry.caret(6, 1));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("world\nsecond".into())
    );

    // Reversed: the same text.
    drag(cx, window, geometry.caret(3, 2), geometry.caret(6, 0));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("world\nsecond line\nthi".into())
    );

    // The selection is painted.
    let selection_color = view.read_with(cx, |view, _| view.palette().selection);
    let painted = cx
        .update_window(window, |_, window, _| {
            window
                .painted_quads()
                .iter()
                .filter(|quad| quad.background.as_solid() == Some(selection_color))
                .count()
        })
        .unwrap();
    assert_eq!(painted, 3, "one rectangle per selected row");

    press(cx, window, keys::COPY);
    cx.run_until_parked();
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("world\nsecond line\nthi".into())
    );

    // A plain click clears it.
    send(cx, window, mouse_down(geometry.cell(1, 1), 1));
    send(cx, window, mouse_up(geometry.cell(1, 1), 1));
    assert_eq!(view.read_with(cx, |view, _| view.selection()), None);
}

#[gpui_test]
fn double_click_selects_a_word_and_triple_click_a_line(cx: &mut TestAppContext) {
    let source = lines(["status=ok path=/dev/cu.usb0 rate".into(), "next".into()]);
    let (window, view) = open(cx, source);
    let geometry = Geometry::of(cx, &view);

    send(cx, window, mouse_down(geometry.cell(18, 0), 1));
    send(cx, window, mouse_down(geometry.cell(18, 0), 2));
    send(cx, window, mouse_up(geometry.cell(18, 0), 2));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("/dev/cu.usb0".into())
    );

    send(cx, window, mouse_down(geometry.cell(2, 0), 3));
    send(cx, window, mouse_up(geometry.cell(2, 0), 3));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("status=ok path=/dev/cu.usb0 rate".into())
    );
}

#[gpui_test]
fn select_all_takes_every_retained_line(cx: &mut TestAppContext) {
    let source = numbered(5);
    let (window, view) = open(cx, source.clone());
    source.evict(2);
    focus(cx, window, &view);
    press(cx, window, keys::SELECT_ALL);
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("line 2\nline 3\nline 4".into())
    );
}

#[gpui_test]
fn wrapping_splits_long_lines_into_rows_and_hit_tests_them(cx: &mut TestAppContext) {
    let long: String = (0..500)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    let source = lines(["short".into(), long.clone(), "after".into()]);
    let (window, view) = open(cx, source);
    view.update(cx, |view, cx| view.set_wrap(true, cx));
    draw(cx, window);
    let geometry = Geometry::of(cx, &view);
    let columns = {
        let width =
            geometry.bounds.size.width - PADDING_LEFT - crate::terminal::element::PADDING_RIGHT;
        (f32::from(width) / f32::from(geometry.metrics.cell_width)).floor() as usize
    };
    assert!(columns < 500, "the window is narrower than the line");
    let wrapped_rows = 500usize.div_ceil(columns);

    // "after" sits below every wrap row of the long line.
    send(
        cx,
        window,
        mouse_down(geometry.cell(0, 1 + wrapped_rows), 2),
    );
    send(cx, window, mouse_up(geometry.cell(0, 1 + wrapped_rows), 2));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("after".into())
    );

    // A drag from the second wrap row's start to the third's selects a row's worth.
    drag(cx, window, geometry.caret(0, 2), geometry.caret(0, 3));
    let selected = view.read_with(cx, |view, _| view.selection_text()).unwrap();
    assert_eq!(selected, long[columns..2 * columns]);

    // Unwrapped again: one row per line.
    view.update(cx, |view, cx| view.set_wrap(false, cx));
    draw(cx, window);
    send(cx, window, mouse_down(geometry.cell(0, 2), 2));
    send(cx, window, mouse_up(geometry.cell(0, 2), 2));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("after".into())
    );
}

#[gpui_test]
fn the_timestamp_gutter_shifts_the_text(cx: &mut TestAppContext) {
    let source = lines(["abc".into()]);
    let (window, view) = open(cx, source);
    focus(cx, window, &view);
    press(cx, window, "alt-t");
    assert_eq!(
        view.read_with(cx, |view, _| view.timestamps()),
        TimestampMode::Absolute
    );
    let geometry = Geometry::of(cx, &view);
    // Double-click where the text used to start: now the gutter, which clamps to
    // the line's first cell.
    let gutter = geometry.metrics.cell_width * (TimestampMode::Absolute.width() + 1) as f32;
    send(cx, window, mouse_down(geometry.cell(0, 0), 2));
    send(cx, window, mouse_up(geometry.cell(0, 0), 2));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("abc".into())
    );
    let past_gutter = point(geometry.caret(2, 0).x + gutter, geometry.caret(2, 0).y);
    drag(cx, window, geometry.caret(0, 0), past_gutter);
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("ab".into())
    );
    for mode in [
        TimestampMode::Relative,
        TimestampMode::Delta,
        TimestampMode::Off,
    ] {
        press(cx, window, "alt-t");
        assert_eq!(view.read_with(cx, |view, _| view.timestamps()), mode);
    }
}

/// Delegates to a real searcher and counts calls, recording whether each saw its
/// cancel flag set.
struct CountingSearcher {
    inner: Arc<MemoryLines>,
    calls: AtomicUsize,
    saw_cancel: AtomicBool,
}

impl Searcher for CountingSearcher {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancel.load(Ordering::SeqCst) {
            self.saw_cancel.store(true, Ordering::SeqCst);
        }
        self.inner.search(pattern, from, backward, limit, cancel)
    }
}

fn searchable(
    cx: &mut TestAppContext,
) -> (AnyWindowHandle, Entity<TerminalView>, Arc<CountingSearcher>) {
    let source = lines((0..30).map(|i| {
        if i % 10 == 3 {
            format!("{i} error: timeout")
        } else {
            format!("{i} ok")
        }
    }));
    let searcher = Arc::new(CountingSearcher {
        inner: source.clone(),
        calls: AtomicUsize::new(0),
        saw_cancel: AtomicBool::new(false),
    });
    let (window, view) = open(cx, source);
    view.update(cx, |view, cx| {
        view.set_searcher(Some(searcher.clone() as Arc<dyn Searcher>), cx)
    });
    focus(cx, window, &view);
    (window, view, searcher)
}

fn painted_with(cx: &mut TestAppContext, window: AnyWindowHandle, color: Hsla) -> usize {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .painted_quads()
            .iter()
            .filter(|quad| quad.background.as_solid() == Some(color))
            .count()
    })
    .unwrap()
}

#[gpui_test]
fn search_runs_in_the_background_and_highlights_the_active_match(cx: &mut TestAppContext) {
    let (window, view, searcher) = searchable(cx);
    press(cx, window, keys::SEARCH);
    assert!(view.read_with(cx, |view, _| view.is_search_open()));
    cx.update_window(window, |_, window, cx| window.input("error", cx))
        .unwrap();
    // Typing started a search but did not run it on the main thread.
    assert_eq!(searcher.calls.load(Ordering::SeqCst), 0);
    assert!(view.read_with(cx, |view, _| view.search_results().pending));
    cx.run_until_parked();
    assert_eq!(searcher.calls.load(Ordering::SeqCst), 1);

    let (matches, active, label) = view.read_with(cx, |view, _| {
        let results = view.search_results();
        (
            results.matches.clone(),
            results.active,
            results.count_label(),
        )
    });
    let lines: Vec<u64> = matches.iter().map(|m| m.line.0).collect();
    assert_eq!(lines, [3, 13, 23]);
    // The whole scrollback fits, so the first match below the top is the first.
    assert_eq!(active, Some(0));
    assert_eq!(label, "1/3");

    let palette = view.read_with(cx, |view, _| view.palette().clone());
    assert_eq!(painted_with(cx, window, palette.active_match), 1);
    assert_eq!(painted_with(cx, window, palette.search_match), 2);

    // Enter steps forward, shift-enter back, wrapping around.
    press(cx, window, "enter");
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().active),
        Some(1)
    );
    press(cx, window, "shift-enter");
    press(cx, window, "shift-enter");
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().active),
        Some(2)
    );
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().count_label()),
        "3/3"
    );

    // An invalid pattern reports the searcher's error.
    cx.update_window(window, |_, window, cx| window.input("(", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().count_label()),
        "Invalid pattern"
    );

    // Escape closes the bar and drops the highlights.
    press(cx, window, "escape");
    assert!(!view.read_with(cx, |view, _| view.is_search_open()));
    assert_eq!(painted_with(cx, window, palette.active_match), 0);
    assert_eq!(painted_with(cx, window, palette.search_match), 0);
}

#[gpui_test]
fn escape_cancels_an_in_flight_search(cx: &mut TestAppContext) {
    let (window, view, searcher) = searchable(cx);
    press(cx, window, keys::SEARCH);
    cx.update_window(window, |_, window, cx| window.input("timeout", cx))
        .unwrap();
    let flag = view
        .read_with(cx, |view, _| view.search_cancel_flag())
        .expect("a search in flight");
    assert!(!flag.load(Ordering::SeqCst));

    press(cx, window, "escape");
    assert!(flag.load(Ordering::SeqCst), "escape set the cancel flag");
    assert!(view.read_with(cx, |view, _| view.search_cancel_flag().is_none()));
    cx.run_until_parked();
    // Either the task was dropped before it ran, or the searcher saw the flag.
    let calls = searcher.calls.load(Ordering::SeqCst);
    assert!(calls == 0 || searcher.saw_cancel.load(Ordering::SeqCst));
    view.read_with(cx, |view, _| {
        assert!(!view.is_search_open());
        assert!(view.search_results().matches.is_empty());
        assert!(!view.search_results().pending);
    });

    // Typing a new query cancels the old one the same way. Select-all here is the search
    // input's own chord (the terminal's select-all is ctrl-shift-a off macOS).
    press(cx, window, keys::SEARCH);
    press(cx, window, INPUT_SELECT_ALL);
    cx.update_window(window, |_, window, cx| window.input("err", cx))
        .unwrap();
    let first = view
        .read_with(cx, |view, _| view.search_cancel_flag())
        .unwrap();
    cx.update_window(window, |_, window, cx| window.input("or", cx))
        .unwrap();
    assert!(first.load(Ordering::SeqCst));
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().query.clone()),
        "error"
    );
    assert_eq!(
        view.read_with(cx, |view, _| view.search_results().matches.len()),
        3
    );
}

#[gpui_test]
fn search_reveals_a_match_scrolled_out_of_view(cx: &mut TestAppContext) {
    let source = lines((0..5000).map(|i| {
        if i == 1234 {
            "needle".to_string()
        } else {
            format!("hay {i}")
        }
    }));
    let (window, view) = open(cx, source.clone());
    view.update(cx, |view, cx| {
        view.set_searcher(Some(source.clone() as Arc<dyn Searcher>), cx)
    });
    view.update(cx, |view, cx| view.run_search("needle".into(), cx));
    cx.run_until_parked();
    draw(cx, window);
    let top = top_line(cx, &view);
    let rows = Geometry::of(cx, &view).rows() as u64;
    assert!(top.0 <= 1234 && 1234 < top.0 + rows, "top {top:?}");
    assert!(!view.read_with(cx, |view, _| view.is_following_tail()));
}

#[gpui_test]
fn hex_view_swaps_the_source(cx: &mut TestAppContext) {
    let text = lines(["AT".into(), "OK".into()]);
    let hex = Arc::new(HexLines::new(b"AT\r\nOK\r\n".repeat(10)));
    let (window, view) = open(cx, text);
    view.update(cx, |view, cx| {
        view.set_hex_source(Some(hex.clone() as Arc<dyn LineSource>), None, cx)
    });
    let geometry = Geometry::of(cx, &view);
    drag(cx, window, geometry.caret(0, 0), geometry.caret(2, 0));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some("AT".into())
    );

    focus(cx, window, &view);
    press(cx, window, "alt-h");
    assert_eq!(
        view.read_with(cx, |view, _| view.display_mode()),
        DisplayMode::Hex
    );
    assert_eq!(
        view.read_with(cx, |view, _| view.selection()),
        None,
        "ids changed meaning"
    );
    hex.fetched().reset();
    draw(cx, window);
    assert_eq!(hex.fetched().get(), 5, "five dump lines drawn");

    send(cx, window, mouse_down(geometry.cell(0, 0), 3));
    send(cx, window, mouse_up(geometry.cell(0, 0), 3));
    assert_eq!(
        view.read_with(cx, |view, _| view.selection_text()),
        Some(HexLines::format(0, b"AT\r\nOK\r\nAT\r\nOK\r\n"))
    );
    press(cx, window, "alt-h");
    assert_eq!(
        view.read_with(cx, |view, _| view.display_mode()),
        DisplayMode::Text
    );
}

#[gpui_test]
fn pause_freezes_the_end_and_resume_follows_the_live_tail(cx: &mut TestAppContext) {
    let source = numbered(1000);
    let (window, view) = open(cx, source.clone());
    let before = top_line(cx, &view);
    view.update(cx, |view, cx| assert!(view.pause(cx)));
    for i in 1000..1100 {
        source.push(&format!("line {i}"));
    }
    draw(cx, window);
    assert_eq!(top_line(cx, &view), before, "the screen stands still");
    view.read_with(cx, |view, _| {
        assert_eq!(view.frozen_end(), Some(LineId(1000)));
        assert_eq!(view.lines_since_pause(), Some(100));
        assert_eq!(view.displayed_span().end, LineId(1000));
    });
    // Select-all while paused takes only what is displayed.
    view.update(cx, |view, cx| view.select_all(cx));
    let text = view.read_with(cx, |view, _| view.selection_text()).unwrap();
    assert!(text.ends_with("line 999"));

    view.update(cx, |view, cx| assert!(view.resume(cx)));
    draw(cx, window);
    assert_eq!(top_line(cx, &view), before.offset(100));
}

#[gpui_test]
fn the_frame_stats_overlay_toggles_and_reports(cx: &mut TestAppContext) {
    let (window, view) = open(cx, numbered(100));
    focus(cx, window, &view);
    press(cx, window, keys::TOGGLE_FRAME_STATS);
    assert!(view.read_with(cx, |view, _| view.shows_frame_stats()));
    draw(cx, window);
    let summary = view.read_with(cx, |view, _| view.frame_summary());
    assert!(summary.frames >= 2);
    assert!(summary.lines()[0].starts_with("prepaint "));
    press(cx, window, keys::TOGGLE_FRAME_STATS);
    assert!(!view.read_with(cx, |view, _| view.shows_frame_stats()));
}

/// Draw `frames` frames, scrolling `rows_per_frame` rows each (negative is down), and
/// return each frame's element cost and the lines the source handed out for it.
fn scroll_frames(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<TerminalView>,
    source: &SyntheticLines,
    frames: usize,
    rows_per_frame: f32,
) -> Vec<(Duration, usize, usize)> {
    let row = Geometry::of(cx, view).metrics.row_height;
    let mut out = Vec::with_capacity(frames);
    for _ in 0..frames {
        // The test app draws a notified window when an update ends, so a scroll step
        // can take more than one frame; every frame drawn is counted.
        let before = view.read_with(cx, |view, _| view.frame_stats().frames());
        source.fetched().reset();
        view.update(cx, |view, cx| view.scroll_by(row * rows_per_frame, cx));
        draw(cx, window);
        let samples: Vec<FrameSample> = view.read_with(cx, |view, _| {
            let stats = view.frame_stats();
            let drawn = (stats.frames() - before) as usize;
            let mut samples: Vec<_> = stats.samples().rev().take(drawn).copied().collect();
            samples.reverse();
            samples
        });
        let fetched = source.fetched().reset() as usize;
        assert_eq!(
            fetched,
            samples.iter().map(|s| s.lines_fetched).sum::<usize>(),
            "the element counts what it fetched"
        );
        out.extend(
            samples
                .iter()
                .map(|s| (s.total(), s.lines_fetched, s.lines_shaped)),
        );
    }
    out
}

/// The per-frame budget: 8 ms optimized. Unoptimized builds of this crate get 40 ms.
fn frame_budget() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(40)
    } else {
        Duration::from_millis(8)
    }
}

/// One measured pass: from a cold cache at the bottom, 100 scroll steps up three rows
/// each through fresh text, then 100 back down over the same text.
struct Pass {
    up: Vec<(Duration, usize, usize)>,
    down: Vec<(Duration, usize, usize)>,
}

impl Pass {
    fn frames(&self) -> impl Iterator<Item = &(Duration, usize, usize)> {
        self.up.iter().chain(&self.down)
    }

    fn over_budget(&self, budget: Duration) -> Vec<(usize, Duration)> {
        self.frames()
            .enumerate()
            .filter(|(_, (cost, _, _))| *cost >= budget)
            .map(|(ix, (cost, _, _))| (ix, *cost))
            .collect()
    }

    fn summary(&self) -> String {
        let costs: Vec<f64> = self
            .frames()
            .map(|(cost, _, _)| cost.as_secs_f64() * 1000.0)
            .collect();
        let mean = costs.iter().sum::<f64>() / costs.len() as f64;
        let max = costs.iter().copied().fold(0.0, f64::max);
        let shaped: usize = self.up.iter().map(|(_, _, shaped)| shaped).sum();
        let fetched = self.frames().map(|(_, f, _)| *f).max().unwrap_or(0);
        format!(
            "{} frames, mean {mean:.3} ms, max {max:.3} ms (prepaint+paint), \
             {shaped} lines shaped going up, 0 coming back, at most {fetched} lines fetched a frame",
            costs.len()
        )
    }
}

fn measure_pass(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<TerminalView>,
    source: &SyntheticLines,
    wrap: bool,
    timestamps: TimestampMode,
) -> Pass {
    view.update(cx, |view, cx| {
        view.set_wrap(wrap, cx);
        view.set_timestamps(timestamps, cx);
        // A palette change drops the shaped-line and wrap caches: start cold.
        let palette = view.palette().clone();
        view.set_palette(palette, cx);
        view.jump_to_bottom(cx);
    });
    draw(cx, window);
    let up = scroll_frames(cx, window, view, source, 100, 3.0);
    let down = scroll_frames(cx, window, view, source, 100, -3.0);
    Pass { up, down }
}

/// The milestone's performance gate, headless: a million lines, 100 scroll steps each
/// way, per frame under budget and fetching only about a screen of lines.
///
/// The frames are real GPUI frames in a test window, but GPUI's test platform shapes
/// with a no-op text system, so this measures everything the element does (fetching,
/// cache lookups, layout, run conversion, rectangles, scene building) except CoreText's
/// own shaping; the shaped-line counts bound that part. A pass with a frame over budget
/// is repeated from a cold cache, up to three times, so a descheduled test thread on a
/// busy machine does not fail the gate while a real regression still does.
#[gpui_test]
fn a_million_lines_cost_what_a_screen_costs(cx: &mut TestAppContext) {
    let source = Arc::new(SyntheticLines::new(1_000_000));
    let (window, view) = open(cx, source.clone());
    let rows = Geometry::of(cx, &view).rows();
    // At most a screen of lines, the rows scrolled, and the line before the top that
    // delta timestamps read.
    let bound = rows + 3 + 1;
    let budget = frame_budget();

    for (wrap, timestamps) in [(false, TimestampMode::Off), (true, TimestampMode::Delta)] {
        let mut attempts = Vec::new();
        let pass = loop {
            let pass = measure_pass(cx, window, &view, &source, wrap, timestamps);
            let over = pass.over_budget(budget);
            if over.is_empty() {
                break pass;
            }
            attempts.push(over);
            assert!(
                attempts.len() < 3,
                "wrap={wrap}: frames over the {budget:?} budget in three passes: {attempts:?}"
            );
        };
        for (frame, (_, fetched, _)) in pass.frames().enumerate() {
            assert!(
                *fetched <= bound,
                "wrap={wrap} frame {frame} fetched {fetched} lines for {rows} rows"
            );
        }
        let reshaped: usize = pass.down.iter().map(|(_, _, shaped)| shaped).sum();
        assert_eq!(
            reshaped, 0,
            "wrap={wrap}: scrolling back over seen text shaped lines"
        );
        let shaped: usize = pass.up.iter().map(|(_, _, shaped)| shaped).sum();
        assert!(
            shaped > 0 && shaped <= 100 * 3 + rows,
            "wrap={wrap}: shaped {shaped} lines scrolling 300 rows"
        );
        // Visible with `cargo test -- --nocapture`.
        eprintln!(
            "terminal frame cost, 1M lines, {rows} rows, wrap={wrap}, retries {}: {}",
            attempts.len(),
            pass.summary()
        );
    }
}
