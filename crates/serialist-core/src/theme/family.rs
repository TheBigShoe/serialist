//! Zed theme family files (schema v0.2.0) parsed into plain Rust types.
//!
//! A file is `{ name, author, themes: [ { name, appearance, style } ] }`. The style is a
//! flat object of dotted keys to colors, plus `players` and `syntax`. Only the keys the
//! app reads matter; the rest are kept, so a theme with 134 keys loads whole.

use std::collections::BTreeMap;
use std::fmt;

use jsonc_parser::ParseOptions;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::color::Rgba;
use super::keys::TERMINAL_ANSI_KEYS;

/// Whether a theme is meant for a light or a dark window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    Light,
    Dark,
}

impl Appearance {
    pub fn is_dark(self) -> bool {
        self == Appearance::Dark
    }

    pub fn from_dark(dark: bool) -> Self {
        if dark {
            Appearance::Dark
        } else {
            Appearance::Light
        }
    }
}

impl fmt::Display for Appearance {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        })
    }
}

/// A syntax capture's font style.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FontStyle {
    Normal,
    Italic,
    Oblique,
}

/// One entry of a theme's `syntax` map.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SyntaxStyle {
    pub color: Option<Rgba>,
    pub font_style: Option<FontStyle>,
    /// 100 to 900.
    pub font_weight: Option<f32>,
}

/// One entry of a theme's `players` list. Player 0 is the local user.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PlayerColors {
    pub cursor: Option<Rgba>,
    pub selection: Option<Rgba>,
    pub background: Option<Rgba>,
}

/// One theme: a named set of colors.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub appearance: Appearance,
    /// Every color key of the file's `style`, such as `terminal.background`.
    pub style: BTreeMap<String, Rgba>,
    pub players: Vec<PlayerColors>,
    /// Keyed by capture name, such as `comment` or `string`.
    pub syntax: BTreeMap<String, SyntaxStyle>,
}

impl Theme {
    pub fn new(name: impl Into<String>, appearance: Appearance) -> Self {
        Self {
            name: name.into(),
            appearance,
            style: BTreeMap::new(),
            players: Vec::new(),
            syntax: BTreeMap::new(),
        }
    }

    pub fn is_dark(&self) -> bool {
        self.appearance.is_dark()
    }

    /// The color for a style key such as `terminal.background`.
    ///
    /// Two prefixes reach into the other tables, so every color the app reads uses one
    /// lookup: `syntax.<capture>` is that capture's color, and `players[N].cursor`,
    /// `players[N].selection` and `players[N].background` are player N's.
    pub fn color(&self, key: &str) -> Option<Rgba> {
        if let Some(color) = self.style.get(key) {
            return Some(*color);
        }
        if let Some(capture) = key.strip_prefix("syntax.") {
            return self.syntax_color(capture);
        }
        let (index, field) = player_key(key)?;
        let player = self.players.get(index)?;
        match field {
            "cursor" => player.cursor,
            "selection" => player.selection,
            "background" => player.background,
            _ => None,
        }
    }

    /// [`color`](Self::color), or `fallback` when the theme lacks the key.
    pub fn color_or(&self, key: &str, fallback: Rgba) -> Rgba {
        self.color(key).unwrap_or(fallback)
    }

    /// One of the 16 terminal colors: 0 to 7 are black, red, green, yellow, blue,
    /// magenta, cyan and white; 8 to 15 are their bright forms. `None` past 15 or when
    /// the theme leaves the key out.
    pub fn terminal_ansi(&self, index: u8) -> Option<Rgba> {
        let key = TERMINAL_ANSI_KEYS.get(usize::from(index))?;
        self.style.get(*key).copied()
    }

    pub fn player(&self, index: usize) -> Option<&PlayerColors> {
        self.players.get(index)
    }

    pub fn syntax_style(&self, capture: &str) -> Option<&SyntaxStyle> {
        self.syntax.get(capture)
    }

    pub fn syntax_color(&self, capture: &str) -> Option<Rgba> {
        self.syntax.get(capture).and_then(|style| style.color)
    }
}

