//! Saved commands and the compose history against the real engine: collections in a
//! temporary config directory loaded by the real loaders, the Commands panel and the
//! workspace in a headless window, and `Session` and ingest threads over `SimWorld`
//! links to the simulator's AT modem or a device that records what it receives.

use std::sync::Arc;

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{CollectionSource, CommandCollection, CommandRef, Direction, PortId};
use serialist_sim::{DeviceOutput, LinkConfig, SimDevice, SimWorld};

use crate::actions::keys;
use crate::commands_panel::{Field, PayloadKind, Row};
use crate::config::{self, Config, ConfigPiece};
use crate::history::SAVE_DELAY;
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, open_test_window, run_until, step, type_line,
    wait_connected,
};
use crate::workspace::{AppOptions, Workspace};

/// A user collection for the tests, in the file format.
const BENCH: &str = r#"{
  "name": "Bench",
  "groups": [
    {
      "name": "Modem",
      "commands": [
        {
          "name": "Version",
          "description": "Ask for the firmware version",
          "payload": { "text": "AT+VER?" },
          "expect": { "pattern": "^\\+VER: (.+)", "timeout_ms": 2000 }
        },
        {
          "name": "Silent",
          "payload": { "text": "AT+NOPE" },
          "expect": { "pattern": "^NEVER", "timeout_ms": 50 }
        },
        { "name": "Reset", "payload": { "text": "ATZ" } }
      ]
    },
    {
      "name": "Frames",
      "commands": [
        {
          "name": "Query",
          "payload": { "hex": "05 5A 02 00 {{id}}" },
          "params": [ { "name": "id", "label": "Command id", "default": "0x0F15", "kind": "hex16" } ]
        }
      ]
    }
  ]
}"#;

/// A config directory with `commands/bench.json`.
fn config_dir(name: &str) -> TestDir {
    let dir = TestDir::new(name);
    std::fs::create_dir_all(dir.join("commands")).unwrap();
    std::fs::write(dir.join("commands").join("bench.json"), BENCH).unwrap();
    dir
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
        select_port: Some(PortId::new(port)),
        connect_on_start: true,
        store: None,
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

fn reference(group: &str, name: &str) -> CommandRef {
    CommandRef::new("Bench", group, name)
}

fn lines(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<(Direction, String)> {
    displayed(cx, view)
        .into_iter()
        .map(|line| (line.direction, line.text))
        .collect()
}

fn notice(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Option<String> {
    view.read_with(cx, |v, _| v.notice().map(|notice| notice.text.clone()))
}

fn send_from_panel(cx: &mut TestAppContext, workspace: &Entity<Workspace>, command: CommandRef) {
    let panel = workspace.read_with(cx, |w, _| w.commands().clone());
    panel.update(cx, |panel, cx| panel.send(command, cx));
    cx.run_until_parked();
}

fn row_names(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<String> {
    workspace.read_with(cx, |w, cx| {
        w.commands()
            .read(cx)
            .rows()
            .iter()
            .map(|row| match row {
                Row::Collection { name, .. } => format!("# {name}"),
                Row::Group { name, .. } => format!("## {name}"),
                Row::Command { reference, .. } => reference.name.clone(),
            })
            .collect()
    })
}

fn row_of(cx: &mut TestAppContext, workspace: &Entity<Workspace>, command: &CommandRef) -> usize {
    workspace.read_with(cx, |w, cx| {
        w.commands()
            .read(cx)
            .rows()
            .iter()
            .position(|row| row.command() == Some(command))
            .expect("the command is listed")
    })
}

#[gpui_test]
fn a_click_sends_enter_sends_and_the_edit_button_only_edits(cx: &mut TestAppContext) {
    let dir = config_dir("commands-click");
    let (world, received) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:recorder", false);
    let reset = row_of(cx, &workspace, &reference("Modem", "Reset"));

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("command-row", reset), cx);
    })
    .unwrap();
    run_until(cx, "the clicked command", |_| {
        received.lock().as_slice() == b"ATZ\r\n"
    });
    run_until(cx, "the echo", |cx| {
        lines(cx, &view).contains(&(Direction::Tx, "ATZ".into()))
    });

    // The row's Edit button edits and sends nothing.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(("edit-command", reset), cx);
    })
    .unwrap();
    cx.run_until_parked();
    let panel = workspace.read_with(cx, |w, _| w.commands().clone());
    assert!(panel.read_with(cx, |p, _| p.editor().is_some()));
    assert_eq!(received.lock().as_slice(), b"ATZ\r\n");

    // Enter in the filter sends the best match.
    // Closing the dialog gives the focus back when it is done, so let it finish first.
    cx.update_window(window, |_, window, cx| window.close_dialog(cx))
        .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.set_filter("versi", window, cx));
        let filter = panel.read(cx).filter_input().clone();
        filter.update(cx, |input, cx| input.focus(window, cx));
        window.press("enter", cx);
    })
    .unwrap();
    run_until(cx, "the filtered command", |_| {
        received.lock().as_slice() == b"ATZ\r\nAT+VER?\r\n"
    });
    // Once: Enter is one send, not the field's and the panel's both.
    step(cx, 5);
    assert_eq!(received.lock().as_slice(), b"ATZ\r\nAT+VER?\r\n");
}

