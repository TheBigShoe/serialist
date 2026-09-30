//! The window's root view: the Devices and Commands panels on the left, the session in
//! the center, the status line along the bottom. The workspace owns sessions; panels
//! only ask for them.
//!
//! Saved commands reach the session through here: the Commands panel's
//! [`CommandsPanelEvent::Send`] and every [`commands::Send`](crate::actions::commands::Send)
//! keybinding call [`Workspace::send_command`], which asks for the command's parameters
//! in a dialog when it has any, then hands it to the session view. The workspace also
//! owns the persisted compose history, which every session's compose bar shares.
//!
//! Scripts start here too, whatever starts them: the Script console's Run buttons and
//! REPL, a [`scripts::Run`](crate::actions::scripts::Run) key binding or menu entry, a
//! saved command whose payload is `{ "script": … }`, and the `on_connect` script of the
//! device profile that matches a port, run once the session is open (and the opener
//! has set DTR and RTS). Each reads its file from the scripts folder and queues it on
//! the session view, which owns the session's script thread; the console shows what
//! the runs report.

use std::path::Path;
use std::sync::Arc;

use serialist_core::settings::ConfigPaths;
use serialist_core::{
    CommandRef, ParamValues, PortId, PortInfo, PortKind, PortSource, SerialConfig, StoreConfig,
    TransportError, TransportFactory,
};
use serialist_script::ScriptSource;

use crate::actions::scripts::{ClearConsole, Run as RunScript, RunInline, Stop as StopScript};
use crate::actions::{self, Clear, Disconnect, Export, Pause, ToggleInline, ToggleRecord, context};
use crate::commands_panel::{CommandsPanel, CommandsPanelEvent};
use crate::config::{self, Config};
use crate::devices_panel::{DevicesPanel, DevicesPanelEvent};
use crate::export::ExportFormat;
use crate::history::PersistentHistory;
use crate::param_prompt::{ParamPrompt, ParamPromptEvent};
use crate::prelude::*;
use crate::script_bridge::{CommandsSnapshot, ConsoleKind, ConsoleLine, ScriptEnv, inline_source};
use crate::script_console::{ScriptConsole, ScriptConsoleEvent};
use crate::script_files::{display_name, resolve_script};
use crate::session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
use crate::session_options::SessionOptions;
use crate::session_view::{SessionView, SessionViewEvent};
use crate::status::{ConnectionState, Notice, StatusLine};

const STATUS_LINE_HEIGHT: Pixels = px(26.);

