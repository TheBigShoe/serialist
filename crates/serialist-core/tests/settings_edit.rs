//! Comment-preserving edits to `settings.json` and `keymap.json`.
//!
//! The byte-for-byte cases start from `tests/fixtures/settings_edit/input.crlf.jsonc`
//! (tabs, CRLF, comments before and after keys, trailing commas) and compare the saved
//! file with an expected fixture beside it. Fixtures named `*.crlf.*` are marked `-text`
//! in `.gitattributes` so git leaves their line endings alone.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};
use serialist_core::settings::{EditError, Platform, keymap_template, settings_template};
use serialist_core::{
    ConfigPaths, KeymapEditor, Settings, SettingsEditor, load_keymap, load_settings,
};

// ---- Helpers ----

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "serialist-settings-edit-{label}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        fs::write(&path, text).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/settings_edit")
        .join(name)
}

fn fixture(name: &str) -> String {
    String::from_utf8(fs::read(fixture_path(name)).expect("read fixture")).expect("utf-8 fixture")
}

/// Copies fixture `input` to a fresh `settings.json`, runs `edit`, saves, and returns the
/// bytes on disk as text (with `\r` and `\t` still in it).
fn settings_after(input: &str, edit: impl FnOnce(&mut SettingsEditor)) -> String {
    let dir = TempDir::new("fixture");
    let path = dir.file("settings.json");
    fs::copy(fixture_path(input), &path).expect("copy fixture");
    let mut editor = SettingsEditor::open(&path).expect("open");
    edit(&mut editor);
    editor.save().expect("save");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name())
        .collect();
    assert_eq!(leftovers.len(), 1, "no temp file is left: {leftovers:?}");
    String::from_utf8(fs::read(&path).unwrap()).unwrap()
}

fn keymap_after(edit: impl FnOnce(&mut KeymapEditor)) -> String {
    let dir = TempDir::new("keymap-fixture");
    let path = dir.file("keymap.json");
    fs::copy(fixture_path("keymap_input.jsonc"), &path).expect("copy fixture");
    let mut editor = KeymapEditor::open(&path).expect("open");
    edit(&mut editor);
    editor.save().expect("save");
    String::from_utf8(fs::read(&path).unwrap()).unwrap()
}

/// An editor over `text` in a temp dir; the dir lives as long as the first value.
fn editor_over(text: &str) -> (TempDir, SettingsEditor) {
    let dir = TempDir::new("memory");
    let path = dir.write("settings.json", text);
    let editor = SettingsEditor::open(&path).expect("open");
    (dir, editor)
}

fn changed_lines<'a>(before: &'a str, after: &'a str) -> Vec<(&'a str, &'a str)> {
    assert_eq!(
        before.lines().count(),
        after.lines().count(),
        "the line count is the same"
    );
    before
        .lines()
        .zip(after.lines())
        .filter(|(a, b)| a != b)
        .collect()
}

// ---- Byte-for-byte cases on the fixture ----

#[test]
fn settings_edit_sets_an_existing_top_level_key() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor.set("/buffer_font_size", 18).unwrap();
    });
    assert_eq!(out, fixture("set_existing.crlf.jsonc"));
    assert!(out.contains("// points\r\n"), "the trailing comment stays");
}

#[test]
fn settings_edit_sets_a_key_inside_an_existing_object() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor.set("/theme/mode", "dark").unwrap();
    });
    assert_eq!(out, fixture("set_nested_existing.crlf.jsonc"));
}

#[test]
fn settings_edit_sets_a_nested_key_that_does_not_exist() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor.set("/display/timestamps", "absolute").unwrap();
    });
    assert_eq!(out, fixture("set_nested_missing.crlf.jsonc"));
}

#[test]
fn settings_edit_sets_a_key_in_an_empty_file() {
    let out = settings_after("empty.jsonc", |editor| {
        editor.set("/buffer_font_size", 18).unwrap();
    });
    assert_eq!(out, fixture("set_in_empty.jsonc"));
}

#[test]
fn settings_edit_sets_inside_the_first_device() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor.set("/devices/0/baud", 115_200).unwrap();
        editor.set("/devices/0/name", "Headset").unwrap();
    });
    assert_eq!(out, fixture("set_device_field.crlf.jsonc"));
}

#[test]
fn settings_edit_appends_to_devices() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor
            .set(
                "/devices/-",
                json!({ "baud": 9600, "match": { "vid": "0x1234" }, "name": "Second" }),
            )
            .unwrap();
    });
    assert_eq!(out, fixture("append_device.crlf.jsonc"));
}

#[test]
fn settings_edit_removes_keys() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        assert!(editor.remove("/buffer_font_weight").unwrap());
        assert!(editor.remove("/theme/dark").unwrap());
        assert!(!editor.remove("/not_there").unwrap());
        assert!(!editor.remove("/theme/not/there").unwrap());
    });
    assert_eq!(out, fixture("remove_key.crlf.jsonc"));
}

