//! Saved-command collection files across versions: a collection a later 1.x build wrote
//! must load and survive an edit in this one, and the 1.0 format must keep its meaning.
//!
//! The fixtures are in `tests/fixtures/commands/`.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use serialist_core::commands::{LoadedCollection, ParamValues, PayloadError};
use serialist_core::{
    CollectionSource, Command, CommandCollection, CommandRef, CommandStore, Expect, LineEnding,
    Param, ParamKind, Payload,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/commands")
        .join(name)
}

fn load(name: &str) -> LoadedCollection {
    let path = fixture(name);
    CommandCollection::load(&path, CollectionSource::User(path.clone()))
        .unwrap_or_else(|err| panic!("{err}"))
}

/// A JSON-with-comments document as a plain JSON value.
fn value_of(text: &str) -> Value {
    jsonc_parser::parse_to_serde_value(text, &Default::default()).unwrap()
}

#[test]
fn a_collection_from_a_later_build_loads_what_it_can() {
    let loaded = load("from_the_future.json");
    let collection = &loaded.collection;
    assert_eq!(collection.name, "Future bench");
    let names: Vec<_> = collection
        .commands()
        .map(|(group, command)| format!("{}/{}", group.name, command.name))
        .collect();
    assert_eq!(names, ["Modem/Version", "Modem/Reset", "Later/Frame"]);

    // One warning per command (and group) it could not read, naming the group and the
    // command, by name or by position.
    let skipped: Vec<_> = loaded
        .warnings
        .iter()
        .filter(|w| w.contains("skipped"))
        .collect();
    assert_eq!(skipped.len(), 4, "{:#?}", loaded.warnings);
    let one = |needle: &str| {
        let found: Vec<_> = skipped.iter().filter(|w| w.contains(needle)).collect();
        assert_eq!(found.len(), 1, "{needle:?} in {skipped:#?}");
        found[0].to_string()
    };
    assert!(one("`Modem` \u{203a} `Base64 blob`").contains("no text, hex, codec or script"));
    assert!(one("`Modem` \u{203a} `Float`").contains("float"));
    one("`Later` \u{203a} command 1");
    one("group 2");

    // Each readable command means what it says.
    let version = collection.find("Version").unwrap().1;
    assert_eq!(version.payload, Payload::Text("AT+VER?".into()));
    assert_eq!(version.params[0].kind, ParamKind::Int);
    assert_eq!(version.expect, Some(Expect::new("^OK", 1000)));
    assert!(!version.carried.is_empty());
    assert!(
        loaded
            .warnings
            .iter()
            .any(|w| w.contains("unknown key `groups[0].commands[0].colour`")),
        "{:#?}",
        loaded.warnings
    );
}

