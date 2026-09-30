//! Inline interactive mode through the real keyboard path: keys pressed in a headless
//! window go through GPUI's dispatch (interceptors, the keymap, key listeners) to the
//! session. The fake session records each write; the simulator's AT modem and a
//! recording device check what reaches the far end of a real link.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{Direction, PortId, SerialConfig};
use serialist_sim::{DeviceOutput, LinkConfig, SimDevice, SimWorld};

use crate::actions::keys;
use crate::config::{self, Config};
use crate::inline::Mode;
use crate::prelude::*;
use crate::session_options::SessionOptions;
use crate::session_view::SessionView;
use crate::test_support::{
    FakeFeed, TestDir, allow_engine_threads, displayed, fake_session, has_rx_line,
    open_test_window, open_workspace, run_until, step, wait_connected,
};

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .unwrap();
}

/// A session view over a fake session, connected, with local echo as given.
fn open_view(
    cx: &mut TestAppContext,
    local_echo: bool,
) -> (AnyWindowHandle, Entity<SessionView>, FakeFeed) {
    allow_engine_threads(cx);
    let (session, feed) = fake_session();
    let options = SessionOptions {
        local_echo,
        ..SessionOptions::default()
    };
    let (window, view) = open_test_window(cx, move |window, cx| {
        SessionView::new(
            PortId::new("virtual:echo"),
            SerialConfig::default(),
            session,
            options,
            window,
            cx,
        )
    });
    feed.connected("virtual:echo");
    run_until(cx, "the connect notice", |cx| {
        displayed(cx, &view).len() == 1
    });
    (window, view, feed)
}

fn set_mode(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SessionView>,
    mode: Mode,
) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| view.set_mode(mode, window, cx));
    })
    .unwrap();
}

fn mode(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Mode {
    view.read_with(cx, |view, _| view.mode())
}

fn tx_lines(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<String> {
    displayed(cx, view)
        .into_iter()
        .filter(|line| line.direction == Direction::Tx)
        .map(|line| line.text)
        .collect()
}

fn terminal_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SessionView>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        view.read(cx).terminal().focus_handle(cx).is_focused(window)
    })
    .unwrap()
}

fn compose_focused(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SessionView>,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        let compose = view.read(cx).compose().read(cx);
        compose
            .input()
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    })
    .unwrap()
}

fn compose_rendered(cx: &mut TestAppContext, window: AnyWindowHandle) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find("compose-input").is_some()
    })
    .unwrap()
}

#[gpui_test]
fn every_key_is_one_write_of_its_encoding(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, false);
    set_mode(cx, window, &view, Mode::Inline);
    assert!(terminal_focused(cx, window, &view));

    let keys_and_bytes: &[(&str, &[u8])] = &[
        ("a", b"a"),
        ("shift-a", b"A"),
        ("space", b" "),
        ("ctrl-c", &[0x03]),
        // Plain ctrl chords the app binds elsewhere (Pause and Quit off macOS) belong to
        // the device in inline mode.
        ("ctrl-p", &[0x10]),
        ("ctrl-q", &[0x11]),
        ("up", b"\x1b[A"),
        ("f5", b"\x1b[15~"),
        // Bound to scrolling in the `Terminal` context; sent in `TerminalInline`.
        ("pageup", b"\x1b[5~"),
        ("home", b"\x1b[H"),
        // Not stolen by focus navigation.
        ("tab", b"\t"),
        ("shift-tab", b"\x1b[Z"),
        ("escape", &[0x1b]),
        ("backspace", &[0x7f]),
        ("alt-x", b"\x1bx"),
        // The session's line ending (CRLF by default).
        ("enter", b"\r\n"),
    ];
    for (key, _) in keys_and_bytes {
        press(cx, window, key);
    }
    let expected: Vec<Vec<u8>> = keys_and_bytes
        .iter()
        .map(|(_, bytes)| bytes.to_vec())
        .collect();
    assert_eq!(feed.written(), expected, "one write per key, in order");

    // A key bound in `TerminalInline` stays an action: shift-pageup scrolls.
    press(cx, window, "shift-pageup");
    assert_eq!(feed.written().len(), expected.len());
    assert_eq!(mode(cx, &view), Mode::Inline);
}