#[gpui_test]
fn the_panel_lists_collections_and_filters_them(cx: &mut TestAppContext) {
    let dir = config_dir("commands-filter");
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);

    let names = row_names(cx, &workspace);
    assert_eq!(
        names[..6],
        [
            "# Bench",
            "## Modem",
            "Version",
            "Silent",
            "Reset",
            "## Frames"
        ]
        .map(str::to_owned)
    );
    assert!(
        names.contains(&"# AT basics".to_owned()),
        "the bundled examples"
    );

    let panel = workspace.read_with(cx, |w, _| w.commands().clone());
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.set_filter("rst", window, cx));
    })
    .unwrap();
    let names = row_names(cx, &workspace);
    assert_eq!(names[0], "Reset", "best match first: {names:?}");
    let first = panel.read_with(cx, |p, _| p.rows()[0].clone());
    let Row::Command { location, .. } = first else {
        panic!("a command row");
    };
    assert_eq!(location.as_deref(), Some("Bench \u{203a} Modem"));
    assert_eq!(
        panel.read_with(cx, |p, _| p.selected().cloned()),
        Some(reference("Modem", "Reset")),
        "the best match is selected, so Enter sends it"
    );
}

#[gpui_test]
fn a_hex16_parameter_is_asked_for_encoded_and_remembered(cx: &mut TestAppContext) {
    let dir = config_dir("commands-params");
    let (world, received) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:recorder", false);

    send_from_panel(cx, &workspace, reference("Frames", "Query"));
    let prompt = workspace
        .read_with(cx, |w, _| w.param_prompt().cloned())
        .expect("a parameter dialog");
    assert_eq!(
        prompt.read_with(cx, |p, cx| p.value("id", cx)).as_deref(),
        Some("0x0F15"),
        "prefilled with the default"
    );
    cx.update_window(window, |_, window, cx| {
        prompt.update(cx, |prompt, cx| {
            prompt.set_value("id", "zz", window, cx);
            assert!(!prompt.confirm(cx), "not a hex16");
            assert!(prompt.errors()[0].is_some());
            prompt.set_value("id", "0x1234", window, cx);
            assert!(prompt.confirm(cx));
        });
    })
    .unwrap();
    run_until(cx, "the frame at the device", |_| {
        received.lock().len() == 6
    });
    assert_eq!(
        *received.lock(),
        [0x05, 0x5A, 0x02, 0x00, 0x34, 0x12],
        "little-endian, no EOL"
    );
    assert!(
        workspace.read_with(cx, |w, _| w.param_prompt().is_none()),
        "the dialog closed"
    );
    run_until(cx, "the echo", |cx| {
        lines(cx, &view).contains(&(Direction::Tx, "05 5A 02 00 34 12".into()))
    });

    // The next send starts from the value sent last.
    send_from_panel(cx, &workspace, reference("Frames", "Query"));
    let prompt = workspace
        .read_with(cx, |w, _| w.param_prompt().cloned())
        .expect("a parameter dialog");
    assert_eq!(
        prompt.read_with(cx, |p, cx| p.value("id", cx)).as_deref(),
        Some("0x1234")
    );
    // Enter in the field (focused when the dialog opened) sends it, once.
    cx.update_window(window, |_, window, cx| window.press("enter", cx))
        .unwrap();
    run_until(cx, "the second frame", |_| received.lock().len() == 12);
    step(cx, 5);
    assert_eq!(received.lock()[6..], [0x05, 0x5A, 0x02, 0x00, 0x34, 0x12]);
    assert_eq!(received.lock().len(), 12, "sent once");
    assert!(workspace.read_with(cx, |w, _| w.param_prompt().is_none()));
}

