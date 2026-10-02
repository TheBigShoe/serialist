//! TCP and replay sessions in the window: "Connect to TCP…" and "Open capture…", the
//! tab title, the status line, the toolbar's settings control and the replay speed menu.
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
    Direction, PortId, REPLAY_SCHEME, ReplayAddress, ReplayEnd, ReplayOptions, ReplaySpeed,
    ReplayTransportFactory, RoutingTransportFactory, TCP_SCHEME, TcpAddress, TcpTransportFactory,
    TimingWriter, VIRTUAL_SCHEME, timing_path,
};
use serialist_sim::SimWorld;

use crate::actions::{ConnectTcp, OpenCapture, keys};
use crate::config::{self, Config};
use crate::dialog_footer;
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::status::{LinkKind, StatusLine};
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
    let (window, workspace) =
        open_test_window(cx, move |window, cx| Workspace::new(options, window, cx));
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
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
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
