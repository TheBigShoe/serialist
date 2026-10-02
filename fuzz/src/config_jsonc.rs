//! `config_jsonc`: the JSONC loaders for the files a user edits, on arbitrary text.
//!
//! The input is an [`Input`], read as whole documents: the chunk lengths are ignored and
//! the stream is the file. The app reads these files as UTF-8, so the bytes become text
//! with [`String::from_utf8_lossy`]: an invalid sequence is replaced by U+FFFD, and every
//! run of bytes is some document. Config bits 0..=1 pick the loader (config 0, a
//! settings file, is the most realistic):
//!
//! | bits | loader |
//! | --- | --- |
//! | 0 | [`Settings::from_jsonc`], a settings file over the bundled defaults |
//! | 1 | [`ThemeFamily::parse`], a Zed theme family |
//! | 2 | [`Keymap::parse`], a keymap |
//! | 3 | [`CommandCollection::parse`] with [`CollectionSource::Bundled`], a saved-commands file |
//!
//! Beyond not panicking (or overflowing the stack: a hostile theme file must not crash
//! the app), it checks:
//!
//! - The loader is deterministic: a second call on the same text gives a result whose
//!   `Debug` form is identical. (Compared as text because `Settings` holds `f32`s that
//!   can be NaN and the error types are not `PartialEq`.)
//! - An error that carries a position has one inside the text: a line from 1 to the
//!   number of lines, and a column from 1 to one past the characters of that line. Lines
//!   end at `\n` or at a `\r` that is not followed by `\n`, which is how jsonc-parser
//!   counts them. The error names the origin it was given, and its `Display` form works.
//! - Settings: a document that parsed is never a `SettingsError::Merged` ("every layer is
//!   checked on its own first"), and the resolved fonts have finite, positive sizes and
//!   line heights and weights from 100 to 900, as the settings documentation says.
//!   `default_baud` is at least 1.
//! - Commands: `to_json` writes the collection out and parsing that gives back the same
//!   collection, since the file form is "the model and nothing else".
//! - Keymap and commands: reading a loaded model (`resolved`, `action_names`, `commands`)
//!   does not panic.
//!
//! Deep nesting is the one input that can exhaust the stack in a recursive parser.
//! jsonc-parser 0.34 stops at a nesting depth of 512 with an error, so these checks hold
//! the loaders to that: the depth tests below nest 20,000 levels on a thread with 2 MiB
//! of stack, which an unlimited recursion would overflow many times over.

use std::path::Path;

use serialist_core::commands::CommandsError;
use serialist_core::theme::ThemeError;
use serialist_core::{
    CollectionSource, CommandCollection, Keymap, KeymapError, Settings, SettingsError, ThemeFamily,
};

use crate::Input;

/// The loaders config bits 0..=1 choose from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loader {
    Settings,
    Theme,
    Keymap,
    Commands,
}

/// In config order: bits 0..=1 index this.
pub const LOADERS: [Loader; 4] = [
    Loader::Settings,
    Loader::Theme,
    Loader::Keymap,
    Loader::Commands,
];

/// The file name every loader but settings is told it is reading.
const ORIGIN: &str = "fuzz.json";

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    let text = String::from_utf8_lossy(input.stream);
    load(input.pick(0, &LOADERS[..]), &text);
}

/// Run one loader on `text`, with every check.
pub fn load(loader: Loader, text: &str) {
    match loader {
        Loader::Settings => settings(text),
        Loader::Theme => theme(text),
        Loader::Keymap => keymap(text),
        Loader::Commands => commands(text),
    }
}

fn settings(text: &str) {
    let result = Settings::from_jsonc(text);
    deterministic("settings", text, &result, &Settings::from_jsonc(text));
    match &result {
        Ok(settings) => check_settings(settings),
        Err(
            err @ SettingsError::Invalid {
                file, line, column, ..
            },
        ) => {
            assert_eq!(
                file,
                Path::new("<settings>"),
                "the error names another file"
            );
            in_text("settings", text, *line, *column);
            display("settings", err);
        }
        Err(err) => panic!("settings: {err:?} from text that is not a file read"),
    }
}

