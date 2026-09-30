//! Reading settings files: JSONC, layering and validation.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use jsonc_parser::ParseOptions;
use serde_json::{Map, Value};

use super::defaults;
use super::profile::DeviceProfile;
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
    /// A file that is not valid JSONC, or a value of the wrong type. `line` and
    /// `column` count from 1; `message` is the parser's or serde's own text.
    #[error("{}:{line}:{column}: {message}", file.display())]
    Invalid {
        file: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    /// The merged layers do not fit the settings types, though each file parsed. Not
    /// expected in practice because every layer is checked on its own first.
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

/// A problem that did not stop the load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsWarning {
    pub file: PathBuf,
    /// The dotted key the warning is about, such as `display.wrapp`.
    pub key: String,
    pub message: String,
}

impl fmt::Display for SettingsWarning {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.file.display(), self.message)
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

/// Checks a single layer against the settings types so an error names that file and
/// position. Every key has a bundled default, so a partial document deserializes.
fn validate_layer(layer: &SettingsLayer) -> Result<(), SettingsError> {
    // Comments-only text reads as null, which is an empty layer, not an error.
    if is_blank(&layer.text) {
        return Ok(());
    }
    jsonc_parser::parse_to_serde_value::<Settings>(&layer.text, &parse_options())
        .map(drop)
        .map_err(|err| invalid(&layer.origin, &err))
}

/// Whether `text` holds nothing but whitespace and comments.
fn is_blank(text: &str) -> bool {
    matches!(
        jsonc_parser::parse_to_serde_value::<Value>(text, &parse_options()),
        Ok(Value::Null)
    )
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

/// The key structure of a full `Settings`, used to spot unknown keys. An empty object
/// marks a free-form map (font features); `null` marks an optional key whose shape is
/// not known until it is set.
static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    let mut schema = serde_json::to_value(Settings::default()).unwrap_or(Value::Null);
    if let Some(devices) = schema.get_mut("devices") {
        let sample = serde_json::to_value(DeviceProfile::default()).unwrap_or(Value::Null);
        *devices = Value::Array(vec![sample]);
    }
    schema
});

fn collect_unknown(value: &Value, schema: &Value, path: &str, out: &mut Vec<String>) {
    match (value, schema) {
        (Value::Object(values), Value::Object(known)) if !known.is_empty() => {
            for (key, child) in values {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match known.get(key) {
                    Some(child_schema) => collect_unknown(child, child_schema, &child_path, out),
                    None => out.push(child_path),
                }
            }
        }
        (Value::Array(items), Value::Array(sample)) if sample.len() == 1 => {
            for (index, item) in items.iter().enumerate() {
                collect_unknown(item, &sample[0], &format!("{path}[{index}]"), out);
            }
        }
        _ => {}
    }
}

/// Keys in one layer that Settings does not know, as dotted paths.
fn unknown_keys(layer: &Value) -> Vec<String> {
    let mut found = Vec::new();
    collect_unknown(layer, &SCHEMA, "", &mut found);
    found
}

/// Loads settings from in-memory layers, lowest first, over the bundled defaults.
///
/// Objects merge key by key, arrays and scalars from a later layer replace, and a
/// `null` unsets an optional key. Each layer is checked on its own so an error names
/// its file, line and column. Unknown keys are ignored and reported in
/// [`Settings::warnings`].
pub fn load_settings_from_layers(layers: &[SettingsLayer]) -> Result<Settings, SettingsError> {
    let mut merged = defaults::bundled_value();
    let mut warnings = Vec::new();
    for layer in layers {
        let value = parse_object(&layer.text, &layer.origin)?;
        validate_layer(layer)?;
        for key in unknown_keys(&value) {
            warnings.push(SettingsWarning {
                file: layer.origin.clone(),
                message: format!("unknown setting `{key}` is ignored"),
                key,
            });
        }
        merge(&mut merged, value);
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
