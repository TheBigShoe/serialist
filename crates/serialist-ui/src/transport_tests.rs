//! TCP and replay sessions in the window: "Connect to TCP…" and "Open capture…", the
//! tab title, the status line, the toolbar's settings control and the replay speed menu,
//! and what a restored `tcp:` or `replay:` tab does at the next start.
//!
//! Each test runs the real engine in a headless window. Ports open through a
//! [`RoutingTransportFactory`] with the schemes the binary registers (`tcp:` through
//! [`TcpTransportFactory`] against a loopback listener the test owns, `replay:` through
//! a [`ReplayTransportFactory`] on captures written to a temporary directory, `virtual:`
//! through the simulator), so nothing here needs hardware.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serialist_core::settings::ConfigPaths;
use serialist_core::{
    Direction, Emulation, LineSource, PortId, REPLAY_SCHEME, ReplayAddress, ReplayEnd,
    ReplayOptions, ReplaySpeed, ReplayTransportFactory, RoutingTransportFactory, SerialConfig,
    TCP_SCHEME, TcpAddress, TcpTransportFactory, TimingWriter, VIRTUAL_SCHEME, timing_path,
};
use serialist_sim::SimWorld;

use crate::actions::{ConnectTcp, OpenCapture, keys};
use crate::config::{self, Config};
use crate::dialog_footer;
use crate::inline::Mode;
use crate::prelude::*;
use crate::session_state::{SavedMode, SavedTab, SessionState, state_path};
use crate::session_view::SessionView;
use crate::status::{LinkKind, StatusLine};
use crate::tabs::TabState;
use crate::test_support::{
    TestDir, allow_engine_threads, open_test_window, run_until, step, wait_connected,
    wait_for_lines,
};
use crate::workspace::{AppOptions, Workspace};

/// A workspace whose router maps `tcp:`, `replay:` and `virtual:`, with the replay
/// factory handed over as the binary's wiring hands it.
struct Env {
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    replay: Arc<ReplayTransportFactory>,
    _world: SimWorld,
}

fn open(cx: &mut TestAppContext) -> Env {
    open_in(cx, None)
}

/// [`open`] with the configuration under `config_dir` loaded before the workspace is
/// built, so the tabs its `state.json` saved are restored.
fn open_in(cx: &mut TestAppContext, config_dir: Option<&TestDir>) -> Env {
    allow_engine_threads(cx);
    let world = SimWorld::new();
    let replay = Arc::new(ReplayTransportFactory::new());
    let router = RoutingTransportFactory::new(world.transport_factory())
        .with_scheme(VIRTUAL_SCHEME, world.transport_factory())
        .with_scheme(TCP_SCHEME, Arc::new(TcpTransportFactory::new()))
        .with_scheme(REPLAY_SCHEME, replay.clone());
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: Arc::new(router),
        baud: None,
        select_port: None,
        open_ports: Vec::new(),
        store: None,
        replay: Some(replay.clone()),
    };
    let paths = config_dir.map(|dir| ConfigPaths::new(dir.path()));
    let (window, workspace) = open_test_window(cx, move |window, cx| {
        if let Some(paths) = paths {
            config::install(Config::load(paths, false), cx);
        }
        Workspace::new(options, window, cx)
    });
    Env {
        window,
        workspace,
        replay,
        _world: world,
    }
}

/// A loopback listener that writes `banner` to the first client and then holds the
/// connection until the returned sender is dropped.
struct Bridge {
    port: u16,
    hold: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Bridge {
    fn start(banner: &'static [u8]) -> Self {
        Self::start_on(0, banner)
    }

    /// [`Self::start`] on `port` (0 for any free one).
    fn start_on(port: u16, banner: &'static [u8]) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind a loopback port");
        let port = listener.local_addr().unwrap().port();
        let (hold, held) = channel::<()>();
        let thread = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream.write_all(banner).ok();
            // Until the test drops its end.
            held.recv().ok();
        });
        Self {
            port,
            hold: Some(hold),
            thread: Some(thread),
        }
    }

    fn address(&self) -> TcpAddress {
        TcpAddress::new("127.0.0.1", self.port)
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Let go of the connection, and, in case nobody ever connected, wake `accept`
        // with a connection of our own.
        self.hold.take();
        std::net::TcpStream::connect(("127.0.0.1", self.port)).ok();
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

/// A loopback port nothing listens on: bound to get a free one, then let go.
fn closed_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
    listener.local_addr().unwrap().port()
}