fn check_settings(settings: &Settings) {
    let fonts = [
        settings.resolved_terminal_font(),
        settings.resolved_ui_font(),
    ];
    for font in &fonts {
        assert!(
            font.size.is_finite() && font.size > 0.0,
            "a font size of {} loaded",
            font.size
        );
        assert!(
            (100.0..=900.0).contains(&font.weight),
            "a font weight of {} loaded",
            font.weight
        );
        assert!(
            font.line_height.is_finite() && font.line_height > 0.0,
            "a line height of {} loaded",
            font.line_height
        );
    }
    assert!(settings.default_baud >= 1, "a baud rate of 0 loaded");
}

fn theme(text: &str) {
    let result = ThemeFamily::parse(text, ORIGIN);
    deterministic("theme", text, &result, &ThemeFamily::parse(text, ORIGIN));
    match &result {
        Ok(_) => {}
        Err(
            err @ ThemeError::Invalid {
                file, line, column, ..
            },
        ) => {
            assert_eq!(file, ORIGIN, "the error names another file");
            in_text("theme", text, *line, *column);
            display("theme", err);
        }
        Err(err @ ThemeError::Schema { file, .. }) => {
            assert_eq!(file, ORIGIN, "the error names another file");
            display("theme", err);
        }
        Err(err) => panic!("theme: {err:?} from text that is not a file read"),
    }
}

fn keymap(text: &str) {
    let origin = Path::new(ORIGIN);
    let result = Keymap::parse(text, origin);
    deterministic("keymap", text, &result, &Keymap::parse(text, origin));
    match &result {
        Ok(keymap) => {
            assert!(keymap.resolved().len() <= keymap.entries.len());
            assert!(keymap.action_names().len() <= keymap.entries.len());
        }
        Err(
            err @ KeymapError::Invalid {
                file, line, column, ..
            },
        ) => {
            assert_eq!(file, origin, "the error names another file");
            in_text("keymap", text, *line, *column);
            display("keymap", err);
        }
        Err(err) => panic!("keymap: {err:?} from text that is not a file read"),
    }
}

fn commands(text: &str) {
    let origin = Path::new(ORIGIN);
    let parse = |text: &str| CommandCollection::parse(text, CollectionSource::Bundled, origin);
    let result = parse(text);
    deterministic("commands", text, &result, &parse(text));
    match &result {
        Ok(loaded) => {
            let written = loaded.collection.to_json();
            let again = parse(&written)
                .unwrap_or_else(|err| panic!("commands: to_json wrote what does not load: {err}"));
            assert_eq!(
                again.collection, loaded.collection,
                "commands: to_json and a re-parse changed the collection:\n{written}"
            );
        }
        Err(
            err @ CommandsError::Invalid {
                file, line, column, ..
            },
        ) => {
            assert_eq!(file, origin, "the error names another file");
            in_text("commands", text, *line, *column);
            display("commands", err);
        }
        Err(err) => panic!("commands: {err:?} from text that is not a file read"),
    }
}

/// The two results of one loader on one text are the same.
fn deterministic<T: std::fmt::Debug>(what: &str, text: &str, first: &T, second: &T) {
    let (first, second) = (format!("{first:?}"), format!("{second:?}"));
    assert!(
        first == second,
        "{what}: two loads of the same {} bytes differ:\n{first}\n{second}",
        text.len()
    );
}

/// Formatting an error does not panic and says something.
fn display(what: &str, err: &dyn std::error::Error) {
    assert!(
        !err.to_string().is_empty(),
        "{what}: an error with no message"
    );
}

