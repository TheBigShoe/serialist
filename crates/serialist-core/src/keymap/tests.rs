use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;
use serde_json::json;

use crate::test_util::TempDir;

use super::*;

fn parse(text: &str) -> Keymap {
    Keymap::parse(text, Path::new("keymap.json")).unwrap_or_else(|err| panic!("{err}"))
}

fn parse_err(text: &str) -> KeymapError {
    Keymap::parse(text, Path::new("keymap.json")).expect_err("keymap should be rejected")
}

fn position(err: &KeymapError) -> (usize, usize) {
    match err {
        KeymapError::Invalid { line, column, .. } => (*line, *column),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn string_array_and_null_actions() {
    let keymap = parse(
        r#"[
            {
                "context": "Terminal",
                "bindings": {
                    "cmd-k": "terminal::Clear",
                    "cmd-p": ["terminal::Pause", { "arg": 1 }],
                    "cmd-l": ["terminal::JumpToBottom"],
                    "ctrl-x": null
                }
            }
        ]"#,
    );
    assert_eq!(
        keymap.entries,
        vec![
            KeyBinding::bind(Some("Terminal"), "cmd-k", "terminal::Clear"),
            KeyBinding {
                context: Some("Terminal".into()),
                keystrokes: "cmd-p".into(),
                action: Some(ActionRef {
                    name: "terminal::Pause".into(),
                    args: Some(json!({ "arg": 1 })),
                }),
                use_key_equivalents: false,
            },
            KeyBinding::bind(Some("Terminal"), "cmd-l", "terminal::JumpToBottom"),
            KeyBinding::unbind(Some("Terminal"), "ctrl-x"),
        ]
    );
    assert!(keymap.entries[3].action.is_none());
}

#[test]
fn action_arguments_can_be_any_json() {
    let keymap = parse(
        r#"[ { "bindings": {
            "a": ["x::A", 5],
            "b": ["x::B", "text"],
            "c": ["x::C", [1, 2, { "k": null }]],
            "d": ["x::D", null],
            "e": ["x::E", false]
        } } ]"#,
    );
    let args: Vec<_> = keymap
        .entries
        .iter()
        .map(|entry| entry.action.as_ref().unwrap().args.clone())
        .collect();
    assert_eq!(
        args,
        vec![
            Some(json!(5)),
            Some(json!("text")),
            Some(json!([1, 2, { "k": null }])),
            // A null argument is no argument.
            None,
            Some(json!(false)),
        ]
    );
}

#[test]
fn sections_without_a_context_bind_everywhere() {
    let keymap = parse(
        r#"[ { "bindings": { "cmd-q": "serialist::Quit" } },
              { "context": "", "bindings": { "cmd-w": "serial::Disconnect" } },
              { "context": null, "bindings": { "cmd-e": "compose::CycleLineEnding" } },
              { "context": "  Workspace ", "bindings": { "cmd-r": "terminal::ToggleRecord" } } ]"#,
    );
    let contexts: Vec<_> = keymap
        .entries
        .iter()
        .map(|e| e.context.as_deref())
        .collect();
    assert_eq!(contexts, vec![None, None, None, Some("Workspace")]);
}

#[test]
fn use_key_equivalents_is_carried_per_section() {
    let keymap = parse(
        r#"[ { "use_key_equivalents": true, "bindings": { "cmd-a": "terminal::SelectAll" } },
              { "use_key_equivalents": false, "bindings": { "cmd-b": "x::B" } },
              { "bindings": { "cmd-c": "x::C" } } ]"#,
    );
    let flags: Vec<_> = keymap
        .entries
        .iter()
        .map(|e| e.use_key_equivalents)
        .collect();
    assert_eq!(flags, vec![true, false, false]);
}

#[test]
fn file_order_is_kept_including_repeated_keystrokes() {
    let keymap = parse(
        r#"[ { "bindings": { "z": "x::Z", "a": "x::A", "m": "x::M", "a": "x::A2" } },
              { "context": "Terminal", "bindings": { "b": "x::B" } } ]"#,
    );
    let order: Vec<_> = keymap
        .entries
        .iter()
        .map(|e| {
            (
                e.keystrokes.as_str(),
                e.action.as_ref().unwrap().name.as_str(),
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            ("z", "x::Z"),
            ("a", "x::A"),
            ("m", "x::M"),
            ("a", "x::A2"),
            ("b", "x::B")
        ]
    );
}

