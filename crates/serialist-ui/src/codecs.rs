//! Codecs in the app: the registry the configuration keeps, how a session runs the codec
//! it selected on its ingest thread, and the frame helpers the session view, the Decoded
//! panel, saved commands and export share.
//!
//! # The registry and reloading
//!
//! [`CodecSet`] lives in [`Config`](crate::config::Config): the built-in codecs from
//! [`serialist_plugins::builtin_registry`], made once, plus one codec per plugin folder
//! under the config directory's `plugins/` (`plugins/<name>/plugin.lua`), registered under
//! the folder's name. A folder named like a built-in (`plugins/airoha-race/`) replaces
//! it; `plugins/airoha-race-lua/` sits beside it.
//!
//! A plugin is loaded from its text: the file is read once and a [`LuaCodecFactory`] made
//! from the source, so the factory is a snapshot of the plugin as it loaded. A later edit
//! that does not load (a syntax error, a `describe` that fails) is reported as a problem
//! and the last good version stays registered and keeps running, however many codecs it
//! still has to make. A reload keeps the factory (the same `Arc`) of every plugin whose
//! text did not change, and of every built-in, so a session can tell by pointer whether
//! a reload concerns the codec it runs.
//!
//! # Running a session's codec
//!
//! A session's ingest thread gets a [`CodecSlotSink`], built on that thread by
//! [`Ingest::spawn_with`](serialist_core::Ingest::spawn_with)'s closure. With no codec
//! selected (`none`) the slot holds no [`CodecSink`]: nothing decodes and nothing is
//! stored, and a chunk costs one atomic load. When the session view selects a codec it
//! puts the factory in the session's [`CodecSelection`] and bumps its generation. At the
//! next chunk the slot makes a [`CodecSink`] (once per session) over a factory whose codec
//! is a [`SwitchingCodec`]; that codec compares the selection's generation on every
//! chunk and, when it moved, makes a fresh codec from the selected factory, on the ingest
//! thread. So switching codecs, or a plugin reload the session view passes on (it
//! compares the registry's factory with the one it runs whenever the configuration
//! changes), takes effect at the next chunk; frames decoded so far stay in the session's
//! one [`FrameStore`](serialist_core::FrameStore), and the new codec starts from a clean
//! state at that chunk's offset.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::{Map, Value as JsonValue};
use serialist_core::codec::{encode_hex, parse_hex_uint};
use serialist_core::frames::CODEC_ERROR_KIND;
use serialist_core::{
    ChunkSink, Codec, CodecError, CodecFactory, CodecInfo, CodecRegistry, CodecSink, Command,
    EncodeRequest, FieldType, Frame, FrameStore, ParamKind, ParamValues, Payload, Severity, Value,
};
use serialist_plugins::{LuaCodecFactory, LuaLimits, PLUGIN_ERROR_KIND, PluginKind, find_plugins};

use crate::terminal::{Clock, TimestampMode};

/// What the codec picker calls "no codec".
pub const NO_CODEC: &str = "none";

/// Starts every one-line summary of a decoded frame in the scrollback, so the terminal
/// can draw it in the plugin color (see [`is_decoded_summary`]).
pub const DECODED_MARK: &str = "\u{25B8} ";

// --- The registry ------------------------------------------------------------------------

/// A codec loaded from a plugin folder.
#[derive(Clone)]
pub struct PluginCodec {
    /// The folder's name, which the codec is registered under.
    pub name: String,
    /// The entry file, such as `plugins/airoha-race/plugin.lua`.
    pub entry: PathBuf,
    /// The name the plugin describes itself by, which may differ from the folder's.
    pub described: String,
    /// The text it was loaded from, to tell an edit from a save that changed nothing.
    source: Arc<str>,
    pub factory: Arc<dyn CodecFactory>,
}

impl std::fmt::Debug for PluginCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginCodec")
            .field("name", &self.name)
            .field("entry", &self.entry)
            .field("described", &self.described)
            .finish_non_exhaustive()
    }
}

/// A plugin that did not load, or did not reload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginProblem {
    pub path: PathBuf,
    pub message: String,
    /// An older version of the plugin loaded earlier is still in use.
    pub kept_previous: bool,
}

impl std::fmt::Display for PluginProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Plugin {} not loaded: {}",
            self.path.display(),
            self.message
        )?;
        if self.kept_previous {
            f.write_str(" (the last version that loaded is still in use)")?;
        }
        Ok(())
    }
}

