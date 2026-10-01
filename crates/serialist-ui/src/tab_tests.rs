//! Tabs against the real engine: several `SimWorld` ports open at once in one workspace,
//! each with its own `Session`, ingest thread, store and script thread, driven through
//! the keyboard as a user would.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use serialist_core::settings::ConfigPaths;
use serialist_core::{
    Direction, Emulation, LineId, LineSource, PortId, PortInfo, PortKind, SerialConfig, UsbInfo,
};
use serialist_sim::{
    AtDevice, EchoDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, LinkConfig, SimWorld,
};

use crate::actions::keys;
use crate::config::{self, Config};
use crate::inline::Mode;
use crate::prelude::*;
use crate::session_state::{SavedMode, SessionState, state_path};
use crate::session_view::{HOUSEKEEPING, SessionView};
use crate::tabs::{TabLabel, TabState};
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, has_rx_line, install_example_plugin,
    open_test_window, run_until, step, type_line, wait_for_received,
};
use crate::workspace::{AppOptions, Workspace};

fn options(world: &SimWorld, ports: &[&str]) -> AppOptions {
    AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: None,
        open_ports: ports.iter().map(|port| PortId::new(*port)).collect(),
        store: None,
    }
}

/// A workspace over `world` opening `ports` at startup, a tab each, with the
/// configuration under `dir` loaded if there is one.
fn open_tabs(
    cx: &mut TestAppContext,
    world: &SimWorld,
    dir: Option<&TestDir>,
    ports: &[&str],
) -> (AnyWindowHandle, Entity<Workspace>) {
    allow_engine_threads(cx);
    let options = options(world, ports);
    let paths = dir.map(|dir| ConfigPaths::new(dir.path()));
    open_test_window(cx, move |window, cx| {
        if let Some(paths) = paths {
            config::install(Config::load(paths, false), cx);
        }
        Workspace::new(options, window, cx)
    })
}

