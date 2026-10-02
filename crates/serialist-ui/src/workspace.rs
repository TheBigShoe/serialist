//! The window's root view: the Devices and Commands panels in the left dock, a tab per
//! open port in the center, the Decoded panel and the Script console in the right dock,
//! the status line along the bottom.
//!
//! # Docks
//!
//! Each dock stands next to a rail of icons, one per panel, that opens and closes it;
//! narrow windows collapse the docks to their rails (see [`docks`](crate::docks)). The
//! Decoded panel opens when the active session decodes and closes when it stops; the
//! Script console opens when a script on the active tab prints. The widths are saved in
//! `state.json` with the tabs.
//!
//! # The command palette
//!
//! `command_palette::Toggle` opens [`CommandPalette`] in a dialog; what it confirms runs
//! here (see [`palette`](crate::palette)).
//!
//! # Tabs
//!
//! The workspace owns the tabs, and each tab owns at most one session view: its
//! session, ingest thread and store, codec slot, script host, pause, recording, port
//! settings and export state all live in the view (see
//! [`session_view`](crate::session_view)), so a tab is a port and a view, nothing more.
//! Connecting again after a disconnect (the toolbar's Connect, or Connect in the Devices
//! panel) hands the new session to the same view, which carries on in the same store
//! with its scrollback and settings ([`SessionView::reconnect`]); the tab keeps its
//! place, its [`TabId`] and its Script console output. A tab may hold no view: a new tab
//! (`tabs::NewTab`) before a port is picked, a port being opened for the first time, or
//! a restored port whose device is not plugged in.
//!
//! "Connect" in the Devices panel goes to the tab already holding that port if there is
//! one (and opens it again if it was disconnected), else fills the active tab if it is
//! a new one, else opens a new tab. A new tab whose port does not open goes away again
//! and the Devices panel says why.
//!
//! One tab is active. Its view is the one on screen and the one the rest of the window
//! is about: the status line, the compose bar (the view's own), the window title
//! (`<port> — Serialist`), the Decoded panel and the Script console follow it, and the
//! workspace-level actions (clear, pause, export, record, disconnect, the mode toggle),
//! the Commands panel's sends and the console's Run buttons and REPL go to it. The
//! other views are hidden ([`SessionView::set_visible`]): they keep ingesting, decoding,
//! recording and running scripts, but take no snapshots and draw nothing, and the
//! workspace repaints for one of them only when what its tab label shows changes (the
//! dot, and the bytes that arrived since it was left), at most every housekeeping tick.
//!
//! What was started in a tab stays with that tab's session, whichever tab is active
//! later: a saved command's `expect` is watched on the session it was sent on, and a
//! script's `serial.current()` is the session of the tab it was started in (its output
//! goes to that tab's Script console output, and its `commands.send` sends there too).
//!
//! The tab bar shows once there are two tabs or more, or while the Settings tab is open;
//! with one session tab, the status line and the window title already name the port.
//!
//! The Settings screen ([`SettingsView`], `serialist::OpenSettingsUi`) opens in a tab of
//! its own beside the sessions, one at a time; it closes like any tab. Tabs switch with `tabs::ActivateTab1` to `9`
//! and `tabs::NextTab`/`PreviousTab`, reorder by dragging, and close with
//! `tabs::CloseTab` or their × button, which disconnects the port and stops its script
//! and recording, asking first while either runs.
//!
//! # Session restore
//!
//! When the workspace closes (the window closes, or the app quits) it writes the tabs'
//! ports and settings (line settings, codec, input mode, monitor or VT) to `state.json`
//! in the config directory (see
//! [`session_state`](crate::session_state)), and the next start reopens them, unless
//! ports were named on the command line or the `restore_session` setting is off. A
//! restored port connects if the port source lists it (a simulated port with the
//! simulator on, a real device that is plugged in) and otherwise waits in its tab.
//!
//! # Commands and scripts
//!
//! Saved commands reach the session through here: the Commands panel's
//! [`CommandsPanelEvent::Send`] and every [`commands::Send`](crate::actions::commands::Send)
//! keybinding call [`Workspace::send_command`], which asks for the command's parameters
//! in a dialog when it has any, then hands it to the active tab's session view. The
//! workspace also owns the persisted compose history, which every session's compose bar
//! shares.
//!
//! Scripts start here too, whatever starts them: the Script console's Run buttons and
//! REPL, a [`scripts::Run`](crate::actions::scripts::Run) key binding or menu entry, a
//! saved command whose payload is `{ "script": … }`, and the `on_connect` script of the
//! device profile that matches a port, run once the session is open (and the opener
//! has set DTR and RTS) on that port's tab. Each reads its file from the scripts folder
//! and queues it on the session view, which owns the session's script thread; the
//! console shows what the runs report.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serialist_core::settings::ConfigPaths;
use serialist_core::{
    CommandRef, ParamValues, Payload, PortId, PortInfo, PortKind, PortSource,
    ReplayTransportFactory, SerialConfig, StoreConfig, TransportError, TransportFactory,
};
use serialist_script::ScriptSource;

use crate::actions::command_palette::Toggle as ToggleCommandPalette;
use crate::actions::plugins::InstallExamplePlugin;
use crate::actions::scripts::{ClearConsole, Run as RunScript, RunInline, Stop as StopScript};
use crate::actions::tabs::{
    ActivateTab1, ActivateTab2, ActivateTab3, ActivateTab4, ActivateTab5, ActivateTab6,
    ActivateTab7, ActivateTab8, ActivateTab9, CloseTab, NewTab, NextTab, PreviousTab,
};
use crate::actions::{
    self, Clear, Disconnect, Export, OpenSettingsUi, Pause, ToggleEmulation, ToggleInline,
    ToggleRecord, context,
};
use crate::chrome;
use crate::codecs::codec_not_installed;
use crate::commands_panel::{CommandsPanel, CommandsPanelEvent};
use crate::config::{self, Config};
use crate::decoded_panel::DecodedPanel;
use crate::devices_panel::{DevicesPanel, DevicesPanelEvent};
use crate::dialog_footer::DialogButtons;
use crate::docks::{CENTER_MIN, DockPanel, DockSide, Docks, RAIL_WIDTH};
use crate::emulation::Emulation;
use crate::export::ExportFormat;
use crate::history::PersistentHistory;
use crate::inline::Mode;
use crate::palette::{self, CommandPalette, PaletteEvent, PaletteTarget};
use crate::param_prompt::{ParamPrompt, ParamPromptEvent};
use crate::plugin_files;
use crate::port_settings::PortSettings;
use crate::prelude::*;
use crate::script_bridge::{CommandsSnapshot, ConsoleKind, ConsoleLine, ScriptEnv, inline_source};
use crate::script_console::{ScriptConsole, ScriptConsoleEvent};
use crate::script_files::{display_name, resolve_script};
use crate::session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
use crate::session_options::SessionOptions;
use crate::session_state::{STATE_VERSION, SavedTab, SessionState, state_path};
use crate::session_view::{SessionView, SessionViewEvent};
use crate::settings_view::SettingsView;
use crate::status::{ConnectionState, Notice, NoticeAction, StatusLine};
use crate::tabs::{TabId, TabLabel, TabState, TabStatus};

/// The widest a tab grows; longer names are cut with an ellipsis.
const TAB_MAX_WIDTH: Pixels = px(240.);

/// What the window is called with no port on screen.
pub const APP_TITLE: &str = "Serialist";

/// What the binary hands the UI: where ports come from and how to open them.
pub struct AppOptions {
    pub port_source: Arc<dyn PortSource>,
    pub transport_factory: Arc<dyn TransportFactory>,
    /// The `--baud` flag: every connect uses it, over device profiles and the
    /// `default_baud` setting.
    pub baud: Option<u32>,
    /// Port to select in the Devices panel at startup, even before the source lists it.
    pub select_port: Option<PortId>,
    /// Ports to open at startup, a tab each, the first one active (the `--port` and
    /// `--virtual NAME` flags). With none, the tabs saved at the last quit reopen.
    pub open_ports: Vec<PortId>,
    /// Sizes each session's store in place of the `scrollback_budget_bytes` setting.
    pub store: Option<StoreConfig>,
    /// The factory `replay:` ids open through, so the settings can set its defaults;
    /// `None` in tests that do not replay.
    pub replay: Option<Arc<ReplayTransportFactory>>,
}

/// Global setup: gpui-kit's components, the bundled configuration (theme, fonts and
/// key bindings; [`config::start`] replaces it with the user's), and the app-level
/// actions and menu. Call once, before opening a window.
pub fn init(cx: &mut App) {
    kit_init(cx);
    actions::init(cx);
    config::install(Config::bundled(ConfigPaths::default_for_platform()), cx);
}

/// Open the main window titled "Serialist" with a [`Workspace`] as its root view.
pub fn open_main_window(options: AppOptions, cx: &mut App) -> Result<Entity<Workspace>> {
    let bounds = Bounds::centered(None, size(px(1320.), px(780.)), cx);
    let window_options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some(APP_TITLE.into()),
            ..Default::default()
        }),
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(640.), px(400.))),
        app_id: Some("dev.serialist.Serialist".into()),
        ..Default::default()
    };
    let (_, workspace) = kit_open_window(window_options, cx, |window, cx| {
        cx.new(|cx| Workspace::new(options, window, cx))
    })?;
    // macOS keeps an app alive with no windows; a terminal with its only window closed
    // has nothing left to do.
    cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
            cx.quit();
        }
    })
    .detach();
    cx.activate(true);
    Ok(workspace)
}

/// Frames a reconnect waits for the last session's ingest thread to end by itself
/// before stopping it.
const REATTACH_PATIENCE: u32 = 10;

/// A session opened for a tab that already has a view, on its way into it.
struct Reopen {
    port: PortId,
    serial: SerialConfig,
    session: Box<dyn SessionHandle>,
    /// Frames waited so far for the view to be ready.
    waited: u32,
}

/// What a restored tab applies to its session once the port opens.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Restore {
    codec: Option<String>,
    mode: Mode,
    /// Monitor or VT as the tab showed it, if the file said (one from before it was
    /// kept leaves the session with the setting's).
    emulation: Option<Emulation>,
}

/// A tab: a port, and the session view open on it, if any.
struct SessionTab {
    id: TabId,
    /// `None` until a port is picked.
    port: Option<PortId>,
    /// The port as it was known when opened, for its device profile when the settings
    /// change.
    info: Option<PortInfo>,
    /// The line settings it was opened with, or is to open with.
    serial: Option<SerialConfig>,
    view: Option<Entity<SessionView>>,
    /// The port is being opened on the background executor.
    connecting: bool,
    /// A restored tab's codec and mode, applied when its port opens.
    restore: Option<Restore>,
    /// Port settings a Devices row set for this port: its line ending, echo and control
    /// levels, applied when the port opens.
    port_settings: Option<PortSettings>,
    /// Why the last open failed.
    error: Option<String>,
    /// Made by the connect in flight, which removes it again if the port does not
    /// open, going back to this tab.
    provisional: Option<Option<TabId>>,
    /// The status last repainted for while in the background.
    shown: Option<TabStatus>,
    /// The Settings screen, in the one tab that shows it instead of a session.
    settings: Option<Entity<SettingsView>>,
    _connect_task: Option<Task<()>>,
    _observer: Option<Subscription>,
    _events: Option<Subscription>,
}

impl SessionTab {
    fn new(id: TabId) -> Self {
        Self {
            id,
            port: None,
            info: None,
            serial: None,
            view: None,
            connecting: false,
            restore: None,
            port_settings: None,
            error: None,
            provisional: None,
            shown: None,
            settings: None,
            _connect_task: None,
            _observer: None,
            _events: None,
        }
    }

    /// A new tab with no port picked.
    fn is_blank(&self) -> bool {
        self.port.is_none() && self.view.is_none() && !self.connecting && self.settings.is_none()
    }

    /// Opening, or open.
    fn is_live(&self, cx: &App) -> bool {
        self.connecting
            || self
                .view
                .as_ref()
                .is_some_and(|view| !view.read(cx).state().is_disconnected())
    }

    fn status(&self, cx: &App) -> TabStatus {
        let state = if self.connecting {
            TabState::Connecting
        } else if let Some(view) = &self.view {
            return view.read(cx).tab_status();
        } else if self.port.is_some() {
            TabState::NotConnected
        } else {
            TabState::Empty
        };
        TabStatus {
            state,
            unseen_bytes: 0,
        }
    }
}