/// A capture in `dir`: the chunks as `(milliseconds since the start, bytes)`, and, with
/// `sidecar`, the timing file beside it.
fn write_capture(dir: &TestDir, name: &str, chunks: &[(u64, &[u8])], sidecar: bool) -> PathBuf {
    let raw = dir.join(name);
    let bytes: Vec<u8> = chunks
        .iter()
        .flat_map(|(_, chunk)| chunk.to_vec())
        .collect();
    std::fs::write(&raw, bytes).unwrap();
    if sidecar {
        let origin = Instant::now();
        let mut timing = TimingWriter::create(&timing_path(&raw), origin).unwrap();
        for (ms, chunk) in chunks {
            timing
                .rx(origin + Duration::from_millis(*ms), chunk.len())
                .unwrap();
        }
        timing.finish().unwrap();
    }
    raw
}

/// A short boot log, three chunks over 20 ms.
fn boot_log(dir: &TestDir) -> PathBuf {
    write_capture(
        dir,
        "boot.bin",
        &[
            (0, b"U-Boot 2024.01\r\n"),
            (10, b"DRAM: 512 MiB\r\n"),
            (20, b"Hit any key to stop autoboot\r\n"),
        ],
        true,
    )
}

/// Whether the control `id` is on screen after a couple of frames.
fn drawn(cx: &mut TestAppContext, env: &Env, id: &'static str) -> bool {
    cx.update_window(env.window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .unwrap()
}

fn view_of(cx: &mut TestAppContext, env: &Env) -> Entity<SessionView> {
    env.workspace
        .read_with(cx, |w, _| w.session().cloned())
        .expect("an active session")
}

fn status(cx: &mut TestAppContext, env: &Env) -> StatusLine {
    env.workspace
        .read_with(cx, |w, cx| w.status_line(cx))
        .expect("a status line")
}

fn titles(cx: &mut TestAppContext, env: &Env) -> Vec<String> {
    env.workspace
        .read_with(cx, |w, cx| w.tab_labels(cx))
        .into_iter()
        .map(|label| label.title)
        .collect()
}

fn dialog_is_open(cx: &mut TestAppContext, window: AnyWindowHandle) -> bool {
    cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
        .unwrap()
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
    cx.run_until_parked();
}

/// Wait until the session in the active tab has shown `text` as a received line.
fn wait_for_rx_line(cx: &mut TestAppContext, view: &Entity<SessionView>, text: &str) {
    wait_for_lines(cx, view, &format!("the line {text:?}"), |lines| {
        lines
            .iter()
            .any(|line| line.direction == Direction::Rx && line.text == text)
    });
}

#[gpui_test]
fn connect_tcp_opens_a_tab_on_a_loopback_listener(cx: &mut TestAppContext) {
    let env = open(cx);
    let bridge = Bridge::start(b"ESP-Link ready\r\nuptime 3 s\r\n");
    let address = bridge.address();
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.connect_tcp(address.clone(), window, cx));
    })
    .unwrap();

    let view = wait_connected(cx, &env.workspace);
    assert_eq!(titles(cx, &env), [format!("127.0.0.1:{}", bridge.port)]);
    assert_eq!(
        env.workspace.read_with(cx, |w, _| w.window_title()),
        format!("127.0.0.1:{} \u{2014} Serialist", bridge.port)
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.port().to_string()),
        format!("tcp:127.0.0.1:{}", bridge.port),
        "the tab's port is the canonical id"
    );

    // What the listener wrote reaches the scrollback.
    wait_for_rx_line(cx, &view, "ESP-Link ready");
    wait_for_rx_line(cx, &view, "uptime 3 s");

    // The status line names the endpoint and says nothing about line settings.
    let line = status(cx, &env);
    assert_eq!(line.title, format!("tcp:127.0.0.1:{}", bridge.port));
    assert_eq!(line.settings, None);
    assert!(!line.title.contains("8N1"), "{}", line.title);
    assert_eq!(line.state, "Connected");
}

