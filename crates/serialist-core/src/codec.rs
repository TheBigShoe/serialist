//! Protocol codecs: the one trait every protocol plugin implements, the frames it
//! produces, and the registry the app keeps codecs in.
//!
//! A [`Codec`] turns the received byte stream into structured [`Frame`]s and turns
//! structured commands ([`EncodeRequest`]) into bytes to send. The app's codecs are
//! plugins the user installs: the plugin crate adapts Lua and WebAssembly plugins to this
//! trait (and keeps Rust reference codecs for its tests), so the rest of the app never
//! knows which kind it is talking to.
//!
//! # Decoding
//!
//! A codec runs on the ingest thread, fed through
//! [`CodecSink`](crate::frames::CodecSink), once per received chunk and never per byte.
//! It is a stateful framer: bytes that do not yet make a whole frame are held back and
//! finished by a later chunk. Each frame names the bytes it came from with a range of
//! *stream offsets* (`Frame::raw`), the same offsets the scrollback store uses, so the
//! UI can show or re-read a frame's bytes from a store snapshot without the codec
//! copying them. See [`Codec::decode`] for the exact contract.
//!
//! # Encoding
//!
//! [`Codec::encode`] takes a command name and JSON fields, the same shape as a saved
//! command's `{ "codec": "airoha-race", "fields": { … } }` payload. [`CodecInfo`] lists
//! the commands a codec accepts and their fields, so a UI can build a form. The helpers
//! on [`EncodeRequest`] ([`uint`](EncodeRequest::uint), [`bytes`](EncodeRequest::bytes),
//! [`check_fields`](EncodeRequest::check_fields)) implement the common field
//! conventions: integers as JSON numbers or hex strings, bytes as hex text or a list.
//!
//! # Registry
//!
//! The app keeps a [`CodecRegistry`] of [`CodecFactory`]s by name. A session that
//! activates a codec asks the registry for a fresh instance; nothing is shared between
//! instances, so two sessions never see each other's framing state.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Map, Value as JsonValue};
pub use smol_str::SmolStr;

use crate::text::Direction;

/// A protocol plugin: a stateful framer for received bytes and an encoder for commands.
///
/// Implementations must never panic on any input and must keep bounded memory whatever
/// arrives (a codec that holds back partial frames caps how much it holds).
///
/// Not `Send`: a codec may hold state bound to one thread (a Lua VM). It is made on the
/// thread that uses it, by a [`CodecFactory`], which is `Send + Sync` and is what
/// crosses threads (see [`CodecSink::from_factory`](crate::frames::CodecSink::from_factory)).
pub trait Codec {
    /// Name, version, the frame kinds it produces and the commands it encodes.
    fn describe(&self) -> CodecInfo;

    /// Decode one received chunk, appending any frames it completes to `out` in stream
    /// order. `out` may already hold frames; never clear it.
    ///
    /// - `chunk` is exactly what the session delivered. Consecutive calls see
    ///   consecutive bytes of one stream, until [`reset`](Self::reset).
    /// - `raw_offset` is the stream offset of `chunk[0]`. A codec holding `n` bytes back
    ///   from earlier calls knows they start at `raw_offset - n`.
    /// - `at` is when the chunk arrived; frames completed by this chunk carry it.
    /// - Every frame's `raw` lies within the bytes seen since the last reset, and frames
    ///   come out in order of `raw.start`. Frames that do not correspond to received bytes
    ///   use an empty range at the offset where they apply.
    ///
    /// Keep it O(chunk) plus the bytes held back: it runs on the ingest thread.
    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>);

    /// The bytes for one command, or why it cannot be encoded.
    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError>;

    /// Forget everything held back: the stream starts over (a disconnect, a reconnect).
    fn reset(&mut self);
}

/// Makes codec instances. What the [`CodecRegistry`] holds. Call
/// [`create`](Self::create) on the thread that will use the codec.
pub trait CodecFactory: Send + Sync {
    /// The same description the codecs it makes return from [`Codec::describe`].
    fn info(&self) -> CodecInfo;

    /// A fresh codec with no held-back state.
    fn create(&self) -> Result<Box<dyn Codec>, CodecError>;
}

/// A [`CodecFactory`] from a description and a constructor.
pub struct FnCodecFactory<F> {
    info: CodecInfo,
    make: F,
}

