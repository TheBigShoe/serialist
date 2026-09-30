//! The session view: scrollback, follow-tail and the compose bar for one open port.
//!
//! Milestone 0 renders the scrollback as a `uniform_list` of text rows. Milestone 1
//! replaces it with the custom terminal element over the page store; the batching and
//! follow-tail behaviour here carry over.

use std::ops::Range;
use std::sync::Arc;

use serialist_core::{PortId, SerialConfig, SessionEvent, SessionStats, TransportError};

use crate::actions::{JumpToBottom, context};
use crate::compose::{ComposeBar, ComposeEvent};
use crate::drain::drain_into;
use crate::line_buffer::{LineBuffer, LineKind, LineSplitter, RxText};
use crate::prelude::*;
use crate::session_handle::SessionHandle;

const ROW_HEIGHT: Pixels = px(18.);

/// A session event after the drain worker has turned received bytes into text; this,
/// not [`SessionEvent`], is what reaches the main thread.
#[derive(Debug)]
pub enum SessionUpdate {
    Connected {
        description: String,
    },
    /// Every `Data` chunk between two other events, split into lines.
    Received(RxText),
    Disconnected {
        error: Option<TransportError>,
    },
    WriteFailed(TransportError),
}

/// The drain worker's `prepare` step: runs of `Data` chunks go through the splitter
/// together, and every other event keeps its place in the order.
pub(crate) fn prepare_updates(
    splitter: &mut LineSplitter,
    events: Vec<SessionEvent>,
) -> Vec<SessionUpdate> {
    fn flush(
        splitter: &mut LineSplitter,
        chunks: &mut Vec<Arc<[u8]>>,
        out: &mut Vec<SessionUpdate>,
    ) {
        if !chunks.is_empty() {
            out.push(SessionUpdate::Received(
                splitter.split(chunks.iter().map(|chunk| &chunk[..])),
            ));
            chunks.clear();
        }
    }

    let mut out = Vec::new();
    let mut chunks = Vec::new();
    for event in events {
        let update = match event {
            SessionEvent::Data { bytes, .. } => {
                chunks.push(bytes);
                continue;
            }
            SessionEvent::Connected { description } => SessionUpdate::Connected { description },
            SessionEvent::Disconnected { error } => SessionUpdate::Disconnected { error },
            SessionEvent::WriteFailed(error) => SessionUpdate::WriteFailed(error),
        };
        flush(splitter, &mut chunks, &mut out);
        out.push(update);
    }
    flush(splitter, &mut chunks, &mut out);
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// Opened, waiting for the session's `Connected` event.
    Connecting,
    Connected,
    Disconnected {
        /// `None` after an orderly close.
        error: Option<String>,
    },
}

impl ConnectionState {
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Connecting => "Connecting",
            ConnectionState::Connected => "Connected",
            ConnectionState::Disconnected { error: None } => "Disconnected",
            ConnectionState::Disconnected { error: Some(_) } => "Connection lost",
        }
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, ConnectionState::Disconnected { .. })
    }
}

/// Everything the view shows, without GPUI, so event handling tests as plain Rust.
#[derive(Clone, Debug)]
pub struct SessionModel {
    pub port: PortId,
    pub serial: SerialConfig,
    /// The transport's own label, known once `Connected` arrives.
    pub description: Option<String>,
    pub state: ConnectionState,
    pub stats: SessionStats,
    pub buffer: LineBuffer,
}

impl SessionModel {
    pub fn new(port: PortId, serial: SerialConfig) -> Self {
        Self {
            port,
            serial,
            description: None,
            state: ConnectionState::Connecting,
            stats: SessionStats::default(),
            buffer: LineBuffer::default(),
        }
    }

    /// What the status line names the session by.
    pub fn title(&self) -> String {
        self.description
            .clone()
            .unwrap_or_else(|| format!("{} @ {}", self.port, self.serial.summary()))
    }

