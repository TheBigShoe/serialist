//! GPUI views. All GPUI and gpui-kit imports go through [`prelude`] so a snapshot bump
//! or a move to the official crate touches one file.
//!
//! A workspace with the Devices and Commands panels on the left, a session view in the
//! center (the terminal element over the session's page store, and a compose bar, or
//! the terminal alone in inline mode), the Decoded panel and the Script console on the
//! right, and a status line. Settings, themes, fonts, key bindings and codec plugins come
//! from Zed-format files and plugin folders through [`config`], and apply to the running
//! app as the files change.

pub mod actions;
pub mod capture;
pub mod codecs;
pub mod commands_panel;
pub mod compose;
pub mod config;
pub mod decoded_panel;
pub mod devices_panel;
pub mod dialog_footer;
pub mod export;
pub mod fonts;
pub mod framed;
pub mod history;
pub mod inline;
pub mod keymap;
pub mod param_prompt;
pub mod script_bridge;
pub mod script_console;
pub mod script_files;
pub mod scrollback;
pub mod session_handle;
pub mod session_options;
pub mod session_view;
pub mod status;
pub mod terminal;
pub mod theme_bridge;
pub mod workspace;

#[cfg(test)]
mod commands_tests;
#[cfg(test)]
mod config_tests;
#[cfg(test)]
mod decoded_tests;
#[cfg(test)]
mod gate;
#[cfg(test)]
mod inline_tests;
#[cfg(test)]
mod script_tests;
#[cfg(test)]
mod stream_tests;
#[cfg(test)]
mod test_support;

pub use capture::{Recorder, RecorderStats, RecordingSink, RecordingSlot};
pub use codecs::{CodecSelection, CodecSet, CodecSlotSink, PluginCodec, PluginProblem};
pub use commands_panel::{CommandEditor, CommandsPanel, CommandsPanelEvent, EditorSeed};
pub use compose::{ComposeBar, ComposeEvent, History, LineEnding, LineEndingExt};
pub use config::{Config, ConfigPiece, ConfigProblem, Opener};
pub use decoded_panel::{DecodedPanel, FrameTable};
pub use devices_panel::{
    BaudError, DeviceEntry, DeviceList, DevicesPanel, DevicesPanelEvent, parse_baud,
};
pub use dialog_footer::DialogButtons;
pub use export::{ExportFormat, ExportJob, FramesFormat};
pub use fonts::{TerminalFont, UiFont};
pub use framed::{FilteredText, FramedFilter};
pub use history::PersistentHistory;
pub use inline::{InlineConfig, KeyEncoder, Mode, encode_key};
pub use param_prompt::{ParamPrompt, ParamPromptEvent};
pub use script_bridge::{CommandsSnapshot, ConsoleKind, ConsoleLine, ScriptEnv, SessionScripts};
pub use script_console::{ScriptConsole, ScriptConsoleEvent, ScriptPrompt};
pub use script_files::{ScriptEntry, list_scripts, resolve_script};
pub use scrollback::{Floored, Floors, Scrollback};
pub use session_handle::{
    CoreSessionOpener, SessionControl, SessionHandle, SessionOpener, SharedSession,
};
pub use session_options::{DisplayDefaults, SessionOptions};
pub use session_view::{ActiveCodec, SessionView, SessionViewEvent};
pub use status::{ConnectionState, Notice, PauseMark, RecordingStatus, ScriptStatus, StatusLine};
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
    // The bundled icon set (chevrons, the dialog's close X, ...). An app registers it with
    // `Application::with_assets`; without one every SVG icon renders as nothing.
    pub use gpui_kit::assets::Assets;

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
    pub use gpui_kit::component::dialog::{
        Cancel, Confirm, Dialog, DialogButtonProps, DialogFooter,
    };
    pub use gpui_kit::component::tooltip::Tooltip;
    pub use gpui_kit::component::{WindowExt, v_resizable};
    // For `#[derive(JsonSchema)]` on actions with fields; GPUI's derive names the trait
    // through its private re-export, and `#[schemars(crate = …)]` points the derive here.
    pub use gpui_kit::component::IndexPath;
    pub use gpui_kit::component::highlighter::HighlightThemeStyle;
    pub use gpui_kit::component::input::{Input, InputEvent, InputState};
    pub use gpui_kit::component::scroll::{Scrollbar, ScrollbarHandle};
    pub use gpui_kit::component::select::{Select, SelectEvent, SelectState};
    pub use gpui_kit::component::table::{
        Column, DataTable, TableDelegate, TableEvent, TableState,
    };
    pub use gpui_kit::component::{
        ActiveTheme, Disableable, Sizable, StyledExt, Theme, ThemeConfig, ThemeConfigColors,
        ThemeMode, h_flex, h_resizable, resizable_panel, v_flex,
    };
    pub use gpui_kit::private::schemars::{self, JsonSchema};
}