impl<F> FnCodecFactory<F>
where
    F: Fn() -> Result<Box<dyn Codec>, CodecError> + Send + Sync,
{
    pub fn new(info: CodecInfo, make: F) -> Self {
        Self { info, make }
    }
}

impl<F> CodecFactory for FnCodecFactory<F>
where
    F: Fn() -> Result<Box<dyn Codec>, CodecError> + Send + Sync,
{
    fn info(&self) -> CodecInfo {
        self.info.clone()
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        (self.make)()
    }
}

impl<F> fmt::Debug for FnCodecFactory<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FnCodecFactory")
            .field("name", &self.info.name)
            .finish_non_exhaustive()
    }
}

/// The codecs the app knows, by name. Cheap to clone: factories are shared.
#[derive(Clone, Default)]
pub struct CodecRegistry {
    factories: BTreeMap<String, Arc<dyn CodecFactory>>,
}

impl CodecRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `factory` under the name its [`CodecInfo`] gives, replacing (and
    /// returning) any factory already registered under that name.
    pub fn register(&mut self, factory: Arc<dyn CodecFactory>) -> Option<Arc<dyn CodecFactory>> {
        let name = factory.info().name;
        self.factories.insert(name, factory)
    }

    /// [`register`](Self::register) a description and a constructor.
    pub fn register_fn<F>(&mut self, info: CodecInfo, make: F) -> Option<Arc<dyn CodecFactory>>
    where
        F: Fn() -> Result<Box<dyn Codec>, CodecError> + Send + Sync + 'static,
    {
        self.register(Arc::new(FnCodecFactory::new(info, make)))
    }

    /// Remove a codec. Returns whether it was registered.
    pub fn unregister(&mut self, name: &str) -> bool {
        self.factories.remove(name).is_some()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.factories.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn CodecFactory>> {
        self.factories.get(name).cloned()
    }

    /// Registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.factories.keys().map(String::as_str)
    }

    /// Every registered codec's description, sorted by name.
    pub fn list(&self) -> Vec<CodecInfo> {
        self.factories.values().map(|f| f.info()).collect()
    }

    /// A fresh instance of the codec called `name`.
    pub fn create(&self, name: &str) -> Result<Box<dyn Codec>, CodecError> {
        self.factories
            .get(name)
            .ok_or_else(|| CodecError::UnknownCodec(name.to_owned()))?
            .create()
    }

    pub fn len(&self) -> usize {
        self.factories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.factories.is_empty()
    }
}

impl fmt::Debug for CodecRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.factories.keys()).finish()
    }
}

/// Why a codec could not be made or could not encode a command.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CodecError {
    /// The registry has no codec by this name.
    #[error("no codec named `{0}` is loaded")]
    UnknownCodec(String),
    /// The codec has no command by this name.
    #[error("unknown command `{0}`")]
    UnknownCommand(String),
    /// A required field is absent.
    #[error("missing field `{0}`")]
    MissingField(String),
    /// A field is present but unusable (wrong type, out of range, not a field of the
    /// command at all).
    #[error("field `{field}`: {reason}")]
    BadField { field: String, reason: String },
    /// Anything else: a plugin that failed to load, raised an error or broke its contract.
    #[error("{0}")]
    Internal(String),
}

impl CodecError {
    pub fn bad_field(field: impl Into<String>, reason: impl Into<String>) -> Self {
        CodecError::BadField {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

/// What a codec is and what it speaks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodecInfo {
    /// The name saved commands and settings use, such as `airoha-race`.
    pub name: String,
    pub version: String,
    pub description: String,
    /// The frame kinds `decode` produces, with their fields in the order frames list them.
    pub kinds: Vec<FrameKindInfo>,
    /// What `encode` accepts. The first is the default for a saved command that does not
    /// name one.
    pub commands: Vec<CommandInfo>,
}

impl CodecInfo {
    pub fn kind(&self, kind: &str) -> Option<&FrameKindInfo> {
        self.kinds.iter().find(|k| k.kind == kind)
    }

    pub fn command(&self, name: &str) -> Option<&CommandInfo> {
        self.commands.iter().find(|c| c.name == name)
    }

    /// The command a saved command gets when its fields do not name one.
    pub fn default_command(&self) -> Option<&CommandInfo> {
        self.commands.first()
    }
}

/// One kind of frame and its fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameKindInfo {
    pub kind: String,
    pub description: String,
    pub fields: Vec<FieldInfo>,
}

impl FrameKindInfo {
    pub fn new(kind: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            description: description.into(),
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, field: FieldInfo) -> Self {
        self.fields.push(field);
        self
    }
}

/// One command `encode` accepts and its fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    pub fields: Vec<FieldInfo>,
}