/// The codecs the app knows: the built-ins and the plugins. Cheap to clone.
#[derive(Clone)]
pub struct CodecSet {
    builtins: CodecRegistry,
    plugins: Vec<PluginCodec>,
    registry: Arc<CodecRegistry>,
}

impl Default for CodecSet {
    fn default() -> Self {
        Self::builtin()
    }
}

impl std::fmt::Debug for CodecSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodecSet")
            .field("registry", &self.registry)
            .field("plugins", &self.plugins)
            .finish()
    }
}

impl CodecSet {
    /// The built-in codecs alone.
    pub fn builtin() -> Self {
        let builtins = serialist_plugins::builtin_registry();
        Self {
            registry: Arc::new(builtins.clone()),
            builtins,
            plugins: Vec::new(),
        }
    }

    /// Every codec by name: the built-ins, with the plugins over them.
    pub fn registry(&self) -> &Arc<CodecRegistry> {
        &self.registry
    }

    /// The plugins loaded, sorted by name.
    pub fn plugins(&self) -> &[PluginCodec] {
        &self.plugins
    }

    pub fn is_plugin(&self, name: &str) -> bool {
        self.plugins.iter().any(|plugin| plugin.name == name)
    }

    /// What the codec picker lists: [`NO_CODEC`], then every codec, sorted.
    pub fn choices(&self) -> Vec<String> {
        std::iter::once(NO_CODEC.to_owned())
            .chain(self.registry.names().map(str::to_owned))
            .collect()
    }

    /// Load the plugin folders under `root` again (see the module docs). A folder that
    /// is gone drops its codec; one that does not load keeps its last good version.
    /// Never fails: what went wrong comes back as problems.
    pub fn reload_plugins(&mut self, root: &Path, limits: LuaLimits) -> Vec<PluginProblem> {
        let mut plugins = Vec::new();
        let mut problems = Vec::new();
        for dir in find_plugins(root) {
            let name = dir
                .dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let previous = self.plugins.iter().find(|plugin| plugin.name == name);
            let mut fail = |message: String| {
                problems.push(PluginProblem {
                    path: dir.entry.clone(),
                    message,
                    kept_previous: previous.is_some(),
                });
                previous.cloned()
            };
            let loaded = match dir.kind {
                PluginKind::Lua => match fs::read_to_string(&dir.entry) {
                    Err(error) => fail(error.to_string()),
                    Ok(text) => match previous {
                        Some(previous) if *previous.source == *text => Some(previous.clone()),
                        _ => {
                            let origin = dir.entry.display().to_string();
                            match LuaCodecFactory::from_source(&origin, &text, limits) {
                                Ok(factory) => {
                                    let described = factory.info().name;
                                    tracing::info!(plugin = %name, %described, "plugin loaded");
                                    Some(PluginCodec {
                                        factory: Arc::new(NamedFactory {
                                            name: name.clone(),
                                            inner: Arc::new(factory),
                                        }),
                                        name: name.clone(),
                                        entry: dir.entry.clone(),
                                        described,
                                        source: Arc::from(text),
                                    })
                                }
                                Err(error) => fail(error.to_string()),
                            }
                        }
                    },
                },
                // WebAssembly plugins are loaded by `serialist_plugins::load_plugins`, which
                // registers under the described name; this build keys plugins by folder
                // and has no per-folder loader for them.
                _ => fail("WebAssembly plugins are not supported by this build yet".to_owned()),
            };
            plugins.extend(loaded);
        }
        let mut registry = self.builtins.clone();
        for plugin in &plugins {
            registry.register(plugin.factory.clone());
        }
        self.plugins = plugins;
        self.registry = Arc::new(registry);
        problems
    }
}

/// A factory registered under a name of the app's choosing (the plugin's folder).
struct NamedFactory {
    name: String,
    inner: Arc<dyn CodecFactory>,
}

impl CodecFactory for NamedFactory {
    fn info(&self) -> CodecInfo {
        let mut info = self.inner.info();
        info.name.clone_from(&self.name);
        info
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        self.inner.create()
    }
}

// --- A session's codec -------------------------------------------------------------------

/// Which codec a session decodes with, shared by the session view (which sets it) and
/// the ingest thread's [`CodecSlotSink`] (which reads it once per chunk).
#[derive(Default)]
pub struct CodecSelection {
    /// Bumped by every [`set`](Self::set); the fast path the ingest thread checks.
    generation: AtomicU64,
    selected: AtomicBool,
    current: Mutex<(u64, Option<Arc<dyn CodecFactory>>)>,
}

