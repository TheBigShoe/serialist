use std::fs;

use super::*;
use crate::test_util::TempDir;

fn entries(history: &History) -> Vec<&str> {
    history.iter_newest_first().collect()
}

#[test]
fn entries_come_back_newest_first() {
    let mut history = History::in_memory();
    assert!(history.is_empty());
    for entry in ["AT", "ATI", "AT+VER?"] {
        assert!(history.push(entry));
    }
    assert_eq!(entries(&history), ["AT+VER?", "ATI", "AT"]);
    assert_eq!(history.len(), 3);
    assert_eq!(history.get_from_newest(0), Some("AT+VER?"));
    assert_eq!(history.get_from_newest(2), Some("AT"));
    assert_eq!(history.get_from_newest(3), None);
    assert_eq!(History::in_memory().get_from_newest(0), None);
    // It can be walked from the far end too.
    assert_eq!(
        history.iter_newest_first().rev().collect::<Vec<_>>(),
        ["AT", "ATI", "AT+VER?"]
    );
}

#[test]
fn consecutive_repeats_are_dropped_but_later_ones_are_not() {
    let mut history = History::in_memory();
    assert!(history.push("AT"));
    assert!(!history.push("AT"), "a repeat of the newest");
    assert!(!history.push(String::from("AT")));
    assert!(history.push("ATI"));
    assert!(history.push("AT"), "not a repeat of the newest any more");
    assert_eq!(entries(&history), ["AT", "ATI", "AT"]);
    // Whitespace makes an entry different.
    assert!(history.push("AT "));
}

#[test]
fn an_empty_entry_is_ignored() {
    let mut history = History::in_memory();
    assert!(!history.push(""));
    assert!(history.is_empty());
    assert!(history.push(" "), "a space is something");
}

#[test]
fn only_the_newest_thousand_are_kept() {
    let mut history = History::in_memory();
    for n in 0..MAX_ENTRIES + 5 {
        history.push(format!("cmd {n}"));
    }
    assert_eq!(history.len(), MAX_ENTRIES);
    assert_eq!(history.get_from_newest(0), Some("cmd 1004"));
    assert_eq!(history.get_from_newest(MAX_ENTRIES - 1), Some("cmd 5"));
    // A repeat of the newest does not push anything out.
    history.push("cmd 1004");
    assert_eq!(history.len(), MAX_ENTRIES);
    assert_eq!(history.get_from_newest(MAX_ENTRIES - 1), Some("cmd 5"));
}

#[test]
fn saved_history_loads_back_exactly() {
    let root = TempDir::new("hist-roundtrip");
    let path = root.path().join("config").join("history.jsonl");
    let long = "long ".repeat(10_000);
    let tricky = [
        "AT+VER?",
        "line one\nline two",
        "cr\rlf\r\ncrlf",
        "tab\there",
        "quote \" backslash \\ slash /",
        "\\x41 is text here, not an escape",
        "caf\u{e9} \u{1F600} \u{2028} \u{2029} \u{85}",
        "nul \u{0} bell \u{7} esc \u{1b}[31m del \u{7f}",
        "  leading and trailing  ",
        "{\"json\": [1, 2]}",
        long.as_str(),
    ];
    let mut history = History::load(&path);
    for entry in tricky {
        history.push(entry);
    }
    history.save().unwrap();

    // One line per entry, however odd the entry.
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), tricky.len());
    assert!(text.ends_with('\n'));

    let loaded = History::load(&path);
    assert!(loaded.warnings().is_empty(), "{:?}", loaded.warnings());
    assert_eq!(
        entries(&loaded),
        tricky.iter().rev().copied().collect::<Vec<_>>()
    );
    assert_eq!(loaded.path(), Some(path.as_path()));
}

#[test]
fn the_file_holds_one_json_string_per_line_oldest_first() {
    let root = TempDir::new("hist-format");
    let path = root.path().join("history.jsonl");
    let mut history = History::load(&path);
    history.push("AT+VER?");
    history.push("a\nb");
    history.push("q\"");
    history.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\"AT+VER?\"\n\"a\\nb\"\n\"q\\\"\"\n"
    );
}

