//! Reading and writing collection files.
//!
//! Reading takes JSON with comments and trailing commas, as Zed's files do. Writing
//! produces plain pretty-printed JSON from the model and preserves nothing else:
//! comments, key order beyond the model's own, unknown keys and formatting are lost. A
//! file the app edits is therefore one to keep free of hand-written comments; a file
//! that is only ever hand-edited is never rewritten.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jsonc_parser::ParseOptions;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{CollectionSource, CommandCollection, CommandGroup};

/// Why a collection file could not be used.
#[derive(Debug, thiserror::Error)]
pub enum CommandsError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Bad syntax, or a value of the wrong shape. `line` and `column` count from 1.
    #[error("{}:{line}:{column}: {message}", file.display())]
    Invalid {
        file: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
}

/// A problem that did not stop a file loading: an unknown key, a command that will not
/// encode, a pattern that is not a regex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandWarning {
    pub file: PathBuf,
    pub message: String,
}

impl std::fmt::Display for CommandWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.message)
    }
}

/// A parsed collection and what was odd about it.
#[derive(Clone, Debug, PartialEq)]
pub struct LoadedCollection {
    pub collection: CommandCollection,
    pub warnings: Vec<String>,
}

/// The file's shape. `name` falls back to the file's name.
#[derive(Deserialize)]
struct CollectionFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    groups: Vec<CommandGroup>,
}

#[derive(Serialize)]
struct CollectionOut<'a> {
    name: &'a str,
    groups: &'a [CommandGroup],
}

/// Keys the model reads, by level, so a typo is reported instead of silently dropped.
const COLLECTION_KEYS: &[&str] = &["name", "groups"];
const GROUP_KEYS: &[&str] = &["name", "commands"];
const COMMAND_KEYS: &[&str] = &[
    "name",
    "description",
    "payload",
    "eol",
    "expect",
    "keybinding",
    "params",
];
const PAYLOAD_KEYS: &[&str] = &["text", "hex", "codec", "fields", "script"];
const EXPECT_KEYS: &[&str] = &["pattern", "timeout_ms"];
const PARAM_KEYS: &[&str] = &["name", "label", "default", "kind"];

fn options() -> ParseOptions {
    ParseOptions::default()
}

fn invalid(file: &Path, err: &jsonc_parser::errors::ParseError) -> CommandsError {
    CommandsError::Invalid {
        file: file.to_path_buf(),
        line: err.line_display(),
        column: err.column_display(),
        // The kind alone: `Display` on the error appends the position again.
        message: err.kind().to_string(),
    }
}

impl CommandCollection {
    /// Parses one collection document. `origin` names it in errors and gives the
    /// collection its name (the file stem) when the file has none. Empty text, or text
    /// with only comments, is an empty collection.
    pub fn parse(
        text: &str,
        source: CollectionSource,
        origin: &Path,
    ) -> Result<LoadedCollection, CommandsError> {
        let fallback = origin.file_stem().map_or_else(
            || "Commands".to_owned(),
            |s| s.to_string_lossy().into_owned(),
        );
        let value: Value = jsonc_parser::parse_to_serde_value(text, &options())
            .map_err(|e| invalid(origin, &e))?;
        if value.is_null() {
            return Ok(LoadedCollection {
                collection: CommandCollection::new(fallback, source),
                warnings: Vec::new(),
            });
        }
        // Read the typed form straight from the text so an error carries a position.
        let file: CollectionFile = jsonc_parser::parse_to_serde_value(text, &options())
            .map_err(|e| invalid(origin, &e))?;
        let collection = CommandCollection {
            name: file
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or(fallback),
            source,
            groups: file.groups,
        };
        let mut warnings = Vec::new();
        unknown_keys(&value, &mut warnings);
        collection.check(&mut warnings);
        Ok(LoadedCollection {
            collection,
            warnings,
        })
    }

    /// Reads and parses the file at `path`.
    pub fn load(path: &Path, source: CollectionSource) -> Result<LoadedCollection, CommandsError> {
        let text = fs::read_to_string(path).map_err(|source| CommandsError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, source, path)
    }

    /// The file form: pretty JSON, two-space indent, ending in a newline. Nothing but
    /// the model is written, so comments and unknown keys in the original are gone.
    pub fn to_json(&self) -> String {
        let out = CollectionOut {
            name: &self.name,
            groups: &self.groups,
        };
        // Serializing plain data cannot fail.
        let mut text = serde_json::to_string_pretty(&out).unwrap_or_else(|_| "{}".to_owned());
        text.push('\n');
        text
    }

    /// Problems in the model that are not a load failure.
    fn check(&self, warnings: &mut Vec<String>) {
        for (index, group) in self.groups.iter().enumerate() {
            if group.name.trim().is_empty() {
                warnings.push(format!("group {} has no name", index + 1));
            }
            if self.groups[..index].iter().any(|g| g.name == group.name) {
                warnings.push(format!("there are two groups called `{}`", group.name));
            }
        }
        let mut seen: Vec<&str> = Vec::new();
        for (group, command) in self.commands() {
            let label = format!("`{}` \u{203a} `{}`", group.name, command.name);
            for problem in command.problems() {
                warnings.push(format!("{label}: {problem}"));
            }
            if seen.contains(&command.name.as_str()) {
                warnings.push(format!(
                    "{label}: another command is also called `{}`; sending by name reaches the first",
                    command.name
                ));
            }
            seen.push(&command.name);
        }
    }
}

fn unknown_keys(root: &Value, out: &mut Vec<String>) {
    let mut check = |value: &Value, known: &[&str], path: &str| {
        if let Value::Object(map) = value {
            for key in map.keys().filter(|key| !known.contains(&key.as_str())) {
                out.push(format!("unknown key `{path}{key}` is ignored"));
            }
        }
    };
    check(root, COLLECTION_KEYS, "");
    let items = |value: &Value, key: &str| -> Vec<Value> {
        value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    for (g, group) in items(root, "groups").iter().enumerate() {
        check(group, GROUP_KEYS, &format!("groups[{g}]."));
        for (c, command) in items(group, "commands").iter().enumerate() {
            let at = format!("groups[{g}].commands[{c}].");
            check(command, COMMAND_KEYS, &at);
            if let Some(payload) = command.get("payload") {
                check(payload, PAYLOAD_KEYS, &format!("{at}payload."));
            }
            if let Some(expect) = command.get("expect") {
                check(expect, EXPECT_KEYS, &format!("{at}expect."));
            }
            for (p, param) in items(command, "params").iter().enumerate() {
                check(param, PARAM_KEYS, &format!("{at}params[{p}]."));
            }
        }
    }
}

/// Writes `contents` to `path` so a reader never sees half a file: into a temp file in
/// the same directory (named so that neither the loader nor the watcher takes it for
/// config), synced, then renamed over `path`. Creates missing directories.
pub(crate) fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?
        .to_string_lossy();
    let temp = parent.join(format!(
        ".{name}.{}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    let renamed = written.and_then(|()| fs::rename(&temp, path));
    if renamed.is_err() {
        let _ = fs::remove_file(&temp);
    }
    renamed
}