impl std::fmt::Debug for CodecSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodecSelection")
            .field("generation", &self.generation.load(Ordering::Relaxed))
            .field("selected", &self.selected.load(Ordering::Relaxed))
            .finish()
    }
}

impl CodecSelection {
    /// Decode with a fresh codec from `factory` from the next chunk on, or stop decoding.
    pub fn set(&self, factory: Option<Arc<dyn CodecFactory>>) {
        let mut current = self.current.lock();
        current.0 += 1;
        self.selected.store(factory.is_some(), Ordering::Release);
        current.1 = factory;
        self.generation.store(current.0, Ordering::Release);
    }

    /// The factory selected now.
    pub fn factory(&self) -> Option<Arc<dyn CodecFactory>> {
        self.current.lock().1.clone()
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn is_selected(&self) -> bool {
        self.selected.load(Ordering::Acquire)
    }

    /// The generation and its factory, read together.
    fn snapshot(&self) -> (u64, Option<Arc<dyn CodecFactory>>) {
        self.current.lock().clone()
    }
}

/// The factory the session's [`CodecSink`] is made with: its codec follows the selection.
struct SwitchingFactory(Arc<CodecSelection>);

impl CodecFactory for SwitchingFactory {
    fn info(&self) -> CodecInfo {
        self.0
            .factory()
            .map(|factory| factory.info())
            .unwrap_or_default()
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        Ok(Box::new(SwitchingCodec {
            selection: self.0.clone(),
            seen: None,
            inner: None,
        }))
    }
}

/// A codec that is whatever the session's [`CodecSelection`] says, made anew on the
/// ingest thread at the first chunk after each change (see the module docs).
pub struct SwitchingCodec {
    selection: Arc<CodecSelection>,
    /// The generation `inner` was made for.
    seen: Option<u64>,
    inner: Option<Box<dyn Codec>>,
}

impl Codec for SwitchingCodec {
    fn describe(&self) -> CodecInfo {
        self.inner
            .as_ref()
            .map(|codec| codec.describe())
            .unwrap_or_default()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        if self.seen != Some(self.selection.generation()) {
            let (generation, factory) = self.selection.snapshot();
            self.seen = Some(generation);
            self.inner = None;
            if let Some(factory) = factory {
                let name = factory.info().name;
                match factory.create() {
                    Ok(codec) => {
                        tracing::debug!(codec = %name, raw_offset, "decoding with a fresh codec");
                        self.inner = Some(codec);
                    }
                    Err(error) => {
                        tracing::error!(codec = %name, %error, "the codec could not be made");
                        let raw = raw_offset..raw_offset + chunk.len() as u64;
                        out.push(
                            Frame::new(CODEC_ERROR_KIND, raw, at)
                                .with_severity(Severity::Error)
                                .with_summary(format!("{name} could not be made: {error}"))
                                .with_field("error", error.to_string()),
                        );
                    }
                }
            }
        }
        if let Some(codec) = &mut self.inner {
            codec.decode(chunk, at, raw_offset, out);
        }
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        match &mut self.inner {
            Some(codec) => codec.encode(request),
            None => Err(CodecError::Internal("no codec is selected".to_owned())),
        }
    }

    fn reset(&mut self) {
        if let Some(codec) = &mut self.inner {
            codec.reset();
        }
    }
}

/// The session's decoding, on its ingest thread. Holds no [`CodecSink`] until a codec is
/// first selected; see the module docs.
pub struct CodecSlotSink {
    selection: Arc<CodecSelection>,
    /// The store and waker the sink is made with, until it is.
    parts: Option<(FrameStore, Box<dyn Fn() + Send>)>,
    sink: Option<CodecSink>,
    /// Stream offset of the next chunk.
    offset: u64,
}

impl CodecSlotSink {
    /// Frames go to `store`, and `waker` is called as [`CodecSink`] calls it.
    pub fn new(
        selection: Arc<CodecSelection>,
        store: FrameStore,
        waker: Box<dyn Fn() + Send>,
    ) -> Self {
        Self {
            selection,
            parts: Some((store, waker)),
            sink: None,
            offset: 0,
        }
    }
}

impl ChunkSink for CodecSlotSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        if self.sink.is_none()
            && self.selection.is_selected()
            && let Some((store, waker)) = self.parts.take()
        {
            let factory: Arc<dyn CodecFactory> = Arc::new(SwitchingFactory(self.selection.clone()));
            self.sink =
                Some(CodecSink::from_factory(factory, store, Some(waker)).starting_at(self.offset));
        }
        self.offset += bytes.len() as u64;
        if let Some(sink) = &mut self.sink {
            sink.on_chunk(bytes, at);
        }
    }

    fn on_disconnect(&mut self) {
        if let Some(sink) = &mut self.sink {
            sink.on_disconnect();
        }
    }
}

