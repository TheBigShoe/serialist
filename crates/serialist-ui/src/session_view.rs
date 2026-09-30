//! The session view: one open port's scrollback in the terminal element, with pause,
//! export, recording and the compose bar.
//!
//! # Data path
//!
//! Received bytes never reach this thread. Opening the view spawns the session's ingest
//! thread ([`Ingest::spawn`]), which owns the page store: it appends every chunk, turns
//! the session's connect, disconnect and write-failure events into notice lines, and
//! hands every chunk to the [`RecordingSink`], which also flushes a recording when the
//! stream goes quiet ([`ChunkSink::on_idle`]). The view holds a [`Snapshot`] of the
//! store (an `Arc`), and the terminal draws from it. Where the link stands, and what the
//! transport calls it, come from [`IngestHandle::connection`].
//!
//! # Waking
//!
//! The ingest thread calls its waker once per publication that finds the store clean,
//! and the waker rings a one-slot doorbell (`async_channel::bounded(1)`; ringing a
//! doorbell that is already ringing does nothing), so at most one wake is ever pending.
//! A foreground task waits on the doorbell. Per ring the view:
//!
//! 1. calls [`IngestHandle::acknowledge`], so anything published from now on rings
//!    again;
//! 2. takes [`IngestHandle::snapshot`], which is therefore never older than the ring;
//! 3. hands the terminal the snapshot's text and hex sources
//!    ([`TerminalView::update_sources`]) and the changed lines
//!    ([`TerminalView::lines_appended`]), which keeps scroll, selection and pause, and
//!    lets the terminal follow the tail and refresh an open search;
//! 4. reads the session's counters and the link state and repaints the status line;
//! 5. waits a frame before answering the next ring.
//!
//! However fast the port, that is at most one snapshot and one repaint per frame, and
//! none while idle. A slow timer ([`HOUSEKEEPING`]) covers what moves without new lines:
//! TX counters and a recording's byte count.
//!
//! # Two modes
//!
//! In command mode the compose bar sends a line at a time. In inline mode (see
//! [`inline`](crate::inline)) the compose bar is hidden and the terminal takes the
//! keyboard: a keystroke interceptor encodes every key the terminal has focus for and
//! writes it at once, except keys bound in the `TerminalInline` context, which stay
//! actions. Paste goes out in paced chunks from an async task that leaving inline mode
//! cancels.
//!
//! # Saved commands
//!
//! [`SessionView::send_command`] encodes a saved command with the session's line
//! ending, echoes it as a `Tx` line (whatever the local echo setting), registers its
//! `expect` with the ingest thread's matchers *before* writing, then writes. A task
//! polls the expectation each frame: a match highlights the matched text in the
//! terminal and says `Version: OK in 12 ms` in the status line; a timeout adds a notice
//! line and says so in the status line too. Parameter values sent are remembered for
//! the session.
//!
//! # Pause, clear, export, record
//!
//! Pause freezes the terminal's end at the current snapshot's end; snapshots keep
//! arriving underneath (so eviction and the counters move) and the status line shows
//! what arrived since. Clear raises a floor over the scrollback (see
//! [`scrollback`](crate::scrollback)); the store and the raw stream are untouched.
//! Export writes the selection, else what a paused view shows, else everything retained,
//! from a snapshot on a background thread. Recording is a sink installed with the ingest
//! thread and switched through a [`RecordingSlot`], so it starts and stops mid-session.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serialist_core::{
    ChunkSink, Command, CommandRef, ConnectionInfo, Direction, ExpectResult, Expectation, Ingest,
    IngestHandle, IngestPanicked, IngestStats, LineEnding, LineId, LineSource, LinkState,
    ParamValues, Payload, PortId, SearchMatch, SerialConfig, SessionStats, Snapshot, Store,
    TextOptions, Timestamps,
};

use crate::actions::{self, context};
use crate::capture::{Recorder, RecorderStats, RecordingSink, RecordingSlot};
use crate::compose::{ComposeBar, ComposeEvent};
use crate::config::Config;
use crate::export::{ExportFormat, ExportJob};
use crate::history::PersistentHistory;
use crate::inline::{
    EchoLine, EncodedKey, EscapeChord, InlineConfig, KeyEncoder, Mode, PasteProgress, is_chord,
    paste_bytes,
};
use crate::prelude::*;
use crate::scrollback::{Floors, Scrollback};
use crate::session_handle::SessionHandle;
use crate::session_options::SessionOptions;
use crate::status::{
    ConnectionState, Notice, PauseMark, RecordingStatus, StatusInputs, StatusLine, file_name,
    format_bytes,
};
use crate::terminal::{DisplayMode, TerminalView, TimestampMode};

/// The shortest time between two snapshots: about a frame at 120 Hz.
pub const FRAME: Duration = Duration::from_millis(8);

/// How often the view reads counters that move without new lines (TX, a recording's
/// bytes).
pub const HOUSEKEEPING: Duration = Duration::from_millis(250);

/// How often a pending expectation is looked at: about a frame.
pub const EXPECT_POLL: Duration = FRAME;

/// What the session view tells the workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionViewEvent {
    /// "Save as command" in the compose bar: open the editor on `text`.
    SaveAsCommand { text: String },
}

