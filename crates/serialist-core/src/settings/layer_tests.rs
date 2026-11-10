//! A value this build cannot read rejects only its own key: the rest of the file
//! applies, and the layer below keeps that key.

use crate::config::{FlowControl, Parity};

use super::*;

fn load(layers: &[(&str, &str)]) -> Settings {
    let layers: Vec<SettingsLayer> = layers
        .iter()
        .map(|(origin, text)| SettingsLayer::new(origin, *text))
        .collect();
    load_settings_from_layers(&layers).unwrap_or_else(|err| panic!("settings failed: {err}"))
}

fn keys(settings: &Settings) -> Vec<&str> {
    settings.warnings.iter().map(|w| w.key.as_str()).collect()
}

/// `settings` without its warnings, to compare with another load.
fn quiet(mut settings: Settings) -> Settings {
    settings.warnings.clear();
    settings
}

/// A user file that sets many keys, with `{line_ending}` and `{emulation}` to fill in.
const USER_FILE: &str = r#"{
  "buffer_font_family": "Berkeley Mono",
  "buffer_font_size": 13,
  "theme": "Serialist Ember",
  "display": { "timestamps": "delta", "wrap": true, "view": "hex" },
  "terminal": { "font_size": 12, "emulation": EMULATION, "cursor_blink": true },
  "default_baud": 921600,
  "line_ending": LINE_ENDING,
  "local_echo": true,
  "replay": { "speed": "4x", "end": "hold" },
  "inline": { "backspace": "bs" },
  "devices": [ { "match": { "path": "/dev/cu.usbserial" }, "baud": 9600 } ]
}"#;

fn user_file(line_ending: &str, emulation: &str) -> String {
    USER_FILE
        .replace("LINE_ENDING", line_ending)
        .replace("EMULATION", emulation)
}

#[test]
fn a_line_ending_from_a_later_release_keeps_every_other_user_setting() {
    let future = load(&[("user", &user_file("\"crcrlf\"", "\"vt\""))]);
    let [warning] = future.warnings.as_slice() else {
        panic!("{:?}", future.warnings);
    };
    assert_eq!(warning.key, "line_ending");
    assert_eq!((warning.line, warning.column), (8, 18));
    assert!(warning.to_string().contains("crcrlf"), "{warning}");
    assert_eq!(future.line_ending, LineEnding::Crlf, "the bundled default");

    // Everything else is as if the key were not there.
    let without = USER_FILE
        .replace("  \"line_ending\": LINE_ENDING,\n", "")
        .replace("EMULATION", "\"vt\"");
    let without = load(&[("user", &without)]);
    assert!(without.warnings.is_empty(), "{:?}", without.warnings);
    assert_eq!(quiet(future.clone()), without);
    assert_eq!(future.buffer_font_family.as_deref(), Some("Berkeley Mono"));
    assert_eq!(future.default_baud, 921_600);
    assert_eq!(future.terminal.emulation, Emulation::Vt);
    assert_eq!(future.devices.len(), 1);
}

#[test]
fn an_emulation_from_a_later_release_keeps_every_other_user_setting() {
    let future = load(&[("user", &user_file("\"lf\"", "\"graphics\""))]);
    assert_eq!(keys(&future), ["terminal.emulation"]);
    assert_eq!(
        (future.warnings[0].line, future.warnings[0].column),
        (6, 47)
    );
    assert_eq!(future.terminal.emulation, Emulation::Monitor);
    // The other keys of the same object apply.
    assert_eq!(future.terminal.font_size, Some(12.0));
    assert!(future.terminal.cursor_blink);
    assert_eq!(future.line_ending, LineEnding::Lf);

    let without = USER_FILE
        .replace(" \"emulation\": EMULATION,", "")
        .replace("LINE_ENDING", "\"lf\"");
    assert_eq!(quiet(future), load(&[("user", &without)]));

    // In a device profile too: the profile keeps its other keys.
    let settings = load(&[(
        "user",
        r#"{ "devices": [ { "match": { "path": "/dev/a" }, "emulation": "graphics", "baud": 9600 } ] }"#,
    )]);
    assert_eq!(keys(&settings), ["devices[0].emulation"]);
    assert_eq!(settings.devices[0].emulation, None);
    assert_eq!(settings.devices[0].baud, Some(9600));
}

