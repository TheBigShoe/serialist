//! Screenshots of the real workspace, rendered offscreen with Metal: `just screenshots`.
//!
//! Each shot opens the workspace the way the headless tests do (GPUI's test platform, the
//! simulator's devices behind the real engine, a temporary config directory with the
//! example scripts, and for the shots of `virtual:race` the example RACE plugin, loaded
//! by the real loaders), drives it with the same flows (keys
//! pressed through the window, commands sent from the Commands panel, waits on the
//! engine's state rather than on time), then draws a frame and reads it back from a
//! Metal texture. No window reaches the screen and no screen-recording permission is
//! needed. Text is shaped and rasterized by the platform's text system, as in the app.
//!
//! The context is gpui's `HeadlessAppContext`: the test platform the unit tests run on,
//! with the platform's text system and a headless Metal renderer behind each window, so
//! `capture_screenshot` has pixels to read. Windows are 2x, like the test platform's
//! (and a Retina display's): a 1440x900 window is a 2880x1800 PNG.
//!
//! It is an example rather than a test: AppKit, which the text system is reached
//! through, must be set up on the main thread, and a picture is for a person to look
//! at. `cargo test` still builds it, so it keeps up with the UI's API. It only runs on
//! macOS; elsewhere it says so and exits.
//!
//! `cargo run -p serialist-ui --example screenshots -- [--out DIR] [NAME...]` writes to
//! `target/screenshots/` by default; each NAME keeps the shots whose file name contains
//! it (`03`, `race`).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serialist_core::settings::ConfigPaths;
use serialist_core::{CommandRef, Direction, LineId, LineSource, PortId, StyleFlags};
use serialist_sim::{
    AtDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator, LinkConfig,
    MenuDevice, SimWorld,
};
use serialist_ui::config::{self, Config};
use serialist_ui::export::ExportFormat;
use serialist_ui::prelude::gpui_test::TestWindowExt;
use serialist_ui::prelude::*;
use serialist_ui::session_view::FRAME;
use serialist_ui::terminal::DisplayMode;
use serialist_ui::{AppOptions, SessionView, Workspace};

const WIDE: (f32, f32) = (1440., 900.);
const NARROW: (f32, f32) = (1024., 640.);
const DARK: &str = "Serialist Dark";
const LIGHT: &str = "Serialist Light";
/// The simulated RACE device, which the shots that decode connect to.
const RACE_PORT: &str = "virtual:race";

/// Lines the firehose shots send before the picture is taken.
const FIREHOSE_LINES: usize = 200;
/// Bytes of text firehose under the hex view: a few screens of rows.
const HEX_BYTES: u64 = 3000;
/// Frames the Decoded panel holds before a row is selected.
const DECODED_FRAMES: usize = 12;

/// Generous failure bound for engine-driven waits; shots finish far sooner.
const ENGINE_WAIT: Duration = Duration::from_secs(15);

struct Shot {
    file: &'static str,
    /// Window size in logical pixels.
    size: (f32, f32),
    theme: &'static str,
    world: fn() -> SimWorld,
    /// The `--port` flag: opened at startup.
    connect: Option<&'static str>,
    drive: fn(&mut Stage),
}

const SHOTS: &[Shot] = &[
    Shot {
        file: "01-startup.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: None,
        drive: startup,
    },
    Shot {
        file: "02-connected-firehose.png",
        size: WIDE,
        theme: DARK,
        world: ansi_firehose_world,
        connect: Some("virtual:firehose"),
        drive: firehose,
    },
    Shot {
        file: "03-decoded-race.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:race"),
        drive: decoded_race,
    },
    Shot {
        file: "04-commands.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:at"),
        drive: commands,
    },
    Shot {
        file: "05-inline.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:at"),
        drive: inline,
    },
    Shot {
        file: "06-search-hex.png",
        size: WIDE,
        theme: DARK,
        world: hex_firehose_world,
        connect: Some("virtual:firehose"),
        drive: search_hex,
    },
    Shot {
        file: "07-scripts.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:at"),
        drive: scripts,
    },
    Shot {
        file: "08-paused-recording.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:firehose"),
        drive: paused_recording,
    },
    Shot {
        file: "09-light-theme.png",
        size: WIDE,
        theme: LIGHT,
        world: ansi_firehose_world,
        connect: Some("virtual:firehose"),
        drive: firehose,
    },
    Shot {
        file: "10-narrow.png",
        size: NARROW,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:race"),
        drive: decoded_race,
    },
    Shot {
        file: "11-param-prompt.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:at"),
        drive: param_prompt,
    },
    Shot {
        file: "12-palette.png",
        size: WIDE,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:at"),
        drive: palette,
    },
    Shot {
        file: "13-narrow-overflow.png",
        size: NARROW,
        theme: DARK,
        world: SimWorld::new,
        connect: Some("virtual:race"),
        drive: overflow_menu,
    },
    Shot {
        file: "14-vt-menu.png",
        size: WIDE,
        theme: DARK,
        world: menu_world,
        connect: Some("virtual:menu"),
        drive: vt_menu,
    },
];

