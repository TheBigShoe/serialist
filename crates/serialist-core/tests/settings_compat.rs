//! Settings files across releases: a complete file written for 0.2.0 still loads as it
//! did, and a value this build cannot read (as a file written for a later release may
//! hold) costs only its own key.

use std::path::PathBuf;

use proptest::prelude::*;
use serde_json::{Map, Value, json};
use serialist_core::settings::{FontFeatures, LineHeight, ThemeMode, ThemeSelection, UsbId};
use serialist_core::{
    DataBits, DeviceProfile, DisplayView, Emulation, FlowControl, LineEnding, Parity, ReplayEnd,
    ReplaySpeed, Settings, StopBits, TimestampMode,
};

const V0_2_0: &str = include_str!("fixtures/settings_v0.2.0.jsonc");

fn load(text: &str) -> Settings {
    Settings::from_jsonc(text).unwrap_or_else(|err| panic!("settings failed to load: {err}"))
}

fn features(pairs: &[(&str, u32)]) -> FontFeatures {
    pairs
        .iter()
        .map(|(tag, value)| (tag.to_string(), *value))
        .collect()
}

#[test]
fn a_complete_0_2_0_settings_file_loads_with_every_value() {
    let s = load(V0_2_0);
    assert!(s.warnings.is_empty(), "{:?}", s.warnings);

    assert_eq!(s.buffer_font_family.as_deref(), Some("Berkeley Mono"));
    assert_eq!(s.buffer_font_size, 13.5);
    assert_eq!(s.buffer_font_weight, 500.0);
    assert_eq!(
        s.buffer_font_features,
        features(&[("calt", 0), ("ss01", 1), ("cv01", 7)])
    );
    assert_eq!(s.buffer_line_height, LineHeight::Custom(1.45));
    assert_eq!(
        s.buffer_font_fallbacks,
        ["Symbols Nerd Font Mono", "Noto Emoji"]
    );

    assert_eq!(s.ui_font_family.as_deref(), Some("Inter"));
    assert_eq!(s.ui_font_size, 14.0);
    assert_eq!(s.ui_font_weight, 600.0);
    assert_eq!(s.ui_font_features, features(&[("tnum", 1)]));
    assert_eq!(s.ui_font_fallbacks, ["Noto Sans"]);

    let t = &s.terminal;
    assert_eq!(t.font_family.as_deref(), Some("JetBrains Mono"));
    assert_eq!(t.font_size, Some(12.0));
    assert_eq!(t.font_weight, Some(300.0));
    assert_eq!(t.font_features, Some(features(&[("liga", 0)])));
    assert_eq!(t.font_fallbacks, Some(vec!["Menlo".to_string()]));
    assert_eq!(t.line_height, Some(LineHeight::Custom(1.5)));
    assert_eq!(t.emulation, Emulation::Vt);
    assert!(t.cursor_blink);

    assert_eq!(
        s.theme,
        ThemeSelection::Dynamic {
            mode: ThemeMode::Dark,
            light: "Serialist Paper".into(),
            dark: "Serialist Ember".into(),
        }
    );

    let d = &s.display;
    assert_eq!(d.timestamps, TimestampMode::Absolute);
    assert_eq!(d.timestamp_format, "%H:%M:%S");
    assert_eq!(d.view, DisplayView::HexAscii);
    assert_eq!(d.hex_bytes_per_row, 32);
    assert!(d.show_control_chars);
    assert!(d.wrap);
    assert!(!d.decoded_inline);
    assert!(!d.hide_framed_bytes);

    assert_eq!(s.scrollback_budget_bytes, 64 << 20);
    assert_eq!(s.default_baud, 921_600);
    assert_eq!(s.line_ending, LineEnding::Lf);
    assert!(s.local_echo);
    assert!(!s.restore_session);
    assert_eq!(s.replay.speed, ReplaySpeed::Times(4.0));
    assert_eq!(s.replay.end, ReplayEnd::Hold);

    assert_eq!(s.inline.backspace.name(), "bs");
    assert_eq!(s.inline.escape_chord, "ctrl-alt-x");
    assert_eq!(s.inline.paste_chunk_bytes, 16);
    assert_eq!(s.inline.paste_chunk_delay_ms, 25);

    let [airoha, uboot, any, com, ttys] = s.devices.as_slice() else {
        panic!("{:?}", s.devices);
    };
    assert_eq!(airoha.name.as_deref(), Some("Airoha headset"));
    let m = &airoha.r#match;
    assert_eq!(m.vid, Some(UsbId(0x0e8d)));
    assert_eq!(m.pid, Some(UsbId(0x0616)));
    assert_eq!(m.product.as_deref(), Some("Airoha"));
    assert_eq!(m.manufacturer.as_deref(), Some("Airoha Technology"));
    assert_eq!(m.serial_number.as_deref(), Some("AB12"));
    assert_eq!(m.path, None);
    assert_eq!(airoha.baud, Some(921_600));
    assert_eq!(airoha.data_bits, Some(DataBits::Seven));
    assert_eq!(airoha.parity, Some(Parity::Even));
    assert_eq!(airoha.stop_bits, Some(StopBits::Two));
    assert_eq!(airoha.flow_control, Some(FlowControl::Hardware));
    assert_eq!(airoha.plugin.as_deref(), Some("airoha-race"));
    assert_eq!(airoha.eol, Some(LineEnding::Cr));
    assert_eq!(airoha.on_connect, Some(PathBuf::from("airoha/init.lua")));
    assert_eq!(airoha.emulation, Some(Emulation::Monitor));

    assert_eq!(uboot.r#match.vid, Some(UsbId(1027)));
    assert_eq!(uboot.r#match.pid, Some(UsbId(24577)));
    assert_eq!(uboot.r#match.path.as_deref(), Some("/dev/cu.usbserial"));
    assert_eq!(uboot.data_bits, Some(DataBits::Five));
    assert_eq!(uboot.parity, Some(Parity::Odd));
    assert_eq!(uboot.stop_bits, Some(StopBits::One));
    assert_eq!(uboot.flow_control, Some(FlowControl::Software));
    assert_eq!(uboot.eol, Some(LineEnding::None));
    assert_eq!(uboot.emulation, Some(Emulation::Vt));

    assert_eq!(any.r#match, Default::default());
    assert_eq!(any.data_bits, Some(DataBits::Six));
    assert_eq!(any.stop_bits, Some(StopBits::One));
    assert_eq!(any.parity, Some(Parity::Mark));
    assert_eq!(any.flow_control, Some(FlowControl::Hardware));
    assert_eq!(any.eol, Some(LineEnding::Lf));

    assert_eq!(com.data_bits, Some(DataBits::Eight));
    assert_eq!(com.parity, Some(Parity::Space));
    assert_eq!(com.flow_control, Some(FlowControl::Software));
    assert_eq!(com.eol, Some(LineEnding::Crlf));

    assert_eq!(ttys.parity, Some(Parity::None));
    assert_eq!(ttys.flow_control, Some(FlowControl::None));
}

/// Every key of `default` (a serialized `Settings` or part of one) is in `file` and
/// loaded as something else. Free-form maps (`{}`) only need to be set.
fn check_every_key_is_set(default: &Value, file: &Value, loaded: &Value, path: &str) {
    match default {
        Value::Object(keys) if !keys.is_empty() => {
            for (key, default) in keys {
                let path = format!("{path}/{key}");
                let file = file
                    .get(key)
                    .unwrap_or_else(|| panic!("the fixture does not set {path}"));
                check_every_key_is_set(default, file, &loaded[key], &path);
            }
        }
        _ => assert_ne!(loaded, default, "{path} is left at its default"),
    }
}

#[test]
fn the_0_2_0_fixture_sets_every_key_away_from_its_default() {
    let file: Value = jsonc_value(V0_2_0);
    let loaded = serde_json::to_value(load(V0_2_0)).unwrap();
    let mut default = serde_json::to_value(Settings::default()).unwrap();
    // The device list is checked below, profile key by profile key.
    default.as_object_mut().unwrap().remove("devices");
    check_every_key_is_set(&default, &file, &loaded, "");

    // Every profile key, and every `match` key, is set by some profile.
    let profiles = file["devices"].as_array().unwrap();
    let sample = serde_json::to_value(DeviceProfile::default()).unwrap();
    for key in sample.as_object().unwrap().keys() {
        assert!(
            profiles.iter().any(|profile| profile.get(key).is_some()),
            "no profile sets {key}"
        );
    }
    for key in sample["match"].as_object().unwrap().keys() {
        assert!(
            profiles
                .iter()
                .any(|profile| profile["match"].get(key).is_some()),
            "no profile matches on {key}"
        );
    }
}

/// The fixture as plain JSON, read with the loader's own parser.
fn jsonc_value(text: &str) -> Value {
    jsonc_parser::parse_to_serde_value(text, &Default::default()).expect("the fixture parses")
}

/// One step into a settings document.
#[derive(Clone, Copy, Debug)]
enum Step {
    Key(&'static str),
    Index(usize),
}

use Step::{Index, Key};

/// A leaf the property test may set: its path, values this build reads and values it
/// does not.
struct Leaf {
    path: &'static [Step],
    good: Vec<Value>,
    bad: Vec<Value>,
}

fn leaf(path: &'static [Step], good: Vec<Value>, bad: Vec<Value>) -> Leaf {
    Leaf { path, good, bad }
}

fn leaves() -> Vec<Leaf> {
    vec![
        leaf(
            &[Key("buffer_font_family")],
            vec![json!("Mono"), json!(null)],
            vec![json!(5), json!(["a"])],
        ),
        leaf(
            &[Key("buffer_font_size")],
            vec![json!(12), json!(13.5)],
            vec![json!("big"), json!(0), json!(null), json!(1e300)],
        ),
        leaf(
            &[Key("buffer_font_weight")],
            vec![json!(300), json!(700)],
            vec![json!(950), json!("bold")],
        ),
        leaf(
            &[Key("buffer_line_height")],
            vec![json!("comfortable"), json!(1.4), json!({ "custom": 1.2 })],
            vec![json!("airy"), json!(0), json!({ "size": 1 }), json!(true)],
        ),
        leaf(
            &[Key("buffer_font_fallbacks")],
            vec![json!(["A"]), json!([])],
            vec![json!("A"), json!([1])],
        ),
        leaf(
            &[Key("buffer_font_features"), Key("calt")],
            vec![json!(false), json!(1)],
            vec![json!("on"), json!(-1)],
        ),
        leaf(
            &[Key("ui_font_size")],
            vec![json!(14)],
            vec![json!(0), json!(-3)],
        ),
        leaf(
            &[Key("ui_font_weight")],
            vec![json!(450)],
            vec![json!(1000)],
        ),
        leaf(
            &[Key("terminal"), Key("emulation")],
            vec![json!("vt"), json!("monitor")],
            vec![json!("graphics"), json!(3)],
        ),
        leaf(
            &[Key("terminal"), Key("cursor_blink")],
            vec![json!(true)],
            vec![json!("yes")],
        ),
        leaf(
            &[Key("terminal"), Key("font_size")],
            vec![json!(11), json!(null)],
            vec![json!(0), json!("x")],
        ),
        leaf(
            &[Key("terminal"), Key("line_height")],
            vec![json!({ "custom": 1.1 }), json!("standard"), json!(null)],
            vec![json!("airy")],
        ),
        leaf(
            &[Key("theme"), Key("mode")],
            vec![json!("dark"), json!("light")],
            vec![json!("dusk"), json!(1)],
        ),
        leaf(
            &[Key("theme"), Key("light")],
            vec![json!("Serialist Paper")],
            vec![json!(5)],
        ),
        leaf(
            &[Key("display"), Key("timestamps")],
            vec![json!("delta")],
            vec![json!("sometimes")],
        ),
        leaf(
            &[Key("display"), Key("view")],
            vec![json!("hex"), json!("hex_ascii")],
            vec![json!("binary")],
        ),
        leaf(
            &[Key("display"), Key("hex_bytes_per_row")],
            vec![json!(8)],
            vec![json!(0), json!(999), json!("8")],
        ),
        leaf(
            &[Key("display"), Key("wrap")],
            vec![json!(true)],
            vec![json!("no"), json!(null)],
        ),
        leaf(
            &[Key("display"), Key("timestamp_format")],
            vec![json!("%H")],
            vec![json!(5)],
        ),
        leaf(
            &[Key("scrollback_budget_bytes")],
            vec![json!(1_048_576)],
            vec![json!(-1), json!("lots")],
        ),
        leaf(
            &[Key("default_baud")],
            vec![json!(9600)],
            vec![json!(0), json!("fast")],
        ),
        leaf(
            &[Key("line_ending")],
            vec![json!("lf"), json!("none")],
            vec![json!("crcrlf"), json!(1)],
        ),
        leaf(&[Key("local_echo")], vec![json!(true)], vec![json!("yes")]),
        leaf(
            &[Key("restore_session")],
            vec![json!(false)],
            vec![json!(0)],
        ),
        leaf(
            &[Key("replay"), Key("speed")],
            vec![json!("4x"), json!("max"), json!(2)],
            vec![json!("warp"), json!(0)],
        ),
        leaf(
            &[Key("replay"), Key("end")],
            vec![json!("hold")],
            vec![json!("loop")],
        ),
        leaf(
            &[Key("inline"), Key("backspace")],
            vec![json!("bs")],
            vec![json!("0x20")],
        ),
        leaf(
            &[Key("inline"), Key("escape_chord")],
            vec![json!("ctrl-b")],
            vec![json!("a")],
        ),
        leaf(
            &[Key("inline"), Key("paste_chunk_bytes")],
            vec![json!(8)],
            vec![json!(0)],
        ),
        leaf(
            &[Key("inline"), Key("paste_chunk_delay_ms")],
            vec![json!(0)],
            vec![json!(10_001)],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("name")],
            vec![json!("board")],
            vec![json!(5)],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("baud")],
            vec![json!(9600)],
            vec![json!(0)],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("parity")],
            vec![json!("even"), json!("n")],
            vec![json!("sometimes")],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("data_bits")],
            vec![json!(7), json!("eight")],
            vec![json!(9)],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("eol")],
            vec![json!("cr")],
            vec![json!("lfcr")],
        ),
        leaf(
            &[Key("devices"), Index(0), Key("emulation")],
            vec![json!("vt")],
            vec![json!("graphics")],
        ),
    ]
}