/// `line` and `column` (both counted from 1) lie inside `text`.
fn in_text(what: &str, text: &str, line: usize, column: usize) {
    let lines = line_lengths(text);
    assert!(
        (1..=lines.len()).contains(&line),
        "{what}: error at line {line}, but the text has {} lines",
        lines.len()
    );
    let longest = lines[line - 1] + 1;
    assert!(
        (1..=longest).contains(&column),
        "{what}: error at {line}:{column}, but line {line} has {} characters",
        longest - 1
    );
}

/// The characters on each line of `text`. A line ends at `\n`, or at a `\r` that is not
/// followed by `\n` (a lone `\r` ends a `//` comment, so jsonc-parser breaks lines there
/// too). The last entry is the line after the final break, empty when the text ends in one.
fn line_lengths(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let mut lines = vec![0];
    for (i, c) in text.char_indices() {
        if c == '\n' || (c == '\r' && bytes.get(i + 1) != Some(&b'\n')) {
            lines.push(0);
        } else if let Some(last) = lines.last_mut() {
            *last += 1;
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_replay() {
        crate::replay_seeds("config_jsonc", super::run);
    }

    #[test]
    fn lines_break_as_jsonc_parser_counts_them() {
        assert_eq!(line_lengths(""), [0]);
        assert_eq!(line_lengths("ab\ncd"), [2, 2]);
        assert_eq!(line_lengths("ab\n"), [2, 0]);
        // CRLF is one break and the CR is a character of its line; a lone CR is a break.
        assert_eq!(line_lengths("ab\r\ncd"), [3, 2]);
        assert_eq!(line_lengths("ab\rcd"), [2, 2]);
    }

    #[test]
    fn well_formed_documents_load_in_every_mode() {
        load(Loader::Settings, "{ \"buffer_font_size\": 12, } // x");
        load(Loader::Theme, "{ \"name\": \"t\", \"themes\": [] }");
        load(Loader::Keymap, "[ { \"bindings\": { \"cmd-k\": null } } ]");
        load(
            Loader::Commands,
            "{ \"groups\": [ { \"name\": \"g\", \"commands\": [ \
             { \"name\": \"AT\", \"payload\": { \"text\": \"AT\" } } ] } ] }",
        );
    }

    /// Run `f` on a thread with 2 MiB of stack, what a thread the app spawns gets (the
    /// main thread has 8 MiB). A debug build needs about 1 MiB for the 512 levels the
    /// parser allows, and an unlimited recursion would need over 20 MiB for 20,000.
    fn on_small_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(f)
            .expect("a thread")
            .join()
            .expect("the loaders panicked (a stack overflow would abort the process)");
    }

    /// Nesting up to 20,000 levels, past jsonc-parser's limit of 512, in every shape the
    /// loaders accept, shut and left open, never panics or overflows a 2 MiB stack.
    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        on_small_stack(|| {
            for depth in [1, 64, 255, 256, 257, 511, 512, 513, 600, 4096, 20_000] {
                let shapes = [
                    format!("{}{}", "[".repeat(depth), "]".repeat(depth)),
                    "[".repeat(depth),
                    format!("{}1{}", "{\"a\":".repeat(depth), "}".repeat(depth)),
                    "{\"a\":".repeat(depth),
                    format!(
                        "{}{}",
                        "[{\"bindings\":".repeat(depth / 2),
                        "}]".repeat(depth / 2)
                    ),
                ];
                for text in &shapes {
                    for loader in LOADERS {
                        load(loader, text);
                    }
                }
            }
        });
    }

    /// Past the limit a loader reports an error rather than cutting the document short.
    #[test]
    fn past_the_limit_is_an_error() {
        on_small_stack(|| {
            let deep = format!("{}{}", "[".repeat(600), "]".repeat(600));
            assert!(Settings::from_jsonc(&deep).is_err());
            assert!(ThemeFamily::parse(&deep, ORIGIN).is_err());
            assert!(Keymap::parse(&deep, Path::new(ORIGIN)).is_err());
            assert!(
                CommandCollection::parse(&deep, CollectionSource::Bundled, Path::new(ORIGIN))
                    .is_err()
            );
        });
    }
}