/// The store behind a session view as it is now, whether the view shows it or not.
fn stored(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<(Direction, String)> {
    view.read_with(cx, |v, _| {
        let snapshot = v.latest_snapshot().expect("an ingest thread");
        let mut lines = Vec::new();
        snapshot.lines(snapshot.first_line()..snapshot.end(), &mut lines);
        lines
            .into_iter()
            .map(|line| (line.direction, line.text))
            .collect()
    })
}

fn rx_lines(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<String> {
    stored(cx, view)
        .into_iter()
        .filter(|(direction, _)| *direction == Direction::Rx)
        .map(|(_, text)| text)
        .collect()
}

/// Wait for the tab holding `port` to have a session whose store says it connected;
/// the tab need not be the active one.
fn wait_tab(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    port: &str,
) -> Entity<SessionView> {
    let id = PortId::new(port);
    run_until(cx, &format!("{port} to connect"), |cx| {
        let view = workspace.read_with(cx, |w, _| w.session_for_port(&id).cloned());
        view.is_some_and(|view| {
            view.read_with(cx, |v, _| {
                v.latest_snapshot()
                    .and_then(|snapshot| snapshot.line(LineId::ZERO))
                    .is_some_and(|line| line.text.starts_with("Connected to "))
            })
        })
    });
    workspace.read_with(cx, |w, _| w.session_for_port(&id).cloned().unwrap())
}

fn labels(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<TabLabel> {
    workspace.read_with(cx, |w, cx| w.tab_labels(cx))
}

fn states(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<TabState> {
    labels(cx, workspace).into_iter().map(|l| l.state).collect()
}

fn active_index(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Option<usize> {
    workspace.read_with(cx, |w, _| w.active_index())
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
    cx.run_until_parked();
}

/// The window's title as the platform has it, after a frame.
fn platform_title(cx: &mut TestAppContext, window: AnyWindowHandle) -> Option<String> {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    VisualTestContext::from_window(window, cx).window_title()
}

#[gpui_test]
fn two_tabs_keep_their_own_sessions_scrollback_and_pause(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (window, workspace) = open_tabs(cx, &world, None, &["virtual:at", "virtual:echo"]);
    let at = wait_tab(cx, &workspace, "virtual:at");
    let echo = wait_tab(cx, &workspace, "virtual:echo");

    // The first port given is in front; the other one is behind it, hidden.
    assert_eq!(active_index(cx, &workspace), Some(0));
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.tab_count(), 2);
        assert!(w.shows_tab_bar());
        assert_eq!(w.window_title(), "virtual:at \u{2014} Serialist");
    });
    assert_eq!(
        platform_title(cx, window).as_deref(),
        Some("virtual:at \u{2014} Serialist")
    );
    let titles: Vec<String> = labels(cx, &workspace)
        .into_iter()
        .map(|l| l.title)
        .collect();
    assert_eq!(titles, ["AT modem (virtual)", "Echo (virtual)"]);
    assert_eq!(states(cx, &workspace), [TabState::Connected; 2]);
    assert!(at.read_with(cx, |v, _| v.is_visible()));
    assert!(echo.read_with(cx, |v, _| !v.is_visible()));

    // Sending in the first tab reaches only its port.
    type_line(cx, window, "AT");
    run_until(cx, "OK from the modem", |cx| has_rx_line(cx, &at, "OK"));
    let echo_link = world.link(&PortId::new("virtual:echo")).unwrap();
    assert_eq!(echo_link.stats().host_to_device_bytes, 0);
    assert!(
        rx_lines(cx, &echo).is_empty(),
        "nothing echoed in the other tab"
    );

    // cmd-2 goes to the echo tab, whose compose bar takes the typing.
    press(cx, window, keys::TAB_2);
    assert_eq!(active_index(cx, &workspace), Some(1));
    assert!(echo.read_with(cx, |v, _| v.is_visible()));
    assert!(at.read_with(cx, |v, _| !v.is_visible()));
    assert_eq!(
        workspace.read_with(cx, |w, _| w.window_title()),
        "virtual:echo \u{2014} Serialist"
    );
    type_line(cx, window, "ping");
    wait_for_received(cx, &echo, 6);
    assert!(has_rx_line(cx, &echo, "ping"));
    let at_link = world.link(&PortId::new("virtual:at")).unwrap();
    assert_eq!(
        at_link.stats().host_to_device_bytes,
        4,
        "only the AT went there"
    );
    assert!(!rx_lines(cx, &at).contains(&"ping".to_owned()));
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(
        status.title, "virtual:echo @ 115200 8N1",
        "the status line follows"
    );

    // Pause the echo tab, go back to the modem with cmd-shift-[: its scrollback is as it
    // was left, and the echo tab stays paused behind it.
    press(cx, window, keys::PAUSE);
    assert!(echo.read_with(cx, |v, _| v.is_paused()));
    let echo_shown = displayed(cx, &echo);
    let at_shown = displayed(cx, &at);
    press(cx, window, keys::PREVIOUS_TAB);
    assert_eq!(active_index(cx, &workspace), Some(0));
    step(cx, 3);
    assert_eq!(
        displayed(cx, &at),
        at_shown,
        "the modem's scrollback as it was left"
    );
    assert!(
        echo.read_with(cx, |v, _| v.is_paused()),
        "still paused behind"
    );
    assert_eq!(
        states(cx, &workspace),
        [TabState::Connected, TabState::Paused]
    );

    // cmd-shift-] comes back to it, paused, with the same lines on screen.
    press(cx, window, keys::NEXT_TAB);
    assert_eq!(active_index(cx, &workspace), Some(1));
    step(cx, 3);
    assert!(echo.read_with(cx, |v, _| v.is_paused()));
    assert_eq!(displayed(cx, &echo), echo_shown);
    // And around the end to the first tab.
    press(cx, window, keys::NEXT_TAB);
    assert_eq!(active_index(cx, &workspace), Some(0));
    press(cx, window, keys::TAB_1);
    assert_eq!(active_index(cx, &workspace), Some(0));
}

#[gpui_test]
fn tab_labels_show_each_sessions_state(cx: &mut TestAppContext) {
    let dir = TestDir::new("tab-labels");
    let world = SimWorld::new();
    let (window, workspace) = open_tabs(cx, &world, None, &["virtual:echo", "virtual:at"]);
    let echo = wait_tab(cx, &workspace, "virtual:echo");
    let at = wait_tab(cx, &workspace, "virtual:at");
    assert_eq!(states(cx, &workspace), [TabState::Connected; 2]);

    press(cx, window, keys::PAUSE);
    assert_eq!(states(cx, &workspace)[0], TabState::Paused);
    // Recording outranks pause.
    echo.update(cx, |v, cx| v.start_recording(dir.join("echo.bin"), cx));
    run_until(cx, "the recording to open", |cx| {
        echo.read_with(cx, |v, _| v.recording().is_some_and(|r| r.stats.is_some()))
    });
    assert_eq!(states(cx, &workspace)[0], TabState::Recording);
    echo.update(cx, |v, cx| v.stop_recording(cx));
    press(cx, window, keys::PAUSE);
    assert_eq!(states(cx, &workspace)[0], TabState::Connected);

    // A background tab's dot follows too: disconnected, then (for a device that goes
    // away) lost, which the housekeeping tick notices without a repaint of its own.
    at.update(cx, |v, cx| v.disconnect(cx));
    run_until(cx, "the modem tab to read disconnected", |cx| {
        states(cx, &workspace)[1] == TabState::Disconnected
    });
    press(cx, window, keys::DISCONNECT);
    run_until(cx, "the echo tab to read disconnected", |cx| {
        states(cx, &workspace)[0] == TabState::Disconnected
    });

    let (window2, workspace2) = open_tabs(cx, &world, None, &["virtual:at", "virtual:echo-lines"]);
    wait_tab(cx, &workspace2, "virtual:echo-lines");
    world.unplug(&PortId::new("virtual:echo-lines"));
    run_until(cx, "the lost device's dot", |cx| {
        states(cx, &workspace2)[1] == TabState::Lost
    });
    assert_eq!(states(cx, &workspace2)[0], TabState::Connected);
    let _ = window2;
}

/// A script that takes a while, then talks to its own port.
const SLOW_SCRIPT: &str = "local port = assert(serial.current())\n\
                           print('started')\n\
                           sleep(300)\n\
                           port:write('AT\\r\\n')\n\
                           assert(port:expect('^OK$', 3000), 'no OK')\n\
                           print('got OK')\n";

fn config_with_scripts(name: &str, scripts: &[(&str, &str)]) -> TestDir {
    let dir = TestDir::new(name);
    let scripts_dir = ConfigPaths::new(dir.path()).scripts_dir();
    std::fs::create_dir_all(&scripts_dir).unwrap();
    for (file, code) in scripts {
        std::fs::write(scripts_dir.join(file), code).unwrap();
    }
    dir
}

fn console_texts(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    tab: Option<crate::tabs::TabId>,
) -> Vec<String> {
    let console = workspace.read_with(cx, |w, _| w.console().clone());
    console.read_with(cx, |c, _| {
        c.lines_of(tab).into_iter().map(|line| line.text).collect()
    })
}

#[gpui_test]
fn a_script_keeps_running_in_its_tab_while_another_is_active(cx: &mut TestAppContext) {
    let dir = config_with_scripts("tab-script", &[("slow.lua", SLOW_SCRIPT)]);
    let world = SimWorld::new();
    let (window, workspace) = open_tabs(cx, &world, Some(&dir), &["virtual:at", "virtual:echo"]);
    let at = wait_tab(cx, &workspace, "virtual:at");
    let echo = wait_tab(cx, &workspace, "virtual:echo");
    let ids = workspace.read_with(cx, |w, _| w.tab_ids());

    let queued = cx
        .update_window(window, |_, window, cx| {
            workspace.update(cx, |w, cx| {
                w.run_script_path(std::path::Path::new("slow.lua"), "console", window, cx)
            })
        })
        .unwrap();
    assert!(queued);
    run_until(cx, "the script to start", |cx| {
        console_texts(cx, &workspace, Some(ids[0])).contains(&"started".to_owned())
    });

    // Go to the echo tab while the script waits.
    press(cx, window, keys::TAB_2);
    assert_eq!(active_index(cx, &workspace), Some(1));
    let console = workspace.read_with(cx, |w, _| w.console().clone());
    assert_eq!(console.read_with(cx, |c, _| c.source()), Some(ids[1]));
    assert!(
        console.read_with(cx, |c, _| c.lines().is_empty()),
        "the console shows the echo tab's output"
    );
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.script, None, "the status line is the echo tab's");
    assert!(at.read_with(cx, |v, _| v.script_status().is_some()));

    // The script finishes in the background, on its own port.
    run_until(cx, "the script to finish", |cx| {
        console_texts(cx, &workspace, Some(ids[0]))
            .iter()
            .any(|text| text.starts_with("\u{2713} slow.lua finished"))
    });
    assert!(console_texts(cx, &workspace, Some(ids[0])).contains(&"got OK".to_owned()));
    assert!(rx_lines(cx, &at).contains(&"OK".to_owned()));
    let echo_link = world.link(&PortId::new("virtual:echo")).unwrap();
    assert_eq!(
        echo_link.stats().host_to_device_bytes,
        0,
        "nothing went to the echo tab"
    );
    assert!(rx_lines(cx, &echo).is_empty());
    assert!(console.read_with(cx, |c, _| c.lines().is_empty()));

    // Back in the modem tab, its output is there.
    press(cx, window, keys::TAB_1);
    let shown: Vec<String> = console.read_with(cx, |c, _| c.texts());
    assert!(shown.contains(&"got OK".to_owned()), "{shown:?}");
    assert!(at.read_with(cx, |v, _| v.script_status().is_none()));
}

#[gpui_test]
fn closing_a_tab_asks_then_stops_its_recording_and_script(cx: &mut TestAppContext) {
    let dir = config_with_scripts(
        "tab-close",
        &[(
            "wait.lua",
            "print('waiting')\nwhile true do sleep(20) end\n",
        )],
    );
    let world = SimWorld::new();
    let (window, workspace) = open_tabs(cx, &world, Some(&dir), &["virtual:at", "virtual:echo"]);
    let at = wait_tab(cx, &workspace, "virtual:at");
    wait_tab(cx, &workspace, "virtual:echo");
    let ids = workspace.read_with(cx, |w, _| w.tab_ids());

    let recording = dir.join("at.bin");
    at.update(cx, |v, cx| v.start_recording(recording.clone(), cx));
    run_until(cx, "the recording to open", |cx| {
        at.read_with(cx, |v, _| v.recording().is_some_and(|r| r.stats.is_some()))
    });
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            w.run_script_path(std::path::Path::new("wait.lua"), "console", window, cx)
        })
    })
    .unwrap();
    run_until(cx, "the script to run", |cx| {
        console_texts(cx, &workspace, Some(ids[0])).contains(&"waiting".to_owned())
    });
    type_line(cx, window, "AT");
    run_until(cx, "OK", |cx| has_rx_line(cx, &at, "OK"));

    // cmd-w asks first, since the tab records and runs a script.
    press(cx, window, keys::CLOSE_TAB);
    assert_eq!(
        workspace.read_with(cx, |w, _| w.pending_close()),
        Some(ids[0])
    );
    assert_eq!(
        workspace.read_with(cx, |w, _| w.tab_count()),
        2,
        "nothing closed yet"
    );
    cx.update_window(window, |_, window, cx| {
        assert!(window.has_active_dialog(cx));
        workspace.update(cx, |w, cx| w.confirm_close(window, cx));
        assert!(!window.has_active_dialog(cx));
    })
    .unwrap();
    cx.run_until_parked();

    workspace.read_with(cx, |w, _| {
        assert_eq!(w.tab_count(), 1);
        assert_eq!(w.tab_ids(), [ids[1]]);
        assert_eq!(
            w.active_tab_id(),
            Some(ids[1]),
            "the next tab takes its place"
        );
        assert_eq!(w.pending_close(), None);
    });
    at.read_with(cx, |v, _| {
        assert!(v.state().is_disconnected());
        assert!(v.recording().is_none(), "the recording stopped");
        assert!(!v.scripts_attached(), "the script host let go");
    });
    run_until(cx, "the script to stop", |cx| {
        at.read_with(cx, |v, _| v.script_status().is_none())
    });
    run_until(cx, "the modem's link to close", |_| {
        world
            .link(&PortId::new("virtual:at"))
            .is_none_or(|link| !link.is_device_running())
    });
    run_until(cx, "the recording to be written", |_| {
        std::fs::read(&recording).is_ok_and(|bytes| bytes.ends_with(b"OK\r\n"))
    });
    assert!(
        console_texts(cx, &workspace, Some(ids[0])).is_empty(),
        "its output went with it"
    );

    // The last tab has nothing running: it closes at once.
    press(cx, window, keys::CLOSE_TAB);
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.tab_count(), 0);
        assert!(w.session().is_none());
        assert_eq!(w.window_title(), "Serialist");
    });
    assert_eq!(platform_title(cx, window).as_deref(), Some("Serialist"));
}

