//! Reading settings files: JSONC, layering and validation.
//!
//! A layer is applied one leaf key at a time. Each leaf (`line_ending`, `display.wrap`,
//! `theme.mode`, one tag of `buffer_font_features`, the whole `devices` list) is merged
//! onto what the layers below produced and the result is read as [`Settings`]; a leaf
//! that does not read is left out with a [`SettingsWarning`], so the value from the layer
//! below stays in force and the rest of the file still applies. A file written for a
//! later release, with a value this build does not know, so loses only that key.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use jsonc_parser::common::Ranged;
use jsonc_parser::{CollectOptions, ParseOptions, ast};
use serde::Deserialize;
use serde_json::{Map, Value};

use super::defaults;
use super::profile::{DeviceMatch, DeviceProfile};
use super::types::Settings;

/// Why settings could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// A file that is not valid JSONC, or whose root is not an object. `line` and
    /// `column` count from 1; `message` is the parser's own text. A value of the wrong
    /// type is not an error but a [`SettingsWarning`].
    #[error("{}:{line}:{column}: {message}", file.display())]
    Invalid {
        file: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    /// The merged layers do not fit the settings types, though each file parsed. Not
    /// expected in practice because every key is checked against the layers below it.
    #[error("merged settings are invalid: {0}")]
    Merged(String),
}

impl SettingsError {
    /// The file the problem is in, when there is one.
    pub fn file(&self) -> Option<&Path> {
        match self {
            SettingsError::Io { path, .. } => Some(path),
            SettingsError::Invalid { file, .. } => Some(file),
            SettingsError::Merged(_) => None,
        }
    }
}

/// What a [`SettingsWarning`] is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsWarningKind {
    /// A key this build does not know. It is ignored.
    UnknownKey,
    /// A value this build cannot read: an enum value it does not know, a number out of
    /// range, a wrong type. The key is left out of its file, so the value from the layer
    /// below (or the bundled default) stays in force. A device profile without a
    /// readable `match` is left out whole. `reason` is serde's or the validator's text.
    InvalidValue { reason: String },
}

/// A problem that did not stop the load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsWarning {
    pub file: PathBuf,
    /// Where in the file, counting from 1: the value for a value this build cannot read,
    /// the key's name for an unknown key.
    pub line: usize,
    pub column: usize,
    /// The dotted key the warning is about, such as `display.wrapp` or `devices[0].baud`.
    pub key: String,
    pub kind: SettingsWarningKind,
    pub message: String,
}

impl SettingsWarning {
    /// Whether the warning is about a value this build cannot read, rather than a key it
    /// does not know.
    pub fn is_invalid_value(&self) -> bool {
        matches!(self.kind, SettingsWarningKind::InvalidValue { .. })
    }
}

impl fmt::Display for SettingsWarning {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {}",
            self.file.display(),
            self.line,
            self.column,
            self.message
        )
    }
}

/// One settings document with the name to report it under.
#[derive(Clone, Debug)]
pub struct SettingsLayer {
    /// Usually the file path; any label for an in-memory document.
    pub origin: PathBuf,
    pub text: String,
}

impl SettingsLayer {
    pub fn new(origin: impl Into<PathBuf>, text: impl Into<String>) -> Self {
        Self {
            origin: origin.into(),
            text: text.into(),
        }
    }
}

/// Zed's settings files allow comments and trailing commas; the parser's defaults also
/// allow a few other leniencies (hex numbers, unquoted keys), which do no harm.
fn parse_options() -> ParseOptions {
    ParseOptions::default()
}

fn invalid(file: &Path, err: &jsonc_parser::errors::ParseError) -> SettingsError {
    SettingsError::Invalid {
        file: file.to_path_buf(),
        line: err.line_display(),
        column: err.column_display(),
        // The kind alone: `Display` on the error appends the position again.
        message: err.kind().to_string(),
    }
}

/// Parses JSONC text to a JSON object. Empty text or comments only give `{}`.
pub(super) fn parse_object(text: &str, file: &Path) -> Result<Value, SettingsError> {
    let value: Value = jsonc_parser::parse_to_serde_value(text, &parse_options())
        .map_err(|err| invalid(file, &err))?;
    match value {
        Value::Null => Ok(Value::Object(Map::new())),
        Value::Object(_) => Ok(value),
        _ => Err(SettingsError::Invalid {
            file: file.to_path_buf(),
            line: 1,
            column: 1,
            message: "settings must be a JSON object".to_string(),
        }),
    }
}