#[gpui_test]
fn the_backspace_setting_and_the_line_ending_apply(cx: &mut TestAppContext) {
    let dir = TestDir::new("inline-backspace");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "inline": { "backspace": "0x08" } }"#,
    )
    .unwrap();
    let (window, view, feed) = open_view(cx, false);
    let paths = ConfigPaths::new(dir.path());
    cx.update(|cx| config::install(Config::load(paths, false), cx));
    let problems = cx.update(|cx| cx.global::<Config>().problems().to_vec());
    assert!(
        problems.is_empty(),
        "inline.* is not an unknown key: {problems:?}"
    );

    view.update(cx, |view, cx| {
        view.compose().update(cx, |compose, cx| {
            compose.set_line_ending(serialist_core::LineEnding::Lf, cx)
        });
    });
    set_mode(cx, window, &view, Mode::Inline);
    press(cx, window, "backspace");
    press(cx, window, "enter");
    assert_eq!(feed.written(), [vec![0x08], b"\n".to_vec()]);
}

#[gpui_test]
fn local_echo_shows_typed_text_as_it_is_typed(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, true);
    set_mode(cx, window, &view, Mode::Inline);

    // Each key shows in a Tx line of its own the moment it is pressed.
    press(cx, window, "h");
    run_until(cx, "the first character", |cx| tx_lines(cx, &view) == ["h"]);
    press(cx, window, "i");
    press(cx, window, "x");
    run_until(cx, "the rest", |cx| tx_lines(cx, &view) == ["hix"]);
    // Backspace takes the last character back; keys that are not text echo as nothing.
    for key in ["backspace", "left", "ctrl-c"] {
        press(cx, window, key);
    }
    run_until(cx, "the backspace", |cx| tx_lines(cx, &view) == ["hi"]);
    step(cx, 3);
    assert_eq!(tx_lines(cx, &view), ["hi"]);
    let open = displayed(cx, &view).pop().expect("lines");
    assert_eq!(
        (open.direction, open.text.as_str(), open.complete),
        (Direction::Tx, "hi", false),
        "still being typed"
    );

    // Enter ends the line. The reply comes after it.
    press(cx, window, "enter");
    feed.data(b"hi there\r\n");
    run_until(cx, "the reply", |cx| has_rx_line(cx, &view, "hi there"));
    let lines: Vec<(Direction, String, bool)> = displayed(cx, &view)
        .into_iter()
        .map(|line| (line.direction, line.text, line.complete))
        .collect();
    assert_eq!(
        lines[1..],
        [
            (Direction::Tx, "hi".into(), true),
            (Direction::Rx, "hi there".into(), true)
        ],
        "the echo lands before the reply"
    );

    // Leaving inline mode ends what was typed since the last Enter.
    press(cx, window, "o");
    press(cx, window, "k");
    run_until(cx, "the partial line", |cx| tx_lines(cx, &view).len() == 2);
    assert_eq!(tx_lines(cx, &view), ["hi", "ok"]);
    set_mode(cx, window, &view, Mode::Command);
    run_until(cx, "the line to end", |cx| {
        displayed(cx, &view)
            .last()
            .is_some_and(|line| line.complete)
    });
    // Backspace in the next session of typing does not eat into it.
    set_mode(cx, window, &view, Mode::Inline);
    press(cx, window, "backspace");
    step(cx, 3);
    assert_eq!(tx_lines(cx, &view), ["hi", "ok"]);
}

