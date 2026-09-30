//! The window's root view: Devices panel on the left, the session in the center, the
//! status line along the bottom. The workspace owns sessions; panels only ask for them.

use std::sync::Arc;

use serialist_core::{PortId, PortSource, SerialConfig, TransportError, TransportFactory};

use crate::actions::{self, Clear, Disconnect, Quit, context};
use crate::devices_panel::{DevicesPanel, DevicesPanelEvent};
use crate::prelude::*;
use crate::session_handle::{CoreSessionOpener, SessionHandle, SessionOpener};
use crate::session_view::{ConnectionState, SessionView, StatusLine};

const STATUS_LINE_HEIGHT: Pixels = px(26.);

/// What the binary hands the UI: where ports come from and how to open them.
pub struct AppOptions {
    pub port_source: Arc<dyn PortSource>,
    pub transport_factory: Arc<dyn TransportFactory>,
    /// Starting line settings; the baud field is prefilled from it.
    pub serial: SerialConfig,
    /// Port to select at startup, even before the source lists it.
    pub select_port: Option<PortId>,
    /// Open `select_port` right away (the `--port` flag).
    pub connect_on_start: bool,
}

/// Global setup: gpui-kit's components and theme, the dark default, key bindings and
/// the app-level actions. Call once, before opening a window.
pub fn init(cx: &mut App) {
    kit_init(cx);
    Theme::change(ThemeMode::Dark, None, cx);
    actions::bind_keys(cx);
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.set_menus([Menu::new("Serialist").items([MenuItem::action("Quit Serialist", Quit)])]);
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
    opener: Arc<dyn SessionOpener>,
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
        let mut workspace = Self::with_opener(
            options.port_source,
            opener,
            options.serial.clone(),
            window,
            cx,
        );
        if let Some(port) = options.select_port {
            workspace
                .devices
                .update(cx, |devices, cx| devices.select_port(port.clone(), cx));
            if options.connect_on_start {
                workspace.connect(port, options.serial, window, cx);
            }
        }
        workspace
    }

    /// A workspace with a custom way of opening sessions; tests pass fakes here.
    pub fn with_opener(
        port_source: Arc<dyn PortSource>,
        opener: Arc<dyn SessionOpener>,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let devices = cx.new(|cx| DevicesPanel::new(port_source, serial, window, cx));
        let devices_events =
            cx.subscribe_in(&devices, window, |this, _, event, window, cx| match event {
                DevicesPanelEvent::Connect { port, serial } => {
                    this.connect(port.clone(), serial.clone(), window, cx);
                }
            });
        // Start with the port list focused so arrows and Enter work immediately.
        let devices_focus = devices.focus_handle(cx);
        window.focus(&devices_focus, cx);

        Self {
            devices,
            session: None,
            opener,
            connecting: None,
            focus_handle: cx.focus_handle(),
            _connect_task: None,
            _session_observer: None,
            _subscriptions: vec![devices_events],
        }
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
                let view = cx.new(|cx| SessionView::new(port.clone(), serial, session, window, cx));
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

        let model = session.read(cx).model();
        let state_color = match model.state {
            ConnectionState::Connecting => theme.warning,
            ConnectionState::Connected => theme.success,
            ConnectionState::Disconnected { error: None } => theme.muted_foreground,
            ConnectionState::Disconnected { error: Some(_) } => theme.danger,
        };
        let status = model.status_line();
        line.child(
            h_flex()
                .gap_1p5()
                .child(dot(state_color))
                .child(status.state),
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
    }

    /// The status line's text for the current session, as rendered.
    pub fn status_line(&self, cx: &App) -> Option<StatusLine> {
        self.session
            .as_ref()
            .map(|session| session.read(cx).model().status_line())
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let center = self.render_center(cx);
        let status_line = self.render_status_line(cx);

        v_flex()
            .id("workspace")
            .key_context(context::WORKSPACE)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::clear))
            .on_action(cx.listener(Self::disconnect))
            .size_full()
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
    use crate::drain::FRAME;
    use crate::test_support::{FakeOpener, FakePortSource, open_test_window, port};

    fn open_workspace(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Workspace>, Arc<FakeOpener>) {
        let source = FakePortSource::new([port("/dev/a"), port("virtual:echo")]);
        let opener = Arc::new(FakeOpener::default());
        let for_window = opener.clone();
        let (window, workspace) = open_test_window(cx, move |window, cx| {
            Workspace::with_opener(source, for_window, SerialConfig::default(), window, cx)
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
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();

        workspace.read_with(cx, |workspace, cx| {
            let session = workspace.session().expect("session view").read(cx);
            assert_eq!(session.model().state, ConnectionState::Connected);
            assert_eq!(
                session.model().buffer.rows().last().unwrap().text,
                "boot ok"
            );
            assert_eq!(workspace.connecting(), None);
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
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();

        let (clear, disconnect) = if cfg!(target_os = "macos") {
            ("cmd-k", "cmd-w")
        } else {
            ("ctrl-shift-k", "ctrl-shift-w")
        };
        cx.update_window(window, |_, window, cx| window.press(clear, cx))
            .unwrap();
        let session = workspace.read_with(cx, |w, _| w.session().unwrap().clone());
        session.read_with(cx, |view, _| assert!(view.model().buffer.is_empty()));

        cx.update_window(window, |_, window, cx| window.press(disconnect, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(feed.was_closed());
        session.read_with(cx, |view, _| assert!(view.model().state.is_disconnected()));
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
