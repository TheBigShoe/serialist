//! The bundled defaults. `assets/default_settings.jsonc` is the single source of truth:
//! each `#[serde(default = "...")]` function below reads its key from that file, so a
//! partial settings file resolves without a second list of defaults to keep in sync.

use std::sync::LazyLock;

use serde::Deserialize as _;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::font::{FontFeatures, LineHeight};
use super::inline::{BackspaceKey, InlineSettings};
use super::profile::DeviceProfile;
use super::types::{
    DisplaySettings, DisplayView, LineEnding, Settings, ThemeSelection, TimestampMode,
};

/// The bundled defaults file, with its comments.
pub const DEFAULT_SETTINGS_JSONC: &str = include_str!("../../assets/default_settings.jsonc");

/// The name a bundled-defaults problem is reported under.
pub(super) const BUNDLED_NAME: &str = "<bundled default_settings.jsonc>";

static DEFAULTS: LazyLock<Value> = LazyLock::new(|| {
    match super::load::parse_object(DEFAULT_SETTINGS_JSONC, BUNDLED_NAME.as_ref()) {
        Ok(value) => value,
        Err(err) => panic!("the bundled default settings are invalid: {err}"),
    }
});

/// The bundled defaults as a JSON object, for layering.
pub(super) fn bundled_value() -> Value {
    DEFAULTS.clone()
}

/// Reads `path` from the bundled defaults as a `T`.
fn get<T: DeserializeOwned>(path: &[&str]) -> T {
    let mut value = &*DEFAULTS;
    for key in path {
        value = value
            .get(key)
            .unwrap_or_else(|| panic!("default_settings.jsonc has no {}", path.join(".")));
    }
    T::deserialize(value)
        .unwrap_or_else(|err| panic!("default_settings.jsonc {}: {err}", path.join(".")))
}

macro_rules! defaults {
    ($($name:ident: $ty:ty = $($key:literal).+;)*) => {
        $(pub(super) fn $name() -> $ty { get(&[$($key),+]) })*
    };
}

defaults! {
    buffer_font_size: f32 = "buffer_font_size";
    buffer_font_weight: f32 = "buffer_font_weight";
    buffer_font_features: FontFeatures = "buffer_font_features";
    buffer_line_height: LineHeight = "buffer_line_height";
    buffer_font_fallbacks: Vec<String> = "buffer_font_fallbacks";
    ui_font_size: f32 = "ui_font_size";
    ui_font_weight: f32 = "ui_font_weight";
    ui_font_features: FontFeatures = "ui_font_features";
    ui_font_fallbacks: Vec<String> = "ui_font_fallbacks";
    theme: ThemeSelection = "theme";
    scrollback_budget_bytes: u64 = "scrollback_budget_bytes";
    default_baud: u32 = "default_baud";
    line_ending: LineEnding = "line_ending";
    local_echo: bool = "local_echo";
    restore_session: bool = "restore_session";
    devices: Vec<DeviceProfile> = "devices";
    display_timestamps: TimestampMode = "display"."timestamps";
    display_timestamp_format: String = "display"."timestamp_format";
    display_view: DisplayView = "display"."view";
    display_hex_bytes_per_row: usize = "display"."hex_bytes_per_row";
    display_show_control_chars: bool = "display"."show_control_chars";
    display_wrap: bool = "display"."wrap";
    display_decoded_inline: bool = "display"."decoded_inline";
    display_hide_framed_bytes: bool = "display"."hide_framed_bytes";
    inline_backspace: BackspaceKey = "inline"."backspace";
    inline_escape_chord: String = "inline"."escape_chord";
    inline_paste_chunk_bytes: usize = "inline"."paste_chunk_bytes";
    inline_paste_chunk_delay_ms: u64 = "inline"."paste_chunk_delay_ms";
}

/// The whole `inline` object, for a settings file that leaves it out.
pub(super) fn inline() -> InlineSettings {
    InlineSettings {
        backspace: inline_backspace(),
        escape_chord: inline_escape_chord(),
        paste_chunk_bytes: inline_paste_chunk_bytes(),
        paste_chunk_delay_ms: inline_paste_chunk_delay_ms(),
    }
}

/// The whole `display` object, for a settings file that leaves it out.
pub(super) fn display() -> DisplaySettings {
    DisplaySettings {
        timestamps: display_timestamps(),
        timestamp_format: display_timestamp_format(),
        view: display_view(),
        hex_bytes_per_row: display_hex_bytes_per_row(),
        show_control_chars: display_show_control_chars(),
        wrap: display_wrap(),
        decoded_inline: display_decoded_inline(),
        hide_framed_bytes: display_hide_framed_bytes(),
    }
}

/// The bundled light and dark theme names, for a `theme` object that names only one.
pub(super) fn theme_names() -> (String, String) {
    (get(&["theme", "light"]), get(&["theme", "dark"]))
}

/// A `Settings` with every key at its bundled default.
pub(super) fn bundled_settings() -> Settings {
    static SETTINGS: LazyLock<Settings> = LazyLock::new(|| {
        Settings::deserialize(&Value::Object(serde_json::Map::new()))
            .unwrap_or_else(|err| panic!("the bundled default settings are invalid: {err}"))
    });
    SETTINGS.clone()
}