#[gpui_test]
fn typing_over_a_prompt_does_not_end_it_and_the_typed_line_follows(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, true);
    feed.data(b"login: ");
    run_until(cx, "the prompt", |cx| has_rx_line(cx, &view, "login: "));
    set_mode(cx, window, &view, Mode::Inline);
    for key in ["r", "o", "o", "t"] {
        press(cx, window, key);
    }
    run_until(cx, "the typed text", |cx| tx_lines(cx, &view) == ["root"]);
    let lines: Vec<(Direction, String, bool)> = displayed(cx, &view)
        .into_iter()
        .map(|line| (line.direction, line.text, line.complete))
        .collect();
    assert_eq!(
        lines[1..],
        [
            (Direction::Rx, "login: ".into(), false),
            (Direction::Tx, "root".into(), false),
        ],
        "the prompt is still the line in progress and the typed text follows it"
    );

    // Enter, and the device answers on a new line: the prompt ends where the device
    // ended it, with what was typed right after, and the reply after that.
    press(cx, window, "enter");
    feed.data(b"\r\nPassword: ");
    run_until(cx, "the next prompt", |cx| {
        has_rx_line(cx, &view, "Password: ")
    });
    let lines: Vec<(Direction, String, bool)> = displayed(cx, &view)
        .into_iter()
        .map(|line| (line.direction, line.text, line.complete))
        .collect();
    assert_eq!(
        lines[1..],
        [
            (Direction::Rx, "login: ".into(), true),
            (Direction::Tx, "root".into(), true),
            (Direction::Rx, "Password: ".into(), false),
        ]
    );
}

#[gpui_test]
fn a_pasted_text_is_echoed_line_by_line(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, true);
    set_mode(cx, window, &view, Mode::Inline);
    view.update(cx, |view, cx| view.paste_text("one\r\ntwo\nthr", cx));
    run_until(cx, "the echo", |cx| {
        tx_lines(cx, &view) == ["one", "two", "thr"]
    });
    let lines = displayed(cx, &view);
    let completeness: Vec<bool> = lines[1..].iter().map(|line| line.complete).collect();
    assert_eq!(completeness, [true, true, false], "the last is still open");
    // What goes out is the paste with Enter for each break (CRLF here).
    run_until(cx, "the paste to go out", |_| {
        feed.written().concat() == b"one\r\ntwo\r\nthr"
    });
    // And without local echo nothing is echoed.
    let (window, view, feed) = open_view(cx, false);
    set_mode(cx, window, &view, Mode::Inline);
    view.update(cx, |view, cx| view.paste_text("one\ntwo", cx));
    run_until(cx, "the paste to go out", |_| {
        feed.written().concat() == b"one\r\ntwo"
    });
    assert!(tx_lines(cx, &view).is_empty());
}

#[gpui_test]
fn without_local_echo_nothing_is_echoed(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, false);
    set_mode(cx, window, &view, Mode::Inline);
    for key in ["h", "i", "backspace", "enter"] {
        press(cx, window, key);
    }
    feed.data(b"reply\r\n");
    run_until(cx, "the reply", |cx| has_rx_line(cx, &view, "reply"));
    assert!(tx_lines(cx, &view).is_empty());
    assert_eq!(feed.written().len(), 4);
    // Leaving inline mode adds nothing either.
    set_mode(cx, window, &view, Mode::Command);
    step(cx, 3);
    assert!(tx_lines(cx, &view).is_empty());
}

#[gpui_test]
fn the_escape_chord_leaves_and_a_double_press_sends_it(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, false);
    set_mode(cx, window, &view, Mode::Inline);

    press(cx, window, "ctrl-]");
    assert_eq!(mode(cx, &view), Mode::Command, "the chord leaves");
    assert!(feed.written().is_empty(), "and is not sent");
    assert!(
        compose_focused(cx, window, &view),
        "the compose bar has the focus"
    );

    // A second press right away: back in inline mode, and the chord goes out.
    cx.executor().advance_clock(Duration::from_millis(100));
    press(cx, window, "ctrl-]");
    assert_eq!(mode(cx, &view), Mode::Inline);
    assert_eq!(feed.written(), [vec![0x1d]]);
    assert!(terminal_focused(cx, window, &view));

    // Leaving again, then a press after the double-press window: stays in command mode.
    press(cx, window, "ctrl-]");
    assert_eq!(mode(cx, &view), Mode::Command);
    cx.executor()
        .advance_clock(crate::inline::DOUBLE_PRESS + Duration::from_millis(1));
    press(cx, window, "ctrl-]");
    assert_eq!(mode(cx, &view), Mode::Command);
    assert_eq!(feed.written(), [vec![0x1d]], "nothing more sent");

    // A configured chord replaces ctrl-], which is then an ordinary key.
    let dir = TestDir::new("inline-chord");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "inline": { "escape_chord": "ctrl-b" } }"#,
    )
    .unwrap();
    let paths = ConfigPaths::new(dir.path());
    cx.update(|cx| config::install(Config::load(paths, false), cx));
    set_mode(cx, window, &view, Mode::Inline);
    press(cx, window, "ctrl-]");
    assert_eq!(mode(cx, &view), Mode::Inline);
    assert_eq!(feed.written().last(), Some(&vec![0x1d]));
    press(cx, window, "ctrl-b");
    assert_eq!(mode(cx, &view), Mode::Command);
}

