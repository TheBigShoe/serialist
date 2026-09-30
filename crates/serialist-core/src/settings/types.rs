//! The settings types. Key names are Zed's own.

use std::fmt;
use std::path::PathBuf;

use serde::de::{self, Deserializer, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::config::SerialConfig;
use crate::port::PortInfo;

use super::de::{baud_rate, font_size, font_weight, opt_font_size, opt_font_weight, row_bytes};
use super::defaults as d;
use super::font::{FontFeatures, FontSpec, LineHeight};
use super::load::SettingsWarning;
use super::profile::DeviceProfile;

/// What Enter appends to a line sent to the device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    None,
    Cr,
    Lf,
    #[default]
    Crlf,
}

impl LineEnding {
    /// The order a cycle button walks through.
    pub const ALL: [LineEnding; 4] = [
        LineEnding::None,
        LineEnding::Cr,
        LineEnding::Lf,
        LineEnding::Crlf,
    ];

    pub fn bytes(self) -> &'static [u8] {
        match self {
            LineEnding::None => b"",
            LineEnding::Cr => b"\r",
            LineEnding::Lf => b"\n",
            LineEnding::Crlf => b"\r\n",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LineEnding::None => "None",
            LineEnding::Cr => "CR",
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
        }
    }
}

/// The timestamp gutter setting: `off`, `absolute`, `relative` or `delta`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampMode {
    #[default]
    Off,
    Absolute,
    Relative,
    Delta,
}

/// How received bytes are shown: `text`, `hex` or `hex_ascii`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayView {
    #[default]
    Text,
    Hex,
    HexAscii,
}

/// The `display` object: defaults for each session's view, which the status line can
/// override per session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplaySettings {
    #[serde(default = "d::display_timestamps")]
    pub timestamps: TimestampMode,
    /// `strftime`-style format for absolute timestamps.
    #[serde(default = "d::display_timestamp_format")]
    pub timestamp_format: String,
    #[serde(default = "d::display_view")]
    pub view: DisplayView,
    #[serde(
        default = "d::display_hex_bytes_per_row",
        deserialize_with = "row_bytes"
    )]
    pub hex_bytes_per_row: usize,
    /// Render CR, LF and ESC as dim glyphs.
    #[serde(default = "d::display_show_control_chars")]
    pub show_control_chars: bool,
    #[serde(default = "d::display_wrap")]
    pub wrap: bool,
}

/// The `terminal` object. A key that is set replaces the matching `buffer_font_*` value
/// for the terminal only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TerminalSettings {
    #[serde(default)]
    pub font_family: Option<String>,
    #[serde(default, deserialize_with = "opt_font_size")]
    pub font_size: Option<f32>,
    #[serde(default, deserialize_with = "opt_font_weight")]
    pub font_weight: Option<f32>,
    #[serde(default)]
    pub font_features: Option<FontFeatures>,
    #[serde(default)]
    pub font_fallbacks: Option<Vec<String>>,
    #[serde(default)]
    pub line_height: Option<LineHeight>,
}

/// Whether a `theme` object follows the system appearance or pins one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// The `theme` setting: a theme name, or `{ "mode", "light", "dark" }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ThemeSelection {
    /// One theme regardless of the system appearance.
    Static(String),
    Dynamic {
        mode: ThemeMode,
        light: String,
        dark: String,
    },
}

impl ThemeSelection {
    /// Whether the dark variant applies given the system appearance. A plain theme
    /// name pins one theme, so it follows the system for the fallback choice only.
    pub fn prefers_dark(&self, system_dark: bool) -> bool {
        match self {
            ThemeSelection::Static(_) => system_dark,
            ThemeSelection::Dynamic { mode, .. } => match mode {
                ThemeMode::System => system_dark,
                ThemeMode::Light => false,
                ThemeMode::Dark => true,
            },
        }
    }

