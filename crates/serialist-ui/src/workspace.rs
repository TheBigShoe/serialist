//! The window's root view: Devices panel on the left, the session in the center, the
//! status line along the bottom. The workspace owns sessions; panels only ask for them.

use std::sync::Arc;

use serialist_core::settings::ConfigPaths;
use serialist_core::{
    PortId, PortInfo, PortKind, PortSource, SerialConfig, StoreConfig, TransportError,
    TransportFactory,
};

use crate::actions::{self, Clear, Disconnect, Export, Pause, ToggleInline, ToggleRecord, context};
use crate::config::{self, Config};
use crate::devices_panel::{DevicesPanel, DevicesPanelEvent};
use crate::export::ExportFormat;
use crate::prelude::*;
use crate::session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
use crate::session_options::SessionOptions;
use crate::session_view::SessionView;
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
            _subscriptions: vec![devices_events, config_changes, appearance],
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

    pub fn connecting(&self) -> Option<&PortId> {
        self.connecting.as_ref()
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
                view.update(cx, |view, cx| view.focus_compose(window, cx));
                self.devices
                    .update(cx, |devices, cx| devices.set_connected(Some(port), cx));
                self.session = Some(view);
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
                                .child(self.devices.clone()),
                        )
                        .child(resizable_panel().child(center)),
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
