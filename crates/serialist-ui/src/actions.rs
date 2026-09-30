//! Actions, the app-level action handlers and the menu. Names follow Zed's
//! `namespace::Action` style; every action here registers under that name, which is
//! how a Zed-format keymap file names it (see [`keymap`](crate::keymap)). The default
//! bindings are the bundled keymap in `serialist-core`; [`keys`] restates the chords
//! tests press.

use crate::config::{self, Config};
use crate::prelude::*;

actions!(
    serialist,
    [
        Quit,
        /// Open settings.json, writing the commented template first if there is none.
        OpenSettings,
        /// Open keymap.json, writing the commented template first if there is none.
        OpenKeymap,
        /// Open the themes folder, creating it first if needed.
        OpenThemesFolder,
        /// Read settings, keymap and themes from disk again.
        ReloadConfig,
    ]
);
actions!(terminal, [Clear, JumpToBottom, Pause, Export, ToggleRecord]);
actions!(
    terminal,
    [
        /// Switch between inline mode (every keystroke goes to the port) and command
        /// mode (the compose bar and saved commands).
        ToggleInline,
        /// In inline mode, send the clipboard to the port in paced chunks.
        Paste,
    ]
);
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

/// The Commands panel's actions and [`commands::Send`](Send), in a module of their own:
/// `SelectNext` and `SelectPrevious` are also the Devices panel's.
pub mod commands {
    use serialist_core::CommandRef;

    use crate::prelude::*;

    actions!(
        commands,
        [
            /// Move the Commands panel's selection down.
            SelectNext,
            /// Move the Commands panel's selection up.
            SelectPrevious,
            /// Send the command selected in the Commands panel.
            SendSelected,
            /// Edit the command selected in the Commands panel.
            EditSelected,
            /// Open the command editor for a new command.
            NewCommand,
            /// Ask for a name and create an empty collection.
            NewCollection,
        ]
    );

    /// Send a saved command, named by its collection, group and name. Each command with a
    /// `keybinding` is bound to one of these; a keymap file can bind one too:
    /// `["commands::Send", { "collection": "AT basics", "group": "Basics", "name": "AT" }]`.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, JsonSchema, Action)]
    #[action(namespace = commands)]
    #[serde(deny_unknown_fields)]
    #[schemars(crate = "crate::prelude::schemars")]
    pub struct Send {
        pub collection: String,
        pub group: String,
        pub name: String,
    }

    impl Send {
        /// The command this sends.
        pub fn reference(&self) -> CommandRef {
            CommandRef::new(&self.collection, &self.group, &self.name)
        }
    }

    impl From<&CommandRef> for Send {
        fn from(reference: &CommandRef) -> Self {
            Self {
                collection: reference.collection.clone(),
                group: reference.group.clone(),
                name: reference.name.clone(),
            }
        }
    }
}

actions!(
    compose,
    [
        HistoryPrevious,
        HistoryNext,
        CycleLineEnding,
        /// Open the command editor with the compose bar's text.
        SaveAsCommand,
    ]
);

/// Key contexts set by the views, referenced by the bundled keymap.
pub mod context {
    pub const WORKSPACE: &str = "Workspace";
    pub const DEVICES_PANEL: &str = "DevicesPanel";
    pub const SESSION_VIEW: &str = "SessionView";
    pub const COMPOSE_BAR: &str = "ComposeBar";
    pub const COMMANDS_PANEL: &str = "CommandsPanel";
    pub const TERMINAL: &str = "Terminal";
    /// The terminal in inline mode, in place of `Terminal`: keys go to the port except
    /// the ones bound here.
    pub const TERMINAL_INLINE: &str = "TerminalInline";
    pub const TERMINAL_SEARCH: &str = "TerminalSearch";
}

/// The default chords tests press. The bundled keymap in `serialist-core` is where they
/// are bound; `test_chords_match_the_bundled_keymap` keeps the two in step.
#[cfg(all(test, target_os = "macos"))]
pub(crate) mod keys {
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
    pub const TOGGLE_INLINE: &str = "cmd-i";
    pub const PASTE: &str = "cmd-v";
    pub const SAVE_AS_COMMAND: &str = "cmd-alt-s";
}

// Plain ctrl chords belong to the device once inline mode sends keystrokes to the port
// (ctrl-s is XOFF), so the other platforms take shifted chords. Pause is the exception
// at plain ctrl-p; inline mode will need an escape for it.
#[cfg(all(test, not(target_os = "macos")))]
pub(crate) mod keys {
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
    pub const TOGGLE_INLINE: &str = "ctrl-i";
    pub const PASTE: &str = "ctrl-shift-v";
    pub const SAVE_AS_COMMAND: &str = "ctrl-alt-s";
}