#[gpui_test]
fn the_tcp_prompt_rejects_a_bad_address(cx: &mut TestAppContext) {
    let env = open(cx);
    let bridge = Bridge::start(b"hello\r\n");

    cx.dispatch_action(env.window, ConnectTcp);
    cx.run_until_parked();
    let prompt = env
        .workspace
        .read_with(cx, |w, _| w.tcp_prompt().cloned())
        .expect("the dialog's form");
    assert!(dialog_is_open(cx, env.window));

    // A host with no port: the error shows under the field and the dialog stays.
    cx.update_window(env.window, |_, window, cx| window.input("host", cx))
        .unwrap();
    assert_eq!(
        prompt.read_with(cx, |p, cx| p.text(cx)),
        "host",
        "the field has the focus"
    );
    press(cx, env.window, "enter");
    assert!(
        dialog_is_open(cx, env.window),
        "an invalid entry stays open"
    );
    let error = prompt
        .read_with(cx, |p, _| p.error().map(str::to_owned))
        .expect("the error is shown");
    assert!(
        error.contains("a tcp port id is tcp:<host>:<port>"),
        "{error}"
    );
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 0);

    // Typing again takes the complaint away; the dialog's own Connect button refuses a
    // bad entry too.
    cx.update_window(env.window, |_, window, cx| window.input("x", cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        prompt.read_with(cx, |p, _| p.error().map(str::to_owned)),
        None
    );
    cx.update_window(env.window, |_, window, cx| {
        prompt.update(cx, |p, cx| p.set_text("host:99999", window, cx));
    })
    .unwrap();
    let mut last = None;
    run_until(cx, "the Connect button to hold still", |cx| {
        let now = cx
            .update_window(env.window, |_, window, cx| {
                window.render_frame(cx);
                window
                    .try_find(dialog_footer::OK_BUTTON)
                    .map(|b| b.bounds())
            })
            .unwrap();
        let at_rest = now.is_some() && now == last;
        last = now;
        at_rest
    });
    cx.update_window(env.window, |_, window, cx| {
        window.click(dialog_footer::OK_BUTTON, cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(dialog_is_open(cx, env.window));
    assert!(
        prompt
            .read_with(cx, |p, _| p.error().map(str::to_owned))
            .is_some_and(|error| error.contains("99999")),
    );

    // A good one connects, and Enter does it.
    let endpoint = format!("127.0.0.1:{}", bridge.port);
    cx.update_window(env.window, |_, window, cx| {
        prompt.update(cx, |p, cx| p.set_text(&endpoint, window, cx));
    })
    .unwrap();
    press(cx, env.window, "enter");
    run_until(cx, "the dialog to close", |cx| {
        !dialog_is_open(cx, env.window)
    });
    assert!(env.workspace.read_with(cx, |w, _| w.tcp_prompt().is_none()));
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(titles(cx, &env), [endpoint]);
    wait_for_rx_line(cx, &view, "hello");

    // Escape cancels a prompt and opens nothing.
    cx.dispatch_action(env.window, ConnectTcp);
    cx.run_until_parked();
    assert!(dialog_is_open(cx, env.window));
    press(cx, env.window, "escape");
    run_until(cx, "the dialog to close", |cx| {
        !dialog_is_open(cx, env.window)
    });
    assert!(env.workspace.read_with(cx, |w, _| w.tcp_prompt().is_none()));
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);
}

#[gpui_test]
fn a_pasted_tcp_id_connects_like_host_and_port(cx: &mut TestAppContext) {
    let env = open(cx);
    let bridge = Bridge::start(b"hello\r\n");
    cx.dispatch_action(env.window, ConnectTcp);
    cx.run_until_parked();
    let prompt = env
        .workspace
        .read_with(cx, |w, _| w.tcp_prompt().cloned())
        .expect("the dialog's form");
    let pasted = format!("tcp:127.0.0.1:{}", bridge.port);
    cx.update_window(env.window, |_, window, cx| {
        prompt.update(cx, |p, cx| p.set_text(&pasted, window, cx));
    })
    .unwrap();
    press(cx, env.window, "enter");
    wait_connected(cx, &env.workspace);
    assert_eq!(titles(cx, &env), [format!("127.0.0.1:{}", bridge.port)]);
}

#[gpui_test]
fn open_capture_prompts_for_a_file(cx: &mut TestAppContext) {
    let env = open(cx);
    let dir = TestDir::new("transport-open-capture");
    let capture = boot_log(&dir);

    cx.dispatch_action(env.window, OpenCapture);
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths(), "Open capture asks for a file");

    // Cancelled: nothing opens.
    cx.simulate_path_prompt_response(|_| None);
    cx.run_until_parked();
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 0);

    // A file: a tab titled with its name, which plays.
    cx.dispatch_action(env.window, OpenCapture);
    cx.run_until_parked();
    assert!(cx.did_prompt_for_paths());
    let chosen = capture.clone();
    cx.simulate_path_prompt_response(move |options| {
        assert!(options.files && !options.directories && !options.multiple);
        Some(vec![chosen])
    });
    cx.run_until_parked();
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(titles(cx, &env), ["boot.bin"]);
    assert_eq!(
        env.workspace.read_with(cx, |w, _| w.window_title()),
        "boot.bin \u{2014} Serialist"
    );
    assert_eq!(view.read_with(cx, |v, _| v.link()), LinkKind::Replay);
    wait_for_rx_line(cx, &view, "U-Boot 2024.01");
    wait_for_rx_line(cx, &view, "DRAM: 512 MiB");
    wait_for_rx_line(cx, &view, "Hit any key to stop autoboot");

    // The status line names the capture and its speed, and no line settings: the
    // capture has its own timing.
    let line = status(cx, &env);
    assert_eq!(line.title, "replay:boot.bin (1x)");
    assert_eq!(line.settings, None);

    // Playing the last byte ends the session, which is not a lost connection.
    run_until(cx, "the replay to end", |cx| {
        view.read_with(cx, |v, _| v.state().is_disconnected())
    });
    assert_eq!(status(cx, &env).state, "Disconnected");
}

