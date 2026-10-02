//! Scripts through the UI against the real engine: a temporary config directory with
//! the example scripts (and whatever a test adds) loaded by the real loaders, the
//! workspace and the Script console in a headless window, and `Session`, ingest and
//! script threads over `SimWorld` links to the simulator's AT modem or a device that
//! records what it receives.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{CommandRef, ControlLine, Direction, PortId, PortInfo, PortKind, UsbInfo};
use serialist_script::LogLevel;
use serialist_sim::{AtDevice, DeviceOutput, LinkConfig, SimDevice, SimWorld};

use crate::actions::keys;
use crate::actions::scripts::OpenScriptsFolder;
use crate::config::{self, Config, Opener};
use crate::docks::DockPanel;
use crate::prelude::*;
use crate::script_bridge::{ConsoleKind, ConsoleLine};
use crate::script_console::ScriptConsole;
use crate::session_view::SessionView;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, open_test_window, run_until, step, type_line,
    wait_connected,
};
use crate::workspace::{AppOptions, Workspace};

/// A config directory with the example scripts and `scripts`, as (path under
/// `scripts/`, code).
fn config_dir(name: &str, scripts: &[(&str, &str)]) -> TestDir {
    let dir = TestDir::new(name);
    let paths = ConfigPaths::new(dir.path());
    paths.ensure_example_scripts().unwrap();
    for (relative, code) in scripts {
        write(&paths.scripts_dir().join(relative), code);
    }
    dir
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Records every byte the host sends.
struct Recorder(Arc<Mutex<Vec<u8>>>);

impl SimDevice for Recorder {
    fn name(&self) -> &str {
        "recorder"
    }

    fn on_receive(&mut self, bytes: &[u8], _: &mut dyn DeviceOutput) {
        self.0.lock().extend_from_slice(bytes);
    }
}

/// The simulator's built-ins plus `virtual:recorder`, whose bytes land in the returned
/// buffer.
fn world() -> (SimWorld, Arc<Mutex<Vec<u8>>>) {
    let world = SimWorld::new();
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    world.add_virtual("recorder", "Recorder", LinkConfig::unpaced(), move || {
        Box::new(Recorder(sink.clone()))
    });
    (world, received)
}

/// A workspace over `world` with the configuration under `dir` loaded (and watched,
/// with `watch`), opening `port` at startup.
fn open(
    cx: &mut TestAppContext,
    world: &SimWorld,
    dir: &TestDir,
    port: &str,
    watch: bool,
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
    let (window, workspace) = open_test_window(cx, move |window, cx| {
        if watch {
            config::start(paths, cx);
        } else {
            config::install(Config::load(paths, false), cx);
        }
        Workspace::new(options, window, cx)
    });
    let view = wait_connected(cx, &workspace);
    (window, workspace, view)
}

fn console(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Entity<ScriptConsole> {
    workspace.read_with(cx, |w, _| w.console().clone())
}

fn lines(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<ConsoleLine> {
    let console = console(cx, workspace);
    console.read_with(cx, |c, _| c.lines().iter().cloned().collect())
}

fn texts(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<String> {
    lines(cx, workspace)
        .into_iter()
        .map(|line| line.text)
        .collect()
}

/// Whether the console has a line starting with `prefix`.
fn has_line(cx: &mut TestAppContext, workspace: &Entity<Workspace>, prefix: &str) -> bool {
    texts(cx, workspace)
        .iter()
        .any(|text| text.starts_with(prefix))
}

fn wait_line(cx: &mut TestAppContext, workspace: &Entity<Workspace>, prefix: &str) {
    run_until(cx, prefix, |cx| has_line(cx, workspace, prefix));
}

/// The status line's script segment.
fn script_status(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Option<String> {
    workspace.read_with(cx, |w, cx| {
        w.status_line(cx).and_then(|status| status.script)
    })
}

fn run_path(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    path: &str,
) -> bool {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            w.run_script_path(Path::new(path), "console", window, cx)
        })
    })
    .unwrap()
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, key: &str) {
    cx.update_window(window, |_, window, cx| window.press(key, cx))
        .unwrap();
}

#[gpui_test]
fn version_probe_runs_from_the_console_against_the_at_modem(cx: &mut TestAppContext) {
    let dir = config_dir("script-probe", &[]);
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);

    let listed: Vec<String> = console(cx, &workspace).read_with(cx, |c, _| {
        c.scripts()
            .iter()
            .map(|entry| entry.relative.clone())
            .collect()
    });
    assert_eq!(listed, ["firehose_stats.lua", "version_probe.lua"]);
    assert_eq!(script_status(cx, &workspace), None, "idle");

    // The Script console is closed until a script runs or the user opens it: open it
    // from its rail, then the list's second Run button.
    assert!(!workspace.read_with(cx, |w, _| w.is_panel_shown(DockPanel::Scripts)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(DockPanel::Scripts.rail_id(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |w, _| w.is_panel_shown(DockPanel::Scripts)));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("run-script", 1usize), cx)
    })
    .unwrap();
    let status = script_status(cx, &workspace).expect("a script in the status line");
    assert!(status.starts_with("Script: version_probe.lua "), "{status}");
    wait_line(cx, &workspace, "\u{2713} version_probe.lua finished in ");

    let lines = lines(cx, &workspace);
    let texts: Vec<&str> = lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(
        texts[..5],
        [
            "\u{25b6} version_probe.lua (console)",
            "[info] matched OK",
            "[info] value 1 1.0.0",
            "[info] value 2 1.0.0",
            "[info] value 3 1.0.0",
        ]
    );
    assert!(texts[5].starts_with("\u{2713} version_probe.lua finished in "));
    assert_eq!(texts.len(), 6, "{texts:#?}");
    assert_eq!(
        lines[1].kind,
        ConsoleKind::Log(LogLevel::Info),
        "colored as a log line"
    );
    assert_eq!(lines[5].kind, ConsoleKind::Finished);
    assert_eq!(script_status(cx, &workspace), None, "idle again");
    let notice = workspace.read_with(cx, |w, cx| w.status_line(cx).unwrap().notice);
    assert_eq!(
        notice.map(|notice| notice.text).as_deref(),
        Some("version_probe.lua finished")
    );
}