#[test]
fn settings_edit_only_ever_writes_the_line_endings_the_file_had() {
    let out = settings_after("input.crlf.jsonc", |editor| {
        editor.set("/display/view", "hex").unwrap();
        editor
            .set("/devices/-", json!({ "match": { "path": "/dev/ttyUSB0" } }))
            .unwrap();
    });
    assert!(!out.replace("\r\n", "").contains('\n'), "no bare LF");
    assert!(!out.replace("\r\n", "").contains('\r'), "no bare CR");
    assert!(!out.contains("\n  "), "no space indentation in a tab file");
}

// ---- Arrays with comments between their elements ----
//
// `devices_commented.jsonc` has three profiles with comments above them, after their
// commas, a block comment, blank lines and a comment left after the last one. Adding,
// removing and moving a profile touches that profile only.

/// The comment on each line of `text` that has one, trimmed (one comment per line).
fn comments_in(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let at = line.find("/*").or_else(|| line.find("//"))?;
            Some(line[at..].trim().to_owned())
        })
        .collect()
}

/// The comments of `devices_commented.jsonc` by the profile they are written with, and
/// the ones about the array as a whole: what an add, a removal or a move of another
/// profile must never lose.
const EARBUDS_COMMENTS: [&str; 2] = ["// Airoha earbuds on the bench", "// fast link"];
const DONGLE_COMMENTS: [&str; 2] = [
    "/* The dongle: leave this one on 9600 */",
    "// kept for the lab",
];
const FALLBACK_COMMENTS: [&str; 2] = ["// Anything else", "// last resort"];
const ARRAY_COMMENTS: [&str; 5] = [
    "// Serialist settings for the bench",
    "// points",
    "// Profiles are tried in order; the first match wins.",
    "// new boards go above the fallback",
    "// the end of devices",
];