#[test]
fn a_missing_file_is_an_empty_history() {
    let root = TempDir::new("hist-missing");
    let path = root.path().join("nowhere").join("history.jsonl");
    let history = History::load(&path);
    assert!(history.is_empty());
    assert!(history.warnings().is_empty());
    // Saving creates the directory.
    let mut history = history;
    history.push("AT");
    history.save().unwrap();
    assert_eq!(History::load(&path).len(), 1);
}

#[test]
fn a_corrupt_line_is_skipped_with_a_warning() {
    let root = TempDir::new("hist-corrupt");
    let path = root.write(
        "history.jsonl",
        "\"one\"\nnot json at all\n\"two\"\n{\"a\":1}\n123\n\n   \n\"three\"\n\"unterminated\n",
    );
    let history = History::load(&path);
    assert_eq!(entries(&history), ["three", "two", "one"]);
    // Lines 2, 4, 5 and 9; the blank ones are not worth a warning.
    let warnings = history.warnings();
    assert_eq!(warnings.len(), 4, "{warnings:?}");
    for (warning, line) in warnings.iter().zip([2, 4, 5, 9]) {
        assert!(
            warning.contains(&format!("history.jsonl:{line}:")),
            "{warning}"
        );
    }

    // The next save writes the file without the bad lines.
    history.save().unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "\"one\"\n\"two\"\n\"three\"\n"
    );
    assert!(History::load(&path).warnings().is_empty());
}

#[test]
fn a_windows_file_and_bad_utf8_still_load() {
    let root = TempDir::new("hist-odd");
    let path = root.path().join("history.jsonl");
    fs::write(&path, b"\"a\"\r\n\"b\xFFc\"\r\n").unwrap();
    let history = History::load(&path);
    assert_eq!(entries(&history), ["b\u{FFFD}c", "a"]);
    assert!(history.warnings().is_empty());
}

#[test]
fn an_unreadable_history_is_empty_with_a_warning() {
    let root = TempDir::new("hist-dir");
    // A directory where the file should be.
    let history = History::load(root.path());
    assert!(history.is_empty());
    assert_eq!(history.warnings().len(), 1, "{:?}", history.warnings());
}

#[test]
fn a_file_longer_than_the_cap_keeps_the_newest() {
    let root = TempDir::new("hist-long");
    let mut text = String::new();
    for n in 0..MAX_ENTRIES + 20 {
        text.push_str(&format!("\"cmd {n}\"\n"));
    }
    let path = root.write("history.jsonl", &text);
    let history = History::load(&path);
    assert_eq!(history.len(), MAX_ENTRIES);
    assert_eq!(history.get_from_newest(0), Some("cmd 1019"));
    assert_eq!(history.get_from_newest(MAX_ENTRIES - 1), Some("cmd 20"));
}

#[test]
fn prefix_matches_lists_each_text_once_newest_first() {
    let mut history = History::in_memory();
    for entry in [
        "AT+VER?", "ATI", "at lower", "AT+VER?", "ATE0", "AT", "ATI", "OTHER",
    ] {
        history.push(entry);
    }
    // `AT+VER?` was sent twice and `ATI` twice, apart; each is listed once, at its
    // newest position.
    assert_eq!(
        history.prefix_matches("AT"),
        ["ATI", "AT", "ATE0", "AT+VER?"]
    );
    assert_eq!(history.prefix_matches("AT+"), ["AT+VER?"]);
    assert_eq!(history.prefix_matches("at"), ["at lower"], "case counts");
    assert!(history.prefix_matches("zzz").is_empty());
    assert_eq!(history.prefix_matches("").len(), 6);
    assert_eq!(history.prefix_matches("AT+VER?"), ["AT+VER?"]);
    assert!(
        history.prefix_matches("AT+VER?!").is_empty(),
        "the prefix is longer than the entry"
    );
}

#[test]
fn saving_replaces_the_file_and_leaves_nothing_behind() {
    let root = TempDir::new("hist-atomic");
    let path = root.path().join("history.jsonl");
    let mut history = History::load(&path);
    history.push("one");
    history.save().unwrap();
    history.push("two");
    history.save().unwrap();
    history.clear();
    assert!(history.is_empty());
    history.save().unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "");
    let names: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["history.jsonl"], "no temp file is left");
}

#[test]
fn a_history_without_a_path_saves_to_nowhere() {
    let mut history = History::default();
    history.push("AT");
    assert_eq!(history.path(), None);
    history.save().unwrap();
}