impl CommandInfo {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            fields: Vec::new(),
        }
    }

    pub fn field(mut self, field: FieldInfo) -> Self {
        self.fields.push(field);
        self
    }
}

/// A named, typed field of a frame kind or a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldInfo {
    pub name: String,
    pub ty: FieldType,
    pub description: String,
    /// A frame may leave it out; a command may be sent without it.
    pub optional: bool,
}

impl FieldInfo {
    pub fn new(name: impl Into<String>, ty: FieldType, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ty,
            description: description.into(),
            optional: false,
        }
    }

    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// The type of a field's [`Value`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FieldType {
    Bool,
    Int,
    UInt,
    Float,
    Str,
    Bytes,
    List,
}

impl FieldType {
    pub const ALL: [FieldType; 7] = [
        FieldType::Bool,
        FieldType::Int,
        FieldType::UInt,
        FieldType::Float,
        FieldType::Str,
        FieldType::Bytes,
        FieldType::List,
    ];

    /// The name plugins use: `bool`, `int`, `uint`, `float`, `str`, `bytes`, `list`.
    pub fn name(self) -> &'static str {
        match self {
            FieldType::Bool => "bool",
            FieldType::Int => "int",
            FieldType::UInt => "uint",
            FieldType::Float => "float",
            FieldType::Str => "str",
            FieldType::Bytes => "bytes",
            FieldType::List => "list",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.name() == name)
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How much a frame deserves attention.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Error,
}

impl Severity {
    /// `info`, `warning` or `error`.
    pub fn name(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "info" => Some(Severity::Info),
            "warning" => Some(Severity::Warning),
            "error" => Some(Severity::Error),
            _ => None,
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A field's value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
}

impl Value {
    pub fn ty(&self) -> FieldType {
        match self {
            Value::Bool(_) => FieldType::Bool,
            Value::Int(_) => FieldType::Int,
            Value::UInt(_) => FieldType::UInt,
            Value::Float(_) => FieldType::Float,
            Value::Str(_) => FieldType::Str,
            Value::Bytes(_) => FieldType::Bytes,
            Value::List(_) => FieldType::List,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// A `UInt`, or an `Int` that is not negative.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Value::UInt(n) => Some(*n),
            Value::Int(n) => u64::try_from(*n).ok(),
            _ => None,
        }
    }

    /// An `Int`, or a `UInt` that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::UInt(n) => i64::try_from(*n).ok(),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
}

/// Numbers in decimal, strings as they are, bytes as upper-case hex pairs, lists in
/// brackets: what a table cell shows.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(n) => write!(f, "{n}"),
            Value::UInt(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Str(s) => f.write_str(s),
            Value::Bytes(b) => f.write_str(&encode_hex(b, " ")),
            Value::List(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
        }
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::UInt(n)
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Value::Float(x)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_owned())
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(b)
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::Bytes(b.to_vec())
    }
}

impl From<Vec<Value>> for Value {
    fn from(items: Vec<Value>) -> Self {
        Value::List(items)
    }
}

/// One decoded frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    /// When the chunk that completed the frame arrived.
    pub at: Instant,
    /// `Rx` for frames decoded from received bytes.
    pub direction: Direction,
    /// One of the kinds in the codec's [`CodecInfo::kinds`], such as `response`.
    pub kind: SmolStr,
    /// Named values, in the order the kind declares them.
    pub fields: Vec<(SmolStr, Value)>,
    pub severity: Severity,
    /// The frame's bytes as stream offsets (the store's raw offsets), not a copy.
    pub raw: Range<u64>,
    /// A one-line description for a list or the terminal.
    pub summary: String,
}

impl Frame {
    /// A received (`Rx`), `Info` frame of `kind` with no fields and no summary.
    pub fn new(kind: impl Into<SmolStr>, raw: Range<u64>, at: Instant) -> Self {
        Self {
            at,
            direction: Direction::Rx,
            kind: kind.into(),
            fields: Vec::new(),
            severity: Severity::Info,
            raw,
            summary: String::new(),
        }
    }

    pub fn with_field(mut self, name: impl Into<SmolStr>, value: impl Into<Value>) -> Self {
        self.push_field(name, value);
        self
    }

