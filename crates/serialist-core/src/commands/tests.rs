use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::{Map, json};

use super::*;
use crate::settings::{ConfigPaths, LineEnding};
use crate::test_util::TempDir;

// ---- Encoding ----

/// Encodes with no values and no session line ending.
fn bytes(command: &Command) -> Vec<u8> {
    command
        .encode(&ParamValues::new(), LineEnding::None)
        .expect("encodes")
}

fn err(command: &Command, values: &ParamValues) -> PayloadError {
    command
        .encode(values, LineEnding::None)
        .expect_err("should not encode")
}

fn hex16(name: &str) -> Param {
    Param::new(name, ParamKind::Hex16)
}

#[test]
fn a_text_payload_takes_the_session_line_ending() {
    let command = Command::text("t", "AT");
    let values = ParamValues::new();
    for (eol, want) in [
        (LineEnding::None, &b"AT"[..]),
        (LineEnding::Cr, b"AT\r"),
        (LineEnding::Lf, b"AT\n"),
        (LineEnding::Crlf, b"AT\r\n"),
    ] {
        assert_eq!(command.encode(&values, eol).unwrap(), want, "{eol:?}");
    }
}

#[test]
fn a_commands_own_eol_wins_over_the_session() {
    let values = ParamValues::new();
    for (own, want) in [
        (LineEnding::None, &b"AT"[..]),
        (LineEnding::Cr, b"AT\r"),
        (LineEnding::Lf, b"AT\n"),
        (LineEnding::Crlf, b"AT\r\n"),
    ] {
        let command = Command::text("t", "AT").with_eol(own);
        // Whatever the session says.
        for session in LineEnding::ALL {
            assert_eq!(command.encode(&values, session).unwrap(), want);
        }
    }
}

#[test]
fn text_escapes() {
    let cases: &[(&str, &[u8])] = &[
        ("a\\rb", b"a\rb"),
        ("a\\nb", b"a\nb"),
        ("a\\tb", b"a\tb"),
        ("a\\0b", b"a\0b"),
        ("a\\\\b", b"a\\b"),
        ("\\{\\}", b"{}"),
        ("\\x41\\x7e", b"A~"),
        ("\\xFF\\x00", &[0xFF, 0x00]),
        ("caf\u{e9}", "caf\u{e9}".as_bytes()),
        // Real control characters, as JSON's own escapes produce them, pass through.
        ("a\rb\n", b"a\rb\n"),
        // A lone brace is not a placeholder.
        ("{x} }}", b"{x} }}"),
    ];
    for (text, want) in cases {
        assert_eq!(bytes(&Command::text("t", *text)), *want, "{text:?}");
    }
}

#[test]
fn an_escaped_brace_pair_is_literal() {
    assert_eq!(bytes(&Command::text("t", "\\{{a}}")), b"{{a}}");
}

#[test]
fn bad_escapes_are_errors() {
    let cases = [
        ("a\\qb", "q"),
        ("trailing\\", ""),
        ("\\x4", "x4"),
        ("\\xZZ", "xZZ"),
        ("\\x", "x"),
    ];
    for (text, seen) in cases {
        let got = err(&Command::text("t", text), &ParamValues::new());
        assert_eq!(got, PayloadError::BadEscape(seen.to_owned()), "{text:?}");
    }
}

#[test]
fn placeholders_take_values_then_defaults() {
    let command = Command::text("t", "SET {{key}}={{val}}")
        .with_param(Param::new("key", ParamKind::Text).with_default("mode"))
        .with_param(Param::new("val", ParamKind::Text));
    // The default fills in `key`; `val` has none.
    let values = ParamValues::new().with("val", "fast");
    assert_eq!(bytes_with(&command, &values), b"SET mode=fast");
    // A value beats the default.
    let values = values.with("key", "speed");
    assert_eq!(bytes_with(&command, &values), b"SET speed=fast");
    // Spaces inside the braces are fine, and an empty value is a value.
    let spaced = Command::text("t", "<{{ a }}>").with_param(Param::new("a", ParamKind::Text));
    let values = ParamValues::new().with("a", "");
    assert_eq!(bytes_with(&spaced, &values), b"<>");
}

fn bytes_with(command: &Command, values: &ParamValues) -> Vec<u8> {
    command.encode(values, LineEnding::None).expect("encodes")
}

#[test]
fn a_missing_parameter_is_named() {
    let command =
        Command::text("t", "GET {{slot}}").with_param(Param::new("slot", ParamKind::Text));
    let got = err(&command, &ParamValues::new());
    assert_eq!(got, PayloadError::MissingParam("slot".to_owned()));
    assert!(got.to_string().contains("slot"), "{got}");
}

#[test]
fn a_placeholder_without_a_param_is_an_error_naming_it() {
    let command = Command::text("t", "GET {{nope}}");
    let got = err(&command, &ParamValues::new().with("nope", "1"));
    assert_eq!(got, PayloadError::UnknownParam("nope".to_owned()));
    assert!(got.to_string().contains("{{nope}}"), "{got}");
    // The same in a hex payload.
    let command = Command::hex("t", "05 {{other}}");
    assert_eq!(
        err(&command, &ParamValues::new()),
        PayloadError::UnknownParam("other".to_owned())
    );
}

#[test]
fn broken_placeholders_are_errors() {
    let unterminated = Command::text("t", "a {{b");
    assert_eq!(
        err(&unterminated, &ParamValues::new()),
        PayloadError::UnterminatedPlaceholder
    );
    for (text, inner) in [
        ("{{}}", ""),
        ("{{a b}}", "a b"),
        ("{{a:}}", "a:"),
        ("{{:be}}", ":be"),
    ] {
        assert_eq!(
            err(&Command::text("t", text), &ParamValues::new()),
            PayloadError::BadPlaceholder(inner.to_owned()),
            "{text:?}"
        );
    }
}

#[test]
fn a_value_is_never_expanded_again() {
    let command = Command::text("t", "{{a}}").with_param(Param::new("a", ParamKind::Text));
    let values = ParamValues::new().with("a", "\\r{{a}}\\x41");
    assert_eq!(bytes_with(&command, &values), b"\\r{{a}}\\x41");
}

#[test]
fn int_parameters_are_checked_and_written_in_decimal() {
    let command = Command::text("t", "N={{n}}").with_param(Param::new("n", ParamKind::Int));
    for (given, want) in [
        ("42", "N=42"),
        (" 7 ", "N=7"),
        ("0x10", "N=16"),
        ("-5", "N=-5"),
        ("+9", "N=9"),
        ("007", "N=7"),
    ] {
        let values = ParamValues::new().with("n", given);
        assert_eq!(bytes_with(&command, &values), want.as_bytes(), "{given}");
    }
    for bad in ["", "abc", "1.5", "--5", "0x", "0xg", "0x-1", "5 5"] {
        let values = ParamValues::new().with("n", bad);
        assert!(
            matches!(err(&command, &values), PayloadError::BadParamValue { ref name, .. } if name == "n"),
            "{bad:?}"
        );
    }
}