/// Splits `players[3].cursor` into `(3, "cursor")`.
fn player_key(key: &str) -> Option<(usize, &str)> {
    let rest = key.strip_prefix("players[")?;
    let (index, field) = rest.split_once("].")?;
    Some((index.parse().ok()?, field))
}

/// A theme file: an author's themes, usually a dark and a light variant.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeFamily {
    pub name: String,
    pub author: String,
    pub themes: Vec<Theme>,
}

/// A problem in a theme file that did not stop it loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThemeWarning {
    /// The file path or label the theme came from.
    pub source: String,
    /// The theme the warning is about, when it is about one.
    pub theme: Option<String>,
    pub message: String,
}

impl fmt::Display for ThemeWarning {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.source)?;
        if let Some(theme) = &self.theme {
            write!(f, " ({theme})")?;
        }
        write!(f, ": {}", self.message)
    }
}

/// Why a theme file could not be used at all.
#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("{file}:{line}:{column}: {message}")]
    Invalid {
        file: String,
        line: usize,
        column: usize,
        message: String,
    },
    /// Valid JSON that is not a theme family.
    #[error("{file}: {message}")]
    Schema { file: String, message: String },
    #[error("{file}: {source}")]
    Io {
        file: String,
        #[source]
        source: std::io::Error,
    },
}

/// Style keys that hold something other than a color and are not warned about.
const NON_COLOR_KEYS: &[&str] = &["background.appearance", "accents"];

struct Parser<'a> {
    source: &'a str,
    theme: Option<String>,
    warnings: Vec<ThemeWarning>,
}

impl Parser<'_> {
    fn warn(&mut self, message: String) {
        self.warnings.push(ThemeWarning {
            source: self.source.to_string(),
            theme: self.theme.clone(),
            message,
        });
    }

    /// A color that may be absent or null without comment.
    fn optional_color(&mut self, owner: &str, field: &str, value: Option<&Value>) -> Option<Rgba> {
        match value {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => match Rgba::parse_hex(text) {
                Ok(color) => Some(color),
                Err(err) => {
                    self.warn(format!("{owner}.{field}: {err}; skipped"));
                    None
                }
            },
            Some(_) => {
                self.warn(format!("{owner}.{field}: expected a color string; skipped"));
                None
            }
        }
    }

    fn style_key(&mut self, key: &str, value: &Value, out: &mut BTreeMap<String, Rgba>) {
        match value {
            Value::String(text) => match Rgba::parse_hex(text) {
                Ok(color) => {
                    out.insert(key.to_string(), color);
                }
                Err(err) => self.warn(format!("{key}: {err}; skipped")),
            },
            Value::Null => self.warn(format!("{key} has no value (null); skipped")),
            _ => self.warn(format!("{key}: expected a color string; skipped")),
        }
    }

    fn players(&mut self, value: &Value) -> Vec<PlayerColors> {
        let Some(items) = value.as_array() else {
            self.warn("players: expected a list; skipped".to_string());
            return Vec::new();
        };
        items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let owner = format!("players[{index}]");
                let Some(object) = item.as_object() else {
                    self.warn(format!("{owner}: expected an object; skipped"));
                    return PlayerColors::default();
                };
                PlayerColors {
                    cursor: self.optional_color(&owner, "cursor", object.get("cursor")),
                    selection: self.optional_color(&owner, "selection", object.get("selection")),
                    background: self.optional_color(&owner, "background", object.get("background")),
                }
            })
            .collect()
    }

    fn syntax(&mut self, value: &Value) -> BTreeMap<String, SyntaxStyle> {
        let mut out = BTreeMap::new();
        let Some(captures) = value.as_object() else {
            self.warn("syntax: expected an object; skipped".to_string());
            return out;
        };
        for (capture, entry) in captures {
            let owner = format!("syntax.{capture}");
            let Some(entry) = entry.as_object() else {
                self.warn(format!("{owner}: expected an object; skipped"));
                continue;
            };
            let font_style = match entry.get("font_style") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) => match s.as_str() {
                    "normal" => Some(FontStyle::Normal),
                    "italic" => Some(FontStyle::Italic),
                    "oblique" => Some(FontStyle::Oblique),
                    other => {
                        self.warn(format!(
                            "{owner}.font_style: unknown style {other:?}; skipped"
                        ));
                        None
                    }
                },
                Some(_) => {
                    self.warn(format!("{owner}.font_style: expected a string; skipped"));
                    None
                }
            };
            let font_weight = match entry.get("font_weight") {
                None | Some(Value::Null) => None,
                Some(Value::Number(n)) => match n.as_f64() {
                    Some(w) if (100.0..=900.0).contains(&w) => Some(w as f32),
                    _ => {
                        self.warn(format!("{owner}.font_weight: expected 100 to 900; skipped"));
                        None
                    }
                },
                Some(_) => {
                    self.warn(format!("{owner}.font_weight: expected a number; skipped"));
                    None
                }
            };
            out.insert(
                capture.clone(),
                SyntaxStyle {
                    color: self.optional_color(&owner, "color", entry.get("color")),
                    font_style,
                    font_weight,
                },
            );
        }
        out
    }

    fn theme(&mut self, value: &Value) -> Option<Theme> {
        let Some(object) = value.as_object() else {
            self.warn("a theme entry is not an object; skipped".to_string());
            return None;
        };
        let Some(name) = object.get("name").and_then(Value::as_str) else {
            self.warn("a theme entry has no name; skipped".to_string());
            return None;
        };
        self.theme = Some(name.to_string());
        let appearance = match object.get("appearance").and_then(Value::as_str) {
            Some("dark") => Appearance::Dark,
            Some("light") => Appearance::Light,
            _ => {
                self.warn("appearance must be \"light\" or \"dark\"; theme skipped".to_string());
                return None;
            }
        };
        let Some(style) = object.get("style").and_then(Value::as_object) else {
            self.warn("the theme has no style object; skipped".to_string());
            return None;
        };

        let mut theme = Theme::new(name, appearance);
        for (key, value) in style {
            match key.as_str() {
                "players" => theme.players = self.players(value),
                "syntax" => theme.syntax = self.syntax(value),
                other if NON_COLOR_KEYS.contains(&other) => {}
                other => self.style_key(other, value, &mut theme.style),
            }
        }
        Some(theme)
    }
}

