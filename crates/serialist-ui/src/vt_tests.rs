//! VT mode against the real engine: sessions over `SimWorld` links, the ingest thread
//! feeding a terminal screen next to the store, keys pressed through the window, frames
//! drawn in a headless window. The boot menu device (`virtual:menu`) and small devices
//! written here stand in for U-Boot and a Linux console.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{LineSource, PortId, SerialConfig, StyleFlags};
use serialist_sim::{DeviceOutput, LinkConfig, MenuDevice, SimDevice, SimWorld};
use serialist_vt::VtSnapshot;

use crate::actions::keys;
use crate::config::{self, Config};
use crate::emulation::Emulation;
use crate::export::ExportFormat;
use crate::inline::Mode;
use crate::prelude::*;
use crate::session_options::{DisplayDefaults, SessionOptions};
use crate::session_view::{BELL_FLASH, FRAME, SessionView};
use crate::terminal::element::PADDING_LEFT;
use crate::test_support::{
    ENGINE_WAIT, TestDir, allow_engine_threads, displayed, fake_session, open_test_window,
    open_test_window_sized, resize_window, run_until, wait_connected,
};
use crate::workspace::{AppOptions, Workspace};

/// A config directory whose settings put sessions in VT mode.
fn vt_config(name: &str) -> TestDir {
    let dir = TestDir::new(name);
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "terminal": { "emulation": "vt" } }"#,
    )
    .expect("write settings.json");
    dir
}

/// A workspace over `world` in a `size` window with the configuration in `dir`, opening
/// `port` at startup, and its session once connected.
fn open(
    cx: &mut TestAppContext,
    world: &SimWorld,
    dir: &TestDir,
    port: &str,
    size: (f32, f32),
) -> (AnyWindowHandle, Entity<Workspace>, Entity<SessionView>) {
    allow_engine_threads(cx);
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: None,
        open_ports: vec![PortId::new(port)],
        store: None,
        replay: None,
    };
    let paths = ConfigPaths::new(dir.path());
    let (window, workspace) = open_test_window_sized(cx, size, move |window, cx| {
        config::install(Config::load(paths, false), cx);
        Workspace::new(options, window, cx)
    });
    let view = wait_connected(cx, &workspace);
    (window, workspace, view)
}

/// A world with only `virtual:menu`, which redraws on keys and never on a timer, so a
/// test sees exactly the redraws it causes.
fn quiet_menu_world() -> SimWorld {
    let world = SimWorld::empty();
    world.add_virtual(
        SimWorld::MENU,
        "Boot menu (virtual)",
        LinkConfig::unpaced(),
        || Box::new(MenuDevice::new().with_redraw_interval(None)),
    );
    world
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .unwrap();
    cx.run_until_parked();
}

/// Draw one frame.
fn frame(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    cx.run_until_parked();
}

/// The screen snapshot the view shows.
fn screen(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Arc<VtSnapshot> {
    view.read_with(cx, |v, _| v.vt_snapshot().cloned())
        .expect("the session is in VT mode")
}

/// Wait until the screen the view shows satisfies `done`, and return it.
fn wait_for_screen(
    cx: &mut TestAppContext,
    view: &Entity<SessionView>,
    what: &str,
    mut done: impl FnMut(&VtSnapshot) -> bool,
) -> Arc<VtSnapshot> {
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        let snapshot = screen(cx, view);
        if done(&snapshot) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ENGINE_WAIT:?} waiting for {what}; the screen shows {:?}",
            snapshot.screen_text()
        );
        cx.executor().advance_clock(FRAME);
    }
}

/// Draw until the screen is the size the element fits, and return that size.
fn settle_size(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SessionView>,
) -> (usize, usize) {
    let terminal = view.read_with(cx, |v, _| v.terminal().clone());
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        frame(cx, window);
        let grid = terminal.read_with(cx, |t, _| t.grid_size());
        let snapshot = screen(cx, view);
        if let Some(grid) = grid
            && grid == (snapshot.columns(), snapshot.viewport_rows())
        {
            let state = terminal.read_with(cx, |t, _| t.screen_state()).unwrap();
            assert_eq!((state.columns, state.rows), grid, "the terminal was told");
            return grid;
        }
        assert!(
            Instant::now() < deadline,
            "the screen never took the element's size: {grid:?}"
        );
        cx.executor().advance_clock(FRAME);
    }
}

