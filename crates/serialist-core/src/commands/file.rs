//! Reading and writing collection files.
//!
//! Reading takes JSON with comments and trailing commas, as Zed's files do. Writing
//! produces plain pretty-printed JSON from the model, plus everything the file held that
//! this build does not read ([`Carried`]), so that an edit in an earlier build does not
//! destroy a later build's data. Comments, formatting and the order of keys beyond the
//! model's own are lost. A file the app edits is therefore one to keep free of
//! hand-written comments; a file that is only ever hand-edited is never rewritten.

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use jsonc_parser::ParseOptions;
use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::model::{
    Carried, CollectionSource, Command, CommandCollection, CommandGroup, Expect, Param, Payload,
    Skipped,
};
use crate::settings::LineEnding;

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

/// The file's top-level shape. `name` falls back to the file's name. The groups are
/// read one by one, so here they only have to be a list.
#[derive(Deserialize)]
struct CollectionFile {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "groups")]
    _groups: Vec<IgnoredAny>,
}

/// The written form: the model's keys in the model's order, then what it carried.
#[derive(Serialize)]
struct CollectionOut<'a> {
    name: &'a str,
    groups: Vec<Entry<'a, GroupOut<'a>>>,
    #[serde(flatten)]
    carried: &'a Map<String, Value>,
}

#[derive(Serialize)]
struct GroupOut<'a> {
    name: &'a str,
    commands: Vec<Entry<'a, CommandOut<'a>>>,
    #[serde(flatten)]
    carried: &'a Map<String, Value>,
}

/// A [`Command`]'s keys, as its own `Serialize` writes them, with what it carried at
/// each level.
#[derive(Serialize)]
struct CommandOut<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    description: &'a str,
    payload: WithCarried<'a, Payload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    eol: Option<LineEnding>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expect: Option<WithCarried<'a, Expect>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keybinding: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    params: Vec<WithCarried<'a, Param>>,
    #[serde(flatten)]
    carried: &'a Map<String, Value>,
}

#[derive(Serialize)]
struct WithCarried<'a, T: Serialize> {
    #[serde(flatten)]
    known: &'a T,
    #[serde(flatten)]
    carried: &'a Map<String, Value>,
}

impl<'a> CommandOut<'a> {
    fn new(command: &'a Command) -> Self {
        let carried = &command.carried;
        Self {
            name: &command.name,
            description: &command.description,
            payload: WithCarried {
                known: &command.payload,
                carried: &carried.payload,
            },
            eol: command.eol,
            expect: command.expect.as_ref().map(|known| WithCarried {
                known,
                carried: &carried.expect,
            }),
            keybinding: command.keybinding.as_deref(),
            params: command
                .params
                .iter()
                .map(|known| WithCarried {
                    known,
                    carried: &known.carried.keys,
                })
                .collect(),
            carried: &carried.keys,
        }
    }
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
const EXPECT_KEYS: &[&str] = &["pattern", "timeout_ms", "frame"];
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
        // Read the top level straight from the text so a wrong shape there carries a
        // position. Below it, a group or command that will not read is skipped alone.
        let file: CollectionFile = jsonc_parser::parse_to_serde_value(text, &options())
            .map_err(|e| invalid(origin, &e))?;
        let name = file
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(fallback);
        let mut collection = CommandCollection::new(name, source);
        let mut warnings = Vec::new();
        collection.carried.keys = unknown_of(&value, COLLECTION_KEYS);
        warn_unknown(&collection.carried.keys, "", &mut warnings);
        for (g, value) in items(&value, "groups").iter().enumerate() {
            match read_group(g, value, &mut warnings) {
                Ok(group) => collection.groups.push(group),
                Err(reason) => {
                    let label = match value.get("name").and_then(Value::as_str) {
                        Some(name) if !name.trim().is_empty() => format!("group `{name}`"),
                        _ => format!("group {}", g + 1),
                    };
                    warnings.push(format!("{label} {reason}; {SKIPPED}"));
                    collection.carried.skipped.push(Skipped {
                        before: collection.groups.len(),
                        value: value.clone(),
                    });
                }
            }
        }
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