#[test]
fn a_bad_value_in_the_project_layer_falls_back_to_the_users_value() {
    let settings = load(&[
        (
            "user",
            r#"{ "line_ending": "lf", "display": { "view": "hex", "wrap": true } }"#,
        ),
        (
            "project",
            r#"{ "line_ending": "crcrlf", "display": { "view": "binary", "wrap": false } }"#,
        ),
    ]);
    assert_eq!(settings.line_ending, LineEnding::Lf, "the user's, not CRLF");
    assert_eq!(
        settings.display.view,
        DisplayView::Hex,
        "the user's, not text"
    );
    assert!(!settings.display.wrap, "the project's good key applies");
    let warnings: Vec<(&str, &str)> = settings
        .warnings
        .iter()
        .map(|w| (w.file.to_str().unwrap(), w.key.as_str()))
        .collect();
    assert_eq!(
        warnings,
        [("project", "line_ending"), ("project", "display.view")]
    );

    // A bad value in both layers leaves the bundled default.
    let settings = load(&[
        ("user", r#"{ "default_baud": 0 }"#),
        ("project", r#"{ "default_baud": -5 }"#),
    ]);
    assert_eq!(settings.default_baud, Settings::default().default_baud);
    assert_eq!(settings.warnings.len(), 2);
}

#[test]
fn a_value_of_the_wrong_shape_drops_the_whole_key() {
    let settings = load(&[
        ("user", r#"{ "display": { "wrap": true, "view": "hex" } }"#),
        ("project", "{ \"display\": 5, \"local_echo\": true }"),
    ]);
    assert_eq!(keys(&settings), ["display"]);
    assert!(settings.display.wrap, "the user's display object stays");
    assert_eq!(settings.display.view, DisplayView::Hex);
    assert!(settings.local_echo);
    assert_eq!(
        (settings.warnings[0].line, settings.warnings[0].column),
        (1, 14)
    );

    // A theme object with one bad leaf keeps the others.
    let settings = load(&[(
        "user",
        r#"{ "theme": { "mode": "dusk", "light": "Serialist Paper" } }"#,
    )]);
    assert_eq!(keys(&settings), ["theme.mode"]);
    assert_eq!(
        settings.theme,
        ThemeSelection::Dynamic {
            mode: ThemeMode::System,
            light: "Serialist Paper".into(),
            dark: "Serialist Dark".into(),
        }
    );

    // One bad font feature leaves the others.
    let settings = load(&[(
        "user",
        r#"{ "buffer_font_features": { "calt": false, "toolong": 1, "ss01": "on" } }"#,
    )]);
    assert_eq!(
        keys(&settings),
        ["buffer_font_features.toolong", "buffer_font_features.ss01"]
    );
    assert_eq!(settings.buffer_font_features.get("calt"), Some(0));
    assert_eq!(settings.buffer_font_features.len(), 1);
}

#[test]
fn unknown_keys_and_bad_values_are_told_apart() {
    let settings = load(&[(
        "user",
        "{\n  \"line_endings\": \"lf\",\n  \"line_ending\": \"lfcr\"\n}",
    )]);
    let [unknown, bad] = settings.warnings.as_slice() else {
        panic!("{:?}", settings.warnings);
    };
    assert_eq!(unknown.kind, SettingsWarningKind::UnknownKey);
    assert!(!unknown.is_invalid_value());
    assert_eq!((unknown.line, unknown.column), (2, 3), "the key's name");
    assert_eq!(
        unknown.to_string(),
        "user:2:3: unknown setting `line_endings` is ignored"
    );
    assert!(bad.is_invalid_value());
    assert_eq!((bad.line, bad.column), (3, 18), "the value");
    let SettingsWarningKind::InvalidValue { reason } = &bad.kind else {
        panic!("{bad:?}");
    };
    assert!(reason.contains("unknown variant `lfcr`"), "{reason}");
    assert_eq!(
        bad.to_string(),
        format!("user:3:18: setting `line_ending` is ignored: {reason}")
    );
}

#[test]
fn syntax_errors_and_a_non_object_root_still_fail_the_whole_file() {
    for bad in ["{ \"line_ending\": ", "[]", "5", "\"text\""] {
        let result = load_settings_from_layers(&[
            SettingsLayer::new("user", r#"{ "line_ending": "lf" }"#),
            SettingsLayer::new("project", bad),
        ]);
        assert!(
            matches!(result, Err(SettingsError::Invalid { ref file, .. }) if file.as_os_str() == "project"),
            "{bad}: {result:?}"
        );
    }
}

#[test]
fn devices_that_are_not_a_list_leave_the_users_list() {
    let user = r#"{ "devices": [ { "match": { "path": "/dev/a" }, "baud": 9600 } ] }"#;
    for bad in [
        r#"{ "devices": { "match": {} } }"#,
        r#"{ "devices": "none" }"#,
        r#"{ "devices": null }"#,
    ] {
        let settings = load(&[("user", user), ("project", bad)]);
        assert_eq!(keys(&settings), ["devices"], "{bad}");
        assert_eq!(settings.devices.len(), 1, "{bad}");
        assert_eq!(settings.devices[0].baud, Some(9600), "{bad}");
    }
}

#[test]
fn a_bad_profile_is_left_out_and_the_others_load() {
    let settings = load(&[(
        "user",
        r#"{ "devices": [
            { "name": "first", "match": { "path": "/dev/a" } },
            5,
            { "name": "no match", "baud": 9600 },
            { "name": "bad match", "match": { "vid": "zzzz" } },
            { "name": "match not an object", "match": "/dev/b" },
            { "name": "last", "match": { "vid": "0x0e8d" }, "parity": "sometimes", "baud": 0,
              "data_bits": 7 }
        ] }"#,
    )]);
    let names: Vec<Option<&str>> = settings
        .devices
        .iter()
        .map(|profile| profile.name.as_deref())
        .collect();
    assert_eq!(names, [Some("first"), Some("last")]);
    let last = &settings.devices[1];
    assert_eq!(last.r#match.vid, Some(UsbId(0x0e8d)));
    assert_eq!(last.parity, None);
    assert_eq!(last.baud, None);
    assert_eq!(last.data_bits, Some(crate::config::DataBits::Seven));
    assert_eq!(
        keys(&settings),
        [
            "devices[1]",
            "devices[2]",
            "devices[3]",
            "devices[4]",
            "devices[5].parity",
            "devices[5].baud",
        ]
    );
    for warning in &settings.warnings {
        assert!(warning.is_invalid_value(), "{warning}");
    }
    let lines: Vec<usize> = settings.warnings.iter().map(|w| w.line).collect();
    assert_eq!(lines, [3, 4, 5, 6, 7, 7]);
    assert!(
        settings.warnings[1].message.contains("no `match`"),
        "{}",
        settings.warnings[1]
    );
    assert!(
        settings.warnings[2].message.contains("zzzz"),
        "{}",
        settings.warnings[2]
    );
}

#[test]
fn a_project_files_devices_replace_the_users_list() {
    let settings = load(&[
        (
            "user",
            r#"{ "devices": [ { "name": "user a", "match": { "path": "/dev/a" } },
                            { "name": "user b", "match": { "path": "/dev/b" } } ] }"#,
        ),
        (
            "project",
            r#"{ "devices": [ { "name": "project", "match": { "path": "/dev/p" } } ] }"#,
        ),
    ]);
    let names: Vec<Option<&str>> = settings
        .devices
        .iter()
        .map(|profile| profile.name.as_deref())
        .collect();
    assert_eq!(names, [Some("project")]);

    // An empty list in the project clears the user's.
    let settings = load(&[
        (
            "user",
            r#"{ "devices": [ { "match": { "path": "/dev/a" } } ] }"#,
        ),
        ("project", r#"{ "devices": [] }"#),
    ]);
    assert!(settings.devices.is_empty());
}

#[test]
fn parity_takes_its_names_and_letters_in_either_case() {
    for (spelling, parity) in [
        ("none", Parity::None),
        ("n", Parity::None),
        ("N", Parity::None),
        ("odd", Parity::Odd),
        ("o", Parity::Odd),
        ("O", Parity::Odd),
        ("even", Parity::Even),
        ("e", Parity::Even),
        ("E", Parity::Even),
        ("mark", Parity::Mark),
        ("m", Parity::Mark),
        ("M", Parity::Mark),
        ("space", Parity::Space),
        ("s", Parity::Space),
        ("S", Parity::Space),
    ] {
        let settings = load(&[(
            "user",
            &format!(r#"{{ "devices": [ {{ "match": {{}}, "parity": "{spelling}" }} ] }}"#),
        )]);
        assert!(
            settings.warnings.is_empty(),
            "{spelling}: {:?}",
            settings.warnings
        );
        assert_eq!(settings.devices[0].parity, Some(parity), "{spelling}");
    }
}

#[test]
fn flow_control_takes_its_names_and_the_rtscts_and_xonxoff_spellings() {
    for (spelling, flow) in [
        ("none", FlowControl::None),
        ("hardware", FlowControl::Hardware),
        ("rtscts", FlowControl::Hardware),
        ("software", FlowControl::Software),
        ("xonxoff", FlowControl::Software),
    ] {
        let settings = load(&[(
            "user",
            &format!(r#"{{ "devices": [ {{ "match": {{}}, "flow_control": "{spelling}" }} ] }}"#),
        )]);
        assert!(
            settings.warnings.is_empty(),
            "{spelling}: {:?}",
            settings.warnings
        );
        assert_eq!(settings.devices[0].flow_control, Some(flow), "{spelling}");
    }
}