fn device_names(text: &str) -> Vec<String> {
    let (_dir, editor) = editor_over(text);
    editor
        .get("/devices")
        .and_then(|devices| devices.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .map(|device| device["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn assert_all_present(text: &str, comments: &[&str]) {
    let found = comments_in(text);
    for comment in comments {
        assert!(
            found.iter().any(|c| c == comment),
            "lost `{comment}` in:\n{text}"
        );
    }
}

#[test]
fn settings_edit_adding_a_profile_keeps_every_comment() {
    let input = fixture("devices_commented.jsonc");
    let out = settings_after("devices_commented.jsonc", |editor| {
        editor
            .set(
                "/devices/-",
                json!({ "name": "Bench board", "match": { "vid": "0x1234" }, "baud": 57600 }),
            )
            .unwrap();
    });
    assert_eq!(out, fixture("devices_commented_add.jsonc"));
    assert_eq!(
        comments_in(&out),
        comments_in(&input),
        "same comments, same order"
    );
    assert_eq!(
        device_names(&out),
        ["Earbuds", "Dongle", "Fallback", "Bench board"]
    );
}

#[test]
fn settings_edit_removing_a_profile_keeps_the_comments_of_the_others() {
    let out = settings_after("devices_commented.jsonc", |editor| {
        assert!(editor.remove("/devices/1").unwrap());
    });
    assert_eq!(out, fixture("devices_commented_remove_middle.jsonc"));
    for kept in [
        &EARBUDS_COMMENTS[..],
        &FALLBACK_COMMENTS[..],
        &ARRAY_COMMENTS[..],
    ] {
        assert_all_present(&out, kept);
    }
    assert_eq!(device_names(&out), ["Earbuds", "Fallback"]);

    // The comment after a removed profile's comma goes with it; the ones above it stay,
    // as they do for a removed key.
    let out = settings_after("devices_commented.jsonc", |editor| {
        assert!(editor.remove("/devices/0").unwrap());
    });
    assert!(!out.contains("// fast link"), "{out}");
    assert_all_present(&out, &["// Airoha earbuds on the bench"]);
    for kept in [
        &DONGLE_COMMENTS[..],
        &FALLBACK_COMMENTS[..],
        &ARRAY_COMMENTS[..],
    ] {
        assert_all_present(&out, kept);
    }
    let out = settings_after("devices_commented.jsonc", |editor| {
        assert!(editor.remove("/devices/2").unwrap());
    });
    assert!(!out.contains("// last resort"), "{out}");
    for kept in [
        &EARBUDS_COMMENTS[..],
        &DONGLE_COMMENTS[..],
        &ARRAY_COMMENTS[..],
    ] {
        assert_all_present(&out, kept);
    }
    assert_all_present(&out, &["// Anything else"]);
}

#[test]
fn settings_edit_moving_a_profile_takes_its_comments_along() {
    let input = fixture("devices_commented.jsonc");

    let out = settings_after("devices_commented.jsonc", |editor| {
        assert!(editor.move_element("/devices/0", 2).unwrap());
    });
    assert_eq!(out, fixture("devices_commented_move_to_end.jsonc"));
    assert_eq!(device_names(&out), ["Dongle", "Fallback", "Earbuds"]);

    let out_front = settings_after("devices_commented.jsonc", |editor| {
        assert!(editor.move_element("/devices/2", 0).unwrap());
    });
    assert_eq!(out_front, fixture("devices_commented_move_to_front.jsonc"));
    assert_eq!(device_names(&out_front), ["Fallback", "Earbuds", "Dongle"]);

    for moved in [&out, &out_front] {
        let mut before = comments_in(&input);
        let mut after = comments_in(moved);
        before.sort();
        after.sort();
        assert_eq!(after, before, "no comment is lost or added");
    }
}

#[test]
fn settings_edit_every_move_of_a_commented_profile_matches_vec_semantics() {
    let input = fixture("devices_commented.jsonc");
    let mut expected_comments = comments_in(&input);
    expected_comments.sort();
    let names = device_names(&input);
    for from in 0..3 {
        for to in 0..3 {
            let (_dir, mut editor) = editor_over(&input);
            let moved = editor
                .move_element(&format!("/devices/{from}"), to)
                .unwrap();
            assert_eq!(moved, from != to, "{from} -> {to}");
            let mut want = names.clone();
            let name = want.remove(from);
            want.insert(to, name);
            assert_eq!(device_names(editor.text()), want, "{from} -> {to}");
            let mut found = comments_in(editor.text());
            found.sort();
            assert_eq!(found, expected_comments, "{from} -> {to}");
            if from == to {
                assert_eq!(editor.text(), input);
            }
        }
    }
}

#[test]
fn settings_edit_a_moved_profile_keeps_its_comments_around_it() {
    // Earbuds to the end: its two comments sit with it, the dongle's stay with the dongle.
    let (_dir, mut editor) = editor_over(&fixture("devices_commented.jsonc"));
    editor.move_element("/devices/0", 2).unwrap();
    let lines: Vec<&str> = editor.text().lines().map(str::trim).collect();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no `{needle}` in:\n{}", editor.text()))
    };
    assert_eq!(
        at("// Airoha earbuds on the bench") + 1,
        at("\"name\": \"Earbuds\"")
    );
    assert!(lines[at("\"name\": \"Earbuds\"")].ends_with("// fast link"));
    assert_eq!(at("// kept for the lab") + 1, at("\"name\": \"Dongle\""));
    assert!(at("\"name\": \"Dongle\"") < at("// Anything else"));
    assert!(at("// Anything else") < at("// Airoha earbuds on the bench"));
}

#[test]
fn settings_edit_moves_in_an_inline_array_and_keeps_the_file_style() {
    let (_dir, mut editor) = editor_over(
        "{ \"devices\": [ { \"baud\": 1 }, { \"baud\": 2 }, { \"baud\": 3 } ] // inline\n}\n",
    );
    assert!(editor.move_element("/devices/0", 2).unwrap());
    assert_eq!(
        editor.text(),
        "{ \"devices\": [ { \"baud\": 2 }, { \"baud\": 3 }, { \"baud\": 1 } ] // inline\n}\n"
    );
    assert!(editor.move_element("/devices/2", 1).unwrap());
    assert_eq!(
        editor.get("/devices"),
        Some(json!([{ "baud": 2 }, { "baud": 1 }, { "baud": 3 }]))
    );

    // CRLF and tabs are the file's own.
    let crlf = fixture("devices_commented.jsonc").replace('\n', "\r\n");
    let (_dir, mut editor) = editor_over(&crlf);
    editor.move_element("/devices/1", 0).unwrap();
    let text = editor.text().replace("\r\n", "");
    assert!(
        !text.contains('\n') && !text.contains('\r'),
        "no bare line ending"
    );
    assert_eq!(
        device_names(editor.text()),
        ["Dongle", "Earbuds", "Fallback"]
    );
}

#[test]
fn settings_edit_moving_a_profile_refuses_what_is_not_one() {
    let (_dir, mut editor) = editor_over(&fixture("devices_commented.jsonc"));
    let before = editor.text().to_owned();
    for (pointer, to) in [
        ("/devices/3", 0),
        ("/devices/0", 3),
        ("/devices/-", 0),
        ("/devices/01", 0),
        ("/devices", 0),
        ("/display/wrap", 0),
        ("/not_there/0", 0),
        ("", 0),
        ("devices/0", 0),
    ] {
        let err = editor.move_element(pointer, to).unwrap_err();
        assert!(
            matches!(err, EditError::Pointer { .. }),
            "{pointer} -> {to}: {err:?}"
        );
        assert_eq!(editor.text(), before, "{pointer} -> {to}");
    }
    assert!(!editor.is_dirty());
    assert!(
        !editor.move_element("/devices/1", 1).unwrap(),
        "already there"
    );
    assert!(!editor.is_dirty());
}

// ---- Smaller behaviours ----

#[test]
fn settings_edit_setting_the_current_value_changes_nothing() {
    let (_dir, mut editor) = editor_over("{ \"size\": 15.0, \"name\": \"a\" } // note\n");
    let before = editor.text().to_owned();
    editor.set("/size", 15).unwrap();
    editor.set("/name", "a").unwrap();
    assert_eq!(editor.text(), before);
    assert!(!editor.is_dirty());
    assert!(editor.diff_summary().is_empty());
}

#[test]
fn settings_edit_get_reads_the_file_not_the_defaults() {
    let (_dir, editor) = editor_over(
        "{\n  \"buffer_font_size\": 20,\n  \"devices\": [ { \"baud\": 9600 } ] // x\n}\n",
    );
    assert_eq!(editor.get("/buffer_font_size"), Some(json!(20)));
    assert_eq!(editor.get("/devices/0/baud"), Some(json!(9600)));
    assert_eq!(
        editor.get("/display/timestamps"),
        None,
        "a default, not in the file"
    );
    assert_eq!(editor.get("/devices/3"), None);
    assert_eq!(editor.get("/devices").unwrap().as_array().unwrap().len(), 1);
}

#[test]
fn settings_edit_replaces_an_element_by_index_and_appends_at_the_length() {
    let (_dir, mut editor) = editor_over("{\n  \"devices\": [\n    { \"baud\": 1 }\n  ]\n}\n");
    editor.set("/devices/0", json!({ "baud": 2 })).unwrap();
    assert_eq!(editor.get("/devices"), Some(json!([{ "baud": 2 }])));
    editor.set("/devices/1", json!({ "baud": 3 })).unwrap();
    editor.set("/devices/-", json!({ "baud": 4 })).unwrap();
    assert_eq!(
        editor.get("/devices"),
        Some(json!([{ "baud": 2 }, { "baud": 3 }, { "baud": 4 }]))
    );
}

#[test]
fn settings_edit_creates_arrays_for_zero_and_dash_and_objects_otherwise() {
    let (_dir, mut editor) = editor_over("{}");
    editor.set("/devices/0/baud", 9600).unwrap();
    editor.set("/profiles/-", "a").unwrap();
    editor.set("/a/b/c", true).unwrap();
    assert_eq!(
        editor.get(""),
        Some(json!({
            "devices": [{ "baud": 9600 }],
            "profiles": ["a"],
            "a": { "b": { "c": true } },
        }))
    );
}

#[test]
fn settings_edit_a_null_on_the_way_gives_way_but_other_values_do_not() {
    let (_dir, mut editor) =
        editor_over("{\n  \"terminal\": null, // unset\n  \"theme\": \"Fadetouched Blur\"\n}\n");
    editor.set("/terminal/font_size", 12).unwrap();
    assert_eq!(editor.get("/terminal"), Some(json!({ "font_size": 12 })));
    assert!(editor.text().contains("// unset"));

    let before = editor.text().to_owned();
    let err = editor.set("/theme/mode", "dark").unwrap_err();
    assert!(matches!(err, EditError::Pointer { .. }), "{err:?}");
    assert!(err.to_string().contains("`/theme` holds a string"), "{err}");
    assert_eq!(
        editor.text(),
        before,
        "a refused edit leaves the text alone"
    );
}

#[test]
fn settings_edit_refuses_bad_pointers_and_leaves_the_text_alone() {
    let (_dir, mut editor) = editor_over("{\n  \"devices\": [ { \"baud\": 1 } ]\n}\n");
    let before = editor.text().to_owned();
    for pointer in [
        "devices",
        "",
        "/devices/5",
        "/devices/x/baud",
        "/devices/01",
    ] {
        let err = editor.set(pointer, 1).unwrap_err();
        assert!(
            matches!(err, EditError::Pointer { .. }),
            "{pointer}: {err:?}"
        );
        assert_eq!(editor.text(), before, "{pointer}");
    }
    assert!(editor.remove("").is_err());
    assert!(editor.remove("devices").is_err());
}

#[test]
fn settings_edit_escaped_keys() {
    let (_dir, mut editor) = editor_over("{}");
    editor.set("/a~1b/c~0d", 1).unwrap();
    assert!(editor.text().contains("\"a/b\""), "{}", editor.text());
    assert!(editor.text().contains("\"c~d\""), "{}", editor.text());
    assert_eq!(editor.get("/a~1b/c~0d"), Some(json!(1)));
    assert!(editor.remove("/a~1b/c~0d").unwrap());
}

#[test]
fn settings_edit_keeps_a_comment_only_file_and_adds_the_object_after_it() {
    let (_dir, mut editor) = editor_over("// nothing yet\n");
    editor.set("/default_baud", 9600).unwrap();
    assert_eq!(
        editor.text(),
        "// nothing yet\n{\n  \"default_baud\": 9600\n}\n"
    );
}

#[test]
fn settings_edit_follows_the_trailing_comma_style_of_the_file() {
    let (_dir, mut with) = editor_over("{\n  \"a\": 1,\n  \"b\": 2,\n}\n");
    with.set("/c", 3).unwrap();
    assert_eq!(with.text(), "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n}\n");

    let (_dir, mut without) = editor_over("{\n  \"a\": 1,\n  \"b\": 2\n}\n");
    without.set("/c", 3).unwrap();
    assert_eq!(
        without.text(),
        "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n"
    );
}

#[test]
fn settings_edit_remove_and_prune_takes_emptied_parents_but_not_commented_ones() {
    let (_dir, mut editor) = editor_over(
        "{\n  \"keep\": 1,\n  \"a\": { \"b\": { \"c\": 1 } },\n  \"x\": {\n    // why\n    \"y\": 1\n  }\n}\n",
    );
    assert!(editor.remove_and_prune("/a/b/c").unwrap());
    assert_eq!(editor.get("/a"), None, "both emptied levels went");
    assert!(editor.remove_and_prune("/x/y").unwrap());
    assert_eq!(
        editor.get("/x"),
        Some(json!({})),
        "the commented object stays"
    );
    assert!(editor.text().contains("// why"));
    assert_eq!(editor.get("/keep"), Some(json!(1)));

    // The plain removal never prunes.
    let (_dir, mut plain) = editor_over("{ \"a\": { \"b\": 1 } }");
    assert!(plain.remove("/a/b").unwrap());
    assert_eq!(plain.get("/a"), Some(json!({})));
}

#[test]
fn settings_edit_diff_summary_lists_what_changed_since_open() {
    let (_dir, mut editor) =
        editor_over("{\n  \"size\": 15,\n  \"display\": { \"wrap\": false },\n  \"old\": 1\n}\n");
    editor.set("/size", 18).unwrap();
    editor.set("/display/view", "hex").unwrap();
    editor.set("/devices/-", json!({ "baud": 1 })).unwrap();
    editor.remove("/old").unwrap();
    let summary: Vec<String> = editor
        .diff_summary()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        summary,
        [
            r#"/devices: (unset) -> [{"baud":1}]"#,
            r#"/display/view: (unset) -> "hex""#,
            "/old: 1 -> (removed)",
            "/size: 15 -> 18",
        ]
    );
    // Setting a key back is no change at all.
    editor.set("/size", 15).unwrap();
    assert!(
        editor
            .diff_summary()
            .iter()
            .all(|change| change.pointer != "/size")
    );
}

// ---- Files ----

#[test]
fn settings_edit_a_missing_file_is_created_from_the_template() {
    let dir = TempDir::new("missing");
    let path = dir.file("config/serialist/settings.json");
    let editor = SettingsEditor::open(&path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), settings_template());
    assert_eq!(editor.text(), settings_template());
    assert!(!editor.is_dirty());

    // The same text `ensure_settings_file` writes.
    let paths = ConfigPaths::new(dir.file("other"));
    paths.ensure_settings_file().unwrap();
    assert_eq!(
        fs::read_to_string(&paths.settings).unwrap(),
        settings_template()
    );
}