#[test]
fn hex16_parameters_are_checked_and_written_as_four_digits() {
    let command = Command::text("t", "ID={{id}}").with_param(hex16("id"));
    for (given, want) in [
        ("0x0F15", "ID=0F15"),
        ("0f15", "ID=0F15"),
        ("F", "ID=000F"),
        ("0XABCD", "ID=ABCD"),
        (" 12 ", "ID=0012"),
    ] {
        let values = ParamValues::new().with("id", given);
        assert_eq!(bytes_with(&command, &values), want.as_bytes(), "{given}");
    }
    for bad in ["", "12345", "0x", "zz", "1 2", "-1"] {
        let values = ParamValues::new().with("id", bad);
        assert!(
            matches!(err(&command, &values), PayloadError::BadParamValue { .. }),
            "{bad:?}"
        );
    }
}

#[test]
fn modifiers_do_not_apply_to_text_payloads() {
    let command = Command::text("t", "{{id:be}}").with_param(hex16("id"));
    let values = ParamValues::new().with("id", "1");
    assert!(matches!(
        err(&command, &values),
        PayloadError::BadModifier { .. }
    ));
}

#[test]
fn hex_payloads_tolerate_separators_and_prefixes() {
    let want = [0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F];
    for text in [
        "05 5A 02 00 15 0F",
        "05,5A,02,00,15,0F",
        "05, 5A, 02, 00, 15, 0F",
        "0x05 0x5a 0x02 0x00 0x15 0x0f",
        "055A0200150F",
        "05 5A\n02 00\r\n15 0F\n",
        "  05\t5a 0200 150f  ",
        "0X055A 0200,150F",
    ] {
        assert_eq!(bytes(&Command::hex("h", text)), want, "{text:?}");
    }
    assert_eq!(bytes(&Command::hex("h", "")), b"");
    assert_eq!(bytes(&Command::hex("h", " \n ")), b"");
}

#[test]
fn bad_hex_is_named() {
    let cases = [
        ("05 5G", "5G"),
        ("05 5", "5"),
        ("0x", "0x"),
        ("12 345", "345"),
        ("\\x41", "\\x41"),
        ("05;5A", "05;5A"),
    ];
    for (text, token) in cases {
        match err(&Command::hex("h", text), &ParamValues::new()) {
            PayloadError::BadHex { token: got, .. } => assert_eq!(got, token, "{text:?}"),
            other => panic!("{text:?}: {other:?}"),
        }
    }
    let shown = err(&Command::hex("h", "0Z"), &ParamValues::new()).to_string();
    assert!(
        shown.contains("0Z") && shown.contains("hex digit"),
        "{shown}"
    );
}

#[test]
fn hex_payloads_expand_hex16_little_endian_by_default() {
    let command = Command::hex("h", "05 5A 02 00 {{id}}").with_param(hex16("id"));
    let values = ParamValues::new().with("id", "0x0F15");
    assert_eq!(bytes_with(&command, &values), [5, 0x5A, 2, 0, 0x15, 0x0F]);
    let be = Command::hex("h", "05 5A 02 00 {{id:be}}").with_param(hex16("id"));
    assert_eq!(bytes_with(&be, &values), [5, 0x5A, 2, 0, 0x0F, 0x15]);
    let le = Command::hex("h", "{{id:le}} {{ id : LE }}").with_param(hex16("id"));
    assert_eq!(bytes_with(&le, &values), [0x15, 0x0F, 0x15, 0x0F]);
    // Runs of digits meet across a placeholder.
    let joined = Command::hex("h", "05{{id}}").with_param(hex16("id"));
    assert_eq!(bytes_with(&joined, &values), [5, 0x15, 0x0F]);
}

#[test]
fn hex_payloads_expand_int_as_one_byte() {
    let command = Command::hex("h", "AA {{n}}").with_param(Param::new("n", ParamKind::Int));
    for (given, want) in [("0", 0u8), ("255", 255), ("0x7f", 0x7F)] {
        let values = ParamValues::new().with("n", given);
        assert_eq!(bytes_with(&command, &values), [0xAA, want], "{given}");
    }
    for bad in ["256", "-1", "x"] {
        let values = ParamValues::new().with("n", bad);
        assert!(
            matches!(err(&command, &values), PayloadError::BadParamValue { .. }),
            "{bad}"
        );
    }
}

#[test]
fn hex_payloads_splice_text_parameters_as_hex() {
    let command =
        Command::hex("h", "01 {{data}} 02").with_param(Param::new("data", ParamKind::Text));
    let values = ParamValues::new().with("data", "DE AD,be ef");
    assert_eq!(
        bytes_with(&command, &values),
        [1, 0xDE, 0xAD, 0xBE, 0xEF, 2]
    );
    let bad = ParamValues::new().with("data", "not hex");
    assert!(matches!(err(&command, &bad), PayloadError::BadHex { .. }));
}

#[test]
fn bad_modifiers_in_hex_payloads() {
    let unknown = Command::hex("h", "{{id:xx}}").with_param(hex16("id"));
    let values = ParamValues::new().with("id", "1");
    assert!(matches!(
        err(&unknown, &values),
        PayloadError::BadModifier { .. }
    ));
    // `:be` only means something for hex16.
    let int = Command::hex("h", "{{n:be}}").with_param(Param::new("n", ParamKind::Int));
    let values = ParamValues::new().with("n", "1");
    assert!(matches!(
        err(&int, &values),
        PayloadError::BadModifier { .. }
    ));
}

#[test]
fn hex_and_codec_payloads_send_no_line_ending_unless_told_to() {
    let values = ParamValues::new();
    let hex = Command::hex("h", "05 5A");
    assert_eq!(hex.encode(&values, LineEnding::Crlf).unwrap(), [5, 0x5A]);
    let told = Command::hex("h", "05 5A").with_eol(LineEnding::Lf);
    assert_eq!(
        told.encode(&values, LineEnding::Crlf).unwrap(),
        [5, 0x5A, b'\n']
    );
}

#[test]
fn a_codec_payload_is_unavailable_for_now() {
    let mut fields = Map::new();
    fields.insert("cmd_id".to_owned(), json!("0x0F15"));
    let command = Command::new(
        "c",
        Payload::Codec {
            codec: "airoha-race".into(),
            fields,
        },
    );
    assert_eq!(
        command.encode(&ParamValues::new(), LineEnding::Crlf),
        Err(PayloadError::CodecUnavailable)
    );
}

#[test]
fn placeholders_lists_names_once_in_order() {
    let text = Command::text("t", "{{b}} {{a}} {{b}} \\{{x}}");
    // `\{{x}}` is a literal, not a placeholder.
    assert_eq!(text.placeholders(), ["b", "a"]);
    let hex = Command::hex("h", "05 {{ id:be }} {{n}}");
    assert_eq!(hex.placeholders(), ["id", "n"]);
    assert!(Command::text("t", "AT").placeholders().is_empty());
    // Names found before a syntax error still count.
    assert_eq!(Command::text("t", "{{a}} {{b").placeholders(), ["a"]);
    let mut fields = Map::new();
    fields.insert(
        "cmd".to_owned(),
        json!({ "id": "{{id}}", "list": ["{{n}}", 5] }),
    );
    let codec = Command::new(
        "c",
        Payload::Codec {
            codec: "x".into(),
            fields,
        },
    );
    assert_eq!(codec.placeholders(), ["id", "n"]);
}

