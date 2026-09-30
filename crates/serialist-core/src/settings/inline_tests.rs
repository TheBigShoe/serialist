//! The `inline` settings object: defaults, parsing, validation and layering.

use std::time::Duration;

use super::*;

fn user_settings(text: &str) -> Settings {
    Settings::from_jsonc(text).unwrap_or_else(|err| panic!("settings failed to load: {err}"))
}

fn error_of(text: &str) -> SettingsError {
    Settings::from_jsonc(text).expect_err("settings should be rejected")
}

fn line_of(err: &SettingsError) -> usize {
    match err {
        SettingsError::Invalid { line, .. } => *line,
        other => panic!("expected an Invalid error, got {other:?}"),
    }
}

#[test]
fn defaults_are_the_documented_values() {
    let inline = Settings::default().inline;
    assert_eq!(inline.backspace, BackspaceKey::Del);
    assert_eq!(inline.backspace.byte(), 0x7f);
    assert_eq!(inline.backspace.alternate_byte(), 0x08);
    assert_eq!(inline.escape_chord, "ctrl-]");
    assert_eq!(inline.paste_chunk_bytes, 64);
    assert_eq!(inline.paste_chunk_delay_ms, 10);
    assert_eq!(inline.paste_chunk_delay(), Duration::from_millis(10));
    assert_eq!(inline, InlineSettings::default());
    // A document that leaves `inline` out, or only part of it, takes the defaults.
    assert_eq!(user_settings("{}").inline, inline);
    let partial = user_settings(r#"{ "inline": { "paste_chunk_bytes": 8 } }"#).inline;
    assert_eq!(partial.paste_chunk_bytes, 8);
    assert_eq!(
        InlineSettings {
            paste_chunk_bytes: 64,
            ..partial
        },
        inline
    );
    // Straight from serde, without the loader, the types fill the gaps too.
    let bare: Settings = serde_json::from_str(r#"{ "inline": { "backspace": "bs" } }"#).unwrap();
    assert_eq!(bare.inline.backspace, BackspaceKey::Bs);
    assert_eq!(bare.inline.escape_chord, "ctrl-]");
}

#[test]
fn every_key_is_read() {
    let settings = user_settings(
        r#"{ "inline": {
            "backspace": "bs",
            "escape_chord": " ctrl-alt-x ",
            "paste_chunk_bytes": 16,
            "paste_chunk_delay_ms": 0,
        } }"#,
    );
    let inline = settings.inline;
    assert_eq!(inline.backspace, BackspaceKey::Bs);
    assert_eq!(inline.backspace.byte(), 0x08);
    assert_eq!(inline.backspace.alternate_byte(), 0x7f);
    assert_eq!(inline.backspace.name(), "bs");
    assert_eq!(inline.escape_chord, "ctrl-alt-x", "trimmed");
    assert_eq!(inline.paste_chunk_bytes, 16);
    assert_eq!(inline.paste_chunk_delay_ms, 0);
    assert_eq!(inline.paste_chunk_delay(), Duration::ZERO);
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
}