#[test]
fn settings_edit_saving_without_changes_does_not_touch_the_file() {
    let dir = TempDir::new("untouched");
    let path = dir.write("settings.json", "{ \"a\": 1 }");
    let mut editor = SettingsEditor::open(&path).unwrap();
    fs::remove_file(&path).unwrap();
    editor.save().unwrap();
    assert!(!path.exists(), "nothing was written");
}

#[test]
fn settings_edit_save_refuses_to_overwrite_a_file_that_changed_meanwhile() {
    let dir = TempDir::new("changed");
    let path = dir.write("settings.json", "{ \"a\": 1 }\n");
    let mut editor = SettingsEditor::open(&path).unwrap();
    editor.set("/b", 2).unwrap();
    fs::write(&path, "{ \"a\": 1, \"external\": true }\n").unwrap();
    let err = editor.save().unwrap_err();
    assert!(matches!(err, EditError::Changed { .. }), "{err:?}");
    assert_eq!(err.path(), path);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{ \"a\": 1, \"external\": true }\n",
        "the outside change is still there"
    );
}

#[test]
fn settings_edit_convenience_calls_open_edit_and_save() {
    let dir = TempDir::new("convenience");
    let path = dir.write("settings.json", "{\n  // keep me\n  \"a\": 1\n}\n");
    SettingsEditor::set_in_file(&path, "/b/c", "x").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\n  // keep me\n  \"a\": 1,\n  \"b\": {\n    \"c\": \"x\"\n  }\n}\n"
    );
    assert!(SettingsEditor::remove_in_file(&path, "/b").unwrap());
    assert!(!SettingsEditor::remove_in_file(&path, "/b").unwrap());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\n  // keep me\n  \"a\": 1\n}\n"
    );
}