/// Screen rows drawn in reverse video: the menu's highlight.
fn highlighted(snapshot: &VtSnapshot) -> Vec<usize> {
    (0..snapshot.viewport_rows())
        .filter(|&row| {
            snapshot.visible_line(row).is_some_and(|line| {
                line.runs
                    .iter()
                    .any(|run| run.style.flags.contains(StyleFlags::INVERSE))
            })
        })
        .collect()
}

#[gpui_test]
fn the_boot_menu_draws_in_vt_mode_and_the_arrows_redraw_only_what_changed(cx: &mut TestAppContext) {
    let dir = vt_config("vt-menu");
    let (window, workspace, view) =
        open(cx, &quiet_menu_world(), &dir, "virtual:menu", (1280., 800.));
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.emulation, Some("VT"), "the status bar's chip");

    // The keys go to the device from here on.
    press(cx, window, keys::TOGGLE_INLINE);
    assert_eq!(view.read_with(cx, |v, _| v.mode()), Mode::Inline);
    let drawn = wait_for_screen(cx, &view, "the menu", |snap| highlighted(snap) == [2]);
    let rows = drawn.screen_text();
    assert_eq!(rows[0], "  *** Serialist Boot Menu ***");
    assert_eq!(rows[2], "     Boot from eMMC");
    assert_eq!(rows[3], "     Boot from network (TFTP)");
    assert_eq!(rows[4], "     U-Boot console");
    assert_eq!(rows[6], "  Press UP/DOWN to move, ENTER to select");
    assert_eq!(rows[7], "  Redraw 1");
    // The terminal draws the screen's rows, not the store's lines.
    let lines = displayed(cx, &view);
    assert!(lines.iter().any(|line| line.text == "     U-Boot console"));
    assert!(
        !lines
            .iter()
            .any(|line| line.text.starts_with("Connected to ")),
        "the log's notice is not on the screen"
    );
    // The store has everything too, as a smear of escape leftovers. (Its publication
    // and the screen's are separate, so either may show first.)
    run_until(cx, "the menu in the store", |cx| {
        view.read_with(cx, |v, _| v.snapshot().raw_range().end > 0)
    });

    let (columns, _) = settle_size(cx, window, &view);
    assert!(columns >= 40, "the menu fits: {columns} columns");
    let before = screen(cx, &view);
    let terminal = view.read_with(cx, |v, _| v.terminal().clone());
    let frames = terminal.read_with(cx, |t, _| t.frame_stats().frames());

    press(cx, window, "down");
    let after = wait_for_screen(cx, &view, "the second item", |snap| {
        highlighted(snap) == [3]
    });
    // The device redrew everything in place; three rows differ (the old and the new
    // highlight and the redraw count), and the range runs from the first to the last.
    let first = after.first_visible();
    assert_eq!(
        after.first_visible(),
        before.first_visible(),
        "nothing scrolled"
    );
    assert_eq!(
        after.changed_since(&before),
        first.offset(2)..first.offset(8)
    );
    assert_eq!(after.screen_text()[7], "  Redraw 2");
    frame(cx, window);
    // Frames since the key (pressing draws some, a repaint after the wake may draw
    // another): together they shaped the three changed rows and nothing else, and the
    // rest of the screen came from the cache.
    let (samples, shaped, hits) = terminal.read_with(cx, |t, _| {
        let stats = t.frame_stats();
        let new = (stats.frames() - frames) as usize;
        let since: Vec<_> = stats.samples().rev().take(new).copied().collect();
        let shaped: usize = since.iter().map(|s| s.lines_shaped).sum();
        let hits: usize = since.iter().map(|s| s.cache_hits).sum();
        (since, shaped, hits)
    });
    assert!(
        samples.len() < crate::terminal::stats::WINDOW,
        "{samples:?}"
    );
    assert_eq!(
        shaped, 3,
        "only the changed rows are shaped again: {samples:?}"
    );
    assert!(hits > 0, "{samples:?}");

    // Enter selects.
    press(cx, window, "enter");
    wait_for_screen(cx, &view, "the selection", |snap| {
        snap.visible_line(MenuDevice::STATUS_ROW - 1)
            .is_some_and(|line| line.text.ends_with("selected: Boot from network (TFTP)"))
    });
    assert!(
        screen(cx, &view).cursor().is_some_and(|c| !c.visible),
        "the menu hides the cursor"
    );

    // Selection and copy read the screen's rows.
    terminal.update(cx, |t, cx| t.select_all(cx));
    let selected = terminal
        .read_with(cx, |t, _| t.selection_text())
        .expect("a selection");
    assert!(
        selected.starts_with("  *** Serialist Boot Menu ***\n\n     Boot from eMMC\n"),
        "{selected:?}"
    );
    press(cx, window, keys::COPY);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(selected)
    );
}