#[gpui_test]
fn a_matched_reply_is_highlighted_and_timed(cx: &mut TestAppContext) {
    let dir = config_dir("commands-expect");
    let (world, _) = world();
    let (_window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);

    send_from_panel(cx, &workspace, reference("Modem", "Version"));
    run_until(cx, "the reply to be matched", |cx| {
        notice(cx, &view).is_some_and(|text| text.contains("OK in"))
    });
    let text = notice(cx, &view).unwrap();
    assert!(
        text.starts_with("Version: OK in ") && text.ends_with(" ms"),
        "{text}"
    );
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.notice.map(|n| n.text), Some(text));

    let shown = lines(cx, &view);
    assert!(
        shown.contains(&(Direction::Tx, "AT+VER?".into())),
        "echoed even with local echo off: {shown:?}"
    );
    let (marks, source) = view.read_with(cx, |v, cx| {
        let terminal = v.terminal().read(cx);
        (terminal.marks().clone(), terminal.source().clone())
    });
    assert_eq!(marks.len(), 1);
    let marked = source.line(marks[0].line).expect("the marked line");
    assert_eq!(marked.direction, Direction::Rx);
    assert_eq!(marked.text, "+VER: 1.0.0");
    assert_eq!(marks[0].range, 0..marked.text.len());
}

#[gpui_test]
fn a_timeout_adds_a_notice_line_and_a_status_notice(cx: &mut TestAppContext) {
    let dir = config_dir("commands-timeout");
    let (world, _) = world();
    let (_window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);

    send_from_panel(cx, &workspace, reference("Modem", "Silent"));
    let message = "Silent: no response within 50 ms";
    run_until(cx, "the timeout", |cx| {
        lines(cx, &view).contains(&(Direction::Notice, message.into()))
    });
    let status = view.read_with(cx, |v, _| v.notice().cloned()).unwrap();
    assert!(status.is_error);
    assert_eq!(status.text, message);
    assert!(
        view.read_with(cx, |v, cx| v.terminal().read(cx).marks().is_empty()),
        "nothing to highlight"
    );
}

#[gpui_test]
fn editing_saves_the_file_and_the_watcher_brings_it_back(cx: &mut TestAppContext) {
    let dir = config_dir("commands-edit");
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", true);
    let panel = workspace.read_with(cx, |w, _| w.commands().clone());

    // A right click on a command opens the editor on it.
    let version_row = panel.read_with(cx, |p, _| {
        p.rows()
            .iter()
            .position(|row| row.command() == Some(&reference("Modem", "Version")))
            .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.right_click(("command-row", version_row), cx);
    })
    .unwrap();
    let editor = panel
        .read_with(cx, |p, _| p.editor().cloned())
        .expect("the editor opened");
    assert_eq!(
        editor.read_with(cx, |e, cx| e.field(Field::Payload, cx)),
        "AT+VER?"
    );
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_field(Field::Name, "Firmware", window, cx);
            editor.set_field(Field::Keybinding, "ctrl-alt-9", window, cx);
            editor.set_field(Field::ExpectTimeout, "750", window, cx);
            assert!(editor.save(cx), "{:?}", editor.error());
        });
    })
    .unwrap();

    // On disk, rewritten as plain JSON.
    let file = dir.join("commands").join("bench.json");
    let loaded = CommandCollection::load(&file, CollectionSource::User(file.clone())).unwrap();
    let (group, command) = loaded
        .collection
        .find("Firmware")
        .expect("renamed in the file");
    assert_eq!(group.name, "Modem");
    assert_eq!(command.keybinding.as_deref(), Some("ctrl-alt-9"));
    assert_eq!(command.expect.as_ref().map(|e| e.timeout_ms), Some(750));
    assert!(loaded.collection.find("Version").is_none());

    // The watcher's reload shows it.
    let started = std::time::Instant::now();
    run_until(cx, "the watcher's reload", |cx| {
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        row_names(cx, &workspace).contains(&"Firmware".to_owned())
    });
    let store = cx.update(|cx| cx.global::<Config>().commands().clone());
    assert!(store.get(&reference("Modem", "Firmware")).is_some());

    // A bad value keeps the dialog open and says why.
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            editor.set_field(Field::ExpectPattern, "(unclosed", window, cx);
            assert!(!editor.save(cx));
        });
    })
    .unwrap();
    assert!(editor.read_with(cx, |e, _| e.error().is_some()));
}