#[cfg(unix)]
#[test]
fn settings_edit_writes_through_a_symlink_and_keeps_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = TempDir::new("symlink");
    let real = dir.write("real.json", "{ \"a\": 1 }\n");
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    let link = dir.file("settings.json");
    symlink(&real, &link).unwrap();
    SettingsEditor::set_in_file(&link, "/b", 2).unwrap();
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(&real).unwrap().contains("\"b\": 2"));
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

// ---- Files that do not parse ----

#[test]
fn settings_edit_a_file_that_does_not_parse_is_an_error_with_line_and_column() {
    let dir = TempDir::new("invalid");
    let text = "{\n  \"a\": 1,\n  \"b\": ,\n}\n";
    let path = dir.write("settings.json", text);
    let err = SettingsEditor::open(&path).unwrap_err();
    let EditError::Invalid {
        path: ref reported,
        line,
        column,
        ref message,
    } = err
    else {
        panic!("expected Invalid, got {err:?}");
    };
    assert_eq!(reported, &path);
    assert_eq!((line, column), (3, 8), "{message}");
    assert_eq!(err.position(), Some((3, 8)));
    assert!(
        err.to_string()
            .starts_with(&format!("{}:3:8: ", path.display())),
        "{err}"
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        text,
        "the file is left alone"
    );
}

