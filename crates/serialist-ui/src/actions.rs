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
        /// Open the Settings screen in a tab, or go to the one open.
        OpenSettingsUi,
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
        /// Switch the session between monitor mode (a log of lines) and VT mode (a
        /// terminal screen the device draws on).
        ToggleEmulation,
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

/// The command palette's actions (see [`palette`](crate::palette)).
pub mod command_palette {
    use crate::prelude::*;

    actions!(
        command_palette,
        [
            /// Open the command palette: every action and saved command, filtered as you
            /// type.
            Toggle,
            /// Move the palette's selection down.
            SelectNext,
            /// Move the palette's selection up.
            SelectPrevious,
        ]
    );
}

/// The session tabs' actions: a tab per open port (see [`workspace`](crate::workspace)).
pub mod tabs {
    use crate::prelude::*;

    actions!(
        tabs,
        [
            /// Open a blank tab (or go to the one open) and focus the Devices panel to
            /// pick its port.
            NewTab,
            /// Close the active tab: disconnect its port and stop its script and its
            /// recording, asking first while either runs.
            CloseTab,
            /// Go to the tab on the right, or the first after the last.
            NextTab,
            /// Go to the tab on the left, or the last before the first.
            PreviousTab,
            ActivateTab1,
            ActivateTab2,
            ActivateTab3,
            ActivateTab4,
            ActivateTab5,
            ActivateTab6,
            ActivateTab7,
            ActivateTab8,
            ActivateTab9,
        ]
    );
}

/// The Script console's actions and [`scripts::Run`](Run).
pub mod scripts {
    use crate::prelude::*;

    actions!(
        scripts,
        [
            /// Run the Script console's input line as a script (`=expr` prints `expr`).
            RunInline,
            /// Stop the script running on the session; queued ones still run.
            Stop,
            /// Empty the Script console's output.
            ClearConsole,
            /// Open the scripts folder, first creating it with the example scripts if
            /// it holds none.
            OpenScriptsFolder,
        ]
    );

    /// Run a script on the session, named by its path under the scripts folder (or an
    /// absolute path). A keymap file binds one like this:
    /// `["scripts::Run", { "path": "version_probe.lua" }]`. A script started while
    /// another runs waits for it.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, JsonSchema, Action)]
    #[action(namespace = scripts)]
    #[serde(deny_unknown_fields)]
    #[schemars(crate = "crate::prelude::schemars")]
    pub struct Run {
        pub path: String,
    }
}

/// The plugins folder's actions and [`plugins::InstallExamplePlugin`](InstallExamplePlugin).
pub mod plugins {
    use crate::prelude::*;

    actions!(
        plugins,
        [
            /// Open the plugins folder, first writing copies of the example plugins into
            /// its examples folder, where they decode nothing until installed.
            OpenPluginsFolder,
        ]
    );