// --- Frames ------------------------------------------------------------------------------

/// Whether `frame` is text between frames rather than a decoded frame: its kind declares
/// a single `text` field (RACE's `text`, text-lines' `line`), or, for a kind the codec
/// does not declare, is called `text`.
pub fn is_text_frame(frame: &Frame, info: Option<&CodecInfo>) -> bool {
    match info.and_then(|info| info.kind(&frame.kind)) {
        Some(kind) => {
            kind.fields.len() == 1
                && kind.fields[0].name == "text"
                && kind.fields[0].ty == FieldType::Str
        }
        None => frame.kind == "text",
    }
}

/// Whether `frame` reports a codec or plugin failure.
pub fn is_error_frame(frame: &Frame) -> bool {
    frame.kind == CODEC_ERROR_KIND || frame.kind == PLUGIN_ERROR_KIND
}

/// Whether the bytes of `frame` can be hidden from the text view: a decoded binary frame,
/// not text and not a failure (whose bytes are worth seeing).
pub fn hides_bytes(frame: &Frame, info: Option<&CodecInfo>) -> bool {
    !frame.raw.is_empty() && !is_text_frame(frame, info) && !is_error_frame(frame)
}

/// The scrollback line for `frame`: [`DECODED_MARK`], then its summary (or its kind).
pub fn inline_summary(frame: &Frame) -> String {
    let text = if frame.summary.is_empty() {
        frame.kind.to_string()
    } else {
        frame.summary.clone()
    };
    format!("{DECODED_MARK}{}", text.lines().next().unwrap_or_default())
}

/// Whether a scrollback line is a decoded frame's summary.
pub fn is_decoded_summary(text: &str) -> bool {
    text.starts_with(DECODED_MARK)
}

/// How the Decoded panel and a decoded export stamp a frame: the session's timestamp
/// mode and `display.timestamp_format`, with absolute time while the gutter is off, so a
/// frame always has a time.
#[derive(Clone, Debug)]
pub struct FrameTime {
    pub clock: Clock,
    pub mode: TimestampMode,
    /// The `strftime` format of absolute stamps; `None` for the default.
    pub format: Option<String>,
}

impl FrameTime {
    /// The stamp of a frame completed `at`, whose predecessor (for delta stamps) was
    /// completed at `previous`.
    pub fn stamp(&self, at: Instant, previous: Option<Instant>) -> String {
        let mode = match self.mode {
            TimestampMode::Off => TimestampMode::Absolute,
            mode => mode,
        };
        self.clock
            .format(mode, self.format.as_deref(), at, previous)
            .unwrap_or_default()
    }
}

/// `RX`, `TX` or `--`.
pub fn direction_label(frame: &Frame) -> &'static str {
    match frame.direction {
        serialist_core::Direction::Rx => "RX",
        serialist_core::Direction::Tx => "TX",
        serialist_core::Direction::Notice => "--",
    }
}

/// A field's value as JSON: numbers as numbers, bytes as hex text.
pub fn value_json(value: &Value) -> JsonValue {
    match value {
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(n) => JsonValue::from(*n),
        Value::UInt(n) => JsonValue::from(*n),
        Value::Float(x) => {
            serde_json::Number::from_f64(*x).map_or(JsonValue::Null, JsonValue::Number)
        }
        Value::Str(s) => JsonValue::String(s.clone()),
        Value::Bytes(b) => JsonValue::String(encode_hex(b, "")),
        Value::List(items) => JsonValue::Array(items.iter().map(value_json).collect()),
    }
}

/// Whether `frame` is what `predicate` asks for: its `kind` key names the frame's kind,
/// and every other key a field of the frame with an equal value (see [`loose_eq`]).
pub fn frame_matches(predicate: &Map<String, JsonValue>, frame: &Frame) -> bool {
    predicate.iter().all(|(key, expected)| {
        if key == "kind" {
            return expected.as_str() == Some(frame.kind.as_str());
        }
        frame
            .field(key)
            .is_some_and(|actual| loose_eq(expected, actual))
    })
}