/// Asks the terminal who it is and where its cursor is on connect, and keeps what comes
/// back.
struct Asker(Arc<Mutex<Vec<u8>>>);

impl SimDevice for Asker {
    fn name(&self) -> &str {
        "asker"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(b"login: \x1b[c\x1b[6n");
    }

    fn on_receive(&mut self, bytes: &[u8], _: &mut dyn DeviceOutput) {
        self.0.lock().extend_from_slice(bytes);
    }
}

fn wait_heard(
    cx: &mut TestAppContext,
    heard: &Arc<Mutex<Vec<u8>>>,
    what: &str,
    done: impl Fn(&[u8]) -> bool,
) {
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        if done(&heard.lock()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; the device heard {:?}",
            String::from_utf8_lossy(&heard.lock())
        );
        std::thread::sleep(Duration::from_millis(1));
        cx.executor().advance_clock(FRAME);
    }
}

#[gpui_test]
fn a_device_attribute_query_is_answered_from_the_ingest_thread(cx: &mut TestAppContext) {
    let dir = vt_config("vt-asker");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heard);
    let world = SimWorld::empty();
    world.add_virtual("asker", "Asker", LinkConfig::unpaced(), move || {
        Box::new(Asker(Arc::clone(&log)))
    });
    let (_window, _workspace, view) = open(cx, &world, &dir, "virtual:asker", (1280., 800.));
    // Primary device attributes, then the cursor after "login: ": row 1, column 8.
    let expected = b"\x1b[?6c\x1b[1;8R";
    wait_heard(cx, &heard, "the answers", |heard| {
        heard.len() >= expected.len()
    });
    assert_eq!(heard.lock().as_slice(), expected);
    wait_for_screen(cx, &view, "the prompt", |snap| {
        snap.screen_text()[0] == "login:"
    });
    assert!(
        view.read_with(cx, |v, _| v.vt_handle().unwrap().take_events())
            .is_empty(),
        "answered on the ingest thread, not queued"
    );
}

#[gpui_test]
fn a_session_without_a_control_handle_answers_from_the_view(cx: &mut TestAppContext) {
    allow_engine_threads(cx);
    let (session, feed) = fake_session();
    let options = SessionOptions {
        display: DisplayDefaults {
            emulation: Emulation::Vt,
            ..DisplayDefaults::default()
        },
        ..SessionOptions::default()
    };
    let (_window, _view) = open_test_window(cx, move |window, cx| {
        SessionView::new(
            PortId::new("virtual:fake"),
            SerialConfig::default(),
            session,
            options,
            window,
            cx,
        )
    });
    feed.connected("virtual:fake");
    // Device status, then the default foreground color.
    feed.data(b"$ \x1b[5n\x1b]10;?\x07");
    run_until(cx, "the answers", |_| feed.written().len() >= 2);
    let written = feed.written();
    assert_eq!(written[0], b"\x1b[0n");
    assert!(
        String::from_utf8_lossy(&written[1]).starts_with("\x1b]10;rgb:"),
        "{:?}",
        String::from_utf8_lossy(&written[1])
    );
}

/// Asks for the text area's size (`CSI 18 t`) whenever it receives a byte, and keeps
/// the reports that come back.
struct Sizer(Arc<Mutex<Vec<u8>>>);

impl SimDevice for Sizer {
    fn name(&self) -> &str {
        "sizer"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(b"# ");
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        let mut heard = self.0.lock();
        if bytes.starts_with(b"\x1b[8;") {
            heard.extend_from_slice(bytes);
        } else {
            out.send(b"\x1b[18t");
        }
    }
}

/// The columns of the last `CSI 8 ; rows ; columns t` report in `heard`.
fn reported_columns(heard: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(heard);
    let report = text.rsplit("\x1b[8;").next()?.strip_suffix('t')?;
    report.split(';').nth(1)?.parse().ok()
}