#[gpui_test]
fn stop_ends_a_script_that_never_ends_and_the_session_keeps_working(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-stop",
        &[
            ("forever.lua", "print('spinning')\nwhile true do end\n"),
            ("after.lua", "print('after')\n"),
        ],
    );
    let (world, received) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:recorder", false);

    assert!(run_path(cx, window, &workspace, "forever.lua"));
    wait_line(cx, &workspace, "spinning");
    // A second script waits its turn, and says so.
    assert!(run_path(cx, window, &workspace, "after.lua"));
    assert!(has_line(
        cx,
        &workspace,
        "Queued after.lua (console) behind forever.lua"
    ));
    run_until(cx, "the running time", |cx| {
        script_status(cx, &workspace).is_some_and(|status| {
            status.starts_with("Script: forever.lua running ") && status.ends_with("(+1 queued)")
        })
    });

    // The session is the user's too while the script spins: the compose bar sends...
    type_line(cx, window, "hello");
    run_until(cx, "the typed line", |_| {
        received.lock().as_slice() == b"hello\r\n"
    });
    // ...and so do keys in inline mode.
    press(cx, window, keys::TOGGLE_INLINE);
    press(cx, window, "k");
    run_until(cx, "the key", |_| {
        received.lock().as_slice() == b"hello\r\nk"
    });
    press(cx, window, keys::TOGGLE_INLINE);

    let pressed = Instant::now();
    cx.update_window(window, |_, window, cx| window.click("script-stop", cx))
        .unwrap();
    wait_line(cx, &workspace, "\u{25a0} forever.lua stopped after ");
    assert!(
        pressed.elapsed() < Duration::from_secs(1),
        "stopped in {:?}",
        pressed.elapsed()
    );
    // The queued one runs next.
    wait_line(cx, &workspace, "\u{2713} after.lua finished");
    assert!(has_line(cx, &workspace, "after"));
    assert_eq!(script_status(cx, &workspace), None);
}