/// A predicate's JSON value against a frame's field, compared the way a codec reads its
/// request fields: an integer field matches a JSON integer or hex digits (`"0x0F15"` or
/// `"0F15"`), a bytes field matches hex text or a list of bytes, a string matches the
/// same string, a bool the same bool.
pub fn loose_eq(expected: &JsonValue, actual: &Value) -> bool {
    match (expected, actual) {
        (JsonValue::Bool(e), Value::Bool(a)) => e == a,
        (JsonValue::Number(e), Value::UInt(a)) => e.as_u64() == Some(*a),
        (JsonValue::Number(e), Value::Int(a)) => e.as_i64() == Some(*a),
        (JsonValue::Number(e), Value::Float(a)) => e.as_f64() == Some(*a),
        (JsonValue::String(e), Value::UInt(a)) => parse_hex_uint(e.trim()) == Some(*a),
        (JsonValue::String(e), Value::Int(a)) => {
            parse_hex_uint(e.trim()).and_then(|n| i64::try_from(n).ok()) == Some(*a)
        }
        (JsonValue::String(e), Value::Str(a)) => e == a,
        (JsonValue::String(e), Value::Bytes(a)) => {
            serialist_core::codec::decode_hex(e).is_ok_and(|bytes| bytes == *a)
        }
        (JsonValue::Array(e), Value::Bytes(a)) => {
            e.len() == a.len()
                && e.iter()
                    .zip(a)
                    .all(|(e, a)| e.as_u64() == Some(u64::from(*a)))
        }
        (JsonValue::Array(e), Value::List(a)) => {
            e.len() == a.len() && e.iter().zip(a).all(|(e, a)| loose_eq(e, a))
        }
        _ => false,
    }
}

// --- Saved commands ----------------------------------------------------------------------

/// The bytes for a saved command's `{ "codec": …, "fields": … }` payload: placeholders
/// filled from `params` (see [`fill_placeholders`]), then encoded by the codec it names
/// in `registry`, then the command's own `eol` if it has one (a codec payload has no line
/// ending otherwise). The error is a message for the status line.
pub fn encode_codec_command(
    registry: &CodecRegistry,
    command: &Command,
    params: &ParamValues,
) -> Result<Vec<u8>, String> {
    let Payload::Codec { codec, fields } = &command.payload else {
        return Err("not a codec payload".to_owned());
    };
    let fields = fill_placeholders(fields, command, params)?;
    let mut bytes = serialist_plugins::encode_payload(registry, codec, &fields)
        .map_err(|error| error.to_string())?;
    if let Some(eol) = command.eol {
        bytes.extend_from_slice(eol.bytes());
    }
    Ok(bytes)
}

/// `fields` with every `{{name}}` in its strings replaced by the value of the command's
/// parameter `name` (from `params`, else its default). A string that is one placeholder
/// and nothing else takes the parameter's type: an `int` becomes a JSON number, a `hex16`
/// the text `0xNNNN`, a `text` the text. Inside a longer string an `int` is written in
/// decimal and a `hex16` as four hex digits, as in a text payload. `\{{` is a literal
/// `{{`.
///
/// This is the saved-command crate's placeholder rule for text payloads; the crate keeps
/// its expansion private, so codec payloads (and frame predicates) are filled here.
pub fn fill_placeholders(
    fields: &Map<String, JsonValue>,
    command: &Command,
    params: &ParamValues,
) -> Result<Map<String, JsonValue>, String> {
    fields
        .iter()
        .map(|(key, value)| Ok((key.clone(), fill_value(value, command, params)?)))
        .collect()
}

fn fill_value(
    value: &JsonValue,
    command: &Command,
    params: &ParamValues,
) -> Result<JsonValue, String> {
    Ok(match value {
        JsonValue::String(text) => fill_string(text, command, params)?,
        JsonValue::Array(items) => JsonValue::Array(
            items
                .iter()
                .map(|item| fill_value(item, command, params))
                .collect::<Result<_, _>>()?,
        ),
        JsonValue::Object(map) => JsonValue::Object(fill_placeholders(map, command, params)?),
        other => other.clone(),
    })
}

/// One parameter's value, checked: its kind and the text the user gave (or the default).
fn param_value<'a>(
    command: &'a Command,
    params: &'a ParamValues,
    name: &str,
) -> Result<(ParamKind, &'a str), String> {
    let param = command
        .params
        .iter()
        .find(|param| param.name == name)
        .ok_or_else(|| format!("`{{{{{name}}}}}` does not name a parameter of this command"))?;
    let value = params
        .get(name)
        .or(param.default.as_deref())
        .ok_or_else(|| format!("no value for parameter `{name}`, and it has no default"))?;
    param.validate(value).map_err(|error| error.to_string())?;
    Ok((param.kind, value))
}

fn parse_int(text: &str) -> Option<i64> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let magnitude = match digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        Some(hex) => i64::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<i64>().ok()?,
    };
    Some(if negative { -magnitude } else { magnitude })
}