    /// Apply one update. Returns whether anything visible changed.
    pub fn apply(&mut self, update: SessionUpdate) -> bool {
        match update {
            SessionUpdate::Connected { description } => {
                self.buffer
                    .push_line(LineKind::Info, &format!("Connected to {description}"));
                self.description = Some(description);
                self.state = ConnectionState::Connected;
            }
            SessionUpdate::Received(rx) => return self.buffer.apply_rx(rx),
            SessionUpdate::Disconnected { error: None } if self.state.is_disconnected() => {
                // `close()` reports an orderly disconnect after we already showed one
                // (local disconnect, or a lost device we then closed).
                return false;
            }
            SessionUpdate::Disconnected { error } => {
                let error = error.map(|e| e.to_string());
                match &error {
                    Some(error) => self
                        .buffer
                        .push_line(LineKind::Error, &format!("Disconnected: {error}")),
                    None => self.buffer.push_line(LineKind::Info, "Disconnected"),
                }
                self.state = ConnectionState::Disconnected { error };
            }
            SessionUpdate::WriteFailed(error) => {
                self.buffer
                    .push_line(LineKind::Error, &format!("Write failed: {error}"));
            }
        }
        true
    }

    /// The user asked to disconnect. Returns false if already disconnected.
    pub fn disconnect_locally(&mut self) -> bool {
        if self.state.is_disconnected() {
            return false;
        }
        self.buffer.push_line(LineKind::Info, "Disconnected");
        self.state = ConnectionState::Disconnected { error: None };
        true
    }
}

/// Whether to keep following new output after a wheel event. Scrolling up always
/// detaches; scrolling down re-attaches once the list sits at its end; a list too short
/// to scroll always follows.
pub(crate) fn follow_after_scroll(following: bool, delta_y: f32, at_end: Option<bool>) -> bool {
    match at_end {
        None => true,
        Some(_) if delta_y > 0.0 => false,
        Some(true) if delta_y < 0.0 => true,
        Some(_) => following,
    }
}

/// Byte counts for the status line: exact below 1 KiB, one decimal above.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

pub struct SessionView {
    model: SessionModel,
    /// `None` once disconnected; the scrollback stays readable.
    session: Option<Box<dyn SessionHandle>>,
    compose: Entity<ComposeBar>,
    scroll: UniformListScrollHandle,
    follow_tail: bool,
    focus_handle: FocusHandle,
    _drain: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SessionView {
    pub fn new(
        port: PortId,
        serial: SerialConfig,
        session: Box<dyn SessionHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let compose = cx.new(|cx| ComposeBar::new(window, cx));
        let compose_events =
            cx.subscribe_in(&compose, window, |this, _, event, _, cx| match event {
                ComposeEvent::Submit { text, bytes } => this.send(text, bytes.clone(), cx),
            });
        // Joining the session's threads can take a read timeout's worth of time, so a
        // dropped view hands the session to the background executor to close.
        let release = cx.on_release(|this, cx| {
            if let Some(session) = this.session.take() {
                cx.background_spawn(async move { session.close() }).detach();
            }
        });
        let drain = drain_into(
            session.events(),
            LineSplitter::default(),
            prepare_updates,
            cx,
            |this: &mut Self, updates, cx| this.apply_updates(updates, cx),
        );

        Self {
            model: SessionModel::new(port, serial),
            session: Some(session),
            compose,
            scroll: UniformListScrollHandle::new(),
            follow_tail: true,
            focus_handle: cx.focus_handle(),
            _drain: drain,
            _subscriptions: vec![compose_events, release],
        }
    }

    pub fn model(&self) -> &SessionModel {
        &self.model
    }

    pub fn compose(&self) -> &Entity<ComposeBar> {
        &self.compose
    }

    pub fn is_following_tail(&self) -> bool {
        self.follow_tail
    }

    /// The port this view holds open, if it is still open.
    pub fn open_port(&self) -> Option<&PortId> {
        (!self.model.state.is_disconnected()).then_some(&self.model.port)
    }

    pub fn focus_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compose
            .update(cx, |compose, cx| compose.focus(window, cx));
    }