#[gpui_test]
fn command_keybindings_send_and_conflicts_are_reported(cx: &mut TestAppContext) {
    let dir = config_dir("commands-keys");
    let bound = format!(
        r#"{{ "name": "Keys", "groups": [ {{ "name": "Hot", "commands": [
            {{ "name": "Hex", "payload": {{ "hex": "AA 55" }}, "keybinding": "ctrl-alt-1" }},
            {{ "name": "Clash", "payload": {{ "text": "X" }}, "keybinding": "{}" }}
        ] }} ] }}"#,
        keys::CLEAR
    );
    std::fs::write(dir.join("commands").join("keys.json"), bound).unwrap();
    let (world, received) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:recorder", false);

    // The compose bar has the focus; the binding works from there.
    cx.update_window(window, |_, window, cx| window.press("ctrl-alt-1", cx))
        .unwrap();
    run_until(cx, "the bound command at the device", |_| {
        received.lock().as_slice() == [0xAA, 0x55]
    });

    let problems = cx.update(|cx| cx.global::<Config>().problems().to_vec());
    let clash = problems
        .iter()
        .find(|p| p.message.contains("Clash"))
        .unwrap_or_else(|| panic!("no conflict reported: {problems:?}"));
    assert_eq!(clash.piece, ConfigPiece::Bindings);
    assert_eq!(
        clash.message,
        format!(
            "Command Clash ({}) conflicts with terminal::Clear",
            keys::CLEAR
        )
    );
    let notice = workspace
        .read_with(cx, |w, cx| w.config_notice(cx))
        .unwrap();
    assert!(
        notice.text.contains("conflicts with terminal::Clear"),
        "{notice:?}"
    );
}

#[gpui_test]
fn the_bundled_examples_are_read_only_and_copy_to_the_user(cx: &mut TestAppContext) {
    let dir = config_dir("commands-bundled");
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:at", false);
    let panel = workspace.read_with(cx, |w, _| w.commands().clone());

    let bundled = panel.read_with(cx, |p, _| {
        p.rows()
            .iter()
            .find(|row| matches!(row, Row::Collection { name, .. } if name == "AT basics"))
            .cloned()
    });
    assert!(matches!(
        bundled,
        Some(Row::Collection {
            read_only: true,
            ..
        })
    ));

    // Editing an example opens a copy for the user's own collection.
    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit(&CommandRef::new("AT basics", "Basics", "AT"), window, cx);
        });
    })
    .unwrap();
    let editor = panel.read_with(cx, |p, _| p.editor().cloned()).unwrap();
    assert_eq!(
        editor.read_with(cx, |e, cx| e.field(Field::Collection, cx)),
        "Bench"
    );

    panel.update(cx, |panel, cx| panel.copy_to_user("AT basics", cx));
    let copy = dir.join("commands").join("at-basics-copy.json");
    assert!(
        copy.is_file(),
        "{:?}",
        panel.read_with(cx, |p, _| p.notice().cloned())
    );
    cx.update(|cx| config::reload(ConfigPiece::Commands, cx));
    cx.run_until_parked();
    let store = cx.update(|cx| cx.global::<Config>().commands().clone());
    let copied = store
        .collection("AT basics (copy)")
        .expect("the copy is loaded");
    assert!(!copied.is_read_only());
    assert_eq!(copied.commands().count(), 5);
}