/// A tab being dragged to a new place, and what the drag shows.
#[derive(Clone)]
struct DraggedTab {
    id: TabId,
    title: SharedString,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .child(self.title.clone())
    }
}

pub struct Workspace {
    devices: Entity<DevicesPanel>,
    commands: Entity<CommandsPanel>,
    /// The Decoded panel, at the top of the right dock.
    decoded: Entity<DecodedPanel>,
    /// The Script console in the right dock.
    console: Entity<ScriptConsole>,
    /// The saved commands scripts send by name, kept current on every reload.
    script_commands: CommandsSnapshot,
    /// The compose history every session shares, kept in `history.jsonl`.
    history: Entity<PersistentHistory>,
    /// The parameter dialog, while it is open.
    param_prompt: Option<Entity<ParamPrompt>>,
    /// The session the parameter dialog sends to: the active one when it opened.
    param_target: Option<WeakEntity<SessionView>>,
    /// In the order the tab bar shows them.
    tabs: Vec<SessionTab>,
    active: Option<TabId>,
    /// The tab with a session that was in front last before the active one, which the
    /// status line keeps showing while the Settings tab is in front.
    last_session: Option<TabId>,
    next_tab: u64,
    /// The tab a close confirmation is open for.
    pending_close: Option<TabId>,
    /// The panels open and the dock widths.
    docks: Docks,
    /// Whether the active session decoded when last looked at, for the Decoded panel.
    decoding_seen: Option<bool>,
    /// The command palette, while it is open.
    palette: Option<Entity<CommandPalette>>,
    /// The keystrokes that open the palette, for the empty center to mention.
    palette_binding: Option<String>,
    /// The window title last set.
    window_title: String,
    opener: Arc<dyn SessionOpener>,
    port_source: Arc<dyn PortSource>,
    /// The `--baud` flag.
    baud: Option<u32>,
    /// Store sizing over the settings' budget.
    store: Option<StoreConfig>,
    /// The replay factory whose defaults follow the `replay` setting.
    replay: Option<Arc<ReplayTransportFactory>>,
    focus_handle: FocusHandle,
    _param_prompt_events: Option<Subscription>,
    _palette_events: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    /// The workspace for `options`: the ports it names open in tabs, or, with none,
    /// the tabs saved at the last quit reopen (see "Session restore").
    pub fn new(options: AppOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let opener = Arc::new(CoreSessionOpener::new(options.transport_factory));
        let mut workspace =
            Self::with_opener(options.port_source, opener, options.baud, window, cx);
        workspace.store = options.store;
        // Before any tab opens: a `--port replay:...` or a restored replay tab opens with
        // the settings' speed and end.
        workspace.replay = options.replay;
        workspace.apply_replay_defaults(cx);
        let select = options
            .select_port
            .or_else(|| options.open_ports.first().cloned());
        if let Some(port) = select {
            workspace.devices.update(cx, |devices, cx| {
                devices.select_port(port, window, cx);
            });
        }
        let saved = workspace.saved_state(cx);
        if let Some(docks) = saved.as_ref().and_then(|state| state.docks) {
            workspace.docks = Docks::restored(&docks);
        }
        if !options.open_ports.is_empty() {
            workspace.open_ports(options.open_ports, window, cx);
        } else if let Some(state) = saved {
            workspace.restore(state, window, cx);
        }
        workspace
    }

