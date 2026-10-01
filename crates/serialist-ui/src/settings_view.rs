//! The Settings screen: a front end to `settings.json` (and `keymap.json`), opened in a
//! center tab by `serialist::OpenSettingsUi` (`cmd-,`, `ctrl-,` elsewhere), the app menu's
//! "Settings…" or the command palette.
//!
//! # The files stay the source of truth
//!
//! The view shows what the files on disk say: it reads the bundled defaults, the user's
//! `settings.json` and a project's `.serialist/settings.json` itself (see
//! [`settings_io`]), and reads them again whenever the [`Config`] global is installed, so
//! an edit made in another editor shows here as soon as the watcher has reloaded it.
//!
//! Every control writes one key the moment it changes (a text field 300 ms after the
//! last keystroke, or at once on Enter or when it loses focus) through the
//! comment-preserving editor in `serialist-core`, which keeps the file's comments and
//! layout. The watcher then reloads the file into the running app, exactly as for a save
//! from an editor. A value the control can check (a font size, a `strftime` format, a
//! keystroke) is checked before it is written; a write the loader rejects anyway is
//! undone, and the loader's message shows under the control.
//!
//! A value nobody set shows a quiet "default" marker; one the user's file sets has a
//! reset button that removes the key ("Default"); one a project file sets is read-only
//! here, marked "project". A `settings.json` that does not load when the screen opens
//! shows the loader's error and an "Open settings.json" button instead of the form.
//!
//! # Layout
//!
//! A column of sections on the left (Appearance, Terminal font, Display, Session,
//! Devices, Keymap, Plugins), the selected section's form on the right, scrolling, and a
//! footer with "Open settings.json", "Reveal config folder" and the config directory.
//! It uses the chrome's rhythm: 11 px uppercase group labels, 28 px rows, an 8 px grid.

mod devices;
mod keymap_list;
mod plugins;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use serialist_core::settings::{
    ConfigPaths, MAX_PASTE_CHUNK_BYTES, MAX_PASTE_CHUNK_DELAY_MS, validate_chord,
};
use serialist_core::{
    BackspaceKey, DisplayView, Emulation, LineEnding, LineHeight, PortSource, Settings, ThemeMode,
    ThemeSelection, TimestampMode,
};

pub use self::devices::{MatchKey, ProfileEditor, match_summary, profile_json};
pub use self::keymap_list::{BindingRow, BindingSource, binding_rows};
pub use self::plugins::{PluginRow, plugin_rows};
use crate::actions;
use crate::chrome;
use crate::config::{self, Config};
use crate::devices_panel::parse_baud;
use crate::fonts::{DEFAULT_MONO_FAMILY, installed_families};
use crate::keystroke_input::{KeystrokeInput, KeystrokeInputEvent};
use crate::port_settings::STANDARD_BAUDS;
use crate::prelude::*;
use crate::settings_io::{self, Origin, SettingsFiles};

/// How long a text field waits after the last keystroke before it writes.
pub const DEBOUNCE: Duration = Duration::from_millis(300);

/// The key context of the Settings view.
pub const CONTEXT: &str = "SettingsView";

/// The width of the section column.
const NAV_WIDTH: Pixels = px(184.);
/// The width of a row's label.
const LABEL_WIDTH: Pixels = px(176.);
/// The widest a row's control grows.
const CONTROL_MAX: Pixels = px(380.);
/// The width of the marker column at a row's end.
const MARKER_WIDTH: Pixels = px(72.);

const MIB: u64 = 1024 * 1024;

/// A section of the screen, listed in the left column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Appearance,
    TerminalFont,
    Display,
    Session,
    Devices,
    Keymap,
    Plugins,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::Appearance,
        Section::TerminalFont,
        Section::Display,
        Section::Session,
        Section::Devices,
        Section::Keymap,
        Section::Plugins,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "Appearance",
            Section::TerminalFont => "Terminal font",
            Section::Display => "Display",
            Section::Session => "Session",
            Section::Devices => "Devices",
            Section::Keymap => "Keymap",
            Section::Plugins => "Plugins",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Section::Appearance => "Theme and the font of panels, tabs and the status line.",
            Section::TerminalFont => "The font of the scrollback.",
            Section::Display => "How each new session shows what it receives.",
            Section::Session => "Defaults for opening a port, and inline mode.",
            Section::Devices => {
                "Per-device profiles. The first profile whose match fits a port applies when it connects."
            }
            Section::Keymap => "Every key binding in effect, the bundled ones and yours.",
            Section::Plugins => "Codec plugins installed in the plugins folder.",
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::Appearance => IconName::Palette,
            Section::TerminalFont => IconName::Type,
            Section::Display => IconName::Monitor,
            Section::Session => IconName::Plug,
            Section::Devices => IconName::Usb,
            Section::Keymap => IconName::Keyboard,
            Section::Plugins => IconName::Puzzle,
        }
    }

    /// The element id of its entry in the section column.
    pub fn nav_id(self) -> &'static str {
        match self {
            Section::Appearance => "settings-nav-appearance",
            Section::TerminalFont => "settings-nav-terminal-font",
            Section::Display => "settings-nav-display",
            Section::Session => "settings-nav-session",
            Section::Devices => "settings-nav-devices",
            Section::Keymap => "settings-nav-keymap",
            Section::Plugins => "settings-nav-plugins",
        }
    }
}

// --- Text fields ---------------------------------------------------------------------------

/// A setting edited as text: a family name, a number, a format string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    UiFontFamily,
    UiFontSize,
    BufferFontFamily,
    BufferFontSize,
    /// The custom `buffer_line_height` multiple.
    BufferLineHeight,
    BufferFallbacks,
    TerminalFontFamily,
    TerminalFontSize,
    TerminalLineHeight,
    TerminalFallbacks,
    TimestampFormat,
    HexBytesPerRow,
    ScrollbackMib,
    DefaultBaud,
    /// The escape chord typed as text, beside the field that records it.
    EscapeChord,
    PasteChunkBytes,
    PasteChunkDelay,
}

impl Field {
    const ALL: [Field; 17] = [
        Field::UiFontFamily,
        Field::UiFontSize,
        Field::BufferFontFamily,
        Field::BufferFontSize,
        Field::BufferLineHeight,
        Field::BufferFallbacks,
        Field::TerminalFontFamily,
        Field::TerminalFontSize,
        Field::TerminalLineHeight,
        Field::TerminalFallbacks,
        Field::TimestampFormat,
        Field::HexBytesPerRow,
        Field::ScrollbackMib,
        Field::DefaultBaud,
        Field::EscapeChord,
        Field::PasteChunkBytes,
        Field::PasteChunkDelay,
    ];