#[test]
fn settings_edit_a_top_level_that_is_not_an_object_is_invalid() {
    let dir = TempDir::new("shape");
    let path = dir.write("settings.json", "// c\n[1, 2]\n");
    let err = SettingsEditor::open(&path).unwrap_err();
    assert_eq!(err.position(), Some((2, 1)), "{err}");
    assert!(err.to_string().contains("must be a JSON object"), "{err}");

    let keymap = dir.write("keymap.json", "{ \"a\": 1 }");
    let err = KeymapEditor::open(&keymap).unwrap_err();
    assert!(err.to_string().contains("must be a JSON array"), "{err}");
}

// ---- Round trips through the loader ----

fn resolved(path: &Path) -> Value {
    let settings: Settings = load_settings(Some(path), None).expect("settings load");
    serde_json::to_value(settings).unwrap()
}

/// Asserts that `after` is `before` with the value at `pointer` replaced, and returns
/// the value `after` has there.
fn only_changed(before: &Value, after: &Value, pointer: &str) -> Value {
    let now = after.pointer(pointer).cloned().expect("resolved value");
    let mut expected = before.clone();
    *expected.pointer_mut(pointer).expect("resolved before") = now.clone();
    assert_eq!(&expected, after, "only {pointer} differs");
    now
}

fn number(value: &Value) -> f64 {
    value.as_f64().expect("a number")
}

const SAMPLE: &str = r#"{
  // fonts
  "buffer_font_family": "Berkeley Mono",
  "buffer_font_size": 15,
  "theme": { "mode": "dark", "dark": "Fadetouched Blur" },
  "display": {
    "timestamps": "delta", // off | absolute | relative | delta
    "wrap": true,
  },
  "devices": [
    { "match": { "vid": "0x0e8d" }, "baud": 921600, "plugin": "airoha-race", },
  ],
}
"#;

#[test]
fn settings_edit_round_trip_changes_only_the_edited_value() {
    for (name, text) in [("sample", SAMPLE), ("template", &settings_template())] {
        let dir = TempDir::new(name);
        let path = dir.write("settings.json", text);
        let mut before = resolved(&path);
        if name == "template" {
            // A device to edit inside; the template has none.
            let device = json!({ "match": { "vid": "0x0e8d" }, "baud": 115_200 });
            SettingsEditor::set_in_file(&path, "/devices/0", device).unwrap();
            before = resolved(&path);
        }

        let edits: [(&str, Value, &str); 5] = [
            ("/buffer_font_size", json!(18), "/buffer_font_size"),
            (
                "/display/timestamps",
                json!("absolute"),
                "/display/timestamps",
            ),
            (
                "/display/hex_bytes_per_row",
                json!(32),
                "/display/hex_bytes_per_row",
            ),
            ("/default_baud", json!(9600), "/default_baud"),
            ("/devices/0/baud", json!(57_600), "/devices/0/baud"),
        ];
        for (pointer, value, resolved_pointer) in edits {
            let previous = resolved(&path);
            SettingsEditor::set_in_file(&path, pointer, value.clone()).unwrap();
            let after = resolved(&path);
            let now = only_changed(&previous, &after, resolved_pointer);
            if value.is_number() {
                assert_eq!(number(&now), number(&value), "{name} {pointer}");
            } else {
                assert_eq!(now, value, "{name} {pointer}");
            }
        }

        // Appending a device grows the resolved list and nothing else.
        let previous = resolved(&path);
        SettingsEditor::set_in_file(
            &path,
            "/devices/-",
            json!({ "match": { "product": "New" }, "baud": 1200, "name": "New" }),
        )
        .unwrap();
        let after = resolved(&path);
        let now = only_changed(&previous, &after, "/devices");
        assert_eq!(
            now.as_array().unwrap().len(),
            previous["devices"].as_array().unwrap().len() + 1
        );
        assert_eq!(now.as_array().unwrap().last().unwrap()["baud"], json!(1200));

        // Removing a key returns the resolved value to the default.
        SettingsEditor::remove_in_file(&path, "/default_baud").unwrap();
        assert_eq!(
            resolved(&path)["default_baud"],
            before["default_baud"],
            "{name}"
        );
    }
}

#[test]
fn settings_edit_keeps_the_sample_comments_through_edits() {
    let dir = TempDir::new("sample-comments");
    let path = dir.write("settings.json", SAMPLE);
    SettingsEditor::set_in_file(&path, "/display/timestamps", "absolute").unwrap();
    SettingsEditor::set_in_file(&path, "/display/view", "hex").unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("// fonts\n"));
    assert!(text.contains("\"timestamps\": \"absolute\", // off | absolute | relative | delta\n"));
    assert!(text.contains("\"plugin\": \"airoha-race\", },\n"));
    assert_eq!(
        load_settings(Some(&path), None).unwrap().display.timestamps,
        serialist_core::TimestampMode::Absolute
    );
}