/// Deep-merges `overlay` into `base`: objects merge key by key, and any other value,
/// arrays included, replaces. `null` replaces too, which unsets an optional key.
pub(super) fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// The key structure of a full `Settings`, used to spot unknown keys and to find the
/// leaves of a layer. An empty object marks a free-form map (font features); `null` marks
/// an optional key whose shape is not known until it is set.
static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut schema = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
    if let Some(devices) = schema.get_mut("devices") {
        let sample = serde_json::to_value(DeviceProfile::default()).unwrap_or(Value::Null);
        *devices = Value::Array(vec![sample]);
    }
    schema
});

/// One step of a key path: an object key or an array index.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Seg {
    Key(String),
    Index(usize),
}

/// The path as the warnings spell it: `display.wrap`, `devices[0].match.vid`.
fn dotted(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(key) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Seg::Index(index) => out.push_str(&format!("[{index}]")),
        }
    }
    out
}

fn collect_unknown(value: &Value, schema: &Value, path: &mut Vec<Seg>, out: &mut Vec<Vec<Seg>>) {
    match (value, schema) {
        (Value::Object(values), Value::Object(known)) if !known.is_empty() => {
            for (key, child) in values {
                path.push(Seg::Key(key.clone()));
                match known.get(key) {
                    Some(child_schema) => collect_unknown(child, child_schema, path, out),
                    None => out.push(path.clone()),
                }
                path.pop();
            }
        }
        (Value::Array(items), Value::Array(sample)) if sample.len() == 1 => {
            for (index, item) in items.iter().enumerate() {
                path.push(Seg::Index(index));
                collect_unknown(item, &sample[0], path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Keys in one layer that Settings does not know.
fn unknown_keys(layer: &Value) -> Vec<Vec<Seg>> {
    let mut found = Vec::new();
    collect_unknown(layer, &SCHEMA, &mut Vec::new(), &mut found);
    found
}

/// The leaves of one layer, in file order: a non-empty object the schema knows as an
/// object is opened, anything else (a scalar, an array, an empty object, a value of the
/// wrong shape, one entry of a free-form map, an unknown key) is a leaf.
fn collect_leaves(
    value: &Value,
    schema: Option<&Value>,
    path: &mut Vec<String>,
    out: &mut Vec<Leaf>,
) {
    match (value, schema) {
        (Value::Object(map), Some(Value::Object(_))) if !map.is_empty() => {
            for (key, child) in map {
                path.push(key.clone());
                collect_leaves(child, schema.and_then(|schema| schema.get(key)), path, out);
                path.pop();
            }
        }
        _ => out.push(Leaf {
            path: path.clone(),
            value: value.clone(),
            wants_object: schema.is_some_and(|s| s.as_object().is_some_and(|o| !o.is_empty())),
        }),
    }
}

/// Why an array is not read where a settings object belongs.
const ARRAY_FOR_OBJECT: &str = "invalid type: sequence, expected an object";

struct Leaf {
    path: Vec<String>,
    value: Value,
    /// The key holds a settings object (`display`, `terminal`, ...). serde would read
    /// such an object from an array too, field by field, which no one means.
    wants_object: bool,
}

/// `base` with `value` merged in at `path`, as the whole layer's merge would put it.
fn merge_at(base: &mut Value, path: &[String], value: Value) {
    let overlay = path.iter().rev().fold(value, |inner, key| {
        let mut object = Map::new();
        object.insert(key.clone(), inner);
        Value::Object(object)
    });
    merge(base, overlay);
}

/// Line and column (from 1) of byte `offset` in `text`, counted as the parser counts
/// them for its own errors.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let offset = offset.min(text.len());
    let mut line = 1;
    let mut column = 1;
    for (i, c) in text[..offset].char_indices() {
        // A lone `\r` ends a line comment, so it counts as a line break too.
        if c == '\n' || c == '\r' && text.as_bytes().get(i + 1) != Some(&b'\n') {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

/// Finds where a key path is in a layer's text.
struct Positions<'a> {
    text: &'a str,
    root: Option<ast::Value<'a>>,
}

impl<'a> Positions<'a> {
    fn new(text: &'a str) -> Self {
        let root = jsonc_parser::parse_to_ast(text, &CollectOptions::default(), &parse_options())
            .ok()
            .and_then(|result| result.value);
        Self { text, root }
    }

    /// The line and column of the value at `path`, or of its key's name when `at_name`.
    /// A path that cannot be followed to the end stops at the last node it reached.
    fn of(&self, path: &[Seg], at_name: bool) -> (usize, usize) {
        let Some(mut node) = self.root.as_ref() else {
            return (1, 1);
        };
        let mut offset = node.start();
        for (i, seg) in path.iter().enumerate() {
            let last = i + 1 == path.len();
            match (seg, node) {
                (Seg::Key(key), ast::Value::Object(object)) => {
                    // The last of a duplicated key is the one that counts, as in the value.
                    let Some(prop) = object
                        .properties
                        .iter()
                        .rev()
                        .find(|prop| prop.name.as_str() == key)
                    else {
                        break;
                    };
                    node = &prop.value;
                    offset = if last && at_name {
                        prop.name.start()
                    } else {
                        node.start()
                    };
                }
                (Seg::Index(index), ast::Value::Array(array)) => {
                    let Some(element) = array.elements.get(*index) else {
                        break;
                    };
                    node = element;
                    offset = node.start();
                }
                _ => break,
            }
        }
        line_column(self.text, offset)
    }
}

/// Applies one layer to `merged`, leaf by leaf, collecting warnings.
struct LayerLoad<'a> {
    origin: &'a Path,
    positions: Positions<'a>,
    warnings: &'a mut Vec<SettingsWarning>,
}

impl LayerLoad<'_> {
    fn warn(&mut self, path: &[Seg], kind: SettingsWarningKind, message: String) {
        let at_name = kind == SettingsWarningKind::UnknownKey;
        let (line, column) = self.positions.of(path, at_name);
        self.warnings.push(SettingsWarning {
            file: self.origin.to_path_buf(),
            line,
            column,
            key: dotted(path),
            kind,
            message,
        });
    }

    /// A value at `path` this build cannot read, left out.
    fn skip(&mut self, path: &[Seg], what: &str, reason: String) {
        let message = format!("{what} `{}` is ignored: {reason}", dotted(path));
        self.warn(path, SettingsWarningKind::InvalidValue { reason }, message);
    }

    fn apply(&mut self, merged: &mut Value, layer: Value) {
        for path in unknown_keys(&layer) {
            let message = format!("unknown setting `{}` is ignored", dotted(&path));
            self.warn(&path, SettingsWarningKind::UnknownKey, message);
        }
        let mut leaves = Vec::new();
        collect_leaves(&layer, Some(&SCHEMA), &mut Vec::new(), &mut leaves);
        for Leaf {
            path,
            value,
            wants_object,
        } in leaves
        {
            let segs: Vec<Seg> = path.iter().cloned().map(Seg::Key).collect();
            if wants_object && value.is_array() {
                self.skip(&segs, "setting", ARRAY_FOR_OBJECT.to_string());
                continue;
            }
            let value = if path == ["devices"] {
                match self.devices(&value) {
                    Some(value) => value,
                    None => continue,
                }
            } else {
                value
            };
            let mut candidate = merged.clone();
            merge_at(&mut candidate, &path, value);
            match Settings::deserialize(&candidate) {
                Ok(_) => *merged = candidate,
                Err(err) => self.skip(&segs, "setting", err.to_string()),
            }
        }
    }

    /// The `devices` list with what this build cannot read left out: a key of a profile
    /// that does not read, or a whole profile that is not an object or has no readable
    /// `match`. `None` when the value is not a list at all.
    fn devices(&mut self, value: &Value) -> Option<Value> {
        let devices = Seg::Key("devices".to_string());
        let Value::Array(items) = value else {
            let reason = match Vec::<DeviceProfile>::deserialize(value) {
                Err(err) => err.to_string(),
                Ok(_) => "expected a list of device profiles".to_string(),
            };
            self.skip(&[devices], "setting", reason);
            return None;
        };
        let mut kept = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let at = [devices.clone(), Seg::Index(index)];
            let reason = match item {
                Value::Object(map) => match map.get("match") {
                    Some(matcher) if !matcher.is_object() => {
                        "its `match` must be an object".to_string()
                    }
                    Some(matcher) => match DeviceMatch::deserialize(matcher) {
                        Ok(_) => {
                            kept.push(self.profile(&at, map, matcher));
                            continue;
                        }
                        Err(err) => format!("its `match` does not read: {err}"),
                    },
                    None => "it has no `match`".to_string(),
                },
                other => match DeviceProfile::deserialize(other) {
                    Err(err) => err.to_string(),
                    Ok(_) => "expected a device profile object".to_string(),
                },
            };
            self.skip(&at, "device profile", reason);
        }
        Some(Value::Array(kept))
    }

    /// One profile, `match` first, then each other key that reads.
    fn profile(&mut self, at: &[Seg], map: &Map<String, Value>, matcher: &Value) -> Value {
        let mut profile = Map::new();
        profile.insert("match".to_string(), matcher.clone());
        for (key, value) in map {
            if key == "match" {
                continue;
            }
            let mut candidate = profile.clone();
            candidate.insert(key.clone(), value.clone());
            let candidate = Value::Object(candidate);
            match DeviceProfile::deserialize(&candidate) {
                Ok(_) => {
                    let Value::Object(map) = candidate else {
                        unreachable!("built as an object")
                    };
                    profile = map;
                }
                Err(err) => {
                    let mut path = at.to_vec();
                    path.push(Seg::Key(key.clone()));
                    self.skip(&path, "setting", err.to_string());
                }
            }
        }
        Value::Object(profile)
    }
}

/// Loads settings from in-memory layers, lowest first, over the bundled defaults.
///
/// Objects merge key by key, arrays and scalars from a later layer replace, and a
/// `null` unsets an optional key. A file that is not JSONC, or whose root is not an
/// object, is an error naming its file, line and column. Within a file each leaf key is
/// checked against the layers below it: a value this build cannot read leaves that key
/// out, so the lower layer's value stays, and an unknown key is ignored. Both are
/// reported in [`Settings::warnings`] with file, line and column.
pub fn load_settings_from_layers(layers: &[SettingsLayer]) -> Result<Settings, SettingsError> {
    let mut merged = defaults::bundled_value();
    let mut warnings = Vec::new();
    for layer in layers {
        let value = parse_object(&layer.text, &layer.origin)?;
        let first = warnings.len();
        LayerLoad {
            origin: &layer.origin,
            positions: Positions::new(&layer.text),
            warnings: &mut warnings,
        }
        .apply(&mut merged, value);
        // Keys are visited in sorted order; report them in the order the file has them.
        warnings[first..].sort_by_key(|warning| (warning.line, warning.column));
    }
    let mut settings: Settings =
        serde_json::from_value(merged).map_err(|err| SettingsError::Merged(err.to_string()))?;
    settings.warnings = warnings;
    Ok(settings)
}

fn read_layer(path: &Path) -> Result<Option<SettingsLayer>, SettingsError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(SettingsLayer::new(path, text))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SettingsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Loads the bundled defaults, then the user file, then the project file.
///
/// A path that is `None` or names a file that does not exist adds no layer. See
/// [`load_settings_from_layers`] for the merge rules and how warnings are reported.
pub fn load_settings(
    user_path: Option<&Path>,
    project_path: Option<&Path>,
) -> Result<Settings, SettingsError> {
    let mut layers = Vec::new();
    for path in [user_path, project_path].into_iter().flatten() {
        if let Some(layer) = read_layer(path)? {
            layers.push(layer);
        }
    }
    load_settings_from_layers(&layers)
}

impl Settings {
    /// The bundled defaults.
    pub fn bundled() -> Settings {
        Settings::default()
    }

    /// Defaults with one JSONC document on top, for tests and tools.
    pub fn from_jsonc(text: &str) -> Result<Settings, SettingsError> {
        load_settings_from_layers(&[SettingsLayer::new("<settings>", text)])
    }
}