#[gpui_test]
fn a_prompt_opens_a_dialog_and_answers_or_cancels(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-prompt",
        &[(
            "ask.lua",
            "local name = ui.prompt('Your name?', 'Ada')\nprint('name', name)\n\
             local again = ui.prompt('Again?')\nprint('again', tostring(again))\n",
        )],
    );
    let (world, _) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);

    assert!(run_path(cx, window, &workspace, "ask.lua"));
    run_until(cx, "the first prompt", |cx| {
        view.read_with(cx, |v, _| v.script_prompt().is_some())
    });
    let prompt = view.read_with(cx, |v, _| v.script_prompt().cloned().unwrap());
    assert_eq!(
        prompt.read_with(cx, |p, _| p.label().to_owned()),
        "Your name?"
    );
    assert_eq!(
        prompt.read_with(cx, |p, cx| p.value(cx)),
        "Ada",
        "the default"
    );
    assert!(
        cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    );
    // Type an answer and press Enter, the dialog's OK.
    cx.update_window(window, |_, window, cx| {
        prompt.update(cx, |p, cx| p.set_value("Grace", window, cx));
    })
    .unwrap();
    press(cx, window, "enter");
    wait_line(cx, &workspace, "name\tGrace");

    run_until(cx, "the second prompt", |cx| {
        view.read_with(cx, |v, cx| {
            v.script_prompt()
                .is_some_and(|p| p.read(cx).label() == "Again?")
        })
    });
    // Escape dismisses it: the script gets nil.
    press(cx, window, "escape");
    wait_line(cx, &workspace, "\u{2713} ask.lua finished");
    let texts = texts(cx, &workspace);
    assert_eq!(
        texts[..7],
        [
            "\u{25b6} ask.lua (console)",
            "? Your name? [Ada]",
            "\u{2192} Grace",
            "name\tGrace",
            "? Again?",
            "\u{2192} (dismissed)",
            "again\tnil",
        ]
    );
    assert!(view.read_with(cx, |v, _| v.script_prompt().is_none()));
    assert!(
        !cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    );
}

#[gpui_test]
fn a_prompt_left_open_closes_when_the_script_is_stopped(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-prompt-stop",
        &[("ask.lua", "ui.prompt('Waiting')\n")],
    );
    let (world, _) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);
    assert!(run_path(cx, window, &workspace, "ask.lua"));
    run_until(cx, "the prompt", |cx| {
        view.read_with(cx, |v, _| v.script_prompt().is_some())
    });
    cx.update_window(window, |_, _, cx| {
        workspace.update(cx, |w, cx| w.stop_script(cx));
    })
    .unwrap();
    wait_line(cx, &workspace, "\u{25a0} ask.lua stopped");
    assert!(has_line(
        cx,
        &workspace,
        "\u{2192} (closed: the script ended)"
    ));
    assert!(view.read_with(cx, |v, _| v.script_prompt().is_none()));
    assert!(
        !cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
            .unwrap()
    );
}

/// A collection with a command whose payload is a script, and one that sends text.
const SCRIPTED: &str = r#"{
  "name": "Bench",
  "groups": [
    {
      "name": "Scripts",
      "commands": [
        { "name": "Hello", "payload": { "script": "hello.lua" } },
        { "name": "Ping", "payload": { "text": "ping" } }
      ]
    }
  ]
}"#;