    /// The file form: pretty JSON, two-space indent, ending in a newline. The model's
    /// keys come first, in its order, then every key the file held that this build does
    /// not read ([`Carried`]), at the level it was found; a group or command it could
    /// not read is written back as it was, in its place. Comments are not kept.
    pub fn to_json(&self) -> String {
        let out = CollectionOut {
            name: &self.name,
            groups: interleave(&self.groups, &self.carried.skipped, |group| GroupOut {
                name: &group.name,
                commands: interleave(&group.commands, &group.carried.skipped, CommandOut::new),
                carried: &group.carried.keys,
            }),
            carried: &self.carried.keys,
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

/// The end of a warning about a group or command that was not read.
const SKIPPED: &str = "it is skipped, and kept as it is when saving";

/// Reads group `g` of the file, warning about each command in it that will not read and
/// skipping that command alone. `Err` says why the group itself will not read.
fn read_group(g: usize, value: &Value, warnings: &mut Vec<String>) -> Result<CommandGroup, String> {
    let Value::Object(map) = value else {
        return Err("is not an object".to_owned());
    };
    let name = match map.get("name") {
        Some(Value::String(name)) => name.clone(),
        None | Some(Value::Null) => return Err("has no name".to_owned()),
        Some(_) => return Err("has a name that is not text".to_owned()),
    };
    let commands = match map.get("commands") {
        None | Some(Value::Null) => &[][..],
        Some(Value::Array(commands)) => commands.as_slice(),
        Some(_) => return Err("has `commands` that is not a list".to_owned()),
    };
    let mut group = CommandGroup::new(name, Vec::new());
    group.carried.keys = unknown_of(value, GROUP_KEYS);
    warn_unknown(&group.carried.keys, &format!("groups[{g}]."), warnings);
    for (c, value) in commands.iter().enumerate() {
        let at = format!("groups[{g}].commands[{c}].");
        match read_command(value, &at, warnings) {
            Ok(command) => group.commands.push(command),
            Err(reason) => {
                let command = match value.get("name").and_then(Value::as_str) {
                    Some(name) if !name.trim().is_empty() => format!("`{name}`"),
                    _ => format!("command {}", c + 1),
                };
                warnings.push(format!(
                    "`{}` \u{203a} {command}: this version cannot read it ({reason}); {SKIPPED}",
                    group.name
                ));
                group.carried.skipped.push(Skipped {
                    before: group.commands.len(),
                    value: value.clone(),
                });
            }
        }
    }
    Ok(group)
}

/// Reads one command, carrying the keys the model does not read, at every level, and
/// warning about them. `at` is its path in the file, for the warnings.
fn read_command(value: &Value, at: &str, warnings: &mut Vec<String>) -> Result<Command, String> {
    let mut command: Command = serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    let unknown = |key: &str, known: &[&str]| {
        value
            .get(key)
            .map(|inner| unknown_of(inner, known))
            .unwrap_or_default()
    };
    command.carried = Carried {
        keys: unknown_of(value, COMMAND_KEYS),
        payload: unknown("payload", PAYLOAD_KEYS),
        expect: unknown("expect", EXPECT_KEYS),
        skipped: Vec::new(),
    };
    warn_unknown(&command.carried.keys, at, warnings);
    warn_unknown(&command.carried.payload, &format!("{at}payload."), warnings);
    warn_unknown(&command.carried.expect, &format!("{at}expect."), warnings);
    // Params read in file order, so they pair up with the file's.
    for (p, (param, value)) in command
        .params
        .iter_mut()
        .zip(items(value, "params"))
        .enumerate()
    {
        param.carried.keys = unknown_of(value, PARAM_KEYS);
        warn_unknown(&param.carried.keys, &format!("{at}params[{p}]."), warnings);
    }
    Ok(command)
}

fn warn_unknown(keys: &Map<String, Value>, path: &str, warnings: &mut Vec<String>) {
    for key in keys.keys() {
        warnings.push(format!(
            "unknown key `{path}{key}` is not used, and is kept when saving"
        ));
    }
}

/// A list as written: each readable item through `write`, with the skipped ones put
/// back where they were.
fn interleave<'a, T, O>(
    readable: &'a [T],
    skipped: &'a [Skipped],
    write: impl Fn(&'a T) -> O,
) -> Vec<Entry<'a, O>> {
    let mut out = Vec::with_capacity(readable.len() + skipped.len());
    let mut skipped = skipped.iter().peekable();
    for (index, item) in readable.iter().enumerate() {
        while let Some(entry) = skipped.next_if(|s| s.before <= index) {
            out.push(Entry::Skipped(&entry.value));
        }
        out.push(Entry::Read(write(item)));
    }
    out.extend(skipped.map(|entry| Entry::Skipped(&entry.value)));
    out
}

/// One item of a written list: one this build read, or one it kept verbatim.
#[derive(Serialize)]
#[serde(untagged)]
enum Entry<'a, O> {
    Read(O),
    Skipped(&'a Value),
}

/// The array under `key` in `value`, or nothing.
fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// The keys of the object `value` that are not in `known`, with their values.
fn unknown_of(value: &Value, known: &[&str]) -> Map<String, Value> {
    match value {
        Value::Object(map) => map
            .iter()
            .filter(|(key, _)| !known.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        _ => Map::new(),
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