#[gpui_test]
fn resizing_the_window_resizes_the_screen_and_the_device_hears_it(cx: &mut TestAppContext) {
    let dir = vt_config("vt-sizer");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heard);
    let world = SimWorld::empty();
    world.add_virtual("sizer", "Sizer", LinkConfig::unpaced(), move || {
        Box::new(Sizer(Arc::clone(&log)))
    });
    let (window, _workspace, view) = open(cx, &world, &dir, "virtual:sizer", (1280., 800.));
    press(cx, window, keys::TOGGLE_INLINE);
    let (wide, rows) = settle_size(cx, window, &view);
    assert_ne!(
        (wide, rows),
        (80, 24),
        "sized to the element, not the default"
    );

    press(cx, window, "x");
    wait_heard(cx, &heard, "the first report", |heard| {
        reported_columns(heard).is_some()
    });
    assert_eq!(reported_columns(&heard.lock()), Some(wide));

    // A narrower window: fewer columns, and the device is told so when it asks.
    resize_window(cx, window, (1000., 700.));
    let (narrow, _) = settle_size(cx, window, &view);
    assert!(narrow < wide, "{narrow} columns after {wide}");
    heard.lock().clear();
    press(cx, window, "x");
    wait_heard(cx, &heard, "the second report", |heard| {
        reported_columns(heard).is_some()
    });
    assert_eq!(reported_columns(&heard.lock()), Some(narrow));
    assert_eq!(screen(cx, &view).columns(), narrow);
}

#[gpui_test]
fn switching_modes_keeps_the_store_and_the_screen_exports_as_shown(cx: &mut TestAppContext) {
    let dir = TestDir::new("vt-switch");
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    let (window, _workspace, view) =
        open(cx, &quiet_menu_world(), &dir, "virtual:menu", (1280., 800.));
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Monitor);
    // In monitor mode the menu's first frame is a line of escape leftovers.
    let first_frame = MenuDevice::new().frame().len() as u64;
    run_until(cx, "the menu's bytes", |cx| {
        view.read_with(cx, |v, _| v.snapshot().raw_range().end >= first_frame)
    });
    let monitor_end = view.read_with(cx, |v, _| v.snapshot().end());

    // alt-v in the terminal switches to VT mode: a fresh screen, nothing replayed.
    focus_terminal(cx, window, &view);
    press(cx, window, "alt-v");
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    let fresh = screen(cx, &view);
    assert!(
        fresh.screen_text().iter().all(|row| row.is_empty()),
        "earlier bytes are not replayed: {:?}",
        fresh.screen_text()
    );

    // The next redraw draws the menu on the screen, and the store keeps recording.
    press(cx, window, keys::TOGGLE_INLINE);
    press(cx, window, "down");
    let menu = wait_for_screen(cx, &view, "the redrawn menu", |snap| {
        highlighted(snap) == [3]
    });
    run_until(cx, "the redraw in the store", |cx| {
        view.read_with(cx, |v, _| {
            v.snapshot().raw_range().end >= 2 * first_frame && v.snapshot().end() >= monitor_end
        })
    });
    assert_eq!(
        view.read_with(cx, |v, _| v.snapshot().raw_range().start),
        0,
        "the store has every byte"
    );

    // The screen exports as it is drawn.
    let path = dir.join("screen.txt");
    view.update(cx, |v, cx| {
        v.export_to(path.clone(), ExportFormat::Screen, cx)
    })
    .detach();
    run_until(cx, "the screen export", |cx| {
        view.read_with(cx, |v, _| {
            v.notice()
                .is_some_and(|notice| notice.text.contains("screen.txt"))
        })
    });
    let exported = std::fs::read_to_string(&path).unwrap();
    let grid: String = menu
        .screen_text()
        .iter()
        .map(|row| format!("{row}\n"))
        .collect();
    assert_eq!(exported, grid);
    assert!(exported.contains("     Boot from network (TFTP)\n"));

    // The text export still reads the log.
    let log_path = dir.join("log.txt");
    view.update(cx, |v, cx| {
        v.export_to(log_path.clone(), ExportFormat::Text, cx)
    })
    .detach();
    run_until(cx, "the text export", |cx| {
        view.read_with(cx, |v, _| {
            v.notice()
                .is_some_and(|notice| notice.text.contains("log.txt"))
        })
    });
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.starts_with("Connected to "), "{log:?}");

    // The toolbar's button switches back: the log's lines again, still all there.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("emulation", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Monitor);
    assert!(view.read_with(cx, |v, _| v.vt_snapshot().is_none()));
    let lines = displayed(cx, &view);
    assert!(
        lines[0].text.starts_with("Connected to "),
        "{:?}",
        lines[0].text
    );
    assert!(
        view.read_with(cx, |v, _| v.snapshot().end()) >= monitor_end,
        "nothing was lost"
    );
}