// --- The shots -----------------------------------------------------------------------

/// A fresh workspace: no session, the simulator's devices listed.
fn startup(stage: &mut Stage) {
    let devices = stage
        .workspace
        .read_with(&stage.cx, |w, _| w.devices().clone());
    let ports: Vec<PortId> = stage
        .world
        .port_source()
        .snapshot()
        .into_iter()
        .map(|info| info.id)
        .collect();
    run_until(&mut stage.cx, "the simulator's ports to be listed", |cx| {
        devices.read_with(cx, |d, _| {
            ports
                .iter()
                .all(|id| d.list().get(id).is_some_and(|entry| entry.present))
        })
    });
}

/// `virtual:firehose` sending ANSI-coloured log lines, stopped after
/// [`FIREHOSE_LINES`] so the picture shows a settled stream, following the tail.
fn firehose(stage: &mut Stage) {
    let view = stage.session();
    let total = bytes_for_lines(FirehoseContent::Ansi, FIREHOSE_LINES);
    wait_for_received(&mut stage.cx, &view, total);
    // The notice line and every received one.
    run_until(&mut stage.cx, "every line to be shown", |cx| {
        view.read_with(cx, |v, _| v.snapshot().end().0 > FIREHOSE_LINES as u64)
    });
    if !view.read_with(&stage.cx, |v, cx| v.is_following_tail(cx)) {
        eprintln!("  note: the terminal is not following the tail");
    }
}

/// `virtual:race` decoded by the example `airoha-race` plugin (installed into the shot's
/// config directory, and named by the device profile in the settings), with the bundled
/// RACE version command sent from the Commands panel and its response selected in the
/// Decoded panel.
fn decoded_race(stage: &mut Stage) {
    stage.session();
    let (commands, decoded) = stage.workspace.read_with(&stage.cx, |w, _| {
        (w.commands().clone(), w.decoded().clone())
    });
    commands.update(&mut stage.cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "RACE", "RACE version"), cx);
    });
    run_until(
        &mut stage.cx,
        "a dozen frames and the version response",
        |cx| {
            decoded.read_with(cx, |panel, cx| {
                let frames = panel.frames(cx);
                frames.len() >= DECODED_FRAMES && frames.iter().any(|f| f.kind == "response")
            })
        },
    );
    let row = decoded
        .read_with(&stage.cx, |panel, cx| {
            panel.frames(cx).iter().position(|f| f.kind == "response")
        })
        .expect("the response row");
    // As a click on the row does: the terminal shows the frame.
    decoded.update(&mut stage.cx, |panel, cx| panel.select_row(row, cx));
    stage.cx.run_until_parked();
}

/// The Commands panel with the bundled examples, after sending ATI from it, and the
/// command form open on the Echo example (a copy, since the examples are read-only).
fn commands(stage: &mut Stage) {
    let view = stage.session();
    let commands = stage
        .workspace
        .read_with(&stage.cx, |w, _| w.commands().clone());
    commands.update(&mut stage.cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "Basics", "ATI"), cx);
    });
    run_until(&mut stage.cx, "the modem's identity and OK", |cx| {
        has_rx_line(cx, &view, AtDevice::DEFAULT_IDENTITY) && has_rx_line(cx, &view, "OK")
    });
    let echo = CommandRef::new("AT basics", "With parameters", "Echo");
    stage
        .cx
        .update_window(stage.window, |_, window, cx| {
            commands.update(cx, |panel, cx| {
                panel.select(echo.clone(), cx);
                panel.edit(&echo, window, cx);
            });
        })
        .expect("the window is open");
    run_until(&mut stage.cx, "the command form", |cx| {
        commands.read_with(cx, |panel, _| panel.editor().is_some())
    });
}