/// What the binary hands the UI: where ports come from and how to open them.
pub struct AppOptions {
    pub port_source: Arc<dyn PortSource>,
    pub transport_factory: Arc<dyn TransportFactory>,
    /// The `--baud` flag: every connect uses it, over device profiles and the
    /// `default_baud` setting.
    pub baud: Option<u32>,
    /// Port to select at startup, even before the source lists it.
    pub select_port: Option<PortId>,
    /// Open `select_port` right away (the `--port` flag).
    pub connect_on_start: bool,
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
    let bounds = Bounds::centered(None, size(px(1100.), px(720.)), cx);
    let window_options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Serialist".into()),
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

pub struct Workspace {
    devices: Entity<DevicesPanel>,
    commands: Entity<CommandsPanel>,
    /// The Script console in the right dock.
    console: Entity<ScriptConsole>,
    /// The saved commands scripts send by name, kept current on every reload.
    script_commands: CommandsSnapshot,
    /// The compose history every session shares, kept in `history.jsonl`.
    history: Entity<PersistentHistory>,
    /// The parameter dialog, while it is open.
    param_prompt: Option<Entity<ParamPrompt>>,
    session: Option<Entity<SessionView>>,
    /// The port the session is on, for its device profile when settings change.
    session_port: Option<PortInfo>,
    opener: Arc<dyn SessionOpener>,
    port_source: Arc<dyn PortSource>,
    /// The `--baud` flag.
    baud: Option<u32>,
    /// Store sizing over the settings' budget.
    store: Option<StoreConfig>,
    /// Port being opened on the background executor, for the placeholder and status line.
    connecting: Option<PortId>,
    focus_handle: FocusHandle,
    _connect_task: Option<Task<()>>,
    _session_observer: Option<Subscription>,
    _session_events: Option<Subscription>,
    _param_prompt_events: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Workspace {
    pub fn new(options: AppOptions, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let opener = Arc::new(CoreSessionOpener::new(options.transport_factory));
        let mut workspace =
            Self::with_opener(options.port_source, opener, options.baud, window, cx);
        workspace.store = options.store;
        if let Some(port) = options.select_port {
            workspace.devices.update(cx, |devices, cx| {
                devices.select_port(port.clone(), window, cx)
            });
            if options.connect_on_start {
                let serial = workspace.serial_for(&port, cx);
                workspace.connect(port, serial, window, cx);
            }
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
                DevicesPanelEvent::Connect { port, serial } => {
                    this.connect(port.clone(), serial.clone(), window, cx);
                }
            });
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
        let flush_history = cx.on_release(|this, cx| {
            this.history.update(cx, |history, cx| history.flush(cx));
        });
        // A reload reaches the session's display defaults and the status line.
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
            console,
            script_commands,
            history,
            param_prompt: None,
            session: None,
            session_port: None,
            opener,
            port_source,
            baud,
            store: None,
            connecting: None,
            focus_handle: cx.focus_handle(),
            _connect_task: None,
            _session_observer: None,
            _session_events: None,
            _param_prompt_events: None,
            _subscriptions: vec![
                devices_events,
                commands_events,
                console_events,
                flush_history,
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

    /// The configuration changed: hand the session its new defaults and repaint the
    /// status line.
    fn config_changed(&mut self, cx: &mut Context<Self>) {
        if let Some(config) = cx.try_global::<Config>() {
            self.script_commands.set(config.commands().clone());
        }
        if let (Some(session), Some(port)) = (&self.session, &self.session_port) {
            let options = self.session_options_for(port, cx);
            session.update(cx, |view, cx| view.apply_options(options, cx));
        }
        cx.notify();
    }

    pub fn devices(&self) -> &Entity<DevicesPanel> {
        &self.devices
    }

    pub fn session(&self) -> Option<&Entity<SessionView>> {
        self.session.as_ref()
    }

    pub fn commands(&self) -> &Entity<CommandsPanel> {
        &self.commands
    }

    pub fn console(&self) -> &Entity<ScriptConsole> {
        &self.console
    }

    pub fn history(&self) -> &Entity<PersistentHistory> {
        &self.history
    }

    /// The parameter dialog's form, while it is open.
    pub fn param_prompt(&self) -> Option<&Entity<ParamPrompt>> {
        self.param_prompt.as_ref()
    }

    /// Send the saved command `reference` names on the session. A command with
    /// parameters first asks for them in a dialog, prefilled with the values sent last
    /// in this session or the defaults.
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
        let Some(session) = self.session.clone() else {
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
        if let (Some(command), Some(session)) = (command, &self.session) {
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

    pub fn connecting(&self) -> Option<&PortId> {
        self.connecting.as_ref()
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

    /// Run the script at `path` (under the scripts folder, or absolute) on the session;
    /// `origin` says what started it. Returns whether it was queued; a script that
    /// cannot be read, or no session, is reported in the console.
    pub fn run_script_path(
        &mut self,
        path: &Path,
        origin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let paths = cx
            .try_global::<Config>()
            .map(|config| config.paths().clone())
            .unwrap_or_else(ConfigPaths::default_for_platform);
        let file = resolve_script(&paths, path);
        // Small, local and read once per run: read here, like the settings.
        let code = match std::fs::read_to_string(&file) {
            Ok(code) => code,
            Err(error) => {
                self.script_problem(
                    format!("Could not read the script {}: {error}", file.display()),
                    cx,
                );
                return false;
            }
        };
        let source = ScriptSource {
            name: display_name(&paths, &file),
            code,
            path: Some(file),
        };
        self.run_script(source, origin, window, cx)
    }

    /// Run a REPL line on the session.
    pub fn run_inline(&mut self, code: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.run_script(inline_source(code), "console", window, cx);
    }

    /// Queue `source` on the session's script thread.
    pub fn run_script(
        &mut self,
        source: ScriptSource,
        origin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(session) = self.session.clone() else {
            self.script_problem(
                format!("Not connected; connect a port to run {}", source.name),
                cx,
            );
            return false;
        };
        session.update(cx, |view, cx| view.run_script(source, origin, window, cx))
    }

    /// Stop the script running on the session.
    pub fn stop_script(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.stop_script(cx));
        }
    }

    fn script_problem(&mut self, message: String, cx: &mut Context<Self>) {
        tracing::warn!("{message}");
        self.console.update(cx, |console, cx| {
            console.push_lines([ConsoleLine::new(ConsoleKind::Error, message)], cx);
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

    /// Open `port` on the background executor, then swap in a new session view.
    pub fn connect(
        &mut self,
        port: PortId,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connecting = Some(port.clone());
        self.devices
            .update(cx, |devices, cx| devices.set_notice(None, cx));
        let opener = self.opener.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let (open_port, open_serial) = (port.clone(), serial.clone());
            let result = cx
                .background_spawn(async move { opener.open(&open_port, &open_serial) })
                .await;
            this.update_in(cx, |this, window, cx| {
                this.finish_connect(port, serial, result, window, cx);
            })
            .ok();
        });
        self._connect_task = Some(task);
        cx.notify();
    }

    fn finish_connect(
        &mut self,
        port: PortId,
        serial: SerialConfig,
        result: Result<Box<dyn SessionHandle>, TransportError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.connecting = None;
        match result {
            Ok(session) => {
                if let Some(previous) = self.session.take() {
                    previous.update(cx, |view, cx| view.disconnect(cx));
                }
                let info = self.port_info(&port, cx);
                let options = self.session_options_for(&info, cx);
                let on_connect = cx
                    .try_global::<Config>()
                    .and_then(|config| config.settings().on_connect_for(&info).cloned());
                self.session_port = Some(info);
                let view = cx
                    .new(|cx| SessionView::new(port.clone(), serial, session, options, window, cx));
                // The status line and the Devices panel's "open" dot follow the session.
                self._session_observer = Some(cx.observe(&view, |this, view, cx| {
                    let open = view.read(cx).open_port().cloned();
                    this.devices
                        .update(cx, |devices, cx| devices.set_connected(open, cx));
                    cx.notify();
                }));
                self._session_events = Some(cx.subscribe_in(
                    &view,
                    window,
                    |this, _, event, window, cx| match event {
                        SessionViewEvent::SaveAsCommand { text } => {
                            this.commands.update(cx, |panel, cx| {
                                panel.save_text_as_command(text, window, cx);
                            });
                        }
                        SessionViewEvent::Script(lines) => {
                            this.console.update(cx, |console, cx| {
                                console.push_lines(lines.iter().cloned(), cx);
                            });
                        }
                    },
                ));
                let history = self.history.clone();
                let env = self.script_env(cx);
                view.update(cx, |view, cx| {
                    view.set_history(history, cx);
                    view.attach_scripts(env);
                    view.focus_compose(window, cx);
                });
                let weak = view.downgrade();
                self.console
                    .update(cx, |console, cx| console.set_session(Some(weak), cx));
                self.devices
                    .update(cx, |devices, cx| devices.set_connected(Some(port), cx));
                self.session = Some(view);
                // The profile's script, now that the port is open and the opener has set
                // DTR and RTS (queued ahead of anything the script writes).
                if let Some(script) = on_connect {
                    self.run_script_path(&script, "on_connect", window, cx);
                }
            }
            Err(error) => {
                tracing::warn!(%port, %error, "could not open port");
                let notice = format!("Could not open {port}: {error}");
                self.devices.update(cx, |devices, cx| {
                    devices.set_notice(Some(notice.into()), cx)
                });
            }
        }
        cx.notify();
    }

    fn clear(&mut self, _: &Clear, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.clear(cx));
        }
    }

    fn disconnect(&mut self, _: &Disconnect, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.disconnect(cx));
        }
    }