impl ThemeFamily {
    /// Parses a theme family file. `origin` is the file path or a label, used in
    /// errors and warnings.
    ///
    /// A theme entry that is unusable (no name, no appearance, no style) is skipped with
    /// a warning, and so is each style key with a null or unparsable value, so a
    /// theme file written for a newer Zed still loads. The text may contain comments and
    /// trailing commas.
    pub fn parse(text: &str, origin: &str) -> Result<(ThemeFamily, Vec<ThemeWarning>), ThemeError> {
        let value: Value = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
            .map_err(|err| ThemeError::Invalid {
                file: origin.to_string(),
                line: err.line_display(),
                column: err.column_display(),
                message: err.kind().to_string(),
            })?;
        let schema = |message: &str| ThemeError::Schema {
            file: origin.to_string(),
            message: message.to_string(),
        };
        let root: &Map<String, Value> = value
            .as_object()
            .ok_or_else(|| schema("a theme file is a JSON object"))?;
        let name = root
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| schema("a theme file needs a \"name\""))?;
        let entries = root
            .get("themes")
            .and_then(Value::as_array)
            .ok_or_else(|| schema("a theme file needs a \"themes\" list"))?;

        let mut parser = Parser {
            source: origin,
            theme: None,
            warnings: Vec::new(),
        };
        let mut themes = Vec::new();
        for entry in entries {
            parser.theme = None;
            if let Some(theme) = parser.theme(entry) {
                themes.push(theme);
            }
        }
        let family = ThemeFamily {
            name: name.to_string(),
            author: root
                .get("author")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            themes,
        };
        Ok((family, parser.warnings))
    }
}
