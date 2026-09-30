//! A window that runs the terminal element on its own, fed by the in-memory double:
//! `serialist --terminal-demo`. The session view draws the same element over the page
//! store; this keeps a store-free way to look at the element with styled content.
//!
//! It starts with 200 000 styled lines, appends 2 000 a second (a sent line and an app
//! notice every fifty, a highlighted alert now and then), keeps at most 300 000 so
//! eviction is exercised, and offers a hex dump of the same bytes. Every terminal key
//! binding works: cmd-f, alt-z, alt-t, alt-h, cmd-alt-i, page keys, cmd-a, cmd-c.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serialist_core::{Color, Direction, LineId, LineSource, Searcher, StyleFlags, StyleRun};

use crate::prelude::*;
use crate::terminal::double::{HexLines, MemoryLines, SyntheticLines, plain_runs, run};
use crate::terminal::view::TerminalView;

const PREFILL: u64 = 200_000;
const CAPACITY: usize = 300_000;
const LINES_PER_SECOND: u64 = 2_000;
const TICK: Duration = Duration::from_millis(16);

/// Line `n` of the demo stream.
fn demo_line(generator: &SyntheticLines, n: u64) -> (String, Vec<StyleRun>, Direction) {
    match n % 50 {
        0 => {
            let text = format!("AT+READ={}", n % 97);
            let runs = plain_runs(text.len());
            (text, runs, Direction::Tx)
        }
        25 => {
            let text = format!("Reconnected after {} ms", n % 700);
            let runs = plain_runs(text.len());
            (text, runs, Direction::Notice)
        }
        13 => {
            let alert = " ALERT ";
            let rest = format!(" over-temperature on sensor {:03}", n % 997);
            let mut flags = StyleFlags::BOLD;
            flags.insert(StyleFlags::INVERSE);
            let runs = vec![
                run(alert.len(), Color::Ansi(1), flags),
                run(rest.len(), Color::Indexed(208), StyleFlags::UNDERLINE),
            ];
            (format!("{alert}{rest}"), runs, Direction::Rx)
        }
        _ => {
            let line = generator
                .line(LineId(n))
                .expect("the generator covers every id");
            (line.text, line.runs, Direction::Rx)
        }
    }
}

fn append(
    lines: &MemoryLines,
    hex: &HexLines,
    generator: &SyntheticLines,
    range: std::ops::Range<u64>,
) {
    let mut bytes = Vec::new();
    for n in range {
        let (text, runs, direction) = demo_line(generator, n);
        if direction == Direction::Rx {
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }
        lines.push_styled(&text, runs, direction, Instant::now());
    }
    hex.extend(&bytes);
}

/// Open the demo window.
pub fn open_terminal_demo(cx: &mut App) -> Result<Entity<TerminalView>> {
    let generator = Arc::new(SyntheticLines::new(u64::MAX / 2));
    let lines = Arc::new(MemoryLines::new().with_capacity(CAPACITY));
    let hex = Arc::new(HexLines::new(Vec::new()));
    append(&lines, &hex, &generator, 0..PREFILL);

    let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Serialist terminal demo".into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
    };
    let (_, view) = kit_open_window(options, cx, move |window, cx| {
        cx.new(|cx| {
            let mut view = TerminalView::new(lines.clone() as Arc<dyn LineSource>, window, cx);
            view.set_searcher(Some(lines.clone() as Arc<dyn Searcher>), cx);
            view.set_hex_source(Some(hex.clone() as Arc<dyn LineSource>), None, cx);
            view.focus_handle(cx).focus(window, cx);
            // The feed: a frame's worth of lines each tick, one repaint per tick.
            cx.spawn(async move |this, cx| {
                let per_tick = LINES_PER_SECOND * TICK.as_millis() as u64 / 1000;
                let mut next = PREFILL;
                loop {
                    cx.background_executor().timer(TICK).await;
                    append(&lines, &hex, &generator, next..next + per_tick);
                    let changed = LineId(next)..LineId(next + per_tick);
                    next += per_tick;
                    if this
                        .update(cx, |view, cx| view.lines_appended(changed, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
            view
        })
    })?;
    cx.activate(true);
    Ok(view)
}