    pub fn push_field(&mut self, name: impl Into<SmolStr>, value: impl Into<Value>) {
        self.fields.push((name.into(), value.into()));
    }

    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// The first field called `name`.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value)
    }

    /// Bytes the frame covers.
    pub fn raw_len(&self) -> u64 {
        self.raw.end - self.raw.start
    }
}

/// A command for [`Codec::encode`]: its name and JSON fields, as a saved command's
/// `{ "codec": …, "fields": { … } }` payload carries them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EncodeRequest {
    pub command: String,
    pub fields: Map<String, JsonValue>,
}

impl EncodeRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            fields: Map::new(),
        }
    }

    /// Set a field, for building a request in one expression.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<JsonValue>) -> Self {
        self.fields.insert(name.into(), value.into());
        self
    }

    /// A request from a saved command's `fields` object. A `command` key names the codec
    /// command and is not passed on as a field; without one the command is
    /// `default_command` (the codec's first, see [`CodecInfo::default_command`]).
    pub fn from_payload(
        fields: &Map<String, JsonValue>,
        default_command: &str,
    ) -> Result<Self, CodecError> {
        let mut fields = fields.clone();
        let command = match fields.remove("command") {
            None => default_command.to_owned(),
            Some(JsonValue::String(command)) => command,
            Some(_) => {
                return Err(CodecError::bad_field(
                    "command",
                    "must be a string naming one of the codec's commands",
                ));
            }
        };
        Ok(Self { command, fields })
    }

    pub fn field(&self, name: &str) -> Option<&JsonValue> {
        self.fields.get(name)
    }

    /// Fail with [`CodecError::BadField`] if any field is not in `allowed`. With several,
    /// the error names the alphabetically first, so the result never depends on the
    /// map's order.
    pub fn check_fields(&self, allowed: &[&str]) -> Result<(), CodecError> {
        let first = self
            .fields
            .keys()
            .filter(|key| !allowed.contains(&key.as_str()))
            .min();
        match first {
            None => Ok(()),
            Some(key) => Err(CodecError::bad_field(
                key.clone(),
                format!("not a field of `{}`", self.command),
            )),
        }
    }

    /// An unsigned integer field no larger than `max`: a JSON integer, or a string of hex
    /// digits with or without a `0x` prefix (`"0x0F15"`, `"0f15"`). `None` if absent.
    pub fn uint(&self, name: &str, max: u64) -> Result<Option<u64>, CodecError> {
        let Some(value) = self.fields.get(name) else {
            return Ok(None);
        };
        let n = json_uint(value).map_err(|reason| CodecError::bad_field(name, reason))?;
        if n > max {
            return Err(CodecError::bad_field(
                name,
                format!("{n:#X} is larger than {max:#X}"),
            ));
        }
        Ok(Some(n))
    }

    /// A bytes field: hex text as [`decode_hex`] reads it (`"05 5A 00"`), or a list of
    /// integers from 0 to 255. `None` if absent.
    pub fn bytes(&self, name: &str) -> Result<Option<Vec<u8>>, CodecError> {
        let Some(value) = self.fields.get(name) else {
            return Ok(None);
        };
        match value {
            JsonValue::String(text) => decode_hex(text)
                .map(Some)
                .map_err(|err| CodecError::bad_field(name, err.to_string())),
            JsonValue::Array(items) => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    item.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(|| {
                            CodecError::bad_field(
                                name,
                                format!("item {i} is not a byte (an integer from 0 to 255)"),
                            )
                        })
                })
                .collect::<Result<Vec<u8>, _>>()
                .map(Some),
            _ => Err(CodecError::bad_field(
                name,
                "must be hex text or a list of bytes",
            )),
        }
    }

    /// A string field. `None` if absent.
    pub fn str(&self, name: &str) -> Result<Option<&str>, CodecError> {
        match self.fields.get(name) {
            None => Ok(None),
            Some(JsonValue::String(s)) => Ok(Some(s)),
            Some(_) => Err(CodecError::bad_field(name, "must be a string")),
        }
    }
}

/// A JSON integer, or hex digits with an optional `0x` prefix.
fn json_uint(value: &JsonValue) -> Result<u64, String> {
    match value {
        JsonValue::Number(n) => n
            .as_u64()
            .ok_or_else(|| format!("{n} is not a non-negative integer")),
        JsonValue::String(s) => {
            parse_hex_uint(s).ok_or_else(|| format!("{s:?} is not hex digits (with or without 0x)"))
        }
        _ => Err("must be an integer or a hex string".to_owned()),
    }
}