fn focus_terminal(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<SessionView>) {
    cx.update_window(window, |_, window, cx| {
        let handle = view.read(cx).terminal().focus_handle(cx);
        window.focus(&handle, cx);
        window.render_frame(cx);
    })
    .unwrap();
}

/// Prints a login prompt and leaves the cursor after it.
struct Prompt;

impl SimDevice for Prompt {
    fn name(&self) -> &str {
        "prompt"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(b"login: ");
    }

    fn on_receive(&mut self, _: &[u8], _: &mut dyn DeviceOutput) {}
}

/// The painted quads inside the terminal: solid ones of `color`, and outlines of it.
fn cursor_quads(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SessionView>,
    color: Hsla,
) -> (Vec<Bounds<Pixels>>, Vec<Bounds<Pixels>>) {
    let bounds = view.read_with(cx, |v, cx| {
        v.terminal().read(cx).scroll_handle().state().bounds
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let scale = window.scale_factor();
        let unscale = |b: Bounds<ScaledPixels>| {
            Bounds::new(
                point(px(b.origin.x.0 / scale), px(b.origin.y.0 / scale)),
                size(px(b.size.width.0 / scale), px(b.size.height.0 / scale)),
            )
        };
        let inside = |b: &Bounds<Pixels>| bounds.contains(&b.center());
        let quads = window.painted_quads();
        let solid = quads
            .iter()
            .filter(|quad| quad.background.as_solid() == Some(color))
            .map(|quad| unscale(quad.bounds))
            .filter(inside)
            .collect();
        // The scene keeps a bordered quad per edge, so an outline can show more than once.
        let mut outlined: Vec<Bounds<Pixels>> = quads
            .iter()
            .filter(|quad| quad.border_color == color && quad.background.as_solid() != Some(color))
            .map(|quad| unscale(quad.bounds))
            .filter(inside)
            .collect();
        outlined.dedup();
        (solid, outlined)
    })
    .unwrap()
}

#[gpui_test]
fn the_cursor_is_painted_where_the_screen_puts_it(cx: &mut TestAppContext) {
    let dir = vt_config("vt-cursor");
    let world = SimWorld::empty();
    world.add_virtual("prompt", "Prompt", LinkConfig::unpaced(), || {
        Box::new(Prompt)
    });
    let (window, _workspace, view) = open(cx, &world, &dir, "virtual:prompt", (1280., 800.));
    press(cx, window, keys::TOGGLE_INLINE);
    let snapshot = wait_for_screen(cx, &view, "the prompt", |snap| {
        snap.screen_text()[0] == "login:"
    });
    let cursor = snapshot.cursor().unwrap();
    assert!(cursor.visible);
    assert_eq!((cursor.line, cursor.column), (snapshot.first_visible(), 7));
    settle_size(cx, window, &view);

    let terminal = view.read_with(cx, |v, _| v.terminal().clone());
    let (bounds, metrics, color) = terminal.read_with(cx, |t, _| {
        (
            t.scroll_handle().state().bounds,
            t.cell_metrics().unwrap(),
            t.palette().cursor,
        )
    });
    let (solid, outlined) = cursor_quads(cx, window, &view, color);
    assert_eq!(solid.len(), 1, "one block cursor: {solid:?}");
    assert!(outlined.is_empty());
    let expected = point(
        bounds.left() + PADDING_LEFT + metrics.cell_width * 7.,
        bounds.top(),
    );
    let at = solid[0].origin;
    assert!(
        (at.x - expected.x).abs() < px(0.5) && (at.y - expected.y).abs() < px(0.5),
        "the cursor is at {at:?}, the cell at {expected:?}"
    );
    assert!((solid[0].size.width - metrics.cell_width).abs() < px(0.5));

    // Without the keyboard (command mode focuses the compose bar), an outline.
    press(cx, window, keys::TOGGLE_INLINE);
    let (solid, outlined) = cursor_quads(cx, window, &view, color);
    assert!(solid.is_empty(), "{solid:?}");
    assert_eq!(outlined.len(), 1, "{outlined:?}");
    assert!((outlined[0].origin.x - expected.x).abs() < px(0.5));
}