// ---- The commented template ----

#[test]
fn settings_edit_template_uncomments_the_matching_line_in_place() {
    let dir = TempDir::new("template-line");
    let path = dir.file("settings.json");
    let mut editor = SettingsEditor::open(&path).unwrap();
    editor.set("/buffer_font_size", 18).unwrap();
    let template = settings_template();
    assert_eq!(
        changed_lines(&template, editor.text()),
        [(
            "  // \"buffer_font_size\": 15.0,",
            "  \"buffer_font_size\": 18,"
        )]
    );
    editor.save().unwrap();
    assert_eq!(
        load_settings(Some(&path), None).unwrap().buffer_font_size,
        18.0
    );
}

#[test]
fn settings_edit_template_uncomments_a_block_for_a_nested_key() {
    let (_dir, mut editor) = editor_over(&settings_template());
    editor.set("/display/timestamps", "absolute").unwrap();
    let template = settings_template();
    assert_eq!(
        changed_lines(&template, editor.text()),
        [
            ("  // \"display\": {", "  \"display\": {"),
            (
                "    // \"timestamps\": \"off\",",
                "    \"timestamps\": \"absolute\","
            ),
            ("  // },", "  },"),
        ]
    );
    // The block's other lines are still commented out and still there.
    assert!(
        editor
            .text()
            .contains("    // \"timestamp_format\": \"%H:%M:%S%.3f\",")
    );
    assert_eq!(
        editor.get("/display"),
        Some(json!({ "timestamps": "absolute" }))
    );

    // A second key in the same block uncomments its own line.
    editor.set("/display/wrap", true).unwrap();
    assert_eq!(
        editor.get("/display"),
        Some(json!({ "timestamps": "absolute", "wrap": true }))
    );
    assert!(editor.text().contains("    \"wrap\": true,\n"));
    assert!(!editor.text().contains("// \"wrap\""));
}

#[test]
fn settings_edit_template_uncommented_lines_get_the_commas_the_file_needs() {
    // The last template line has no comma, and the member before it is live and has none.
    let (_dir, mut editor) = editor_over(&settings_template());
    editor.set("/devices/-", json!({ "baud": 9600 })).unwrap();
    editor.set("/default_baud", 57_600).unwrap();
    editor.set("/buffer_font_size", 17).unwrap();
    let value = editor.get("").unwrap();
    assert_eq!(value["default_baud"], json!(57_600));
    assert_eq!(value["buffer_font_size"], json!(17));
    assert_eq!(value["devices"], json!([{ "baud": 9600 }]));
}

#[test]
fn settings_edit_template_adds_a_key_it_does_not_mention_at_the_top() {
    let (_dir, mut editor) = editor_over(&settings_template());
    editor.set("/my_own_key", json!({ "x": 1 })).unwrap();
    let template = settings_template();
    let text = editor.text();
    let top: Vec<&str> = text.lines().skip(5).take(6).collect();
    assert_eq!(
        top,
        [
            "{",
            "  \"my_own_key\": {",
            "    \"x\": 1",
            "  }",
            "  // ---- Buffer font: the terminal text ----",
            "",
        ]
    );
    // Every template line is still there, in order.
    let mut rest = text.lines();
    for line in template.lines() {
        assert!(rest.any(|l| l == line), "lost: {line}");
    }
}