#[gpui_test]
fn a_replay_without_timing_names_the_baud_that_paces_it(cx: &mut TestAppContext) {
    let env = open(cx);
    // 20 bytes at 115200 baud is about 2 ms.
    let dir = TestDir::new("transport-no-timing");
    let capture = write_capture(&dir, "dump.bin", &[(0, b"no timing here\r\nok\r\n")], false);
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.open_capture(capture.clone(), window, cx));
    })
    .unwrap();
    let view = wait_connected(cx, &env.workspace);
    wait_for_rx_line(cx, &view, "ok");
    let line = status(cx, &env);
    assert_eq!(line.title, "replay:dump.bin (1x, no timing)");
    assert_eq!(line.settings.as_deref(), Some("115200 8N1"));
}

#[gpui_test]
fn replay_speed_menu_reconnects_at_the_new_speed(cx: &mut TestAppContext) {
    let env = open(cx);
    let dir = TestDir::new("transport-speed-menu");
    let capture = boot_log(&dir);
    // The replay stays open after the last byte, so the choice closes a live session.
    env.replay.set_defaults(ReplayOptions {
        speed: ReplaySpeed::REALTIME,
        end: ReplayEnd::Hold,
    });
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.open_capture(capture.clone(), window, cx));
    })
    .unwrap();
    let view = wait_connected(cx, &env.workspace);
    wait_for_rx_line(cx, &view, "Hit any key to stop autoboot");
    assert_eq!(view.read_with(cx, |v, _| v.connection_label()), "1x");
    assert_eq!(
        view.read_with(cx, |v, _| v.replay_speed()),
        Some(ReplaySpeed::REALTIME)
    );

    // Choosing 4x plays it again from the start, in the same tab and view.
    let faster = ReplayAddress::new(&capture)
        .with_speed(ReplaySpeed::times(4.0).unwrap())
        .port_id();
    assert_eq!(
        faster.to_string(),
        format!("replay:{}?speed=4x", capture.display())
    );
    view.update(cx, |v, cx| {
        v.choose_replay_speed(ReplaySpeed::times(4.0).unwrap(), cx)
    });
    run_until(cx, "the tab to play at 4x", |cx| {
        env.workspace
            .read_with(cx, |w, _| w.session_for_port(&faster).is_some())
            && view.read_with(cx, |v, _| {
                v.replay_speed() == ReplaySpeed::times(4.0) && v.title() == "replay:boot.bin (4x)"
            })
    });
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);
    assert_eq!(titles(cx, &env), ["boot.bin"]);
    assert_eq!(view.read_with(cx, |v, _| v.connection_label()), "4x");
    assert_eq!(view.read_with(cx, |v, _| v.port().clone()), faster);
    assert_eq!(
        env.workspace.read_with(cx, |w, _| w.session().cloned()),
        Some(view.clone()),
        "the same view carries on"
    );
    // The scrollback has the first play and the second: the boot lines twice.
    wait_for_lines(cx, &view, "the capture played twice", |lines| {
        lines
            .iter()
            .filter(|line| line.direction == Direction::Rx && line.text == "DRAM: 512 MiB")
            .count()
            == 2
    });

    // After the end (here: disconnect) choosing a speed plays it again too.
    env.replay.set_defaults(ReplayOptions {
        speed: ReplaySpeed::REALTIME,
        end: ReplayEnd::Disconnect,
    });
    view.update(cx, |v, cx| v.choose_replay_speed(ReplaySpeed::Max, cx));
    let at_max = ReplayAddress::new(&capture)
        .with_speed(ReplaySpeed::Max)
        .port_id();
    run_until(cx, "the replay to play at max and end", |cx| {
        view.read_with(cx, |v, _| {
            v.port() == &at_max && v.state().is_disconnected() && v.title().ends_with("(max)")
        })
    });
    assert_eq!(view.read_with(cx, |v, _| v.connection_label()), "max");
    wait_for_lines(cx, &view, "the capture played three times", |lines| {
        lines
            .iter()
            .filter(|line| line.direction == Direction::Rx && line.text == "DRAM: 512 MiB")
            .count()
            == 3
    });
}