#[test]
fn defaults_are_the_prefilled_values() {
    let command = Command::text("t", "{{a}}{{b}}")
        .with_param(Param::new("a", ParamKind::Text).with_default("x"))
        .with_param(Param::new("b", ParamKind::Text));
    assert_eq!(command.defaults(), ParamValues::new().with("a", "x"));
    assert_eq!(command.prompt_params().len(), 2);
    assert_eq!(command.prompt_params()[1].display_label(), "b");
    assert_eq!(
        Param::new("a", ParamKind::Int)
            .with_label("Amount")
            .display_label(),
        "Amount"
    );
}

#[test]
fn problems_finds_what_will_not_send() {
    let good = Command::text("Version", "AT+VER?").with_expect("^OK", 500);
    assert!(good.problems().is_empty(), "{:?}", good.problems());

    let has = |command: &Command, needle: &str| {
        let found = command.problems();
        assert!(
            found.iter().any(|p| p.contains(needle)),
            "no problem with {needle:?} in {found:?}"
        );
    };
    has(&Command::text(" ", "AT"), "no name");
    has(&Command::text("t", "bad \\q"), "escape");
    has(&Command::text("t", "{{ghost}}"), "ghost");
    has(&Command::hex("t", "0Z"), "hex");
    has(
        &Command::text("t", "AT").with_expect("(", 100),
        "expect pattern",
    );
    has(
        &Command::text("t", "AT").with_expect("ok", 0),
        "timeout_ms is 0",
    );
    has(
        &Command::text("t", "AT").with_keybinding("  "),
        "keybinding",
    );
    has(
        &Command::text("t", "{{a}}").with_param(Param::new("a b", ParamKind::Text)),
        "not a parameter name",
    );
    has(
        &Command::text("t", "{{a}}")
            .with_param(Param::new("a", ParamKind::Text))
            .with_param(Param::new("a", ParamKind::Text)),
        "declared twice",
    );
    has(
        &Command::text("t", "{{n}}")
            .with_param(Param::new("n", ParamKind::Int).with_default("many")),
        "default of `n`",
    );
    // A hex payload whose default does not fit a byte.
    has(
        &Command::hex("t", "{{n}}").with_param(Param::new("n", ParamKind::Int).with_default("300")),
        "one byte",
    );
    // A codec command is fine until codecs exist; it just cannot be sent.
    let codec = Command::new(
        "c",
        Payload::Codec {
            codec: "airoha-race".into(),
            fields: Map::new(),
        },
    );
    assert!(codec.problems().is_empty());
}

#[test]
fn param_validate_is_what_a_prompt_can_call() {
    assert!(hex16("id").validate("0x1234").is_ok());
    assert!(hex16("id").validate("nope").is_err());
    assert!(Param::new("n", ParamKind::Int).validate("12").is_ok());
    assert!(Param::new("n", ParamKind::Int).validate("").is_err());
    assert!(
        Param::new("t", ParamKind::Text)
            .validate("anything at all")
            .is_ok()
    );
}

// ---- The file format ----

/// The example from the plan: comments, trailing commas, every key.
const FULL: &str = r#"
// A collection of my own.
{
  "name": "Airoha bring-up",
  "groups": [
    {
      "name": "Basics",
      "commands": [
        {
          "name": "Version",
          "description": "Ask the device for its firmware version",
          "payload": { "text": "AT+VER?" }, // trailing commas below are fine
          "eol": "crlf",
          "expect": { "pattern": "^OK|^ERROR", "timeout_ms": 1000 },
          "keybinding": "cmd-1",
          "params": [
            { "name": "id", "label": "Command id", "default": "0x0F15", "kind": "hex16" },
          ],
        },
        { "name": "Frame", "payload": { "hex": "05 5A 02 00 15 0F" } },
        {
          "name": "Codec",
          "payload": { "codec": "airoha-race", "fields": { "cmd_id": "0x0F15", "n": 3 } },
        },
      ],
    },
    { "name": "Empty" },
  ],
}
"#;

fn parse(text: &str) -> LoadedCollection {
    CommandCollection::parse(text, CollectionSource::Bundled, "test.json".as_ref())
        .unwrap_or_else(|err| panic!("{err}"))
}

fn parse_err(text: &str) -> CommandsError {
    CommandCollection::parse(text, CollectionSource::Bundled, "test.json".as_ref())
        .expect_err("should not parse")
}

#[test]
fn the_plans_example_file_parses_field_by_field() {
    let loaded = parse(FULL);
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let collection = loaded.collection;
    assert_eq!(collection.name, "Airoha bring-up");
    assert_eq!(collection.groups.len(), 2);
    assert_eq!(collection.groups[0].name, "Basics");
    assert!(collection.groups[1].commands.is_empty());

    let version = &collection.groups[0].commands[0];
    assert_eq!(version.name, "Version");
    assert_eq!(
        version.description,
        "Ask the device for its firmware version"
    );
    assert_eq!(version.payload, Payload::Text("AT+VER?".into()));
    assert_eq!(version.eol, Some(LineEnding::Crlf));
    assert_eq!(version.expect, Some(Expect::new("^OK|^ERROR", 1000)));
    assert_eq!(version.keybinding.as_deref(), Some("cmd-1"));
    assert_eq!(
        version.params,
        [Param::new("id", ParamKind::Hex16)
            .with_label("Command id")
            .with_default("0x0F15")]
    );

    let frame = &collection.groups[0].commands[1];
    assert_eq!(frame.payload, Payload::Hex("05 5A 02 00 15 0F".into()));
    assert_eq!(
        (frame.eol, &frame.expect, &frame.keybinding),
        (None, &None, &None)
    );
    assert_eq!(frame.description, "");
    assert!(frame.params.is_empty());

    let Payload::Codec { codec, fields } = &collection.groups[0].commands[2].payload else {
        panic!("a codec payload");
    };
    assert_eq!(codec, "airoha-race");
    assert_eq!(fields["cmd_id"], json!("0x0F15"));
    assert_eq!(fields["n"], json!(3));
}

#[test]
fn defaults_for_omitted_keys() {
    let loaded = parse(
        r#"{ "groups": [ { "name": "G", "commands": [
            { "name": "C", "payload": { "text": "x" }, "expect": { "pattern": "ok" },
              "params": [ { "name": "n", "default": 5 }, { "name": "b", "default": true } ] } ] } ] }"#,
    );
    // Named after the file when it has no name.
    assert_eq!(loaded.collection.name, "test");
    let command = &loaded.collection.groups[0].commands[0];
    assert_eq!(
        command.expect.as_ref().unwrap().timeout_ms,
        DEFAULT_EXPECT_TIMEOUT_MS
    );
    assert_eq!(command.params[0].kind, ParamKind::Text);
    assert_eq!(command.params[0].default.as_deref(), Some("5"));
    assert_eq!(command.params[1].default.as_deref(), Some("true"));
}