/// The parameter prompt a saved command with a parameter (the Echo example) opens when
/// it is sent from the Commands panel.
fn param_prompt(stage: &mut Stage) {
    stage.session();
    let (commands, workspace) = (
        stage
            .workspace
            .read_with(&stage.cx, |w, _| w.commands().clone()),
        stage.workspace.clone(),
    );
    commands.update(&mut stage.cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "With parameters", "Echo"), cx);
    });
    run_until(&mut stage.cx, "the parameter prompt", |cx| {
        workspace.read_with(cx, |w, _| w.param_prompt().is_some())
    });
}

/// The command palette, opened with its key and narrowed to the toggles.
fn palette(stage: &mut Stage) {
    stage.session();
    stage.press("cmd-shift-p");
    stage
        .cx
        .update_window(stage.window, |_, window, cx| window.input("toggle", cx))
        .expect("the window is open");
    stage.cx.run_until_parked();
}

/// `virtual:race`, decoded, in a 1024 px window: the right dock on its rail, and the
/// toolbar's overflow menu open on what did not fit.
fn overflow_menu(stage: &mut Stage) {
    stage.session();
    stage
        .cx
        .update_window(stage.window, |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
            if window.try_find("toolbar-overflow").is_some() {
                window.click("toolbar-overflow", cx);
            } else {
                eprintln!("  note: everything fits; there is no overflow menu");
            }
        })
        .expect("the window is open");
    stage.cx.run_until_parked();
}

/// Inline mode on `virtual:at`: `ati` and Enter typed key by key, and the reply.
fn inline(stage: &mut Stage) {
    let view = stage.session();
    // The mode key, from the compose bar the connect focused.
    stage.press("cmd-i");
    for key in ["a", "t", "i", "enter"] {
        stage.press(key);
    }
    run_until(&mut stage.cx, "the modem's identity and OK", |cx| {
        has_rx_line(cx, &view, AtDevice::DEFAULT_IDENTITY) && has_rx_line(cx, &view, "OK")
    });
    run_until(&mut stage.cx, "ati and CRLF counted out", |cx| {
        view.read_with(cx, |v, _| v.stats().tx_bytes == 5)
    });
}

/// `virtual:menu` in VT mode (its device profile in the shots' settings says so), in
/// inline mode, with Down pressed once: the boot menu drawn on a terminal screen, the
/// second item highlighted and the cursor after the status row.
fn vt_menu(stage: &mut Stage) {
    let view = stage.session();
    stage.press("cmd-i");
    let highlighted = |cx: &HeadlessAppContext, row: usize| {
        view.read_with(cx, |v, _| {
            v.vt_snapshot()
                .and_then(|snapshot| snapshot.visible_line(row).cloned())
                .is_some_and(|line| {
                    line.runs
                        .iter()
                        .any(|run| run.style.flags.contains(StyleFlags::INVERSE))
                })
        })
    };
    run_until(&mut stage.cx, "the menu on the screen", |cx| {
        highlighted(cx, MenuDevice::FIRST_ITEM_ROW - 1)
    });
    stage.press("down");
    run_until(&mut stage.cx, "the second item highlighted", |cx| {
        highlighted(cx, MenuDevice::FIRST_ITEM_ROW)
    });
}

/// The hex view of a short text firehose with the search bar open on line endings.
fn search_hex(stage: &mut Stage) {
    let view = stage.session();
    wait_for_received(&mut stage.cx, &view, HEX_BYTES);
    let terminal = view.read_with(&stage.cx, |v, _| v.terminal().clone());
    stage
        .cx
        .update_window(stage.window, |_, window, cx| {
            let handle = terminal.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        })
        .expect("the window is open");
    stage.press("alt-h");
    assert_eq!(
        terminal.read_with(&stage.cx, |t, _| t.display_mode()),
        DisplayMode::Hex
    );
    stage
        .cx
        .update_window(stage.window, |_, window, cx| {
            terminal.update(cx, |t, cx| t.deploy_search(window, cx));
            window.input("0d 0a", cx);
        })
        .expect("the window is open");
    run_until(&mut stage.cx, "the search's matches", |cx| {
        terminal.read_with(cx, |t, _| {
            let results = t.search_results();
            !results.pending && !results.matches.is_empty() && results.active.is_some()
        })
    });
}

