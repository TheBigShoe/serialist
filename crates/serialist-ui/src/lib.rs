//! GPUI views. All GPUI and gpui-kit imports go through [`prelude`] so a snapshot bump
//! or a move to the official crate touches one file.
//!
//! Milestone 0 shell: a workspace with a Devices panel on the left, a session view in
//! the center (a plain line list until the terminal element lands in milestone 1), a
//! compose bar and a status line.

pub mod actions;
pub mod compose;
pub mod devices_panel;
mod drain;
pub mod line_buffer;
pub mod session_handle;
pub mod session_view;
pub mod workspace;

#[cfg(test)]
mod test_support;

pub use compose::{ComposeBar, ComposeEvent, History, LineEnding};
pub use devices_panel::{
    BaudError, DeviceEntry, DeviceList, DevicesPanel, DevicesPanelEvent, parse_baud,
};
pub use line_buffer::{Line, LineBuffer, LineKind, LineSplitter, RxText};
pub use session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
pub use session_view::{ConnectionState, SessionModel, SessionUpdate, SessionView};
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

    // GPUI exports its `test` attribute unconditionally, and a glob import outranks the
    // language prelude, so `use crate::prelude::*` would silently turn every `#[test]`
    // into GPUI's. Re-exporting the built-in by name wins over the glob; GPUI's attribute
    // stays reachable as `gpui_test`.
    pub use ::core::prelude::v1::test;
    pub use gpui_kit::test as gpui_test;

    pub use gpui_kit::component::button::{Button, ButtonVariants};
    pub use gpui_kit::component::input::{Input, InputEvent, InputState};
    pub use gpui_kit::component::{
        ActiveTheme, Disableable, Sizable, StyledExt, Theme, ThemeMode, h_flex, h_resizable,
        resizable_panel, v_flex,
    };
}