    /// Apply one drained batch: however many events it held, at most one notify.
    fn apply_updates(&mut self, updates: Vec<SessionUpdate>, cx: &mut Context<Self>) {
        let mut changed = false;
        let mut lost = false;
        for update in updates {
            lost |= matches!(update, SessionUpdate::Disconnected { error: Some(_) });
            changed |= self.model.apply(update);
        }
        changed |= self.refresh_stats();
        if lost {
            self.close_session(cx);
        }
        if changed {
            self.after_append();
            cx.notify();
        }
    }

    /// Counters also move without events (TX completes on the writer thread), which is
    /// why idle wakes of the drain loop poll them too.
    fn refresh_stats(&mut self) -> bool {
        let Some(session) = &self.session else {
            return false;
        };
        let stats = session.stats();
        if stats == self.model.stats {
            return false;
        }
        self.model.stats = stats;
        true
    }

    fn after_append(&self) {
        if self.follow_tail {
            self.scroll.scroll_to_bottom();
        }
    }

    /// Write a submitted line and echo it into the scrollback.
    pub fn send(&mut self, text: &str, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let written = match &self.session {
            Some(session) if !self.model.state.is_disconnected() => session.write(bytes).is_ok(),
            _ => false,
        };
        if written {
            self.model.buffer.push_line(LineKind::Tx, text);
        } else {
            self.model
                .buffer
                .push_line(LineKind::Error, &format!("Not connected; not sent: {text}"));
        }
        self.after_append();
        cx.notify();
    }

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.model.buffer.clear();
        self.follow_tail = true;
        cx.notify();
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if self.model.disconnect_locally() {
            self.after_append();
        }
        self.close_session(cx);
        cx.notify();
    }

    fn close_session(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            self.model.stats = session.stats();
            cx.background_spawn(async move { session.close() }).detach();
        }
    }

    pub fn jump_to_bottom(&mut self, cx: &mut Context<Self>) {
        self.follow_tail = true;
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn jump_to_bottom_action(&mut self, _: &JumpToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.jump_to_bottom(cx);
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta_y = f32::from(event.delta.pixel_delta(window.line_height()).y);
        let follow =
            follow_after_scroll(self.follow_tail, delta_y, self.scroll.is_scrolled_to_end());
        if follow != self.follow_tail {
            self.follow_tail = follow;
            cx.notify();
        }
    }

    fn render_rows(&mut self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<Div> {
        let theme = cx.theme();
        range
            .filter_map(|ix| self.model.buffer.row(ix))
            .map(|row| {
                let color = match row.kind {
                    LineKind::Rx => theme.foreground,
                    LineKind::Tx => theme.info,
                    LineKind::Info => theme.muted_foreground,
                    LineKind::Error => theme.danger,
                };
                div()
                    .h(ROW_HEIGHT)
                    .px_3()
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .text_color(color)
                    .child(SharedString::from(row.text.to_owned()))
            })
            .collect()
    }
}

impl Render for SessionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let scrollback = uniform_list(
            "scrollback",
            self.model.buffer.len(),
            cx.processor(|this, range: Range<usize>, _window, cx| this.render_rows(range, cx)),
        )
        .track_scroll(&self.scroll)
        .size_full()
        .py_1()
        .font_family(theme.mono_font_family.clone())
        .text_size(theme.mono_font_size)
        .line_height(ROW_HEIGHT);

        v_flex()
            .id("session-view")
            .key_context(context::SESSION_VIEW)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::jump_to_bottom_action))
            .size_full()
            .bg(theme.background)
            .child(
                div()
                    .id("scrollback-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
                    .child(scrollback)
                    .when(!self.follow_tail, |area| {
                        area.child(
                            div().absolute().bottom_3().right_4().child(
                                Button::new("jump-to-bottom")
                                    .label("Jump to bottom")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.jump_to_bottom(cx);
                                    })),
                            ),
                        )
                    }),
            )
            .child(self.compose.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Instant;

    use super::*;
    use crate::drain::FRAME;
    use crate::test_support::{fake_session, open_test_window};

    fn rows(model: &SessionModel) -> Vec<(LineKind, String)> {
        model
            .buffer
            .rows()
            .map(|r| (r.kind, r.text.to_owned()))
            .collect()
    }

    fn data(bytes: &[u8]) -> SessionEvent {
        SessionEvent::Data {
            bytes: Arc::from(bytes),
            received_at: Instant::now(),
        }
    }

    /// Run events through the worker step and the model, as the drain loop does.
    fn apply(
        model: &mut SessionModel,
        splitter: &mut LineSplitter,
        events: Vec<SessionEvent>,
    ) -> bool {
        prepare_updates(splitter, events)
            .into_iter()
            .fold(false, |changed, update| model.apply(update) | changed)
    }

    #[test]
    fn prepare_merges_data_runs_and_keeps_event_order() {
        let mut splitter = LineSplitter::default();
        let updates = prepare_updates(
            &mut splitter,
            vec![
                SessionEvent::Connected {
                    description: "d".into(),
                },
                data(b"a\nb"),
                data(b"c\n"),
                SessionEvent::WriteFailed(TransportError::Disconnected),
                data(b"tail"),
            ],
        );
        let summary: Vec<String> = updates
            .iter()
            .map(|u| match u {
                SessionUpdate::Connected { .. } => "connected".to_owned(),
                SessionUpdate::Received(rx) => format!("rx {:?} {:?}", rx.lines, rx.partial),
                SessionUpdate::Disconnected { .. } => "disconnected".to_owned(),
                SessionUpdate::WriteFailed(_) => "write failed".to_owned(),
            })
            .collect();
        assert_eq!(
            summary,
            [
                "connected",
                r#"rx ["a", "bc"] """#,
                "write failed",
                r#"rx [] "tail""#,
            ]
        );
    }

    #[test]
    fn model_tracks_connection_state() {
        let mut splitter = LineSplitter::default();
        let mut model = SessionModel::new(PortId::new("virtual:echo"), SerialConfig::default());
        assert_eq!(model.title(), "virtual:echo @ 115200 8N1");
        assert!(apply(
            &mut model,
            &mut splitter,
            vec![SessionEvent::Connected {
                description: "virtual:echo".into()
            }]
        ));
        assert_eq!(model.state, ConnectionState::Connected);
        assert_eq!(model.title(), "virtual:echo");
        assert!(apply(&mut model, &mut splitter, vec![data(b"hi\r\n")]));
        assert!(
            !apply(&mut model, &mut splitter, vec![data(b"")]),
            "empty chunks change nothing"
        );
        assert!(apply(
            &mut model,
            &mut splitter,
            vec![SessionEvent::Disconnected {
                error: Some(TransportError::Disconnected)
            }]
        ));
        assert_eq!(
            model.state,
            ConnectionState::Disconnected {
                error: Some("device disconnected".into())
            }
        );
        assert!(
            !apply(
                &mut model,
                &mut splitter,
                vec![SessionEvent::Disconnected { error: None }]
            ),
            "the close that follows a lost device is not shown twice"
        );
        assert_eq!(
            rows(&model),
            [
                (LineKind::Info, "Connected to virtual:echo".into()),
                (LineKind::Rx, "hi".into()),
                (LineKind::Error, "Disconnected: device disconnected".into()),
            ]
        );
    }

    #[test]
    fn local_disconnect_is_shown_once() {
        let mut model = SessionModel::new(PortId::new("p"), SerialConfig::default());
        assert!(model.disconnect_locally());
        assert!(!model.disconnect_locally());
        assert!(!model.apply(SessionUpdate::Disconnected { error: None }));
        assert_eq!(rows(&model), [(LineKind::Info, "Disconnected".into())]);
    }

    #[test]
    fn follow_tail_rules() {
        assert!(
            !follow_after_scroll(true, 12.0, Some(false)),
            "scroll up detaches"
        );
        assert!(
            !follow_after_scroll(true, 12.0, Some(true)),
            "even from the end"
        );
        assert!(
            follow_after_scroll(false, -12.0, Some(true)),
            "back at the end"
        );
        assert!(
            !follow_after_scroll(false, -12.0, Some(false)),
            "not there yet"
        );
        assert!(follow_after_scroll(false, 12.0, None), "nothing to scroll");
        assert!(
            follow_after_scroll(true, 0.0, Some(false)),
            "sideways keeps state"
        );
    }

    #[test]
    fn bytes_are_formatted_for_the_status_line() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    fn open_session_view(
        cx: &mut TestAppContext,
    ) -> (
        AnyWindowHandle,
        Entity<SessionView>,
        crate::test_support::FakeFeed,
    ) {
        let (session, feed) = fake_session();
        let (window, view) = open_test_window(cx, move |window, cx| {
            SessionView::new(
                PortId::new("virtual:echo"),
                SerialConfig::default(),
                session,
                window,
                cx,
            )
        });
        (window, view, feed)
    }

    #[gpui_test]
    fn a_burst_of_events_is_one_batch_and_one_notify(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        let notifies = Rc::new(Cell::new(0));
        cx.update(|cx| {
            let notifies = notifies.clone();
            cx.observe(&view, move |_, _| notifies.set(notifies.get() + 1))
                .detach();
        });

        feed.connected("virtual:echo");
        for i in 0..200 {
            feed.data(format!("line {i}\r\n").as_bytes());
        }
        cx.run_until_parked();

        assert_eq!(notifies.get(), 1);
        view.read_with(cx, |view, _| {
            let model = view.model();
            assert_eq!(model.state, ConnectionState::Connected);
            assert_eq!(model.buffer.len(), 201);
            assert_eq!(model.buffer.row(200).unwrap().text, "line 199");
            assert_eq!(model.stats.rx_chunks, 200);
        });

        // Nothing new: idle wakes must not repaint.
        for _ in 0..3 {
            cx.executor().advance_clock(FRAME);
            cx.run_until_parked();
        }
        assert_eq!(notifies.get(), 1);

        feed.data(b"more\n");
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();
        assert_eq!(notifies.get(), 2);
    }

    #[gpui_test]
    fn submitting_writes_and_echoes(cx: &mut TestAppContext) {
        let (window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        cx.run_until_parked();

        cx.update_window(window, |_, window, cx| {
            let compose = view.read(cx).compose().clone();
            compose.update(cx, |compose, cx| {
                compose
                    .input()
                    .update(cx, |input, cx| input.set_value("AT", window, cx));
                compose.submit(window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(feed.written(), [b"AT\r\n".to_vec()]);
        view.read_with(cx, |view, cx| {
            let last = view.model().buffer.rows().last().unwrap();
            assert_eq!((last.kind, last.text), (LineKind::Tx, "AT"));
            assert_eq!(view.compose().read(cx).text(cx), "", "input clears");
        });
    }

    #[gpui_test]
    fn disconnect_closes_the_session_off_the_main_thread(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        cx.run_until_parked();

        view.update(cx, |view, cx| view.disconnect(cx));
        cx.run_until_parked();
        assert!(feed.was_closed());

        // The close echoes an orderly Disconnected, which must not print twice.
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let model = view.model();
            assert!(model.state.is_disconnected());
            let disconnects = model
                .buffer
                .rows()
                .filter(|r| r.text == "Disconnected")
                .count();
            assert_eq!(disconnects, 1);
            assert_eq!(view.open_port(), None);
        });

        view.update(cx, |view, cx| {
            view.send("AT", b"AT\r\n".to_vec(), cx);
        });
        assert!(feed.written().is_empty());
    }

    #[gpui_test]
    fn losing_the_device_releases_the_session(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        feed.data(b"last words");
        feed.disconnected(Some(TransportError::Disconnected));
        cx.run_until_parked();
        assert!(feed.was_closed(), "the dead session's threads are joined");

        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let model = view.model();
            assert_eq!(
                model.state,
                ConnectionState::Disconnected {
                    error: Some("device disconnected".into())
                }
            );
            let rows = rows(model);
            assert_eq!(
                rows[rows.len() - 2..],
                [
                    (LineKind::Rx, "last words".into()),
                    (LineKind::Error, "Disconnected: device disconnected".into()),
                ]
            );
        });
    }

    #[gpui_test]
    fn clear_empties_the_scrollback(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.data(b"one\ntwo\n");
        cx.run_until_parked();
        view.update(cx, |view, cx| view.clear(cx));
        view.read_with(cx, |view, _| assert!(view.model().buffer.is_empty()));
    }
}
