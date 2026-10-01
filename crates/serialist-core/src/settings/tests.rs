use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::{DataBits, FlowControl, Parity, StopBits};
use crate::port::{PortId, PortInfo, PortKind, UsbInfo};
use crate::test_util::TempDir;

use super::*;

fn usb_port(
    path: &str,
    vid: u16,
    pid: u16,
    product: Option<&str>,
    manufacturer: Option<&str>,
    serial: Option<&str>,
) -> PortInfo {
    PortInfo {
        id: PortId::new(path),
        kind: PortKind::Usb(UsbInfo {
            vid,
            pid,
            serial_number: serial.map(str::to_string),
            manufacturer: manufacturer.map(str::to_string),
            product: product.map(str::to_string),
        }),
        display_name: product.unwrap_or(path).to_string(),
    }
}

fn airoha_port() -> PortInfo {
    usb_port(
        "/dev/cu.usbmodem1101",
        0x0e8d,
        0x0616,
        Some("Airoha Headset Debug"),
        Some("Airoha Technology"),
        Some("AB12CD"),
    )
}

/// The plan's sample settings block, with a trailing comma added to each list to
/// exercise them.
const PLAN_SAMPLE: &str = r#"{
  "buffer_font_family": "Berkeley Mono",
  "buffer_font_features": { "calt": false },
  "buffer_font_size": 15,
  "ui_font_size": 16,
  "theme": { "mode": "dark", "light": "One Light", "dark": "Fadetouched Blur" },
  "display": {
    "timestamps": "delta",          // off | absolute | relative | delta
    "timestamp_format": "%H:%M:%S%.3f",
    "view": "text",                 // text | hex | hex_ascii
    "hex_bytes_per_row": 16,
    "show_control_chars": true,     // render CR/LF/ESC as dim glyphs
    "wrap": true,
  },
  "devices": [
    { "match": { "vid": "0x0e8d", "product": "Airoha" },
      "baud": 921600, "plugin": "airoha-race", "eol": "crlf", },
  ],
}"#;

fn user_settings(text: &str) -> Settings {
    Settings::from_jsonc(text).unwrap_or_else(|err| panic!("settings failed to load: {err}"))
}

fn error_of(text: &str) -> SettingsError {
    Settings::from_jsonc(text).expect_err("settings should be rejected")
}

fn position(err: &SettingsError) -> (usize, usize) {
    match err {
        SettingsError::Invalid { line, column, .. } => (*line, *column),
        other => panic!("expected an Invalid error, got {other:?}"),
    }
}

#[test]
fn plan_sample_parses_and_resolves() {
    let settings = user_settings(PLAN_SAMPLE);
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);

    assert_eq!(
        settings.buffer_font_family.as_deref(),
        Some("Berkeley Mono")
    );
    assert_eq!(settings.buffer_font_size, 15.0);
    assert_eq!(settings.ui_font_size, 16.0);
    assert_eq!(settings.buffer_font_features.get("calt"), Some(0));
    assert!(!settings.buffer_font_features.ligatures_enabled());
    assert_eq!(
        settings.theme,
        ThemeSelection::Dynamic {
            mode: ThemeMode::Dark,
            light: "One Light".into(),
            dark: "Fadetouched Blur".into(),
        }
    );
    assert_eq!(settings.theme.name(false), "Fadetouched Blur");

    let display = &settings.display;
    assert_eq!(display.timestamps, TimestampMode::Delta);
    assert_eq!(display.timestamp_format, "%H:%M:%S%.3f");
    assert_eq!(display.view, DisplayView::Text);
    assert_eq!(display.hex_bytes_per_row, 16);
    assert!(display.show_control_chars);
    assert!(display.wrap);

    let port = airoha_port();
    let profile = settings.profile_for(&port).expect("the Airoha profile");
    assert_eq!(profile.baud, Some(921_600));
    assert_eq!(profile.plugin.as_deref(), Some("airoha-race"));
    assert_eq!(profile.eol, Some(LineEnding::Crlf));
    assert_eq!(settings.serial_config_for(&port).baud, 921_600);

    let other = usb_port(
        "/dev/cu.usbserial-1",
        0x0403,
        0x6001,
        Some("FT232R"),
        None,
        None,
    );
    assert!(settings.profile_for(&other).is_none());
    assert_eq!(settings.serial_config_for(&other).baud, 115_200);
}

#[test]
fn bundled_defaults_are_complete_and_as_documented() {
    let settings = Settings::default();
    assert_eq!(settings.buffer_font_family, None);
    assert_eq!(settings.buffer_font_size, 15.0);
    assert_eq!(settings.buffer_font_weight, 400.0);
    assert!(settings.buffer_font_features.is_empty());
    assert_eq!(settings.buffer_line_height, LineHeight::Standard);
    assert!(settings.buffer_font_fallbacks.is_empty());
    assert_eq!(settings.ui_font_size, 16.0);
    assert_eq!(settings.ui_font_weight, 400.0);
    assert_eq!(settings.terminal, TerminalSettings::default());
    assert_eq!(
        settings.theme,
        ThemeSelection::Dynamic {
            mode: ThemeMode::System,
            light: "Serialist Light".into(),
            dark: "Serialist Dark".into(),
        }
    );
    assert_eq!(settings.display.timestamps, TimestampMode::Off);
    assert_eq!(settings.display.timestamp_format, "%H:%M:%S%.3f");
    assert_eq!(settings.display.view, DisplayView::Text);
    assert_eq!(settings.display.hex_bytes_per_row, 16);
    assert!(!settings.display.show_control_chars);
    assert!(!settings.display.wrap);
    assert_eq!(settings.scrollback_budget_bytes, 256 * 1024 * 1024);
    assert_eq!(settings.default_baud, 115_200);
    assert_eq!(settings.line_ending, LineEnding::Crlf);
    assert!(!settings.local_echo);
    assert!(settings.restore_session);
    assert!(settings.devices.is_empty());
    assert!(settings.warnings.is_empty());
    assert_eq!(settings, load_settings(None, None).unwrap());
    assert_eq!(settings, Settings::from_jsonc("").unwrap());
}