#[gpui_test]
fn toolbar_labels(cx: &mut TestAppContext) {
    let env = open(cx);
    let dir = TestDir::new("transport-toolbar");
    let capture = boot_log(&dir);
    let bridge = Bridge::start(b"hello\r\n");
    env.replay.set_defaults(ReplayOptions {
        speed: ReplaySpeed::times(4.0).unwrap(),
        end: ReplayEnd::Hold,
    });

    let label = |cx: &mut TestAppContext, view: &Entity<SessionView>| {
        view.read_with(cx, |v, _| (v.link(), v.connection_label()))
    };

    // A virtual serial port: its line settings, in the port settings popover.
    cx.update_window(env.window, |_, window, cx| {
        env.workspace.update(cx, |w, cx| {
            let port = PortId::new("virtual:at");
            let serial = w.serial_for(&port, cx);
            w.connect(port, serial, window, cx);
        });
    })
    .unwrap();
    let serial = wait_connected(cx, &env.workspace);
    assert_eq!(
        label(cx, &serial),
        (LinkKind::Serial, "115200 8N1".to_owned())
    );
    assert!(drawn(cx, &env, "port-settings") && !drawn(cx, &env, "replay-speed"));
    assert!(serial.read_with(cx, |v, cx| v.port_form().read(cx).shows_line_settings()));

    // TCP: `TCP`, and a form with the line ending and echo only.
    let address = bridge.address();
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.connect_tcp(address.clone(), window, cx));
    })
    .unwrap();
    let tcp_port = address.port_id();
    run_until(cx, "the TCP tab", |cx| {
        env.workspace
            .read_with(cx, |w, _| w.session_for_port(&tcp_port).is_some())
    });
    let tcp = view_of(cx, &env);
    assert_eq!(label(cx, &tcp), (LinkKind::Tcp, "TCP".to_owned()));
    assert!(drawn(cx, &env, "port-settings") && !drawn(cx, &env, "replay-speed"));
    assert!(!tcp.read_with(cx, |v, cx| v.port_form().read(cx).shows_line_settings()));
    // The popover opens on that form.
    cx.update_window(env.window, |_, window, cx| {
        window.click("port-settings", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(env.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    press(cx, env.window, "escape");

    // A replay: its speed, in a menu of its own; the settings default said 4x.
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.open_capture(capture.clone(), window, cx));
    })
    .unwrap();
    let replay_port = PortId::new(format!("replay:{}", capture.display()));
    run_until(cx, "the replay tab", |cx| {
        env.workspace
            .read_with(cx, |w, _| w.session_for_port(&replay_port).is_some())
    });
    let replay = view_of(cx, &env);
    run_until(cx, "the replay's description", |cx| {
        replay.read_with(cx, |v, _| v.connection_label() == "4x")
    });
    assert_eq!(label(cx, &replay), (LinkKind::Replay, "4x".to_owned()));
    assert!(drawn(cx, &env, "replay-speed") && !drawn(cx, &env, "port-settings"));
    assert!(!replay.read_with(cx, |v, cx| v.port_form().read(cx).shows_line_settings()));
    // The menu opens.
    cx.update_window(env.window, |_, window, cx| {
        window.click("replay-speed", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(env.window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    press(cx, env.window, "escape");

    // Back on the serial tab the toolbar is the serial one again.
    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.activate_index(0, window, cx));
    })
    .unwrap();
    assert!(drawn(cx, &env, "port-settings") && !drawn(cx, &env, "replay-speed"));
}

#[gpui_test]
fn the_replay_setting_reaches_new_replays(cx: &mut TestAppContext) {
    let env = open(cx);
    let dir = TestDir::new("transport-replay-setting");
    let capture = boot_log(&dir);

    // The settings say max; a capture opened without a ?speed= plays at it.
    let config_dir = TestDir::new("transport-replay-setting-config");
    std::fs::write(
        config_dir.join("settings.json"),
        r#"{ "replay": { "speed": "max", "end": "hold" } }"#,
    )
    .unwrap();
    let paths = ConfigPaths::new(config_dir.path());
    cx.update(|cx| config::install(Config::load(paths, false), cx));
    cx.run_until_parked();
    assert_eq!(env.replay.defaults().speed, ReplaySpeed::Max);

    cx.update_window(env.window, |_, window, cx| {
        env.workspace
            .update(cx, |w, cx| w.open_capture(capture.clone(), window, cx));
    })
    .unwrap();
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(status(cx, &env).title, "replay:boot.bin (max)");
    assert_eq!(view.read_with(cx, |v, _| v.connection_label()), "max");
    // At max the whole capture is there at once, with no wait for its 20 ms.
    wait_for_rx_line(cx, &view, "Hit any key to stop autoboot");

    // Another reload changes what the next one opens with; this one keeps its speed.
    std::fs::write(
        config_dir.join("settings.json"),
        r#"{ "replay": { "speed": "2x" } }"#,
    )
    .unwrap();
    let paths = ConfigPaths::new(config_dir.path());
    cx.update(|cx| config::install(Config::load(paths, false), cx));
    cx.run_until_parked();
    assert_eq!(
        env.replay.defaults().speed,
        ReplaySpeed::times(2.0).unwrap()
    );
    assert_eq!(status(cx, &env).title, "replay:boot.bin (max)");
    step(cx, 2);
}

#[gpui_test]
fn the_new_actions_are_in_the_palette(cx: &mut TestAppContext) {
    let env = open(cx);
    press(cx, env.window, keys::COMMAND_PALETTE);
    let palette = env
        .workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .expect("the palette is open");
    let labels = palette.read_with(cx, |p, _| p.labels());
    assert!(
        labels.contains(&"Serial: Connect TCP".to_owned()),
        "{labels:?}"
    );
    assert!(
        labels.contains(&"Serial: Open capture".to_owned()),
        "{labels:?}"
    );

    // Confirming one runs it: the dialog for a TCP endpoint opens.
    cx.update_window(env.window, |_, window, cx| window.input("connect tcp", cx))
        .unwrap();
    press(cx, env.window, "enter");
    run_until(cx, "the TCP dialog", |cx| {
        env.workspace.read_with(cx, |w, _| w.tcp_prompt().is_some())
    });
    assert!(dialog_is_open(cx, env.window));
}

// --- Restored tabs ---------------------------------------------------------------------
//
// The tabs open at quit are in `state.json`; the next start reopens them. A `tcp:` tab
// connects, a `replay:` tab waits for Connect, and serial and `virtual:` tabs connect when
// their port is listed (that is `tab_tests`).

/// A tab as `state.json` keeps it: default line settings, command mode and a VT screen
/// (the settings a restored tab hands its session, for a test to see them arrive).
fn saved_tab(port: PortId) -> SavedTab {
    SavedTab {
        port,
        serial: SerialConfig::default(),
        codec: None,
        mode: SavedMode::Command,
        emulation: Some(Emulation::Vt),
    }
}

/// Write `tabs` where the last quit left them, in the config directory `dir`.
fn save_tabs(dir: &TestDir, active: usize, tabs: Vec<SavedTab>) {
    let state = SessionState {
        active,
        tabs,
        ..SessionState::default()
    };
    state
        .save(&state_path(&ConfigPaths::new(dir.path())))
        .unwrap();
}

fn states(cx: &mut TestAppContext, env: &Env) -> Vec<TabState> {
    env.workspace
        .read_with(cx, |w, cx| w.tab_labels(cx))
        .into_iter()
        .map(|label| label.state)
        .collect()
}

/// The received lines in the session's store, in order, whatever the view shows.
fn stored_rx(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<String> {
    view.read_with(cx, |v, _| {
        let snapshot = v.latest_snapshot().expect("an ingest thread");
        let mut lines = Vec::new();
        snapshot.lines(snapshot.first_line()..snapshot.end(), &mut lines);
        lines
            .into_iter()
            .filter(|line| line.direction == Direction::Rx)
            .map(|line| line.text)
            .collect()
    })
}

fn wait_for_stored_rx(cx: &mut TestAppContext, view: &Entity<SessionView>, text: &str) {
    run_until(cx, &format!("the received line {text:?}"), |cx| {
        stored_rx(cx, view).iter().any(|line| line == text)
    });
}

/// Press the Connect button that a tab with no session shows.
fn press_connect(cx: &mut TestAppContext, env: &Env) {
    assert!(drawn(cx, env, "reconnect-tab"), "the tab offers Connect");
    cx.update_window(env.window, |_, window, cx| {
        window.click("reconnect-tab", cx)
    })
    .unwrap();
    cx.run_until_parked();
}

/// What the Devices panel's notice says now.
fn devices_notice(cx: &mut TestAppContext, env: &Env) -> Option<String> {
    env.workspace.read_with(cx, |w, cx| {
        w.devices()
            .read(cx)
            .notice()
            .map(|notice| notice.to_string())
    })
}

#[gpui_test]
fn a_restored_tcp_tab_reconnects_at_startup(cx: &mut TestAppContext) {
    let dir = TestDir::new("transport-restore-tcp");
    let bridge = Bridge::start(b"ESP-Link ready\r\nuptime 3 s\r\n");
    let id = bridge.address().port_id();
    save_tabs(&dir, 0, vec![saved_tab(id.clone())]);

    // Nobody presses Connect: the workspace opens the endpoint itself, though no port
    // source lists it.
    let env = open_in(cx, Some(&dir));
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);
    assert_eq!(titles(cx, &env), [format!("127.0.0.1:{}", bridge.port)]);
    assert_eq!(
        env.workspace.read_with(cx, |w, _| w.window_title()),
        format!("127.0.0.1:{} \u{2014} Serialist", bridge.port)
    );
    assert_eq!(view.read_with(cx, |v, _| v.port().clone()), id);
    assert_eq!(view.read_with(cx, |v, _| v.link()), LinkKind::Tcp);
    assert_eq!(states(cx, &env), [TabState::Connected]);
    wait_for_stored_rx(cx, &view, "ESP-Link ready");
    wait_for_stored_rx(cx, &view, "uptime 3 s");

    // The saved settings reach the session, as they do for a serial tab.
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    assert_eq!(view.read_with(cx, |v, _| v.mode()), Mode::Command);
    assert_eq!(status(cx, &env).state, "Connected");
}

#[gpui_test]
fn a_restored_tcp_tab_whose_listener_is_gone_waits_with_connect(cx: &mut TestAppContext) {
    let dir = TestDir::new("transport-restore-tcp-gone");
    let port = closed_port();
    let id = TcpAddress::new("127.0.0.1", port).port_id();
    save_tabs(&dir, 0, vec![saved_tab(id.clone())]);

    // Refused: the tab stays, the Devices panel says why, as for a serial port that
    // is listed and does not open.
    let env = open_in(cx, Some(&dir));
    run_until(cx, "the refused connection to be reported", |cx| {
        devices_notice(cx, &env).is_some()
    });
    let notice = devices_notice(cx, &env).unwrap();
    assert!(
        notice.starts_with(&format!("Could not open tcp:127.0.0.1:{port}: ")),
        "{notice}"
    );
    env.workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 1, "the tab stays");
        assert!(w.session().is_none());
        assert_eq!(w.connecting(), None);
        let label = &w.tab_labels(cx)[0];
        assert_eq!(label.title, format!("127.0.0.1:{port}"));
        assert_eq!(label.state, TabState::NotConnected);
        // The center names the endpoint and says why, in the words of the notice.
        assert_eq!(
            w.placeholder_text(),
            (format!("127.0.0.1:{port} is not connected"), notice.clone())
        );
        // The tab is kept for the next quit, settings and all.
        let state = w.session_state(cx);
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].port, id);
        assert_eq!(state.tabs[0].emulation, Some(Emulation::Vt));
    });
    assert!(
        drawn(cx, &env, "reconnect-tab"),
        "a Connect button on the tab"
    );

    // Connect with the listener still gone fails the same way, and the tab stays.
    press_connect(cx, &env);
    run_until(cx, "Connect to fail again", |cx| {
        env.workspace.read_with(cx, |w, _| w.connecting().is_none())
    });
    assert_eq!(states(cx, &env), [TabState::NotConnected]);
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);

    // The listener comes back: Connect opens the endpoint in the same tab.
    let _bridge = Bridge::start_on(port, b"back\r\n");
    press_connect(cx, &env);
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(view.read_with(cx, |v, _| v.port().clone()), id);
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    wait_for_stored_rx(cx, &view, "back");
}