#[test]
fn editing_a_collection_from_a_later_build_keeps_the_rest_of_it() {
    let dir = std::env::temp_dir().join(format!(
        "serialist-future-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let text = std::fs::read_to_string(fixture("from_the_future.json")).unwrap();
    let file = dir.join("future.json");
    std::fs::write(&file, &text).unwrap();

    let mut store = CommandStore::empty();
    let loaded = CommandCollection::load(&file, CollectionSource::User(file.clone())).unwrap();
    store.set_collection(loaded.collection);
    // The edit a form makes: the command built afresh, carrying nothing.
    let reset = CommandRef::new("Future bench", "Modem", "Reset");
    store
        .update_command(&reset, Command::hex("Reset", "FF 01"))
        .unwrap();
    store.save_collection("Future bench").unwrap();

    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let mut want = value_of(&text);
    want["groups"][0]["commands"][3]["payload"]["hex"] = json!("FF 01");
    assert_eq!(saved, want);

    // Deleting a group deletes what it carried; the group the build could not read
    // stays where it was.
    store.remove_group("Future bench", "Modem").unwrap();
    store.save_collection("Future bench").unwrap();
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let groups = want["groups"].as_array_mut().unwrap();
    groups.remove(0);
    assert_eq!(saved, want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_1_0_format_loads_without_warnings_and_means_what_it_says() {
    let loaded = load("format_1_0.json");
    assert!(loaded.warnings.is_empty(), "{:#?}", loaded.warnings);
    let collection = &loaded.collection;
    assert_eq!(collection.name, "Format 1.0");
    let groups: Vec<_> = collection.groups.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(groups, ["Text", "Binary", "Empty"]);
    assert!(collection.groups[2].commands.is_empty());
    for (_, command) in collection.commands() {
        assert!(command.carried.is_empty(), "{}", command.name);
        assert!(
            command.problems().is_empty(),
            "{}: {:?}",
            command.name,
            command.problems()
        );
    }
    let get = |name: &str| collection.find(name).unwrap().1;
    let none = ParamValues::new();

    let escapes = get("Every escape");
    assert_eq!(
        escapes.description,
        "The text escapes and a text placeholder"
    );
    assert_eq!(escapes.eol, Some(LineEnding::None));
    assert_eq!(escapes.expect, Some(Expect::new("^OK|^ERROR", 250)));
    assert_eq!(escapes.keybinding.as_deref(), Some("ctrl-alt-1"));
    assert_eq!(
        escapes.params,
        [Param::new("who", ParamKind::Text)
            .with_label("Who")
            .with_default("world")]
    );
    assert_eq!(
        escapes.encode(&none, LineEnding::Crlf).unwrap(),
        b"A\r\n\t\0\\{}\x7F world"
    );
    assert_eq!(
        escapes
            .encode(&ParamValues::new().with("who", "you"), LineEnding::Crlf)
            .unwrap(),
        b"A\r\n\t\0\\{}\x7F you"
    );

    let numbers = get("Numbers in text");
    assert_eq!(numbers.params[0].default.as_deref(), Some("42"));
    assert_eq!(
        numbers.encode(&none, LineEnding::Crlf).unwrap(),
        b"AT+N=42,0F15\r"
    );

    // Without its own `eol`, a text command takes the session's; `timeout_ms` defaults.
    let session = get("Session ending");
    assert_eq!(session.eol, None);
    assert_eq!(session.expect, Some(Expect::new("^OK", 1000)));
    assert_eq!(session.encode(&none, LineEnding::Lf).unwrap(), b"AT\n");

    let hex = get("Hex");
    assert_eq!(
        hex.encode(&none, LineEnding::Crlf).unwrap(),
        [
            0x05, 0x5A, 0x15, 0x0F, 0x15, 0x0F, 0x0F, 0x15, 0x07, 0xAA, 0xBB, b'\n'
        ]
    );

    let codec = get("Codec");
    let Payload::Codec {
        codec: name,
        fields,
    } = &codec.payload
    else {
        panic!("a codec payload");
    };
    assert_eq!(name, "airoha-race");
    assert_eq!(
        Value::Object(fields.clone()),
        json!({ "command": "race_version", "cmd_id": "{{id}}" })
    );
    let expect = codec.expect.as_ref().unwrap();
    assert_eq!(expect.pattern, "");
    assert_eq!(expect.timeout_ms, 500);
    assert_eq!(
        expect.frame.clone().map(Value::Object),
        Some(json!({ "kind": "response", "cmd_id": "0x0F15" }))
    );
    assert_eq!(codec.placeholders(), ["id"]);
    assert_eq!(
        codec.encode(&none, LineEnding::Crlf),
        Err(PayloadError::CodecUnavailable)
    );

    let both = get("Both expects").expect.as_ref().unwrap();
    assert_eq!(both.pattern, "^RACE");
    assert_eq!(
        both.frame.clone().map(Value::Object),
        Some(json!({ "kind": "response" }))
    );

    let script = get("Script");
    assert_eq!(script.payload.script(), Some(Path::new("probe.lua")));
    assert_eq!(script.keybinding.as_deref(), Some("cmd-shift-9"));
}

// ---- Round trips with unknown keys ----

/// A key no build reads.
fn unknown_key() -> impl Strategy<Value = String> {
    "[a-z]{1,8}".prop_map(|name| format!("x_{name}"))
}

fn unknown_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| json!(n)),
        "[ -~]{0,12}".prop_map(Value::String),
    ];
    leaf.prop_recursive(2, 8, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
            prop::collection::btree_map(unknown_key(), inner, 0..3)
                .prop_map(|map| Value::Object(map.into_iter().collect())),
        ]
    })
}

/// Unknown keys to sprinkle into one object.
fn unknown_keys() -> impl Strategy<Value = Map<String, Value>> {
    prop::collection::btree_map(unknown_key(), unknown_value(), 0..3)
        .prop_map(|map| map.into_iter().collect())
}

fn with_unknown(mut known: Value, unknown: Map<String, Value>) -> Value {
    known.as_object_mut().unwrap().extend(unknown);
    known
}