#[test]
fn a_partial_document_deserializes_with_bundled_defaults() {
    // The types themselves fill in missing keys, so a `Settings` can be read from any
    // document without the loader.
    let settings: Settings = serde_json::from_str(r#"{ "buffer_font_size": 12 }"#).unwrap();
    assert_eq!(settings.buffer_font_size, 12.0);
    assert_eq!(settings.ui_font_size, 16.0);
    assert_eq!(settings.display.hex_bytes_per_row, 16);
}

#[test]
fn comments_and_trailing_commas_are_accepted() {
    let text =
        "// leading\n{ /* block */ \"buffer_font_size\": 14, // tail\n \"devices\": [],\n}\n// end";
    assert_eq!(user_settings(text).buffer_font_size, 14.0);
    let comments_only = "// nothing to see\n/* here */\n";
    assert_eq!(user_settings(comments_only), Settings::default());
}

#[test]
fn a_non_object_root_is_rejected() {
    let err = error_of("[1, 2]");
    assert!(err.to_string().contains("JSON object"), "{err}");
}

#[test]
fn layering_is_defaults_then_user_then_project() {
    let dir = TempDir::new("layers");
    let user = dir.write(
        "user.json",
        r#"{
            "buffer_font_size": 13,
            "buffer_font_family": "User Mono",
            "display": { "wrap": true, "timestamps": "absolute" },
            "terminal": { "font_size": 12, "font_family": "Terminal Mono" },
            "buffer_font_fallbacks": ["A", "B"],
            "devices": [ { "match": { "path": "/dev/a" }, "baud": 9600 } ]
        }"#,
    );
    let project = dir.write(
        "project.json",
        r#"{
            "buffer_font_size": 11,
            "display": { "timestamps": "delta" },
            "terminal": { "font_size": null },
            "buffer_font_fallbacks": ["C"],
            "devices": [ { "match": { "path": "/dev/b" }, "baud": 19200 } ]
        }"#,
    );

    let user_only = load_settings(Some(&user), None).unwrap();
    assert_eq!(user_only.buffer_font_size, 13.0);
    assert_eq!(user_only.terminal.font_size, Some(12.0));

    let both = load_settings(Some(&user), Some(&project)).unwrap();
    // A later layer wins for scalars.
    assert_eq!(both.buffer_font_size, 11.0);
    // Objects merge: the project changed timestamps only, the user's wrap survives, and
    // untouched keys keep their bundled defaults.
    assert_eq!(both.display.timestamps, TimestampMode::Delta);
    assert!(both.display.wrap);
    assert_eq!(both.display.hex_bytes_per_row, 16);
    // A user key the project does not mention stays.
    assert_eq!(both.buffer_font_family.as_deref(), Some("User Mono"));
    // Nested objects merge too, and `null` unsets an optional key.
    assert_eq!(both.terminal.font_family.as_deref(), Some("Terminal Mono"));
    assert_eq!(both.terminal.font_size, None);
    assert_eq!(both.resolved_terminal_font().size, 11.0);
    // Arrays replace rather than concatenate.
    assert_eq!(both.buffer_font_fallbacks, vec!["C".to_string()]);
    assert_eq!(both.devices.len(), 1);
    assert_eq!(both.devices[0].r#match.path.as_deref(), Some("/dev/b"));
    assert_eq!(both.devices[0].baud, Some(19_200));
}

#[test]
fn a_missing_file_adds_no_layer() {
    let dir = TempDir::new("missing");
    let missing = dir.path().join("nope.json");
    assert_eq!(
        load_settings(Some(&missing), Some(&missing)).unwrap(),
        Settings::default()
    );
}