#[gpui_test]
fn the_toolbar_toggles_the_mode_and_the_compose_bar(cx: &mut TestAppContext) {
    let (window, view, _feed) = open_view(cx, false);
    assert!(compose_rendered(cx, window));
    cx.update_window(window, |_, window, cx| window.click("inline-mode", cx))
        .unwrap();
    assert_eq!(mode(cx, &view), Mode::Inline);
    assert!(!compose_rendered(cx, window), "hidden in inline mode");
    assert!(terminal_focused(cx, window, &view));
    assert_eq!(view.read_with(cx, |v, _| v.status_line().mode), "INLINE");

    cx.update_window(window, |_, window, cx| window.click("inline-mode", cx))
        .unwrap();
    assert_eq!(mode(cx, &view), Mode::Command);
    assert!(compose_rendered(cx, window));
    assert!(
        compose_focused(cx, window, &view),
        "leaving focuses the compose bar"
    );
    assert_eq!(view.read_with(cx, |v, _| v.status_line().mode), "COMMAND");
}

#[gpui_test]
fn inline_typing_reaches_the_at_modem_and_its_ok_comes_back(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:at"));
    let view = wait_connected(cx, &workspace);

    // The mode key, from the compose bar the connect focused.
    press(cx, window, keys::TOGGLE_INLINE);
    assert_eq!(mode(cx, &view), Mode::Inline);
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.mode, "INLINE");
    assert!(!compose_rendered(cx, window));

    for key in ["a", "t", "i", "enter"] {
        press(cx, window, key);
    }
    run_until(cx, "the modem's identity", |cx| {
        has_rx_line(cx, &view, serialist_sim::AtDevice::DEFAULT_IDENTITY)
    });
    run_until(cx, "OK", |cx| has_rx_line(cx, &view, "OK"));
    run_until(cx, "ati and CRLF counted out", |cx| {
        view.read_with(cx, |v, _| v.stats().tx_bytes == 5)
    });

    // And the same key leaves.
    press(cx, window, keys::TOGGLE_INLINE);
    assert_eq!(mode(cx, &view), Mode::Command);
    assert!(compose_focused(cx, window, &view));
}

/// Each piece of what the host sent, with when it arrived.
type ArrivalLog = Arc<Mutex<Vec<(Instant, Vec<u8>)>>>;

/// Records when each piece of what the host sent arrived.
struct Arrivals(ArrivalLog);

impl SimDevice for Arrivals {
    fn name(&self) -> &str {
        "arrivals"
    }

    fn on_receive(&mut self, bytes: &[u8], _: &mut dyn DeviceOutput) {
        self.0.lock().push((Instant::now(), bytes.to_vec()));
    }
}

fn received(arrivals: &ArrivalLog) -> usize {
    arrivals.lock().iter().map(|(_, bytes)| bytes.len()).sum()
}

/// Real time for a byte already written to cross the link; used only to check that
/// nothing more arrives before the next chunk is due.
const SETTLE: Duration = Duration::from_millis(40);