fn name() -> impl Strategy<Value = String> {
    "[A-Za-z][A-Za-z0-9 ]{0,10}"
}

fn param() -> impl Strategy<Value = Value> {
    (
        "[a-z][a-z0-9_]{0,5}",
        prop::option::of(name()),
        prop::option::of("[0-9]{1,3}"),
        prop::sample::select(vec!["text", "int", "hex16"]),
        unknown_keys(),
    )
        .prop_map(|(name, label, default, kind, unknown)| {
            let mut known = json!({ "name": name, "kind": kind });
            if let Some(label) = label {
                known["label"] = json!(label);
            }
            if let Some(default) = default {
                known["default"] = json!(default);
            }
            with_unknown(known, unknown)
        })
}

fn payload() -> impl Strategy<Value = Value> {
    let form = prop_oneof![
        "[ -~]{0,12}".prop_map(|text| json!({ "text": text.replace('\\', "") })),
        "([0-9A-F]{2} ){0,4}".prop_map(|hex| json!({ "hex": hex.trim() })),
        name().prop_map(|codec| json!({ "codec": codec, "fields": { "command": "x" } })),
        "[a-z]{1,8}".prop_map(|script| json!({ "script": format!("{script}.lua") })),
    ];
    (form, unknown_keys()).prop_map(|(form, unknown)| with_unknown(form, unknown))
}

fn expect() -> impl Strategy<Value = Value> {
    ("[a-z^]{1,6}", 0u64..10_000, any::<bool>(), unknown_keys()).prop_map(
        |(pattern, timeout, frame, unknown)| {
            let mut known = json!({ "pattern": pattern, "timeout_ms": timeout });
            if frame {
                known["frame"] = json!({ "kind": "response" });
            }
            with_unknown(known, unknown)
        },
    )
}

fn command() -> impl Strategy<Value = Value> {
    (
        name(),
        prop::option::of(name()),
        payload(),
        prop::option::of(prop::sample::select(vec!["none", "cr", "lf", "crlf"])),
        prop::option::of(expect()),
        prop::option::of("(cmd|ctrl)-[0-9]"),
        prop::collection::vec(param(), 0..3),
        unknown_keys(),
    )
        .prop_map(
            |(name, description, payload, eol, expect, keybinding, params, unknown)| {
                let mut known = json!({ "name": name, "payload": payload });
                if let Some(description) = description {
                    known["description"] = json!(description);
                }
                if let Some(eol) = eol {
                    known["eol"] = json!(eol);
                }
                if let Some(expect) = expect {
                    known["expect"] = expect;
                }
                if let Some(keybinding) = keybinding {
                    known["keybinding"] = json!(keybinding);
                }
                if !params.is_empty() {
                    known["params"] = Value::Array(params);
                }
                with_unknown(known, unknown)
            },
        )
}

/// A command no build reads: a payload form that does not exist.
fn unreadable() -> impl Strategy<Value = Value> {
    (prop::option::of(name()), unknown_value()).prop_map(|(name, blob)| match name {
        Some(name) => json!({ "name": name, "payload": { "x_form": blob } }),
        None => json!({ "payload": { "x_form": blob } }),
    })
}

fn collection() -> impl Strategy<Value = Value> {
    let entry = prop_oneof![4 => command(), 1 => unreadable()];
    let group = (name(), prop::collection::vec(entry, 0..4), unknown_keys()).prop_map(
        |(name, commands, unknown)| {
            with_unknown(json!({ "name": name, "commands": commands }), unknown)
        },
    );
    (name(), prop::collection::vec(group, 0..4), unknown_keys()).prop_map(
        |(name, groups, unknown)| with_unknown(json!({ "name": name, "groups": groups }), unknown),
    )
}

proptest! {
    #[test]
    fn parse_save_parse_keeps_the_model_and_the_unknown_keys(document in collection()) {
        let origin = Path::new("p.json");
        let source = CollectionSource::User(origin.to_path_buf());
        let text = serde_json::to_string_pretty(&document).unwrap();
        let first = CommandCollection::parse(&text, source.clone(), origin).unwrap().collection;
        let saved = first.to_json();
        let second = CommandCollection::parse(&saved, source, origin).unwrap().collection;
        // The same model, carrying the same unknown keys and unreadable commands.
        prop_assert_eq!(&first, &second);
        // And the file says what it said before.
        let again: Value = serde_json::from_str(&saved).unwrap();
        prop_assert_eq!(again, document);
    }
}
