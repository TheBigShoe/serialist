//! The saved-command model: collections of groups of commands.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::settings::LineEnding;

/// The `timeout_ms` an [`Expect`] gets when the file leaves it out.
pub const DEFAULT_EXPECT_TIMEOUT_MS: u64 = 1000;

/// Where a collection came from, which decides whether it can be saved.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CollectionSource {
    /// A `*.json` file in the user's `commands/` directory.
    User(PathBuf),
    /// A project's `.serialist/commands.json`.
    Project(PathBuf),
    /// The examples compiled into the binary. Read-only.
    Bundled,
}

impl CollectionSource {
    /// The file behind the collection, `None` for the bundled one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            CollectionSource::User(path) | CollectionSource::Project(path) => Some(path),
            CollectionSource::Bundled => None,
        }
    }
}

/// One file's worth of commands, in groups.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandCollection {
    pub name: String,
    pub source: CollectionSource,
    pub groups: Vec<CommandGroup>,
}

/// A named run of commands inside a collection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandGroup {
    pub name: String,
    #[serde(default)]
    pub commands: Vec<Command>,
}

/// A saved command: a payload, and what to do around sending it.
///
/// In a file every key but `name` and `payload` is optional:
///
/// ```jsonc
/// { "name": "Version", "description": "Ask for the firmware version",
///   "payload": { "text": "AT+VER?" },
///   "eol": "crlf",
///   "expect": { "pattern": "^OK|^ERROR", "timeout_ms": 1000 },
///   "keybinding": "cmd-1",
///   "params": [ { "name": "id", "label": "Command id", "default": "0x0F15", "kind": "hex16" } ] }
/// ```
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub name: String,
    /// One line for the panel's tooltip. Empty when there is none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    pub payload: Payload,
    /// Overrides the session's line ending when present. Without it a `text` payload
    /// uses the session's ending and a `hex` or `codec` payload sends none: a binary
    /// frame is complete as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eol: Option<LineEnding>,
    /// The reply to wait for after sending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<Expect>,
    /// Zed-style keystrokes, such as `cmd-1` or `ctrl-shift-f5`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keybinding: Option<String>,
    /// Values to ask for when the command is sent, in the order to ask.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub params: Vec<Param>,
}

impl Command {
    /// A command with just a name and payload.
    pub fn new(name: impl Into<String>, payload: Payload) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            payload,
            eol: None,
            expect: None,
            keybinding: None,
            params: Vec::new(),
        }
    }

    /// A command that sends `text`.
    pub fn text(name: impl Into<String>, text: impl Into<String>) -> Self {
        Self::new(name, Payload::Text(text.into()))
    }

    /// A command that sends the bytes in `hex`.
    pub fn hex(name: impl Into<String>, hex: impl Into<String>) -> Self {
        Self::new(name, Payload::Hex(hex.into()))
    }

    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn with_eol(mut self, eol: LineEnding) -> Self {
        self.eol = Some(eol);
        self
    }

    pub fn with_expect(mut self, pattern: impl Into<String>, timeout_ms: u64) -> Self {
        self.expect = Some(Expect {
            pattern: pattern.into(),
            timeout_ms,
        });
        self
    }

    pub fn with_keybinding(mut self, keystrokes: impl Into<String>) -> Self {
        self.keybinding = Some(keystrokes.into());
        self
    }

    pub fn with_param(mut self, param: Param) -> Self {
        self.params.push(param);
        self
    }

    /// The parameters to prompt for, in order.
    pub fn prompt_params(&self) -> &[Param] {
        &self.params
    }
}