    fn pause(&mut self, _: &Pause, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.toggle_pause(cx));
        }
    }

    fn export(&mut self, _: &Export, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.export(ExportFormat::Text, cx));
        }
    }

    fn toggle_record(&mut self, _: &ToggleRecord, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.toggle_record(cx));
        }
    }

    /// The session view handles the toggle when it holds the focus; this is for when
    /// another panel does.
    fn toggle_inline(&mut self, _: &ToggleInline, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            session.update(cx, |view, cx| view.toggle_mode(window, cx));
        }
    }

    fn render_center(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(session) = &self.session {
            return session.clone().into_any_element();
        }
        let theme = cx.theme();
        let message = match &self.connecting {
            Some(port) => format!("Opening {port}…"),
            None => "Select a port and press Enter, or click Connect.".to_owned(),
        };
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_1()
            .bg(theme.background)
            .text_color(theme.muted_foreground)
            .child(div().text_lg().child("No session"))
            .child(div().text_sm().child(SharedString::from(message)))
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

        let Some(session) = &self.session else {
            let label = match &self.connecting {
                Some(port) => format!("Opening {port}…"),
                None => "No session".to_owned(),
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
        let inline = session.mode() == crate::inline::Mode::Inline;
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

    /// The status line's text for the current session, as rendered.
    pub fn status_line(&self, cx: &App) -> Option<StatusLine> {
        self.session
            .as_ref()
            .map(|session| session.read(cx).status_line())
    }
}

impl Workspace {
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                                .size(px(320.))
                                .size_range(px(220.)..px(720.))
                                .child(self.console.clone()),
                        ),
                ),
            )
            .child(status_line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let status = workspace.status_line(cx).expect("a status line");
            assert_eq!(status.title, "/dev/a @ 115200 8N1");
            assert_eq!(status.retained, "2 lines, 8 B kept");
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

        let (clear, disconnect) = if cfg!(target_os = "macos") {
            ("cmd-k", "cmd-w")
        } else {
            ("ctrl-shift-k", "ctrl-shift-w")
        };
        cx.update_window(window, |_, window, cx| window.press(clear, cx))
            .unwrap();
        assert!(displayed(cx, &session).is_empty());

        cx.update_window(window, |_, window, cx| window.press(disconnect, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(feed.was_closed());
        session.read_with(cx, |view, _| assert!(view.state().is_disconnected()));
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
}