    /// The JSON pointer of the key it writes.
    pub fn pointer(self) -> &'static str {
        match self {
            Field::UiFontFamily => "/ui_font_family",
            Field::UiFontSize => "/ui_font_size",
            Field::BufferFontFamily => "/buffer_font_family",
            Field::BufferFontSize => "/buffer_font_size",
            Field::BufferLineHeight => "/buffer_line_height",
            Field::BufferFallbacks => "/buffer_font_fallbacks",
            Field::TerminalFontFamily => "/terminal/font_family",
            Field::TerminalFontSize => "/terminal/font_size",
            Field::TerminalLineHeight => "/terminal/line_height",
            Field::TerminalFallbacks => "/terminal/font_fallbacks",
            Field::TimestampFormat => "/display/timestamp_format",
            Field::HexBytesPerRow => "/display/hex_bytes_per_row",
            Field::ScrollbackMib => "/scrollback_budget_bytes",
            Field::DefaultBaud => "/default_baud",
            Field::EscapeChord => "/inline/escape_chord",
            Field::PasteChunkBytes => "/inline/paste_chunk_bytes",
            Field::PasteChunkDelay => "/inline/paste_chunk_delay_ms",
        }
    }

    /// The element id of its input.
    pub fn id(self) -> &'static str {
        match self {
            Field::UiFontFamily => "settings-ui-font-family",
            Field::UiFontSize => "settings-ui-font-size",
            Field::BufferFontFamily => "settings-buffer-font-family",
            Field::BufferFontSize => "settings-buffer-font-size",
            Field::BufferLineHeight => "settings-buffer-line-height",
            Field::BufferFallbacks => "settings-buffer-fallbacks",
            Field::TerminalFontFamily => "settings-terminal-font-family",
            Field::TerminalFontSize => "settings-terminal-font-size",
            Field::TerminalLineHeight => "settings-terminal-line-height",
            Field::TerminalFallbacks => "settings-terminal-fallbacks",
            Field::TimestampFormat => "settings-timestamp-format",
            Field::HexBytesPerRow => "settings-hex-bytes-per-row",
            Field::ScrollbackMib => "settings-scrollback",
            Field::DefaultBaud => "settings-default-baud",
            Field::EscapeChord => "settings-escape-chord-text",
            Field::PasteChunkBytes => "settings-paste-chunk-bytes",
            Field::PasteChunkDelay => "settings-paste-chunk-delay",
        }
    }

    fn placeholder(self) -> String {
        match self {
            Field::UiFontFamily => "System UI font".to_owned(),
            Field::BufferFontFamily => DEFAULT_MONO_FAMILY.to_owned(),
            Field::TerminalFontFamily
            | Field::TerminalFontSize
            | Field::TerminalLineHeight
            | Field::TerminalFallbacks => "Same as the buffer font".to_owned(),
            Field::BufferFallbacks => "Family, family, \u{2026}".to_owned(),
            _ => String::new(),
        }
    }

    /// Stepper settings for a numeric field: step, minimum, maximum.
    fn number(self) -> Option<(f64, f64, f64)> {
        match self {
            Field::UiFontSize | Field::BufferFontSize => Some((1., 4., 128.)),
            Field::BufferLineHeight => Some((0.1, 1., 4.)),
            Field::HexBytesPerRow => Some((1., 1., 256.)),
            Field::ScrollbackMib => Some((64., 1., 1_048_576.)),
            Field::PasteChunkBytes => Some((16., 1., MAX_PASTE_CHUNK_BYTES as f64)),
            Field::PasteChunkDelay => Some((5., 0., MAX_PASTE_CHUNK_DELAY_MS as f64)),
            _ => None,
        }
    }

    /// The text the field shows for `settings`.
    pub fn show(self, settings: &Settings) -> String {
        let t = &settings.terminal;
        match self {
            Field::UiFontFamily => settings.ui_font_family.clone().unwrap_or_default(),
            Field::UiFontSize => number_text(f64::from(settings.ui_font_size)),
            Field::BufferFontFamily => settings.buffer_font_family.clone().unwrap_or_default(),
            Field::BufferFontSize => number_text(f64::from(settings.buffer_font_size)),
            Field::BufferLineHeight => number_text(f64::from(settings.buffer_line_height.value())),
            Field::BufferFallbacks => settings.buffer_font_fallbacks.join(", "),
            Field::TerminalFontFamily => t.font_family.clone().unwrap_or_default(),
            Field::TerminalFontSize => t
                .font_size
                .map(|size| number_text(f64::from(size)))
                .unwrap_or_default(),
            Field::TerminalLineHeight => match t.line_height {
                None => String::new(),
                Some(LineHeight::Comfortable) => "comfortable".to_owned(),
                Some(LineHeight::Standard) => "standard".to_owned(),
                Some(LineHeight::Custom(v)) => number_text(f64::from(v)),
            },
            Field::TerminalFallbacks => t
                .font_fallbacks
                .as_ref()
                .map(|list| list.join(", "))
                .unwrap_or_default(),
            Field::TimestampFormat => settings.display.timestamp_format.clone(),
            Field::HexBytesPerRow => settings.display.hex_bytes_per_row.to_string(),
            Field::ScrollbackMib => {
                number_text(settings.scrollback_budget_bytes as f64 / MIB as f64)
            }
            Field::DefaultBaud => settings.default_baud.to_string(),
            Field::EscapeChord => settings.inline.escape_chord.clone(),
            Field::PasteChunkBytes => settings.inline.paste_chunk_bytes.to_string(),
            Field::PasteChunkDelay => settings.inline.paste_chunk_delay_ms.to_string(),
        }
    }

    /// The value to write for `text`: `Ok(None)` removes the key (the default comes
    /// back), `Err` says why the text is not a value.
    pub fn parse(self, text: &str) -> Result<Option<Value>, String> {
        let text = text.trim();
        match self {
            Field::UiFontFamily | Field::BufferFontFamily | Field::TerminalFontFamily => {
                Ok((!text.is_empty()).then(|| Value::String(text.to_owned())))
            }
            Field::UiFontSize | Field::BufferFontSize => font_size(text).map(Some),
            Field::TerminalFontSize => {
                if text.is_empty() {
                    Ok(None)
                } else {
                    font_size(text).map(Some)
                }
            }
            Field::BufferLineHeight => line_height_number(text).map(Some),
            Field::TerminalLineHeight => match text.to_ascii_lowercase().as_str() {
                "" => Ok(None),
                "comfortable" | "standard" => Ok(Some(Value::String(text.to_ascii_lowercase()))),
                _ => line_height_number(text).map(Some),
            },
            Field::BufferFallbacks => {
                let list = family_list(text);
                Ok((!list.is_empty()).then(|| json!(list)))
            }
            Field::TerminalFallbacks => Ok((!text.is_empty()).then(|| json!(family_list(text)))),
            Field::TimestampFormat => {
                if text.is_empty() {
                    return Ok(None);
                }
                timestamp_format_error(text).map_or(Ok(Some(Value::String(text.to_owned()))), Err)
            }
            Field::HexBytesPerRow => whole(text, 1, 256, "Bytes per row").map(|v| Some(json!(v))),
            Field::ScrollbackMib => {
                let mib: f64 = text
                    .parse()
                    .map_err(|_| format!("{text:?} is not a number of MiB"))?;
                if !(mib.is_finite() && mib >= 1.) {
                    return Err("The scrollback budget must be at least 1 MiB".to_owned());
                }
                Ok(Some(json!((mib * MIB as f64).round() as u64)))
            }
            Field::DefaultBaud => parse_baud(text)
                .map(|baud| Some(json!(baud)))
                .map_err(|error| format!("Baud: {error}")),
            Field::EscapeChord => chord(text).map(|chord| Some(Value::String(chord))),
            Field::PasteChunkBytes => {
                whole(text, 1, MAX_PASTE_CHUNK_BYTES as u64, "Chunk size").map(|v| Some(json!(v)))
            }
            Field::PasteChunkDelay => {
                whole(text, 0, MAX_PASTE_CHUNK_DELAY_MS, "Delay").map(|v| Some(json!(v)))
            }
        }
    }
}