/// Wait in real time, without moving the app's clock, until `done`: the link and the
/// device run on real threads, and the paste's pacing runs on the app's clock.
fn wait_in_real_time(cx: &mut TestAppContext, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + crate::test_support::ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[gpui_test]
fn paste_goes_out_in_chunks_of_the_configured_size_and_spacing(cx: &mut TestAppContext) {
    let dir = TestDir::new("inline-paste");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "inline": { "paste_chunk_bytes": 16, "paste_chunk_delay_ms": 50 } }"#,
    )
    .unwrap();
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let world = SimWorld::empty();
    let seen = arrivals.clone();
    world.add_virtual("arrivals", "Arrivals", LinkConfig::unpaced(), move || {
        Box::new(Arrivals(seen.clone()))
    });
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:arrivals"));
    cx.update(|cx| config::install(Config::load(ConfigPaths::new(dir.path()), false), cx));
    let view = wait_connected(cx, &workspace);
    press(cx, window, keys::TOGGLE_INLINE);

    // 100 bytes: six chunks of 16 and one of 4. Line breaks become CRLF.
    let text = format!("{}\n{}", "a".repeat(49), "b".repeat(49));
    cx.update(|cx| cx.write_to_clipboard(ClipboardItem::new_string(text)));
    press(cx, window, keys::PASTE);

    let status = |cx: &mut TestAppContext| {
        workspace
            .read_with(cx, |w, cx| w.status_line(cx))
            .unwrap()
            .paste
    };
    wait_in_real_time(cx, "the first chunk", || received(&arrivals) == 16);
    assert_eq!(status(cx).as_deref(), Some("Pasting 16 B of 100 B"));
    for chunk in 1..7 {
        // Nothing more until the delay has passed on the app's clock.
        cx.executor().advance_clock(Duration::from_millis(49));
        cx.run_until_parked();
        std::thread::sleep(SETTLE);
        assert_eq!(received(&arrivals), chunk * 16, "chunk {chunk} came early");
        cx.executor().advance_clock(Duration::from_millis(1));
        let expected = (chunk * 16 + 16).min(100);
        wait_in_real_time(cx, "the next chunk", || received(&arrivals) == expected);
    }
    run_until(cx, "the paste to finish", |cx| status(cx).is_none());
    let arrivals = arrivals.lock();
    assert!(
        arrivals.iter().all(|(_, bytes)| bytes.len() <= 16),
        "no two chunks arrived as one: {:?}",
        arrivals.iter().map(|(_, b)| b.len()).collect::<Vec<_>>()
    );
    let bytes: Vec<u8> = arrivals.iter().flat_map(|(_, b)| b.clone()).collect();
    let expected = format!("{}\r\n{}", "a".repeat(49), "b".repeat(49));
    assert_eq!(bytes, expected.as_bytes());
    drop(arrivals);
    assert!(view.read_with(cx, |v, _| v.paste_progress().is_none()));
}

#[gpui_test]
fn leaving_inline_mode_cancels_a_paste(cx: &mut TestAppContext) {
    let (window, view, feed) = open_view(cx, false);
    let dir = TestDir::new("inline-paste-cancel");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "inline": { "paste_chunk_bytes": 8, "paste_chunk_delay_ms": 10 } }"#,
    )
    .unwrap();
    cx.update(|cx| config::install(Config::load(ConfigPaths::new(dir.path()), false), cx));
    set_mode(cx, window, &view, Mode::Inline);
    view.update(cx, |view, cx| view.paste_text(&"x".repeat(64), cx));
    step(cx, 1);
    assert_eq!(feed.written().len(), 1, "the first chunk goes at once");

    cx.executor().advance_clock(Duration::from_millis(10));
    cx.run_until_parked();
    assert_eq!(feed.written().len(), 2);

    set_mode(cx, window, &view, Mode::Command);
    for _ in 0..10 {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
    }
    assert_eq!(feed.written().len(), 2, "no chunk after leaving");
    assert!(view.read_with(cx, |v, _| v.paste_progress().is_none()));
    assert!(
        feed.written().iter().all(|chunk| chunk.len() == 8),
        "chunks of the configured size"
    );
}