#[gpui_test]
fn a_saved_command_with_a_script_payload_runs_the_script(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-command",
        &[(
            "hello.lua",
            "local port = assert(serial.current())\nport:write('from script\\n')\n\
             print(commands.send('Ping'))\nprint(commands.send('Nope'))\n\
             print(commands.send('Hello'))\n",
        )],
    );
    write(&dir.join("commands").join("bench.json"), SCRIPTED);
    let (world, received) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:recorder", false);

    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| {
            w.send_command(CommandRef::new("Bench", "Scripts", "Hello"), window, cx);
        });
    })
    .unwrap();
    wait_line(cx, &workspace, "\u{2713} hello.lua finished");
    // The script's bytes, then the saved command it sent by name: nothing of the
    // script command itself.
    run_until(cx, "both writes", |_| {
        received.lock().as_slice() == b"from script\nping\r\n"
    });
    let texts = texts(cx, &workspace);
    assert_eq!(texts[0], "\u{25b6} hello.lua (command Hello)");
    assert_eq!(texts[1], "true");
    assert_eq!(texts[2], "nil\tno saved command named \"Nope\"");
    assert_eq!(
        texts[3],
        "nil\tHello runs a script; a script cannot start another"
    );
    // The sent command is echoed as the Commands panel's sends are.
    run_until(cx, "the echo", |cx| {
        displayed(cx, &view)
            .iter()
            .any(|line| line.direction == Direction::Tx && line.text == "ping")
    });
    step(cx, 5);
    assert_eq!(received.lock().as_slice(), b"from script\nping\r\n");
}

/// Logs control-line changes and received bytes in order, and answers as the AT
/// modem does.
struct Watched {
    log: Arc<Mutex<Vec<String>>>,
    modem: AtDevice,
}

impl SimDevice for Watched {
    fn name(&self) -> &str {
        "watched"
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        self.log
            .lock()
            .push(format!("rx {}", String::from_utf8_lossy(bytes).trim_end()));
        self.modem.on_receive(bytes, out);
    }

    fn on_control(&mut self, line: ControlLine, asserted: bool, _: &mut dyn DeviceOutput) {
        self.log.lock().push(format!("{line:?} {asserted}"));
    }
}

#[gpui_test]
fn a_device_profile_runs_its_on_connect_script_once_the_port_is_open(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-on-connect",
        &[(
            "bench/init.lua",
            "local port = assert(serial.current())\nport:write('ATI\\r\\n')\n\
             local m = port:expect('Virtual Modem')\nprint('identified', m ~= nil)\n",
        )],
    );
    write(
        &dir.join("settings.json"),
        r#"{ "devices": [ { "match": { "product": "Bench Board" }, "on_connect": "bench/init.lua" } ] }"#,
    );
    let world = SimWorld::empty();
    let log = Arc::new(Mutex::new(Vec::new()));
    let device_log = log.clone();
    let port = "/dev/cu.usbmodem-BENCH";
    world.add_device(
        PortInfo {
            id: PortId::new(port),
            kind: PortKind::Usb(UsbInfo {
                vid: 0x0e8d,
                pid: 0x2000,
                serial_number: Some("B1".into()),
                manufacturer: Some("Bench".into()),
                product: Some("Bench Board".into()),
            }),
            display_name: "Bench Board".into(),
        },
        LinkConfig::unpaced(),
        move || {
            Box::new(Watched {
                log: device_log.clone(),
                modem: AtDevice::new(),
            })
        },
    );
    let (_window, workspace, _view) = open(cx, &world, &dir, port, false);
    wait_line(cx, &workspace, "\u{2713} bench/init.lua finished");
    let texts = texts(cx, &workspace);
    assert_eq!(texts[0], "\u{25b6} bench/init.lua (on_connect)");
    assert_eq!(texts[1], "identified\ttrue");
    // DTR and RTS were set when the port opened, before the script wrote anything.
    assert_eq!(log.lock()[..3], ["Dtr true", "Rts true", "rx ATI"]);
}

