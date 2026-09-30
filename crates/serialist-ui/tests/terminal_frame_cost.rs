//! The terminal's frame cost with the platform's real text system.
//!
//! The unit tests draw with GPUI's no-op text system, which places glyphs without
//! shaping them. This test swaps in the platform's own (CoreText on macOS) so every
//! line the element shapes goes through real font shaping and glyph rasterization
//! bounds, and holds the element to the same per-frame budget over a million lines.
//!
//! It runs as a plain binary (`harness = false`) because AppKit objects, which the
//! platform text system is reached through, must be created on the main thread. Other
//! platforms skip it; their text systems come with their own windowing requirements.

use std::sync::Arc;
use std::time::Duration;

use serialist_ui::prelude::*;
use serialist_ui::terminal::double::SyntheticLines;
use serialist_ui::terminal::{FrameSample, TerminalView, TimestampMode};

fn budget() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_millis(40)
    } else {
        Duration::from_millis(8)
    }
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| {
        window.refresh();
        window.draw(cx).clear(cx);
    })
    .expect("the window is open");
}

/// Scroll `steps` times by `rows` rows and return every frame drawn.
fn scroll(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<TerminalView>,
    steps: usize,
    rows: f32,
) -> Vec<FrameSample> {
    let row = view.read_with(cx, |view, _| {
        view.cell_metrics().expect("a frame was drawn").row_height
    });
    let mut out = Vec::new();
    for _ in 0..steps {
        let before = view.read_with(cx, |view, _| view.frame_stats().frames());
        view.update(cx, |view, cx| view.scroll_by(row * rows, cx));
        draw(cx, window);
        view.read_with(cx, |view, _| {
            let stats = view.frame_stats();
            let drawn = (stats.frames() - before) as usize;
            let start = out.len();
            out.extend(stats.samples().rev().take(drawn).copied());
            out[start..].reverse();
        });
    }
    out
}

fn run() {
    let platform = platform::current_platform(true);
    let mut cx = TestAppContext::build_with_text_system(
        TestDispatcher::new(0),
        None,
        platform.text_system(),
    );
    let source = Arc::new(SyntheticLines::new(1_000_000));
    let (window, view) = cx.update(|cx| {
        serialist_ui::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(700.)),
        };
        let source = source.clone();
        kit_open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| TerminalView::new(source, window, cx)),
        )
        .expect("open a test window")
    });
    draw(&mut cx, window);
    let metrics = view.read_with(&cx, |view, _| view.cell_metrics().unwrap());
    let rows = (700.0 / f32::from(metrics.row_height)).ceil() as usize + 1;
    let budget = budget();

    for (wrap, timestamps) in [(false, TimestampMode::Off), (true, TimestampMode::Delta)] {
        let mut attempts = 0;
        loop {
            view.update(&mut cx, |view, cx| {
                view.set_wrap(wrap, cx);
                view.set_timestamps(timestamps, cx);
                // A palette change drops the caches: every pass starts cold.
                let palette = view.palette().clone();
                view.set_palette(palette, cx);
                view.jump_to_bottom(cx);
            });
            draw(&mut cx, window);
            let up = scroll(&mut cx, window, &view, 100, 3.0);
            let down = scroll(&mut cx, window, &view, 100, -3.0);
            let frames: Vec<&FrameSample> = up.iter().chain(&down).collect();
            let over: Vec<_> = frames
                .iter()
                .enumerate()
                .filter(|(_, s)| s.total() >= budget)
                .map(|(ix, s)| (ix, s.total()))
                .collect();
            attempts += 1;
            if !over.is_empty() {
                assert!(
                    attempts < 3,
                    "wrap={wrap}: frames over the {budget:?} budget in three passes: {over:?}"
                );
                continue;
            }
            for (ix, sample) in frames.iter().enumerate() {
                assert!(
                    sample.lines_fetched <= rows + 4,
                    "wrap={wrap} frame {ix} fetched {} lines for {rows} rows",
                    sample.lines_fetched
                );
            }
            let reshaped: usize = down.iter().map(|s| s.lines_shaped).sum();
            assert_eq!(reshaped, 0, "wrap={wrap}: seen text was shaped again");
            let shaped: usize = up.iter().map(|s| s.lines_shaped).sum();
            let ms = |d: Duration| d.as_secs_f64() * 1000.0;
            let mean_total =
                frames.iter().map(|s| ms(s.total())).sum::<f64>() / frames.len() as f64;
            let shaping: Vec<&&FrameSample> =
                frames.iter().filter(|s| s.lines_shaped > 0).collect();
            let mean_shaping =
                shaping.iter().map(|s| ms(s.total())).sum::<f64>() / shaping.len().max(1) as f64;
            let max = frames.iter().map(|s| ms(s.total())).fold(0.0, f64::max);
            eprintln!(
                "terminal frame cost (platform text system, 1M lines, {rows} rows, wrap={wrap}): \
                 {} frames, mean {mean_total:.3} ms, mean {mean_shaping:.3} ms over the {} frames \
                 that shaped new lines, max {max:.3} ms; {shaped} lines shaped going up, 0 \
                 coming back; retries {}",
                frames.len(),
                shaping.len(),
                attempts - 1
            );
            break;
        }
    }
    cx.update(|cx| cx.quit());
}

fn main() {
    if cfg!(target_os = "macos") {
        run();
    } else {
        eprintln!("terminal_frame_cost: skipped, needs the macOS text system");
    }
}