/// What a command sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "PayloadRepr", into = "PayloadRepr")]
pub enum Payload {
    /// `{ "text": "AT+VER?" }`: text with `{{param}}` placeholders and the escapes
    /// `\r \n \t \0 \\ \{ \}` and `\xNN`.
    Text(String),
    /// `{ "hex": "05 5A 02 00 {{id}}" }`: bytes as hex digits, with spaces, commas,
    /// newlines and `0x` prefixes tolerated, and `{{param}}` placeholders.
    Hex(String),
    /// `{ "codec": "airoha-race", "fields": { … } }`: a plugin encodes it. Parsed and
    /// kept, but [`Command::encode`] fails with
    /// [`PayloadError::CodecUnavailable`](super::PayloadError::CodecUnavailable) until
    /// codecs land.
    Codec {
        codec: String,
        fields: Map<String, Value>,
    },
    /// `{ "script": "probe.lua" }`: sending the command runs this Lua script on the
    /// session instead of sending bytes. A relative path is relative to the scripts
    /// folder. [`Command::encode`] fails with
    /// [`PayloadError::ScriptPayload`](super::PayloadError::ScriptPayload), so nothing
    /// sends it as bytes by mistake.
    Script { path: PathBuf },
}

impl Payload {
    /// The script a `{ "script": … }` payload runs.
    pub fn script(&self) -> Option<&Path> {
        match self {
            Payload::Script { path } => Some(path),
            _ => None,
        }
    }
}

/// The file form of a [`Payload`]: exactly one of `text`, `hex`, `codec` or `script`.
#[derive(Serialize, Deserialize)]
struct PayloadRepr {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fields: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    script: Option<PathBuf>,
}

impl TryFrom<PayloadRepr> for Payload {
    type Error = String;

    fn try_from(repr: PayloadRepr) -> Result<Self, String> {
        const HINT: &str = "a payload is { \"text\": … }, { \"hex\": … }, \
             { \"codec\": …, \"fields\": … } or { \"script\": \"probe.lua\" }";
        let PayloadRepr {
            text,
            hex,
            codec,
            fields,
            script,
        } = repr;
        if let Some(path) = script {
            return match (text, hex, codec, fields) {
                (None, None, None, None) if path.as_os_str().is_empty() => {
                    Err("a script payload needs the script's path".to_owned())
                }
                (None, None, None, None) => Ok(Payload::Script { path }),
                _ => Err(format!(
                    "a payload has exactly one of text, hex, codec and script: {HINT}"
                )),
            };
        }
        match (text, hex, codec, fields) {
            (Some(text), None, None, None) => Ok(Payload::Text(text)),
            (None, Some(hex), None, None) => Ok(Payload::Hex(hex)),
            (None, None, Some(codec), fields) => Ok(Payload::Codec {
                codec,
                fields: fields.unwrap_or_default(),
            }),
            (None, None, None, None) => Err(format!(
                "this payload has no text, hex, codec or script: {HINT}"
            )),
            (None, None, None, Some(_)) => {
                Err(format!("`fields` belongs to a codec payload: {HINT}"))
            }
            _ => Err(format!(
                "a payload has exactly one of text, hex, codec and script: {HINT}"
            )),
        }
    }
}

impl From<Payload> for PayloadRepr {
    fn from(payload: Payload) -> Self {
        let empty = PayloadRepr {
            text: None,
            hex: None,
            codec: None,
            fields: None,
            script: None,
        };
        match payload {
            Payload::Text(text) => PayloadRepr {
                text: Some(text),
                ..empty
            },
            Payload::Hex(hex) => PayloadRepr {
                hex: Some(hex),
                ..empty
            },
            Payload::Codec { codec, fields } => PayloadRepr {
                codec: Some(codec),
                fields: Some(fields),
                ..empty
            },
            Payload::Script { path } => PayloadRepr {
                script: Some(path),
                ..empty
            },
        }
    }
}

/// The reply a command waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Expect {
    /// A regex over incoming lines. Smart case, as in search: all lowercase matches
    /// either case.
    pub pattern: String,
    /// How long to wait for a matching line, from the moment of sending. Defaults to
    /// [`DEFAULT_EXPECT_TIMEOUT_MS`] when the file leaves it out.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_timeout_ms() -> u64 {
    DEFAULT_EXPECT_TIMEOUT_MS
}