/// The Script console after `version_probe.lua` ran against `virtual:at`.
fn scripts(stage: &mut Stage) {
    stage.session();
    let console = stage
        .workspace
        .read_with(&stage.cx, |w, _| w.console().clone());
    run_until(&mut stage.cx, "the example scripts to be listed", |cx| {
        console.read_with(cx, |c, _| {
            c.scripts()
                .iter()
                .any(|entry| entry.relative == "version_probe.lua")
        })
    });
    stage
        .cx
        .update_window(stage.window, |_, _, cx| {
            console.update(cx, |c, cx| c.run("version_probe.lua", cx));
        })
        .expect("the window is open");
    run_until(&mut stage.cx, "version_probe.lua to finish", |cx| {
        console.read_with(cx, |c, _| {
            c.lines()
                .iter()
                .any(|line| line.text.starts_with("\u{2713} version_probe.lua finished"))
        })
    });
}

/// The built-in (paced) text firehose, recorded to a file, paused with data arriving
/// behind the pause, and exported as text while paused.
fn paused_recording(stage: &mut Stage) {
    let view = stage.session();
    let recording = stage.dir.join("capture.bin");
    view.update(&mut stage.cx, |v, cx| v.start_recording(recording, cx));
    run_until(&mut stage.cx, "the recording file to open", |cx| {
        view.read_with(cx, |v, _| v.recording().is_some_and(|r| r.stats.is_some()))
    });
    run_until(&mut stage.cx, "a screenful of lines", |cx| {
        view.read_with(cx, |v, _| v.snapshot().end().0 >= 120)
    });
    view.update(&mut stage.cx, |v, cx| v.pause(cx));
    run_until(&mut stage.cx, "lines behind the pause", |cx| {
        view.read_with(cx, |v, _| {
            v.pause_mark()
                .is_some_and(|mark| mark.since(&v.snapshot().stats()).0 >= 20)
        })
    });
    let export = stage.dir.join("session.txt");
    view.update(&mut stage.cx, |v, cx| {
        v.export_to(export, ExportFormat::Text, cx)
    })
    .detach();
    run_until(&mut stage.cx, "the export notice", |cx| {
        view.read_with(cx, |v, _| {
            v.notice()
                .is_some_and(|notice| notice.text.contains("session.txt"))
        })
    });
}

// --- Simulated worlds ----------------------------------------------------------------

/// The built-in devices, with the boot menu leaving its cursor on (U-Boot hides it) so
/// the picture shows one.
fn menu_world() -> SimWorld {
    let world = SimWorld::new();
    world.add_virtual(
        SimWorld::MENU,
        "Boot menu (virtual)",
        LinkConfig::default(),
        || Box::new(MenuDevice::new().with_cursor_shown(true)),
    );
    world
}

/// The built-in devices, with `virtual:firehose` sending ANSI-coloured lines on an
/// unpaced link and stopping after [`FIREHOSE_LINES`] of them.
fn ansi_firehose_world() -> SimWorld {
    capped_firehose_world(
        FirehoseContent::Ansi,
        bytes_for_lines(FirehoseContent::Ansi, FIREHOSE_LINES),
    )
}

/// The built-in devices, with `virtual:firehose` sending [`HEX_BYTES`] of text.
fn hex_firehose_world() -> SimWorld {
    capped_firehose_world(FirehoseContent::Text, HEX_BYTES)
}

fn capped_firehose_world(content: FirehoseContent, total: u64) -> SimWorld {
    let world = SimWorld::new();
    world.add_virtual(
        SimWorld::FIREHOSE,
        "Firehose (virtual)",
        LinkConfig::unpaced(),
        move || {
            Box::new(FirehoseDevice::new(
                FirehoseConfig::new(content).with_total(total),
            ))
        },
    );
    world
}

/// The length of the first `lines` records a firehose of `content` sends (seed 0, the
/// device's default), line endings included.
fn bytes_for_lines(content: FirehoseContent, lines: usize) -> u64 {
    let mut stream = Vec::new();
    FirehoseGenerator::new(content, 0).fill(&mut stream, lines * 1024);
    let end = stream
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(lines - 1)
        .map(|(ix, _)| ix + 1)
        .expect("records are shorter than 1 KiB");
    end as u64
}

// --- Driving the workspace -----------------------------------------------------------

/// One shot's app, window and workspace, with its simulator and config directory.
/// Fields drop in order: the workspace handle, then the app (which flushes the history
/// into the config directory), then the simulator and the directory.
struct Stage {
    workspace: Entity<Workspace>,
    window: AnyWindowHandle,
    cx: HeadlessAppContext,
    world: SimWorld,
    dir: TempDir,
}