    /// The theme name this selection asks for under the given system appearance.
    pub fn name(&self, system_dark: bool) -> &str {
        match self {
            ThemeSelection::Static(name) => name,
            ThemeSelection::Dynamic { light, dark, .. } => {
                if self.prefers_dark(system_dark) {
                    dark
                } else {
                    light
                }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ThemeSelection {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ThemeVisitor;

        impl<'de> Visitor<'de> for ThemeVisitor {
            type Value = ThemeSelection;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a theme name or { \"mode\", \"light\", \"dark\" }")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<ThemeSelection, E> {
                Ok(ThemeSelection::Static(v.to_string()))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ThemeSelection, A::Error> {
                let mut mode = None;
                let mut light = None;
                let mut dark = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "mode" => mode = Some(map.next_value::<ThemeMode>()?),
                        "light" => light = Some(map.next_value::<String>()?),
                        "dark" => dark = Some(map.next_value::<String>()?),
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                let names = d::theme_names();
                Ok(ThemeSelection::Dynamic {
                    mode: mode.unwrap_or_default(),
                    light: light.unwrap_or(names.0),
                    dark: dark.unwrap_or(names.1),
                })
            }
        }

        deserializer.deserialize_any(ThemeVisitor)
    }
}

/// Everything configurable, resolved against the bundled defaults.
///
/// Every key is optional in a settings file. Load one with
/// [`load_settings`](super::load_settings) or [`Settings::from_jsonc`]; a `Settings`
/// deserialized straight from a partial document takes the bundled default for each
/// missing key too.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// Family for the terminal text. `None` means the bundled monospace font.
    #[serde(default)]
    pub buffer_font_family: Option<String>,
    /// Points. Default 15.
    #[serde(default = "d::buffer_font_size", deserialize_with = "font_size")]
    pub buffer_font_size: f32,
    /// 100 to 900. Default 400.
    #[serde(default = "d::buffer_font_weight", deserialize_with = "font_weight")]
    pub buffer_font_weight: f32,
    #[serde(default = "d::buffer_font_features")]
    pub buffer_font_features: FontFeatures,
    #[serde(default = "d::buffer_line_height")]
    pub buffer_line_height: LineHeight,
    #[serde(default = "d::buffer_font_fallbacks")]
    pub buffer_font_fallbacks: Vec<String>,

    #[serde(default)]
    pub ui_font_family: Option<String>,
    /// Points. Default 16.
    #[serde(default = "d::ui_font_size", deserialize_with = "font_size")]
    pub ui_font_size: f32,
    #[serde(default = "d::ui_font_weight", deserialize_with = "font_weight")]
    pub ui_font_weight: f32,
    #[serde(default = "d::ui_font_features")]
    pub ui_font_features: FontFeatures,
    #[serde(default = "d::ui_font_fallbacks")]
    pub ui_font_fallbacks: Vec<String>,

    /// Terminal-only font overrides.
    #[serde(default)]
    pub terminal: TerminalSettings,

    #[serde(default = "d::theme")]
    pub theme: ThemeSelection,
    #[serde(default = "d::display")]
    pub display: DisplaySettings,

    /// Upper bound on scrollback memory per session, in bytes. Default 256 MiB.
    #[serde(default = "d::scrollback_budget_bytes")]
    pub scrollback_budget_bytes: u64,
    /// Baud rate for a port with no matching profile. Default 115200.
    #[serde(default = "d::default_baud", deserialize_with = "baud_rate")]
    pub default_baud: u32,
    /// What Enter sends by default. Default CRLF.
    #[serde(default = "d::line_ending")]
    pub line_ending: LineEnding,
    #[serde(default = "d::local_echo")]
    pub local_echo: bool,

    /// Per-device profiles; the first one whose `match` fits a port applies.
    #[serde(default = "d::devices")]
    pub devices: Vec<DeviceProfile>,

    /// Problems found while loading that did not stop it, such as unknown keys.
    /// Filled by the loader, never read from a file.
    #[serde(skip)]
    pub warnings: Vec<SettingsWarning>,
}

impl Default for Settings {
    /// The bundled defaults.
    fn default() -> Self {
        d::bundled_settings()
    }
}

impl Settings {
    /// The terminal font: the `terminal.*` keys where set, else the `buffer_*` keys.
    pub fn resolved_terminal_font(&self) -> FontSpec {
        let t = &self.terminal;
        FontSpec {
            family: t
                .font_family
                .clone()
                .or_else(|| self.buffer_font_family.clone()),
            size: t.font_size.unwrap_or(self.buffer_font_size),
            weight: t.font_weight.unwrap_or(self.buffer_font_weight),
            features: t
                .font_features
                .clone()
                .unwrap_or_else(|| self.buffer_font_features.clone()),
            fallbacks: t
                .font_fallbacks
                .clone()
                .unwrap_or_else(|| self.buffer_font_fallbacks.clone()),
            line_height: t.line_height.unwrap_or(self.buffer_line_height).value(),
        }
    }

    /// The UI font. There is no UI line-height key, so this is always "standard".
    pub fn resolved_ui_font(&self) -> FontSpec {
        FontSpec {
            family: self.ui_font_family.clone(),
            size: self.ui_font_size,
            weight: self.ui_font_weight,
            features: self.ui_font_features.clone(),
            fallbacks: self.ui_font_fallbacks.clone(),
            line_height: LineHeight::Standard.value(),
        }
    }

    /// The first device profile that matches `port`.
    pub fn profile_for(&self, port: &PortInfo) -> Option<&DeviceProfile> {
        self.devices.iter().find(|profile| profile.matches(port))
    }

    /// The line configuration to open `port` with: the matching profile's overrides on
    /// top of `default_baud` and 8N1.
    pub fn serial_config_for(&self, port: &PortInfo) -> SerialConfig {
        let base = SerialConfig {
            baud: self.default_baud,
            ..SerialConfig::default()
        };
        match self.profile_for(port) {
            Some(profile) => profile.apply_to(&base),
            None => base,
        }
    }

    /// The line ending Enter sends for `port`: the profile's `eol`, else `line_ending`.
    pub fn line_ending_for(&self, port: &PortInfo) -> LineEnding {
        self.profile_for(port)
            .and_then(|profile| profile.eol)
            .unwrap_or(self.line_ending)
    }

    /// The name the Devices panel shows for `port`: the matching profile's `name`, if
    /// it has one.
    pub fn device_name_for(&self, port: &PortInfo) -> Option<&str> {
        self.profile_for(port)
            .and_then(|profile| profile.name.as_deref())
    }

    /// The scrollback budget as a `usize`, saturating on 32-bit targets.
    pub fn scrollback_budget(&self) -> usize {
        usize::try_from(self.scrollback_budget_bytes).unwrap_or(usize::MAX)
    }

    /// The `on_connect` script of the profile matching `port`.
    pub fn on_connect_for(&self, port: &PortInfo) -> Option<&PathBuf> {
        self.profile_for(port)
            .and_then(|profile| profile.on_connect.as_ref())
    }
}
