//! GPUI views. All GPUI and gpui-kit imports go through [`prelude`] so a snapshot bump
//! or a move to the official crate touches one file.
//!
//! A workspace with a Devices panel on the left, a session view in the center (the
//! terminal element over the session's page store, and a compose bar) and a status
//! line. Settings, themes, fonts and key bindings come from Zed-format files through
//! [`config`], and apply to the running app as the files change.

pub mod actions;
pub mod capture;
pub mod compose;
pub mod config;
pub mod devices_panel;
pub mod export;
pub mod fonts;
pub mod keymap;
pub mod scrollback;
pub mod session_handle;
pub mod session_options;
pub mod session_view;
pub mod status;
pub mod terminal;
pub mod theme_bridge;
pub mod workspace;

#[cfg(test)]
mod gate;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod test_support;

pub use capture::{Recorder, RecorderStats, RecordingSink, RecordingSlot};
pub use compose::{ComposeBar, ComposeEvent, History, LineEnding, LineEndingExt};
pub use config::{Config, ConfigPiece, ConfigProblem, Opener};
pub use devices_panel::{
    BaudError, DeviceEntry, DeviceList, DevicesPanel, DevicesPanelEvent, parse_baud,
};
pub use export::{ExportFormat, ExportJob};
pub use fonts::{TerminalFont, UiFont};
pub use scrollback::{Floored, Floors, Scrollback};
pub use session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
pub use session_options::{DisplayDefaults, SessionOptions};
pub use session_view::SessionView;
pub use status::{ConnectionState, Notice, PauseMark, RecordingStatus, StatusLine};
pub use workspace::{AppOptions, Workspace, init, open_main_window};

pub mod prelude {
    //! The only door to GPUI and gpui-kit. Everything else in the app imports
    //! `crate::prelude::*` (or `serialist_ui::prelude::*` from the binary).

    // gpui-kit re-exports all of GPUI at its root, plus `application()` from the
    // platform crate, `init()` and `open_window()`. The last two get unambiguous names
    // because this crate defines its own `init`.
    pub use gpui_kit::prelude::*;
    pub use gpui_kit::*;
    pub use gpui_kit::{init as kit_init, open_window as kit_open_window};

    // Headless UI-test helpers; they exist only with gpui-kit's `test-support` feature,
    // which the dev-dependency turns on.
    #[cfg(test)]
    pub use gpui_kit::test::{TestAppContextExt, TestWindowExt};
    // GPUI's input-event trait, for tests that dispatch raw mouse events. Imported
    // anonymously: its name collides with gpui-kit's `InputEvent` below.
    #[cfg(test)]
    pub use gpui_kit::InputEvent as _;

    // GPUI exports its `test` attribute unconditionally, and a glob import outranks the
    // language prelude, so `use crate::prelude::*` would silently turn every `#[test]`
    // into GPUI's. Re-exporting the built-in by name wins over the glob; GPUI's attribute
    // stays reachable as `gpui_test`.
    pub use ::core::prelude::v1::test;
    pub use gpui_kit::test as gpui_test;

    pub use gpui_kit::component::button::{Button, ButtonVariants};
    pub use gpui_kit::component::highlighter::HighlightThemeStyle;
    pub use gpui_kit::component::input::{Input, InputEvent, InputState};
    pub use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle};
    pub use gpui_kit::component::{
        ActiveTheme, Disableable, Sizable, StyledExt, Theme, ThemeConfig, ThemeConfigColors,
        ThemeMode, h_flex, h_resizable, resizable_panel, v_flex,
    };
}