    /// A workspace with a custom way of opening sessions; tests pass fakes here.
    /// `baud`, when set, is used for every connect, as the `--baud` flag is.
    pub fn with_opener(
        port_source: Arc<dyn PortSource>,
        opener: Arc<dyn SessionOpener>,
        baud: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let devices = cx.new(|cx| DevicesPanel::new(port_source.clone(), baud, window, cx));
        let devices_events =
            cx.subscribe_in(&devices, window, |this, _, event, window, cx| match event {
                DevicesPanelEvent::Connect {
                    port,
                    serial,
                    settings,
                } => {
                    this.connect_with(port.clone(), serial.clone(), settings.clone(), window, cx);
                }
            });
        let decoded = cx.new(|cx| DecodedPanel::new(window, cx));
        let console = cx.new(|cx| ScriptConsole::new(window, cx));
        let console_events =
            cx.subscribe_in(&console, window, |this, _, event, window, cx| match event {
                ScriptConsoleEvent::Run(path) => {
                    this.run_script_path(Path::new(path), "console", window, cx);
                }
                ScriptConsoleEvent::RunInline(code) => this.run_inline(code, window, cx),
                ScriptConsoleEvent::Stop => this.stop_script(cx),
            });
        let script_commands = CommandsSnapshot::new(
            cx.try_global::<Config>()
                .map(|config| config.commands().clone())
                .unwrap_or_default(),
        );
        let commands = cx.new(|cx| CommandsPanel::new(window, cx));
        let commands_events =
            cx.subscribe_in(
                &commands,
                window,
                |this, _, event, window, cx| match event {
                    CommandsPanelEvent::Send(reference) => {
                        this.send_command(reference.clone(), window, cx);
                    }
                },
            );
        // The history file is only read and written for a configuration loaded from its
        // directory; the bundled defaults keep it in memory.
        let history_path = cx
            .try_global::<Config>()
            .filter(|config| config.is_loaded())
            .map(|config| config.paths().history_path());
        let history = cx.new(|_| PersistentHistory::load(history_path));
        // The window closing, or the app quitting (which closes it): write the history
        // and the tabs.
        let released = cx.on_release(|this, cx| {
            this.history.update(cx, |history, cx| history.flush(cx));
            this.save_session_state(cx);
        });
        // Quitting with the window open: the tabs as they are now (the release that
        // follows writes the same again).
        let quitting = cx.on_app_quit(|this, cx| {
            this.save_session_state(cx);
            async {}
        });
        // A reload reaches the sessions' display defaults and the status line.
        let config_changes = cx.observe_global_in::<Config>(window, |this, _, cx| {
            this.config_changed(cx);
        });
        // `"mode": "system"` themes follow the window's appearance.
        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            config::set_appearance(window.appearance(), cx);
        });
        config::set_appearance(window.appearance(), cx);
        // Start with the port list focused so arrows and Enter work immediately.
        let devices_focus = devices.focus_handle(cx);
        window.focus(&devices_focus, cx);

        Self {
            devices,
            commands,
            decoded,
            console,
            script_commands,
            history,
            param_prompt: None,
            param_target: None,
            tabs: Vec::new(),
            active: None,
            last_session: None,
            next_tab: 0,
            pending_close: None,
            docks: Docks::default(),
            decoding_seen: None,
            palette: None,
            palette_binding: None,
            window_title: APP_TITLE.to_owned(),
            opener,
            port_source,
            baud,
            store: None,
            replay: None,
            focus_handle: cx.focus_handle(),
            _param_prompt_events: None,
            _palette_events: None,
            _subscriptions: vec![
                devices_events,
                commands_events,
                console_events,
                released,
                quitting,
                config_changes,
                appearance,
            ],
        }
    }

    /// What the app knows about `port`: its entry in the Devices panel, else the port
    /// source's current list, else just its id (enough for a path-matched profile).
    pub fn port_info(&self, port: &PortId, cx: &App) -> PortInfo {
        self.devices
            .read(cx)
            .list()
            .get(port)
            .map(|entry| entry.info.clone())
            .or_else(|| {
                self.port_source
                    .snapshot()
                    .into_iter()
                    .find(|info| &info.id == port)
            })
            .unwrap_or_else(|| PortInfo {
                id: port.clone(),
                kind: PortKind::Unknown,
                display_name: port.to_string(),
            })
    }

    /// The line settings to open `port` with: its device profile over `default_baud`,
    /// with `--baud` over both.
    pub fn serial_for(&self, port: &PortId, cx: &App) -> SerialConfig {
        let info = self.port_info(port, cx);
        let mut serial = cx.global::<Config>().serial_config_for(&info);
        if let Some(baud) = self.baud {
            serial.baud = baud;
        }
        serial
    }

    /// What a session on `port` starts with, from the settings and its profile.
    fn session_options_for(&self, port: &PortInfo, cx: &App) -> SessionOptions {
        let options = SessionOptions::from_settings(cx.global::<Config>().settings(), port);
        match &self.store {
            Some(store) => options.with_store(store.clone()),
            None => options,
        }
    }

    /// Hand the replay factory the `replay` setting as its defaults. A replay already open
    /// keeps the options it opened with; the next open sees the change.
    fn apply_replay_defaults(&self, cx: &App) {
        if let (Some(replay), Some(config)) = (&self.replay, cx.try_global::<Config>()) {
            replay.set_defaults(config.settings().replay.options());
        }
    }

    /// The configuration changed: hand every session its new defaults and repaint the
    /// status line.
    fn config_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(config) = cx.try_global::<Config>() {
            self.script_commands.set(config.commands().clone());
        }
        self.apply_replay_defaults(cx);
        let sessions: Vec<(Entity<SessionView>, PortInfo)> = self
            .tabs
            .iter()
            .filter_map(|tab| Some((tab.view.clone()?, tab.info.clone()?)))
            .collect();
        for (view, info) in sessions {
            let options = self.session_options_for(&info, cx);
            view.update(cx, |view, cx| view.apply_options(options, cx));
        }
        cx.notify();
    }

    pub fn devices(&self) -> &Entity<DevicesPanel> {
        &self.devices
    }

    /// The active tab's session view.
    pub fn session(&self) -> Option<&Entity<SessionView>> {
        self.active_tab().and_then(|tab| tab.view.as_ref())
    }

    /// The session the status line is about: the active tab's, or, with the Settings
    /// tab in front, the one that was in front before it (else any open one, if that tab
    /// is gone). A tab that has no session shows no segment of its own.
    pub fn status_session(&self) -> Option<&Entity<SessionView>> {
        if let Some(session) = self.session() {
            return Some(session);
        }
        if !self.active_tab().is_some_and(|tab| tab.settings.is_some()) {
            return None;
        }
        self.last_session
            .and_then(|id| self.session_in(id))
            .or_else(|| self.tabs.iter().find_map(|tab| tab.view.as_ref()))
    }

    /// What the status line says when there is no session to show: a port being opened
    /// or not connected, the Settings screen and where its files are, or that there is
    /// no session.
    pub fn status_placeholder(&self, cx: &App) -> String {
        match self.active_tab() {
            Some(tab) if tab.settings.is_some() => {
                let dir = cx
                    .try_global::<Config>()
                    .map(|config| config.paths().dir.clone())
                    .unwrap_or_else(|| ConfigPaths::default_for_platform().dir);
                format!("Settings: {}", dir.display())
            }
            Some(tab) if tab.connecting => format!(
                "Opening {}\u{2026}",
                tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
            ),
            Some(tab) if tab.port.is_some() => format!(
                "{}: not connected",
                tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
            ),
            _ => "No session".to_owned(),
        }
    }

    /// Every tab's session view, in tab order.
    pub fn sessions(&self) -> Vec<Entity<SessionView>> {
        self.tabs
            .iter()
            .filter_map(|tab| tab.view.clone())
            .collect()
    }

    /// The session view of the tab holding `port`.
    pub fn session_for_port(&self, port: &PortId) -> Option<&Entity<SessionView>> {
        self.tabs
            .iter()
            .find(|tab| tab.port.as_ref() == Some(port))
            .and_then(|tab| tab.view.as_ref())
    }

    /// The session view in tab `id`.
    pub fn session_in(&self, id: TabId) -> Option<&Entity<SessionView>> {
        self.tab(id).and_then(|tab| tab.view.as_ref())
    }

    pub fn commands(&self) -> &Entity<CommandsPanel> {
        &self.commands
    }

    pub fn console(&self) -> &Entity<ScriptConsole> {
        &self.console
    }

    pub fn decoded(&self) -> &Entity<DecodedPanel> {
        &self.decoded
    }

    pub fn history(&self) -> &Entity<PersistentHistory> {
        &self.history
    }

    /// The parameter dialog's form, while it is open.
    pub fn param_prompt(&self) -> Option<&Entity<ParamPrompt>> {
        self.param_prompt.as_ref()
    }

    /// The port the active tab is opening.
    pub fn connecting(&self) -> Option<&PortId> {
        self.active_tab()
            .filter(|tab| tab.connecting)
            .and_then(|tab| tab.port.as_ref())
    }

    // --- Tabs --------------------------------------------------------------------------

    fn tab_position(&self, id: TabId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn tab(&self, id: TabId) -> Option<&SessionTab> {
        self.tabs.iter().find(|tab| tab.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut SessionTab> {
        self.tabs.iter_mut().find(|tab| tab.id == id)
    }

    fn active_tab(&self) -> Option<&SessionTab> {
        self.active.and_then(|id| self.tab(id))
    }

    fn tab_for_port(&self, port: &PortId) -> Option<TabId> {
        self.tabs
            .iter()
            .find(|tab| tab.port.as_ref() == Some(port))
            .map(|tab| tab.id)
    }

    /// The tabs, in order.
    pub fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.iter().map(|tab| tab.id).collect()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_tab_id(&self) -> Option<TabId> {
        self.active
    }

    /// Where the active tab is in the tab bar.
    pub fn active_index(&self) -> Option<usize> {
        self.active.and_then(|id| self.tab_position(id))
    }

    /// The port held by tab `id`.
    pub fn tab_port(&self, id: TabId) -> Option<&PortId> {
        self.tab(id).and_then(|tab| tab.port.as_ref())
    }

    /// Whether the tab bar shows: with two tabs or more, or the Settings tab.
    pub fn shows_tab_bar(&self) -> bool {
        self.tabs.len() > 1 || self.settings_view().is_some()
    }

    /// The Settings screen, while its tab is open.
    pub fn settings_view(&self) -> Option<&Entity<SettingsView>> {
        self.tabs.iter().find_map(|tab| tab.settings.as_ref())
    }

    /// Open the Settings screen in a tab after the others, or go to the one open.
    pub fn open_settings_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        if let Some(id) = self
            .tabs
            .iter()
            .find(|tab| tab.settings.is_some())
            .map(|tab| tab.id)
        {
            self.activate(id, window, cx);
            return id;
        }
        let source = self.port_source.clone();
        let view = cx.new(|cx| SettingsView::new(Some(source), window, cx));
        let id = self.push_tab();
        if let Some(tab) = self.tab_mut(id) {
            tab.settings = Some(view);
        }
        tracing::info!(tab = %id, "opened the settings screen");
        self.activate(id, window, cx);
        id
    }

    fn open_settings_ui_action(
        &mut self,
        _: &OpenSettingsUi,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings_ui(window, cx);
    }

    /// What a tab is called: its device's name in the Devices panel, else its port id.
    fn tab_title(&self, tab: &SessionTab, cx: &App) -> String {
        if tab.settings.is_some() {
            return "Settings".to_owned();
        }
        let Some(port) = &tab.port else {
            return "New tab".to_owned();
        };
        let info = tab.info.clone().unwrap_or_else(|| self.port_info(port, cx));
        self.devices.read(cx).display_name(&info, cx)
    }

    /// Every tab's label, in order, as the tab bar draws them.
    pub fn tab_labels(&self, cx: &App) -> Vec<TabLabel> {
        self.tabs
            .iter()
            .map(|tab| {
                TabLabel::new(
                    tab.id,
                    self.tab_title(tab, cx),
                    tab.status(cx),
                    self.active == Some(tab.id),
                )
                .with_suffix(
                    tab.view
                        .as_ref()
                        .and_then(|view| view.read(cx).screen_title().map(str::to_owned)),
                )
            })
            .collect()
    }

    /// `<port> — Serialist` for the active tab's port, else `Serialist`.
    pub fn window_title(&self) -> String {
        if self.active_tab().is_some_and(|tab| tab.settings.is_some()) {
            return format!("Settings \u{2014} {APP_TITLE}");
        }
        match self.active_tab().and_then(|tab| tab.port.as_ref()) {
            Some(port) => format!("{port} \u{2014} {APP_TITLE}"),
            None => APP_TITLE.to_owned(),
        }
    }

    /// Add a tab with no port at the end of the bar.
    fn push_tab(&mut self) -> TabId {
        self.next_tab += 1;
        let id = TabId(self.next_tab);
        self.tabs.push(SessionTab::new(id));
        id
    }

    /// Make tab `id` the active one: hide the view on screen, show this one (with one
    /// snapshot of what arrived while it was hidden), point the Decoded panel and the
    /// Script console at it, and focus it (or the Devices panel, for a tab with no
    /// session).
    pub fn activate(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab(id).is_none() {
            return;
        }
        if self.active == Some(id) {
            self.focus_active(window, cx);
            return;
        }
        if let Some(previous) = self.session().cloned() {
            previous.update(cx, |view, cx| view.set_visible(false, cx));
            self.last_session = self.active;
        }
        if let Some(previous) = self.active.and_then(|previous| self.tab_mut(previous)) {
            previous.shown = None;
        }
        self.active = Some(id);
        self.show_active(window, cx);
    }

    /// Make the tab at `index` in the bar the active one.
    pub fn activate_index(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.tabs.get(index).map(|tab| tab.id) {
            self.activate(id, window, cx);
        }
    }

    /// The next tab (`forward`) or the previous one, around the ends.
    pub fn activate_next(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.tabs.len();
        let Some(current) = self.active_index() else {
            self.activate_index(0, window, cx);
            return;
        };
        if count < 2 {
            return;
        }
        let next = if forward {
            (current + 1) % count
        } else {
            (current + count - 1) % count
        };
        self.activate_index(next, window, cx);
    }

    /// Put the active tab on screen and everything that follows it.
    fn show_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.session().cloned();
        if let Some(view) = &view {
            view.update(cx, |view, cx| view.set_visible(true, cx));
        }
        self.decoded.update(cx, |decoded, cx| {
            decoded.set_session(view.clone(), window, cx);
        });
        let weak = view.as_ref().map(Entity::downgrade);
        let source = self.active;
        self.console.update(cx, |console, cx| {
            console.set_session(weak, cx);
            console.show_source(source, cx);
        });
        self.focus_active(window, cx);
        self.sync_decoded_panel(cx);
        tracing::debug!(tab = ?self.active, "tab shown");
        cx.notify();
    }

    /// Focus where typing goes in the active tab: its session's compose bar (or its
    /// terminal in inline mode), or the Devices panel for a tab with no session.
    fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(settings) = self.active_tab().and_then(|tab| tab.settings.clone()) {
            let focus = settings.focus_handle(cx);
            window.focus(&focus, cx);
            return;
        }
        match self.session().cloned() {
            Some(view) => view.update(cx, |view, cx| view.focus_default(window, cx)),
            None => {
                let focus = self.devices.focus_handle(cx);
                window.focus(&focus, cx);
            }
        }
    }

    /// Open a new tab and focus the Devices panel to pick its port. A new tab already
    /// open is reused.
    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) -> TabId {
        let id = match self.tabs.iter().find(|tab| tab.is_blank()) {
            Some(tab) => tab.id,
            None => self.push_tab(),
        };
        tracing::info!(tab = %id, tabs = self.tabs.len(), "new tab");
        self.activate(id, window, cx);
        cx.notify();
        id
    }

    /// Close tab `id`, asking first while its session records or runs a script.
    pub fn request_close(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        let busy = tab.view.as_ref().map(|view| {
            let view = view.read(cx);
            (
                view.recording().is_some(),
                view.script_status().map(|status| status.name),
            )
        });
        match busy {
            Some((recording, script)) if recording || script.is_some() => {
                self.confirm_close_dialog(id, recording, script, window, cx);
            }
            _ => self.close_tab(id, window, cx),
        }
    }

    /// The tab whose close confirmation is open.
    pub fn pending_close(&self) -> Option<TabId> {
        self.pending_close
    }

    /// Close the tab whose close confirmation is open, as its Close button does. The
    /// dialog closes first, so the focus it gives back is then moved on to the tab
    /// that takes the closed one's place.
    pub fn confirm_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.pending_close.take() {
            window.close_dialog(cx);
            self.close_tab(id, window, cx);
        }
    }

    fn confirm_close_dialog(
        &mut self,
        id: TabId,
        recording: bool,
        script: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port = self
            .tab_port(id)
            .map_or_else(|| "the tab".to_owned(), ToString::to_string);
        let running = match (recording, script) {
            (true, Some(script)) => format!("{port} is recording, and {script} is running."),
            (true, None) => format!("{port} is recording."),
            (false, Some(script)) => format!("{script} is running on {port}."),
            (false, None) => String::new(),
        };
        let message = SharedString::from(format!(
            "{running} Closing the tab disconnects the port, stops the script and finishes \
             the recording."
        ));
        let title = SharedString::from(format!("Close {port}?"));
        self.pending_close = Some(id);
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = this.clone();
            let closed = this.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(div().text_sm().child(message.clone()))
                .footer(DialogButtons::new("Close Tab"))
                // Closing the dialog is part of confirming.
                .on_ok(move |_, window, cx| {
                    confirm
                        .update(cx, |workspace, cx| workspace.confirm_close(window, cx))
                        .ok();
                    false
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |workspace, _| workspace.pending_close = None)
                        .ok();
                })
        });
    }

    /// Close tab `id` now: disconnect its port, which stops its script and finishes its
    /// recording, and drop its Script console output. The tab that takes its place (or
    /// the one before it) becomes active.
    pub fn close_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        self.remove_tab(id, None, window, cx);
    }

    fn remove_tab(
        &mut self,
        id: TabId,
        prefer: Option<TabId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_position(id) else {
            return;
        };
        let tab = self.tabs.remove(ix);
        if let Some(view) = &tab.view {
            view.update(cx, |view, cx| view.disconnect(cx));
        }
        self.console
            .update(cx, |console, cx| console.forget_source(Some(id), cx));
        if self.pending_close == Some(id) {
            self.pending_close = None;
        }
        tracing::info!(tab = %id, port = ?tab.port, tabs = self.tabs.len(), "tab closed");
        if self.active == Some(id) {
            self.active = None;
            let next = prefer
                .filter(|prefer| self.tab(*prefer).is_some())
                .or_else(|| {
                    self.tabs
                        .get(ix)
                        .or_else(|| ix.checked_sub(1).and_then(|before| self.tabs.get(before)))
                        .map(|tab| tab.id)
                });
            match next {
                Some(next) => self.activate(next, window, cx),
                None => self.show_active(window, cx),
            }
        }
        self.sync_connected_ports(cx);
        cx.notify();
    }

    /// Move the tab at `from` to `to` in the bar.
    pub fn move_tab(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        if from >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        let to = to.min(self.tabs.len());
        self.tabs.insert(to, tab);
        cx.notify();
    }

    /// A tab dropped on the tab at `to` takes its place.
    fn drop_tab(&mut self, id: TabId, to: usize, cx: &mut Context<Self>) {
        if let Some(from) = self.tab_position(id) {
            self.move_tab(from, to, cx);
        }
    }

    /// A session view notified: repaint if it is on screen, or if what its tab label
    /// shows changed. Keep the Devices panel's dots in step either way.
    fn session_notified(&mut self, id: TabId, view: &Entity<SessionView>, cx: &mut Context<Self>) {
        self.sync_connected_ports(cx);
        if self.active == Some(id) {
            self.sync_decoded_panel(cx);
            cx.notify();
            return;
        }
        let status = view.read(cx).tab_status();
        if let Some(tab) = self.tab_mut(id)
            && tab.shown != Some(status)
        {
            tab.shown = Some(status);
            cx.notify();
        }
    }

    /// Mark the ports open in a tab in the Devices panel.
    fn sync_connected_ports(&mut self, cx: &mut Context<Self>) {
        let open: Vec<PortId> = self
            .tabs
            .iter()
            .filter_map(|tab| tab.view.as_ref()?.read(cx).open_port().cloned())
            .collect();
        self.devices
            .update(cx, |devices, cx| devices.set_connected(open, cx));
    }

    // --- Connecting ----------------------------------------------------------------------

    /// Open `port`: in the tab holding it if there is one (just going there if it is
    /// open or opening), else in the active tab if it is a new one, else in a new tab.
    /// The port opens on the background executor.
    pub fn connect(
        &mut self,
        port: PortId,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connect_with(port, serial, None, window, cx);
    }

    /// [`Self::connect`], with the port settings a Devices row set for the port (its
    /// line ending, echo and control levels) for the session to take once open.
    pub fn connect_with(
        &mut self,
        port: PortId,
        serial: SerialConfig,
        settings: Option<PortSettings>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.devices
            .update(cx, |devices, cx| devices.set_notice(None, cx));
        let blank = self
            .active
            .filter(|id| self.tab(*id).is_some_and(SessionTab::is_blank));
        let id = if let Some(id) = self.tab_for_port(&port) {
            self.activate(id, window, cx);
            if self.tab(id).is_some_and(|tab| tab.is_live(cx)) {
                return;
            }
            id
        } else if let Some(id) = blank {
            id
        } else {
            let return_to = self.active;
            let id = self.push_tab();
            if let Some(tab) = self.tab_mut(id) {
                tab.provisional = Some(return_to);
            }
            tracing::info!(tab = %id, %port, tabs = self.tabs.len(), "tab opened");
            self.activate(id, window, cx);
            id
        };
        if let Some(tab) = self.tab_mut(id) {
            tab.port_settings = settings;
        }
        self.open_in_tab(id, port, serial, window, cx);
    }

    /// Open each of `ports` in a tab of its own, and make the first one active.
    pub fn open_ports(&mut self, ports: Vec<PortId>, window: &mut Window, cx: &mut Context<Self>) {
        let mut first = None;
        for port in ports {
            let serial = self.serial_for(&port, cx);
            self.connect(port, serial, window, cx);
            first = first.or(self.active);
        }
        if let Some(first) = first {
            self.activate(first, window, cx);
        }
    }

    /// Open the port of tab `id` again with the settings it has: its session view's
    /// (as the port settings last left them), else those it was restored or opened
    /// with.
    pub fn reconnect_tab(&mut self, id: TabId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tab(id) else {
            return;
        };
        let Some(port) = tab.port.clone() else {
            return;
        };
        if tab.is_live(cx) {
            return;
        }
        let serial = tab
            .view
            .as_ref()
            .map(|view| view.read(cx).serial().clone())
            .or_else(|| tab.serial.clone())
            .unwrap_or_else(|| self.serial_for(&port, cx));
        self.open_in_tab(id, port, serial, window, cx);
    }

    /// Open `port` for tab `id` on the background executor.
    fn open_in_tab(
        &mut self,
        id: TabId,
        port: PortId,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self.port_info(&port, cx);
        let opener = self.opener.clone();
        tracing::info!(tab = %id, %port, serial = %serial.summary(), "opening");
        let (open_port, open_serial) = (port.clone(), serial.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let (task_port, task_serial) = (open_port.clone(), open_serial.clone());
            let result = cx
                .background_spawn(async move { opener.open(&task_port, &task_serial) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.finish_connect(id, open_port, open_serial, result, window, cx);
            })
            .ok();
        });
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        tab.port = Some(port);
        tab.info = Some(info);
        tab.serial = Some(serial);
        tab.error = None;
        tab.connecting = true;
        tab._connect_task = Some(task);
        cx.notify();
    }

    fn finish_connect(
        &mut self,
        id: TabId,
        port: PortId,
        serial: SerialConfig,
        result: Result<Box<dyn SessionHandle>, TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tab_mut(id) else {
            // The tab closed while its port was opening.
            if let Ok(session) = result {
                cx.background_spawn(async move { session.close() }).detach();
            }
            return;
        };
        tab.connecting = false;
        let view = tab.view.clone();
        match result {
            Ok(session) => match view {
                // Connecting again: the same view carries on, scrollback and all.
                Some(view) => {
                    let reopen = Reopen {
                        port,
                        serial,
                        session,
                        waited: 0,
                    };
                    self.reattach(id, view, reopen, window, cx);
                }
                None => self.install_session(id, port, serial, session, window, cx),
            },
            Err(error) => {
                tracing::warn!(%port, %error, "could not open port");
                let notice = format!("Could not open {port}: {error}");
                self.devices.update(cx, |devices, cx| {
                    devices.set_notice(Some(notice.clone().into()), cx)
                });
                let provisional = self.tab_mut(id).and_then(|tab| {
                    tab.error = Some(notice);
                    tab.provisional.take()
                });
                if let Some(return_to) = provisional {
                    self.remove_tab(id, return_to, window, cx);
                }
            }
        }
        cx.notify();
    }

    /// Hand the opened session to the view already in tab `id` (see
    /// [`SessionView::reconnect`]). If the view's last ingest thread is still handing
    /// its store back, try again a frame later, and after a few frames stop it.
    fn reattach(
        &mut self,
        id: TabId,
        view: Entity<SessionView>,
        reopen: Reopen,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.tab(id).is_none() {
            let session = reopen.session;
            cx.background_spawn(async move { session.close() }).detach();
            return;
        }
        if !view.read(cx).can_reconnect() {
            if reopen.waited == REATTACH_PATIENCE {
                view.update(cx, |view, cx| view.retire_ingest(cx));
            }
            let later = view.clone();
            let task = cx.spawn_in(window, async move |this, cx| {
                cx.background_executor()
                    .timer(crate::session_view::FRAME)
                    .await;
                let reopen = Reopen {
                    waited: reopen.waited + 1,
                    ..reopen
                };
                this.update_in(cx, |this, window, cx| {
                    this.reattach(id, later, reopen, window, cx);
                })
                .ok();
            });
            if let Some(tab) = self.tab_mut(id) {
                tab.connecting = true;
                tab._connect_task = Some(task);
            }
            return;
        }
        let Reopen {
            port,
            serial,
            session,
            ..
        } = reopen;
        let info = self.port_info(&port, cx);
        let on_connect = cx
            .try_global::<Config>()
            .and_then(|config| config.settings().on_connect_for(&info).cloned());
        let settings = self.tab_mut(id).and_then(|tab| {
            tab.connecting = false;
            tab.serial = Some(serial.clone());
            tab.info = Some(info);
            tab.error = None;
            tab.shown = None;
            tab.port_settings.take()
        });
        view.update(cx, |view, cx| {
            view.reconnect(session, serial, cx);
            if let Some(settings) = &settings {
                view.apply_port_settings(settings, cx);
            }
        });
        if self.active == Some(id) {
            view.update(cx, |view, cx| view.focus_default(window, cx));
        }
        self.sync_connected_ports(cx);
        tracing::info!(tab = %id, %port, "session open again in its tab");
        if let Some(script) = on_connect {
            match self.read_script(&script, cx) {
                Ok(source) => {
                    view.update(cx, |view, cx| {
                        view.run_script(source, "on_connect", window, cx);
                    });
                }
                Err(message) => self.script_problem_in(Some(id), message, cx),
            }
        }
        cx.notify();
    }

    /// Put a new session view for the opened `session` in tab `id`.
    fn install_session(
        &mut self,
        id: TabId,
        port: PortId,
        serial: SerialConfig,
        session: Box<dyn SessionHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self.port_info(&port, cx);
        let options = self.session_options_for(&info, cx);
        let on_connect = cx
            .try_global::<Config>()
            .and_then(|config| config.settings().on_connect_for(&info).cloned());
        let view = cx
            .new(|cx| SessionView::new(port.clone(), serial.clone(), session, options, window, cx));
        // The status line, the tab bar and the Devices panel's dots follow the session.
        let observer = cx.observe(&view, move |this, view, cx| {
            this.session_notified(id, &view, cx);
        });
        let events = cx.subscribe_in(
            &view,
            window,
            move |this, _, event, window, cx| match event {
                SessionViewEvent::SaveAsCommand { text } => {
                    this.commands.update(cx, |panel, cx| {
                        panel.save_text_as_command(text, window, cx);
                    });
                }
                SessionViewEvent::Script(lines) => {
                    this.console.update(cx, |console, cx| {
                        console.push_lines_to(Some(id), lines.iter().cloned(), cx);
                    });
                    this.reveal_console(Some(id), cx);
                }
                SessionViewEvent::Reconnect => this.reconnect_tab(id, window, cx),
            },
        );
        let active = self.active == Some(id);
        let (restore, settings) = self
            .tab_mut(id)
            .map(|tab| (tab.restore.take(), tab.port_settings.take()))
            .unwrap_or_default();
        let history = self.history.clone();
        let env = self.script_env(cx);
        view.update(cx, |view, cx| {
            view.set_history(history, cx);
            view.attach_scripts(env);
            if let Some(settings) = &settings {
                view.apply_port_settings(settings, cx);
            }
            if let Some(restore) = &restore {
                if let Some(codec) = &restore.codec {
                    view.want_codec(codec, cx);
                }
                view.set_mode(restore.mode, window, cx);
                if let Some(emulation) = restore.emulation {
                    view.set_emulation(emulation, cx);
                }
            }
            if !active {
                view.set_visible(false, cx);
            }
        });
        let Some(tab) = self.tab_mut(id) else {
            return;
        };
        tab.view = Some(view.clone());
        tab.port = Some(port.clone());
        tab.info = Some(info);
        tab.serial = Some(serial);
        tab.error = None;
        tab.provisional = None;
        tab.shown = None;
        tab._observer = Some(observer);
        tab._events = Some(events);
        if active {
            self.show_active(window, cx);
        } else if restore.is_some_and(|restore| restore.mode == Mode::Inline) {
            // Inline mode took the focus for a view that is not on screen.
            self.focus_active(window, cx);
        }
        self.sync_connected_ports(cx);
        tracing::info!(tab = %id, %port, tabs = self.tabs.len(), active, "session open in its tab");
        // The profile's script, now that the port is open and the opener has set DTR and
        // RTS (queued ahead of anything the script writes).
        if let Some(script) = on_connect {
            match self.read_script(&script, cx) {
                Ok(source) => {
                    view.update(cx, |view, cx| {
                        view.run_script(source, "on_connect", window, cx);
                    });
                }
                Err(message) => self.script_problem_in(Some(id), message, cx),
            }
        }
    }

    // --- Session restore -----------------------------------------------------------------

    /// Where the tabs are saved, when a configuration loaded from its directory has
    /// `restore_session` on.
    fn state_file(cx: &App) -> Option<PathBuf> {
        let config = cx.try_global::<Config>()?;
        (config.is_loaded() && config.settings().restore_session)
            .then(|| state_path(config.paths()))
    }

    /// The tabs as `state.json` keeps them: every tab with a port, in order, with the
    /// settings of its session (or those it is to open with).
    pub fn session_state(&self, cx: &App) -> SessionState {
        let mut tabs = Vec::new();
        let mut active = 0;
        for tab in &self.tabs {
            let Some(port) = &tab.port else {
                continue;
            };
            if self.active == Some(tab.id) {
                active = tabs.len();
            }
            tabs.push(match &tab.view {
                Some(view) => {
                    let view = view.read(cx);
                    SavedTab {
                        port: port.clone(),
                        serial: view.serial().clone(),
                        codec: view.codec_name().map(str::to_owned),
                        mode: view.mode().into(),
                        emulation: Some(view.emulation()),
                    }
                }
                None => SavedTab {
                    port: port.clone(),
                    serial: tab
                        .serial
                        .clone()
                        .unwrap_or_else(|| self.serial_for(port, cx)),
                    codec: tab.restore.as_ref().and_then(|r| r.codec.clone()),
                    mode: tab
                        .restore
                        .as_ref()
                        .map_or(Mode::Command, |r| r.mode)
                        .into(),
                    emulation: tab.restore.as_ref().and_then(|r| r.emulation),
                },
            });
        }
        SessionState {
            version: STATE_VERSION,
            active,
            tabs,
            docks: Some(self.docks.saved()),
        }
    }

    /// Write the tabs to `state.json`, if the configuration keeps them and its
    /// directory still exists.
    pub fn save_session_state(&self, cx: &App) {
        let Some(path) = Self::state_file(cx) else {
            return;
        };
        if !path.parent().is_some_and(Path::is_dir) {
            return;
        }
        let state = self.session_state(cx);
        match state.save(&path) {
            Ok(()) => {
                tracing::info!(path = %path.display(), tabs = state.tabs.len(), "saved the open tabs");
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not save the open tabs");
            }
        }
    }

    /// The tabs saved at the last quit, if they are to be restored.
    fn saved_state(&self, cx: &App) -> Option<SessionState> {
        SessionState::load(&Self::state_file(cx)?)
    }

    /// Reopen the tabs of `state`: each port the port source lists connects with its
    /// saved settings, then takes its codec and mode; the others wait in their tabs.
    pub fn restore(&mut self, state: SessionState, window: &mut Window, cx: &mut Context<Self>) {
        let listed: Vec<PortId> = self
            .port_source
            .snapshot()
            .into_iter()
            .map(|info| info.id)
            .collect();
        let mut restored = Vec::new();
        for saved in state.tabs {
            if self.tab_for_port(&saved.port).is_some() {
                continue;
            }
            let id = self.push_tab();
            let info = self.port_info(&saved.port, cx);
            if let Some(tab) = self.tab_mut(id) {
                tab.port = Some(saved.port.clone());
                tab.info = Some(info);
                tab.serial = Some(saved.serial.clone());
                tab.restore = Some(Restore {
                    codec: saved.codec,
                    mode: saved.mode.into(),
                    emulation: saved.emulation,
                });
            }
            restored.push(id);
            if listed.contains(&saved.port) {
                self.open_in_tab(id, saved.port, saved.serial, window, cx);
            } else {
                tracing::info!(port = %saved.port, "restored a tab whose port is not listed");
            }
        }
        tracing::info!(
            tabs = restored.len(),
            "restored the tabs of the last session"
        );
        if let Some(active) = restored.get(state.active).or(restored.first()) {
            self.activate(*active, window, cx);
        }
    }

    // --- Saved commands ------------------------------------------------------------------

    /// Send the saved command `reference` names on the active tab's session. A command
    /// with parameters first asks for them in a dialog, prefilled with the values sent
    /// last in this session or the defaults; the answer goes to the session that was
    /// active when the dialog opened.
    pub fn send_command(
        &mut self,
        reference: CommandRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = cx
            .try_global::<Config>()
            .and_then(|config| config.commands().get(&reference).cloned());
        let Some(command) = command else {
            self.commands.update(cx, |panel, cx| {
                panel.set_notice(Some(Notice::error(format!("No command {reference}"))), cx);
            });
            return;
        };
        let Some(session) = self.session().cloned() else {
            self.commands.update(cx, |panel, cx| {
                panel.set_notice(
                    Some(Notice::error(format!(
                        "Not connected; connect a port to send {}",
                        command.name
                    ))),
                    cx,
                );
            });
            return;
        };
        // A script payload has no bytes: sending it runs the script.
        if let Some(script) = command.payload.script() {
            let script = script.to_path_buf();
            let origin = format!("command {}", command.name);
            self.run_script_path(&script, &origin, window, cx);
            return;
        }
        // A codec payload whose plugin is not installed cannot be sent: say so before
        // asking for its parameters.
        if let Payload::Codec { codec, .. } = &command.payload
            && !cx
                .try_global::<Config>()
                .is_some_and(|config| config.codec_registry().contains(codec))
        {
            let notice = plugin_files::missing_plugin_notice(
                format!("{}: {}", command.name, codec_not_installed(codec)),
                codec,
            );
            session.update(cx, |view, cx| view.set_notice(notice, cx));
            return;
        }
        if command.params.is_empty() {
            session.update(cx, |view, cx| {
                view.send_command(&reference, &command, &ParamValues::new(), cx);
            });
            return;
        }
        let values = session
            .read(cx)
            .remembered_params(&reference)
            .cloned()
            .unwrap_or_else(|| command.defaults());
        let params = command.params.clone();
        let prompt = cx.new(|cx| ParamPrompt::new(reference.clone(), params, &values, window, cx));
        self._param_prompt_events =
            Some(
                cx.subscribe_in(&prompt, window, |this, _, event, window, cx| match event {
                    ParamPromptEvent::Confirmed { reference, values } => {
                        this.send_with_params(reference, values, cx);
                        this.param_prompt = None;
                        window.close_dialog(cx);
                    }
                }),
            );
        self.param_prompt = Some(prompt.clone());
        self.param_target = Some(session.downgrade());
        let title = SharedString::from(format!("Send {}", command.name));
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = prompt.clone();
            let closed = this.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(prompt.clone())
                // The prompt ends in its own Send and Cancel buttons. Confirming emits the values; the subscription sends and closes.
                .on_ok(move |_, _, cx| {
                    confirm.update(cx, |prompt, cx| prompt.confirm(cx));
                    false
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |workspace, _| workspace.param_prompt = None)
                        .ok();
                })
        });
    }

    fn send_with_params(
        &mut self,
        reference: &CommandRef,
        values: &ParamValues,
        cx: &mut Context<Self>,
    ) {
        let command = cx
            .try_global::<Config>()
            .and_then(|config| config.commands().get(reference).cloned());
        let target = self.param_target.take().and_then(|view| view.upgrade());
        if let (Some(command), Some(session)) = (command, target) {
            session.update(cx, |view, cx| {
                view.send_command(reference, &command, values, cx)
            });
        }
    }

    fn send_command_action(
        &mut self,
        action: &crate::actions::commands::Send,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_command(action.reference(), window, cx);
    }

    // --- Scripts ---------------------------------------------------------------------

    /// What a new session's scripts get: the port list, the scripts folder for
    /// `require`, and the saved commands.
    fn script_env(&self, cx: &App) -> ScriptEnv {
        ScriptEnv {
            ports: Some(self.port_source.clone()),
            scripts_dir: cx
                .try_global::<Config>()
                .map(|config| config.paths().scripts_dir()),
            commands: self.script_commands.clone(),
        }
    }

    /// The script at `path` (under the scripts folder, or absolute), read now, or why it
    /// could not be read.
    fn read_script(&self, path: &Path, cx: &App) -> std::result::Result<ScriptSource, String> {
        let paths = cx
            .try_global::<Config>()
            .map(|config| config.paths().clone())
            .unwrap_or_else(ConfigPaths::default_for_platform);
        let file = resolve_script(&paths, path);
        // Small, local and read once per run: read here, like the settings.
        let code = std::fs::read_to_string(&file)
            .map_err(|error| format!("Could not read the script {}: {error}", file.display()))?;
        Ok(ScriptSource {
            name: display_name(&paths, &file),
            code,
            path: Some(file),
        })
    }

    /// Run the script at `path` (under the scripts folder, or absolute) on the active
    /// tab's session; `origin` says what started it. Returns whether it was queued; a
    /// script that cannot be read, or no session, is reported in the console.
    pub fn run_script_path(
        &mut self,
        path: &Path,
        origin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.read_script(path, cx) {
            Ok(source) => self.run_script(source, origin, window, cx),
            Err(message) => {
                self.script_problem(message, cx);
                false
            }
        }
    }

    /// Run a REPL line on the active tab's session.
    pub fn run_inline(&mut self, code: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.run_script(inline_source(code), "console", window, cx);
    }

    /// Queue `source` on the active tab's session's script thread. It runs there to the
    /// end, whichever tab is active meanwhile.
    pub fn run_script(
        &mut self,
        source: ScriptSource,
        origin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session().cloned() else {
            self.script_problem(
                format!("Not connected; connect a port to run {}", source.name),
                cx,
            );
            return false;
        };
        session.update(cx, |view, cx| view.run_script(source, origin, window, cx))
    }

    /// Stop the script running on the active tab's session.
    pub fn stop_script(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.update(cx, |view, cx| view.stop_script(cx));
        }
    }

    /// Report a problem in the active tab's console output.
    fn script_problem(&mut self, message: String, cx: &mut Context<Self>) {
        self.script_problem_in(self.active, message, cx);
    }

    fn script_problem_in(&mut self, tab: Option<TabId>, message: String, cx: &mut Context<Self>) {
        tracing::warn!("{message}");
        self.console.update(cx, |console, cx| {
            console.push_lines_to(tab, [ConsoleLine::new(ConsoleKind::Error, message)], cx);
        });
        self.reveal_console(tab, cx);
    }

    fn run_script_action(
        &mut self,
        action: &RunScript,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.run_script_path(Path::new(&action.path), "key binding", window, cx);
    }

    fn run_inline_action(&mut self, _: &RunInline, window: &mut Window, cx: &mut Context<Self>) {
        self.console
            .update(cx, |console, cx| console.submit_inline(window, cx));
    }

    fn stop_script_action(&mut self, _: &StopScript, _: &mut Window, cx: &mut Context<Self>) {
        self.stop_script(cx);
    }

    fn clear_console_action(&mut self, _: &ClearConsole, _: &mut Window, cx: &mut Context<Self>) {
        self.console.update(cx, |console, cx| console.clear(cx));
    }

    // --- Actions on the active tab -------------------------------------------------------

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.update(cx, |view, cx| view.clear(cx));
        }
    }

    /// Disconnect the active tab's session, asking first while it records or runs a
    /// script.
    fn disconnect(&mut self, _: &Disconnect, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session().cloned() {
            session.update(cx, |view, cx| view.request_disconnect(window, cx));
        }
    }

    fn pause(&mut self, _: &Pause, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.update(cx, |view, cx| view.toggle_pause(cx));
        }
    }

    fn export(&mut self, _: &Export, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.update(cx, |view, cx| view.export(ExportFormat::Text, cx));
        }
    }

    fn toggle_record(&mut self, _: &ToggleRecord, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session() {
            session.update(cx, |view, cx| view.toggle_record(cx));
        }
    }

    /// The session view handles the toggle when it holds the focus; this is for when
    /// another panel does.
    fn toggle_inline(&mut self, _: &ToggleInline, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.session().cloned() {
            session.update(cx, |view, cx| view.toggle_mode(window, cx));
        }
    }

    /// As [`Self::toggle_inline`], for monitor and VT mode.
    fn toggle_emulation(&mut self, _: &ToggleEmulation, cx: &mut Context<Self>) {
        if let Some(session) = self.session().cloned() {
            session.update(cx, |view, cx| view.toggle_emulation(cx));
        }
    }

    // --- The command palette -------------------------------------------------------------

    /// The command palette, while it is open.
    pub fn palette(&self) -> Option<&Entity<CommandPalette>> {
        self.palette.as_ref()
    }

    /// Where the palette's actions may run from, best first: `previous` (what had the
    /// focus before it opened), then the active session's terminal, compose bar and view,
    /// the panels, and the workspace.
    fn palette_targets(&self, previous: Option<FocusHandle>, cx: &App) -> Vec<FocusHandle> {
        let mut handles: Vec<FocusHandle> = previous.into_iter().collect();
        if let Some(session) = self.session() {
            let view = session.read(cx);
            handles.push(view.terminal().focus_handle(cx));
            handles.push(view.compose().read(cx).input().focus_handle(cx));
            handles.push(view.focus_handle(cx));
        }
        handles.push(self.devices.focus_handle(cx));
        handles.push(self.commands.focus_handle(cx));
        handles.push(self.console.focus_handle(cx));
        handles.push(self.decoded.focus_handle(cx));
        handles.push(self.focus_handle.clone());
        handles
    }

    /// Open the command palette in a dialog, or close it if it is open.
    pub fn toggle_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.palette.take().is_some() {
            window.close_dialog(cx);
            cx.notify();
            return;
        }
        let previous = window.focused(cx);
        let targets = self.palette_targets(previous, cx);
        let entries = palette::entries(&targets, window, cx);
        let palette = cx.new(|cx| CommandPalette::new(entries, window, cx));
        self._palette_events = Some(cx.subscribe_in(
            &palette,
            window,
            move |this, _, event, window, cx| match event {
                PaletteEvent::Confirmed(target) => {
                    this.palette = None;
                    window.close_dialog(cx);
                    let target = target.clone();
                    let targets = targets.clone();
                    let workspace = cx.entity().downgrade();
                    // Once the dialog is gone, from where the action would run.
                    window.defer(cx, move |window, cx| {
                        workspace
                            .update(cx, |this, cx| {
                                this.run_palette_target(target, &targets, window, cx);
                            })
                            .ok();
                    });
                }
            },
        ));
        self.palette = Some(palette.clone());
        let this = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = palette.clone();
            let closed = this.clone();
            dialog
                .w(px(600.))
                .margin_top(px(72.))
                .close_button(false)
                .child(palette.clone())
                // Enter: the palette runs its selection and the workspace closes it.
                .on_ok(move |_, _, cx| {
                    confirm.update(cx, |palette, cx| palette.confirm(cx));
                    false
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |workspace, _| workspace.palette = None)
                        .ok();
                })
        });
        if let Some(palette) = &self.palette {
            palette.update(cx, |palette, cx| palette.focus(window, cx));
        }
        cx.notify();
    }

    /// Run what the palette confirmed.
    fn run_palette_target(
        &mut self,
        target: PaletteTarget,
        targets: &[FocusHandle],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(?target, "from the command palette");
        match target {
            PaletteTarget::Command(reference) => self.send_command(reference, window, cx),
            PaletteTarget::Script(path) => {
                self.run_script_path(Path::new(&path), "palette", window, cx);
            }
            PaletteTarget::Action(action) => {
                let handle = palette::target_for(action.as_ref(), targets, window).cloned();
                // Its handler may be the workspace's own, which is busy now.
                window.defer(cx, move |window, cx| match handle {
                    Some(handle) => handle.dispatch_action(action.as_ref(), window, cx),
                    None => window.dispatch_action(action, cx),
                });
            }
        }
    }

    fn toggle_command_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_palette(window, cx);
    }

    fn install_example_plugin_action(
        &mut self,
        action: &InstallExamplePlugin,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.install_example_plugin(&action.name, cx);
    }

    /// Install the bundled example plugin `name` (the watcher then loads it), and say
    /// how it went in the active session's status line, or the Devices panel without one.
    pub fn install_example_plugin(&mut self, name: &str, cx: &mut Context<Self>) {
        match self.session().cloned() {
            Some(session) => {
                session.update(cx, |view, cx| view.install_example_plugin(name, cx));
            }
            None => {
                let notice = plugin_files::install_example(name, cx);
                self.devices.update(cx, |devices, cx| {
                    devices.set_notice(Some(notice.text.into()), cx)
                });
            }
        }
    }

    fn new_tab_action(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.new_tab(window, cx);
    }

    fn close_tab_action(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active {
            self.request_close(id, window, cx);
        }
    }

    fn next_tab_action(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_next(true, window, cx);
    }

    fn previous_tab_action(
        &mut self,
        _: &PreviousTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_next(false, window, cx);
    }

    // --- Docks --------------------------------------------------------------------------

    /// Which panels are open, and the dock widths.
    pub fn docks(&self) -> &Docks {
        &self.docks
    }

    /// Whether `panel` is on screen: open, in a dock the window width leaves room for.
    pub fn is_panel_shown(&self, panel: DockPanel) -> bool {
        self.docks.is_shown(panel)
    }

    /// The rail's click: close `panel` if it shows, else open it (in a narrow window too).
    pub fn toggle_panel(&mut self, panel: DockPanel, cx: &mut Context<Self>) {
        self.docks.toggle(panel);
        tracing::debug!(
            ?panel,
            shown = self.docks.is_shown(panel),
            "dock panel toggled"
        );
        cx.notify();
    }

    /// Open or close `panel` as the user would (opening it in a narrow window too).
    pub fn set_panel_open(&mut self, panel: DockPanel, open: bool, cx: &mut Context<Self>) {
        if self.docks.set_open(panel, open) {
            cx.notify();
        }
    }

    /// Drag a dock to `width`.
    pub fn resize_dock(&mut self, side: DockSide, width: Pixels, cx: &mut Context<Self>) {
        self.docks.resize(side, width);
        cx.notify();
    }

    /// The Decoded panel follows the active session: open while it decodes, closed while
    /// it does not. Only a change does anything, so the user's own choice stands until the
    /// codec (or the tab) changes.
    fn sync_decoded_panel(&mut self, cx: &mut Context<Self>) {
        let decoding = self
            .session()
            .is_some_and(|session| session.read(cx).codec().is_some());
        if self.decoding_seen != Some(decoding) {
            self.decoding_seen = Some(decoding);
            self.docks.show_by_app(DockPanel::Decoded, decoding);
            cx.notify();
        }
    }

    /// Script output for the active tab: open the Script console to show it.
    fn reveal_console(&mut self, tab: Option<TabId>, cx: &mut Context<Self>) {
        if tab == self.active && !self.docks.is_open(DockPanel::Scripts) {
            self.docks.show_by_app(DockPanel::Scripts, true);
            cx.notify();
        }
    }

    fn drag_dock(
        &mut self,
        event: &DragMoveEvent<DockDrag>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let x = event.event.position.x;
        let side = event.drag(cx).0;
        let width = match side {
            DockSide::Left => x - RAIL_WIDTH,
            DockSide::Right => window.viewport_size().width - x - RAIL_WIDTH,
        };
        self.resize_dock(side, width, cx);
    }

    // --- Rendering -------------------------------------------------------------------

    fn render_tab(&self, ix: usize, label: TabLabel, cx: &mut Context<Self>) -> Tab {
        let theme = cx.theme();
        let id = label.id;
        let color = match label.state {
            TabState::Connected => theme.success,
            TabState::Paused => theme.warning,
            TabState::Recording | TabState::Lost => theme.danger,
            TabState::Connecting => theme.info,
            TabState::Disconnected | TabState::NotConnected | TabState::Empty => {
                theme.muted_foreground
            }
        };
        let hollow = matches!(
            label.state,
            TabState::Lost | TabState::NotConnected | TabState::Empty
        );
        let state = SharedString::from(label.state.label());
        let is_settings = self.tab(id).is_some_and(|tab| tab.settings.is_some());
        let dot = if is_settings {
            div().id(("tab-dot", id.0 as usize)).flex_none().child(
                Icon::new(IconName::Settings)
                    .size_3p5()
                    .text_color(theme.muted_foreground),
            )
        } else {
            div()
                .id(("tab-dot", id.0 as usize))
                .flex_none()
                .child(chrome::state_dot(color, hollow))
                .tooltip(move |window, cx| Tooltip::new(state.clone()).build(window, cx))
        };
        let title = SharedString::from(label.title);
        let muted = theme.muted_foreground;
        let drag_border = theme.drag_border;
        let dragged = DraggedTab {
            id,
            title: title.clone(),
        };
        Tab::new()
            .label(title)
            .prefix(div().pl_2().child(dot))
            .suffix(
                h_flex()
                    .gap_1()
                    .pr_1()
                    .items_center()
                    .children(label.suffix.map(|suffix| {
                        div()
                            .max_w(px(160.))
                            .truncate()
                            .text_size(chrome::LABEL_SIZE)
                            .text_color(muted)
                            .child(SharedString::from(suffix))
                    }))
                    .children(label.unseen.map(|unseen| {
                        div()
                            .text_size(chrome::LABEL_SIZE)
                            .text_color(muted)
                            .child(SharedString::from(unseen))
                    }))
                    .child(
                        chrome::icon_button(("close-tab", id.0 as usize), IconName::X, cx)
                            .xsmall()
                            .tooltip_with_action("Close tab", &CloseTab, Some(context::WORKSPACE))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.request_close(id, window, cx);
                            })),
                    ),
            )
            .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
            .drag_over::<DraggedTab>(move |style, _, _, _| {
                style.border_l_2().border_color(drag_border)
            })
            .on_drop(cx.listener(move |this, dragged: &DraggedTab, _, cx| {
                this.drop_tab(dragged.id, ix, cx);
            }))
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let labels = self.tab_labels(cx);
        let selected = labels.iter().position(|label| label.active).unwrap_or(0);
        let tabs: Vec<Tab> = labels
            .into_iter()
            .enumerate()
            .map(|(ix, label)| self.render_tab(ix, label, cx))
            .collect();
        TabBar::new("session-tabs")
            .small()
            .max_width(TAB_MAX_WIDTH)
            .selected_index(selected)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.activate_index(*ix, window, cx);
            }))
            .children(tabs)
            .suffix(
                div().px_1().child(
                    chrome::icon_button("new-tab", IconName::Plus, cx)
                        .tooltip_with_action("New tab", &NewTab, Some(context::WORKSPACE))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.new_tab(window, cx);
                        })),
                ),
            )
            .into_any_element()
    }

    /// What the center shows for a tab with no session view.
    fn render_placeholder(&self, cx: &mut Context<Self>) -> AnyElement {
        let (title, message, reconnect) = match self.active_tab() {
            Some(tab) if tab.connecting => (
                format!(
                    "Opening {}\u{2026}",
                    tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
                ),
                "The port opens in the background.".to_owned(),
                None,
            ),
            Some(tab) if tab.port.is_some() => (
                format!(
                    "{} is not connected",
                    tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
                ),
                tab.error.clone().unwrap_or_else(|| {
                    "Plug the device in, then Connect (or pick another port).".to_owned()
                }),
                Some(tab.id),
            ),
            _ => (
                "No session".to_owned(),
                "Pick a port in Devices and press Enter, or double-click it.".to_owned(),
                None,
            ),
        };
        let palette_key = self.palette_binding.clone();
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .bg(theme.background)
            .text_color(theme.muted_foreground)
            .child(
                div()
                    .flex_none()
                    .size_10()
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(theme.muted)
                    .child(
                        Icon::new(IconName::Usb)
                            .size_5()
                            .text_color(theme.muted_foreground),
                    ),
            )
            .child(
                div()
                    .text_base()
                    .text_color(theme.foreground)
                    .child(SharedString::from(title)),
            )
            .child(div().text_sm().child(SharedString::from(message)))
            .when_some(palette_key, |this, key| {
                this.child(
                    div()
                        .pt_1()
                        .text_size(chrome::LABEL_SIZE)
                        .child(SharedString::from(format!("{key} for every action"))),
                )
            })
            .when_some(reconnect, |this, id| {
                this.child(
                    div().pt_2().child(
                        Button::new("reconnect-tab")
                            .icon(IconName::Plug)
                            .label("Connect")
                            .small()
                            .primary()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.reconnect_tab(id, window, cx);
                            })),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_center(&self, cx: &mut Context<Self>) -> AnyElement {
        let settings = self.active_tab().and_then(|tab| tab.settings.clone());
        let body = match (settings, self.session()) {
            (Some(settings), _) => settings.into_any_element(),
            (None, Some(session)) => session.clone().into_any_element(),
            (None, None) => self.render_placeholder(cx),
        };
        v_flex()
            .id("center")
            .test_support()
            .flex_1()
            .h_full()
            .min_w(CENTER_MIN)
            .when(self.shows_tab_bar(), |this| {
                this.child(self.render_tab_bar(cx))
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }

    /// Whether the right rail has the Decoded panel's icon: while a codec plugin is
    /// installed, while the active session decodes, or while the panel is open (so it
    /// can be closed). With no plugin installed nothing on screen is about codecs.
    pub fn has_decoded_rail(&self, cx: &App) -> bool {
        plugin_files::has_codecs(cx)
            || self.docks.is_open(DockPanel::Decoded)
            || self
                .session()
                .is_some_and(|session| session.read(cx).codec().is_some())
    }

    /// A dock's rail: an icon per panel, pressed while the panel shows.
    fn render_rail(&self, side: DockSide, cx: &mut Context<Self>) -> Div {
        let decoded = self.has_decoded_rail(cx);
        let theme = cx.theme();
        let (border, background) = (theme.border, theme.sidebar);
        let expanded = self.docks.is_expanded(side);
        let panels = DockPanel::ALL
            .into_iter()
            .filter(|panel| panel.side() == side)
            .filter(|panel| *panel != DockPanel::Decoded || decoded)
            .map(|panel| {
                let shown = self.docks.is_shown(panel);
                chrome::toggle_button(panel.rail_id(), rail_icon(panel), shown, cx)
                    .tooltip_placement(match side {
                        DockSide::Left => Placement::Right,
                        DockSide::Right => Placement::Left,
                    })
                    .tooltip(if shown {
                        format!("Hide {}", panel.title())
                    } else {
                        format!("Show {}", panel.title())
                    })
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_panel(panel, cx)))
            })
            .collect::<Vec<_>>();
        v_flex()
            .flex_none()
            .w(RAIL_WIDTH)
            .h_full()
            .pt_1()
            .gap_1()
            .items_center()
            .bg(background)
            // The edge toward the center, when no panel stands between.
            .when(!expanded, |rail| match side {
                DockSide::Left => rail.border_r_1().border_color(border),
                DockSide::Right => rail.border_l_1().border_color(border),
            })
            .children(panels)
    }

    /// A dock: its panels, stacked, at `width`, with a handle on its inner edge.
    fn render_dock(&self, side: DockSide, width: Pixels, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (border, background, drag_border) = (theme.border, theme.sidebar, theme.drag_border);
        let (first, second, split, first_size): (AnyView, AnyView, &'static str, Pixels) =
            match side {
                DockSide::Left => (
                    self.devices.clone().into(),
                    self.commands.clone().into(),
                    "left-dock-split",
                    px(340.),
                ),
                DockSide::Right => (
                    self.decoded.clone().into(),
                    self.console.clone().into(),
                    "right-dock-split",
                    px(380.),
                ),
            };
        let (first_panel, second_panel) = match side {
            DockSide::Left => (DockPanel::Devices, DockPanel::Commands),
            DockSide::Right => (DockPanel::Decoded, DockPanel::Scripts),
        };
        let content = match (
            self.docks.is_open(first_panel),
            self.docks.is_open(second_panel),
        ) {
            (true, true) => v_resizable(split)
                .child(
                    resizable_panel()
                        .size(first_size)
                        .size_range(px(120.)..px(1200.))
                        .child(first),
                )
                .child(resizable_panel().child(second))
                .into_any_element(),
            (true, false) => first.into_any_element(),
            _ => second.into_any_element(),
        };
        let handle = div()
            .id(match side {
                DockSide::Left => "left-dock-handle",
                DockSide::Right => "right-dock-handle",
            })
            .absolute()
            .top_0()
            .bottom_0()
            .w(px(5.))
            .map(|handle| match side {
                DockSide::Left => handle.right(px(-3.)),
                DockSide::Right => handle.left(px(-3.)),
            })
            .cursor_col_resize()
            .hover(|style| style.bg(drag_border.opacity(0.6)))
            .on_drag(DockDrag(side), |drag, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| drag.clone())
            });
        div()
            .id(match side {
                DockSide::Left => "left-dock",
                DockSide::Right => "right-dock",
            })
            .test_support()
            .flex_none()
            .relative()
            .w(width)
            .h_full()
            .bg(background)
            .map(|dock| match side {
                DockSide::Left => dock.border_r_1().border_color(border),
                DockSide::Right => dock.border_l_1().border_color(border),
            })
            .child(div().size_full().overflow_hidden().child(content))
            .child(handle)
            .into_any_element()
    }

    fn render_status_line(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let line = h_flex()
            .id("status-line")
            .flex_none()
            .overflow_hidden()
            .h(chrome::STATUS_HEIGHT)
            .pl_1()
            .pr_2()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme.status_bar_border)
            .bg(theme.status_bar)
            .text_size(chrome::LABEL_SIZE)
            .text_color(theme.muted_foreground);
        let config_notice = self.config_notice(cx).map(|notice| {
            let color = if notice.is_error {
                theme.danger
            } else {
                theme.warning
            };
            status_notice("status-config", notice.text, color)
        });

        // With the Settings tab in front the segment is the session that was in front
        // before it, for reading: its controls act on the active tab's session, which
        // there is none of, so they are left off.
        let live = self.session().is_some();
        let Some(session) = self.status_session().cloned() else {
            let label = self.status_placeholder(cx);
            return line
                .child(
                    h_flex()
                        .px_1()
                        .gap_1p5()
                        .items_center()
                        .child(chrome::state_dot(theme.muted_foreground, true))
                        .child(SharedString::from(label)),
                )
                .child(h_flex().flex_1().min_w_0().children(config_notice))
                .into_any_element();
        };

        let view = session.read(cx);
        let status = view.status_line();
        let inline = view.mode() == Mode::Inline;
        let state_color = match view.state() {
            // The device rang the bell (VT mode): the dot flashes once.
            _ if view.bell_flashing() => theme.warning,
            ConnectionState::Connected => theme.success,
            ConnectionState::Disconnected { error: None } => theme.muted_foreground,
            ConnectionState::Disconnected { error: Some(_) } => theme.danger,
        };
        let disconnected = view.state().is_disconnected();
        let port = view.port().to_string();
        let settings = view.serial().summary();
        let form = view.port_form().clone();
        let (foreground, warning, danger, info, border) = (
            theme.foreground,
            theme.warning,
            theme.danger,
            theme.info,
            theme.border,
        );

        // The connection: state, port and settings, one segment that opens the settings.
        let sync = session.downgrade();
        let segment = h_flex()
            .gap_1p5()
            .items_center()
            .child(chrome::state_dot(state_color, disconnected))
            .child(div().text_color(foreground).child(SharedString::from(port)))
            .child(SharedString::from(settings));
        let connection = if live {
            Popover::new("status-port-popover")
                .anchor(Anchor::BottomLeft)
                .trigger(
                    Button::new("status-port")
                        .ghost()
                        .xsmall()
                        .tooltip(format!("{}: port settings", status.state))
                        .child(segment),
                )
                .content(move |_, _, _| form.clone())
                .on_open_change(move |open: &bool, window, cx| {
                    if *open {
                        sync.update(cx, |view, cx| view.sync_port_form(window, cx))
                            .ok();
                    }
                })
                .into_any_element()
        } else {
            div()
                .id("status-port-previous")
                .test_support()
                .flex_none()
                .px_2()
                .child(segment)
                .into_any_element()
        };

        // The newest notice, cut to fit, all of it in the tooltip, and its button (such
        // as Install for a plugin a device profile names but that is not installed).
        let notice_action = status
            .notice
            .as_ref()
            .and_then(|notice| notice.action.clone())
            .map(|action| notice_action_button(action, session.downgrade()));
        let notice = status.notice.clone().map(|notice| {
            let color = if notice.is_error {
                danger
            } else {
                theme.muted_foreground
            };
            status_notice("status-notice", notice.text, color)
        });

        let mode = div()
            .id("status-mode")
            .flex_none()
            .h(px(16.))
            .px_1p5()
            .flex()
            .items_center()
            .rounded(px(4.))
            .border_1()
            .border_color(if inline { warning } else { border })
            .text_color(if inline {
                warning
            } else {
                theme.muted_foreground
            })
            .child(status.mode)
            .when(live, |chip| {
                chip.cursor_pointer()
                    .hover(|style| style.bg(theme.list_hover))
                    .tooltip(|window, cx| {
                        Tooltip::new("Switch between command and inline mode")
                            .action(&ToggleInline, Some(context::WORKSPACE))
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_inline(&ToggleInline, window, cx);
                    }))
            });

        // RX with its rate, and what the scrollback keeps in the tooltip.
        let mut details = vec![status.retained.clone()];
        details.extend(status.evicted.clone());
        details.extend(status.paste.clone());
        let details = SharedString::from(details.join("\n"));
        let rx = h_flex()
            .id("status-rx")
            .flex_none()
            .gap_1()
            .child(SharedString::from(status.rx.clone()))
            .children(
                status
                    .rx_rate
                    .clone()
                    .map(|rate| div().text_color(info).child(SharedString::from(rate))),
            )
            .children(
                status
                    .paste
                    .clone()
                    .map(|_| div().text_color(info).child("pasting")),
            )
            .tooltip(move |window, cx| Tooltip::new(details.clone()).build(window, cx));
        let tx = h_flex()
            .id("status-tx")
            .flex_none()
            .gap_1()
            .child(SharedString::from(status.tx.clone()))
            .children(
                status
                    .tx_rate
                    .clone()
                    .map(|rate| div().text_color(info).child(SharedString::from(rate))),
            );

        let emulation = status.emulation.map(|label| {
            chrome::chip(info)
                .id("status-emulation")
                .child(Icon::new(IconName::Terminal).size_3())
                .child(label)
                .when(live, |chip| {
                    chip.cursor_pointer()
                        .tooltip(|window, cx| {
                            Tooltip::new("VT mode: the device draws on a terminal screen")
                                .action(&ToggleEmulation, Some(context::TERMINAL))
                                .build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.toggle_emulation(&ToggleEmulation, cx);
                        }))
                })
        });
        let codec = view.codec_name().map(|codec| {
            chrome::chip(info)
                .id("status-codec")
                .child(Icon::new(IconName::Braces).size_3())
                .child(SharedString::from(codec.to_owned()))
                .when(live, |chip| {
                    chip.cursor_pointer()
                        .tooltip(|window, cx| {
                            Tooltip::new("Decoding: show the Decoded panel").build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_panel_open(DockPanel::Decoded, true, cx);
                        }))
                })
        });
        let paused = status.paused.clone().map(|paused| {
            let short = paused
                .split(", ")
                .nth(1)
                .map_or_else(|| "Paused".to_owned(), |since| format!("Paused {since}"));
            let full = SharedString::from(paused);
            chrome::chip(warning)
                .id("status-paused")
                .child(Icon::new(IconName::Pause).size_3())
                .child(SharedString::from(short))
                .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        });
        let recording = status.recording.clone().map(|recording| {
            chrome::chip(danger)
                .id("status-recording")
                .child(chrome::state_dot(danger, false))
                .child(SharedString::from(recording))
        });
        let script = status.script.clone().map(|script| {
            chrome::chip(info)
                .id("status-script")
                .child(Icon::new(IconName::ScrollText).size_3())
                .child(SharedString::from(script))
                .when(live, |chip| {
                    chip.cursor_pointer()
                        .tooltip(|window, cx| {
                            Tooltip::new("Show the Script console").build(window, cx)
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.set_panel_open(DockPanel::Scripts, true, cx);
                        }))
                })
        });
        line.child(connection)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .children(notice)
                    .children(notice_action)
                    .children(config_notice),
            )
            .children(script)
            .children(recording)
            .children(paused)
            .children(codec)
            .children(emulation)
            .child(mode)
            .child(rx)
            .child(tx)
            .into_any_element()
    }

    /// The status line's text for the session it shows ([`Self::status_session`]), as
    /// rendered.
    pub fn status_line(&self, cx: &App) -> Option<StatusLine> {
        self.status_session()
            .map(|session| session.read(cx).status_line())
    }

    /// What the status line says about the configuration: a file that did not load, a
    /// theme that was not found, an unknown key.
    pub fn config_notice(&self, cx: &App) -> Option<Notice> {
        cx.try_global::<Config>().and_then(Config::notice)
    }
}