impl Stage {
    fn open(shot: &Shot, text_system: Arc<dyn PlatformTextSystem>) -> Self {
        let dir = TempDir::new(shot.file.trim_end_matches(".png"));
        let paths = ConfigPaths::new(dir.path());
        std::fs::write(&paths.settings, settings(shot.theme)).expect("write settings.json");
        paths
            .ensure_example_scripts()
            .expect("write the example scripts");
        // The app ships with no decoder active: a shot of the RACE device decoding
        // installs the example plugin first, as "Install example plugin" does. The others
        // show a fresh configuration, where the profile's plugin is not installed.
        if shot.connect == Some(RACE_PORT) {
            let example =
                serialist_plugins::example_plugin("airoha-race").expect("the bundled example");
            paths
                .install_example_plugin(example)
                .expect("install the example plugin");
        }
        let world = (shot.world)();

        // The bundled icon set, as in the app, so SVG icons (a select's chevron, a
        // dialog's close button) draw. Keep this in step with `crates/serialist/src/main.rs`.
        let mut cx = HeadlessAppContext::with_platform(
            text_system,
            Arc::new(Assets),
            platform::current_headless_renderer,
        );
        // The engine's ingest thread rings the session view's doorbell from its own
        // thread, which the test scheduler otherwise treats as nondeterminism.
        cx.allow_parking();
        let options = AppOptions {
            port_source: world.port_source(),
            transport_factory: world.transport_factory(),
            baud: None,
            select_port: shot.connect.map(PortId::new),
            open_ports: shot.connect.map(PortId::new).into_iter().collect(),
            store: None,
        };
        let (width, height) = shot.size;
        let (window, workspace) = cx
            .update(|cx| {
                serialist_ui::init(cx);
                // Animations (the dialog's fade and scale) show their end state, so the
                // picture does not depend on how much real time has passed.
                cx.set_reduce_motion(true);
                config::install(Config::load(paths, true), cx);
                kit_open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Point::default(),
                            size: size(px(width), px(height)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    move |window, cx| cx.new(|cx| Workspace::new(options, window, cx)),
                )
            })
            .expect("open the workspace window");
        cx.run_until_parked();
        Self {
            workspace,
            window,
            cx,
            world,
            dir,
        }
    }

    /// Wait for the session opened at startup to show ingest's `Connected to …` notice.
    fn session(&mut self) -> Entity<SessionView> {
        let workspace = self.workspace.clone();
        let session_of =
            |cx: &HeadlessAppContext| workspace.read_with(cx, |w, _| w.session().cloned());
        run_until(&mut self.cx, "the session to connect", |cx| {
            session_of(cx).is_some_and(|view| {
                view.read_with(cx, |v, _| {
                    v.snapshot()
                        .line(LineId::ZERO)
                        .is_some_and(|line| line.text.starts_with("Connected to "))
                })
            })
        });
        session_of(&self.cx).expect("a session view")
    }

    /// Press `keys` in the window, as a user would.
    fn press(&mut self, keys: &str) {
        self.cx
            .update_window(self.window, |_, window, cx| window.press(keys, cx))
            .expect("the window is open");
        self.cx.run_until_parked();
    }

    /// Draw a few frames so the last wake's repaint and any scrolling done in layout
    /// settle, then read the window back and write it to `path` as a PNG.
    fn capture(&mut self, path: &Path) -> Result<(u32, u32), String> {
        for _ in 0..3 {
            self.draw()?;
            self.cx.run_until_parked();
            self.cx.advance_clock(FRAME);
        }
        self.cx.run_until_parked();
        self.draw()?;
        let image = self
            .cx
            .capture_screenshot(self.window)
            .map_err(|error| format!("capture_screenshot failed: {error:#}"))?;
        image
            .save(path)
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        Ok((image.width(), image.height()))
    }

    fn draw(&mut self) -> Result<(), String> {
        self.cx
            .update_window(self.window, |_, window, cx| window.render_frame(cx))
            .map_err(|error| format!("the window is gone: {error:#}"))
    }
}

/// Run what is ready, then advance the test clock a frame, until `done`. The engine runs
/// on real threads, so the bound is in real time; nothing sleeps.
fn run_until(
    cx: &mut HeadlessAppContext,
    what: &str,
    mut done: impl FnMut(&mut HeadlessAppContext) -> bool,
) {
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ENGINE_WAIT:?} waiting for {what}"
        );
        cx.advance_clock(FRAME);
    }
}