impl Expect {
    pub fn new(pattern: impl Into<String>, timeout_ms: u64) -> Self {
        Self {
            pattern: pattern.into(),
            timeout_ms,
        }
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
}

/// A value the user supplies when sending a command, used by its `{{name}}` placeholders.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    /// The placeholder name: letters, digits, `_` and `-`.
    pub name: String,
    /// The prompt's label. [`Param::display_label`] falls back to the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The prefilled value. A number in the file reads as its text.
    #[serde(
        default,
        deserialize_with = "text_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub default: Option<String>,
    #[serde(default)]
    pub kind: ParamKind,
}

impl Param {
    pub fn new(name: impl Into<String>, kind: ParamKind) -> Self {
        Self {
            name: name.into(),
            label: None,
            default: None,
            kind,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    /// What to call the value in a prompt.
    pub fn display_label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.name)
    }
}

fn text_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    use serde::de::Error;
    match Value::deserialize(d)? {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text)),
        Value::Number(number) => Ok(Some(number.to_string())),
        Value::Bool(flag) => Ok(Some(flag.to_string())),
        _ => Err(D::Error::custom("a default is text or a number")),
    }
}

/// How a parameter's value is checked and written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// Any text, used as typed.
    #[default]
    Text,
    /// A whole number, decimal or `0x` hex. Text payloads get it in decimal; hex payloads
    /// get one byte, so it must fit 0 to 255.
    Int,
    /// A 16-bit number in hex, with or without `0x`. Text payloads get four upper-case
    /// digits; hex payloads get two bytes, little-endian, or big-endian with
    /// `{{name:be}}`.
    Hex16,
}

impl ParamKind {
    pub const ALL: [ParamKind; 3] = [ParamKind::Text, ParamKind::Int, ParamKind::Hex16];

    /// The name the file uses.
    pub fn label(self) -> &'static str {
        match self {
            ParamKind::Text => "text",
            ParamKind::Int => "int",
            ParamKind::Hex16 => "hex16",
        }
    }
}

/// Names a command: its collection, group and name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandRef {
    pub collection: String,
    pub group: String,
    pub name: String,
}

impl CommandRef {
    pub fn new(
        collection: impl Into<String>,
        group: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            collection: collection.into(),
            group: group.into(),
            name: name.into(),
        }
    }
}

impl std::fmt::Display for CommandRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} \u{203a} {} \u{203a} {}",
            self.collection, self.group, self.name
        )
    }
}

impl CommandCollection {
    /// An empty collection.
    pub fn new(name: impl Into<String>, source: CollectionSource) -> Self {
        Self {
            name: name.into(),
            source,
            groups: Vec::new(),
        }
    }

    /// Whether the collection cannot be saved.
    pub fn is_read_only(&self) -> bool {
        self.source == CollectionSource::Bundled
    }

    /// The file behind the collection, `None` for the bundled one.
    pub fn path(&self) -> Option<&Path> {
        self.source.path()
    }

    /// Every command with its group, in order.
    pub fn commands(&self) -> impl Iterator<Item = (&CommandGroup, &Command)> {
        self.groups
            .iter()
            .flat_map(|group| group.commands.iter().map(move |command| (group, command)))
    }

    /// The first command called `name`, in any group.
    pub fn find(&self, name: &str) -> Option<(&CommandGroup, &Command)> {
        self.commands().find(|(_, command)| command.name == name)
    }

    /// The [`CommandRef`] for `command` in `group`.
    pub fn command_ref(&self, group: &CommandGroup, command: &Command) -> CommandRef {
        CommandRef::new(&self.name, &group.name, &command.name)
    }

    pub fn group(&self, name: &str) -> Option<&CommandGroup> {
        self.groups.iter().find(|group| group.name == name)
    }
}