#[gpui_test]
fn a_firehose_in_a_background_tab_counts_without_repainting(cx: &mut TestAppContext) {
    const TICKS: usize = 8;
    let world = SimWorld::empty();
    world.add_virtual(
        SimWorld::FIREHOSE,
        "Firehose (virtual)",
        LinkConfig::unpaced(),
        || {
            Box::new(FirehoseDevice::new(
                FirehoseConfig::new(FirehoseContent::Text).with_rate(1024 * 1024),
            ))
        },
    );
    world.add_virtual(
        SimWorld::AT,
        "AT modem (virtual)",
        LinkConfig::unpaced(),
        || Box::new(AtDevice::new()),
    );
    let (window, workspace) = open_tabs(cx, &world, None, &["virtual:firehose", "virtual:at"]);
    let firehose = wait_tab(cx, &workspace, "virtual:firehose");
    wait_tab(cx, &workspace, "virtual:at");
    run_until(cx, "the firehose on screen", |cx| {
        displayed(cx, &firehose).len() > 100
    });

    press(cx, window, keys::TAB_2);
    assert!(firehose.read_with(cx, |v, _| !v.is_visible()));
    let terminal = firehose.read_with(cx, |v, _| v.terminal().clone());
    let (view_notifies, workspace_notifies) =
        (Rc::new(Cell::new(0usize)), Rc::new(Cell::new(0usize)));
    cx.update(|cx| {
        let count = view_notifies.clone();
        cx.observe(&firehose, move |_, _| count.set(count.get() + 1))
            .detach();
        let count = workspace_notifies.clone();
        cx.observe(&workspace, move |_, _| count.set(count.get() + 1))
            .detach();
    });
    let terminal_span = terminal.read_with(cx, |t, _| t.displayed_span());
    let shown_end = firehose.read_with(cx, |v, _| v.snapshot().raw_range().end);
    let wakes = firehose.read_with(cx, |v, _| v.ingest_stats().unwrap().wakes);
    let rx_before = firehose.read_with(cx, |v, _| v.stats().rx_bytes);

    for _ in 0..TICKS {
        // Let real bytes arrive, then one housekeeping tick of the test clock.
        std::thread::sleep(Duration::from_millis(30));
        cx.executor().advance_clock(HOUSEKEEPING);
        cx.run_until_parked();
    }

    let (rx_after, ingested) = firehose.read_with(cx, |v, _| {
        (v.stats().rx_bytes, v.ingest_stats().unwrap().bytes)
    });
    assert!(rx_after > rx_before, "the counters kept moving");
    assert!(ingested > shown_end, "ingest kept storing");
    firehose.read_with(cx, |v, _| {
        assert_eq!(
            v.snapshot().raw_range().end,
            shown_end,
            "no snapshot was taken for a hidden tab"
        );
        assert!(
            v.ingest_stats().unwrap().wakes <= wakes + 1,
            "an unanswered doorbell rings once"
        );
        assert!(v.missed_wake());
        assert!(v.tab_status().unseen_bytes > 0);
    });
    assert_eq!(
        terminal.read_with(cx, |t, _| t.displayed_span()),
        terminal_span,
        "the hidden terminal was handed no lines"
    );
    assert!(
        view_notifies.get() <= TICKS,
        "{} notifies in {TICKS} housekeeping ticks",
        view_notifies.get()
    );
    assert!(view_notifies.get() > 0, "the label's counter moved");
    assert!(
        workspace_notifies.get() <= TICKS,
        "{} workspace repaints in {TICKS} ticks",
        workspace_notifies.get()
    );
    let unseen = labels(cx, &workspace)[0].unseen.clone();
    assert!(unseen.is_some_and(|text| text.starts_with('+')));

    // Showing it again takes one snapshot of everything that arrived.
    press(cx, window, keys::TAB_1);
    firehose.read_with(cx, |v, _| {
        assert!(v.is_visible());
        assert!(!v.missed_wake());
        assert!(v.snapshot().raw_range().end >= ingested);
    });
    assert!(terminal.read_with(cx, |t, _| t.displayed_span()).end > terminal_span.end);
    assert_eq!(labels(cx, &workspace)[0].unseen, None);
}