#[test]
fn settings_edit_template_keeps_every_comment_through_a_run_of_edits() {
    let (_dir, mut editor) = editor_over(&settings_template());
    editor.set("/buffer_font_size", 18).unwrap();
    editor.set("/display/timestamps", "delta").unwrap();
    editor.set("/theme/mode", "dark").unwrap();
    editor
        .set(
            "/devices/-",
            json!({ "match": { "vid": "0x0e8d" }, "baud": 921_600 }),
        )
        .unwrap();
    editor.set("/inline/escape_chord", "ctrl-alt-x").unwrap();

    let comments = |text: &str| -> Vec<String> {
        text.lines()
            // The `// },` closing a commented block goes live with its block.
            .filter(|l| {
                let body = l.trim_start().strip_prefix("// ");
                body.is_some_and(|b| !b.starts_with('}') && !l.contains("\": "))
            })
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(comments(&settings_template()), comments(editor.text()));
    let settings = Settings::from_jsonc(editor.text()).unwrap();
    assert_eq!(
        settings.display.timestamps,
        serialist_core::TimestampMode::Delta
    );
    assert_eq!(settings.devices.len(), 1);
    assert_eq!(settings.inline.escape_chord, "ctrl-alt-x");
}

// ---- The keymap ----

#[test]
fn settings_keymap_edit_rebinds_a_key_where_it_is() {
    let out = keymap_after(|editor| {
        editor
            .bind("Workspace", "cmd-k", "terminal::ClearAll")
            .unwrap();
    });
    assert_eq!(out, fixture("keymap_bind_existing.jsonc"));
}

#[test]
fn settings_keymap_edit_adds_a_key_to_an_existing_context() {
    let out = keymap_after(|editor| {
        editor.bind("Workspace", "cmd-j", "tabs::NewTab").unwrap();
    });
    assert_eq!(out, fixture("keymap_bind_new_key.jsonc"));
}

#[test]
fn settings_keymap_edit_unbinds_with_null() {
    let out = keymap_after(|editor| {
        editor.unbind("Workspace", "cmd-k").unwrap();
        editor.unbind(None, "cmd-q").unwrap();
    });
    assert_eq!(out, fixture("keymap_unbind.jsonc"));
}

#[test]
fn settings_keymap_edit_removes_a_binding_and_keeps_the_section() {
    let out = keymap_after(|editor| {
        assert!(editor.remove("Workspace", "cmd-w").unwrap());
        assert!(editor.remove(None, "cmd-q").unwrap());
        assert!(!editor.remove("Workspace", "cmd-w").unwrap());
        assert!(!editor.remove("Nowhere", "cmd-w").unwrap());
    });
    assert_eq!(out, fixture("keymap_remove.jsonc"));
}

#[test]
fn settings_keymap_edit_appends_a_section_for_a_new_context() {
    let out = keymap_after(|editor| {
        editor
            .bind("Terminal", "ctrl-l", "terminal::Clear")
            .unwrap();
    });
    assert_eq!(out, fixture("keymap_new_context.jsonc"));
}

#[test]
fn settings_keymap_edit_reads_bindings_and_contexts() {
    let dir = TempDir::new("keymap-read");
    let path = dir.write(
        "keymap.json",
        r#"[
  { "context": "A", "bindings": { "x": "a::X", "y": null } },
  { "bindings": { "g": "global::G" } },
  { "context": "A", "bindings": { "x": ["a::X", { "n": 2 }], "z": "a::Z" } }
]"#,
    );
    let editor = KeymapEditor::open(&path).unwrap();
    assert_eq!(
        editor.contexts(),
        [Some("A".to_owned()), None],
        "each context once, in file order"
    );
    assert_eq!(
        editor.bindings_for("A"),
        [
            ("x".to_owned(), json!(["a::X", { "n": 2 }])),
            ("y".to_owned(), Value::Null),
            ("z".to_owned(), json!("a::Z")),
        ],
        "a later section replaces the earlier action in place"
    );
    assert_eq!(
        editor.bindings_for(None),
        [("g".to_owned(), json!("global::G"))]
    );
    assert!(editor.bindings_for("B").is_empty());
}

#[test]
fn settings_keymap_edit_new_keys_skip_sections_that_set_key_equivalents() {
    let dir = TempDir::new("keymap-equivalents");
    let path = dir.write(
        "keymap.json",
        "[\n  { \"context\": \"A\", \"use_key_equivalents\": true, \"bindings\": { \"x\": \"a::X\" } }\n]\n",
    );
    let mut editor = KeymapEditor::open(&path).unwrap();
    editor.bind("A", "y", "a::Y").unwrap();
    editor.bind("A", "x", "a::Other").unwrap();
    let text = editor.text().to_owned();
    assert!(
        text.contains("\"x\": \"a::Other\""),
        "an existing key stays in its section"
    );
    assert_eq!(
        text.matches("\"context\": \"A\"").count(),
        2,
        "the new key got a section"
    );
    assert_eq!(editor.bindings_for("A").len(), 2);
}

#[test]
fn settings_keymap_edit_template_keeps_its_comments_and_loads_with_the_new_binding() {
    let dir = TempDir::new("keymap-template");
    let path = dir.file("keys/keymap.json");
    let mut editor = KeymapEditor::open(&path).unwrap();
    let template = keymap_template(Platform::current());
    assert_eq!(fs::read_to_string(&path).unwrap(), template);

    editor
        .bind("Terminal", "ctrl-l", "terminal::Clear")
        .unwrap();
    editor.unbind(None, "cmd-q").unwrap();
    editor.save().unwrap();

    let text = fs::read_to_string(&path).unwrap();
    let mut rest = text.lines();
    for line in template.lines().filter(|l| *l != "]") {
        assert!(rest.any(|l| l == line), "lost: {line}");
    }
    let keymap = load_keymap(Some(&path)).unwrap();
    let last = keymap.entries.last().expect("entries");
    assert_eq!(last.keystrokes, "cmd-q");
    assert!(last.action.is_none(), "unbound");
    assert!(keymap.entries.iter().any(|entry| {
        entry.context.as_deref() == Some("Terminal")
            && entry.keystrokes == "ctrl-l"
            && entry
                .action
                .as_ref()
                .is_some_and(|a| a.name == "terminal::Clear")
    }));
}

#[test]
fn settings_keymap_edit_a_file_that_does_not_parse_is_an_error_with_line_and_column() {
    let dir = TempDir::new("keymap-invalid");
    let path = dir.write("keymap.json", "[\n  { \"bindings\": { \"a\": } }\n]\n");
    let err = KeymapEditor::open(&path).unwrap_err();
    assert_eq!(err.position(), Some((2, 24)), "{err}");
    assert_eq!(err.path(), path);
}