/// Puts `value` at `path` in `doc`, making the objects and arrays on the way.
fn set(doc: &mut Value, path: &[Step], value: Value) {
    let (last, parents) = path.split_last().expect("a path");
    let mut node = doc;
    for step in parents {
        node = match *step {
            Key(key) => node
                .as_object_mut()
                .expect("an object")
                .entry(key)
                .or_insert_with(|| Value::Object(Map::new())),
            Index(index) => {
                if !node.is_array() {
                    *node = json!([]);
                }
                let items = node.as_array_mut().unwrap();
                while items.len() <= index {
                    items.push(Value::Object(Map::new()));
                }
                &mut items[index]
            }
        };
    }
    match *last {
        Key(key) => {
            node.as_object_mut()
                .expect("an object")
                .insert(key.to_string(), value);
        }
        Index(_) => unreachable!("no leaf ends in an index"),
    }
}

/// The warning's spelling of a path: `devices[0].baud`.
fn dotted(path: &[Step]) -> String {
    let mut out = String::new();
    for step in path {
        match step {
            Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

/// A document of the leaves in `chosen` (index into `table`, value), with the device
/// profile's `match` when `with_profile`.
fn document(table: &[Leaf], chosen: &[(usize, Value)], with_profile: bool) -> String {
    let mut doc = json!({});
    if with_profile {
        set(
            &mut doc,
            &[Key("devices"), Index(0), Key("match")],
            json!({ "path": "/dev/a" }),
        );
    }
    for (index, value) in chosen {
        set(&mut doc, table[*index].path, value.clone());
    }
    serde_json::to_string_pretty(&doc).unwrap()
}

fn is_device(leaf: &Leaf) -> bool {
    matches!(leaf.path.first(), Some(Key("devices")))
}

proptest! {
    #[test]
    fn one_bad_leaf_costs_only_that_leaf(
        picks in prop::collection::vec(any::<Option<usize>>(), 36),
        corrupt in any::<usize>(),
        bad in any::<usize>(),
    ) {
        let table = leaves();
        prop_assert_eq!(picks.len(), table.len());
        let corrupt = corrupt % table.len();
        // The valid document: each picked leaf with one of its good values, and the
        // leaf to corrupt always in it.
        let mut chosen: Vec<(usize, Value)> = picks
            .iter()
            .enumerate()
            .filter_map(|(index, pick)| {
                let pick = pick.or((index == corrupt).then_some(0))?;
                let good = &table[index].good;
                Some((index, good[pick % good.len()].clone()))
            })
            .collect();
        let with_profile = chosen.iter().any(|(index, _)| is_device(&table[*index]));

        let valid = load(&document(&table, &chosen, with_profile));
        prop_assert!(valid.warnings.is_empty(), "{:?}", valid.warnings);

        let without: Vec<(usize, Value)> = chosen
            .iter()
            .filter(|(index, _)| *index != corrupt)
            .cloned()
            .collect();
        let mut expected = load(&document(&table, &without, with_profile));
        prop_assert!(expected.warnings.is_empty(), "{:?}", expected.warnings);

        let bad_values = &table[corrupt].bad;
        let bad_value = bad_values[bad % bad_values.len()].clone();
        for (index, value) in &mut chosen {
            if *index == corrupt {
                *value = bad_value.clone();
            }
        }
        let text = document(&table, &chosen, with_profile);
        let mut corrupted = load(&text);
        prop_assert_eq!(corrupted.warnings.len(), 1, "{}: {:?}", text, corrupted.warnings);
        let warning = corrupted.warnings.remove(0);
        prop_assert!(warning.is_invalid_value(), "{}", warning);
        prop_assert_eq!(&warning.key, &dotted(table[corrupt].path));
        prop_assert!(warning.line >= 1 && warning.column >= 1);
        expected.warnings.clear();
        prop_assert_eq!(corrupted, expected, "{}", text);
    }
}