/// A notice in the status line: one line, cut with an ellipsis, whole in its tooltip.
fn status_notice(id: &'static str, text: String, color: Hsla) -> Stateful<Div> {
    let text = SharedString::from(text);
    let tip = text.clone();
    div()
        .id(id)
        .min_w_0()
        .truncate()
        .text_color(color)
        .child(text)
        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
}

/// The button after a notice that offers a way out, which runs on `session`.
fn notice_action_button(action: NoticeAction, session: WeakEntity<SessionView>) -> Button {
    let tooltip = match &action {
        NoticeAction::InstallExamplePlugin(name) => {
            format!("Install the bundled {name} example plugin into the plugins folder")
        }
        NoticeAction::OpenPluginsFolder => "Open the plugins folder".to_owned(),
    };
    Button::new("status-notice-action")
        .label(action.label())
        .xsmall()
        .ghost()
        .flex_none()
        .tooltip(tooltip)
        .on_click(move |_, _, cx| {
            session
                .update(cx, |view, cx| view.run_notice_action(&action, cx))
                .ok();
        })
}

/// The icon of a panel on its rail.
fn rail_icon(panel: DockPanel) -> IconName {
    match panel {
        DockPanel::Devices => IconName::Usb,
        DockPanel::Commands => IconName::SquareTerminal,
        DockPanel::Decoded => IconName::Braces,
        DockPanel::Scripts => IconName::ScrollText,
    }
}

