//! The session view: scrollback, follow-tail, pause, export, recording and the compose
//! bar for one open port.
//!
//! This file is rendering and wiring only; what is shown lives in
//! [`SessionModel`](crate::session_model::SessionModel). Milestone 0 renders the
//! scrollback as a `uniform_list` of text rows; milestone 1 replaces it with the custom
//! terminal element over the page store, and the batching, follow-tail, pause and
//! capture behaviour carry over.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use serialist_core::{PortId, SerialConfig};

use crate::actions::{JumpToBottom, context};
use crate::capture::{Recorder, RecorderStats, RecordingSlot};
use crate::compose::{ComposeBar, ComposeEvent};
use crate::drain::drain_into;
use crate::export::{ExportFormat, ExportJob};
use crate::line_buffer::LineKind;
use crate::prelude::*;
use crate::session_handle::SessionHandle;
use crate::session_model::{
    DrainState, Notice, RecordingStatus, SessionModel, SessionUpdate, file_name, format_bytes,
    prepare_updates,
};

const ROW_HEIGHT: Pixels = px(18.);

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

/// Where the save dialog starts.
fn default_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub struct SessionView {
    model: SessionModel,
    /// `None` once disconnected; the scrollback stays readable.
    session: Option<Box<dyn SessionHandle>>,
    /// Shared with the drain worker, which writes the active recording.
    recording: RecordingSlot,
    next_recording_id: u64,
    /// Recorded bytes last shown, so idle wakes repaint only when it moves.
    last_recorded: Option<u64>,
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
        // Joining the session's threads can take a read timeout's worth of time, and a
        // recording needs its final flush, so a dropped view hands both to the
        // background executor.
        let release = cx.on_release(|this, cx| {
            if let Some(session) = this.session.take() {
                cx.background_spawn(async move { session.close() }).detach();
            }
            if let Some(status) = this.model.recording.take() {
                let slot = this.recording.clone();
                cx.background_spawn(async move {
                    if let Some(recorder) = slot.take(status.id) {
                        let _ = recorder.finish();
                    }
                })
                .detach();
            }
        });
        let recording = RecordingSlot::default();
        let drain = drain_into(
            session.events(),
            DrainState {
                recording: recording.clone(),
                ..DrainState::default()
            },
            prepare_updates,
            cx,
            |this: &mut Self, updates, cx| this.apply_updates(updates, cx),
        );

        Self {
            model: SessionModel::new(port, serial),
            session: Some(session),
            recording,
            next_recording_id: 0,
            last_recorded: None,
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

    #[cfg(test)]
    pub(crate) fn model_mut(&mut self) -> &mut SessionModel {
        &mut self.model
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
        // The recording's byte count moves on the drain worker; repaint when it does.
        let recorded = self
            .model
            .recording
            .as_ref()
            .and_then(|status| status.stats.as_ref())
            .map(|stats| stats.bytes());
        if recorded != self.last_recorded {
            self.last_recorded = recorded;
            changed = true;
        }
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
        // Paused rows never move, so there is nothing new to follow.
        if self.follow_tail && !self.model.is_paused() {
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
        self.model.clear();
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
        self.stop_recording(cx);
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

    pub fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.model.is_paused() {
            self.resume(cx);
        } else {
            self.pause(cx);
        }
    }

    /// Pin what is on screen. The session, the drain and the live buffer carry on.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.model.pause() {
            cx.notify();
        }
    }

    /// Show the live buffer again, following its tail.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.model.resume() {
            self.jump_to_bottom(cx);
        }
    }

    fn file_stem(&self) -> String {
        let port: String = self
            .model
            .port
            .as_str()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("serialist-{}", port.trim_matches('-'))
    }

    /// Ask for a file name with the platform's save dialog, then call `then` with it.
    fn prompt_for_path(
        &mut self,
        suggested_name: String,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut Self, PathBuf, &mut Context<Self>) + 'static,
    ) {
        let answer = cx.prompt_for_new_path(&default_directory(), Some(&suggested_name));
        cx.spawn(async move |this, cx| {
            let outcome = match answer.await {
                Ok(Ok(Some(path))) => Ok(path),
                // Cancelled, or the dialog went away.
                Ok(Ok(None)) | Err(_) => return,
                Ok(Err(error)) => Err(error),
            };
            this.update(cx, |view, cx| {
                match outcome {
                    Ok(path) => then(view, path, cx),
                    Err(error) => {
                        view.model.notice = Some(Notice::error(format!(
                            "Could not open the save dialog: {error}"
                        )));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Ask where to export, then export. The file's extension picks the format;
    /// `preferred` is the suggestion and the fallback for unknown extensions.
    pub fn export(&mut self, preferred: ExportFormat, cx: &mut Context<Self>) {
        let suggested = format!("{}.{}", self.file_stem(), preferred.extension());
        self.prompt_for_path(suggested, cx, move |view, path, cx| {
            let format = ExportFormat::from_path(&path).unwrap_or(preferred);
            view.export_to(path, format, cx).detach();
        });
    }

    /// Export to `path`: the displayed rows as text (the pinned rows when paused), or the
    /// raw capture. The content is taken now; the file is written in the background, and
    /// the outcome lands in the status line.
    pub fn export_to(
        &mut self,
        path: PathBuf,
        format: ExportFormat,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let job = match format {
            ExportFormat::Text => ExportJob::Text(self.model.displayed_text()),
            ExportFormat::Raw => ExportJob::Raw {
                chunks: self.model.raw.snapshot(),
                evicted: self.model.raw.evicted_bytes(),
            },
        };
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { job.run(&path) }).await;
            this.update(cx, |view, cx| {
                view.model.notice = Some(match outcome {
                    Ok(summary) => Notice::info(summary),
                    Err(error) => Notice::error(error),
                });
                cx.notify();
            })
            .ok();
        })
    }

    /// Stop the recording, or ask for a file and start one.
    pub fn toggle_record(&mut self, cx: &mut Context<Self>) {
        match &self.model.recording {
            Some(status) if status.stats.is_some() => self.stop_recording(cx),
            // Still opening the file; the next press stops it.
            Some(_) => {}
            None if self.session.is_none() => {
                self.model.notice = Some(Notice::error("Not connected; nothing to record"));
                cx.notify();
            }
            None => {
                let suggested = format!("{}-recording.bin", self.file_stem());
                self.prompt_for_path(suggested, cx, |view, path, cx| {
                    view.start_recording(path, cx);
                });
            }
        }
    }

    /// Append every received chunk to `path` from now on, until stopped, disconnected
    /// or closed. The file is opened on the background executor.
    pub fn start_recording(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.model.recording.is_some() || self.session.is_none() {
            return;
        }
        self.next_recording_id += 1;
        let id = self.next_recording_id;
        self.model.recording = Some(RecordingStatus {
            id,
            path: path.clone(),
            stats: None,
        });
        let slot = self.recording.clone();
        let name = file_name(&path);
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    let recorder = Recorder::create(&path).map_err(|error| error.to_string())?;
                    let stats = recorder.stats();
                    if let Some(displaced) = slot.install(id, recorder) {
                        let _ = displaced.finish();
                    }
                    Ok(stats)
                })
                .await;
            this.update(cx, |view, cx| view.recording_opened(id, name, opened, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    fn recording_opened(
        &mut self,
        id: u64,
        name: String,
        opened: Result<Arc<RecorderStats>, String>,
        cx: &mut Context<Self>,
    ) {
        let is_current = self.model.recording.as_ref().is_some_and(|s| s.id == id);
        match opened {
            Ok(stats) if is_current => {
                if let Some(status) = &mut self.model.recording {
                    status.stats = Some(stats);
                }
                // The session may have ended while the file was opening.
                if self.session.is_none() {
                    self.stop_recording(cx);
                }
            }
            // Stopped while opening: close the file that was just installed.
            Ok(_) => self.finish_recording(id, name, cx),
            Err(error) => {
                if is_current {
                    self.model.recording = None;
                }
                self.model.notice = Some(Notice::error(format!(
                    "Could not record to {name}: {error}"
                )));
            }
        }
        cx.notify();
    }

    /// Stop recording with a final flush. Safe to call when not recording.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.model.recording.take() else {
            return;
        };
        // Still opening: `recording_opened` sees the status gone and closes the file.
        if status.stats.is_some() {
            self.finish_recording(status.id, status.file_name(), cx);
        }
        cx.notify();
    }

    fn finish_recording(&mut self, id: u64, name: String, cx: &mut Context<Self>) {
        let slot = self.recording.clone();
        cx.spawn(async move |this, cx| {
            let finished = cx
                .background_spawn(async move { slot.take(id).map(Recorder::finish) })
                .await;
            let notice = match finished {
                Some(Ok(bytes)) => {
                    Notice::info(format!("Recorded {} to {name}", format_bytes(bytes)))
                }
                Some(Err(error)) => Notice::error(format!("Recording to {name} failed: {error}")),
                None => return,
            };
            this.update(cx, |view, cx| {
                view.model.notice = Some(notice);
                cx.notify();
            })
            .ok();
        })
        .detach();
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
            .filter_map(|ix| self.model.displayed_row(ix))
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

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let paused = self.model.is_paused();
        let recording = self.model.recording.is_some();
        h_flex()
            .flex_none()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("pause")
                    .label(if paused { "Resume" } else { "Pause" })
                    .tooltip("Freeze the view while data keeps arriving")
                    .small()
                    .ghost()
                    .toggled(paused)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))),
            )
            .child(
                Button::new("export-text")
                    .label("Export…")
                    .tooltip("Save the displayed lines (.txt) or the raw capture (.bin)")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.export(ExportFormat::Text, cx))),
            )
            .child(
                Button::new("export-raw")
                    .label("Export raw…")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.export(ExportFormat::Raw, cx))),
            )
            .child(
                Button::new("record")
                    .label(if recording {
                        "Stop recording"
                    } else {
                        "Record…"
                    })
                    .tooltip("Append every received byte to a file")
                    .small()
                    .ghost()
                    .toggled(recording)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx))),
            )
            .when(paused, |bar| {
                bar.child(
                    div()
                        .ml_auto()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Paused: showing a snapshot, still receiving"),
                )
            })
    }
}

impl Render for SessionView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(cx);
        let theme = cx.theme();
        let scrollback = uniform_list(
            "scrollback",
            self.model.displayed_len(),
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
            .child(toolbar)
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

    use serialist_core::TransportError;

    use super::*;
    use crate::drain::FRAME;
    use crate::session_model::ConnectionState;
    use crate::test_support::{fake_session, open_test_window};

    fn rows(view: &SessionView) -> Vec<(LineKind, String)> {
        view.model()
            .buffer
            .rows()
            .map(|r| (r.kind, r.text.to_owned()))
            .collect()
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
            assert_eq!(
                view.model().state,
                ConnectionState::Disconnected {
                    error: Some("device disconnected".into())
                }
            );
            let rows = rows(view);
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