#[test]
fn a_theme_string_replaces_a_theme_object_and_back() {
    let dir = TempDir::new("theme-layers");
    let user = dir.write(
        "user.json",
        r#"{ "theme": { "mode": "light", "light": "Mine" } }"#,
    );
    let project = dir.write("project.json", r#"{ "theme": "Pinned" }"#);

    let user_only = load_settings(Some(&user), None).unwrap();
    assert_eq!(
        user_only.theme,
        ThemeSelection::Dynamic {
            mode: ThemeMode::Light,
            light: "Mine".into(),
            // The object merged with the bundled one, so `dark` keeps its default.
            dark: "Serialist Dark".into(),
        }
    );
    let both = load_settings(Some(&user), Some(&project)).unwrap();
    assert_eq!(both.theme, ThemeSelection::Static("Pinned".into()));
    assert_eq!(both.theme.name(true), "Pinned");
    assert_eq!(both.theme.name(false), "Pinned");
}

#[test]
fn theme_object_defaults_missing_names_to_the_bundled_ones() {
    let settings = user_settings(r#"{ "theme": { "mode": "system", "dark": "Night" } }"#);
    assert_eq!(settings.theme.name(true), "Night");
    assert_eq!(settings.theme.name(false), "Serialist Light");
    assert!(settings.theme.prefers_dark(true));
    assert!(!settings.theme.prefers_dark(false));
    let light = user_settings(r#"{ "theme": { "mode": "light" } }"#);
    assert!(!light.theme.prefers_dark(true));
}

#[test]
fn font_features_read_booleans_and_integers() {
    let settings = user_settings(
        r#"{ "buffer_font_features": { "calt": false, "ss01": true, "cv01": 7 },
             "ui_font_features": { "tnum": 1 } }"#,
    );
    let features = &settings.buffer_font_features;
    assert_eq!(features.get("calt"), Some(0));
    assert_eq!(features.get("ss01"), Some(1));
    assert_eq!(features.get("cv01"), Some(7));
    assert_eq!(features.get("liga"), None);
    assert_eq!(features.len(), 3);
    assert_eq!(
        features.iter().collect::<Vec<_>>(),
        vec![("calt", 0), ("cv01", 7), ("ss01", 1)]
    );
    assert!(!features.ligatures_enabled());
    assert_eq!(settings.ui_font_features.get("tnum"), Some(1));
    assert!(settings.ui_font_features.ligatures_enabled());
    assert!(FontFeatures::new().ligatures_enabled());

    let liga_off = user_settings(r#"{ "buffer_font_features": { "liga": 0 } }"#);
    assert!(!liga_off.buffer_font_features.ligatures_enabled());
    let calt_on = user_settings(r#"{ "buffer_font_features": { "calt": true } }"#);
    assert!(calt_on.buffer_font_features.ligatures_enabled());
}

#[test]
fn font_features_reject_bad_tags_and_values() {
    let err = error_of(r#"{ "buffer_font_features": { "ligatures": true } }"#);
    assert!(err.to_string().contains("four ASCII"), "{err}");
    let err = error_of("{\n \"buffer_font_features\": { \"calt\": \"off\" }\n}");
    assert!(err.to_string().contains("true, false"), "{err}");
    assert_eq!(position(&err).0, 2);
    let err = error_of(r#"{ "buffer_font_features": { "calt": -1 } }"#);
    assert!(err.to_string().contains("non-negative"), "{err}");
}

#[test]
fn line_height_has_three_forms_and_zeds_custom_object() {
    let height = |json: &str| user_settings(json).buffer_line_height;
    assert_eq!(
        height(r#"{ "buffer_line_height": "comfortable" }"#),
        LineHeight::Comfortable
    );
    assert_eq!(
        height(r#"{ "buffer_line_height": "standard" }"#),
        LineHeight::Standard
    );
    assert_eq!(
        height(r#"{ "buffer_line_height": 1.45 }"#),
        LineHeight::Custom(1.45)
    );
    assert_eq!(
        height(r#"{ "buffer_line_height": 2 }"#),
        LineHeight::Custom(2.0)
    );
    assert_eq!(
        height(r#"{ "buffer_line_height": { "custom": 1.5 } }"#),
        LineHeight::Custom(1.5)
    );

    assert_eq!(LineHeight::Comfortable.value(), 1.618);
    assert_eq!(LineHeight::Standard.value(), 1.3);
    assert_eq!(LineHeight::Custom(1.5).value(), 1.5);

    for bad in [
        r#"{ "buffer_line_height": "airy" }"#,
        r#"{ "buffer_line_height": 0 }"#,
        r#"{ "buffer_line_height": -1.2 }"#,
        r#"{ "buffer_line_height": { "custom": 0 } }"#,
        r#"{ "buffer_line_height": { "size": 1.2 } }"#,
        r#"{ "buffer_line_height": {} }"#,
        r#"{ "buffer_line_height": true }"#,
    ] {
        assert!(
            Settings::from_jsonc(bad).is_err(),
            "{bad} should be rejected"
        );
    }
}

#[test]
fn terminal_keys_override_the_buffer_font() {
    let base = user_settings(
        r#"{
            "buffer_font_family": "Buffer Mono",
            "buffer_font_size": 15,
            "buffer_font_weight": 500,
            "buffer_font_features": { "calt": false },
            "buffer_line_height": "comfortable",
            "buffer_font_fallbacks": ["Fallback A"]
        }"#,
    );
    let font = base.resolved_terminal_font();
    assert_eq!(font.family.as_deref(), Some("Buffer Mono"));
    assert_eq!(font.size, 15.0);
    assert_eq!(font.weight, 500.0);
    assert_eq!(font.features.get("calt"), Some(0));
    assert_eq!(font.fallbacks, vec!["Fallback A".to_string()]);
    assert_eq!(font.line_height, LineHeight::COMFORTABLE_VALUE);
    assert!((font.line_height_px() - 15.0 * 1.618).abs() < 1e-4);

    let overridden = user_settings(
        r#"{
            "buffer_font_family": "Buffer Mono",
            "buffer_font_size": 15,
            "buffer_line_height": "comfortable",
            "buffer_font_features": { "calt": false },
            "terminal": {
                "font_family": "Terminal Mono",
                "font_size": 13.5,
                "font_weight": 300,
                "font_features": { "ss01": true },
                "font_fallbacks": [],
                "line_height": { "custom": 1.1 }
            }
        }"#,
    );
    let font = overridden.resolved_terminal_font();
    assert_eq!(font.family.as_deref(), Some("Terminal Mono"));
    assert_eq!(font.size, 13.5);
    assert_eq!(font.weight, 300.0);
    // A terminal feature map replaces the buffer's rather than merging with it.
    assert_eq!(font.features.get("ss01"), Some(1));
    assert_eq!(font.features.get("calt"), None);
    assert!(font.fallbacks.is_empty());
    assert_eq!(font.line_height, 1.1);
}

#[test]
fn the_ui_font_uses_the_ui_keys() {
    let settings = user_settings(
        r#"{
            "ui_font_family": "Inter",
            "ui_font_size": 14,
            "ui_font_weight": 450,
            "ui_font_features": { "tnum": true },
            "ui_font_fallbacks": ["Noto Sans"]
        }"#,
    );
    let font = settings.resolved_ui_font();
    assert_eq!(font.family.as_deref(), Some("Inter"));
    assert_eq!(font.size, 14.0);
    assert_eq!(font.weight, 450.0);
    assert_eq!(font.features.get("tnum"), Some(1));
    assert_eq!(font.fallbacks, vec!["Noto Sans".to_string()]);
    assert_eq!(font.line_height, 1.3);
    // The default UI font has no family, like the terminal font.
    assert_eq!(Settings::default().resolved_ui_font().family, None);
    assert_eq!(Settings::default().resolved_ui_font().size, 16.0);
    assert_eq!(Settings::default().resolved_terminal_font().size, 15.0);
}

#[test]
fn out_of_range_values_are_errors_with_positions() {
    let err = error_of("{\n  \"buffer_font_weight\": 950\n}");
    assert!(err.to_string().contains("100 to 900"), "{err}");
    assert_eq!(position(&err), (2, 25));

    let err = error_of("{\n  \"buffer_font_size\": 0\n}");
    assert!(err.to_string().contains("positive"), "{err}");
    assert_eq!(position(&err).0, 2);

    assert!(Settings::from_jsonc(r#"{ "default_baud": 0 }"#).is_err());
    assert!(Settings::from_jsonc(r#"{ "default_baud": "fast" }"#).is_err());
    assert!(Settings::from_jsonc(r#"{ "display": { "hex_bytes_per_row": 0 } }"#).is_err());
    assert!(Settings::from_jsonc(r#"{ "display": { "hex_bytes_per_row": 999 } }"#).is_err());
}

#[test]
fn errors_carry_file_line_column_and_message() {
    let dir = TempDir::new("errors");
    let bad_syntax = dir.write("syntax.json", "{\n  \"buffer_font_size\": ,\n}\n");
    let err = load_settings(Some(&bad_syntax), None).unwrap_err();
    match &err {
        SettingsError::Invalid {
            file,
            line,
            column,
            message,
        } => {
            assert_eq!(file, &bad_syntax);
            assert_eq!(*line, 2);
            assert!(*column >= 1);
            assert!(!message.is_empty());
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    assert_eq!(err.file(), Some(bad_syntax.as_path()));
    assert!(
        err.to_string()
            .starts_with(&bad_syntax.display().to_string())
    );

    let bad_type = dir.write("type.json", "{\n  \"buffer_font_size\": \"big\",\n}\n");
    let err = load_settings(None, Some(&bad_type)).unwrap_err();
    let SettingsError::Invalid {
        file,
        line,
        column,
        message,
    } = &err
    else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(file, &bad_type);
    assert_eq!((*line, *column), (2, 23));
    assert!(message.contains("expected a font size"), "{message}");

    // The error names the file that is wrong even when an earlier layer is fine.
    let good = dir.write("good.json", "{ \"buffer_font_size\": 12 }");
    let err = load_settings(Some(&good), Some(&bad_type)).unwrap_err();
    assert_eq!(err.file(), Some(bad_type.as_path()));

    let bad_enum = dir.write(
        "enum.json",
        "{\n \"display\": {\n  \"view\": \"binary\"\n }\n}",
    );
    let err = load_settings(Some(&bad_enum), None).unwrap_err();
    let (line, _) = position(&err);
    assert_eq!(line, 3);
    assert!(err.to_string().contains("binary"), "{err}");
}

#[test]
fn unknown_keys_are_warnings_not_errors() {
    let dir = TempDir::new("unknown");
    let path = dir.write(
        "settings.json",
        r#"{
            "buffer_font_sizee": 1,
            "display": { "wrapp": true, "wrap": true },
            "devices": [ { "match": { "vidd": 1, "vid": 1 }, "bogus": 2 } ],
            "buffer_font_features": { "zzzz": 1 },
            "terminal": { "colour": "red" },
            "theme": { "mode": "dark", "extra": 1 },
            "some_zed_only_setting": { "a": 1 }
        }"#,
    );
    let settings = load_settings(Some(&path), None).unwrap();
    assert!(settings.display.wrap);
    let mut keys: Vec<&str> = settings.warnings.iter().map(|w| w.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "buffer_font_sizee",
            "devices[0].bogus",
            "devices[0].match.vidd",
            "display.wrapp",
            "some_zed_only_setting",
            "terminal.colour",
            "theme.extra",
        ]
    );
    let warning = &settings.warnings[0];
    assert_eq!(warning.file, path);
    assert!(warning.message.contains(&warning.key));
    assert!(warning.to_string().contains("unknown setting"));
}

#[test]
fn known_keys_produce_no_warnings() {
    let settings = user_settings(
        r#"{
            "terminal": { "font_family": "X", "font_size": 1, "font_weight": 400,
                          "font_features": { "calt": 0 }, "font_fallbacks": [], "line_height": 1.2 },
            "devices": [ { "name": "n", "match": { "vid": 1, "pid": 2, "product": "p",
                           "manufacturer": "m", "serial_number": "s", "path": "/dev" },
                           "baud": 9600, "data_bits": 8, "parity": "none", "stop_bits": 1,
                           "flow_control": "none", "plugin": "p", "eol": "lf", "on_connect": "a.lua" } ],
            "scrollback_budget_bytes": 1048576, "default_baud": 9600,
            "line_ending": "lf", "local_echo": true
        }"#,
    );
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
}

#[test]
fn null_unsets_optional_keys() {
    let settings = user_settings(
        r#"{ "buffer_font_family": null, "terminal": { "font_family": null }, "ui_font_family": null }"#,
    );
    assert_eq!(settings.buffer_font_family, None);
    assert_eq!(settings.resolved_terminal_font().family, None);
    // A key that has a default is not optional.
    assert!(Settings::from_jsonc(r#"{ "buffer_font_size": null }"#).is_err());
}

#[test]
fn settings_round_trip_through_json() {
    let settings = user_settings(PLAN_SAMPLE);
    let json = serde_json::to_value(&settings).unwrap();
    let mut back: Settings = serde_json::from_value(json).unwrap();
    back.warnings = settings.warnings.clone();
    assert_eq!(back, settings);

    let with_forms = user_settings(
        r#"{ "theme": "Named", "buffer_line_height": { "custom": 1.7 },
             "terminal": { "font_features": { "ss01": 1, "calt": 0 } },
             "devices": [ { "match": { "pid": 3725 }, "data_bits": 7, "parity": "even",
                            "stop_bits": 2, "flow_control": "software" } ] }"#,
    );
    let json = serde_json::to_value(&with_forms).unwrap();
    let mut back: Settings = serde_json::from_value(json).unwrap();
    back.warnings = with_forms.warnings.clone();
    assert_eq!(back, with_forms);
}

#[test]
fn usb_ids_accept_hex_strings_and_integers() {
    let settings = user_settings(
        r#"{ "devices": [
            { "match": { "vid": "0x0e8d" } },
            { "match": { "vid": "0X0E8D" } },
            { "match": { "vid": "0e8d" } },
            { "match": { "vid": 3725 } },
            { "match": { "vid": 0x0e8d } }
        ] }"#,
    );
    for profile in &settings.devices {
        assert_eq!(profile.r#match.vid, Some(UsbId(0x0e8d)));
    }
    assert_eq!(UsbId(0x0e8d).to_string(), "0x0e8d");
    for bad in [
        r#"{ "devices": [ { "match": { "vid": "0x10000" } } ] }"#,
        r#"{ "devices": [ { "match": { "vid": 70000 } } ] }"#,
        r#"{ "devices": [ { "match": { "vid": "zzzz" } } ] }"#,
        r#"{ "devices": [ { "match": { "vid": true } } ] }"#,
        r#"{ "devices": [ { "match": { "vid": -1 } } ] }"#,
    ] {
        assert!(Settings::from_jsonc(bad).is_err(), "{bad}");
    }
    // `match` is required, so a profile cannot apply to everything by accident.
    assert!(Settings::from_jsonc(r#"{ "devices": [ { "baud": 9600 } ] }"#).is_err());
}

#[test]
fn device_matching_rules() {
    let profile = |json: &str| -> DeviceProfile {
        let settings = user_settings(&format!(r#"{{ "devices": [ {json} ] }}"#));
        settings.devices[0].clone()
    };
    let port = airoha_port();

    assert!(profile(r#"{ "match": { "vid": "0x0e8d" } }"#).matches(&port));
    assert!(profile(r#"{ "match": { "vid": "0x0e8d", "pid": "0x0616" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "vid": "0x0e8d", "pid": "0x0617" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "vid": "0x0e8e" } }"#).matches(&port));

    // Text keys are case-insensitive substrings.
    assert!(profile(r#"{ "match": { "product": "airoha" } }"#).matches(&port));
    assert!(profile(r#"{ "match": { "product": "HEADSET DEB" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "product": "bose" } }"#).matches(&port));
    assert!(profile(r#"{ "match": { "manufacturer": "technology" } }"#).matches(&port));
    assert!(profile(r#"{ "match": { "serial_number": "ab12" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "serial_number": "zz" } }"#).matches(&port));

    // Path is exact or a prefix, with no glob syntax.
    assert!(profile(r#"{ "match": { "path": "/dev/cu.usbmodem1101" } }"#).matches(&port));
    assert!(profile(r#"{ "match": { "path": "/dev/cu.usbmodem" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "path": "/dev/tty.usbmodem" } }"#).matches(&port));
    assert!(!profile(r#"{ "match": { "path": "/dev/cu.usbmodem*" } }"#).matches(&port));

    // Every key that is set has to match.
    assert!(
        profile(r#"{ "match": { "vid": 3725, "product": "Airoha", "path": "/dev/cu" } }"#)
            .matches(&port)
    );
    assert!(
        !profile(r#"{ "match": { "vid": 3725, "product": "Bose", "path": "/dev/cu" } }"#)
            .matches(&port)
    );

    // A key the port has no value for does not match.
    let bare = usb_port("COM3", 0x0e8d, 0x0616, None, None, None);
    assert!(!profile(r#"{ "match": { "product": "Airoha" } }"#).matches(&bare));
    assert!(profile(r#"{ "match": { "vid": "0x0e8d" } }"#).matches(&bare));

    // Non-USB ports only satisfy path matches.
    let bluetooth = PortInfo {
        id: PortId::new("/dev/cu.Bluetooth-Incoming-Port"),
        kind: PortKind::Bluetooth,
        display_name: "Bluetooth".into(),
    };
    assert!(!profile(r#"{ "match": { "vid": "0x0e8d" } }"#).matches(&bluetooth));
    assert!(!profile(r#"{ "match": { "product": "x" } }"#).matches(&bluetooth));
    assert!(profile(r#"{ "match": { "path": "/dev/cu.Bluetooth" } }"#).matches(&bluetooth));
    assert!(profile(r#"{ "match": {} }"#).matches(&bluetooth));
    let virtual_port = PortInfo {
        id: PortId::new("virtual:echo"),
        kind: PortKind::Virtual,
        display_name: "Echo".into(),
    };
    assert!(profile(r#"{ "match": { "path": "virtual:" } }"#).matches(&virtual_port));
}

#[test]
fn the_first_matching_profile_wins() {
    let settings = user_settings(
        r#"{
            "line_ending": "lf",
            "default_baud": 57600,
            "devices": [
                { "name": "Other", "match": { "vid": "0x1234" }, "baud": 1 },
                { "name": "Airoha first", "match": { "vid": "0x0e8d" }, "baud": 921600,
                  "data_bits": 7, "parity": "even", "stop_bits": 2, "flow_control": "hardware",
                  "eol": "cr", "on_connect": "scripts/init.lua" },
                { "name": "Airoha second", "match": { "product": "Airoha" }, "baud": 2 },
                { "name": "Anything", "match": {}, "baud": 3 }
            ]
        }"#,
    );
    let port = airoha_port();
    let profile = settings.profile_for(&port).unwrap();
    assert_eq!(profile.name.as_deref(), Some("Airoha first"));
    assert_eq!(settings.device_name_for(&port), Some("Airoha first"));

    let config = settings.serial_config_for(&port);
    assert_eq!(config.baud, 921_600);
    assert_eq!(config.data_bits, DataBits::Seven);
    assert_eq!(config.parity, Parity::Even);
    assert_eq!(config.stop_bits, StopBits::Two);
    assert_eq!(config.flow_control, FlowControl::Hardware);
    assert_eq!(settings.line_ending_for(&port), LineEnding::Cr);
    assert_eq!(
        settings.on_connect_for(&port),
        Some(&PathBuf::from("scripts/init.lua"))
    );

    // The catch-all profile applies to a port nothing else matches.
    let other = usb_port("/dev/ttyUSB0", 0x0403, 0x6001, Some("FT232R"), None, None);
    assert_eq!(settings.device_name_for(&other), Some("Anything"));
    assert_eq!(settings.serial_config_for(&other).baud, 3);
    // No profile eol: the global line ending applies.
    assert_eq!(settings.line_ending_for(&other), LineEnding::Lf);

    // Without a match the default baud applies.
    let bare = user_settings(r#"{ "default_baud": 57600 }"#);
    assert_eq!(bare.serial_config_for(&other).baud, 57_600);
    assert_eq!(bare.serial_config_for(&other).data_bits, DataBits::Eight);
}

#[test]
fn line_settings_accept_numbers_and_names() {
    let settings = user_settings(
        r#"{ "devices": [
            { "match": {}, "data_bits": "five", "stop_bits": "two", "parity": "O", "flow_control": "rtscts" },
            { "match": {}, "data_bits": 6, "stop_bits": 1, "parity": "mark", "flow_control": "xonxoff" }
        ] }"#,
    );
    let first = &settings.devices[0];
    assert_eq!(first.data_bits, Some(DataBits::Five));
    assert_eq!(first.stop_bits, Some(StopBits::Two));
    assert_eq!(first.parity, Some(Parity::Odd));
    assert_eq!(first.flow_control, Some(FlowControl::Hardware));
    let second = &settings.devices[1];
    assert_eq!(second.data_bits, Some(DataBits::Six));
    assert_eq!(second.stop_bits, Some(StopBits::One));
    assert_eq!(second.parity, Some(Parity::Mark));
    assert_eq!(second.flow_control, Some(FlowControl::Software));

    for bad in [
        r#"{ "devices": [ { "match": {}, "data_bits": 9 } ] }"#,
        r#"{ "devices": [ { "match": {}, "stop_bits": 3 } ] }"#,
        r#"{ "devices": [ { "match": {}, "parity": "sometimes" } ] }"#,
        r#"{ "devices": [ { "match": {}, "flow_control": "magic" } ] }"#,
        r#"{ "devices": [ { "match": {}, "baud": 0 } ] }"#,
        r#"{ "devices": [ { "match": {}, "eol": "lfcr" } ] }"#,
    ] {
        assert!(Settings::from_jsonc(bad).is_err(), "{bad}");
    }
}

#[test]
fn line_endings_have_bytes_and_labels() {
    assert_eq!(LineEnding::None.bytes(), b"");
    assert_eq!(LineEnding::Cr.bytes(), b"\r");
    assert_eq!(LineEnding::Lf.bytes(), b"\n");
    assert_eq!(LineEnding::Crlf.bytes(), b"\r\n");
    assert_eq!(LineEnding::Crlf.label(), "CRLF");
    assert_eq!(LineEnding::default(), LineEnding::Crlf);
    assert_eq!(LineEnding::ALL.len(), 4);
}

#[test]
fn scrollback_budget_converts_to_usize() {
    let settings = user_settings(r#"{ "scrollback_budget_bytes": 1048576 }"#);
    assert_eq!(settings.scrollback_budget(), 1_048_576);
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
        .collect();
    move |name| map.get(name).cloned()
}

#[test]
fn config_dir_follows_the_platform() {
    let unix = env_of(&[("HOME", "/home/john")]);
    for platform in [Platform::MacOs, Platform::Linux] {
        let paths = ConfigPaths::from_environment(platform, &unix);
        assert_eq!(paths.dir, Path::new("/home/john/.config/serialist"));
        assert_eq!(
            paths.settings,
            Path::new("/home/john/.config/serialist/settings.json")
        );
        assert_eq!(
            paths.keymap,
            Path::new("/home/john/.config/serialist/keymap.json")
        );
        assert_eq!(
            paths.themes,
            Path::new("/home/john/.config/serialist/themes")
        );
        assert_eq!(paths.project_settings, None);
    }

    let windows = env_of(&[("APPDATA", "C:\\Users\\john\\AppData\\Roaming")]);
    let paths = ConfigPaths::from_environment(Platform::Windows, &windows);
    assert_eq!(
        paths.dir,
        Path::new("C:\\Users\\john\\AppData\\Roaming").join("Serialist")
    );
    assert!(paths.settings.ends_with("settings.json"));

    let profile_only = env_of(&[("USERPROFILE", "C:\\Users\\john")]);
    let paths = ConfigPaths::from_environment(Platform::Windows, &profile_only);
    assert_eq!(
        paths.dir,
        Path::new("C:\\Users\\john")
            .join("AppData")
            .join("Roaming")
            .join("Serialist")
    );
}

#[test]
fn the_environment_variable_overrides_every_platform() {
    for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
        let env = env_of(&[
            (CONFIG_DIR_ENV, "/tmp/custom"),
            ("HOME", "/home/john"),
            ("APPDATA", "C:\\Roaming"),
        ]);
        let paths = ConfigPaths::from_environment(platform, &env);
        assert_eq!(paths.dir, Path::new("/tmp/custom"));
        assert_eq!(paths.settings, Path::new("/tmp/custom/settings.json"));
        assert_eq!(paths.themes, Path::new("/tmp/custom/themes"));
    }
    // An empty override is ignored.
    let env = env_of(&[(CONFIG_DIR_ENV, ""), ("HOME", "/home/john")]);
    let paths = ConfigPaths::from_environment(Platform::Linux, &env);
    assert_eq!(paths.dir, Path::new("/home/john/.config/serialist"));
}

#[test]
fn with_no_home_the_config_falls_back_to_the_temp_dir() {
    let paths = ConfigPaths::from_environment(Platform::Linux, &env_of(&[]));
    assert!(paths.dir.starts_with(std::env::temp_dir()));
}

#[test]
fn default_for_platform_names_the_expected_files() {
    let paths = ConfigPaths::default_for_platform();
    assert_eq!(paths.settings.file_name().unwrap(), "settings.json");
    assert_eq!(paths.keymap.file_name().unwrap(), "keymap.json");
    assert_eq!(paths.themes.file_name().unwrap(), "themes");
    assert_eq!(paths.settings.parent(), Some(paths.dir.as_path()));
}

#[test]
fn project_settings_are_found_by_searching_upward() {
    let root = TempDir::new("project");
    let project_file = root.write("repo/.serialist/settings.json", "{}");
    let deep = root.path().join("repo/firmware/build/out");
    std::fs::create_dir_all(&deep).unwrap();

    assert_eq!(
        ConfigPaths::project_settings_path(&deep),
        Some(project_file.clone())
    );
    assert_eq!(
        project_settings_path(&root.path().join("repo")),
        Some(project_file.clone())
    );

    // The nearest file wins.
    let nearer = root.write("repo/firmware/.serialist/settings.json", "{}");
    assert_eq!(
        ConfigPaths::project_settings_path(&deep),
        Some(nearer.clone())
    );

    // A `.serialist` directory without the file does not count.
    std::fs::create_dir_all(root.path().join("repo/firmware/build/.serialist")).unwrap();
    assert_eq!(ConfigPaths::project_settings_path(&deep), Some(nearer));

    let elsewhere = TempDir::new("no-project");
    // Nothing between here and the filesystem root, as long as the temp dir is not
    // itself inside a project.
    if elsewhere
        .path()
        .ancestors()
        .all(|dir| !dir.join(".serialist").join("settings.json").is_file())
    {
        assert_eq!(ConfigPaths::project_settings_path(elsewhere.path()), None);
    }

    let paths = ConfigPaths::new(root.path().join("config")).with_project_from(&deep);
    assert!(paths.project_settings.is_some());
}

#[test]
fn ensure_settings_file_writes_a_commented_template_once() {
    let root = TempDir::new("ensure");
    let paths = ConfigPaths::new(root.path().join("nested").join("config"));

    assert!(paths.ensure_settings_file().unwrap());
    let text = std::fs::read_to_string(&paths.settings).unwrap();
    assert!(text.contains("  // \"buffer_font_size\": 15.0,"), "{text}");
    assert!(text.contains("    // \"wrap\": false"), "{text}");
    // The template is valid, changes nothing, and lists every default key.
    let loaded = load_settings(Some(&paths.settings), None).unwrap();
    assert_eq!(loaded, Settings::default());
    assert!(loaded.warnings.is_empty());
    for line in text.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed.is_empty() || trimmed.starts_with("//") || trimmed == "{" || trimmed == "}",
            "live line in the template: {line:?}"
        );
    }

    // A second call leaves the user's file alone.
    std::fs::write(&paths.settings, "{ \"buffer_font_size\": 20 }").unwrap();
    assert!(!paths.ensure_settings_file().unwrap());
    assert_eq!(
        std::fs::read_to_string(&paths.settings).unwrap(),
        "{ \"buffer_font_size\": 20 }"
    );
}

#[test]
fn uncommenting_a_template_line_overrides_that_key() {
    let template = settings_template().replace(
        "  // \"buffer_font_size\": 15.0,",
        "  \"buffer_font_size\": 18.0,",
    );
    let settings = user_settings(&template);
    assert_eq!(settings.buffer_font_size, 18.0);
}

#[test]
fn bundled_defaults_text_is_exposed() {
    assert!(DEFAULT_SETTINGS_JSONC.contains("\"buffer_font_size\": 15.0"));
}

#[test]
fn commands_and_history_live_in_the_config_directory() {
    let paths = ConfigPaths::new("/cfg/serialist");
    assert_eq!(paths.commands_dir(), Path::new("/cfg/serialist/commands"));
    assert_eq!(
        paths.history_path(),
        Path::new("/cfg/serialist/history.jsonl")
    );
    assert_eq!(paths.project_commands, None);
}

#[test]
fn project_commands_are_found_by_searching_upward() {
    let root = TempDir::new("project-commands");
    let deep = root.path().join("repo").join("fw").join("src");
    std::fs::create_dir_all(&deep).unwrap();
    assert_eq!(ConfigPaths::project_commands_path(&deep), None);

    let outer = root.write("repo/.serialist/commands.json", "{}");
    assert_eq!(ConfigPaths::project_commands_path(&deep), Some(outer));
    // A nearer one wins.
    let nearer = root.write("repo/fw/.serialist/commands.json", "{}");
    assert_eq!(ConfigPaths::project_commands_path(&deep), Some(nearer));
    // A directory of that name is not a file.
    let elsewhere = TempDir::new("no-commands");
    std::fs::create_dir_all(elsewhere.path().join(".serialist/commands.json")).unwrap();
    assert_eq!(ConfigPaths::project_commands_path(elsewhere.path()), None);

    // Settings and commands are found independently.
    root.write("repo/.serialist/settings.json", "{}");
    let paths = ConfigPaths::new(root.path().join("config")).with_project_from(&deep);
    assert!(paths.project_settings.is_some());
    assert!(paths.project_commands.is_some());
    let only_settings = TempDir::new("only-settings");
    only_settings.write(".serialist/settings.json", "{}");
    let paths =
        ConfigPaths::new(root.path().join("config")).with_project_from(only_settings.path());
    assert!(paths.project_settings.is_some());
    assert_eq!(paths.project_commands, None);
}

/// The template's commented lines with the leading `// ` taken off.
fn uncommented(template: &str) -> String {
    template
        .lines()
        .skip_while(|line| !line.trim().is_empty())
        .map(|line| {
            let trimmed = line.trim_start();
            match trimmed.strip_prefix("// ") {
                Some(code) => format!("{}{code}", &line[..line.len() - trimmed.len()]),
                None => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_keymap_template_is_valid_changes_nothing_and_uncomments_to_the_defaults() {
    for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
        let template = keymap_template(platform);
        let parsed = crate::Keymap::parse(&template, Path::new("keymap.json")).unwrap();
        assert!(parsed.entries.is_empty(), "{platform:?}");
        for line in template.lines() {
            let trimmed = line.trim();
            assert!(
                trimmed.is_empty() || trimmed.starts_with("//") || trimmed == "[" || trimmed == "]",
                "live line in the {platform:?} template: {line:?}"
            );
        }
        // Uncommenting every line gives back the bundled defaults.
        let restored = crate::Keymap::parse(&uncommented(&template), Path::new("keymap.json"))
            .unwrap_or_else(|err| panic!("{platform:?}: {err}"));
        assert_eq!(restored, crate::Keymap::bundled(platform), "{platform:?}");
        // The header teaches the format, and the defaults are there to copy.
        assert!(template.contains("Bind a key to null"), "{platform:?}");
        let clear = template
            .lines()
            .find(|line| line.contains("\"terminal::Clear\""))
            .unwrap_or_else(|| panic!("{platform:?}: no terminal::Clear binding"));
        assert!(clear.trim_start().starts_with("// \""), "{clear}");
    }
    // The header does not repeat the bundled file's own.
    assert!(!keymap_template(Platform::MacOs).contains("default key bindings for macOS"));
}

#[test]
fn ensure_keymap_file_writes_a_commented_template_once() {
    let root = TempDir::new("ensure-keymap");
    let paths = ConfigPaths::new(root.path().join("nested").join("config"));

    assert!(paths.ensure_keymap_file().unwrap());
    let text = std::fs::read_to_string(&paths.keymap).unwrap();
    assert_eq!(text, keymap_template(Platform::current()));
    // Loading it adds nothing to the bundled defaults.
    let loaded = crate::load_keymap(Some(&paths.keymap)).unwrap();
    assert_eq!(loaded, crate::Keymap::bundled_default());

    // A second call leaves the user's file alone.
    std::fs::write(&paths.keymap, "[]").unwrap();
    assert!(!paths.ensure_keymap_file().unwrap());
    assert_eq!(std::fs::read_to_string(&paths.keymap).unwrap(), "[]");
    // And the settings file is separate.
    assert!(paths.ensure_settings_file().unwrap());
}

#[test]
fn emulation_comes_from_the_terminal_setting_then_the_profile() {
    let defaults = Settings::default();
    assert_eq!(defaults.terminal.emulation, Emulation::Monitor);
    assert!(
        !defaults.terminal.cursor_blink,
        "a steady cursor by default"
    );
    assert_eq!(defaults.emulation_for(&airoha_port()), Emulation::Monitor);

    let settings = user_settings(
        r#"{
            "terminal": { "emulation": "vt", "cursor_blink": true },
            "devices": [ { "match": { "product": "Airoha" }, "emulation": "monitor" } ]
        }"#,
    );
    assert!(settings.warnings.is_empty(), "{:?}", settings.warnings);
    assert!(settings.terminal.cursor_blink);
    assert_eq!(
        settings.emulation_for(&airoha_port()),
        Emulation::Monitor,
        "the profile wins"
    );
    let other = usb_port("/dev/cu.usbserial-1", 0x0403, 0x6001, None, None, None);
    assert_eq!(settings.emulation_for(&other), Emulation::Vt);

    error_of(r#"{ "terminal": { "emulation": "vt100" } }"#);
    assert_eq!(Emulation::Monitor.toggled(), Emulation::Vt);
    assert_eq!(Emulation::Vt.toggled().label(), "Monitor");
}