/// App-level setup: remember gpui-kit's own key bindings (so a keymap reload can put
/// them back), register the handlers of the app-wide actions, and set the menu. Call
/// right after `gpui_kit::init`, before anything else binds keys.
pub fn init(cx: &mut App) {
    crate::keymap::snapshot_kit_bindings(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
    cx.on_action(|_: &OpenKeymap, cx| open_keymap(cx));
    cx.on_action(|_: &OpenThemesFolder, cx| open_themes_folder(cx));
    cx.on_action(|_: &ReloadConfig, cx| config::reload_all(cx));
    cx.set_menus([Menu::new("Serialist").items([
        MenuItem::action("Open Settings", OpenSettings),
        MenuItem::action("Open Keymap", OpenKeymap),
        MenuItem::action("Open Themes Folder", OpenThemesFolder),
        MenuItem::action("Reload Configuration", ReloadConfig),
        MenuItem::separator(),
        MenuItem::action("Quit Serialist", Quit),
    ])]);
}

fn paths(cx: &App) -> Option<serialist_core::settings::ConfigPaths> {
    cx.try_global::<Config>()
        .map(|config| config.paths().clone())
}

fn report(what: &str, error: std::io::Error) {
    tracing::error!(%error, "could not prepare {what}");
}

/// Write the commented settings template if there is no settings file, then open it.
pub fn open_settings(cx: &mut App) {
    let Some(paths) = paths(cx) else { return };
    match paths.ensure_settings_file() {
        Ok(_) => config::open_path(&paths.settings, cx),
        Err(error) => report("settings.json", error),
    }
}

/// Write the commented keymap template (the bundled defaults, commented out) if there
/// is no keymap file, then open it.
pub fn open_keymap(cx: &mut App) {
    let Some(paths) = paths(cx) else { return };
    match paths.ensure_keymap_file() {
        Ok(_) => config::open_path(&paths.keymap, cx),
        Err(error) => report("keymap.json", error),
    }
}

/// Create the themes folder if needed, then open it.
pub fn open_themes_folder(cx: &mut App) {
    let Some(paths) = paths(cx) else { return };
    match std::fs::create_dir_all(&paths.themes) {
        Ok(()) => config::open_path(&paths.themes, cx),
        Err(error) => report("the themes folder", error),
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::Keymap;

    use super::*;

    /// The chords tests press are the ones the bundled keymap binds.
    #[test]
    fn test_chords_match_the_bundled_keymap() {
        let keymap = Keymap::bundled_default();
        let bound = |context: Option<&str>, keystrokes: &str| {
            keymap
                .resolved()
                .into_iter()
                .find(|entry| entry.context.as_deref() == context && entry.keystrokes == keystrokes)
                .and_then(|entry| entry.action.as_ref())
                .map(|action| action.name.clone())
                .unwrap_or_else(|| panic!("{keystrokes} is not bound in {context:?}"))
        };
        let workspace = Some(context::WORKSPACE);
        let terminal = Some(context::TERMINAL);
        assert_eq!(bound(None, keys::QUIT), "serialist::Quit");
        assert_eq!(bound(workspace, keys::CLEAR), "terminal::Clear");
        assert_eq!(bound(workspace, keys::DISCONNECT), "serial::Disconnect");
        assert_eq!(bound(workspace, keys::PAUSE), "terminal::Pause");
        assert_eq!(bound(workspace, keys::EXPORT), "terminal::Export");
        assert_eq!(
            bound(workspace, keys::TOGGLE_RECORD),
            "terminal::ToggleRecord"
        );
        assert_eq!(
            bound(Some(context::COMPOSE_BAR), keys::CYCLE_LINE_ENDING),
            "compose::CycleLineEnding"
        );
        assert_eq!(bound(terminal, keys::COPY), "terminal::Copy");
        assert_eq!(bound(terminal, keys::SELECT_ALL), "terminal::SelectAll");
        assert_eq!(bound(terminal, keys::SEARCH), "terminal::Search");
        assert_eq!(
            bound(terminal, keys::SCROLL_TO_TOP),
            "terminal::ScrollToTop"
        );
        assert_eq!(
            bound(terminal, keys::SCROLL_TO_BOTTOM),
            "terminal::JumpToBottom"
        );
        assert_eq!(
            bound(terminal, keys::TOGGLE_FRAME_STATS),
            "terminal::ToggleFrameStats"
        );
        let inline = Some(context::TERMINAL_INLINE);
        assert_eq!(
            bound(workspace, keys::TOGGLE_INLINE),
            "terminal::ToggleInline"
        );
        assert_eq!(bound(inline, keys::PASTE), "terminal::Paste");
        assert_eq!(
            bound(Some(context::COMPOSE_BAR), keys::SAVE_AS_COMMAND),
            "compose::SaveAsCommand"
        );
    }
}