#[test]
fn backspace_takes_names_and_byte_values() {
    let backspace = |value: &str| {
        Settings::from_jsonc(&format!(r#"{{ "inline": {{ "backspace": {value} }} }}"#))
            .map(|settings| settings.inline.backspace)
    };
    for value in [
        r#""del""#,
        r#""DEL""#,
        r#""delete""#,
        r#""0x7f""#,
        r#""127""#,
        "127",
    ] {
        assert_eq!(backspace(value).unwrap(), BackspaceKey::Del, "{value}");
    }
    for value in [
        r#""bs""#,
        r#""Backspace""#,
        r#""0x08""#,
        r#""0x8""#,
        r#""8""#,
        "8",
    ] {
        assert_eq!(backspace(value).unwrap(), BackspaceKey::Bs, "{value}");
    }
    for value in [
        r#""0x20""#,
        r#""nul""#,
        "32",
        "true",
        "7.5",
        "-1",
        "null",
        "[]",
    ] {
        assert!(backspace(value).is_err(), "{value} should be rejected");
    }
}

#[test]
fn bad_values_are_errors_with_positions() {
    let err = error_of("{\n  \"inline\": {\n    \"backspace\": \"0x20\"\n  }\n}");
    assert!(err.to_string().contains("\"del\""), "{err}");
    assert!(err.to_string().contains("\"bs\""), "{err}");
    assert_eq!(line_of(&err), 3);

    let err = error_of("{\n  \"inline\": { \"escape_chord\": \"ctrl-x ctrl-c\" }\n}");
    assert!(err.to_string().contains("one keystroke"), "{err}");
    assert_eq!(line_of(&err), 2);

    let err = error_of(r#"{ "inline": { "escape_chord": "a" } }"#);
    assert!(err.to_string().contains("needs ctrl, alt or cmd"), "{err}");

    let err = error_of(r#"{ "inline": { "escape_chord": 5 } }"#);
    assert!(err.to_string().contains("expected a keystroke"), "{err}");

    for bad in [
        r#"{ "inline": { "paste_chunk_bytes": 0 } }"#,
        r#"{ "inline": { "paste_chunk_bytes": 2097152 } }"#,
        r#"{ "inline": { "paste_chunk_bytes": -4 } }"#,
        r#"{ "inline": { "paste_chunk_bytes": "big" } }"#,
        r#"{ "inline": { "paste_chunk_bytes": 1.5 } }"#,
        r#"{ "inline": { "paste_chunk_delay_ms": 10001 } }"#,
        r#"{ "inline": { "paste_chunk_delay_ms": -1 } }"#,
        r#"{ "inline": { "paste_chunk_delay_ms": true } }"#,
        r#"{ "inline": [] }"#,
        r#"{ "inline": "ctrl-]" }"#,
    ] {
        assert!(
            Settings::from_jsonc(bad).is_err(),
            "{bad} should be rejected"
        );
    }
    // The limits themselves are fine.
    let edge = user_settings(
        r#"{ "inline": { "paste_chunk_bytes": 1048576, "paste_chunk_delay_ms": 10000 } }"#,
    );
    assert_eq!(edge.inline.paste_chunk_bytes, MAX_PASTE_CHUNK_BYTES);
    assert_eq!(edge.inline.paste_chunk_delay_ms, MAX_PASTE_CHUNK_DELAY_MS);
}

#[test]
fn unknown_keys_are_warnings_and_known_ones_are_not() {
    let settings = user_settings(r#"{ "inline": { "backspace": "bs", "colour": true } }"#);
    let keys: Vec<&str> = settings.warnings.iter().map(|w| w.key.as_str()).collect();
    assert_eq!(keys, ["inline.colour"]);
    assert_eq!(settings.inline.backspace, BackspaceKey::Bs);

    let settings = user_settings(
        r#"{ "inline": { "backspace": "del", "escape_chord": "ctrl-]",
                          "paste_chunk_bytes": 64, "paste_chunk_delay_ms": 10 } }"#,
    );
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
}

#[test]
fn the_object_merges_key_by_key_across_layers() {
    let settings = load_settings_from_layers(&[
        SettingsLayer::new(
            "user",
            r#"{ "inline": { "backspace": "bs", "paste_chunk_bytes": 8 } }"#,
        ),
        SettingsLayer::new("project", r#"{ "inline": { "paste_chunk_bytes": 32 } }"#),
    ])
    .unwrap();
    assert_eq!(
        settings.inline.backspace,
        BackspaceKey::Bs,
        "from the user layer"
    );
    assert_eq!(settings.inline.paste_chunk_bytes, 32, "the project wins");
    assert_eq!(settings.inline.escape_chord, "ctrl-]", "from the defaults");
    assert_eq!(settings.inline.paste_chunk_delay_ms, 10);
}

#[test]
fn settings_round_trip_through_json() {
    let settings = user_settings(
        r#"{ "inline": { "backspace": "bs", "escape_chord": "alt-shift-q",
                          "paste_chunk_bytes": 3, "paste_chunk_delay_ms": 250 } }"#,
    );
    let json = serde_json::to_value(&settings.inline).unwrap();
    assert_eq!(json["backspace"], "bs");
    assert_eq!(json["escape_chord"], "alt-shift-q");
    let back: InlineSettings = serde_json::from_value(json).unwrap();
    assert_eq!(back, settings.inline);

    let whole = serde_json::to_value(&settings).unwrap();
    let mut back: Settings = serde_json::from_value(whole).unwrap();
    back.warnings = settings.warnings.clone();
    assert_eq!(back, settings);
}

#[test]
fn the_template_documents_and_comments_out_the_keys() {
    let template = settings_template();
    for line in [
        "  // \"inline\": {",
        "    // \"backspace\": \"del\",",
        "    // \"escape_chord\": \"ctrl-]\",",
        "    // \"paste_chunk_bytes\": 64,",
        "    // \"paste_chunk_delay_ms\": 10",
    ] {
        assert!(template.contains(line), "template lacks {line:?}");
    }
    // Uncommenting the object and one of its lines overrides that key.
    let edited = template
        .replace("  // \"inline\": {", "  \"inline\": {")
        .replace(
            "    // \"backspace\": \"del\",",
            "    \"backspace\": \"bs\",",
        )
        .replace(
            "    // \"paste_chunk_delay_ms\": 10\n  // },",
            "    // \"paste_chunk_delay_ms\": 10\n  },",
        );
    let settings = user_settings(&edited);
    assert_eq!(settings.inline.backspace, BackspaceKey::Bs);
    assert_eq!(settings.inline.escape_chord, "ctrl-]");
    // The bundled file explains each key.
    assert!(DEFAULT_SETTINGS_JSONC.contains("\"escape_chord\": \"ctrl-]\""));
    assert!(DEFAULT_SETTINGS_JSONC.contains("Ctrl-Backspace sends the other one"));
}

#[test]
fn chords_are_checked_for_keystroke_syntax() {
    for good in [
        "ctrl-]",
        "ctrl-\\",
        "Ctrl-A",
        "ctrl-alt-x",
        "alt-shift-q",
        "cmd-k",
        "super-space",
        "secondary-.",
        "ctrl--",
        "ctrl-enter",
        "f12",
        "escape",
        "ctrl-shift-f1",
        "  ctrl-b  ",
    ] {
        assert!(validate_chord(good).is_ok(), "{good:?} should be accepted");
    }
    assert_eq!(validate_chord("  ctrl-b  ").unwrap(), "ctrl-b");
    for bad in [
        "",
        "   ",
        "-",
        "a",
        "]",
        "shift-a",
        "fn-x",
        "space",
        "enter",
        "shift-tab",
        "ctrl-",
        "ctrl-x ctrl-c",
        "bogus-x",
        "ctrl+x",
        "ctrl-bogus-x",
    ] {
        assert!(validate_chord(bad).is_err(), "{bad:?} should be rejected");
    }
}