/// Wait until the view's snapshot holds `total` received bytes.
fn wait_for_received(cx: &mut HeadlessAppContext, view: &Entity<SessionView>, total: u64) {
    run_until(cx, &format!("{total} received bytes"), |cx| {
        view.read_with(cx, |v, _| v.snapshot().raw_range().end >= total)
    });
}

/// A received line with exactly this text among the newest 1000 displayed.
fn has_rx_line(cx: &mut HeadlessAppContext, view: &Entity<SessionView>, text: &str) -> bool {
    view.read_with(cx, |v, cx| {
        let terminal = v.terminal().read(cx);
        let span = terminal.displayed_span();
        let from = LineId(span.end.0.saturating_sub(1000)).max(span.first);
        let mut lines = Vec::new();
        terminal.source().lines(from..span.end, &mut lines);
        lines
            .iter()
            .any(|line| line.direction == Direction::Rx && line.text == text)
    })
}

/// The settings every shot loads: the theme, a device profile that decodes
/// `virtual:race` with the `airoha-race` plugin (installed for the shots that open it),
/// and one that opens `virtual:menu` in VT mode.
fn settings(theme: &str) -> String {
    format!(
        r#"{{
  "theme": "{theme}",
  "devices": [
    {{ "name": "Airoha RACE board", "match": {{ "path": "virtual:race" }}, "plugin": "airoha-race" }},
    {{ "name": "Boot menu", "match": {{ "path": "virtual:menu" }}, "emulation": "vt" }}
  ]
}}
"#
    )
}

/// A scratch directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "serialist-screenshots-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a scratch directory");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// --- Running -------------------------------------------------------------------------

struct Args {
    out: PathBuf,
    names: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut out = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/screenshots");
    let mut names = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().ok_or("--out needs a directory")?.into(),
            flag if flag.starts_with('-') => return Err(format!("unknown flag {flag}")),
            _ => names.push(arg),
        }
    }
    Ok(Args { out, names })
}

fn take(
    shot: &Shot,
    text_system: Arc<dyn PlatformTextSystem>,
    path: &Path,
) -> Result<(u32, u32), String> {
    let mut stage = Stage::open(shot, text_system);
    (shot.drive)(&mut stage);
    stage.capture(path)
}

fn run(args: Args) -> bool {
    let shots: Vec<&Shot> = SHOTS
        .iter()
        .filter(|shot| {
            args.names.is_empty() || args.names.iter().any(|n| shot.file.contains(n.as_str()))
        })
        .collect();
    if shots.is_empty() {
        eprintln!("screenshots: no shot matches {:?}", args.names);
        return false;
    }
    std::fs::create_dir_all(&args.out).expect("create the output directory");
    if args.names.is_empty() {
        // A full run replaces every picture, so none is left over from an older one.
        for entry in std::fs::read_dir(&args.out).into_iter().flatten().flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "png") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let out = args.out.canonicalize().unwrap_or(args.out);

    // One platform for the whole run; each shot gets its own app over its text system.
    let platform = platform::current_platform(true);
    let text_system = platform.text_system();
    let started = Instant::now();
    let mut failed = Vec::new();
    for shot in shots {
        let path = out.join(shot.file);
        let began = Instant::now();
        let text_system = text_system.clone();
        match catch_unwind(AssertUnwindSafe(|| take(shot, text_system, &path))) {
            Ok(Ok((width, height))) => eprintln!(
                "  {:<28} {width}x{height}  {:.1}s",
                shot.file,
                began.elapsed().as_secs_f64()
            ),
            Ok(Err(error)) => {
                eprintln!("  {:<28} FAILED: {error}", shot.file);
                failed.push(shot.file);
            }
            Err(_) => {
                eprintln!("  {:<28} FAILED (panicked, see above)", shot.file);
                failed.push(shot.file);
            }
        }
    }
    eprintln!(
        "screenshots: wrote to {} in {:.1}s",
        out.display(),
        started.elapsed().as_secs_f64()
    );
    if !failed.is_empty() {
        eprintln!(
            "screenshots: {} failed: {}",
            failed.len(),
            failed.join(", ")
        );
    }
    failed.is_empty()
}

fn main() {
    if !cfg!(target_os = "macos") {
        eprintln!("screenshots: skipped; offscreen capture uses the macOS text system and Metal");
        return;
    }
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("screenshots: {error}");
            std::process::exit(2);
        }
    };
    if !run(args) {
        std::process::exit(1);
    }
}