#[test]
fn multi_key_chords_are_kept_as_written() {
    let keymap =
        parse(r#"[ { "bindings": { "ctrl-x ctrl-c": "serialist::Quit", " cmd-k ": "x::K" } } ]"#);
    assert_eq!(keymap.entries[0].keystrokes, "ctrl-x ctrl-c");
    // Surrounding whitespace is trimmed.
    assert_eq!(keymap.entries[1].keystrokes, "cmd-k");
}

#[test]
fn comments_trailing_commas_and_empty_files() {
    let keymap = parse(
        "// my keys\n[\n  { /* section */ \"bindings\": { \"cmd-k\": \"terminal::Clear\", }, },\n]\n",
    );
    assert_eq!(keymap.entries.len(), 1);
    for empty in [
        "",
        "  \n",
        "// only a comment\n",
        "[]",
        "null",
        "[ { } ]",
        "[ { \"bindings\": null } ]",
    ] {
        assert_eq!(parse(empty), Keymap::default(), "{empty:?}");
    }
}

#[test]
fn errors_carry_file_line_and_column() {
    // A binding of the wrong type points at the value.
    let err = parse_err(
        "[\n  { \"context\": \"Terminal\",\n    \"bindings\": {\n      \"cmd-k\": 5\n    } }\n]",
    );
    let (line, column) = position(&err);
    assert_eq!((line, column), (4, 16));
    assert!(err.to_string().starts_with("keymap.json:4:16: "), "{err}");
    assert!(err.to_string().contains("an action name"), "{err}");

    // Bad syntax.
    let err = parse_err("[\n  { \"bindings\": { \"cmd-k\": } }\n]");
    assert_eq!(position(&err).0, 2);

    // The root must be a list.
    let err = parse_err("{ \"bindings\": {} }");
    assert!(err.to_string().contains("a keymap: a list"), "{err}");

    for (text, needle) in [
        (r#"[ { "bindings": { "cmd-k": "" } } ]"#, "cannot be empty"),
        (
            r#"[ { "bindings": { "": "x::A" } } ]"#,
            "keystroke cannot be empty",
        ),
        (
            r#"[ { "bindings": { "cmd-k": [] } } ]"#,
            "action name first",
        ),
        (r#"[ { "bindings": { "cmd-k": [1] } } ]"#, "string"),
        (
            r#"[ { "bindings": { "cmd-k": ["x::A", 1, 2] } } ]"#,
            "[action name, arguments]",
        ),
        (r#"[ { "bindings": { "cmd-k": true } } ]"#, "an action name"),
        (
            r#"[ { "bindings": { "cmd-k": { "a": 1 } } } ]"#,
            "an action name",
        ),
        (r#"[ { "bindings": [] } ]"#, "keystrokes to actions"),
        (r#"[ { "context": 5, "bindings": {} } ]"#, "string"),
        (r#"[ 5 ]"#, "struct"),
    ] {
        let err = parse_err(text);
        assert!(err.to_string().contains(needle), "{text}: {err}");
    }
}

#[test]
fn a_user_keymap_is_appended_after_the_defaults() {
    let dir = TempDir::new("keymap");
    let user = dir.write(
        "keymap.json",
        r#"[ { "context": "Workspace",
               "bindings": { "cmd-k": "terminal::JumpToBottom", "cmd-p": null, "cmd-1": "x::New" } } ]"#,
    );
    let defaults = Keymap::bundled(Platform::MacOs);
    let keymap = load_keymap_for(Platform::MacOs, Some(&user)).unwrap();
    assert_eq!(keymap.entries.len(), defaults.entries.len() + 3);
    // Defaults first, untouched.
    assert_eq!(
        keymap.entries[..defaults.entries.len()],
        defaults.entries[..]
    );
    // The user's entries follow, in file order.
    let tail = &keymap.entries[defaults.entries.len()..];
    assert_eq!(
        tail[0],
        KeyBinding::bind(Some("Workspace"), "cmd-k", "terminal::JumpToBottom")
    );
    assert_eq!(tail[1], KeyBinding::unbind(Some("Workspace"), "cmd-p"));
    assert_eq!(
        tail[2],
        KeyBinding::bind(Some("Workspace"), "cmd-1", "x::New")
    );

    // Applying in order, the user's bindings win and the unbind removes the default.
    let resolved = keymap.resolved();
    let find = |context: &str, keys: &str| {
        resolved
            .iter()
            .find(|b| b.context.as_deref() == Some(context) && b.keystrokes == keys)
            .and_then(|b| b.action.as_ref().map(|a| a.name.as_str()))
    };
    assert_eq!(find("Workspace", "cmd-k"), Some("terminal::JumpToBottom"));
    assert_eq!(find("Workspace", "cmd-p"), None);
    assert_eq!(find("Workspace", "cmd-1"), Some("x::New"));
    assert_eq!(find("Workspace", "cmd-s"), Some("terminal::Export"));
    // The same keystroke in another context is a different binding.
    assert_eq!(find("Terminal", "cmd-f"), Some("terminal::Search"));
    assert_eq!(resolved.len(), defaults.entries.len() + 1 - 1);
}

#[test]
fn load_keymap_handles_missing_and_broken_files() {
    let dir = TempDir::new("keymap-errors");
    let missing = dir.path().join("nope.json");
    assert_eq!(
        load_keymap_for(Platform::Linux, Some(&missing)).unwrap(),
        Keymap::bundled(Platform::Linux)
    );
    assert_eq!(load_keymap(None).unwrap(), Keymap::bundled_default());
    assert_eq!(
        load_keymap(Some(&missing)).unwrap(),
        Keymap::bundled_default()
    );

    let broken = dir.write("broken.json", "[ { \"bindings\": { \"cmd-k\": 5 } } ]");
    match load_keymap(Some(&broken)).unwrap_err() {
        KeymapError::Invalid { file, line, .. } => {
            assert_eq!(file, broken);
            assert_eq!(line, 1);
        }
        other => panic!("expected Invalid, got {other:?}"),
    }

    // A directory where the file should be is an I/O error, not a silent default.
    let err = load_keymap(Some(dir.path())).unwrap_err();
    assert!(matches!(err, KeymapError::Io { .. }), "{err:?}");
}

#[test]
fn resolved_keeps_the_last_entry_per_context_and_keystroke() {
    let mut keymap = Keymap::default();
    keymap.entries.push(KeyBinding::bind(None, "a", "x::One"));
    keymap
        .entries
        .push(KeyBinding::bind(Some("T"), "a", "x::Two"));
    keymap.entries.push(KeyBinding::bind(None, "b", "x::B"));
    keymap.entries.push(KeyBinding::bind(None, "a", "x::Three"));
    keymap.entries.push(KeyBinding::unbind(None, "b"));
    let names: Vec<_> = keymap
        .resolved()
        .iter()
        .map(|b| {
            (
                b.context.clone(),
                b.keystrokes.clone(),
                b.action.as_ref().unwrap().name.clone(),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            (Some("T".to_string()), "a".to_string(), "x::Two".to_string()),
            (None, "a".to_string(), "x::Three".to_string()),
        ]
    );
    assert_eq!(
        keymap.action_names(),
        vec!["x::One", "x::Two", "x::B", "x::Three"]
    );
}

// ---- The bundled defaults against the UI's action definitions ----

const ACTIONS_RS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../serialist-ui/src/actions.rs"
);

/// Every `namespace::Action` declared with `actions!(namespace, [..])` in the UI crate.
fn declared_actions() -> BTreeSet<String> {
    let source = std::fs::read_to_string(ACTIONS_RS)
        .unwrap_or_else(|err| panic!("cannot read {ACTIONS_RS}: {err}"));
    // Drop comments so doc comments inside the list do not read as names.
    let source: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let macro_call = Regex::new(r"(?s)actions!\(\s*(\w+)\s*,\s*\[(.*?)\]\s*\)").unwrap();
    let mut names = BTreeSet::new();
    for call in macro_call.captures_iter(&source) {
        for name in call[2].split(',').map(str::trim).filter(|n| !n.is_empty()) {
            names.insert(format!("{}::{name}", &call[1]));
        }
    }
    names
}

/// The values of the constants in `pub mod context { .. }` in the UI crate.
fn declared_contexts() -> BTreeSet<String> {
    let source = std::fs::read_to_string(ACTIONS_RS).unwrap();
    let module = source
        .split_once("pub mod context {")
        .and_then(|(_, rest)| rest.split_once('}'))
        .map(|(body, _)| body)
        .expect("a `pub mod context` in actions.rs");
    let constant = Regex::new(r#"pub const \w+: &str = "([^"]+)""#).unwrap();
    constant
        .captures_iter(module)
        .map(|c| c[1].to_string())
        .collect()
}

#[test]
fn the_scan_finds_the_ui_actions() {
    let actions = declared_actions();
    for expected in [
        "serialist::Quit",
        "terminal::Clear",
        "terminal::Pause",
        "terminal::SelectAll",
        "serial::Connect",
        "devices::SelectNext",
        "compose::CycleLineEnding",
    ] {
        assert!(
            actions.contains(expected),
            "scan missed {expected}: {actions:?}"
        );
    }
    let contexts = declared_contexts();
    assert!(
        contexts.contains("Terminal") && contexts.contains("ComposeBar"),
        "{contexts:?}"
    );
}

#[test]
fn every_platform_default_loads_and_names_only_real_actions() {
    let actions = declared_actions();
    let contexts = declared_contexts();
    for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
        let keymap = Keymap::bundled(platform);
        assert!(!keymap.entries.is_empty(), "{platform:?}");
        for entry in &keymap.entries {
            let action = entry
                .action
                .as_ref()
                .unwrap_or_else(|| panic!("{platform:?}: a default unbinds {}", entry.keystrokes));
            assert!(
                actions.contains(&action.name),
                "{platform:?}: {} is bound to {}, which actions.rs does not declare",
                entry.keystrokes,
                action.name
            );
            assert!(action.args.is_none());
            if let Some(context) = &entry.context {
                for part in context.split('>').map(str::trim) {
                    assert!(
                        contexts.contains(part) || part == "Input",
                        "{platform:?}: unknown context {part:?} in {context:?}"
                    );
                }
            }
            assert!(!entry.keystrokes.is_empty());
        }
        // No keystroke is bound twice in one context.
        let mut seen = BTreeSet::new();
        for entry in &keymap.entries {
            assert!(
                seen.insert((entry.context.clone(), entry.keystrokes.clone())),
                "{platform:?}: {:?} {} is bound twice",
                entry.context,
                entry.keystrokes
            );
        }
    }
}

/// `(context, keystrokes, action)` for the app's bindings as `bind_keys` registers them,
/// with the modifier for the platform's primary shortcut key filled in.
fn expected_bindings(mac: bool) -> BTreeSet<(Option<&'static str>, &'static str, &'static str)> {
    let workspace = Some("Workspace");
    let terminal = Some("Terminal");
    let inline = Some("TerminalInline");
    let keys: Vec<(Option<&str>, &str, &str)> = if mac {
        vec![
            (None, "cmd-q", "serialist::Quit"),
            (workspace, "cmd-k", "terminal::Clear"),
            (workspace, "cmd-w", "serial::Disconnect"),
            (workspace, "cmd-p", "terminal::Pause"),
            (workspace, "cmd-s", "terminal::Export"),
            (workspace, "cmd-shift-r", "terminal::ToggleRecord"),
            (Some("ComposeBar"), "cmd-e", "compose::CycleLineEnding"),
            (terminal, "cmd-c", "terminal::Copy"),
            (terminal, "cmd-a", "terminal::SelectAll"),
            (terminal, "cmd-f", "terminal::Search"),
            (terminal, "cmd-alt-i", "terminal::ToggleFrameStats"),
            (terminal, "cmd-up", "terminal::ScrollToTop"),
            (terminal, "cmd-down", "terminal::JumpToBottom"),
            (Some("DevicesPanel"), "down", "devices::SelectNext"),
            (Some("DevicesPanel"), "up", "devices::SelectPrevious"),
            (Some("DevicesPanel"), "enter", "serial::Connect"),
            (Some("ComposeBar > Input"), "up", "compose::HistoryPrevious"),
            (Some("ComposeBar > Input"), "down", "compose::HistoryNext"),
            (terminal, "alt-z", "terminal::ToggleWrap"),
            (terminal, "alt-t", "terminal::CycleTimestamps"),
            (terminal, "alt-h", "terminal::ToggleHexView"),
            (terminal, "pageup", "terminal::PageUp"),
            (terminal, "pagedown", "terminal::PageDown"),
            (terminal, "home", "terminal::ScrollToTop"),
            (terminal, "end", "terminal::JumpToBottom"),
            (Some("TerminalSearch"), "escape", "terminal::DismissSearch"),
            (workspace, "cmd-i", "terminal::ToggleInline"),
            (Some("ComposeBar"), "cmd-alt-s", "compose::SaveAsCommand"),
            (inline, "cmd-v", "terminal::Paste"),
            (inline, "cmd-c", "terminal::Copy"),
            (inline, "cmd-a", "terminal::SelectAll"),
            (inline, "cmd-f", "terminal::Search"),
            (inline, "cmd-alt-i", "terminal::ToggleFrameStats"),
            (inline, "shift-pageup", "terminal::PageUp"),
            (inline, "shift-pagedown", "terminal::PageDown"),
            (inline, "cmd-up", "terminal::ScrollToTop"),
            (inline, "cmd-down", "terminal::JumpToBottom"),
        ]
    } else {
        vec![
            (None, "ctrl-q", "serialist::Quit"),
            (workspace, "ctrl-shift-k", "terminal::Clear"),
            (workspace, "ctrl-shift-w", "serial::Disconnect"),
            (workspace, "ctrl-p", "terminal::Pause"),
            (workspace, "ctrl-shift-s", "terminal::Export"),
            (workspace, "ctrl-shift-r", "terminal::ToggleRecord"),
            (
                Some("ComposeBar"),
                "ctrl-shift-e",
                "compose::CycleLineEnding",
            ),
            (terminal, "ctrl-shift-c", "terminal::Copy"),
            (terminal, "ctrl-shift-a", "terminal::SelectAll"),
            (terminal, "ctrl-shift-f", "terminal::Search"),
            (terminal, "ctrl-alt-i", "terminal::ToggleFrameStats"),
            (terminal, "ctrl-home", "terminal::ScrollToTop"),
            (terminal, "ctrl-end", "terminal::JumpToBottom"),
            (Some("DevicesPanel"), "down", "devices::SelectNext"),
            (Some("DevicesPanel"), "up", "devices::SelectPrevious"),
            (Some("DevicesPanel"), "enter", "serial::Connect"),
            (Some("ComposeBar > Input"), "up", "compose::HistoryPrevious"),
            (Some("ComposeBar > Input"), "down", "compose::HistoryNext"),
            (terminal, "alt-z", "terminal::ToggleWrap"),
            (terminal, "alt-t", "terminal::CycleTimestamps"),
            (terminal, "alt-h", "terminal::ToggleHexView"),
            (terminal, "pageup", "terminal::PageUp"),
            (terminal, "pagedown", "terminal::PageDown"),
            (terminal, "home", "terminal::ScrollToTop"),
            (terminal, "end", "terminal::JumpToBottom"),
            (Some("TerminalSearch"), "escape", "terminal::DismissSearch"),
            (workspace, "ctrl-i", "terminal::ToggleInline"),
            (Some("ComposeBar"), "ctrl-alt-s", "compose::SaveAsCommand"),
            (inline, "ctrl-i", "terminal::ToggleInline"),
            (inline, "ctrl-shift-v", "terminal::Paste"),
            (inline, "ctrl-shift-c", "terminal::Copy"),
            (inline, "ctrl-shift-a", "terminal::SelectAll"),
            (inline, "ctrl-shift-f", "terminal::Search"),
            (inline, "ctrl-alt-i", "terminal::ToggleFrameStats"),
            (inline, "shift-pageup", "terminal::PageUp"),
            (inline, "shift-pagedown", "terminal::PageDown"),
            (inline, "ctrl-shift-k", "terminal::Clear"),
            (inline, "ctrl-shift-w", "serial::Disconnect"),
            (inline, "ctrl-shift-p", "terminal::Pause"),
            (inline, "ctrl-shift-s", "terminal::Export"),
            (inline, "ctrl-shift-r", "terminal::ToggleRecord"),
        ]
    };
    keys.into_iter().collect()
}

#[test]
fn the_defaults_reproduce_the_apps_bindings() {
    for (platform, mac) in [
        (Platform::MacOs, true),
        (Platform::Linux, false),
        (Platform::Windows, false),
    ] {
        let keymap = Keymap::bundled(platform);
        let have: BTreeSet<_> = keymap
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.context.as_deref(),
                    entry.keystrokes.as_str(),
                    entry.action.as_ref().unwrap().name.as_str(),
                )
            })
            .collect();
        let expected = expected_bindings(mac);
        assert_eq!(have.len(), keymap.entries.len(), "{platform:?} has repeats");
        assert_eq!(have, expected, "{platform:?}");
    }
    // The primary shortcut modifier differs by platform.
    let text_of = |platform| {
        Keymap::bundled(platform)
            .entries
            .iter()
            .map(|entry| entry.keystrokes.clone())
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert!(text_of(Platform::MacOs).contains("cmd-"));
    assert!(!text_of(Platform::Linux).contains("cmd-"));
    assert!(!text_of(Platform::Windows).contains("cmd-"));
    assert_eq!(
        Keymap::bundled_default(),
        Keymap::bundled(Platform::current())
    );
}
