//! The window's root view: the Devices and Commands panels on the left, a tab per open
//! port in the center, the Decoded panel and the Script console on the right, the
//! status line along the bottom.
//!
//! # Tabs
//!
//! The workspace owns the tabs, and each tab owns at most one session view: its
//! session, ingest thread and store, codec slot, script host, pause, recording and
//! export state all live in the view (see [`session_view`](crate::session_view)), so a
//! tab is a port and a view, nothing more. A tab outlives the views that come and go in
//! it: connecting again after a disconnect puts a new view in the same tab, which keeps
//! its place, its [`TabId`] and its Script console output. A tab may hold no view: a new
//! tab (`tabs::NewTab`) before a port is picked, a port being opened, or a restored port
//! whose device is not plugged in.
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
//! The tab bar shows once there are two tabs or more; with one, the status line and
//! the window title already name the port. Tabs switch with `tabs::ActivateTab1` to `9`
//! and `tabs::NextTab`/`PreviousTab`, reorder by dragging, and close with
//! `tabs::CloseTab` or their × button, which disconnects the port and stops its script
//! and recording, asking first while either runs.
//!
//! # Session restore
//!
//! When the workspace closes (the window closes, or the app quits) it writes the tabs'
//! ports and settings to `state.json` in the config directory (see
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
    CommandRef, ParamValues, PortId, PortInfo, PortKind, PortSource, SerialConfig, StoreConfig,
    TransportError, TransportFactory,
};
use serialist_script::ScriptSource;

use crate::actions::scripts::{ClearConsole, Run as RunScript, RunInline, Stop as StopScript};
use crate::actions::tabs::{
    ActivateTab1, ActivateTab2, ActivateTab3, ActivateTab4, ActivateTab5, ActivateTab6,
    ActivateTab7, ActivateTab8, ActivateTab9, CloseTab, NewTab, NextTab, PreviousTab,
};
use crate::actions::{self, Clear, Disconnect, Export, Pause, ToggleInline, ToggleRecord, context};
use crate::commands_panel::{CommandsPanel, CommandsPanelEvent};
use crate::config::{self, Config};
use crate::decoded_panel::DecodedPanel;
use crate::devices_panel::{DevicesPanel, DevicesPanelEvent};
use crate::export::ExportFormat;
use crate::history::PersistentHistory;
use crate::inline::Mode;
use crate::param_prompt::{ParamPrompt, ParamPromptEvent};
use crate::port_settings::PortSettings;
use crate::prelude::*;
use crate::script_bridge::{CommandsSnapshot, ConsoleKind, ConsoleLine, ScriptEnv, inline_source};
use crate::script_console::{ScriptConsole, ScriptConsoleEvent};
use crate::script_files::{display_name, resolve_script};
use crate::session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
use crate::session_options::SessionOptions;
use crate::session_state::{STATE_VERSION, SavedTab, SessionState, state_path};
use crate::session_view::{SessionView, SessionViewEvent};
use crate::status::{ConnectionState, Notice, StatusLine};
use crate::tabs::{TabId, TabLabel, TabState, TabStatus};