#[gpui_test]
fn a_key_binding_in_the_user_keymap_runs_a_script(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-keymap",
        &[
            ("hello.lua", "print('hello from a key')\n"),
            ("bad.lua", "local x = 1\nerror('boom')\n"),
        ],
    );
    let (hello, bad) = if cfg!(target_os = "macos") {
        ("cmd-alt-1", "cmd-alt-2")
    } else {
        ("ctrl-alt-1", "ctrl-alt-2")
    };
    write(
        &dir.join("keymap.json"),
        &format!(
            r#"[ {{ "context": "Workspace", "bindings": {{
                "{hello}": ["scripts::Run", {{ "path": "hello.lua" }}],
                "{bad}": ["scripts::Run", {{ "path": "bad.lua" }}]
            }} }} ]"#
        ),
    );
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);
    let problems = cx.update(|cx| cx.global::<Config>().problems().to_vec());
    assert!(problems.is_empty(), "{problems:?}");

    press(cx, window, hello);
    wait_line(cx, &workspace, "\u{2713} hello.lua finished");
    assert!(has_line(cx, &workspace, "\u{25b6} hello.lua (key binding)"));
    assert!(has_line(cx, &workspace, "hello from a key"));

    // An error shows the message and the Lua traceback.
    press(cx, window, bad);
    wait_line(cx, &workspace, "\u{2717} bad.lua failed after ");
    let lines = lines(cx, &workspace);
    let failed = lines
        .iter()
        .position(|line| line.text.starts_with("\u{2717} bad.lua failed"))
        .unwrap();
    let error: Vec<&str> = lines[failed + 1..]
        .iter()
        .filter(|line| line.kind == ConsoleKind::Error)
        .map(|line| line.text.as_str())
        .collect();
    assert!(error[0].contains("bad.lua:2: boom"), "{error:#?}");
    assert!(error.contains(&"stack traceback:"), "{error:#?}");
    let notice = workspace.read_with(cx, |w, cx| w.status_line(cx).unwrap().notice.unwrap());
    assert!(notice.is_error);
    assert!(
        notice.text.starts_with("bad.lua failed: "),
        "{}",
        notice.text
    );
}

#[gpui_test]
fn the_repl_runs_a_line_and_equals_prints_it(cx: &mut TestAppContext) {
    let dir = config_dir("script-repl", &[]);
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);
    // The console is on screen once opened from its rail.
    workspace.update(cx, |w, cx| w.set_panel_open(DockPanel::Scripts, true, cx));
    crate::test_support::draw(cx, window);
    let console = console(cx, &workspace);
    let input = console.read_with(cx, |c, _| c.input().clone());
    cx.update_window(window, |_, window, cx| {
        input.update(cx, |input, cx| {
            input.set_value("=6 * 7", window, cx);
            input.focus(window, cx);
        });
        window.press("enter", cx);
    })
    .unwrap();
    wait_line(cx, &workspace, "\u{2713} inline finished");
    assert_eq!(
        texts(cx, &workspace)[..3],
        ["\u{203a} =6 * 7", "\u{25b6} inline (console)", "42"]
    );
    assert_eq!(
        console.read_with(cx, |c, cx| c.input().read(cx).value().to_string()),
        ""
    );

    // Clear empties the output.
    cx.update_window(window, |_, window, cx| window.click("script-clear", cx))
        .unwrap();
    assert!(texts(cx, &workspace).is_empty());
}

#[gpui_test]
fn a_new_script_file_appears_in_the_list(cx: &mut TestAppContext) {
    let dir = config_dir("script-watch", &[]);
    let (world, _) = world();
    let (_window, workspace, _view) = open(cx, &world, &dir, "virtual:at", true);
    let console = console(cx, &workspace);
    let listed = |cx: &mut TestAppContext| -> Vec<String> {
        console.read_with(cx, |c, _| {
            c.scripts()
                .iter()
                .map(|entry| entry.relative.clone())
                .collect()
        })
    };
    assert_eq!(listed(cx), ["firehose_stats.lua", "version_probe.lua"]);
    // Let the OS watcher settle, as the watcher's own tests do.
    std::thread::sleep(Duration::from_millis(200));
    write(
        &ConfigPaths::new(dir.path())
            .scripts_dir()
            .join("lib/new.lua"),
        "print('new')",
    );
    run_until(cx, "the new script in the list", |cx| {
        listed(cx).contains(&"lib/new.lua".to_owned())
    });
    assert_eq!(
        listed(cx),
        ["firehose_stats.lua", "lib/new.lua", "version_probe.lua"]
    );
}