/// `"0x0F15"`, `"0X0f15"` or `"0F15"` as a number. No sign, no spaces, at least one
/// digit; leading zeros are fine. `None` if it is not that or does not fit a `u64`.
pub fn parse_hex_uint(text: &str) -> Option<u64> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

/// Why hex text did not decode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HexError {
    #[error("{0:?} is not a hex digit")]
    BadDigit(char),
    #[error("{0:?} has an odd number of hex digits")]
    OddDigits(String),
}

/// Bytes as upper-case hex pairs joined by `separator`: `encode_hex(b"\x05\x5a", " ")`
/// is `"05 5A"`.
pub fn encode_hex(bytes: &[u8], separator: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * (2 + separator.len()));
    for (i, &byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// Hex text to bytes: runs of digit pairs separated by spaces, tabs, CR, LF or commas,
/// each run optionally prefixed `0x`, each run an even number of digits. `"05 5A 00"`,
/// `"055A00"` and `"0x05,0x5a,0x00"` all decode to `[0x05, 0x5A, 0x00]`. Only those
/// ASCII separators count, so plugins in other languages can match this exactly.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, HexError> {
    let mut out = Vec::with_capacity(text.len() / 2);
    for run in text.split([' ', '\t', '\r', '\n', ',']) {
        let digits = run
            .strip_prefix("0x")
            .or_else(|| run.strip_prefix("0X"))
            .unwrap_or(run);
        if let Some(bad) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(HexError::BadDigit(bad));
        }
        if digits.len() % 2 != 0 {
            return Err(HexError::OddDigits(run.to_owned()));
        }
        let (pairs, _) = digits.as_bytes().as_chunks::<2>();
        for &[hi, lo] in pairs {
            out.push(hex_digit(hi) << 4 | hex_digit(lo));
        }
    }
    Ok(out)
}