/// A dock edge being dragged.
#[derive(Clone)]
struct DockDrag(DockSide);

impl Render for DockDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.window_title();
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let viewport = window.viewport_size().width;
        self.docks.observe_window_width(viewport);
        let widths = self.docks.widths(viewport);
        // The session's toolbar is laid out for the width the center gets.
        let center = viewport
            - RAIL_WIDTH * 2.
            - widths.left.unwrap_or_default()
            - widths.right.unwrap_or_default();
        if let Some(session) = self.session().cloned() {
            let hint = if center < CENTER_MIN {
                CENTER_MIN
            } else {
                center
            };
            session.update(cx, |view, _| view.set_width_hint(Some(hint)));
        }
        if self.palette_binding.is_none() {
            self.palette_binding = window
                .highest_precedence_binding_for_action_in(&ToggleCommandPalette, &self.focus_handle)
                .as_ref()
                .and_then(chrome::keystroke_text);
        }

        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        // Panels, the status line and inputs inherit the UI font's weight and OpenType
        // features from here; gpui-kit's theme carries its family and size.
        let ui_font = cx
            .try_global::<Config>()
            .map(|config| config.ui_font().font.clone());
        let left_rail = self.render_rail(DockSide::Left, cx);
        let right_rail = self.render_rail(DockSide::Right, cx);
        let left = widths
            .left
            .map(|width| self.render_dock(DockSide::Left, width, cx));
        let right = widths
            .right
            .map(|width| self.render_dock(DockSide::Right, width, cx));
        let center = self.render_center(cx);
        let status_line = self.render_status_line(cx);

        v_flex()
            .id("workspace")
            .key_context(context::WORKSPACE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::disconnect))
            .on_action(cx.listener(Self::pause))
            .on_action(cx.listener(Self::export))
            .on_action(cx.listener(Self::toggle_record))
            .on_action(cx.listener(Self::toggle_inline))
            .on_action(cx.listener(|this, action: &ToggleEmulation, _, cx| {
                this.toggle_emulation(action, cx);
            }))
            .on_action(cx.listener(Self::send_command_action))
            .on_action(cx.listener(Self::run_script_action))
            .on_action(cx.listener(Self::run_inline_action))
            .on_action(cx.listener(Self::stop_script_action))
            .on_action(cx.listener(Self::clear_console_action))
            .on_action(cx.listener(Self::new_tab_action))
            .on_action(cx.listener(Self::close_tab_action))
            .on_action(cx.listener(Self::next_tab_action))
            .on_action(cx.listener(Self::previous_tab_action))
            .on_action(cx.listener(Self::toggle_command_palette))
            .on_action(cx.listener(Self::install_example_plugin_action))
            .on_action(cx.listener(Self::open_settings_ui_action))
            .on_action(cx.listener(|this, _: &ActivateTab1, window, cx| {
                this.activate_index(0, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab2, window, cx| {
                this.activate_index(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab3, window, cx| {
                this.activate_index(2, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab4, window, cx| {
                this.activate_index(3, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab5, window, cx| {
                this.activate_index(4, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab6, window, cx| {
                this.activate_index(5, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab7, window, cx| {
                this.activate_index(6, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab8, window, cx| {
                this.activate_index(7, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ActivateTab9, window, cx| {
                this.activate_index(8, window, cx);
            }))
            .on_drag_move(cx.listener(Self::drag_dock))
            .size_full()
            .when_some(ui_font, |this, font| this.font(font))
            .bg(background)
            .text_color(foreground)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(left_rail)
                    .children(left)
                    .child(center)
                    .children(right)
                    .child(right_rail),
            )
            .child(status_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::keys;
    use crate::test_support::{
        FakeOpener, FakePortSource, allow_engine_threads, displayed, open_test_window, port,
        run_until,
    };

    fn open_workspace(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Arc<FakeOpener>) {
        allow_engine_threads(cx);
        let source = FakePortSource::new([port("/dev/a"), port("virtual:echo")]);
        let opener = Arc::new(FakeOpener::default());
        let for_window = opener.clone();
        let (window, workspace) = open_test_window(cx, move |window, cx| {
            Workspace::with_opener(source, for_window, None, window, cx)
        });
        cx.run_until_parked();
        (window, workspace, opener)
    }

    #[gpui_test]
    fn connecting_from_the_panel_opens_a_session_view(cx: &mut TestAppContext) {
        let (_window, workspace, opener) = open_workspace(cx);
        let devices = workspace.read_with(cx, |w, _| w.devices().clone());
        devices.update(cx, |devices, cx| {
            assert!(devices.connect_selected(cx));
        });
        cx.run_until_parked();

        let opened = opener.opened();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].0, PortId::new("/dev/a"));
        opened[0].2.connected("/dev/a @ 115200 8N1");
        opened[0].2.data(b"boot ok\n");
        let session = workspace.read_with(cx, |w, _| w.session().expect("session view").clone());
        run_until(cx, "the boot line", |cx| displayed(cx, &session).len() == 2);

        assert_eq!(displayed(cx, &session)[1].text, "boot ok");
        workspace.read_with(cx, |workspace, cx| {
            let session = workspace.session().expect("session view").read(cx);
            assert_eq!(session.state(), &ConnectionState::Connected);
            assert_eq!(workspace.connecting(), None);
            assert_eq!(workspace.tab_count(), 1);
            assert!(!workspace.shows_tab_bar(), "one tab needs no bar");
            let status = workspace.status_line(cx).expect("a status line");
            assert_eq!(status.title, "/dev/a @ 115200 8N1");
            assert_eq!(status.retained, "2 lines, 8 B kept");
            assert_eq!(workspace.window_title(), "/dev/a \u{2014} Serialist");
        });
    }

    #[gpui_test]
    fn failed_open_is_reported_in_the_panel(cx: &mut TestAppContext) {
        let (_window, workspace, opener) = open_workspace(cx);
        opener.fail_next("no backend");
        let devices = workspace.read_with(cx, |w, _| w.devices().clone());
        devices.update(cx, |devices, cx| {
            devices.connect_selected(cx);
        });
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            assert!(workspace.session().is_none());
            assert_eq!(workspace.tab_count(), 0, "the tab made for it went away");
            assert_eq!(workspace.window_title(), "Serialist");
            let notice = workspace.devices().read(cx).notice().cloned();
            assert_eq!(
                notice.as_deref(),
                Some("Could not open /dev/a: unsupported on this transport: no backend")
            );
        });
    }

    #[gpui_test]
    fn keybindings_clear_and_disconnect(cx: &mut TestAppContext) {
        let (window, workspace, opener) = open_workspace(cx);
        let devices = workspace.read_with(cx, |w, _| w.devices().clone());
        devices.update(cx, |devices, cx| {
            devices.connect_selected(cx);
        });
        cx.run_until_parked();
        let feed = opener.opened()[0].2.clone();
        feed.connected("dev");
        feed.data(b"one\ntwo\n");
        let session = workspace.read_with(cx, |w, _| w.session().unwrap().clone());
        run_until(cx, "two lines", |cx| displayed(cx, &session).len() == 3);

        cx.update_window(window, |_, window, cx| window.press(keys::CLEAR, cx))
            .unwrap();
        assert!(displayed(cx, &session).is_empty());

        cx.update_window(window, |_, window, cx| window.press(keys::DISCONNECT, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(feed.was_closed());
        session.read_with(cx, |view, _| assert!(view.state().is_disconnected()));
        workspace.read_with(cx, |w, cx| {
            assert_eq!(w.tab_count(), 1, "disconnecting keeps the tab");
            assert_eq!(w.tab_labels(cx)[0].state, TabState::Disconnected);
        });

        // Connect again: the same tab and the same view, which carries on.
        devices.update(cx, |devices, cx| {
            devices.connect_selected(cx);
        });
        run_until(cx, "the view to take the new session", |cx| {
            session.read_with(cx, |v, _| !v.state().is_disconnected())
        });
        assert_eq!(opener.opened().len(), 2);
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.tab_count(), 1);
            assert_eq!(w.session().unwrap(), &session, "the same view carries on");
        });
        let again = opener.opened()[1].2.clone();
        again.connected("dev");
        again.data(b"three\n");
        run_until(cx, "the new session's line", |cx| {
            displayed(cx, &session)
                .iter()
                .any(|line| line.text == "three")
        });

        cx.update_window(window, |_, window, cx| window.press(keys::CLOSE_TAB, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(opener.opened()[1].2.was_closed());
        workspace.read_with(cx, |w, _| {
            assert_eq!(w.tab_count(), 0);
            assert!(w.session().is_none());
        });
    }

    #[gpui_test]
    fn up_and_down_recall_history_in_the_compose_bar(cx: &mut TestAppContext) {
        let (window, workspace, _opener) = open_workspace(cx);
        let devices = workspace.read_with(cx, |w, _| w.devices().clone());
        devices.update(cx, |devices, cx| {
            devices.connect_selected(cx);
        });
        cx.run_until_parked();
        let compose =
            workspace.read_with(cx, |w, cx| w.session().unwrap().read(cx).compose().clone());

        cx.update_window(window, |_, window, cx| {
            compose.update(cx, |compose, cx| {
                for line in ["first", "second"] {
                    compose
                        .input()
                        .update(cx, |input, cx| input.set_value(line, window, cx));
                    compose.submit(window, cx);
                }
                compose
                    .input()
                    .update(cx, |input, cx| input.set_value("dra", window, cx));
            });
        })
        .unwrap();

        let text = |cx: &mut TestAppContext| compose.read_with(cx, |c, cx| c.text(cx));
        cx.update_window(window, |_, window, cx| window.press("up", cx))
            .unwrap();
        assert_eq!(text(cx), "second");
        cx.update_window(window, |_, window, cx| window.press("up", cx))
            .unwrap();
        assert_eq!(text(cx), "first");
        cx.update_window(window, |_, window, cx| window.press("down", cx))
            .unwrap();
        assert_eq!(text(cx), "second");
        cx.update_window(window, |_, window, cx| window.press("down", cx))
            .unwrap();
        assert_eq!(text(cx), "dra");
    }

    #[gpui_test]
    fn a_new_tab_waits_for_a_port_and_connect_fills_it(cx: &mut TestAppContext) {
        let (window, workspace, opener) = open_workspace(cx);
        let devices = workspace.read_with(cx, |w, _| w.devices().clone());
        devices.update(cx, |devices, cx| assert!(devices.connect_selected(cx)));
        cx.run_until_parked();
        assert_eq!(workspace.read_with(cx, |w, _| w.tab_count()), 1);

        cx.update_window(window, |_, window, cx| window.press(keys::NEW_TAB, cx))
            .unwrap();
        cx.run_until_parked();
        workspace.read_with(cx, |w, cx| {
            assert_eq!(w.tab_count(), 2);
            assert!(w.shows_tab_bar());
            assert_eq!(w.active_index(), Some(1));
            assert!(w.session().is_none());
            let labels = w.tab_labels(cx);
            assert_eq!(labels[1].title, "New tab");
            assert_eq!(labels[1].state, TabState::Empty);
            assert_eq!(w.window_title(), "Serialist");
        });
        // The Devices panel has the focus, so Enter connects its selection there.
        let devices_focused = cx
            .update_window(window, |_, window, cx| {
                devices
                    .read(cx)
                    .focus_handle(cx)
                    .contains_focused(window, cx)
            })
            .unwrap();
        assert!(devices_focused);
        // A second new tab is the same one.
        cx.update_window(window, |_, window, cx| window.press(keys::NEW_TAB, cx))
            .unwrap();
        assert_eq!(workspace.read_with(cx, |w, _| w.tab_count()), 2);

        cx.update_window(window, |_, window, cx| {
            devices.update(cx, |d, cx| {
                d.select_port(PortId::new("virtual:echo"), window, cx)
            });
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(opener.opened().len(), 2);
        workspace.read_with(cx, |w, cx| {
            assert_eq!(w.tab_count(), 2, "the new tab took the port");
            assert_eq!(w.active_index(), Some(1));
            assert_eq!(w.tab_labels(cx)[1].title, "virtual:echo");
            assert_eq!(
                devices.read(cx).connected(),
                [PortId::new("/dev/a"), PortId::new("virtual:echo")]
            );
        });

        // Connecting a port that has a tab goes there.
        cx.update_window(window, |_, window, cx| {
            devices.update(cx, |d, cx| d.select_port(PortId::new("/dev/a"), window, cx));
            devices.update(cx, |d, cx| d.connect_selected(cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(opener.opened().len(), 2, "nothing opened twice");
        workspace.read_with(cx, |w, _| assert_eq!(w.active_index(), Some(0)));

        // Tabs move.
        workspace.update(cx, |w, cx| w.move_tab(0, 1, cx));
        workspace.read_with(cx, |w, cx| {
            let titles: Vec<String> = w.tab_labels(cx).into_iter().map(|l| l.title).collect();
            assert_eq!(titles, ["virtual:echo", "/dev/a"]);
            assert_eq!(w.active_index(), Some(1), "the active tab moved with it");
        });
    }
}