#[test]
fn an_expect_can_be_a_frame_predicate() {
    let loaded = parse(
        r#"{ "groups": [ { "name": "G", "commands": [
            { "name": "Version", "payload": { "codec": "airoha-race", "fields": { "command": "race_version" } },
              "expect": { "frame": { "kind": "response", "cmd_id": "0x0F15" }, "timeout_ms": 500 } },
            { "name": "Both", "payload": { "text": "x" },
              "expect": { "pattern": "^OK", "frame": { "kind": "response" } } },
            { "name": "Neither", "payload": { "text": "x" }, "expect": { "timeout_ms": 5 } },
            { "name": "Empty", "payload": { "text": "x" }, "expect": { "frame": {} } } ] } ] }"#,
    );
    let commands = &loaded.collection.groups[0].commands;
    let version = commands[0].expect.as_ref().unwrap();
    assert_eq!(version.pattern, "");
    assert_eq!(version.timeout_ms, 500);
    let frame = version.frame.as_ref().unwrap();
    assert_eq!(frame["kind"], json!("response"));
    assert_eq!(frame["cmd_id"], json!("0x0F15"));
    assert!(commands[0].problems().is_empty(), "{:?}", commands[0].problems());
    let both = commands[1].expect.as_ref().unwrap();
    assert_eq!(both.pattern, "^OK");
    assert!(both.frame.is_some());
    assert_eq!(both.timeout_ms, DEFAULT_EXPECT_TIMEOUT_MS);
    assert!(commands[1].problems().is_empty());
    assert!(
        commands[2]
            .problems()
            .contains(&"expect needs a pattern or a frame".to_owned())
    );
    assert!(
        commands[3]
            .problems()
            .contains(&"expect frame names nothing to match".to_owned())
    );

    // A frame-only expect writes no empty pattern, and reads back the same.
    let written = serde_json::to_value(version).unwrap();
    assert_eq!(
        written,
        json!({ "timeout_ms": 500, "frame": { "kind": "response", "cmd_id": "0x0F15" } })
    );
    let read: Expect = serde_json::from_value(written).unwrap();
    assert_eq!(&read, version);
    let mut predicate = Map::new();
    predicate.insert("kind".into(), json!("response"));
    assert_eq!(
        Expect::frame(predicate.clone(), 500).frame,
        Some(predicate)
    );
}

#[test]
fn an_empty_or_comment_only_file_is_an_empty_collection() {
    for text in ["", "  \n", "// nothing yet\n"] {
        let loaded = parse(text);
        assert_eq!(loaded.collection.name, "test");
        assert!(loaded.collection.groups.is_empty());
        assert!(loaded.warnings.is_empty());
    }
    // `{}` is one too.
    assert!(parse("{}").collection.groups.is_empty());
}

#[test]
fn syntax_errors_carry_a_position() {
    let CommandsError::Invalid {
        line,
        message,
        file,
        ..
    } = parse_err("{\n  \"name\": \"X\",\n  \"groups\": [ }\n")
    else {
        panic!("a syntax error");
    };
    assert_eq!(file, PathBuf::from("test.json"));
    assert_eq!(line, 3);
    assert!(!message.is_empty());
}

