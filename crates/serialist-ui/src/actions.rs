//! Actions and default key bindings. Names follow Zed's `namespace::Action` style so a
//! Zed-format keymap file can rebind them once keymap loading lands.

use crate::prelude::*;

actions!(serialist, [Quit]);
actions!(terminal, [Clear, JumpToBottom, Pause, Export, ToggleRecord]);
actions!(
    terminal,
    [
        /// Copy the selection to the clipboard.
        Copy,
        /// Select every retained line.
        SelectAll,
        /// Open the search bar, or focus it if open.
        Search,
        /// Close the search bar and cancel a search in flight.
        DismissSearch,
        SearchNext,
        SearchPrevious,
        ToggleWrap,
        /// Off, absolute, relative, delta, and around again.
        CycleTimestamps,
        ToggleHexView,
        /// The frame-time overlay.
        ToggleFrameStats,
        PageUp,
        PageDown,
        ScrollToTop,
    ]
);
actions!(serial, [Connect, Disconnect]);
actions!(devices, [SelectNext, SelectPrevious]);
actions!(compose, [HistoryPrevious, HistoryNext, CycleLineEnding]);

/// Key contexts set by the views, referenced by the bindings below.
pub mod context {
    pub const WORKSPACE: &str = "Workspace";
    pub const DEVICES_PANEL: &str = "DevicesPanel";
    pub const SESSION_VIEW: &str = "SessionView";
    pub const COMPOSE_BAR: &str = "ComposeBar";
    pub const TERMINAL: &str = "Terminal";
    pub const TERMINAL_SEARCH: &str = "TerminalSearch";
}

#[cfg(target_os = "macos")]
mod keys {
    pub const QUIT: &str = "cmd-q";
    pub const CLEAR: &str = "cmd-k";
    pub const DISCONNECT: &str = "cmd-w";
    pub const CYCLE_LINE_ENDING: &str = "cmd-e";
    pub const PAUSE: &str = "cmd-p";
    pub const EXPORT: &str = "cmd-s";
    pub const TOGGLE_RECORD: &str = "cmd-shift-r";
    pub const COPY: &str = "cmd-c";
    pub const SELECT_ALL: &str = "cmd-a";
    pub const SEARCH: &str = "cmd-f";
    pub const SCROLL_TO_TOP: &str = "cmd-up";
    pub const SCROLL_TO_BOTTOM: &str = "cmd-down";
    pub const TOGGLE_FRAME_STATS: &str = "cmd-alt-i";
}

// Plain ctrl chords belong to the device once inline mode sends keystrokes to the port
// (ctrl-s is XOFF), so the other platforms take shifted chords. Pause is the exception
// at plain ctrl-p; inline mode will need an escape for it.
#[cfg(not(target_os = "macos"))]
mod keys {
    pub const QUIT: &str = "ctrl-q";
    pub const CLEAR: &str = "ctrl-shift-k";
    pub const DISCONNECT: &str = "ctrl-shift-w";
    pub const CYCLE_LINE_ENDING: &str = "ctrl-shift-e";
    pub const PAUSE: &str = "ctrl-p";
    pub const EXPORT: &str = "ctrl-shift-s";
    pub const TOGGLE_RECORD: &str = "ctrl-shift-r";
    pub const COPY: &str = "ctrl-shift-c";
    pub const SELECT_ALL: &str = "ctrl-shift-a";
    pub const SEARCH: &str = "ctrl-shift-f";
    pub const SCROLL_TO_TOP: &str = "ctrl-home";
    pub const SCROLL_TO_BOTTOM: &str = "ctrl-end";
    pub const TOGGLE_FRAME_STATS: &str = "ctrl-alt-i";
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
        KeyBinding::new(keys::PAUSE, Pause, Some(context::WORKSPACE)),
        KeyBinding::new(keys::EXPORT, Export, Some(context::WORKSPACE)),
        KeyBinding::new(keys::TOGGLE_RECORD, ToggleRecord, Some(context::WORKSPACE)),
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
    bind_terminal_keys(cx);
}

/// The terminal's own bindings. They apply while the terminal view has focus; the
/// search field sits inside it, and the field's own Input bindings win there. Escape
/// reaches `DismissSearch` because the Input propagates an escape it has no use for.
fn bind_terminal_keys(cx: &mut App) {
    let terminal = Some(context::TERMINAL);
    cx.bind_keys([
        KeyBinding::new(keys::COPY, Copy, terminal),
        KeyBinding::new(keys::SELECT_ALL, SelectAll, terminal),
        KeyBinding::new(keys::SEARCH, Search, terminal),
        KeyBinding::new("escape", DismissSearch, Some(context::TERMINAL_SEARCH)),
        KeyBinding::new("alt-z", ToggleWrap, terminal),
        KeyBinding::new("alt-t", CycleTimestamps, terminal),
        KeyBinding::new("alt-h", ToggleHexView, terminal),
        KeyBinding::new(keys::TOGGLE_FRAME_STATS, ToggleFrameStats, terminal),
        KeyBinding::new("pageup", PageUp, terminal),
        KeyBinding::new("pagedown", PageDown, terminal),
        KeyBinding::new("home", ScrollToTop, terminal),
        KeyBinding::new("end", JumpToBottom, terminal),
        KeyBinding::new(keys::SCROLL_TO_TOP, ScrollToTop, terminal),
        KeyBinding::new(keys::SCROLL_TO_BOTTOM, JumpToBottom, terminal),
    ]);
}