/// `18` for 18.0, `1.5` for 1.5: what a person would type.
fn number_text(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{value:.0}")
    } else {
        let text = format!("{value:.3}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    }
}

/// A number as JSON: an integer when it is whole, so `18` is written as `18`.
fn number_json(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn font_size(text: &str) -> Result<Value, String> {
    let size: f64 = text
        .parse()
        .map_err(|_| format!("{text:?} is not a font size"))?;
    if !(4.0..=128.0).contains(&size) {
        return Err(format!("A font size must be from 4 to 128, got {text}"));
    }
    Ok(number_json(size))
}

fn line_height_number(text: &str) -> Result<Value, String> {
    let value: f64 = text
        .parse()
        .map_err(|_| format!("{text:?} is not a line height: comfortable, standard or a number"))?;
    if !(1.0..=4.0).contains(&value) {
        return Err(format!("A line height must be from 1 to 4, got {text}"));
    }
    Ok(number_json(value))
}

fn family_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|family| !family.is_empty())
        .map(str::to_owned)
        .collect()
}

fn whole(text: &str, min: u64, max: u64, what: &str) -> Result<u64, String> {
    match text.parse::<u64>() {
        Ok(value) if (min..=max).contains(&value) => Ok(value),
        _ => Err(format!("{what} must be a whole number from {min} to {max}")),
    }
}

/// Why `format` is not a `strftime` format chrono can use, if it is not.
pub fn timestamp_format_error(format: &str) -> Option<String> {
    use chrono::format::{Item, StrftimeItems};
    StrftimeItems::new(format)
        .any(|item| matches!(item, Item::Error))
        .then(|| format!("{format:?} is not a strftime format (try %H:%M:%S%.3f)"))
}

/// What `format` makes of the time now, for the preview under the field.
pub fn timestamp_preview(format: &str) -> String {
    if timestamp_format_error(format).is_some() {
        return String::new();
    }
    chrono::Local::now().format(format).to_string()
}

/// `text` as an escape chord: a keystroke the settings accept and GPUI can parse.
fn chord(text: &str) -> Result<String, String> {
    let chord = validate_chord(text)?;
    Keystroke::parse(&chord).map_err(|error| format!("{chord:?}: {error}"))?;
    Ok(chord)
}

struct TextField {
    input: Entity<InputState>,
    /// The write waiting for typing to pause.
    pending: Option<Task<()>>,
}

// --- Pickers ---------------------------------------------------------------------------

/// A setting picked from a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pick {
    LightTheme,
    DarkTheme,
    UiWeight,
    BufferWeight,
    TerminalWeight,
    /// Installed families, which fill the UI font field.
    UiFamily,
    BufferFamily,
    TerminalFamily,
    /// The standard rates, which fill the default baud field.
    Baud,
}

impl Pick {
    const ALL: [Pick; 9] = [
        Pick::LightTheme,
        Pick::DarkTheme,
        Pick::UiWeight,
        Pick::BufferWeight,
        Pick::TerminalWeight,
        Pick::UiFamily,
        Pick::BufferFamily,
        Pick::TerminalFamily,
        Pick::Baud,
    ];
}

type Choice = Entity<SelectState<Vec<String>>>;

/// Font weights by name, as the weight pickers list them.
const WEIGHTS: [(u16, &str); 9] = [
    (100, "Thin"),
    (200, "Extra Light"),
    (300, "Light"),
    (400, "Regular"),
    (500, "Medium"),
    (600, "Semibold"),
    (700, "Bold"),
    (800, "Extra Bold"),
    (900, "Black"),
];

/// What the terminal weight picker calls "no override".
const SAME_AS_BUFFER: &str = "Same as the buffer font";

fn weight_label(weight: f32) -> String {
    WEIGHTS
        .iter()
        .find(|(value, _)| f32::from(*value) == weight)
        .map_or_else(
            || number_text(f64::from(weight)),
            |(value, name)| format!("{value} {name}"),
        )
}

fn weight_items() -> Vec<String> {
    WEIGHTS
        .iter()
        .map(|(value, name)| format!("{value} {name}"))
        .collect()
}

fn weight_of(label: &str) -> Option<u16> {
    label.split_whitespace().next()?.parse().ok()
}

/// The theme selection as a mode and a light and a dark name: a plain theme name pins
/// that theme, light or dark as the theme itself is.
fn theme_parts(selection: &ThemeSelection, cx: &App) -> (ThemeMode, String, String) {
    match selection {
        ThemeSelection::Dynamic { mode, light, dark } => (*mode, light.clone(), dark.clone()),
        ThemeSelection::Static(name) => {
            let defaults = match Settings::default().theme {
                ThemeSelection::Dynamic { light, dark, .. } => (light, dark),
                ThemeSelection::Static(name) => (name.clone(), name),
            };
            let dark = cx
                .try_global::<Config>()
                .and_then(|config| config.themes().get(name).map(|theme| theme.is_dark()))
                .unwrap_or(true);
            if dark {
                (ThemeMode::Dark, defaults.0, name.clone())
            } else {
                (ThemeMode::Light, name.clone(), defaults.1)
            }
        }
    }
}

fn mode_name(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::System => "system",
        ThemeMode::Light => "light",
        ThemeMode::Dark => "dark",
    }
}

// --- The view ----------------------------------------------------------------------------