/// Turns on bracketed paste and application cursor keys, sets a title, and rings the bell
/// whenever it receives `b`. Keeps what it receives.
struct Modes(Arc<Mutex<Vec<u8>>>);

impl SimDevice for Modes {
    fn name(&self) -> &str {
        "modes"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(b"\x1b[?2004h\x1b[?1h\x1b]2;board: ~\x07ready");
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        if bytes == b"b" {
            out.send(b"\x07");
        }
        self.0.lock().extend_from_slice(bytes);
    }
}

#[gpui_test]
fn the_screens_modes_reach_the_keys_the_paste_the_tab_and_the_status_dot(cx: &mut TestAppContext) {
    let dir = vt_config("vt-modes");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heard);
    let world = SimWorld::empty();
    world.add_virtual("modes", "Modes", LinkConfig::unpaced(), move || {
        Box::new(Modes(Arc::clone(&log)))
    });
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:modes", (1280., 800.));
    press(cx, window, keys::TOGGLE_INLINE);
    let snapshot = wait_for_screen(cx, &view, "ready", |snap| snap.screen_text()[0] == "ready");
    assert!(snapshot.modes().app_cursor_keys && snapshot.modes().bracketed_paste);

    // Application cursor keys: Up sends ESC O A; a modified arrow is unchanged.
    press(cx, window, "up");
    press(cx, window, "shift-up");
    wait_heard(cx, &heard, "the arrows", |heard| heard.len() >= 9);
    assert_eq!(heard.lock().as_slice(), b"\x1bOA\x1b[1;2A");

    // A bracketed paste, line breaks as Enter sends them.
    heard.lock().clear();
    view.update(cx, |v, cx| v.paste_text("ls\nexit", cx));
    let pasted = b"\x1b[200~ls\r\nexit\x1b[201~";
    wait_heard(cx, &heard, "the paste", |heard| heard.len() >= pasted.len());
    assert_eq!(heard.lock().as_slice(), pasted);

    // The title goes on the tab.
    let labels = workspace.read_with(cx, |w, cx| w.tab_labels(cx));
    assert_eq!(labels[0].suffix.as_deref(), Some("board: ~"));

    // The bell flashes the status dot once.
    assert!(!view.read_with(cx, |v, _| v.bell_flashing()));
    press(cx, window, "b");
    let deadline = Instant::now() + ENGINE_WAIT;
    while !view.read_with(cx, |v, _| v.bell_flashing()) {
        assert!(Instant::now() < deadline, "no bell");
        std::thread::sleep(Duration::from_millis(1));
        // The view answers a ring at most once a frame.
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();
    }
    cx.executor().advance_clock(BELL_FLASH + FRAME);
    cx.run_until_parked();
    assert!(!view.read_with(cx, |v, _| v.bell_flashing()), "once");
}

#[gpui_test]
fn search_reads_the_log_under_the_screen_and_says_so(cx: &mut TestAppContext) {
    let dir = vt_config("vt-search");
    let (window, _workspace, view) =
        open(cx, &quiet_menu_world(), &dir, "virtual:menu", (1280., 800.));
    wait_for_screen(cx, &view, "the menu", |snap| highlighted(snap) == [2]);
    let terminal = view.read_with(cx, |v, _| v.terminal().clone());
    cx.update_window(window, |_, window, cx| {
        terminal.update(cx, |t, cx| t.deploy_search(window, cx));
        window.input("TFTP", cx);
    })
    .unwrap();
    run_until(cx, "the search", |cx| {
        terminal.read_with(cx, |t, _| {
            let results = t.search_results();
            !results.pending && !results.matches.is_empty()
        })
    });
    // The match is a line of the log, which the screen does not show as such.
    let found = terminal.read_with(cx, |t, _| t.search_results().matches[0].clone());
    let log_line = view
        .read_with(cx, |v, _| v.log_source().line(found.line))
        .expect("a log line");
    assert!(log_line.text.contains("TFTP"));
    assert!(terminal.read_with(cx, |t, _| t.searches_log()));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("terminal-search-note").is_some(),
            "the search bar says it searches the log"
        );
    })
    .unwrap();
}
