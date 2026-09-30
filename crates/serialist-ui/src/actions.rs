//! Actions and default key bindings. Names follow Zed's `namespace::Action` style so a
//! Zed-format keymap file can rebind them once keymap loading lands.

use crate::prelude::*;

actions!(serialist, [Quit]);
actions!(terminal, [Clear, JumpToBottom]);
actions!(serial, [Connect, Disconnect]);
actions!(devices, [SelectNext, SelectPrevious]);
actions!(compose, [HistoryPrevious, HistoryNext, CycleLineEnding]);

/// Key contexts set by the views, referenced by the bindings below.
pub mod context {
    pub const WORKSPACE: &str = "Workspace";
    pub const DEVICES_PANEL: &str = "DevicesPanel";
    pub const SESSION_VIEW: &str = "SessionView";
    pub const COMPOSE_BAR: &str = "ComposeBar";
}

#[cfg(target_os = "macos")]
mod keys {
    pub const QUIT: &str = "cmd-q";
    pub const CLEAR: &str = "cmd-k";
    pub const DISCONNECT: &str = "cmd-w";
    pub const CYCLE_LINE_ENDING: &str = "cmd-e";
}

// Plain ctrl-k and ctrl-w belong to the device once inline mode sends keystrokes to the
// port, so the other platforms take the shifted chords.
#[cfg(not(target_os = "macos"))]
mod keys {
    pub const QUIT: &str = "ctrl-q";
    pub const CLEAR: &str = "ctrl-shift-k";
    pub const DISCONNECT: &str = "ctrl-shift-w";
    pub const CYCLE_LINE_ENDING: &str = "ctrl-shift-e";
}

pub fn bind_keys(cx: &mut App) {
    // The compose bindings sit on the gpui-kit Input's own context, one level deeper
    // than a plain "ComposeBar" binding could reach, and they are registered after
    // gpui-kit's, so they win over the Input's own up/down handling.
    let compose_input = format!("{} > Input", context::COMPOSE_BAR);
    cx.bind_keys([
        KeyBinding::new(keys::QUIT, Quit, None),
        KeyBinding::new(keys::CLEAR, Clear, Some(context::WORKSPACE)),
        KeyBinding::new(keys::DISCONNECT, Disconnect, Some(context::WORKSPACE)),
        KeyBinding::new("down", SelectNext, Some(context::DEVICES_PANEL)),
        KeyBinding::new("up", SelectPrevious, Some(context::DEVICES_PANEL)),
        KeyBinding::new("enter", Connect, Some(context::DEVICES_PANEL)),
        KeyBinding::new("up", HistoryPrevious, Some(&compose_input)),
        KeyBinding::new("down", HistoryNext, Some(&compose_input)),
        KeyBinding::new(
            keys::CYCLE_LINE_ENDING,
            CycleLineEnding,
            Some(context::COMPOSE_BAR),
        ),
    ]);
}