pub struct SettingsView {
    section: Section,
    /// The files as last read; `None` until one load succeeds.
    files: Option<SettingsFiles>,
    /// Why `settings.json` does not load, while it does not.
    broken: Option<String>,
    fields: HashMap<Field, TextField>,
    picks: HashMap<Pick, Choice>,
    theme_names: Vec<String>,
    escape_chord: Entity<KeystrokeInput>,
    /// Problems by JSON pointer, shown under the row that writes it.
    errors: HashMap<String, String>,
    /// The Devices section's list, and the profile form while it is open.
    devices: devices::DevicesState,
    /// The Keymap section's filter, selection and recorder.
    keymap: keymap_list::KeymapState,
    /// What the last example-plugin install said, and whether it failed.
    plugin_notice: Option<(String, bool)>,
    port_source: Option<Arc<dyn PortSource>>,
    scroll: ScrollHandle,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SettingsView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Why nothing is written while the configuration is the bundled defaults alone.
const NOT_LOADED: &str =
    "The configuration was not read from a directory, so there is no file to write";

/// Whether the configuration was read from its directory, so its files may be written.
fn is_loaded(cx: &App) -> bool {
    cx.try_global::<Config>().is_some_and(Config::is_loaded)
}

/// The files the installed configuration was read from, or the bundled defaults alone
/// when it was not read from a directory (nothing is read or written then).
fn read_files(cx: &App) -> Result<SettingsFiles, String> {
    if is_loaded(cx) {
        SettingsFiles::read(&paths_of(cx))
    } else {
        Ok(SettingsFiles::bundled())
    }
}

fn paths_of(cx: &App) -> ConfigPaths {
    cx.try_global::<Config>()
        .map(|config| config.paths().clone())
        .unwrap_or_else(ConfigPaths::default_for_platform)
}

impl SettingsView {
    /// The screen over the configuration installed now. `port_source` lists the ports
    /// whose identities the device-profile form offers as match hints.
    pub fn new(
        port_source: Option<Arc<dyn PortSource>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (files, broken) = match read_files(cx) {
            Ok(files) => (Some(files), None),
            Err(message) => (None, Some(message)),
        };
        let settings = files
            .as_ref()
            .map(|files| files.settings.clone())
            .unwrap_or_default();
        let mut subscriptions = Vec::new();

        let mut fields = HashMap::new();
        for field in Field::ALL {
            let text = field.show(&settings);
            let input = cx.new(|cx| {
                let input = InputState::new(window, cx)
                    .placeholder(field.placeholder())
                    .default_value(text);
                match field.number() {
                    Some((step, min, max)) => input.step(step).min(min).max(max),
                    None => input,
                }
            });
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => this.field_changed(field, window, cx),
                    InputEvent::PressEnter { .. } | InputEvent::Blur => {
                        this.flush(field, window, cx);
                    }
                    InputEvent::Focus => {}
                },
            ));
            fields.insert(
                field,
                TextField {
                    input,
                    pending: None,
                },
            );
        }

        let theme_names: Vec<String> = cx
            .try_global::<Config>()
            .map(|config| config.themes().names().map(str::to_owned).collect())
            .unwrap_or_default();
        let families: Vec<String> = installed_families(cx).to_vec();
        let mut picks = HashMap::new();
        for pick in Pick::ALL {
            let (items, searchable) = match pick {
                Pick::LightTheme | Pick::DarkTheme => (theme_names.clone(), true),
                Pick::UiWeight | Pick::BufferWeight => (weight_items(), false),
                Pick::TerminalWeight => {
                    let mut items = vec![SAME_AS_BUFFER.to_owned()];
                    items.extend(weight_items());
                    (items, false)
                }
                Pick::UiFamily | Pick::BufferFamily | Pick::TerminalFamily => {
                    (families.clone(), true)
                }
                Pick::Baud => (STANDARD_BAUDS.iter().map(u32::to_string).collect(), false),
            };
            let selected = Self::pick_value(pick, &settings, cx);
            let index = selected
                .as_ref()
                .and_then(|value| items.iter().position(|item| item == value))
                .map(IndexPath::new);
            let select =
                cx.new(|cx| SelectState::new(items, index, window, cx).searchable(searchable));
            subscriptions.push(cx.subscribe_in(
                &select,
                window,
                move |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                    if let SelectEvent::Confirm(Some(value)) = event {
                        this.picked(pick, value, window, cx);
                    }
                },
            ));
            picks.insert(pick, select);
        }

        let chord = Keystroke::parse(&settings.inline.escape_chord).ok();
        let escape_chord = cx.new(|cx| KeystrokeInput::new("settings-escape-chord", chord, cx));
        subscriptions.push(cx.subscribe_in(
            &escape_chord,
            window,
            |this, _, event: &KeystrokeInputEvent, window, cx| match event {
                KeystrokeInputEvent::Captured(keystroke) => {
                    this.set_escape_chord(
                        &crate::keystroke_input::keystroke_text(keystroke),
                        window,
                        cx,
                    );
                }
            },
        ));

        let keymap = keymap_list::KeymapState::new(window, cx, &mut subscriptions);

        // The watcher's reloads, and any other install: read the files again.
        subscriptions.push(cx.observe_global_in::<Config>(window, |this, window, cx| {
            this.reload(window, cx);
        }));

        Self {
            section: Section::Appearance,
            files,
            broken,
            fields,
            picks,
            theme_names,
            escape_chord,
            errors: HashMap::new(),
            devices: devices::DevicesState::default(),
            keymap,
            plugin_notice: None,
            port_source,
            scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    // --- Reading -------------------------------------------------------------------------

    pub fn section(&self) -> Section {
        self.section
    }

    /// Why `settings.json` does not load, while the screen shows that instead of the form.
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// The settings the form shows: the files merged, as last read.
    pub fn settings(&self) -> Settings {
        self.files
            .as_ref()
            .map(|files| files.settings.clone())
            .unwrap_or_default()
    }

    /// Where the value at `pointer` comes from.
    pub fn origin(&self, pointer: &str) -> Origin {
        self.files
            .as_ref()
            .map_or(Origin::Default, |files| files.origin(pointer))
    }

    /// The problem shown under the row that writes `pointer`.
    pub fn error(&self, pointer: &str) -> Option<&str> {
        self.errors.get(pointer).map(String::as_str)
    }

    /// The input of a text field.
    pub fn input(&self, field: Field) -> &Entity<InputState> {
        &self.fields[&field].input
    }

    /// The text a field holds now.
    pub fn field_text(&self, field: Field, cx: &App) -> String {
        self.input(field).read(cx).value().to_string()
    }

    /// A picker's state.
    pub fn picker(&self, pick: Pick) -> &Choice {
        &self.picks[&pick]
    }

    pub fn escape_chord_input(&self) -> &Entity<KeystrokeInput> {
        &self.escape_chord
    }

    fn paths(&self, cx: &App) -> ConfigPaths {
        paths_of(cx)
    }

    fn locked(&self, pointer: &str) -> bool {
        matches!(self.origin(pointer), Origin::Project(_))
    }

    // --- Following the files -------------------------------------------------------------

    /// Read the files again and show what they say, leaving alone a field that is being
    /// typed in.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match read_files(cx) {
            Ok(files) => {
                self.files = Some(files);
                self.broken = None;
                self.sync_controls(window, cx);
            }
            Err(message) => self.broken = Some(message),
        }
        cx.notify();
    }

    fn sync_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.settings();
        for (field, text_field) in &self.fields {
            if text_field.pending.is_some() {
                continue;
            }
            let shown = field.show(&settings);
            let input = text_field.input.clone();
            let (current, focused) = {
                let state = input.read(cx);
                (
                    state.value().to_string(),
                    state.focus_handle(cx).is_focused(window),
                )
            };
            if current == shown {
                continue;
            }
            // Typing that says the same thing in other words ("18.0" for 18) stays.
            if focused && field.parse(&current).ok() == Some(field.parse(&shown).ok().flatten()) {
                continue;
            }
            input.update(cx, |input, cx| input.set_value(shown, window, cx));
        }

        let names: Vec<String> = cx
            .try_global::<Config>()
            .map(|config| config.themes().names().map(str::to_owned).collect())
            .unwrap_or_default();
        if names != self.theme_names {
            self.theme_names = names.clone();
            for pick in [Pick::LightTheme, Pick::DarkTheme] {
                let items = names.clone();
                self.picks[&pick].update(cx, |select, cx| select.set_items(items, window, cx));
            }
        }
        for pick in Pick::ALL {
            let Some(value) = Self::pick_value(pick, &settings, cx) else {
                continue;
            };
            self.picks[&pick].update(cx, |select, cx| {
                if select.selected_value() != Some(&value) {
                    select.set_selected_value(&value, window, cx);
                }
            });
        }
        let chord = Keystroke::parse(&settings.inline.escape_chord).ok();
        self.escape_chord
            .update(cx, |input, cx| input.set_keystroke(chord, cx));
    }

    /// The item a picker shows for `settings`.
    fn pick_value(pick: Pick, settings: &Settings, cx: &App) -> Option<String> {
        match pick {
            Pick::LightTheme => Some(theme_parts(&settings.theme, cx).1),
            Pick::DarkTheme => Some(theme_parts(&settings.theme, cx).2),
            Pick::UiWeight => Some(weight_label(settings.ui_font_weight)),
            Pick::BufferWeight => Some(weight_label(settings.buffer_font_weight)),
            Pick::TerminalWeight => Some(
                settings
                    .terminal
                    .font_weight
                    .map_or_else(|| SAME_AS_BUFFER.to_owned(), weight_label),
            ),
            Pick::UiFamily => settings.ui_font_family.clone(),
            Pick::BufferFamily => settings.buffer_font_family.clone(),
            Pick::TerminalFamily => settings.terminal.font_family.clone(),
            Pick::Baud => Some(settings.default_baud.to_string()),
        }
    }

    // --- Writing -------------------------------------------------------------------------

    /// Write `value` at `pointer` in `settings.json`, or remove the key (`None`). A write
    /// the loader rejects is undone and its message shown under the row.
    pub fn write(
        &mut self,
        pointer: &str,
        value: Option<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.locked(pointer) {
            return;
        }
        if !is_loaded(cx) {
            self.errors
                .insert(pointer.to_owned(), NOT_LOADED.to_owned());
            cx.notify();
            return;
        }
        match settings_io::write_setting(&self.paths(cx), pointer, value) {
            Ok(()) => {
                self.errors.remove(pointer);
            }
            Err(message) => {
                self.errors.insert(pointer.to_owned(), message);
            }
        }
        self.reload(window, cx);
    }

    /// Remove the key at `pointer`, putting the default back: a row's reset button.
    pub fn reset(&mut self, pointer: &str, window: &mut Window, cx: &mut Context<Self>) {
        tracing::info!(pointer, "reset a setting to its default");
        for (field, text_field) in self.fields.iter_mut() {
            if field.pointer() == pointer {
                text_field.pending = None;
            }
        }
        self.write(pointer, None, window, cx);
    }

    fn field_changed(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            this.update_in(cx, |this, window, cx| this.commit(field, window, cx))
                .ok();
        });
        if let Some(text_field) = self.fields.get_mut(&field) {
            text_field.pending = Some(task);
        }
    }

    /// Write a field now if it has a change waiting: Enter, or leaving it.
    fn flush(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .fields
            .get(&field)
            .is_some_and(|text_field| text_field.pending.is_some())
        {
            self.commit(field, window, cx);
        }
    }

    /// Check a field's text and write it, unless it says what the file says already.
    pub fn commit(&mut self, field: Field, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text_field) = self.fields.get_mut(&field) {
            text_field.pending = None;
        }
        let text = self.field_text(field, cx);
        let pointer = field.pointer();
        match field.parse(&text) {
            Ok(value) => {
                let shown = field.show(&self.settings());
                if field.parse(&shown).ok() == Some(value.clone()) && text.trim() == shown {
                    self.errors.remove(pointer);
                    cx.notify();
                    return;
                }
                if value.is_none() && !matches!(self.origin(pointer), Origin::User) {
                    // Emptied a field nobody set: nothing to remove.
                    self.errors.remove(pointer);
                    cx.notify();
                    return;
                }
                self.write(pointer, value, window, cx);
            }
            Err(message) => {
                self.errors.insert(pointer.to_owned(), message);
                cx.notify();
            }
        }
    }

    /// Put `text` in a field and write it at once, as typing it and pressing Enter does.
    pub fn enter(&mut self, field: Field, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input(field).clone();
        input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        self.commit(field, window, cx);
    }

    fn picked(&mut self, pick: Pick, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        match pick {
            Pick::LightTheme => self.set_theme_name(false, value, window, cx),
            Pick::DarkTheme => self.set_theme_name(true, value, window, cx),
            Pick::UiWeight | Pick::BufferWeight => {
                if let Some(weight) = weight_of(value) {
                    let pointer = if pick == Pick::UiWeight {
                        "/ui_font_weight"
                    } else {
                        "/buffer_font_weight"
                    };
                    self.write(pointer, Some(json!(weight)), window, cx);
                }
            }
            Pick::TerminalWeight => {
                let value = weight_of(value).map(|weight| json!(weight));
                self.write("/terminal/font_weight", value, window, cx);
            }
            Pick::UiFamily => self.enter(Field::UiFontFamily, value, window, cx),
            Pick::BufferFamily => self.enter(Field::BufferFontFamily, value, window, cx),
            Pick::TerminalFamily => self.enter(Field::TerminalFontFamily, value, window, cx),
            Pick::Baud => self.enter(Field::DefaultBaud, value, window, cx),
        }
    }

    /// Show `section`.
    pub fn select_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if self.section != section {
            self.section = section;
            self.scroll.set_offset(point(px(0.), px(0.)));
            self.keymap.stop_recording(cx);
            cx.notify();
        }
    }

    // --- The controls' writes ------------------------------------------------------------

    fn write_theme(
        &mut self,
        mode: ThemeMode,
        light: String,
        dark: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let value = json!({ "mode": mode_name(mode), "light": light, "dark": dark });
        self.write("/theme", Some(value), window, cx);
    }

    /// Follow the system appearance, or pin light or dark.
    pub fn set_theme_mode(&mut self, mode: ThemeMode, window: &mut Window, cx: &mut Context<Self>) {
        let (_, light, dark) = theme_parts(&self.settings().theme, cx);
        self.write_theme(mode, light, dark, window, cx);
    }

    /// The theme used in light (`dark` false) or dark appearance.
    pub fn set_theme_name(
        &mut self,
        dark: bool,
        name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (mode, mut light_name, mut dark_name) = theme_parts(&self.settings().theme, cx);
        if dark {
            dark_name = name.to_owned();
        } else {
            light_name = name.to_owned();
        }
        self.write_theme(mode, light_name, dark_name, window, cx);
    }

    /// Ligatures on or off: `buffer_font_features.calt`.
    pub fn set_ligatures(&mut self, on: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.write("/buffer_font_features/calt", Some(json!(on)), window, cx);
    }

    /// `buffer_line_height`: comfortable, standard, or the custom field's number.
    pub fn set_line_height(&mut self, choice: usize, window: &mut Window, cx: &mut Context<Self>) {
        let value = match choice {
            0 => json!("comfortable"),
            1 => json!("standard"),
            _ => number_json(f64::from(self.settings().buffer_line_height.value())),
        };
        self.write("/buffer_line_height", Some(value), window, cx);
    }

    pub fn set_switch(
        &mut self,
        pointer: &'static str,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write(pointer, Some(json!(on)), window, cx);
    }

    /// The escape chord, from the recorder or the text field.
    pub fn set_escape_chord(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.enter(Field::EscapeChord, text, window, cx);
    }

    // --- Rendering -----------------------------------------------------------------------

    fn render_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (active, hover, muted, foreground) = (
            theme.list_active,
            theme.list_hover,
            theme.muted_foreground,
            theme.foreground,
        );
        v_flex()
            .id("settings-nav")
            .flex_none()
            .w(NAV_WIDTH)
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(chrome::panel_header("Settings", cx))
            .children(Section::ALL.into_iter().map(|section| {
                let selected = section == self.section;
                h_flex()
                    .id(section.nav_id())
                    .test_support()
                    .h(chrome::ROW_HEIGHT)
                    .mx_1()
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .text_sm()
                    .cursor_pointer()
                    .text_color(if selected { foreground } else { muted })
                    .when(selected, |row| row.bg(active))
                    .when(!selected, |row| row.hover(|row| row.bg(hover)))
                    .child(Icon::new(section.icon()).size_4())
                    .child(section.title())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_section(section, cx);
                    }))
            }))
    }

    /// A labelled row: the control, then its origin marker (a reset button when the
    /// user's file sets it), with the row's problem or `hint` under it.
    fn row(
        &self,
        label: &str,
        pointer: &'static str,
        control: impl IntoElement,
        hint: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.row_marked(label, pointer, control, hint, true, cx)
    }

    /// [`Self::row`], with the origin marker left out (`marked` false) on a row whose
    /// key another row already marks, so a reset button's id stays unique.
    fn row_marked(
        &self,
        label: &str,
        pointer: &'static str,
        control: impl IntoElement,
        hint: Option<String>,
        marked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let origin = self.origin(pointer);
        // The row that marks the key also says what went wrong with it.
        let error = self.errors.get(pointer).filter(|_| marked).cloned();
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let marker: AnyElement = match &origin {
            Origin::Default => div()
                .id(SharedString::from(format!("default{pointer}")))
                .test_support()
                .child(chrome::quiet_chip("default", cx))
                .tooltip(|window, cx| {
                    Tooltip::new("Not in settings.json: the bundled default").build(window, cx)
                })
                .into_any_element(),
            Origin::User => chrome::icon_button(
                SharedString::from(format!("reset{pointer}")),
                IconName::RotateCcw,
                cx,
            )
            .xsmall()
            .tooltip("Default: remove the key from settings.json")
            .on_click(cx.listener(move |this, _, window, cx| {
                this.reset(pointer, window, cx);
            }))
            .into_any_element(),
            Origin::Project(path) => {
                let tip = SharedString::from(format!(
                    "Set in {}; edit that file to change it",
                    path.display()
                ));
                div()
                    .id(SharedString::from(format!("project{pointer}")))
                    .test_support()
                    .child(chrome::chip(theme.info).child("project"))
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .into_any_element()
            }
        };
        let note = error
            .map(|error| (error, danger))
            .or_else(|| match &origin {
                Origin::Project(path) if marked => {
                    Some((format!("Read-only here: set in {}", path.display()), muted))
                }
                _ => hint.map(|hint| (hint, muted)),
            });
        v_flex()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .min_h(chrome::ROW_HEIGHT)
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(LABEL_WIDTH)
                            .flex_none()
                            .text_sm()
                            .child(SharedString::from(label.to_owned())),
                    )
                    .child(div().flex_1().min_w_0().max_w(CONTROL_MAX).child(control))
                    .child(
                        h_flex()
                            .w(MARKER_WIDTH)
                            .flex_none()
                            .justify_end()
                            .when(marked, |this| this.child(marker)),
                    ),
            )
            .children(note.map(|(text, color)| {
                div()
                    .id(SharedString::from(format!("note{pointer}")))
                    .test_support()
                    .pl(LABEL_WIDTH + px(8.))
                    .pb_1()
                    .text_xs()
                    .text_color(color)
                    .child(SharedString::from(text))
            }))
            .into_any_element()
    }

    /// A group of rows under an 11 px uppercase label.
    fn group(&self, title: &str, rows: Vec<AnyElement>, cx: &App) -> AnyElement {
        v_flex()
            .w_full()
            .gap_2()
            .pt_4()
            .child(chrome::section_label(title, cx))
            .children(rows)
            .into_any_element()
    }

    fn text_input(&self, field: Field) -> AnyElement {
        let locked = self.locked(field.pointer());
        let input = &self.fields[&field].input;
        if field.number().is_some() {
            NumberInput::new(input)
                .small()
                .disabled(locked)
                .into_any_element()
        } else {
            Input::new(input)
                .id(field.id())
                .small()
                .disabled(locked)
                .into_any_element()
        }
    }

    fn select(&self, pick: Pick, placeholder: &str, pointer: &str) -> Select<Vec<String>> {
        Select::new(&self.picks[&pick])
            .small()
            .placeholder(SharedString::from(placeholder.to_owned()))
            .menu_width(px(280.))
            .disabled(self.locked(pointer))
    }

    /// A text field with a list beside it that fills it.
    fn field_with_list(&self, field: Field, pick: Pick, list: &str) -> AnyElement {
        h_flex()
            .gap_1()
            .child(div().flex_1().min_w_0().child(self.text_input(field)))
            .child(
                div()
                    .w(px(120.))
                    .flex_none()
                    .child(self.select(pick, list, field.pointer())),
            )
            .into_any_element()
    }

    /// Buttons side by side, one selected: a choice among a few values.
    fn segmented(
        &self,
        id: &'static str,
        labels: &[&'static str],
        selected: Option<usize>,
        pointer: &str,
        on_pick: impl Fn(&mut Self, usize, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let locked = self.locked(pointer);
        let (muted, foreground) = (cx.theme().muted_foreground, cx.theme().foreground);
        ButtonGroup::new(id)
            .small()
            .outline()
            .children(labels.iter().enumerate().map(|(ix, label)| {
                let on = selected == Some(ix);
                Button::new(SharedString::from(format!("{id}-{ix}")))
                    .label(*label)
                    .selected(on)
                    .disabled(locked)
                    .text_color(if on { foreground } else { muted })
            }))
            .on_click(cx.listener(move |this, clicked: &Vec<usize>, window, cx| {
                if let Some(ix) = clicked.first() {
                    on_pick(this, *ix, window, cx);
                }
            }))
            .into_any_element()
    }

    fn switch(
        &self,
        id: &'static str,
        pointer: &'static str,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Switch::new(id)
            .checked(on)
            .disabled(self.locked(pointer))
            .on_click(cx.listener(move |this, on: &bool, window, cx| {
                this.set_switch(pointer, *on, window, cx);
            }))
            .into_any_element()
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = self.settings();
        let (mode, _, _) = theme_parts(&settings.theme, cx);
        let mode_ix = match mode {
            ThemeMode::System => 0,
            ThemeMode::Light => 1,
            ThemeMode::Dark => 2,
        };
        let static_hint = matches!(settings.theme, ThemeSelection::Static(_)).then(|| {
            "settings.json names one theme; a change here writes the { mode, light, dark } form"
                .to_owned()
        });
        let modes = [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark];
        let theme_rows = vec![
            self.row(
                "Mode",
                "/theme",
                self.segmented(
                    "settings-theme-mode",
                    &["System", "Light", "Dark"],
                    Some(mode_ix),
                    "/theme",
                    move |this, ix, window, cx| this.set_theme_mode(modes[ix], window, cx),
                    cx,
                ),
                static_hint,
                cx,
            ),
            self.row_marked(
                "Light theme",
                "/theme",
                self.select(Pick::LightTheme, "Theme", "/theme"),
                None,
                false,
                cx,
            ),
            self.row_marked(
                "Dark theme",
                "/theme",
                self.select(Pick::DarkTheme, "Theme", "/theme"),
                Some("Zed theme files in the themes folder are listed too".to_owned()),
                false,
                cx,
            ),
        ];
        let font_rows = vec![
            self.row(
                "Family",
                Field::UiFontFamily.pointer(),
                self.field_with_list(Field::UiFontFamily, Pick::UiFamily, "Installed"),
                None,
                cx,
            ),
            self.row(
                "Size",
                Field::UiFontSize.pointer(),
                self.text_input(Field::UiFontSize),
                None,
                cx,
            ),
            self.row(
                "Weight",
                "/ui_font_weight",
                self.select(Pick::UiWeight, "Weight", "/ui_font_weight"),
                None,
                cx,
            ),
        ];
        vec![
            self.group("Theme", theme_rows, cx),
            self.group("UI font", font_rows, cx),
        ]
    }

    fn render_terminal_font(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = self.settings();
        let overridden = |key: &str, set: bool| {
            set.then(|| format!("terminal.{key} overrides this for the terminal (below)"))
        };
        let t = &settings.terminal;
        let line_choice = match settings.buffer_line_height {
            LineHeight::Comfortable => 0,
            LineHeight::Standard => 1,
            LineHeight::Custom(_) => 2,
        };
        let custom = line_choice == 2;
        let line_height = h_flex()
            .gap_2()
            .child(self.segmented(
                "settings-line-height",
                &["Comfortable", "Standard", "Custom"],
                Some(line_choice),
                "/buffer_line_height",
                |this, ix, window, cx| this.set_line_height(ix, window, cx),
                cx,
            ))
            .when(custom, |row| {
                row.child(
                    div()
                        .w(px(110.))
                        .child(self.text_input(Field::BufferLineHeight)),
                )
            });
        let ligatures = settings.buffer_font_features.ligatures_enabled();
        let buffer = vec![
            self.row(
                "Family",
                Field::BufferFontFamily.pointer(),
                self.field_with_list(Field::BufferFontFamily, Pick::BufferFamily, "Installed"),
                overridden("font_family", t.font_family.is_some()),
                cx,
            ),
            self.row(
                "Size",
                Field::BufferFontSize.pointer(),
                self.text_input(Field::BufferFontSize),
                overridden("font_size", t.font_size.is_some()),
                cx,
            ),
            self.row(
                "Weight",
                "/buffer_font_weight",
                self.select(Pick::BufferWeight, "Weight", "/buffer_font_weight"),
                overridden("font_weight", t.font_weight.is_some()),
                cx,
            ),
            self.row(
                "Line height",
                "/buffer_line_height",
                line_height,
                overridden("line_height", t.line_height.is_some()),
                cx,
            ),
            self.row(
                "Ligatures",
                "/buffer_font_features/calt",
                self.switch(
                    "settings-ligatures",
                    "/buffer_font_features/calt",
                    ligatures,
                    cx,
                ),
                Some("Writes buffer_font_features.calt".to_owned()),
                cx,
            ),
            self.row(
                "Fallbacks",
                Field::BufferFallbacks.pointer(),
                self.text_input(Field::BufferFallbacks),
                Some("Families tried in order for glyphs the main one lacks".to_owned()),
                cx,
            ),
        ];
        let features = t.font_features.as_ref().map(|features| {
            let text: Vec<String> = features
                .iter()
                .map(|(tag, value)| format!("{tag}: {value}"))
                .collect();
            format!("{{ {} }}", text.join(", "))
        });
        let mut overrides = vec![
            self.row(
                "Family",
                Field::TerminalFontFamily.pointer(),
                self.field_with_list(Field::TerminalFontFamily, Pick::TerminalFamily, "Installed"),
                None,
                cx,
            ),
            self.row(
                "Size",
                Field::TerminalFontSize.pointer(),
                self.text_input(Field::TerminalFontSize),
                None,
                cx,
            ),
            self.row(
                "Weight",
                "/terminal/font_weight",
                self.select(
                    Pick::TerminalWeight,
                    SAME_AS_BUFFER,
                    "/terminal/font_weight",
                ),
                None,
                cx,
            ),
            self.row(
                "Line height",
                Field::TerminalLineHeight.pointer(),
                self.text_input(Field::TerminalLineHeight),
                Some("comfortable, standard or a number".to_owned()),
                cx,
            ),
            self.row(
                "Fallbacks",
                Field::TerminalFallbacks.pointer(),
                self.text_input(Field::TerminalFallbacks),
                None,
                cx,
            ),
        ];
        if let Some(features) = features {
            overrides.push(self.row(
                "Features",
                "/terminal/font_features",
                div().text_sm().child(SharedString::from(features)),
                Some("Edit in settings.json".to_owned()),
                cx,
            ));
        }
        vec![
            self.group("Buffer font", buffer, cx),
            self.group("Terminal overrides", overrides, cx),
        ]
    }

    fn render_display(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = self.settings();
        let display = &settings.display;
        let stamps = [
            TimestampMode::Off,
            TimestampMode::Absolute,
            TimestampMode::Relative,
            TimestampMode::Delta,
        ];
        let stamp_names = ["off", "absolute", "relative", "delta"];
        let views = [DisplayView::Text, DisplayView::Hex, DisplayView::HexAscii];
        let view_names = ["text", "hex", "hex_ascii"];
        let format_text = self.field_text(Field::TimestampFormat, cx);
        let preview = timestamp_preview(&format_text);
        let preview = (!preview.is_empty()).then(|| format!("Now: {preview}"));
        let budget_mib = settings.scrollback_budget_bytes / MIB;
        let rows = vec![
            self.row(
                "Timestamps",
                "/display/timestamps",
                self.segmented(
                    "settings-timestamps",
                    &["Off", "Absolute", "Relative", "Delta"],
                    stamps.iter().position(|mode| *mode == display.timestamps),
                    "/display/timestamps",
                    move |this, ix, window, cx| {
                        this.write(
                            "/display/timestamps",
                            Some(json!(stamp_names[ix])),
                            window,
                            cx,
                        );
                    },
                    cx,
                ),
                None,
                cx,
            ),
            self.row(
                "Timestamp format",
                Field::TimestampFormat.pointer(),
                self.text_input(Field::TimestampFormat),
                preview,
                cx,
            ),
            self.row(
                "View",
                "/display/view",
                self.segmented(
                    "settings-view",
                    &["Text", "Hex", "Hex + ASCII"],
                    views.iter().position(|view| *view == display.view),
                    "/display/view",
                    move |this, ix, window, cx| {
                        this.write("/display/view", Some(json!(view_names[ix])), window, cx);
                    },
                    cx,
                ),
                None,
                cx,
            ),
            self.row(
                "Hex bytes per row",
                Field::HexBytesPerRow.pointer(),
                self.text_input(Field::HexBytesPerRow),
                None,
                cx,
            ),
            self.row(
                "Show control characters",
                "/display/show_control_chars",
                self.switch(
                    "settings-control-chars",
                    "/display/show_control_chars",
                    display.show_control_chars,
                    cx,
                ),
                Some("CR, LF and ESC as dim glyphs, in sessions opened after a change".to_owned()),
                cx,
            ),
            self.row(
                "Wrap long lines",
                "/display/wrap",
                self.switch("settings-wrap", "/display/wrap", display.wrap, cx),
                None,
                cx,
            ),
            self.row(
                "Decoded frames inline",
                "/display/decoded_inline",
                self.switch(
                    "settings-decoded-inline",
                    "/display/decoded_inline",
                    display.decoded_inline,
                    cx,
                ),
                Some("A summary line per decoded frame in the scrollback".to_owned()),
                cx,
            ),
            self.row(
                "Hide framed bytes",
                "/display/hide_framed_bytes",
                self.switch(
                    "settings-hide-framed",
                    "/display/hide_framed_bytes",
                    display.hide_framed_bytes,
                    cx,
                ),
                Some("Leave lines of decoded binary frames out of the text view".to_owned()),
                cx,
            ),
        ];
        let emulations = [Emulation::Monitor, Emulation::Vt];
        let emulation_names = ["monitor", "vt"];
        let terminal = vec![
            self.row(
                "Emulation",
                "/terminal/emulation",
                self.segmented(
                    "settings-emulation",
                    &["Monitor", "VT"],
                    emulations
                        .iter()
                        .position(|mode| *mode == settings.terminal.emulation),
                    "/terminal/emulation",
                    move |this, ix, window, cx| {
                        let name = emulation_names[ix];
                        this.write("/terminal/emulation", Some(json!(name)), window, cx);
                    },
                    cx,
                ),
                Some(
                    "Monitor logs lines; VT draws a terminal screen (boot menus, consoles). \
                     A device profile's emulation wins"
                        .to_owned(),
                ),
                cx,
            ),
            self.row(
                "Blink the VT cursor",
                "/terminal/cursor_blink",
                self.switch(
                    "settings-cursor-blink",
                    "/terminal/cursor_blink",
                    settings.terminal.cursor_blink,
                    cx,
                ),
                None,
                cx,
            ),
        ];
        let memory = vec![self.row(
            "Scrollback budget (MiB)",
            Field::ScrollbackMib.pointer(),
            self.text_input(Field::ScrollbackMib),
            Some(format!(
                "Memory each session's scrollback may use: {budget_mib} MiB"
            )),
            cx,
        )];
        vec![
            self.group("New sessions", rows, cx),
            self.group("Terminal", terminal, cx),
            self.group("Memory", memory, cx),
        ]
    }

    fn render_session(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = self.settings();
        let endings = LineEnding::ALL;
        let ending_names = ["none", "cr", "lf", "crlf"];
        let backspaces = [BackspaceKey::Del, BackspaceKey::Bs];
        let port = vec![
            self.row(
                "Default baud",
                Field::DefaultBaud.pointer(),
                self.field_with_list(Field::DefaultBaud, Pick::Baud, "Standard"),
                Some("For a port no device profile matches".to_owned()),
                cx,
            ),
            self.row(
                "Line ending",
                "/line_ending",
                self.segmented(
                    "settings-line-ending",
                    &["None", "CR", "LF", "CRLF"],
                    endings
                        .iter()
                        .position(|ending| *ending == settings.line_ending),
                    "/line_ending",
                    move |this, ix, window, cx| {
                        this.write("/line_ending", Some(json!(ending_names[ix])), window, cx);
                    },
                    cx,
                ),
                None,
                cx,
            ),
            self.row(
                "Local echo",
                "/local_echo",
                self.switch(
                    "settings-local-echo",
                    "/local_echo",
                    settings.local_echo,
                    cx,
                ),
                None,
                cx,
            ),
            self.row(
                "Restore session",
                "/restore_session",
                self.switch(
                    "settings-restore-session",
                    "/restore_session",
                    settings.restore_session,
                    cx,
                ),
                Some("Reopen the tabs open at the last quit".to_owned()),
                cx,
            ),
        ];
        let locked_chord = self.locked(Field::EscapeChord.pointer());
        let chord = h_flex()
            .gap_1()
            .child(
                div()
                    .flex_none()
                    .when(locked_chord, |this| this.opacity(0.5))
                    .child(self.escape_chord.clone()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.text_input(Field::EscapeChord)),
            );
        let inline = vec![
            self.row(
                "Backspace sends",
                "/inline/backspace",
                self.segmented(
                    "settings-backspace",
                    &["DEL (0x7f)", "BS (0x08)"],
                    backspaces
                        .iter()
                        .position(|key| *key == settings.inline.backspace),
                    "/inline/backspace",
                    move |this, ix, window, cx| {
                        let name = backspaces[ix].name();
                        this.write("/inline/backspace", Some(json!(name)), window, cx);
                    },
                    cx,
                ),
                None,
                cx,
            ),
            self.row(
                "Escape chord",
                Field::EscapeChord.pointer(),
                chord,
                Some("Click the box and press the chord, or type it".to_owned()),
                cx,
            ),
            self.row(
                "Paste chunk (bytes)",
                Field::PasteChunkBytes.pointer(),
                self.text_input(Field::PasteChunkBytes),
                None,
                cx,
            ),
            self.row(
                "Paste delay (ms)",
                Field::PasteChunkDelay.pointer(),
                self.text_input(Field::PasteChunkDelay),
                Some("Between chunks, for bootloaders that drop bytes".to_owned()),
                cx,
            ),
        ];
        vec![
            self.group("Opening a port", port, cx),
            self.group("Inline mode", inline, cx),
        ]
    }

    fn render_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let groups = match self.section {
            Section::Appearance => self.render_appearance(cx),
            Section::TerminalFont => self.render_terminal_font(cx),
            Section::Display => self.render_display(cx),
            Section::Session => self.render_session(cx),
            Section::Devices => self.render_devices(cx),
            Section::Keymap => self.render_keymap(cx),
            Section::Plugins => self.render_plugins(cx),
        };
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(
                v_flex()
                    .flex_none()
                    .px_4()
                    .pt_3()
                    .gap_1()
                    .child(
                        div()
                            .text_base()
                            .text_color(theme.foreground)
                            .child(self.section.title()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.section.summary()),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("settings-form")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .px_4()
                        .pb_4()
                        .children(groups),
                ),
            )
            .into_any_element()
    }

    fn render_broken(&self, message: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id("settings-broken")
            .test_support()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .px_8()
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size_6()
                    .text_color(theme.danger),
            )
            .child(
                div()
                    .text_base()
                    .text_color(theme.foreground)
                    .child("settings.json does not load"),
            )
            .child(
                div()
                    .id("settings-broken-message")
                    .test_support()
                    .max_w(px(640.))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .font_family(theme.mono_font_family.clone())
                    .child(SharedString::from(message.to_owned())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Fix it in an editor; this screen comes back once it loads."),
            )
            .child(
                Button::new("settings-broken-open")
                    .icon(IconName::FileCode)
                    .label("Open settings.json")
                    .small()
                    .primary()
                    .on_click(|_, _, cx| actions::open_settings(cx)),
            )
            .into_any_element()
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let paths = self.paths(cx);
        let theme = cx.theme();
        let dir = paths.dir.clone();
        h_flex()
            .id("settings-footer")
            .flex_none()
            .h(px(40.))
            .px_3()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("settings-open-json")
                    .icon(IconName::FileCode)
                    .label("Open settings.json")
                    .small()
                    .ghost()
                    .on_click(|_, _, cx| actions::open_settings(cx)),
            )
            .child(
                Button::new("settings-reveal-folder")
                    .icon(IconName::FolderOpen)
                    .label("Reveal config folder")
                    .small()
                    .ghost()
                    .on_click(move |_, _, cx| {
                        if let Err(error) = std::fs::create_dir_all(&dir) {
                            tracing::error!(%error, "could not create the config folder");
                        }
                        config::open_path(&dir, cx);
                    }),
            )
            .child(
                div()
                    .id("settings-config-dir")
                    .test_support()
                    .ml_auto()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(paths.dir.display().to_string())),
            )
            .into_any_element()
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.broken.clone() {
            Some(message) => self.render_broken(&message, cx),
            None => h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(self.render_nav(cx))
                .child(self.render_form(cx))
                .into_any_element(),
        };
        let footer = self.render_footer(cx);
        let theme = cx.theme();
        v_flex()
            .id("settings-view")
            .test_support()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(div().flex_1().min_h_0().w_full().flex().child(body))
            .child(footer)
    }
}
