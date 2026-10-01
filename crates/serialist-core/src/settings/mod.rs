//! Settings in Zed's vocabulary: fonts, theme selection, display formats, session
//! defaults and per-device profiles.
//!
//! Files are JSON with comments and trailing commas. Loading layers three documents
//! and deserializes the result:
//!
//! 1. the bundled `assets/default_settings.jsonc`,
//! 2. the user's `settings.json` in the config directory,
//! 3. a project-local `.serialist/settings.json`.
//!
//! Objects merge key by key, arrays and scalars replace, and `null` unsets an optional
//! key, so a user file may hold a single key. Unknown keys are ignored and reported as
//! [`SettingsWarning`]s on [`Settings::warnings`]; a wrong type or bad syntax is a
//! [`SettingsError`] naming the file, line and column.

mod de;
mod defaults;
mod edit;
mod font;
mod inline;
mod keymap_edit;
mod load;
mod paths;
mod profile;
mod types;

#[cfg(test)]
mod inline_tests;
#[cfg(test)]
mod tests;

pub use defaults::DEFAULT_SETTINGS_JSONC;
pub use edit::{EditError, KeyChange, SettingsEditor};
pub use font::{FontFeatures, FontSpec, LineHeight};
pub use inline::{
    BackspaceKey, InlineSettings, MAX_PASTE_CHUNK_BYTES, MAX_PASTE_CHUNK_DELAY_MS, validate_chord,
};
pub use keymap_edit::KeymapEditor;
pub use load::{
    SettingsError, SettingsLayer, SettingsWarning, load_settings, load_settings_from_layers,
};
pub use paths::{
    CONFIG_DIR_ENV, ConfigPaths, ExamplePlugin, Platform, keymap_template, settings_template,
};
pub use profile::{DeviceMatch, DeviceProfile, UsbId};
pub use types::{
    DisplaySettings, DisplayView, Emulation, LineEnding, Settings, TerminalSettings, ThemeMode,
    ThemeSelection, TimestampMode,
};

/// The nearest `.serialist/settings.json` in `cwd` or an ancestor.
pub fn project_settings_path(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    ConfigPaths::project_settings_path(cwd)
}