const FAKE_ADAPTER: &str = "/dev/cu.usbserial-TABS";

/// The simulator's built-ins plus an echo behind a real-looking port path.
fn world_with_adapter() -> SimWorld {
    let world = SimWorld::new();
    world.add_device(
        PortInfo {
            id: PortId::new(FAKE_ADAPTER),
            kind: PortKind::Usb(UsbInfo {
                vid: 0x0403,
                pid: 0x6001,
                serial_number: Some("TABS".into()),
                manufacturer: Some("FTDI".into()),
                product: Some("FT232R USB UART".into()),
            }),
            display_name: "FT232R USB UART".into(),
        },
        LinkConfig::unpaced(),
        || Box::new(EchoDevice::new()),
    );
    world
}

#[gpui_test]
fn the_open_tabs_come_back_at_the_next_start(cx: &mut TestAppContext) {
    let dir = TestDir::new("tab-restore");
    // The RACE tab decodes, so the example plugin is installed.
    install_example_plugin(&ConfigPaths::new(dir.path()), "airoha-race");
    let world = world_with_adapter();
    let (window, workspace) = open_tabs(cx, &world, Some(&dir), &["virtual:at", "virtual:race"]);
    wait_tab(cx, &workspace, "virtual:at");
    let race = wait_tab(cx, &workspace, "virtual:race");
    // The adapter at a rate of its own, as the Devices panel would open it.
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            let serial = SerialConfig {
                baud: 57_600,
                ..SerialConfig::default()
            };
            w.connect(PortId::new(FAKE_ADAPTER), serial, window, cx);
        })
    })
    .unwrap();
    let adapter = wait_tab(cx, &workspace, FAKE_ADAPTER);
    assert_eq!(workspace.read_with(cx, |w, _| w.tab_count()), 3);
    assert_eq!(
        active_index(cx, &workspace),
        Some(2),
        "a new tab for the adapter"
    );

    // The modem in inline mode, the RACE device decoding, and the RACE tab in front.
    // (In inline mode the next-tab chord is one the terminal does not send.)
    press(cx, window, keys::TAB_1);
    press(cx, window, keys::TOGGLE_INLINE);
    race.update(cx, |v, cx| assert!(v.set_codec(Some("airoha-race"), cx)));
    // The RACE device and the adapter show a terminal screen; the modem stays a monitor.
    race.update(cx, |v, cx| v.set_emulation(Emulation::Vt, cx));
    adapter.update(cx, |v, cx| v.set_emulation(Emulation::Vt, cx));
    press(cx, window, keys::NEXT_TAB);
    assert_eq!(active_index(cx, &workspace), Some(1));
    assert_eq!(adapter.read_with(cx, |v, _| v.serial().baud), 57_600);

    // Closing the window writes the tabs down.
    drop((workspace, race, adapter));
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let path = state_path(&ConfigPaths::new(dir.path()));
    let state = SessionState::load(&path).expect("the tabs were saved");
    let ports: Vec<&str> = state.tabs.iter().map(|tab| tab.port.as_str()).collect();
    assert_eq!(ports, ["virtual:at", "virtual:race", FAKE_ADAPTER]);
    assert_eq!(state.active, 1);
    assert_eq!(state.tabs[0].mode, SavedMode::Inline);
    assert_eq!(state.tabs[1].codec.as_deref(), Some("airoha-race"));
    assert_eq!(state.tabs[2].serial.baud, 57_600);
    assert_eq!(state.version, 2);
    let emulations: Vec<_> = state.tabs.iter().map(|tab| tab.emulation).collect();
    assert_eq!(
        emulations,
        [
            Some(Emulation::Monitor),
            Some(Emulation::Vt),
            Some(Emulation::Vt)
        ]
    );

    // The adapter is unplugged before the next start: its tab comes back, waiting.
    world.unplug(&PortId::new(FAKE_ADAPTER));
    let (window, workspace) = open_tabs(cx, &world, Some(&dir), &[]);
    let at = wait_tab(cx, &workspace, "virtual:at");
    let race = wait_tab(cx, &workspace, "virtual:race");
    workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 3);
        assert_eq!(w.active_index(), Some(1), "the tab that was in front");
        assert!(w.session_for_port(&PortId::new(FAKE_ADAPTER)).is_none());
        assert_eq!(w.tab_labels(cx)[2].state, TabState::NotConnected);
        let state = w.session_state(cx);
        assert_eq!(
            state.tabs[2].serial.baud, 57_600,
            "kept for when it connects"
        );
        assert_eq!(
            state.tabs[2].emulation,
            Some(Emulation::Vt),
            "and so is VT mode"
        );
    });
    assert_eq!(at.read_with(cx, |v, _| v.mode()), Mode::Inline);
    assert_eq!(at.read_with(cx, |v, _| v.emulation()), Emulation::Monitor);
    assert_eq!(race.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    assert_eq!(
        race.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        Some("airoha-race".to_owned())
    );
    assert!(race.read_with(cx, |v, _| v.is_visible()));
    assert!(at.read_with(cx, |v, _| !v.is_visible()));

    // Plugged in again, its tab connects with the saved rate.
    world.plug(&PortId::new(FAKE_ADAPTER));
    let id = workspace.read_with(cx, |w, _| w.tab_ids()[2]);
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| w.reconnect_tab(id, window, cx));
    })
    .unwrap();
    let adapter = wait_tab(cx, &workspace, FAKE_ADAPTER);
    assert_eq!(adapter.read_with(cx, |v, _| v.serial().baud), 57_600);
    assert_eq!(adapter.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    let _ = window;
}