    /// Install a bundled example plugin: copy its folder into the plugins folder, which
    /// enables it. The command palette lists one per example not installed; a keymap
    /// file binds one like this:
    /// `["plugins::InstallExamplePlugin", { "name": "airoha-race" }]`.
    #[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, JsonSchema, Action)]
    #[action(namespace = plugins)]
    #[serde(deny_unknown_fields)]
    #[schemars(crate = "crate::prelude::schemars")]
    pub struct InstallExamplePlugin {
        /// The example's folder name, such as `airoha-race`.
        pub name: String,
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
    pub const SCRIPT_CONSOLE: &str = "ScriptConsole";
    pub const COMMAND_PALETTE: &str = "CommandPalette";
    /// The palette's query field, where Up and Down move the selection.
    pub const COMMAND_PALETTE_INPUT: &str = "CommandPalette > Input";
    /// The Settings screen (see [`settings_view`](crate::settings_view)).
    pub const SETTINGS_VIEW: &str = crate::settings_view::CONTEXT;
}

/// The default chords tests press. The bundled keymap in `serialist-core` is where they
/// are bound; `test_chords_match_the_bundled_keymap` keeps the two in step.
#[cfg(all(test, target_os = "macos"))]
pub(crate) mod keys {
    pub const QUIT: &str = "cmd-q";
    pub const CLEAR: &str = "cmd-k";
    pub const DISCONNECT: &str = "cmd-shift-w";
    pub const NEW_TAB: &str = "cmd-t";
    pub const CLOSE_TAB: &str = "cmd-w";
    pub const NEXT_TAB: &str = "cmd-shift-]";
    pub const PREVIOUS_TAB: &str = "cmd-shift-[";
    pub const TAB_1: &str = "cmd-1";
    pub const TAB_2: &str = "cmd-2";
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
    pub const COMMAND_PALETTE: &str = "cmd-shift-p";
    pub const OPEN_SETTINGS_UI: &str = "cmd-,";
}

// Plain ctrl chords belong to the device once inline mode sends keystrokes to the port
// (ctrl-s is XOFF), so the other platforms take shifted chords. Pause is the exception
// at plain ctrl-p; inline mode will need an escape for it.
#[cfg(all(test, not(target_os = "macos")))]
pub(crate) mod keys {
    pub const QUIT: &str = "ctrl-q";
    pub const CLEAR: &str = "ctrl-shift-k";
    pub const DISCONNECT: &str = "ctrl-alt-w";
    pub const NEW_TAB: &str = "ctrl-shift-t";
    pub const CLOSE_TAB: &str = "ctrl-shift-w";
    pub const NEXT_TAB: &str = "ctrl-shift-]";
    pub const PREVIOUS_TAB: &str = "ctrl-shift-[";
    pub const TAB_1: &str = "ctrl-1";
    pub const TAB_2: &str = "ctrl-2";
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
    pub const COMMAND_PALETTE: &str = "ctrl-shift-p";
    pub const OPEN_SETTINGS_UI: &str = "ctrl-,";
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
    cx.on_action(|_: &scripts::OpenScriptsFolder, cx| open_scripts_folder(cx));
    cx.on_action(|_: &plugins::OpenPluginsFolder, cx| crate::plugin_files::open_plugins_folder(cx));
    // The workspace handles this too, to say how it went in the status line; this is
    // for a dispatch that does not reach it.
    cx.on_action(|action: &plugins::InstallExamplePlugin, cx| {
        let notice = crate::plugin_files::install_example(&action.name, cx);
        if notice.is_error {
            tracing::warn!("{}", notice.text);
        }
    });
    set_menus(cx);
}

/// Most scripts the Scripts menu lists; the console lists them all.
const MENU_SCRIPTS: usize = 40;

/// The app menu, which is the palette: the Serialist menu, and a Scripts menu with a
/// Run entry per script in the scripts folder. The configuration calls it again when
/// the list of scripts changes.
pub fn set_menus(cx: &mut App) {
    let listed = cx
        .try_global::<Config>()
        .map(|config| config.scripts().clone())
        .unwrap_or_default();
    let mut script_items: Vec<MenuItem> = listed
        .iter()
        .take(MENU_SCRIPTS)
        .map(|entry| {
            MenuItem::action(
                format!("Run {}", entry.relative),
                scripts::Run {
                    path: entry.relative.clone(),
                },
            )
        })
        .collect();
    if !script_items.is_empty() {
        script_items.push(MenuItem::separator());
    }
    script_items.extend([
        MenuItem::action("Stop Script", scripts::Stop),
        MenuItem::action("Clear Script Console", scripts::ClearConsole),
        MenuItem::action("Open Scripts Folder", scripts::OpenScriptsFolder),
    ]);
    cx.set_menus([
        Menu::new("Serialist").items([
            MenuItem::action("Settings\u{2026}", OpenSettingsUi),
            MenuItem::action("Open settings.json", OpenSettings),
            MenuItem::action("Open Keymap", OpenKeymap),
            MenuItem::action("Open Themes Folder", OpenThemesFolder),
            MenuItem::action("Open Plugins Folder", plugins::OpenPluginsFolder),
            MenuItem::action("Reload Configuration", ReloadConfig),
            MenuItem::separator(),
            MenuItem::action("Quit Serialist", Quit),
        ]),
        Menu::new("Scripts").items(script_items),
    ]);
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

/// Create the scripts folder if needed, with the example scripts if it holds no script
/// yet, then open it.
pub fn open_scripts_folder(cx: &mut App) {
    let Some(paths) = paths(cx) else { return };
    match paths.ensure_example_scripts() {
        Ok(written) => {
            if !written.is_empty() {
                tracing::info!(count = written.len(), "wrote the example scripts");
                // The watcher would notice too; this makes the console show them now.
                config::reload(config::ConfigPiece::Scripts, cx);
            }
            config::open_path(&paths.scripts_dir(), cx);
        }
        Err(error) => report("the scripts folder", error),
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
        assert_eq!(bound(workspace, keys::NEW_TAB), "tabs::NewTab");
        assert_eq!(bound(workspace, keys::CLOSE_TAB), "tabs::CloseTab");
        assert_eq!(bound(workspace, keys::NEXT_TAB), "tabs::NextTab");
        assert_eq!(bound(workspace, keys::PREVIOUS_TAB), "tabs::PreviousTab");
        assert_eq!(bound(workspace, keys::TAB_1), "tabs::ActivateTab1");
        assert_eq!(bound(workspace, keys::TAB_2), "tabs::ActivateTab2");
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
        assert_eq!(
            bound(workspace, keys::COMMAND_PALETTE),
            "command_palette::Toggle"
        );
        assert_eq!(
            bound(workspace, keys::OPEN_SETTINGS_UI),
            "serialist::OpenSettingsUi"
        );
        let palette = Some(context::COMMAND_PALETTE_INPUT);
        assert_eq!(bound(palette, "down"), "command_palette::SelectNext");
        assert_eq!(bound(palette, "up"), "command_palette::SelectPrevious");
    }
}