#[gpui_test]
fn a_restored_replay_tab_waits_and_connect_plays_it_from_the_start(cx: &mut TestAppContext) {
    let captures = TestDir::new("transport-restore-replay-captures");
    let capture = boot_log(&captures);
    // The id a tab holds after the speed menu chose max, with the replay left open at
    // the end (the settings' end is to disconnect).
    let id = ReplayAddress::new(&capture)
        .with_speed(ReplaySpeed::Max)
        .with_end(ReplayEnd::Hold)
        .port_id();
    let dir = TestDir::new("transport-restore-replay");
    save_tabs(&dir, 0, vec![saved_tab(id.clone())]);

    // The app opens and does not play the capture, however long it runs.
    let env = open_in(cx, Some(&dir));
    cx.run_until_parked();
    step(cx, 30);
    env.workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 1);
        assert!(w.session().is_none(), "no replay at startup");
        assert_eq!(w.connecting(), None, "nothing is opening");
        assert_eq!(w.tab_port(w.tab_ids()[0]), Some(&id));
        let label = &w.tab_labels(cx)[0];
        assert_eq!(label.title, "boot.bin");
        assert_eq!(label.state, TabState::NotConnected);
        assert_eq!(w.window_title(), "boot.bin \u{2014} Serialist");
        // The center names the capture, and says what Connect does.
        assert_eq!(
            w.placeholder_text(),
            (
                "boot.bin is not playing".to_owned(),
                "Connect plays the capture from the beginning.".to_owned()
            )
        );
        // The id keeps its options, and the tab its settings, for the next quit.
        let state = w.session_state(cx);
        assert_eq!(state.tabs[0].port, id);
        assert_eq!(state.tabs[0].emulation, Some(Emulation::Vt));
    });

    // Connect plays it from the beginning, on the options the id names.
    press_connect(cx, &env);
    let view = wait_connected(cx, &env.workspace);
    assert_eq!(view.read_with(cx, |v, _| v.port().clone()), id);
    assert_eq!(view.read_with(cx, |v, _| v.link()), LinkKind::Replay);
    assert_eq!(view.read_with(cx, |v, _| v.connection_label()), "max");
    assert_eq!(view.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    wait_for_stored_rx(cx, &view, "Hit any key to stop autoboot");
    assert_eq!(
        stored_rx(cx, &view),
        [
            "U-Boot 2024.01",
            "DRAM: 512 MiB",
            "Hit any key to stop autoboot"
        ],
        "every line, once, from the first"
    );
    assert_eq!(env.workspace.read_with(cx, |w, _| w.tab_count()), 1);
    assert_eq!(titles(cx, &env), ["boot.bin"]);

    // `end=hold` came from the id: the played capture does not end the session.
    step(cx, 10);
    assert!(view.read_with(cx, |v, _| !v.state().is_disconnected()));
    assert_eq!(states(cx, &env), [TabState::Connected]);
}