#[gpui_test]
fn a_version_1_state_file_reopens_its_tabs_with_the_emulation_the_settings_name(
    cx: &mut TestAppContext,
) {
    let dir = TestDir::new("tab-restore-v1");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "terminal": { "emulation": "vt" } }"#,
    )
    .unwrap();
    // As 0.1.0 wrote it: no `emulation` on the tab.
    std::fs::write(
        dir.join("state.json"),
        r#"{ "version": 1, "active": 0, "tabs": [
            { "port": "virtual:at",
              "serial": { "baud": 115200, "data_bits": "eight", "parity": "none",
                          "stop_bits": "one", "flow_control": "none" },
              "codec": null, "mode": "command" } ] }"#,
    )
    .unwrap();
    let world = SimWorld::new();
    let (_window, workspace) = open_tabs(cx, &world, Some(&dir), &[]);
    let at = wait_tab(cx, &workspace, "virtual:at");
    assert_eq!(at.read_with(cx, |v, _| v.emulation()), Emulation::Vt);
    workspace.read_with(cx, |w, cx| {
        let state = w.session_state(cx);
        assert_eq!(state.version, 2, "written as version 2 from now on");
        assert_eq!(state.tabs[0].emulation, Some(Emulation::Vt));
    });
}

#[gpui_test]
fn restore_session_off_starts_with_no_tabs(cx: &mut TestAppContext) {
    let dir = TestDir::new("tab-restore-off");
    std::fs::write(dir.join("settings.json"), r#"{ "restore_session": false }"#).unwrap();
    let world = SimWorld::new();
    // A state file from before the setting was turned off.
    let state = SessionState {
        tabs: vec![crate::session_state::SavedTab {
            port: PortId::new("virtual:at"),
            serial: SerialConfig::default(),
            codec: None,
            mode: SavedMode::Command,
            emulation: None,
        }],
        ..SessionState::default()
    };
    state
        .save(&state_path(&ConfigPaths::new(dir.path())))
        .unwrap();
    let (window, workspace) = open_tabs(cx, &world, Some(&dir), &[]);
    cx.run_until_parked();
    assert_eq!(workspace.read_with(cx, |w, _| w.tab_count()), 0);
    // And with it on, the same file reopens the tab.
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    let (_window, workspace) = open_tabs(cx, &world, Some(&dir), &[]);
    wait_tab(cx, &workspace, "virtual:at");
    let _ = window;
}

#[gpui_test]
fn several_ports_at_startup_open_a_tab_each_the_first_in_front(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (_window, workspace) = open_tabs(
        cx,
        &world,
        None,
        &["virtual:echo", "virtual:at", "virtual:race"],
    );
    for port in ["virtual:echo", "virtual:at", "virtual:race"] {
        wait_tab(cx, &workspace, port);
    }
    workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 3);
        assert_eq!(w.active_index(), Some(0));
        let ports: Vec<String> = w
            .tab_ids()
            .into_iter()
            .map(|id| w.tab_port(id).unwrap().to_string())
            .collect();
        assert_eq!(ports, ["virtual:echo", "virtual:at", "virtual:race"]);
        let visible: Vec<bool> = w
            .sessions()
            .iter()
            .map(|view| view.read(cx).is_visible())
            .collect();
        assert_eq!(visible, [true, false, false]);
        assert_eq!(
            w.devices().read(cx).connected().len(),
            3,
            "every open port has its dot"
        );
    });
}
