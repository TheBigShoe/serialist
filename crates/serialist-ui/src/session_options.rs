//! What a new session starts with, from the settings and the device profile that
//! matches its port: the store's size, the line ending, local echo and the display
//! defaults (`display.*`). The status line and key bindings change them per session
//! afterwards.
//!
//! `display.show_control_chars` is not a view setting but a parser one: it goes to the
//! session's store, which parses received bytes once, when they arrive. A change to the
//! setting therefore applies to sessions opened after it, not to ones already open.

use serialist_core::{DisplayView, LineEnding, PortInfo, Settings, StoreConfig, TimestampMode};

use crate::scrollback::HEX_BYTES_PER_ROW;
use crate::terminal::DisplayMode;

impl From<DisplayView> for DisplayMode {
    /// The hex view always has its ASCII column, so `hex` and `hex_ascii` are one mode.
    fn from(view: DisplayView) -> Self {
        match view {
            DisplayView::Text => DisplayMode::Text,
            DisplayView::Hex | DisplayView::HexAscii => DisplayMode::Hex,
        }
    }
}

/// How the terminal shows a session until the user says otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayDefaults {
    pub wrap: bool,
    pub timestamps: TimestampMode,
    pub view: DisplayMode,
    pub hex_bytes_per_row: usize,
    /// `display.decoded_inline`: a summary line per decoded frame in the scrollback.
    pub decoded_inline: bool,
    /// `display.hide_framed_bytes`: leave lines of binary frames out of the text view.
    pub hide_framed_bytes: bool,
}

impl Default for DisplayDefaults {
    fn default() -> Self {
        Self {
            wrap: false,
            timestamps: TimestampMode::Off,
            view: DisplayMode::Text,
            hex_bytes_per_row: HEX_BYTES_PER_ROW,
            decoded_inline: true,
            hide_framed_bytes: false,
        }
    }
}

/// Everything a session view is opened with besides the session itself.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    pub store: StoreConfig,
    /// What Enter appends.
    pub line_ending: LineEnding,
    /// Echo sent lines into the scrollback.
    pub local_echo: bool,
    pub display: DisplayDefaults,
    /// The codec to decode with from the start: the matching device profile's `plugin`.
    /// `None` decodes nothing until one is picked.
    pub codec: Option<String>,
}

impl Default for SessionOptions {
    /// A default store, CRLF, sent lines echoed, text view, no codec.
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            line_ending: LineEnding::default(),
            local_echo: true,
            display: DisplayDefaults::default(),
            codec: None,
        }
    }
}

impl SessionOptions {
    /// The options for `port` under `settings`: the scrollback budget, `display.*`,
    /// `local_echo`, and the line ending and codec (`plugin`) of the first matching
    /// device profile, else `line_ending` and no codec. `display.show_control_chars` is
    /// the store's, so it is read here, when the session opens.
    pub fn from_settings(settings: &Settings, port: &PortInfo) -> Self {
        let display = &settings.display;
        Self {
            store: StoreConfig {
                show_control_chars: display.show_control_chars,
                ..StoreConfig::with_budget(settings.scrollback_budget())
            },
            line_ending: settings.line_ending_for(port),
            local_echo: settings.local_echo,
            display: DisplayDefaults {
                wrap: display.wrap,
                timestamps: display.timestamps,
                view: display.view.into(),
                hex_bytes_per_row: display.hex_bytes_per_row,
                decoded_inline: display.decoded_inline,
                hide_framed_bytes: display.hide_framed_bytes,
            },
            codec: settings
                .profile_for(port)
                .and_then(|profile| profile.plugin.clone())
                .filter(|name| !name.trim().is_empty()),
        }
    }

    /// These options with the store sized as `store` says instead.
    pub fn with_store(self, store: StoreConfig) -> Self {
        Self { store, ..self }
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::{PortId, PortKind, UsbInfo};

    use super::*;

    fn usb(product: &str) -> PortInfo {
        PortInfo {
            id: PortId::new("/dev/cu.usbmodem1"),
            kind: PortKind::Usb(UsbInfo {
                vid: 0x0e8d,
                pid: 0x2000,
                serial_number: None,
                manufacturer: None,
                product: Some(product.to_owned()),
            }),
            display_name: product.to_owned(),
        }
    }

    #[test]
    fn settings_and_the_matching_profile_fill_the_options() {
        let settings = Settings::from_jsonc(
            r#"{
                "scrollback_budget_bytes": 67108864,
                "line_ending": "lf",
                "local_echo": true,
                "display": { "wrap": true, "timestamps": "delta", "view": "hex_ascii",
                             "hex_bytes_per_row": 8, "decoded_inline": false,
                             "hide_framed_bytes": true },
                "devices": [ { "match": { "product": "Airoha" }, "eol": "cr",
                               "plugin": "airoha-race" } ]
            }"#,
        )
        .unwrap();
        let airoha = SessionOptions::from_settings(&settings, &usb("Airoha BT"));
        assert_eq!(airoha.store.budget, 64 << 20);
        assert_eq!(airoha.line_ending, LineEnding::Cr, "the profile's eol wins");
        assert!(airoha.local_echo);
        assert_eq!(
            airoha.display,
            DisplayDefaults {
                wrap: true,
                timestamps: TimestampMode::Delta,
                view: DisplayMode::Hex,
                hex_bytes_per_row: 8,
                decoded_inline: false,
                hide_framed_bytes: true,
            }
        );
        assert_eq!(airoha.codec.as_deref(), Some("airoha-race"));
        let other = SessionOptions::from_settings(&settings, &usb("Something else"));
        assert_eq!(other.line_ending, LineEnding::Lf);
        assert_eq!(other.codec, None, "no profile, no codec");
    }

    #[test]
    fn control_characters_are_the_stores_to_parse() {
        let shown = |json: &str| {
            let settings = Settings::from_jsonc(json).unwrap();
            SessionOptions::from_settings(&settings, &usb("x"))
                .store
                .show_control_chars
        };
        assert!(!shown("{}"), "off unless asked for");
        assert!(shown(r#"{ "display": { "show_control_chars": true } }"#));
        assert!(!shown(r#"{ "display": { "show_control_chars": false } }"#));
        // The rest of the store's configuration is as it was.
        let settings =
            Settings::from_jsonc(r#"{ "display": { "show_control_chars": true } }"#).unwrap();
        let options = SessionOptions::from_settings(&settings, &usb("x"));
        assert_eq!(options.store.budget, settings.scrollback_budget());
        assert_eq!(
            options.store.max_line_bytes,
            StoreConfig::default().max_line_bytes
        );
    }

    #[test]
    fn bundled_settings_match_the_old_behavior_except_echo() {
        let options = SessionOptions::from_settings(&Settings::default(), &usb("x"));
        assert_eq!(options.line_ending, LineEnding::Crlf);
        assert_eq!(options.display, DisplayDefaults::default());
        assert!(!options.local_echo, "the bundled default is no local echo");
    }
}