#[gpui_test]
fn open_scripts_folder_adds_the_examples_and_lists_them(cx: &mut TestAppContext) {
    let dir = TestDir::new("script-folder");
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);
    let opened: Arc<Mutex<Vec<std::path::PathBuf>>> = Arc::default();
    let record = opened.clone();
    cx.update(|cx| {
        cx.set_global(Opener(Arc::new(move |path: &Path| {
            record.lock().push(path.to_path_buf());
            Ok(())
        })));
    });
    let console = console(cx, &workspace);
    assert!(console.read_with(cx, |c, _| c.scripts().is_empty()));

    cx.dispatch_action(window, OpenScriptsFolder);
    let scripts_dir = ConfigPaths::new(dir.path()).scripts_dir();
    assert_eq!(opened.lock().as_slice(), std::slice::from_ref(&scripts_dir));
    assert_eq!(console.read_with(cx, |c, _| c.scripts().len()), 2);
    assert!(scripts_dir.join("version_probe.lua").is_file());
}

#[gpui_test]
fn disconnecting_mid_script_stops_it_and_says_so(cx: &mut TestAppContext) {
    let dir = config_dir(
        "script-disconnect",
        &[(
            "wait.lua",
            "print('waiting')\nwhile true do sleep(20) end\n",
        )],
    );
    let (world, _) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);
    assert!(run_path(cx, window, &workspace, "wait.lua"));
    wait_line(cx, &workspace, "waiting");

    // A script is running, so disconnecting asks first.
    press(cx, window, keys::DISCONNECT);
    assert!(view.read_with(cx, |v, _| v.pending_disconnect()));
    assert!(view.read_with(cx, |v, _| !v.state().is_disconnected()));
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| v.confirm_disconnect(window, cx));
    })
    .unwrap();
    wait_line(cx, &workspace, "\u{25a0} wait.lua stopped after ");
    let texts = texts(cx, &workspace);
    assert!(
        texts.contains(&"Stopping wait.lua: the session was disconnected".to_owned()),
        "{texts:#?}"
    );
    assert!(
        texts
            .last()
            .unwrap()
            .ends_with("(the session was disconnected)"),
        "{texts:#?}"
    );
    assert_eq!(script_status(cx, &workspace), None);
    assert!(view.read_with(cx, |v, _| !v.scripts_attached()));

    // Nothing runs on a closed session.
    assert!(!run_path(cx, window, &workspace, "wait.lua"));
    assert!(has_line(
        cx,
        &workspace,
        "Not connected; wait.lua did not run"
    ));
}

#[gpui_test]
fn firehose_stats_counts_the_lines_of_two_seconds(cx: &mut TestAppContext) {
    let dir = config_dir("script-firehose", &[]);
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:firehose", false);
    assert!(run_path(cx, window, &workspace, "firehose_stats.lua"));
    wait_line(cx, &workspace, "\u{2713} firehose_stats.lua finished");
    let report = texts(cx, &workspace)
        .into_iter()
        .find(|text| text.contains(" lines in 2.0 s: "))
        .expect("the rate");
    let count: u64 = report.split(' ').next().unwrap().parse().unwrap();
    assert!(count > 0, "{report}");
    assert!(report.contains(" lines/s, "), "{report}");
}

#[test]
fn the_shipped_examples_are_the_ones_the_console_lists() {
    let dir = TestDir::new("script-examples");
    let paths = ConfigPaths::new(dir.path());
    paths.ensure_example_scripts().unwrap();
    let listed: Vec<String> = crate::script_files::list_scripts(&paths.scripts_dir())
        .into_iter()
        .map(|entry| entry.relative)
        .collect();
    assert_eq!(listed, ["firehose_stats.lua", "version_probe.lua"]);
}