#[gpui_test]
fn save_as_command_and_new_collection_write_files(cx: &mut TestAppContext) {
    let dir = TestDir::new("commands-save-as");
    let (world, _) = world();
    let (window, workspace, view) = open(cx, &world, &dir, "virtual:at", false);
    let panel = workspace.read_with(cx, |w, _| w.commands().clone());

    let compose = view.read_with(cx, |v, _| v.compose().clone());
    cx.update_window(window, |_, window, cx| {
        compose.update(cx, |compose, cx| {
            compose
                .input()
                .update(cx, |input, cx| input.set_value("AT+CSQ", window, cx));
        });
        window.press(keys::SAVE_AS_COMMAND, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let editor = panel
        .read_with(cx, |p, _| p.editor().cloned())
        .expect("save as command opened the editor");
    let (name, payload, collection, group, kind) = editor.read_with(cx, |e, cx| {
        (
            e.field(Field::Name, cx),
            e.field(Field::Payload, cx),
            e.field(Field::Collection, cx),
            e.field(Field::Group, cx),
            e.payload_kind(),
        )
    });
    assert_eq!((name.as_str(), payload.as_str()), ("AT+CSQ", "AT+CSQ"));
    assert_eq!(
        (collection.as_str(), group.as_str()),
        ("My commands", "Saved")
    );
    assert_eq!(kind, PayloadKind::Text);
    assert!(editor.update(cx, |editor, cx| editor.save(cx)));
    let file = dir.join("commands").join("my-commands.json");
    let loaded = CommandCollection::load(&file, CollectionSource::User(file.clone())).unwrap();
    assert!(loaded.collection.find("AT+CSQ").is_some());

    cx.update_window(window, |_, window, cx| {
        panel.update(cx, |panel, cx| panel.new_collection(window, cx));
        let input = panel
            .read(cx)
            .name_prompt()
            .cloned()
            .expect("the name prompt");
        input.update(cx, |input, cx| input.set_value("Bring-up", window, cx));
    })
    .unwrap();
    assert!(panel.update(cx, |panel, cx| panel.confirm_new_collection(cx)));
    assert!(dir.join("commands").join("bring-up.json").is_file());
    cx.update(|cx| config::reload(ConfigPiece::Commands, cx));
    cx.run_until_parked();
    assert!(row_names(cx, &workspace).contains(&"# Bring-up".to_owned()));
}

#[gpui_test]
fn compose_history_persists_across_workspaces(cx: &mut TestAppContext) {
    let dir = TestDir::new("history");
    let (world, _) = world();
    let (window, workspace, _view) = open(cx, &world, &dir, "virtual:echo", false);
    type_line(cx, window, "first");
    type_line(cx, window, "second");
    let history = workspace.read_with(cx, |w, _| w.history().clone());
    assert_eq!(history.read_with(cx, |h, _| h.len()), 2);
    assert_eq!(history.read_with(cx, |h, _| h.saves()), 0, "debounced");
    cx.executor().advance_clock(SAVE_DELAY);
    run_until(cx, "the history to be written", |cx| {
        history.read_with(cx, |h, _| h.saves()) == 1
    });
    let file = std::fs::read_to_string(dir.join("history.jsonl")).unwrap();
    assert_eq!(file, "\"first\"\n\"second\"\n");
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    drop(workspace);

    // Another port, so the first session's close need not have finished.
    let (window, _workspace, view) = open(cx, &world, &dir, "virtual:echo-lines", false);
    let compose = view.read_with(cx, |v, _| v.compose().clone());
    let text = |cx: &mut TestAppContext| compose.read_with(cx, |c, cx| c.text(cx));
    cx.update_window(window, |_, window, cx| window.press("up", cx))
        .unwrap();
    assert_eq!(text(cx), "second");
    cx.update_window(window, |_, window, cx| window.press("up", cx))
        .unwrap();
    assert_eq!(text(cx), "first");
}