#[gpui_test]
fn restored_tabs_of_every_kind_each_do_their_own_thing(cx: &mut TestAppContext) {
    let captures = TestDir::new("transport-restore-mixed-captures");
    let capture = boot_log(&captures);
    let bridge = Bridge::start(b"hello\r\n");
    let replay = ReplayAddress::new(&capture).port_id();
    let tcp = bridge.address().port_id();
    let simulated = PortId::new("virtual:at");
    let dir = TestDir::new("transport-restore-mixed");
    save_tabs(
        &dir,
        2,
        vec![
            saved_tab(replay.clone()),
            saved_tab(simulated.clone()),
            saved_tab(tcp.clone()),
        ],
    );

    let env = open_in(cx, Some(&dir));
    // The simulator lists virtual:at, so it connects, and so does the endpoint; the
    // capture waits.
    run_until(cx, "the virtual and TCP tabs to connect", |cx| {
        states(cx, &env)
            == [
                TabState::NotConnected,
                TabState::Connected,
                TabState::Connected,
            ]
    });
    env.workspace.read_with(cx, |w, _| {
        let ports: Vec<PortId> = w
            .tab_ids()
            .into_iter()
            .filter_map(|id| w.tab_port(id).cloned())
            .collect();
        assert_eq!(ports, [replay.clone(), simulated, tcp.clone()], "in order");
        assert_eq!(w.active_index(), Some(2), "the tab that was in front");
        assert!(w.session_for_port(&replay).is_none());
        assert!(w.session_for_port(&tcp).is_some());
    });
    assert_eq!(
        view_of(cx, &env).read_with(cx, |v, _| v.port().clone()),
        tcp
    );
}