fn parse_hex16(text: &str) -> Option<u16> {
    u16::try_from(parse_hex_uint(text.trim())?).ok()
}

fn fill_string(text: &str, command: &Command, params: &ParamValues) -> Result<JsonValue, String> {
    // A string that is one placeholder takes the parameter's type.
    if let Some(name) = text
        .strip_prefix("{{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .map(str::trim)
        .filter(|name| !name.contains("{{") && !name.contains("}}"))
    {
        let (kind, value) = param_value(command, params, name)?;
        return Ok(match kind {
            ParamKind::Text => JsonValue::String(value.to_owned()),
            ParamKind::Int => JsonValue::from(parse_int(value).unwrap_or_default()),
            ParamKind::Hex16 => {
                JsonValue::String(format!("0x{:04X}", parse_hex16(value).unwrap_or_default()))
            }
        });
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("{{") {
        if rest[..at].ends_with('\\') {
            out.push_str(&rest[..at - 1]);
            out.push_str("{{");
            rest = &rest[at + 2..];
            continue;
        }
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| "`{{` is never closed by `}}`".to_owned())?;
        let name = after[..end].trim();
        let (kind, value) = param_value(command, params, name)?;
        match kind {
            ParamKind::Text => out.push_str(value),
            ParamKind::Int => out.push_str(&parse_int(value).unwrap_or_default().to_string()),
            ParamKind::Hex16 => {
                out.push_str(&format!("{:04X}", parse_hex16(value).unwrap_or_default()))
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(JsonValue::String(out))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use serialist_core::{FrameStore, Param};

    use super::*;

    fn object(value: JsonValue) -> Map<String, JsonValue> {
        match value {
            JsonValue::Object(map) => map,
            _ => panic!("an object"),
        }
    }

    fn codec_command(fields: JsonValue) -> Command {
        Command::new(
            "c",
            Payload::Codec {
                codec: "airoha-race".into(),
                fields: object(fields),
            },
        )
    }

    #[test]
    fn codec_payloads_encode_with_their_placeholders_filled() {
        let registry = serialist_plugins::builtin_registry();
        let version = codec_command(json!({ "command": "race_version" }));
        assert_eq!(
            encode_codec_command(&registry, &version, &ParamValues::new()),
            Ok(vec![0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F])
        );
        let by_id = codec_command(json!({ "cmd_id": "{{id}}", "payload": "AA {{n}}" }))
            .with_param(Param::new("id", ParamKind::Hex16).with_default("0x0F15"))
            .with_param(Param::new("n", ParamKind::Text).with_default("BB"));
        assert_eq!(
            encode_codec_command(&registry, &by_id, &ParamValues::new()),
            Ok(vec![0x05, 0x5A, 0x04, 0x00, 0x15, 0x0F, 0xAA, 0xBB])
        );
        assert_eq!(
            encode_codec_command(&registry, &by_id, &ParamValues::new().with("id", "1234")),
            Ok(vec![0x05, 0x5A, 0x04, 0x00, 0x34, 0x12, 0xAA, 0xBB])
        );
        let filled = fill_placeholders(
            &object(json!({ "a": "{{id}}", "b": ["x{{id}}y", { "c": "{{n}}" }], "d": 7 })),
            &by_id,
            &ParamValues::new(),
        )
        .unwrap();
        assert_eq!(
            JsonValue::Object(filled),
            json!({ "a": "0x0F15", "b": ["x0F15y", { "c": "BB" }], "d": 7 })
        );
        let int = codec_command(json!({ "n": "{{n}}", "s": "n={{n}}" }))
            .with_param(Param::new("n", ParamKind::Int));
        assert_eq!(
            JsonValue::Object(
                fill_placeholders(
                    &object(json!({ "n": "{{n}}", "s": "n={{n}}" })),
                    &int,
                    &ParamValues::new().with("n", "0x10")
                )
                .unwrap()
            ),
            json!({ "n": 16, "s": "n=16" })
        );
        // Errors name what is wrong.
        let ghost = codec_command(json!({ "cmd_id": "{{ghost}}" }));
        assert!(
            encode_codec_command(&registry, &ghost, &ParamValues::new())
                .unwrap_err()
                .contains("ghost")
        );
        let unknown = Command::new(
            "u",
            Payload::Codec {
                codec: "nope".into(),
                fields: Map::new(),
            },
        );
        assert_eq!(
            encode_codec_command(&registry, &unknown, &ParamValues::new()),
            Err("no codec named `nope` is loaded".to_owned())
        );
    }

    #[test]
    fn predicates_compare_the_way_codecs_read_fields() {
        let at = Instant::now();
        let frame = Frame::new("response", 0..10, at)
            .with_field("cmd_id", 0x0F15u64)
            .with_field("payload", vec![0x00u8, 0x41])
            .with_field("text", "hi");
        let matches = |predicate: JsonValue| frame_matches(&object(predicate), &frame);
        assert!(matches(json!({ "kind": "response", "cmd_id": "0x0F15" })));
        assert!(matches(json!({ "cmd_id": "0f15" })));
        assert!(matches(json!({ "cmd_id": 3861 })));
        assert!(matches(json!({ "payload": "00 41", "text": "hi" })));
        assert!(matches(json!({ "payload": [0, 65] })));
        assert!(!matches(json!({ "kind": "log" })));
        assert!(!matches(json!({ "cmd_id": "0x0F40" })));
        assert!(!matches(json!({ "missing": 1 })));
    }

    #[test]
    fn text_and_error_frames_are_told_apart_from_binary_ones() {
        let info = serialist_plugins::race::race_info();
        let at = Instant::now();
        let text = Frame::new("text", 0..5, at).with_field("text", "hello");
        let log = Frame::new("log", 5..20, at).with_summary("log 0x0F40 len 9");
        let failed = Frame::new(PLUGIN_ERROR_KIND, 20..30, at);
        assert!(is_text_frame(&text, Some(&info)));
        assert!(!is_text_frame(&log, Some(&info)));
        assert!(hides_bytes(&log, Some(&info)));
        assert!(!hides_bytes(&text, Some(&info)));
        assert!(!hides_bytes(&failed, Some(&info)));
        let lines = serialist_plugins::TextLines::info();
        assert!(is_text_frame(&Frame::new("line", 0..3, at), Some(&lines)));
        assert_eq!(inline_summary(&log), "\u{25B8} log 0x0F40 len 9");
        assert!(is_decoded_summary(&inline_summary(&Frame::new(
            "x",
            0..0,
            at
        ))));
    }

    fn race_chunk(ty: u8, cmd_id: u16, payload: &[u8]) -> Vec<u8> {
        serialist_plugins::race::encode_frame(
            serialist_plugins::race::RaceType::ALL
                .into_iter()
                .find(|t| t.byte() == ty)
                .unwrap(),
            cmd_id,
            payload,
        )
        .unwrap()
    }

    #[test]
    fn the_slot_decodes_only_once_selected_and_switches_at_the_next_chunk() {
        let selection = Arc::new(CodecSelection::default());
        let store = FrameStore::default();
        let reader = store.reader();
        let wakes = Arc::new(AtomicU64::new(0));
        let counter = wakes.clone();
        let mut slot = CodecSlotSink::new(
            selection.clone(),
            store,
            Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let at = Instant::now();
        let log = race_chunk(0x5D, 0x0F40, b"boot");
        // Nothing selected: nothing decodes, but offsets count.
        slot.on_chunk(&log, at);
        assert!(reader.snapshot().is_empty());

        let registry = serialist_plugins::builtin_registry();
        selection.set(registry.get("airoha-race"));
        slot.on_chunk(&log, at + Duration::from_millis(1));
        let snap = reader.snapshot();
        assert_eq!(snap.count(), 1);
        let frame = snap.last().unwrap();
        assert_eq!(frame.kind, "log");
        assert_eq!(
            frame.raw,
            log.len() as u64..2 * log.len() as u64,
            "offsets from the start"
        );
        assert_eq!(wakes.load(Ordering::Relaxed), 1);

        // Half a frame, then a switch: the new codec starts clean at the next chunk.
        slot.on_chunk(&log[..3], at);
        selection.set(registry.get("text-lines"));
        reader.acknowledge();
        slot.on_chunk(b"hello\n", at);
        let snap = reader.snapshot();
        assert_eq!(snap.count(), 2, "frames so far stay");
        let line = snap.last().unwrap();
        assert_eq!(line.kind, "line");
        let start = 2 * log.len() as u64 + 3;
        assert_eq!(line.raw, start..start + 6);

        // None again: decoding stops, the store keeps what it has.
        selection.set(None);
        slot.on_chunk(b"more\n", at);
        assert_eq!(reader.snapshot().count(), 2);
        slot.on_disconnect();
    }

    #[test]
    fn a_codec_that_cannot_be_made_is_an_error_frame_until_the_next_switch() {
        struct Broken;
        impl CodecFactory for Broken {
            fn info(&self) -> CodecInfo {
                CodecInfo {
                    name: "broken".into(),
                    ..CodecInfo::default()
                }
            }
            fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
                Err(CodecError::Internal("no VM".into()))
            }
        }
        let selection = Arc::new(CodecSelection::default());
        let store = FrameStore::default();
        let reader = store.reader();
        let mut slot = CodecSlotSink::new(selection.clone(), store, Box::new(|| {}));
        selection.set(Some(Arc::new(Broken)));
        slot.on_chunk(b"abc", Instant::now());
        slot.on_chunk(b"def", Instant::now());
        let snap = reader.snapshot();
        assert_eq!(snap.count(), 1, "one error, then nothing");
        let frame = snap.last().unwrap();
        assert_eq!(frame.kind, CODEC_ERROR_KIND);
        assert_eq!(frame.severity, Severity::Error);
        assert!(frame.summary.contains("broken could not be made"));
        selection.set(serialist_plugins::builtin_registry().get("text-lines"));
        slot.on_chunk(b"ok\n", Instant::now());
        assert_eq!(reader.snapshot().last().unwrap().kind, "line");
    }

    #[test]
    fn plugins_load_by_folder_keep_their_last_good_version_and_their_factory() {
        let dir = crate::test_support::TestDir::new("codec-set");
        let root = dir.join("plugins");
        let lua = root.join("race-lua");
        fs::create_dir_all(&lua).unwrap();
        fs::write(lua.join("plugin.lua"), serialist_plugins::AIROHA_RACE_LUA).unwrap();
        let mut set = CodecSet::builtin();
        let builtin = set.registry().get("airoha-race").unwrap();
        assert!(set.reload_plugins(&root, LuaLimits::default()).is_empty());
        assert_eq!(
            set.choices(),
            ["none", "airoha-race", "race-lua", "text-lines"]
        );
        let plugin = set.registry().get("race-lua").unwrap();
        assert_eq!(plugin.info().name, "race-lua", "registered by folder");
        assert_eq!(set.plugins()[0].described, "airoha-race");
        assert!(set.is_plugin("race-lua"));

        // A reload that changed nothing keeps every factory.
        assert!(set.reload_plugins(&root, LuaLimits::default()).is_empty());
        assert!(Arc::ptr_eq(
            &plugin,
            &set.registry().get("race-lua").unwrap()
        ));
        assert!(Arc::ptr_eq(
            &builtin,
            &set.registry().get("airoha-race").unwrap()
        ));

        // A broken edit is a problem, and the last good version stays, and still works.
        fs::write(lua.join("plugin.lua"), "return {").unwrap();
        let problems = set.reload_plugins(&root, LuaLimits::default());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].kept_previous);
        assert!(
            problems[0].to_string().contains("still in use"),
            "{}",
            problems[0]
        );
        let kept = set.registry().get("race-lua").unwrap();
        assert!(Arc::ptr_eq(&plugin, &kept));
        assert!(kept.create().is_ok(), "made from the text it loaded");

        // Going back to the text that loaded changes nothing.
        fs::write(lua.join("plugin.lua"), serialist_plugins::AIROHA_RACE_LUA).unwrap();
        assert!(set.reload_plugins(&root, LuaLimits::default()).is_empty());
        assert!(Arc::ptr_eq(
            &plugin,
            &set.registry().get("race-lua").unwrap()
        ));

        // A good edit replaces it; a folder named like a built-in replaces the built-in.
        let edited = serialist_plugins::AIROHA_RACE_LUA.replace("-- describe", "-- edited");
        assert_ne!(edited, serialist_plugins::AIROHA_RACE_LUA);
        fs::write(lua.join("plugin.lua"), edited).unwrap();
        let shadow = root.join("text-lines");
        fs::create_dir_all(&shadow).unwrap();
        fs::write(
            shadow.join("plugin.lua"),
            serialist_plugins::AIROHA_RACE_LUA,
        )
        .unwrap();
        assert!(set.reload_plugins(&root, LuaLimits::default()).is_empty());
        assert!(!Arc::ptr_eq(
            &plugin,
            &set.registry().get("race-lua").unwrap()
        ));
        assert_eq!(
            set.registry().get("text-lines").unwrap().info().kinds.len(),
            6
        );

        // A folder that goes takes its codec with it.
        fs::remove_dir_all(&shadow).unwrap();
        set.reload_plugins(&root, LuaLimits::default());
        assert_eq!(
            set.registry().get("text-lines").unwrap().info().kinds.len(),
            1,
            "the built-in is back"
        );
    }
}