const STATUS_LINE_HEIGHT: Pixels = px(26.);

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
            _connect_task: None,
            _observer: None,
            _events: None,
        }
    }

    /// A new tab with no port picked.
    fn is_blank(&self) -> bool {
        self.port.is_none() && self.view.is_none() && !self.connecting
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
    next_tab: u64,
    /// The tab a close confirmation is open for.
    pending_close: Option<TabId>,
    /// The window title last set.
    window_title: String,
    opener: Arc<dyn SessionOpener>,
    port_source: Arc<dyn PortSource>,
    /// The `--baud` flag.
    baud: Option<u32>,
    /// Store sizing over the settings' budget.
    store: Option<StoreConfig>,
    focus_handle: FocusHandle,
    _param_prompt_events: Option<Subscription>,
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
        let select = options
            .select_port
            .or_else(|| options.open_ports.first().cloned());
        if let Some(port) = select {
            workspace.devices.update(cx, |devices, cx| {
                devices.select_port(port, window, cx);
            });
        }
        if !options.open_ports.is_empty() {
            workspace.open_ports(options.open_ports, window, cx);
        } else if let Some(state) = workspace.saved_state(cx) {
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
            next_tab: 0,
            pending_close: None,
            window_title: APP_TITLE.to_owned(),
            opener,
            port_source,
            baud,
            store: None,
            focus_handle: cx.focus_handle(),
            _param_prompt_events: None,
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

    /// The configuration changed: hand every session its new defaults and repaint the
    /// status line.
    fn config_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(config) = cx.try_global::<Config>() {
            self.script_commands.set(config.commands().clone());
        }
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

    /// Whether the tab bar shows: with two tabs or more.
    pub fn shows_tab_bar(&self) -> bool {
        self.tabs.len() > 1
    }

    /// What a tab is called: its device's name in the Devices panel, else its port id.
    fn tab_title(&self, tab: &SessionTab, cx: &App) -> String {
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
            })
            .collect()
    }

    /// `<port> — Serialist` for the active tab's port, else `Serialist`.
    pub fn window_title(&self) -> String {
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
        tracing::debug!(tab = ?self.active, "tab shown");
        cx.notify();
    }

    /// Focus where typing goes in the active tab: its session's compose bar (or its
    /// terminal in inline mode), or the Devices panel for a tab with no session.
    fn focus_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Close Tab")
                        .show_cancel(true),
                )
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
                    view.set_codec(Some(codec), cx);
                }
                view.set_mode(restore.mode, window, cx);
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
                },
            });
        }
        SessionState {
            version: STATE_VERSION,
            active,
            tabs,
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
                .button_props(
                    DialogButtonProps::default()
                        .ok_text("Send")
                        .show_cancel(true),
                )
                // Confirming emits the values; the subscription sends and closes.
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
        let dot = div()
            .id(("tab-dot", id.0 as usize))
            .flex_none()
            .size_2()
            .rounded_full()
            .map(|dot| {
                if hollow {
                    dot.border_1().border_color(color)
                } else {
                    dot.bg(color)
                }
            })
            .tooltip(move |window, cx| Tooltip::new(state.clone()).build(window, cx));
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
                    .children(label.unseen.map(|unseen| {
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(unseen))
                    }))
                    .child(
                        Button::new(("close-tab", id.0 as usize))
                            .label("\u{00d7}")
                            .tooltip("Close tab")
                            .xsmall()
                            .ghost()
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
                    Button::new("new-tab")
                        .label("+")
                        .tooltip("New tab")
                        .xsmall()
                        .ghost()
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
                "No session".to_owned(),
                format!(
                    "Opening {}\u{2026}",
                    tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
                ),
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
                "Select a port and press Enter, or click Connect.".to_owned(),
                None,
            ),
        };
        let theme = cx.theme();
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(theme.background)
            .text_color(theme.muted_foreground)
            .child(div().text_lg().child(SharedString::from(title)))
            .child(div().text_sm().child(SharedString::from(message)))
            .when_some(reconnect, |this, id| {
                this.child(
                    div().pt_2().child(
                        Button::new("reconnect-tab")
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
        let body = match self.session() {
            Some(session) => session.clone().into_any_element(),
            None => self.render_placeholder(cx),
        };
        v_flex()
            .size_full()
            .min_w_0()
            .when(self.shows_tab_bar(), |this| {
                this.child(self.render_tab_bar(cx))
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }

    fn render_status_line(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let line = h_flex()
            .flex_none()
            .h(STATUS_LINE_HEIGHT)
            .px_3()
            .gap_4()
            .border_t_1()
            .border_color(theme.status_bar_border)
            .bg(theme.status_bar)
            .text_xs()
            .text_color(theme.muted_foreground);

        let dot = |color: Hsla| div().flex_none().size_2().rounded_full().bg(color);

        let Some(session) = self.session() else {
            let label = match self.active_tab() {
                Some(tab) if tab.connecting => format!(
                    "Opening {}\u{2026}",
                    tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
                ),
                Some(tab) if tab.port.is_some() => format!(
                    "{}: not connected",
                    tab.port.as_ref().map(PortId::as_str).unwrap_or_default()
                ),
                _ => "No session".to_owned(),
            };
            return line.child(
                h_flex()
                    .gap_1p5()
                    .child(dot(theme.muted_foreground))
                    .child(SharedString::from(label)),
            );
        };

        let session = session.read(cx);
        let state_color = match session.state() {
            ConnectionState::Connected => theme.success,
            ConnectionState::Disconnected { error: None } => theme.muted_foreground,
            ConnectionState::Disconnected { error: Some(_) } => theme.danger,
        };
        let status = session.status_line();
        let inline = session.mode() == Mode::Inline;
        line.child(
            h_flex()
                .gap_1p5()
                .child(dot(state_color))
                .child(status.state),
        )
        .child(
            div()
                .id("status-mode")
                .flex_none()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(if inline { theme.warning } else { theme.border })
                .text_color(if inline {
                    theme.warning
                } else {
                    theme.muted_foreground
                })
                .child(status.mode),
        )
        .child(
            div()
                .font_family(theme.mono_font_family.clone())
                .truncate()
                .child(SharedString::from(status.title)),
        )
        .children(status.settings.map(SharedString::from))
        .child(SharedString::from(status.rx))
        .child(SharedString::from(status.tx))
        .child(SharedString::from(status.retained))
        .children(status.evicted.map(SharedString::from))
        .children(status.paused.map(|paused| {
            div()
                .text_color(theme.warning)
                .child(SharedString::from(paused))
        }))
        .children(status.paste.map(|paste| {
            div()
                .text_color(theme.info)
                .child(SharedString::from(paste))
        }))
        .children(status.script.map(|script| {
            div()
                .id("status-script")
                .flex_none()
                .text_color(theme.info)
                .child(SharedString::from(script))
        }))
        .children(status.codec.map(|codec| {
            div()
                .id("status-codec")
                .flex_none()
                .text_color(theme.info)
                .child(SharedString::from(codec))
        }))
        .children(status.recording.map(|recording| {
            h_flex()
                .gap_1p5()
                .text_color(theme.danger)
                .child(dot(theme.danger))
                .child(SharedString::from(recording))
        }))
        .children(status.notice.map(|notice| {
            div()
                .truncate()
                .text_color(if notice.is_error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(SharedString::from(notice.text))
        }))
    }

    /// The status line's text for the active tab's session, as rendered.
    pub fn status_line(&self, cx: &App) -> Option<StatusLine> {
        self.session().map(|session| session.read(cx).status_line())
    }

    /// What the status line says about the configuration: a file that did not load, a
    /// theme that was not found, an unknown key.
    pub fn config_notice(&self, cx: &App) -> Option<Notice> {
        cx.try_global::<Config>().and_then(Config::notice)
    }

    fn render_config_notice(&self, cx: &Context<Self>) -> Option<Div> {
        let notice = self.config_notice(cx)?;
        let theme = cx.theme();
        let color = if notice.is_error {
            theme.danger
        } else {
            theme.warning
        };
        Some(
            div()
                .ml_auto()
                .min_w_0()
                .truncate()
                .text_color(color)
                .child(SharedString::from(notice.text)),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let title = self.window_title();
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        // Panels, the status line and inputs inherit the UI font's weight and OpenType
        // features from here; gpui-kit's theme carries its family and size.
        let ui_font = cx
            .try_global::<Config>()
            .map(|config| config.ui_font().font.clone());
        let center = self.render_center(cx);
        let status_line = self
            .render_status_line(cx)
            .children(self.render_config_notice(cx));

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
            .on_action(cx.listener(Self::send_command_action))
            .on_action(cx.listener(Self::run_script_action))
            .on_action(cx.listener(Self::run_inline_action))
            .on_action(cx.listener(Self::stop_script_action))
            .on_action(cx.listener(Self::clear_console_action))
            .on_action(cx.listener(Self::new_tab_action))
            .on_action(cx.listener(Self::close_tab_action))
            .on_action(cx.listener(Self::next_tab_action))
            .on_action(cx.listener(Self::previous_tab_action))
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
            .size_full()
            .when_some(ui_font, |this, font| this.font(font))
            .bg(background)
            .text_color(foreground)
            .child(
                div().flex_1().min_h_0().w_full().child(
                    h_resizable("workspace-columns")
                        .child(
                            resizable_panel()
                                .size(px(280.))
                                .size_range(px(200.)..px(520.))
                                .child(
                                    v_resizable("left-dock")
                                        .child(
                                            resizable_panel()
                                                .size(px(300.))
                                                .size_range(px(120.)..px(900.))
                                                .child(self.devices.clone()),
                                        )
                                        .child(resizable_panel().child(self.commands.clone())),
                                ),
                        )
                        .child(resizable_panel().child(center))
                        .child(
                            resizable_panel()
                                .size(px(400.))
                                .size_range(px(220.)..px(960.))
                                .child(
                                    v_resizable("right-dock")
                                        .child(
                                            resizable_panel()
                                                .size(px(380.))
                                                .size_range(px(120.)..px(1200.))
                                                .child(self.decoded.clone()),
                                        )
                                        .child(resizable_panel().child(self.console.clone())),
                                ),
                        ),
                ),
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