#[gpui_test]
fn a_restored_tcp_id_that_does_not_parse_waits_for_connect(cx: &mut TestAppContext) {
    let dir = TestDir::new("transport-restore-tcp-bad");
    // As a hand-edited state.json might have it: no port.
    let id = PortId::new("tcp:nope");
    save_tabs(&dir, 0, vec![saved_tab(id.clone())]);

    let env = open_in(cx, Some(&dir));
    cx.run_until_parked();
    step(cx, 10);
    env.workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 1, "the tab is restored, not dropped");
        assert!(w.session().is_none());
        assert_eq!(w.connecting(), None);
        assert_eq!(w.tab_labels(cx)[0].state, TabState::NotConnected);
        assert_eq!(w.tab_port(w.tab_ids()[0]), Some(&id));
        // The center says what a tcp: id is.
        let (title, message) = w.placeholder_text();
        assert_eq!(title, "tcp:nope is not connected");
        assert!(
            message.contains("a tcp port id is tcp:<host>:<port>"),
            "{message}"
        );
    });
    assert_eq!(devices_notice(cx, &env), None, "nothing was tried");

    // Connect says what is wrong with it.
    press_connect(cx, &env);
    run_until(cx, "the open to fail", |cx| {
        devices_notice(cx, &env).is_some()
    });
    let notice = devices_notice(cx, &env).unwrap();
    assert!(notice.starts_with("Could not open tcp:nope: "), "{notice}");
    assert_eq!(states(cx, &env), [TabState::NotConnected]);
}