fn hex_digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(fields: JsonValue) -> EncodeRequest {
        let JsonValue::Object(fields) = fields else {
            panic!("an object");
        };
        EncodeRequest {
            command: "cmd".into(),
            fields,
        }
    }

    #[test]
    fn hex_round_trips_and_accepts_every_form() {
        let bytes = [0x05, 0x5A, 0x00, 0xFF];
        assert_eq!(encode_hex(&bytes, " "), "05 5A 00 FF");
        assert_eq!(encode_hex(&bytes, ""), "055A00FF");
        for text in [
            "05 5A 00 FF",
            "055A00ff",
            "0x05,0x5a, 0x00\n0XfF",
            "  05\t5A00 FF\r",
        ] {
            assert_eq!(decode_hex(text).unwrap(), bytes, "{text:?}");
        }
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert_eq!(decode_hex("05 5"), Err(HexError::OddDigits("5".into())));
        assert_eq!(decode_hex("0G"), Err(HexError::BadDigit('G')));
        // Only ASCII separators: a non-breaking space is not one.
        assert_eq!(decode_hex("05\u{a0}5A"), Err(HexError::BadDigit('\u{a0}')));
    }

    #[test]
    fn uint_fields_take_integers_and_hex_strings() {
        let r = request(json!({
            "a": 3861, "b": "0x0F15", "c": "0f15", "d": "0X0F15", "e": "000000000000000000000F15",
            "neg": -1, "float": 1.5, "big": 70000, "empty": "", "bare": "0x", "sign": "+1",
            "space": " 0F15", "bool": true,
        }));
        for name in ["a", "b", "c", "d", "e"] {
            assert_eq!(r.uint(name, 0xFFFF), Ok(Some(0x0F15)), "{name}");
        }
        assert_eq!(r.uint("missing", 0xFFFF), Ok(None));
        for name in [
            "neg", "float", "big", "empty", "bare", "sign", "space", "bool",
        ] {
            assert!(
                matches!(r.uint(name, 0xFFFF), Err(CodecError::BadField { ref field, .. }) if field == name),
                "{name}"
            );
        }
    }

    #[test]
    fn bytes_fields_take_hex_and_lists() {
        let r = request(json!({
            "hex": "01 02 ff", "list": [1, 2, 255], "none": [], "bad": [256], "neg": [-1],
            "odd": "123", "num": 5,
        }));
        assert_eq!(r.bytes("hex"), Ok(Some(vec![1, 2, 255])));
        assert_eq!(r.bytes("list"), Ok(Some(vec![1, 2, 255])));
        assert_eq!(r.bytes("none"), Ok(Some(vec![])));
        assert_eq!(r.bytes("missing"), Ok(None));
        for name in ["bad", "neg", "odd", "num"] {
            assert!(r.bytes(name).is_err(), "{name}");
        }
    }

    #[test]
    fn unknown_fields_are_reported_alphabetically_first() {
        let r = request(json!({ "zeta": 1, "alpha": 2, "ok": 3 }));
        assert_eq!(r.check_fields(&["ok", "alpha", "zeta"]), Ok(()));
        assert_eq!(
            r.check_fields(&["ok"]),
            Err(CodecError::bad_field("alpha", "not a field of `cmd`"))
        );
    }

    #[test]
    fn a_payload_names_its_command_or_takes_the_default() {
        let JsonValue::Object(fields) = json!({ "cmd_id": "0x0F15" }) else {
            unreachable!()
        };
        let r = EncodeRequest::from_payload(&fields, "race").unwrap();
        assert_eq!(r.command, "race");
        assert_eq!(r.fields.len(), 1);

        let JsonValue::Object(fields) = json!({ "command": "race_version" }) else {
            unreachable!()
        };
        let r = EncodeRequest::from_payload(&fields, "race").unwrap();
        assert_eq!(r.command, "race_version");
        assert!(r.fields.is_empty());

        let JsonValue::Object(fields) = json!({ "command": 1 }) else {
            unreachable!()
        };
        assert!(matches!(
            EncodeRequest::from_payload(&fields, "race"),
            Err(CodecError::BadField { field, .. }) if field == "command"
        ));
    }

    struct Nothing;

    impl Codec for Nothing {
        fn describe(&self) -> CodecInfo {
            CodecInfo {
                name: "nothing".into(),
                ..CodecInfo::default()
            }
        }
        fn decode(&mut self, _: &[u8], _: Instant, _: u64, _: &mut Vec<Frame>) {}
        fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
            Err(CodecError::UnknownCommand(request.command.clone()))
        }
        fn reset(&mut self) {}
    }

    #[test]
    fn the_registry_creates_by_name_and_later_registrations_win() {
        let mut registry = CodecRegistry::new();
        assert!(registry.is_empty());
        let info = Nothing.describe();
        assert!(
            registry
                .register_fn(info.clone(), || Ok(Box::new(Nothing)))
                .is_none()
        );
        assert!(
            registry
                .register_fn(info, || Ok(Box::new(Nothing)))
                .is_some()
        );
        assert_eq!(registry.names().collect::<Vec<_>>(), ["nothing"]);
        assert_eq!(registry.list()[0].name, "nothing");
        let mut codec = registry.create("nothing").unwrap();
        assert_eq!(
            codec.encode(&EncodeRequest::new("x")),
            Err(CodecError::UnknownCommand("x".into()))
        );
        assert!(matches!(
            registry.create("missing"),
            Err(CodecError::UnknownCodec(name)) if name == "missing"
        ));
        assert!(registry.unregister("nothing"));
        assert!(!registry.contains("nothing"));
    }

    #[test]
    fn frames_and_values() {
        let at = Instant::now();
        let frame = Frame::new("response", 10..16, at)
            .with_field("cmd_id", 0x0F15u64)
            .with_field("payload", vec![1u8, 2])
            .with_field("ok", true)
            .with_severity(Severity::Warning)
            .with_summary("hi");
        assert_eq!(frame.field("cmd_id").and_then(Value::as_u64), Some(0x0F15));
        assert_eq!(frame.field("payload").unwrap().to_string(), "01 02");
        assert_eq!(frame.raw_len(), 6);
        assert_eq!(frame.direction, Direction::Rx);
        assert_eq!(
            Value::List(vec![Value::Int(-1), "a".into()]).to_string(),
            "[-1, a]"
        );
        assert_eq!(Value::Int(5).as_u64(), Some(5));
        assert_eq!(Value::Int(-5).as_u64(), None);
        for ty in FieldType::ALL {
            assert_eq!(FieldType::from_name(ty.name()), Some(ty));
        }
        assert_eq!(Severity::from_name("error"), Some(Severity::Error));
        assert!(Severity::Error > Severity::Warning);
    }
}