/// What a sent command's echo line says: text payloads as sent without the line ending,
/// hex and codec payloads as the bytes in hex.
fn command_echo(command: &Command, bytes: &[u8], session_eol: LineEnding) -> String {
    match &command.payload {
        Payload::Text(_) => {
            let eol = command.eol.unwrap_or(session_eol).bytes();
            let body = bytes.strip_suffix(eol).unwrap_or(bytes);
            String::from_utf8_lossy(body).into_owned()
        }
        Payload::Hex(_) | Payload::Codec { .. } => bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// What to highlight for a match at `range` in `text`: that range, or the whole line
/// when the pattern matched nothing in particular (`^`, an empty group).
fn highlight_range(range: Range<usize>, text: &str) -> Range<usize> {
    if range.is_empty() {
        0..text.len()
    } else {
        range
    }
}

/// The `inline.*` settings in force, or the defaults without the app's configuration.
fn inline_settings(cx: &App) -> InlineConfig {
    cx.try_global::<Config>()
        .map(|config| config.inline().clone())
        .unwrap_or_default()
}

/// Whether `keystroke` is bound in the innermost context of `stack` (the terminal's
/// `TerminalInline`), which makes it an action rather than a key for the port.
fn bound_in_innermost_context(keystroke: &Keystroke, stack: &[KeyContext], cx: &App) -> bool {
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    keymap
        .all_bindings_for_input(std::slice::from_ref(keystroke))
        .iter()
        .any(|binding| {
            binding
                .predicate()
                .is_some_and(|predicate| predicate.depth_of(stack) == Some(stack.len()))
        })
}

/// A paste going out in chunks.
struct PasteJob {
    id: u64,
    progress: PasteProgress,
    /// Dropping it stops the paste.
    _task: Task<()>,
}

/// Inline mode's state.
#[derive(Default)]
struct InlineState {
    /// Typed since the last Enter, for local echo.
    echo: EchoLine,
    chord: EscapeChord,
    paste: Option<PasteJob>,
    next_paste: u64,
}

/// Where the save dialog starts.
fn default_directory() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn export_timestamps(mode: TimestampMode) -> Timestamps {
    match mode {
        TimestampMode::Off => Timestamps::None,
        TimestampMode::Absolute => Timestamps::Absolute,
        TimestampMode::Relative => Timestamps::Relative,
        TimestampMode::Delta => Timestamps::Delta,
    }
}

/// The stream bytes under `lines` of `source`: from the first line's first byte to the
/// last line's last. Local lines have no bytes of their own.
fn raw_under(source: &dyn LineSource, lines: Range<LineId>) -> Range<u64> {
    if lines.is_empty() {
        return 0..0;
    }
    let start = source.line(lines.start).map(|line| line.raw.start);
    let end = source
        .line(LineId(lines.end.0 - 1))
        .map(|line| line.raw.end);
    match (start, end) {
        (Some(start), Some(end)) => start..end.max(start),
        _ => 0..0,
    }
}

pub struct SessionView {
    port: PortId,
    serial: SerialConfig,
    /// Where the link stands and the transport's own name for it, as ingest last
    /// reported.
    connection: ConnectionInfo,
    state: ConnectionState,
    stats: SessionStats,
    /// `None` once disconnected; the scrollback stays readable.
    session: Option<Box<dyn SessionHandle>>,
    /// `None` only while the view is being released.
    ingest: Option<IngestHandle>,
    /// The newest snapshot, as the terminal's sources.
    scrollback: Scrollback,
    floors: Floors,
    pause: Option<PauseMark>,
    /// Shared with the ingest thread's recording sink.
    recording: RecordingSlot,
    recording_status: Option<RecordingStatus>,
    next_recording_id: u64,
    /// Recorded bytes last shown, so idle housekeeping repaints only when it moves.
    last_recorded: Option<u64>,
    notice: Option<Notice>,
    /// Bytes per hex row.
    hex_bytes_per_row: usize,
    /// The options last applied, so a settings reload changes only what it changed and
    /// leaves the user's own toggles alone.
    options: SessionOptions,
    terminal: Entity<TerminalView>,
    compose: Entity<ComposeBar>,
    mode: Mode,
    inline: InlineState,
    /// Parameter values last sent per command, to prefill the next prompt.
    remembered: HashMap<CommandRef, ParamValues>,
    /// The workspace's persisted compose history, which sent lines are added to.
    history: Option<Entity<PersistentHistory>>,
    focus_handle: FocusHandle,
    _wake: Task<()>,
    _housekeeping: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SessionViewEvent> for SessionView {}

impl Focusable for SessionView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SessionView {
    /// Show `session`, whose scrollback lives in a new store sized by `options`, which
    /// also give the line ending, local echo and display defaults.
    pub fn new(
        port: PortId,
        serial: SerialConfig,
        session: Box<dyn SessionHandle>,
        options: SessionOptions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_sinks(port, serial, session, options, Vec::new(), window, cx)
    }

    /// [`Self::new`] with more sinks on the ingest thread, after the view's own.
    pub fn with_sinks(
        port: PortId,
        serial: SerialConfig,
        session: Box<dyn SessionHandle>,
        options: SessionOptions,
        extra_sinks: Vec<Box<dyn ChunkSink>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let line_ending = options.line_ending;
        let compose = cx.new(|cx| {
            let mut compose = ComposeBar::new(window, cx);
            compose.set_line_ending(line_ending, cx);
            compose.set_local_echo(options.local_echo, cx);
            compose
        });
        let compose_events =
            cx.subscribe_in(&compose, window, |this, _, event, _, cx| match event {
                ComposeEvent::Submit { text, bytes } => {
                    if let Some(history) = &this.history {
                        history.update(cx, |history, cx| history.push(text, cx));
                    }
                    this.send(text, bytes.clone(), cx);
                }
                ComposeEvent::SaveAsCommand { text } => {
                    cx.emit(SessionViewEvent::SaveAsCommand { text: text.clone() });
                }
            });
        // Joining the session's and the ingest thread can take a read timeout's worth of
        // time, and a recording needs its final flush, so a dropped view hands all three
        // to the background executor.
        let release = cx.on_release(|this, cx| {
            if let Some(session) = this.session.take() {
                cx.background_spawn(async move { session.close() }).detach();
            }
            if let Some(ingest) = this.ingest.take() {
                cx.background_spawn(async move { drop(ingest) }).detach();
            }
            if let Some(status) = this.recording_status.take() {
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
        let (doorbell, rings) = async_channel::bounded::<()>(1);
        let mut sinks: Vec<Box<dyn ChunkSink>> =
            vec![Box::new(RecordingSink::new(recording.clone()))];
        sinks.extend(extra_sinks);
        let ingest = Ingest::spawn(
            session.events(),
            Store::new(options.store.clone()),
            sinks,
            Box::new(move || {
                // Full means a wake is already pending: that one will see this too.
                let _ = doorbell.try_send(());
            }),
        );
        let wake = cx.spawn(async move |this, cx| {
            while rings.recv().await.is_ok() {
                if this.update(cx, |view, cx| view.wake(cx)).is_err() {
                    return;
                }
                cx.background_executor().timer(FRAME).await;
            }
            // The doorbell's only sender lives in the ingest thread's waker, so a closed
            // doorbell means that thread has ended, cleanly or not.
            this.update(cx, |view, cx| view.ingest_ended(cx)).ok();
        });
        let housekeeping = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(HOUSEKEEPING).await;
                if this.update(cx, |view, cx| view.housekeeping(cx)).is_err() {
                    break;
                }
            }
        });

        let floors = Floors::default();
        let display = options.display;
        let scrollback =
            Scrollback::with_hex_row(&ingest.snapshot(), floors, display.hex_bytes_per_row);
        let terminal = cx.new(|cx| {
            let mut terminal = TerminalView::new(scrollback.text_source(), window, cx);
            terminal.set_searcher(Some(scrollback.text_searcher()), cx);
            terminal.set_hex_source(
                Some(scrollback.hex_source()),
                Some(scrollback.hex_searcher()),
                cx,
            );
            terminal.set_wrap(display.wrap, cx);
            terminal.set_timestamps(display.timestamps, cx);
            terminal.set_display_mode(display.view, cx);
            terminal
        });
        // Inline mode takes keys before the keymap sees them (see `intercept_key`).
        let view = cx.entity().downgrade();
        let interceptor = cx.intercept_keystrokes(move |event, window, cx| {
            view.update(cx, |view, cx| view.intercept_key(event, window, cx))
                .ok();
        });
        tracing::info!(%port, serial = %serial.summary(), "session open");

        Self {
            port,
            serial,
            connection: ConnectionInfo::default(),
            state: ConnectionState::Connected,
            stats: session.stats(),
            session: Some(session),
            ingest: Some(ingest),
            scrollback,
            floors,
            pause: None,
            recording,
            recording_status: None,
            next_recording_id: 0,
            last_recorded: None,
            notice: None,
            hex_bytes_per_row: display.hex_bytes_per_row,
            options,
            terminal,
            compose,
            mode: Mode::Command,
            inline: InlineState::default(),
            remembered: HashMap::new(),
            history: None,
            focus_handle: cx.focus_handle(),
            _wake: wake,
            _housekeeping: housekeeping,
            _subscriptions: vec![compose_events, release, interceptor],
        }
    }

    // --- Reading the view ------------------------------------------------------------

    pub fn port(&self) -> &PortId {
        &self.port
    }

    /// The line settings the port was opened with.
    pub fn serial(&self) -> &SerialConfig {
        &self.serial
    }

    pub fn state(&self) -> &ConnectionState {
        &self.state
    }

    /// The session's byte counters as last read.
    pub fn stats(&self) -> SessionStats {
        self.stats
    }

    /// The ingest thread's counters, read now.
    pub fn ingest_stats(&self) -> Option<IngestStats> {
        self.ingest.as_ref().map(IngestHandle::stats)
    }

    /// The newest snapshot the view has taken, and the sources it gave the terminal.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    pub fn snapshot(&self) -> &Snapshot {
        self.scrollback.snapshot()
    }

    pub fn terminal(&self) -> &Entity<TerminalView> {
        &self.terminal
    }

    pub fn compose(&self) -> &Entity<ComposeBar> {
        &self.compose
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub fn recording(&self) -> Option<&RecordingStatus> {
        self.recording_status.as_ref()
    }

    pub fn is_paused(&self) -> bool {
        self.pause.is_some()
    }

    /// Where the stream stood when paused.
    pub fn pause_mark(&self) -> Option<PauseMark> {
        self.pause
    }

    pub fn is_following_tail(&self, cx: &App) -> bool {
        self.terminal.read(cx).is_following_tail()
    }

    /// The port this view holds open, if it is still open.
    pub fn open_port(&self) -> Option<&PortId> {
        (!self.state.is_disconnected()).then_some(&self.port)
    }

    /// What the status line names the session by: the transport's description once
    /// ingest has seen the link connect, else the port and its settings (which is what
    /// every transport describes itself as today).
    pub fn title(&self) -> String {
        self.connection
            .description
            .clone()
            .unwrap_or_else(|| format!("{} @ {}", self.port, self.serial.summary()))
    }

    /// Where the link stands, as ingest last reported it.
    pub fn connection(&self) -> &ConnectionInfo {
        &self.connection
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The paste going out in inline mode, if any.
    pub fn paste_progress(&self) -> Option<PasteProgress> {
        self.inline.paste.as_ref().map(|paste| paste.progress)
    }

    /// The status line's text for this session.
    pub fn status_line(&self) -> StatusLine {
        StatusLine::new(StatusInputs {
            state: &self.state,
            title: self.title(),
            settings: self.serial.summary(),
            session: self.stats,
            store: self.snapshot().stats(),
            paused: self.pause,
            recording: self.recording_status.as_ref(),
            notice: self.notice.as_ref(),
            mode: self.mode,
            paste: self.paste_progress(),
        })
    }

    pub fn focus_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compose
            .update(cx, |compose, cx| compose.focus(window, cx));
    }

    // --- The data path ---------------------------------------------------------------

    /// One ring of the doorbell: acknowledge, then snapshot, then show.
    fn wake(&mut self, cx: &mut Context<Self>) {
        let Some(ingest) = &self.ingest else {
            return;
        };
        ingest.acknowledge();
        let snapshot = ingest.snapshot();
        let shown = self.show(snapshot, cx);
        if self.poll_session(cx) || shown {
            cx.notify();
        }
    }

    /// The ingest thread has ended: after the session's events ran out (the session is
    /// gone), or because it panicked, which takes its store with it. Show what it
    /// published last, then join it off the main thread to learn which.
    fn ingest_ended(&mut self, cx: &mut Context<Self>) {
        self.wake(cx);
        let Some(ingest) = self.ingest.take() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let joined = cx
                .background_spawn(async move { ingest.join().map(drop) })
                .await;
            this.update(cx, |view, cx| view.ingest_joined(joined, cx))
                .ok();
        })
        .detach();
    }

    fn ingest_joined(&mut self, joined: Result<(), IngestPanicked>, cx: &mut Context<Self>) {
        let Err(error) = joined else {
            return;
        };
        // Nothing reaches the scrollback any more, so the session is as good as lost.
        tracing::error!(port = %self.port, %error, "the scrollback stopped");
        let message = error.to_string();
        if !self.state.is_disconnected() {
            self.state = ConnectionState::Disconnected {
                error: Some(message.clone()),
            };
        }
        self.notice = Some(Notice::error(message));
        self.close_session(cx);
        cx.notify();
    }

    /// Hand `snapshot` to the terminal if it holds anything new. Returns whether it did.
    fn show(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) -> bool {
        let before = self.snapshot().stats();
        let after = snapshot.stats();
        if before == after {
            return false;
        }
        // The line that was still arriving may have grown, so it counts as changed.
        let changed =
            LineId(before.end_line.0.saturating_sub(1)).max(after.first_line)..after.end_line;
        self.scrollback = Scrollback::with_hex_row(&snapshot, self.floors, self.hex_bytes_per_row);
        self.push_sources(cx, |terminal, cx| terminal.lines_appended(changed, cx));
        true
    }

    /// Give the terminal the current scrollback's sources, then run `then` on it.
    fn push_sources(
        &self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>),
    ) {
        let scrollback = self.scrollback.clone();
        self.terminal.update(cx, |terminal, cx| {
            terminal.update_sources(
                scrollback.text_source(),
                Some(scrollback.text_searcher()),
                Some(scrollback.hex_source()),
                Some(scrollback.hex_searcher()),
                cx,
            );
            then(terminal, cx);
        });
    }

    /// Read the session's counters and the link state, and notice a lost device.
    /// Returns whether anything the status line shows changed.
    fn poll_session(&mut self, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        if let Some(session) = &self.session {
            let stats = session.stats();
            if stats != self.stats {
                self.stats = stats;
                changed = true;
            }
        }
        if let Some(ingest) = &self.ingest {
            let connection = ingest.connection();
            if connection != self.connection {
                self.connection = connection;
                changed = true;
            }
        }
        if let LinkState::Disconnected { error } = &self.connection.state
            && !self.state.is_disconnected()
        {
            // The link went away by itself (a local disconnect sets the state first).
            let error = error.clone();
            tracing::info!(port = %self.port, ?error, "session ended");
            self.state = ConnectionState::Disconnected { error };
            self.close_session(cx);
            changed = true;
        }
        changed
    }

    fn housekeeping(&mut self, cx: &mut Context<Self>) {
        let mut changed = self.poll_session(cx);
        let recorded = self
            .recording_status
            .as_ref()
            .and_then(|status| status.stats.as_ref())
            .map(|stats| stats.bytes());
        if recorded != self.last_recorded {
            self.last_recorded = recorded;
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    /// Write a submitted line and, with local echo on in the compose bar, echo it into
    /// the scrollback. The echo is queued before the write, and ingest applies local
    /// lines before the next session event, so the echo always lands before the reply
    /// it causes.
    pub fn send(&mut self, text: &str, bytes: Vec<u8>, cx: &mut Context<Self>) {
        let echo = self.compose.read(cx).local_echo();
        let Some((session, ingest)) = self.live() else {
            self.notice = Some(Notice::error(format!("Not connected; not sent: {text}")));
            cx.notify();
            return;
        };
        if echo {
            let _ = ingest.append_local(text, Direction::Tx);
        }
        if session.write(bytes).is_err() {
            let _ = ingest.append_local("Not sent: the session closed", Direction::Notice);
        }
    }

    /// The session and its ingest thread, while the link is up.
    fn live(&self) -> Option<(&dyn SessionHandle, &IngestHandle)> {
        let session = self
            .session
            .as_deref()
            .filter(|session| session.is_connected() && !self.state.is_disconnected())?;
        Some((session, self.ingest.as_ref()?))
    }

    /// The line ending the compose bar sends, which inline Enter sends too.
    pub fn line_ending(&self, cx: &App) -> LineEnding {
        self.compose.read(cx).line_ending()
    }

    /// Share the workspace's persisted history: the compose bar starts from it, and
    /// every line sent is added to it.
    pub fn set_history(&mut self, history: Entity<PersistentHistory>, cx: &mut Context<Self>) {
        let entries = history.read(cx).entries_oldest_first();
        self.compose
            .update(cx, |compose, cx| compose.set_history_entries(entries, cx));
        self.history = Some(history);
    }

    // --- Saved commands --------------------------------------------------------------

    /// The parameter values last sent with `reference` in this session.
    pub fn remembered_params(&self, reference: &CommandRef) -> Option<&ParamValues> {
        self.remembered.get(reference)
    }

    /// Send a saved command: encode it with `params` and the session's line ending,
    /// echo it as a `Tx` line, register its expectation, then write it. The outcome
    /// lands in the status line, and a matched reply is highlighted.
    pub fn send_command(
        &mut self,
        reference: &CommandRef,
        command: &Command,
        params: &ParamValues,
        cx: &mut Context<Self>,
    ) {
        let name = command.name.clone();
        if !params.is_empty() {
            self.remembered.insert(reference.clone(), params.clone());
        }
        let session_eol = self.line_ending(cx);
        let bytes = match command.encode(params, session_eol) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.notice = Some(Notice::error(format!("{name}: {error}")));
                cx.notify();
                return;
            }
        };
        let Some((session, ingest)) = self.live() else {
            self.notice = Some(Notice::error(format!("Not connected; not sent: {name}")));
            cx.notify();
            return;
        };
        // A saved command is always echoed: it is the record of what was sent.
        let _ = ingest.append_local(command_echo(command, &bytes, session_eol), Direction::Tx);
        // Registered before the write, so the reply cannot arrive unwatched.
        let expectation = command.expect.as_ref().map(|expect| {
            ingest
                .matchers()
                .expect(&expect.pattern, expect.timeout())
                .map(|expectation| (expectation, expect.timeout_ms))
        });
        if session.write(bytes).is_err() {
            let _ = ingest.append_local("Not sent: the session closed", Direction::Notice);
        }
        tracing::debug!(command = %reference, "sent saved command");
        match expectation {
            Some(Ok((expectation, timeout_ms))) => {
                self.await_reply(name, expectation, timeout_ms, cx);
            }
            Some(Err(error)) => {
                self.notice = Some(Notice::error(format!(
                    "{name}: the expected reply is not a valid pattern: {error}"
                )));
            }
            None => self.notice = Some(Notice::info(format!("Sent {name}"))),
        }
        cx.notify();
    }

    /// Poll `expectation` each frame until it resolves, then report it. The matcher
    /// resolves it on the ingest thread, so polling never blocks.
    fn await_reply(
        &mut self,
        name: String,
        expectation: Expectation,
        timeout_ms: u64,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(Notice::info(format!("{name}: waiting for a reply…")));
        cx.spawn(async move |this, cx| {
            loop {
                if let Some(result) = expectation.try_wait() {
                    this.update(cx, |view, cx| {
                        view.reply(&name, timeout_ms, result, cx);
                    })
                    .ok();
                    return;
                }
                if this.upgrade().is_none() {
                    expectation.cancel();
                    return;
                }
                cx.background_executor().timer(EXPECT_POLL).await;
            }
        })
        .detach();
    }

    fn reply(&mut self, name: &str, timeout_ms: u64, result: ExpectResult, cx: &mut Context<Self>) {
        match result {
            ExpectResult::Matched {
                line,
                text,
                range,
                elapsed,
                ..
            } => {
                let range = highlight_range(range, &text);
                self.terminal.update(cx, |terminal, cx| {
                    terminal.add_mark(SearchMatch { line, range }, cx);
                });
                self.notice = Some(Notice::info(format!(
                    "{name}: OK in {} ms",
                    elapsed.as_millis()
                )));
            }
            ExpectResult::TimedOut { .. } => {
                let message = format!("{name}: no response within {timeout_ms} ms");
                if let Some(ingest) = &self.ingest {
                    let _ = ingest.append_local(message.clone(), Direction::Notice);
                }
                self.notice = Some(Notice::error(message));
            }
            ExpectResult::Closed => {
                self.notice = Some(Notice::info(format!(
                    "{name}: the session ended before a reply"
                )));
            }
        }
        cx.notify();
    }

    // --- Inline mode -----------------------------------------------------------------

    /// Switch to `mode`. Inline mode hides the compose bar and gives the terminal the
    /// keyboard; leaving it cancels a paste, echoes what was typed since the last Enter,
    /// and gives the compose bar the focus back.
    pub fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        match mode {
            Mode::Inline => {
                self.terminal
                    .update(cx, |terminal, cx| terminal.set_inline(true, cx));
                let focus = self.terminal.focus_handle(cx);
                window.focus(&focus, cx);
            }
            Mode::Command => {
                self.inline.paste = None;
                let typed = self.inline.echo.take();
                if !typed.is_empty()
                    && self.compose.read(cx).local_echo()
                    && let Some((_, ingest)) = self.live()
                {
                    let _ = ingest.append_local(typed, Direction::Tx);
                }
                self.terminal
                    .update(cx, |terminal, cx| terminal.set_inline(false, cx));
                self.focus_compose(window, cx);
            }
        }
        tracing::debug!(port = %self.port, mode = mode.label(), "input mode");
        cx.notify();
    }

    /// The mode toggle: the toolbar button and `terminal::ToggleInline`.
    pub fn toggle_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.inline.chord.reset();
        self.set_mode(self.mode.toggled(), window, cx);
    }

    fn key_encoder(&self, cx: &App) -> KeyEncoder {
        KeyEncoder::new(self.line_ending(cx), inline_settings(cx).backspace)
    }

    /// The keystroke interceptor, which runs before the keymap. In inline mode, with the
    /// terminal itself focused, an encodable key that is not bound in the
    /// `TerminalInline` context goes to the port and no further: not to the keymap, not
    /// to any key listener. It also handles the escape chord in both modes: in inline
    /// mode the chord leaves; in command mode, right after leaving, it comes back and
    /// sends the chord (see [`crate::inline`]).
    fn intercept_key(
        &mut self,
        event: &KeystrokeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keystroke = &event.keystroke;
        let terminal_focused = self.terminal.focus_handle(cx).is_focused(window);
        if is_chord(&inline_settings(cx).escape_chord, keystroke) {
            let now = cx.background_executor().now();
            match self.mode {
                Mode::Inline if terminal_focused => {
                    self.inline.chord.left(now);
                    self.set_mode(Mode::Command, window, cx);
                    cx.stop_propagation();
                }
                Mode::Command if self.focus_handle.contains_focused(window, cx) => {
                    if self.inline.chord.is_held() {
                        // The key that left is still down and repeating.
                        cx.stop_propagation();
                    } else if self.inline.chord.pressed_again(now) {
                        self.set_mode(Mode::Inline, window, cx);
                        if let Some(encoded) = self.key_encoder(cx).encode(keystroke) {
                            self.send_key(encoded, cx);
                        }
                        cx.stop_propagation();
                    }
                }
                _ => {}
            }
            return;
        }
        self.inline.chord.released();
        if self.mode != Mode::Inline
            || !terminal_focused
            || bound_in_innermost_context(keystroke, &event.context_stack, cx)
        {
            return;
        }
        let Some(encoded) = self.key_encoder(cx).encode(keystroke) else {
            return;
        };
        cx.stop_propagation();
        self.send_key(encoded, cx);
    }

    /// Notices the escape chord's key coming up, so a second press can count.
    fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        let chord = inline_settings(cx).escape_chord;
        if event.keystroke.key.eq_ignore_ascii_case(&chord.key) {
            self.inline.chord.released();
        }
    }

    /// Write one key's bytes, echoing it first when local echo is on: typed text
    /// collects until Enter, which echoes the line before the line ending goes out.
    pub fn send_key(&mut self, key: EncodedKey, cx: &mut Context<Self>) {
        if self.live().is_none() {
            self.notice = Some(Notice::error("Not connected; key not sent"));
            cx.notify();
            return;
        }
        let finished = if self.compose.read(cx).local_echo() {
            let finished = self.inline.echo.key(&key.echo);
            self.show_pending_input(cx);
            finished
        } else {
            None
        };
        let Some((session, ingest)) = self.live() else {
            return;
        };
        if let Some(line) = finished.filter(|line| !line.is_empty()) {
            let _ = ingest.append_local(line, Direction::Tx);
        }
        if session.write(key.bytes).is_err() {
            let _ = ingest.append_local("Not sent: the session closed", Direction::Notice);
        }
    }

    /// Show what was typed since the last Enter at the terminal's foot.
    fn show_pending_input(&self, cx: &mut Context<Self>) {
        let pending = SharedString::from(self.inline.echo.text().to_owned());
        self.terminal.update(cx, |terminal, cx| {
            terminal.set_pending_input(Some(pending), cx)
        });
    }

    /// Send `text` as a paste: line breaks become what Enter sends, and the bytes go
    /// out in chunks of `inline.paste_chunk_bytes`, `inline.paste_chunk_delay_ms`
    /// apart, from an async task. A paste replaces one still going; leaving inline mode
    /// stops it.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let settings = inline_settings(cx);
        let bytes = paste_bytes(text, &self.key_encoder(cx).enter);
        if bytes.is_empty() {
            return;
        }
        if self.live().is_none() {
            self.notice = Some(Notice::error("Not connected; nothing pasted"));
            cx.notify();
            return;
        }
        if self.compose.read(cx).local_echo() {
            let lines = self.inline.echo.paste(text);
            self.show_pending_input(cx);
            if let Some((_, ingest)) = self.live() {
                for line in lines.into_iter().filter(|line| !line.is_empty()) {
                    let _ = ingest.append_local(line, Direction::Tx);
                }
            }
        }
        let chunks: Vec<Vec<u8>> = bytes
            .chunks(settings.paste_chunk_bytes.max(1))
            .map(<[u8]>::to_vec)
            .collect();
        let delay = settings.paste_chunk_delay;
        self.inline.next_paste += 1;
        let id = self.inline.next_paste;
        let progress = PasteProgress {
            sent: 0,
            total: bytes.len(),
            chunks: chunks.len(),
        };
        let task = cx.spawn(async move |this, cx| {
            for (ix, chunk) in chunks.into_iter().enumerate() {
                if ix > 0 && !delay.is_zero() {
                    cx.background_executor().timer(delay).await;
                }
                let written = this
                    .update(cx, |view, cx| view.write_paste_chunk(id, chunk, cx))
                    .unwrap_or(false);
                if !written {
                    break;
                }
            }
            this.update(cx, |view, cx| {
                if view
                    .inline
                    .paste
                    .as_ref()
                    .is_some_and(|paste| paste.id == id)
                {
                    view.inline.paste = None;
                    cx.notify();
                }
            })
            .ok();
        });
        self.inline.paste = Some(PasteJob {
            id,
            progress,
            _task: task,
        });
        cx.notify();
    }

    fn write_paste_chunk(&mut self, id: u64, chunk: Vec<u8>, cx: &mut Context<Self>) -> bool {
        let len = chunk.len();
        let written = self
            .live()
            .is_some_and(|(session, _)| session.write(chunk).is_ok());
        if let Some(paste) = self.inline.paste.as_mut().filter(|paste| paste.id == id) {
            paste.progress.sent += len;
            if paste.progress.label().is_some() {
                cx.notify();
            }
        }
        written
    }

    fn paste_action(&mut self, _: &actions::Paste, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Inline {
            cx.propagate();
            return;
        }
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    fn toggle_inline_action(
        &mut self,
        _: &actions::ToggleInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_mode(window, cx);
    }

    /// Hide everything received so far. The store keeps it until its budget says
    /// otherwise, and raw export and recording still see it: they are records of the
    /// stream, not of the screen.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.floors = Floors::above(self.snapshot());
        self.rebuild_scrollback();
        self.push_sources(cx, |terminal, cx| {
            terminal.set_selection(None, cx);
            terminal.jump_to_bottom(cx);
            terminal.refresh_search(None, cx);
        });
        cx.notify();
    }

    /// Rebuild the terminal's sources from the current snapshot, floors and hex width.
    fn rebuild_scrollback(&mut self) {
        self.scrollback = Scrollback::with_hex_row(
            &self.scrollback.snapshot().clone(),
            self.floors,
            self.hex_bytes_per_row,
        );
    }

    /// The options last applied: at opening, or by [`Self::apply_options`].
    pub fn options(&self) -> &SessionOptions {
        &self.options
    }

    pub fn hex_bytes_per_row(&self) -> usize {
        self.hex_bytes_per_row
    }

    /// Take new defaults, as after a settings reload. Only what differs from the last
    /// options applied changes, so a reload that is about something else keeps the
    /// user's own toggles (wrap, timestamps, the view, the line ending). The store's
    /// budget applies to sessions opened from now on.
    pub fn apply_options(&mut self, options: SessionOptions, cx: &mut Context<Self>) {
        let old = std::mem::replace(&mut self.options, options.clone());
        if options.line_ending != old.line_ending || options.local_echo != old.local_echo {
            self.compose.update(cx, |compose, cx| {
                if options.line_ending != old.line_ending {
                    compose.set_line_ending(options.line_ending, cx);
                }
                if options.local_echo != old.local_echo {
                    compose.set_local_echo(options.local_echo, cx);
                }
            });
        }
        let (new, old) = (options.display, old.display);
        if new.hex_bytes_per_row != old.hex_bytes_per_row {
            self.hex_bytes_per_row = new.hex_bytes_per_row;
            self.rebuild_scrollback();
            let scrollback = self.scrollback.clone();
            self.terminal.update(cx, |terminal, cx| {
                terminal.set_hex_source(
                    Some(scrollback.hex_source()),
                    Some(scrollback.hex_searcher()),
                    cx,
                );
            });
        }
        self.terminal.update(cx, |terminal, cx| {
            if new.wrap != old.wrap {
                terminal.set_wrap(new.wrap, cx);
            }
            if new.timestamps != old.timestamps {
                terminal.set_timestamps(new.timestamps, cx);
            }
            if new.view != old.view {
                terminal.set_display_mode(new.view, cx);
            }
        });
        cx.notify();
    }

    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        if !self.state.is_disconnected() {
            self.state = ConnectionState::Disconnected { error: None };
        }
        self.close_session(cx);
        cx.notify();
    }

    /// Close the session off the main thread; ingest stores its `Disconnected` notice.
    fn close_session(&mut self, cx: &mut Context<Self>) {
        self.stop_recording(cx);
        if let Some(session) = self.session.take() {
            self.stats = session.stats();
            cx.background_spawn(async move { session.close() }).detach();
        }
    }

    // --- Pause -----------------------------------------------------------------------

    pub fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.is_paused() {
            self.resume(cx);
        } else {
            self.pause(cx);
        }
    }

    /// Freeze the terminal at the current snapshot's end. The session, ingest and the
    /// store carry on; the status line counts what arrives meanwhile.
    pub fn pause(&mut self, cx: &mut Context<Self>) {
        if self.pause.is_some() {
            return;
        }
        self.pause = Some(PauseMark::at(&self.snapshot().stats()));
        self.terminal.update(cx, |terminal, cx| terminal.pause(cx));
        cx.notify();
    }

    /// Follow the live tail again.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.pause.take().is_none() {
            return;
        }
        self.terminal.update(cx, |terminal, cx| terminal.resume(cx));
        cx.notify();
    }

    // --- Export ----------------------------------------------------------------------

    fn file_stem(&self) -> String {
        let port: String = self
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
                        view.notice = Some(Notice::error(format!(
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

    /// What an export in `format` writes if taken now: the selected lines (or hex
    /// rows) if there is a selection, else what a paused view shows, else everything
    /// retained. Text follows the display: lines, or hex rows in hex view, stamped as
    /// the gutter is. Raw is the stream bytes under the same choice, whole lines at a
    /// time; with no selection it ignores Clear, which hides lines, not bytes.
    pub fn export_job(&self, format: ExportFormat, cx: &App) -> ExportJob {
        let terminal = self.terminal.read(cx);
        let snapshot = self.snapshot().clone();
        let span = terminal.displayed_span();
        let selected = terminal
            .selection()
            .and_then(|selection| selection.clipped(span))
            .map(|range| {
                // A selection ending at the start of a line does not take that line.
                let end = if range.end.column == 0 && range.end.line > range.start.line {
                    range.end.line
                } else {
                    range.end.line.next()
                };
                range.start.line..end
            });
        let lines = selected.clone().unwrap_or(span.range());
        let timestamps = export_timestamps(terminal.timestamps());
        match (format, terminal.display_mode()) {
            (ExportFormat::Text, DisplayMode::Text) => ExportJob::Text {
                snapshot,
                lines,
                options: TextOptions::default().with_timestamps(timestamps),
            },
            (ExportFormat::Text, DisplayMode::Hex) => ExportJob::HexText {
                hex: self.scrollback.hex.inner.clone(),
                rows: lines,
                timestamps,
            },
            (ExportFormat::Raw, _) => {
                let range = match (selected, self.pause) {
                    (Some(lines), _) => raw_under(terminal.source().as_ref(), lines),
                    (None, Some(mark)) => 0..mark.bytes,
                    (None, None) => 0..snapshot.raw_range().end,
                };
                ExportJob::Raw { snapshot, range }
            }
        }
    }

    /// Export to `path` what [`Self::export_job`] describes. The job is taken now; the
    /// file is written in the background, and the outcome lands in the status line.
    pub fn export_to(
        &mut self,
        path: PathBuf,
        format: ExportFormat,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        let job = self.export_job(format, cx);
        cx.spawn(async move |this, cx| {
            let outcome = cx.background_spawn(async move { job.run(&path) }).await;
            this.update(cx, |view, cx| {
                view.notice = Some(match outcome {
                    Ok(summary) => Notice::info(summary),
                    Err(error) => Notice::error(error),
                });
                cx.notify();
            })
            .ok();
        })
    }

    // --- Recording -------------------------------------------------------------------

    /// Stop the recording, or ask for a file and start one.
    pub fn toggle_record(&mut self, cx: &mut Context<Self>) {
        match &self.recording_status {
            Some(status) if status.stats.is_some() => self.stop_recording(cx),
            // Still opening the file; the next press stops it.
            Some(_) => {}
            None if self.session.is_none() => {
                self.notice = Some(Notice::error("Not connected; nothing to record"));
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
    /// or closed. The file is opened on the background executor and then installed in
    /// the slot the ingest thread's sink writes through.
    pub fn start_recording(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.recording_status.is_some() || self.session.is_none() {
            return;
        }
        self.next_recording_id += 1;
        let id = self.next_recording_id;
        self.recording_status = Some(RecordingStatus {
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
        let is_current = self.recording_status.as_ref().is_some_and(|s| s.id == id);
        match opened {
            Ok(stats) if is_current => {
                if let Some(status) = &mut self.recording_status {
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
                    self.recording_status = None;
                }
                self.notice = Some(Notice::error(format!(
                    "Could not record to {name}: {error}"
                )));
            }
        }
        cx.notify();
    }

    /// Stop recording with a final flush. Safe to call when not recording.
    pub fn stop_recording(&mut self, cx: &mut Context<Self>) {
        let Some(status) = self.recording_status.take() else {
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
                view.notice = Some(notice);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // --- Rendering -------------------------------------------------------------------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme();
        let paused = self.is_paused();
        let recording = self.recording_status.is_some();
        let inline = self.mode == Mode::Inline;
        h_flex()
            .flex_none()
            .w_full()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Button::new("inline-mode")
                    .label(if inline { "Inline" } else { "Command" })
                    .tooltip(
                        "Inline: every keystroke goes to the port (the escape chord, ctrl-] by \
                         default, leaves). Command: the compose bar and saved commands.",
                    )
                    .small()
                    .ghost()
                    .toggled(inline)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_mode(window, cx))),
            )
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
                    .tooltip("Save the displayed lines (.txt) or the raw bytes (.bin)")
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
        v_flex()
            .id("session-view")
            .key_context(context::SESSION_VIEW)
            .track_focus(&self.focus_handle)
            .on_key_up(cx.listener(Self::key_up))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::toggle_inline_action))
            .size_full()
            .bg(theme.background)
            .child(toolbar)
            .child(
                div()
                    .id("scrollback-area")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(self.terminal.clone()),
            )
            .when(self.mode == Mode::Command, |view| {
                view.child(self.compose.clone())
            })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Instant;

    use serialist_core::TransportError;

    use super::*;
    use crate::test_support::{
        FakeFeed, allow_engine_threads, displayed, fake_session, open_test_window, run_until,
    };

    fn open_session_view(
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<SessionView>, FakeFeed) {
        allow_engine_threads(cx);
        let (session, feed) = fake_session();
        let (window, view) = open_test_window(cx, move |window, cx| {
            SessionView::new(
                PortId::new("virtual:echo"),
                SerialConfig::default(),
                session,
                SessionOptions::default(),
                window,
                cx,
            )
        });
        (window, view, feed)
    }

    fn texts(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<(Direction, String)> {
        displayed(cx, view)
            .into_iter()
            .map(|line| (line.direction, line.text))
            .collect()
    }

    fn received(cx: &mut TestAppContext, view: &Entity<SessionView>) -> u64 {
        view.read_with(cx, |v, _| v.snapshot().raw_range().end)
    }

    #[gpui_test]
    fn a_burst_of_chunks_costs_a_few_wakes_and_one_repaint_each(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        let notifies = Rc::new(Cell::new(0));
        cx.update(|cx| {
            let notifies = notifies.clone();
            cx.observe(&view, move |_, _| notifies.set(notifies.get() + 1))
                .detach();
        });

        feed.connected("virtual:echo @ 115200 8N1");
        let mut sent = 0;
        for i in 0..200 {
            let chunk = format!("line {i}\r\n");
            sent += chunk.len() as u64;
            feed.data(chunk.as_bytes());
        }
        run_until(cx, "all 200 chunks shown", |cx| received(cx, &view) == sent);

        let lines = texts(cx, &view);
        assert_eq!(lines.len(), 201);
        assert_eq!(
            lines[0],
            (
                Direction::Notice,
                "Connected to virtual:echo @ 115200 8N1".into()
            )
        );
        assert_eq!(lines[200], (Direction::Rx, "line 199".into()));
        let wakes = view.read_with(cx, |v, _| v.ingest_stats().unwrap().wakes);
        assert!(
            wakes < 50,
            "{wakes} wakes for 201 events: the doorbell coalesces"
        );
        assert!(
            notifies.get() as u64 <= wakes + 1,
            "{} repaints for {wakes} wakes",
            notifies.get()
        );
        view.read_with(cx, |v, _| {
            assert_eq!(v.state(), &ConnectionState::Connected);
            assert_eq!(v.title(), "virtual:echo @ 115200 8N1");
            assert_eq!(v.stats().rx_chunks, 200);
        });

        // Nothing new: idle housekeeping must not repaint.
        let before = notifies.get();
        for _ in 0..3 {
            cx.executor().advance_clock(HOUSEKEEPING);
            cx.run_until_parked();
        }
        assert_eq!(notifies.get(), before);

        feed.data(b"more\n");
        run_until(cx, "the next chunk", |cx| received(cx, &view) == sent + 5);
        assert!(notifies.get() > before);
    }

    #[gpui_test]
    fn submitting_writes_and_echoes_before_the_reply(cx: &mut TestAppContext) {
        let (window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        run_until(cx, "the connect notice", |cx| texts(cx, &view).len() == 1);

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
        feed.data(b"OK\r\n");
        run_until(cx, "the reply", |cx| texts(cx, &view).len() == 3);

        assert_eq!(feed.written(), [b"AT\r\n".to_vec()]);
        assert_eq!(
            texts(cx, &view)[1..],
            [(Direction::Tx, "AT".into()), (Direction::Rx, "OK".into())]
        );
        view.read_with(cx, |view, cx| {
            assert_eq!(view.compose().read(cx).text(cx), "", "input clears");
        });
    }

    #[gpui_test]
    fn a_sent_line_ends_a_partial_received_line(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.data(b"prompt> ");
        run_until(cx, "the prompt", |cx| received(cx, &view) == 8);
        view.update(cx, |v, cx| v.send("ls", b"ls\r\n".to_vec(), cx));
        feed.data(b"file.txt\r\n");
        run_until(cx, "the listing", |cx| received(cx, &view) == 18);
        assert_eq!(
            texts(cx, &view),
            [
                (Direction::Rx, "prompt> ".into()),
                (Direction::Tx, "ls".into()),
                (Direction::Rx, "file.txt".into()),
            ]
        );
    }

    #[gpui_test]
    fn disconnect_closes_the_session_off_the_main_thread(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        run_until(cx, "the connect notice", |cx| texts(cx, &view).len() == 1);

        view.update(cx, |view, cx| view.disconnect(cx));
        run_until(cx, "the disconnect notice", |cx| {
            texts(cx, &view).len() == 2
        });
        assert!(feed.was_closed());
        view.read_with(cx, |view, _| {
            assert_eq!(view.state(), &ConnectionState::Disconnected { error: None });
            assert_eq!(view.open_port(), None);
        });
        let lines = texts(cx, &view);
        assert_eq!(
            lines
                .iter()
                .filter(|(_, text)| text == "Disconnected")
                .count(),
            1,
            "{lines:?}"
        );

        view.update(cx, |view, cx| {
            view.send("AT", b"AT\r\n".to_vec(), cx);
        });
        assert!(feed.written().is_empty());
        assert_eq!(
            view.read_with(cx, |v, _| v.notice().cloned()),
            Some(Notice::error("Not connected; not sent: AT"))
        );
    }

    #[gpui_test]
    fn losing_the_device_releases_the_session(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        feed.data(b"last words");
        feed.disconnected(Some(TransportError::Disconnected));
        run_until(cx, "the lost session", |cx| {
            view.read_with(cx, |v, _| v.state().is_disconnected())
        });
        run_until(cx, "the dead session's threads to be joined", |_| {
            feed.was_closed()
        });

        view.read_with(cx, |view, _| {
            assert_eq!(
                view.state(),
                &ConnectionState::Disconnected {
                    error: Some("device disconnected".into())
                }
            );
            assert_eq!(view.status_line().state, "Connection lost");
        });
        let lines = texts(cx, &view);
        assert_eq!(
            lines[lines.len() - 2..],
            [
                (Direction::Rx, "last words".into()),
                (
                    Direction::Notice,
                    "Disconnected: device disconnected".into()
                ),
            ]
        );
    }

    /// Panics on the first chunk it is given.
    struct Exploding;

    impl ChunkSink for Exploding {
        fn on_chunk(&mut self, _: &[u8], _: Instant) {
            panic!("sink exploded");
        }

        fn on_disconnect(&mut self) {}
    }

    #[gpui_test]
    fn a_panicked_ingest_thread_ends_the_session(cx: &mut TestAppContext) {
        allow_engine_threads(cx);
        let (session, feed) = fake_session();
        let (_window, view) = open_test_window(cx, move |window, cx| {
            SessionView::with_sinks(
                PortId::new("virtual:echo"),
                SerialConfig::default(),
                session,
                SessionOptions::default(),
                vec![Box::new(Exploding)],
                window,
                cx,
            )
        });
        feed.connected("virtual:echo");
        feed.data(b"boom\r\n");
        run_until(cx, "the session to end", |cx| {
            view.read_with(cx, |v, _| v.state().is_disconnected())
        });
        run_until(cx, "the session to close", |_| feed.was_closed());

        let expected = "the ingest thread panicked: sink exploded";
        view.read_with(cx, |v, _| {
            assert_eq!(
                v.state(),
                &ConnectionState::Disconnected {
                    error: Some(expected.into())
                }
            );
            assert_eq!(v.notice(), Some(&Notice::error(expected)));
            assert!(v.ingest_stats().is_none(), "the handle was joined");
        });
        assert_eq!(
            texts(cx, &view).last(),
            Some(&(Direction::Rx, "boom".into())),
            "what was stored before the panic stays on screen"
        );
    }

    #[gpui_test]
    fn clear_hides_the_scrollback_but_not_the_raw_stream(cx: &mut TestAppContext) {
        let (_window, view, feed) = open_session_view(cx);
        feed.data(b"one\ntwo\n");
        run_until(cx, "two lines", |cx| received(cx, &view) == 8);
        view.update(cx, |view, cx| view.pause(cx));
        view.update(cx, |view, cx| view.clear(cx));
        assert!(texts(cx, &view).is_empty());
        assert!(view.read_with(cx, |v, _| v.is_paused()), "still paused");

        view.update(cx, |view, cx| view.resume(cx));
        feed.data(b"three\n");
        run_until(cx, "the next line", |cx| received(cx, &view) == 14);
        assert_eq!(texts(cx, &view), [(Direction::Rx, "three".into())]);
        let raw = view.read_with(cx, |v, cx| v.export_job(ExportFormat::Raw, cx));
        let ExportJob::Raw { range, .. } = raw else {
            panic!("a raw job");
        };
        assert_eq!(range, 0..14, "raw export still has the cleared bytes");
    }
}