#[test]
fn a_script_payload_runs_a_script_and_has_no_bytes() {
    let loaded = parse(
        r#"{ "name": "Scripts", "groups": [ { "name": "G", "commands": [
            { "name": "Probe", "payload": { "script": "lib/probe.lua" }, "keybinding": "cmd-9" } ] } ] }"#,
    );
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    let command = &loaded.collection.groups[0].commands[0];
    assert_eq!(
        command.payload,
        Payload::Script {
            path: PathBuf::from("lib/probe.lua")
        }
    );
    assert_eq!(
        command.payload.script(),
        Some(std::path::Path::new("lib/probe.lua"))
    );
    assert_eq!(Payload::Text("AT".into()).script(), None);
    // Nothing may send it as bytes, and that is not a problem with the command.
    let error = command
        .encode(&ParamValues::new(), LineEnding::Crlf)
        .unwrap_err();
    assert_eq!(error, PayloadError::ScriptPayload("lib/probe.lua".into()));
    assert_eq!(
        error.to_string(),
        "this command runs the script lib/probe.lua; it has no bytes to send"
    );
    assert!(command.problems().is_empty(), "{:?}", command.problems());
    assert!(command.placeholders().is_empty());
    // It writes back in the same form.
    let json = loaded.collection.to_json();
    assert!(json.contains(r#""script": "lib/probe.lua""#), "{json}");
    assert!(!json.contains("\"text\""), "{json}");
}

#[test]
fn a_payload_needs_exactly_one_form() {
    let wrap = |payload: &str| {
        format!(
            r#"{{ "groups": [ {{ "name": "G", "commands": [
  {{ "name": "C", "payload": {payload} }}
] }} ] }}"#
        )
    };
    for (payload, needle) in [
        (r#"{}"#, "no text, hex, codec or script"),
        (r#"{ "script": "a.lua", "text": "a" }"#, "exactly one"),
        (r#"{ "script": "a.lua", "fields": {} }"#, "exactly one"),
        (r#"{ "script": "" }"#, "needs the script's path"),
        (r#"{ "text": "a", "hex": "00" }"#, "exactly one"),
        (r#"{ "text": "a", "codec": "x" }"#, "exactly one"),
        (r#"{ "text": "a", "fields": {} }"#, "exactly one"),
        (r#"{ "fields": {} }"#, "`fields` belongs"),
        (r#""AT""#, "expected"),
    ] {
        match parse_err(&wrap(payload)) {
            CommandsError::Invalid { message, line, .. } => {
                assert!(message.contains(needle), "{payload}: {message}");
                // The error points into the file, at the payload or after it.
                assert!(line >= 2, "{payload}: line {line}");
            }
            other => panic!("{payload}: {other:?}"),
        }
    }
    // A command with no payload at all.
    let missing = r#"{ "groups": [ { "name": "G", "commands": [ { "name": "C" } ] } ] }"#;
    match parse_err(missing) {
        CommandsError::Invalid { message, .. } => assert!(message.contains("payload"), "{message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_wrong_shape_is_an_error_not_a_panic() {
    for text in [
        "[]",
        "5",
        r#"{ "groups": {} }"#,
        r#"{ "groups": [ { "commands": [] } ] }"#,
        r#"{ "groups": [ { "name": "G", "commands": [ { "name": "C", "payload": { "text": 5 } } ] } ] }"#,
        r#"{ "groups": [ { "name": "G", "commands": [ { "name": "C", "payload": { "text": "a" }, "eol": "crcr" } ] } ] }"#,
        r#"{ "groups": [ { "name": "G", "commands": [ { "name": "C", "payload": { "text": "a" }, "params": [ { "name": "p", "kind": "float" } ] } ] } ] }"#,
    ] {
        assert!(
            matches!(parse_err(text), CommandsError::Invalid { .. }),
            "{text}"
        );
    }
}

#[test]
fn unknown_keys_and_problems_become_warnings() {
    let loaded = parse(
        r#"{
  "nmae": "typo",
  "groups": [
    { "name": "G", "colour": "red", "commands": [
      { "name": "A", "payload": { "text": "x", "extra": 1 }, "expect": { "pattern": "(", "timeout": 5 },
        "params": [ { "name": "p", "labl": "x" } ] },
      { "name": "B", "payload": { "text": "{{ghost}}" } },
      { "name": "A", "payload": { "text": "dup" } }
    ] },
    { "name": "G", "commands": [] }
  ]
}"#,
    );
    let all = loaded.warnings.join("\n");
    for needle in [
        "unknown key `nmae`",
        "unknown key `groups[0].colour`",
        "unknown key `groups[0].commands[0].payload.extra`",
        "unknown key `groups[0].commands[0].expect.timeout`",
        "unknown key `groups[0].commands[0].params[0].labl`",
        "expect pattern",
        "ghost",
        "another command is also called `A`",
        "two groups called `G`",
    ] {
        assert!(all.contains(needle), "no {needle:?} in:\n{all}");
    }
    // The file still loaded.
    assert_eq!(loaded.collection.groups[0].commands.len(), 3);
}

#[test]
fn writing_produces_plain_pretty_json() {
    let mut collection = CommandCollection::new("Demo", CollectionSource::Bundled);
    collection.groups.push(CommandGroup {
        name: "Basics".into(),
        commands: vec![
            Command::text("Version", "AT+VER?")
                .with_eol(LineEnding::Crlf)
                .with_expect("^OK", 500),
        ],
    });
    let want = r#"{
  "name": "Demo",
  "groups": [
    {
      "name": "Basics",
      "commands": [
        {
          "name": "Version",
          "payload": {
            "text": "AT+VER?"
          },
          "eol": "crlf",
          "expect": {
            "pattern": "^OK",
            "timeout_ms": 500
          }
        }
      ]
    }
  ]
}
"#;
    assert_eq!(collection.to_json(), want);
}

#[test]
fn a_collection_survives_a_write_and_a_read() {
    let mut original = parse(FULL).collection;
    // Comments are gone after a round trip, but every field is not.
    original.source = CollectionSource::User("x.json".into());
    let text = original.to_json();
    assert!(!text.contains("//"), "{text}");
    let again = CommandCollection::parse(&text, original.source.clone(), "x.json".as_ref())
        .expect("reads back")
        .collection;
    assert_eq!(again, original);
}

// ---- The store ----

fn config(root: &TempDir) -> ConfigPaths {
    ConfigPaths::new(root.path().join("config"))
}

fn collection_json(name: &str, command: &str) -> String {
    format!(
        r#"{{ "name": "{name}", "groups": [ {{ "name": "G", "commands": [ {{ "name": "{command}", "payload": {{ "text": "{command}" }} }} ] }} ] }}"#
    )
}

#[test]
fn the_bundled_examples_load_clean_and_encode() {
    let examples = CommandCollection::bundled_examples();
    assert_eq!(examples.source, CollectionSource::Bundled);
    assert!(examples.is_read_only());
    assert_eq!(examples.path(), None);
    let loaded = CommandCollection::parse(
        include_str!("../../assets/commands/examples.json"),
        CollectionSource::Bundled,
        "examples.json".as_ref(),
    )
    .unwrap();
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);

    let session = LineEnding::Crlf;
    let send = |name: &str, values: &ParamValues| {
        let (_, command) = examples.find(name).unwrap_or_else(|| panic!("no {name}"));
        command.encode(values, session).expect("encodes")
    };
    assert_eq!(send("AT", &ParamValues::new()), b"AT\r\n");
    assert_eq!(send("ATI", &ParamValues::new()), b"ATI\r\n");
    assert_eq!(send("AT+VER?", &ParamValues::new()), b"AT+VER?\r\n");
    // A default fills the parameter.
    assert_eq!(send("Echo", &ParamValues::new()), b"ATE0\r\n");
    assert_eq!(
        send("Echo", &ParamValues::new().with("on", "1")),
        b"ATE1\r\n"
    );
    // The plan's RACE frame, from its command id.
    assert_eq!(
        send("RACE query (hex)", &ParamValues::new()),
        [0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F]
    );
    // Every AT command waits for a reply.
    for name in ["AT", "ATI", "AT+VER?", "Echo"] {
        assert!(examples.find(name).unwrap().1.expect.is_some(), "{name}");
    }
    // The RACE group's commands are codec payloads that wait for a response frame.
    let race = examples.group("RACE").expect("a RACE group");
    assert_eq!(race.commands.len(), 2);
    for command in &race.commands {
        assert!(
            matches!(&command.payload, Payload::Codec { codec, .. } if codec == "airoha-race"),
            "{}",
            command.name
        );
        let frame = command.expect.as_ref().and_then(|e| e.frame.as_ref());
        assert_eq!(frame.unwrap()["kind"], json!("response"), "{}", command.name);
        assert!(command.problems().is_empty(), "{:?}", command.problems());
    }
    let (_, version) = examples.find("RACE version").unwrap();
    assert_eq!(
        version.encode(&ParamValues::new(), session),
        Err(PayloadError::CodecUnavailable),
        "the app encodes codec payloads"
    );
    assert_eq!(
        examples.find("RACE command").unwrap().1.placeholders(),
        ["id"]
    );
    for (_, command) in examples.commands() {
        assert!(command.keybinding.is_none(), "examples must not grab keys");
    }
}

#[test]
fn load_reads_user_files_in_order_then_project_then_the_examples() {
    let root = TempDir::new("cmd-load");
    let paths = config(&root);
    root.write(
        "config/commands/b-second.json",
        &collection_json("Second", "two"),
    );
    root.write(
        "config/commands/A-first.json",
        &collection_json("First", "one"),
    );
    root.write(
        "config/commands/.hidden.json",
        &collection_json("Hidden", "h"),
    );
    root.write("config/commands/notes.txt", "not a collection");
    root.write(
        "config/commands/nested/deep.json",
        &collection_json("Deep", "d"),
    );
    root.write(
        "repo/.serialist/commands.json",
        &collection_json("Project", "p"),
    );
    let paths = paths.with_project_from(&root.path().join("repo/src"));
    // `with_project_from` searches upward, so the file two levels up is found.
    assert!(paths.project_commands.is_some());

    let store = CommandStore::load(&paths);
    let names: Vec<_> = store
        .collections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(names, ["First", "Second", "Project", "AT basics"]);
    assert!(store.warnings().is_empty(), "{:?}", store.warnings());
    assert!(matches!(
        store.collections()[0].source,
        CollectionSource::User(_)
    ));
    assert!(matches!(
        store.collections()[2].source,
        CollectionSource::Project(_)
    ));
    assert_eq!(store.collections()[3].source, CollectionSource::Bundled);
    assert_eq!(store.commands_dir(), Some(paths.commands_dir().as_path()));
}

#[test]
fn a_missing_directory_gives_just_the_examples() {
    let root = TempDir::new("cmd-none");
    let store = CommandStore::load(&config(&root));
    assert_eq!(store.collections().len(), 1);
    assert_eq!(store.collections()[0].source, CollectionSource::Bundled);
    assert!(store.warnings().is_empty());
}

#[test]
fn a_bad_file_is_a_warning_and_the_rest_still_load() {
    let root = TempDir::new("cmd-bad");
    let paths = config(&root);
    root.write("config/commands/bad.json", "{\n  \"groups\": [ }\n");
    root.write("config/commands/good.json", &collection_json("Good", "ok"));
    root.write(
        "config/commands/odd.json",
        r#"{ "name": "Odd", "groups": [ { "name": "G", "commands": [ { "name": "x", "payload": { "text": "\\q" } } ] } ] }"#,
    );
    let store = CommandStore::load(&paths);
    let names: Vec<_> = store
        .collections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    // `bad.json` is left out; `odd.json` loads with a warning about its command.
    assert_eq!(names, ["Good", "Odd", "AT basics"]);
    let bad = store
        .warnings()
        .iter()
        .find(|w| w.file.ends_with("bad.json"))
        .expect("a warning for bad.json");
    assert!(bad.message.starts_with("2:"), "{}", bad.message);
    assert!(bad.message.contains("not loaded"), "{}", bad.message);
    let odd = store
        .warnings()
        .iter()
        .find(|w| w.file.ends_with("odd.json"))
        .expect("a warning for odd.json");
    assert!(odd.message.contains("escape"), "{}", odd.message);
    assert!(odd.to_string().contains("odd.json"));
    assert!(store.find("Good", "ok").is_some());
}

#[test]
fn find_get_and_keybindings() {
    let root = TempDir::new("cmd-find");
    let paths = config(&root);
    root.write(
        "config/commands/one.json",
        r#"{ "name": "One", "groups": [
            { "name": "A", "commands": [
                { "name": "x", "payload": { "text": "x" }, "keybinding": "cmd-1" },
                { "name": "y", "payload": { "text": "y" } } ] },
            { "name": "B", "commands": [
                { "name": "z", "payload": { "text": "z" }, "keybinding": "ctrl-shift-f5" } ] } ] }"#,
    );
    root.write(
        "config/commands/two.json",
        r#"{ "name": "Two", "groups": [ { "name": "C", "commands": [
            { "name": "x", "payload": { "text": "x2" }, "keybinding": "cmd-1" } ] } ] }"#,
    );
    let store = CommandStore::load(&paths);

    let (reference, command) = store.find("One", "z").expect("z is in group B");
    assert_eq!(reference, CommandRef::new("One", "B", "z"));
    assert_eq!(command.payload, Payload::Text("z".into()));
    assert_eq!(store.get(&reference), Some(command));
    assert!(store.find("One", "nope").is_none());
    assert!(store.find("Nope", "x").is_none());
    assert_eq!(
        store.get(&CommandRef::new("One", "A", "z")),
        None,
        "z is not in group A"
    );
    assert_eq!(reference.to_string(), "One \u{203a} B \u{203a} z");

    assert_eq!(
        store.all_keybindings(),
        [
            ("cmd-1".to_owned(), CommandRef::new("One", "A", "x")),
            ("ctrl-shift-f5".to_owned(), CommandRef::new("One", "B", "z")),
            ("cmd-1".to_owned(), CommandRef::new("Two", "C", "x")),
        ]
    );
    // The clash is reported on the later file.
    let clash = store
        .warnings()
        .iter()
        .find(|w| w.message.contains("`cmd-1` is also bound to"))
        .expect("a clash warning");
    assert!(clash.file.ends_with("two.json"), "{}", clash.file.display());
    assert!(
        clash.message.contains("One \u{203a} A \u{203a} x"),
        "{}",
        clash.message
    );
    assert_eq!(
        store.commands().count(),
        4 + CommandCollection::bundled_examples().commands().count()
    );
}

#[test]
fn two_collections_with_one_name_are_reported() {
    let root = TempDir::new("cmd-dupe");
    let paths = config(&root);
    root.write("config/commands/a.json", &collection_json("Same", "a"));
    root.write("config/commands/b.json", &collection_json("Same", "b"));
    let store = CommandStore::load(&paths);
    assert!(
        store
            .warnings()
            .iter()
            .any(|w| w.file.ends_with("b.json") && w.message.contains("also called `Same`"))
    );
    // Lookups reach the first.
    assert!(store.find("Same", "a").is_some());
    assert!(store.find("Same", "b").is_none());
}

#[test]
fn save_writes_atomically_and_reloads_equal() {
    let root = TempDir::new("cmd-save");
    let paths = config(&root);
    let mut store = CommandStore::load(&paths);
    assert!(!paths.commands_dir().exists());
    store.create_collection("Bring up").unwrap();
    store
        .add_command(
            "Bring up",
            "Basics",
            Command::text("Version", "AT+VER?\\r")
                .with_expect("^OK", 250)
                .with_keybinding("cmd-2"),
        )
        .unwrap();
    store.save_collection("Bring up").unwrap();

    // The directory was made, holds the file and nothing else (no temp file).
    let file = paths.commands_dir().join("bring-up.json");
    let mut listing: Vec<_> = fs::read_dir(paths.commands_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    listing.sort();
    assert_eq!(listing, ["bring-up.json"]);
    let text = fs::read_to_string(&file).unwrap();
    assert!(text.ends_with("}\n"), "{text}");

    let reloaded = CommandStore::load(&paths);
    assert!(reloaded.warnings().is_empty(), "{:?}", reloaded.warnings());
    assert_eq!(
        reloaded.collection("Bring up"),
        store.collection("Bring up"),
        "what was saved is what loads"
    );

    // Saving again replaces the file (a rename over an existing one).
    store
        .add_command("Bring up", "Basics", Command::text("Other", "X"))
        .unwrap();
    store.save_collection("Bring up").unwrap();
    assert_eq!(
        CommandStore::load(&paths)
            .collection("Bring up")
            .unwrap()
            .commands()
            .count(),
        2
    );
    assert_eq!(fs::read_dir(paths.commands_dir()).unwrap().count(), 1);
}

#[test]
fn saving_drops_comments_and_unknown_keys() {
    let root = TempDir::new("cmd-drop");
    let paths = config(&root);
    let file = root.write(
        "config/commands/mine.json",
        "// my notes\n{ \"name\": \"Mine\", \"extra\": 1, \"groups\": [ { \"name\": \"G\", \"commands\": [ { \"name\": \"c\", \"payload\": { \"text\": \"x\" } } ] } ] }",
    );
    let store = CommandStore::load(&paths);
    store.save_collection("Mine").unwrap();
    let text = fs::read_to_string(file).unwrap();
    assert!(
        !text.contains("my notes") && !text.contains("extra"),
        "{text}"
    );
}

#[test]
fn the_bundled_examples_cannot_be_saved() {
    let store = CommandStore::load(&config(&TempDir::new("cmd-ro")));
    let err = store
        .save(&CommandCollection::bundled_examples())
        .expect_err("read-only");
    assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(
        store
            .save_collection("No such collection")
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
}

fn small_store() -> CommandStore {
    let mut store = CommandStore::empty();
    store.set_collection(CommandCollection {
        name: "Mine".into(),
        source: CollectionSource::User("mine.json".into()),
        groups: vec![
            CommandGroup {
                name: "A".into(),
                commands: vec![Command::text("one", "1"), Command::text("two", "2")],
            },
            CommandGroup {
                name: "B".into(),
                commands: vec![Command::text("three", "3")],
            },
        ],
    });
    store.set_collection(CommandCollection::bundled_examples());
    store
}

#[test]
fn add_update_remove_and_rename_commands() {
    let mut store = small_store();

    // A new group is made on demand.
    let added = store
        .add_command("Mine", "C", Command::text("four", "4"))
        .unwrap();
    assert_eq!(added, CommandRef::new("Mine", "C", "four"));
    assert_eq!(store.collection("Mine").unwrap().groups.len(), 3);
    // Names are unique across the collection, so `find` is unambiguous.
    assert_eq!(
        store.add_command("Mine", "A", Command::text("three", "x")),
        Err(EditError::DuplicateCommand {
            collection: "Mine".into(),
            name: "three".into()
        })
    );
    assert_eq!(
        store.add_command("Mine", "A", Command::text(" ", "x")),
        Err(EditError::EmptyName)
    );
    assert_eq!(
        store.add_command("Nope", "A", Command::text("x", "x")),
        Err(EditError::NoCollection("Nope".into()))
    );

    // Update in place, including a rename.
    let two = CommandRef::new("Mine", "A", "two");
    let updated = store
        .update_command(&two, Command::text("two", "22").with_eol(LineEnding::Cr))
        .unwrap();
    assert_eq!(updated, two);
    assert_eq!(store.get(&two).unwrap().eol, Some(LineEnding::Cr));
    let renamed = store.rename_command(&two, "deux").unwrap();
    assert_eq!(renamed, CommandRef::new("Mine", "A", "deux"));
    assert!(store.get(&two).is_none());
    assert_eq!(
        store.rename_command(&renamed, "one"),
        Err(EditError::DuplicateCommand {
            collection: "Mine".into(),
            name: "one".into()
        })
    );
    assert_eq!(
        store.update_command(&two, Command::text("x", "x")),
        Err(EditError::NoCommand {
            collection: "Mine".into(),
            name: "two".into()
        })
    );
    // Order is kept: `deux` is still second in group A.
    let group = store.collection("Mine").unwrap().group("A").unwrap();
    let order: Vec<_> = group.commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(order, ["one", "deux"]);

    let removed = store.remove_command(&renamed).unwrap();
    assert_eq!(removed.payload, Payload::Text("22".into()));
    assert!(store.get(&renamed).is_none());
    assert!(matches!(
        store.remove_command(&renamed),
        Err(EditError::NoCommand { .. })
    ));
    assert!(matches!(
        store.remove_command(&CommandRef::new("Mine", "Z", "one")),
        Err(EditError::NoGroup { .. })
    ));
}

#[test]
fn rename_and_remove_groups_and_collections() {
    let mut store = small_store();
    store.rename_group("Mine", "A", "Alpha").unwrap();
    assert!(store.find("Mine", "one").is_some());
    assert_eq!(store.find("Mine", "one").unwrap().0.group, "Alpha");
    assert_eq!(
        store.rename_group("Mine", "Alpha", "B"),
        Err(EditError::DuplicateGroup {
            collection: "Mine".into(),
            group: "B".into()
        })
    );
    assert!(matches!(
        store.rename_group("Mine", "Zed", "Q"),
        Err(EditError::NoGroup { .. })
    ));
    assert_eq!(
        store.rename_group("Mine", "B", " "),
        Err(EditError::EmptyName)
    );
    store.remove_group("Mine", "B").unwrap();
    assert!(store.find("Mine", "three").is_none());
    assert!(matches!(
        store.remove_group("Mine", "B"),
        Err(EditError::NoGroup { .. })
    ));

    assert_eq!(
        store.rename_collection("Mine", "AT basics"),
        Err(EditError::DuplicateCollection("AT basics".into()))
    );
    store.rename_collection("Mine", "Renamed").unwrap();
    assert!(store.collection("Mine").is_none());
    assert_eq!(store.collection("Renamed").unwrap().groups.len(), 1);
}

#[test]
fn the_bundled_collection_is_read_only_to_every_edit() {
    let mut store = small_store();
    let read_only = EditError::ReadOnly("AT basics".into());
    assert_eq!(
        store.add_command("AT basics", "Basics", Command::text("n", "n")),
        Err(read_only.clone())
    );
    let at = CommandRef::new("AT basics", "Basics", "AT");
    assert_eq!(store.remove_command(&at), Err(read_only.clone()));
    assert_eq!(store.rename_command(&at, "x"), Err(read_only.clone()));
    assert_eq!(
        store.update_command(&at, Command::text("AT", "x")),
        Err(read_only.clone())
    );
    assert_eq!(
        store.rename_group("AT basics", "Basics", "x"),
        Err(read_only.clone())
    );
    assert_eq!(
        store.remove_group("AT basics", "Basics"),
        Err(read_only.clone())
    );
    assert_eq!(store.rename_collection("AT basics", "x"), Err(read_only));
    // Nothing changed.
    assert_eq!(
        store.collection("AT basics"),
        Some(&CommandCollection::bundled_examples())
    );
}

#[test]
fn create_collection_picks_a_file_and_keeps_the_panel_order() {
    let root = TempDir::new("cmd-create");
    let paths = config(&root);
    root.write(
        "config/commands/airoha-bring-up.json",
        &collection_json("Taken", "t"),
    );
    root.write(
        "repo/.serialist/commands.json",
        &collection_json("Project", "p"),
    );
    let paths = paths.with_project_from(&root.path().join("repo"));
    let mut store = CommandStore::load(&paths);

    let made = store.create_collection("Airoha bring-up!").unwrap();
    // The slug is taken by an existing file, so the file is numbered.
    assert_eq!(
        made.path(),
        Some(
            paths
                .commands_dir()
                .join("airoha-bring-up-2.json")
                .as_path()
        )
    );
    assert!(matches!(made.source, CollectionSource::User(_)));
    assert!(
        !paths.commands_dir().join("airoha-bring-up-2.json").exists(),
        "not written yet"
    );
    let again = store.create_collection("Airoha bring-up!!");
    assert_eq!(
        again
            .map(|c| c.path().map(ToOwned::to_owned))
            .unwrap()
            .unwrap(),
        paths.commands_dir().join("airoha-bring-up-3.json")
    );

    let names: Vec<_> = store
        .collections()
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    // User files, then the project's, then the examples.
    assert_eq!(
        names,
        [
            "Taken",
            "Airoha bring-up!",
            "Airoha bring-up!!",
            "Project",
            "AT basics"
        ]
    );
    assert!(matches!(
        store.create_collection("Taken"),
        Err(EditError::DuplicateCollection(_))
    ));
    assert!(matches!(
        store.create_collection("  "),
        Err(EditError::EmptyName)
    ));
    assert!(matches!(
        CommandStore::empty().create_collection("x"),
        Err(EditError::NoDirectory)
    ));
    // A name with nothing to make a file name from still gets one.
    assert!(
        store
            .create_collection("???")
            .unwrap()
            .path()
            .unwrap()
            .ends_with("commands.json")
    );
}

#[test]
fn set_collection_replaces_by_file() {
    let mut store = small_store();
    let mut edited = store.collection("Mine").unwrap().clone();
    edited.groups.clear();
    store.set_collection(edited);
    assert_eq!(store.collections().len(), 2, "replaced, not added");
    assert!(store.collection("Mine").unwrap().groups.is_empty());
    // The examples are never replaced.
    let mut fake = CommandCollection::bundled_examples();
    fake.groups.clear();
    store.set_collection(fake);
    assert_eq!(store.collections().len(), 3);
    assert_eq!(store.collections()[2].source, CollectionSource::Bundled);
}

// ---- Filtering ----

fn menu() -> CommandStore {
    let mut store = CommandStore::empty();
    let groups = [
        ("Basics", "Version", "Ask for the firmware version"),
        ("Basics", "Reset", ""),
        ("Basics", "Save all events, reset", ""),
        ("Radio", "Power", "Transmit power in dBm"),
        ("Radio", "Verbose logging", ""),
    ];
    let mut collection =
        CommandCollection::new("Airoha bring-up", CollectionSource::User("a.json".into()));
    for (group, name, description) in groups {
        let command = Command::text(name, name).with_description(description);
        match collection.groups.iter_mut().find(|g| g.name == group) {
            Some(g) => g.commands.push(command),
            None => collection.groups.push(CommandGroup {
                name: group.into(),
                commands: vec![command],
            }),
        }
    }
    store.set_collection(collection);
    store
}

fn names(hits: &[(CommandRef, i32)]) -> Vec<&str> {
    hits.iter().map(|(r, _)| r.name.as_str()).collect()
}

#[test]
fn an_empty_query_lists_everything_in_order() {
    let store = menu();
    for query in ["", "   "] {
        let hits = store.filter(query);
        assert_eq!(
            names(&hits),
            [
                "Version",
                "Reset",
                "Save all events, reset",
                "Power",
                "Verbose logging"
            ]
        );
        assert!(hits.iter().all(|(_, score)| *score == 0));
    }
}

#[test]
fn filter_ranks_whole_words_and_prefixes_first() {
    let store = menu();
    let hits = store.filter("ver");
    // `Version` and `Verbose logging` start with it; `Save all events, reset` only
    // holds the letters in order.
    assert_eq!(
        names(&hits),
        ["Version", "Verbose logging", "Save all events, reset"]
    );
    assert!(hits[0].1 > hits[1].1 && hits[1].1 > hits[2].1, "{hits:?}");

    // Case is ignored.
    assert_eq!(names(&store.filter("VERSION")), ["Version"]);
    // Letters in order, anywhere; the closer fit ranks first.
    assert_eq!(names(&store.filter("vsn")), ["Version", "Verbose logging"]);
    assert_eq!(names(&store.filter("versio")), ["Version"]);
    // The exact name beats the prefix match.
    assert_eq!(names(&store.filter("reset"))[0], "Reset");
    assert_eq!(names(&store.filter("nothing like it")), Vec::<&str>::new());
    assert!(store.filter("zzz").is_empty());
}

#[test]
fn every_word_must_match_and_the_group_and_description_count() {
    let store = menu();
    // The group and the collection name are searched.
    assert_eq!(names(&store.filter("radio")), ["Power", "Verbose logging"]);
    assert_eq!(
        names(&store.filter("airoha ver")),
        ["Version", "Verbose logging", "Save all events, reset"]
    );
    // Words are ANDed.
    assert_eq!(names(&store.filter("radio ver")), ["Verbose logging"]);
    assert!(store.filter("radio zzz").is_empty());
    // A description-only match still lists the command, below name matches.
    let hits = store.filter("dbm");
    assert_eq!(names(&hits), ["Power"]);
    let by_name = store.filter("power");
    assert!(by_name[0].1 > hits[0].1, "{by_name:?} vs {hits:?}");
}

#[test]
fn equal_scores_keep_the_store_order() {
    let mut store = CommandStore::empty();
    let mut collection = CommandCollection::new("C", CollectionSource::User("c.json".into()));
    collection.groups.push(CommandGroup {
        name: "G".into(),
        commands: ["b1", "a1", "c1"].map(|n| Command::text(n, "x")).to_vec(),
    });
    store.set_collection(collection);
    assert_eq!(names(&store.filter("1")), ["b1", "a1", "c1"]);
}

#[test]
fn fuzzy_match_reports_positions_for_highlighting() {
    let hit = fuzzy_match("ver", "Version").unwrap();
    assert_eq!(hit.positions, [0, 1, 2]);
    assert_eq!(fuzzy_match("vsn", "Version").unwrap().positions, [0, 3, 6]);
    assert_eq!(
        fuzzy_match("", "anything"),
        Some(FuzzyMatch {
            score: 0,
            positions: vec![]
        })
    );
    assert_eq!(fuzzy_match("longer", "long"), None);
    assert_eq!(fuzzy_match("xyz", "Version"), None);
    // Positions count characters, not bytes.
    assert_eq!(fuzzy_match("b", "\u{e9}\u{e9}b").unwrap().positions, [2]);
    // Order matters.
    assert_eq!(fuzzy_match("nv", "Version"), None);
}

#[test]
fn fuzzy_match_prefers_word_starts_and_tight_spans() {
    let at_word = fuzzy_match("vb", "Very big").unwrap();
    let mid_word = fuzzy_match("vb", "Verbatim").unwrap();
    assert!(at_word.score > mid_word.score, "{at_word:?} {mid_word:?}");
    // Of two places the letters could go, the tight one is chosen.
    let tight = fuzzy_match("axb", "a x zz a x b").unwrap();
    assert_eq!(tight.positions, [7, 9, 11]);
    // A contiguous run beats a spread one.
    let whole = fuzzy_match("abc", "xx abc").unwrap();
    let spread = fuzzy_match("abc", "a x b x c").unwrap();
    assert!(whole.score > spread.score);
    // The whole text, then a prefix, then the rest.
    let exact = fuzzy_match("at", "AT").unwrap().score;
    let prefix = fuzzy_match("at", "ATI").unwrap().score;
    let inside = fuzzy_match("at", "the AT").unwrap().score;
    assert!(
        exact > prefix && prefix > inside,
        "{exact} {prefix} {inside}"
    );
}
