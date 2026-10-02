//! The session view: one open port's scrollback in the terminal element, with pause,
//! export, recording and the compose bar.
//!
//! # Data path
//!
//! Received bytes never reach this thread. Opening the view spawns the session's ingest
//! thread ([`Ingest::spawn_with`]), which owns the page store: it appends every chunk,
//! turns the session's connect, disconnect and write-failure events into notice lines,
//! and hands every chunk to the [`RecordingSink`], which also flushes a recording when
//! the stream goes quiet ([`ChunkSink::on_idle`]), and to the codec slot (see "Codecs and
//! decoded frames" below). The view holds a [`Snapshot`] of the
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
//! 1. calls [`IngestHandle::acknowledge`] and the frame store's
//!    [`acknowledge`](FrameStoreReader::acknowledge), so anything published from now on
//!    rings again;
//! 2. takes the frame store's snapshot, then [`IngestHandle::snapshot`], which are
//!    therefore never older than the ring (frames first: a chunk is stored before it
//!    is decoded, so the store snapshot holds every frame's bytes), and handles the new
//!    frames (see "Codecs and decoded frames");
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
//! # In a background tab
//!
//! The workspace hides the views of the tabs that are not active
//! ([`SessionView::set_visible`]). A hidden view does not answer its doorbell: a ring
//! only marks it stale, and since nothing acknowledges the stores, the ingest thread
//! does not ring again. Everything behind the view carries on as before (the session,
//! ingest and the store, the codec slot and the frame store, the recording sink, the
//! script thread, pending expectations), but no snapshot is taken, no line is handed to
//! the terminal and nothing repaints. Only the housekeeping timer looks in: it reads
//! the counters and the link state (so a lost device still ends the session and stops
//! its script and recording), puts the summaries of newly decoded frames into the
//! scrollback, and notifies only when what the tab's label shows
//! ([`SessionView::tab_status`]: the dot and the bytes received since the tab was left)
//! changed. Showing the view again answers the missed ring at once: one snapshot of
//! everything that arrived meanwhile.
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
//!
//! # Scripts
//!
//! The ingest thread also rings a [`LineBell`](serialist_script::LineBell) through a
//! [`ScriptLink`](crate::script_bridge::ScriptLink) sink, from the moment the session
//! opens. Once the workspace calls [`SessionView::attach_scripts`], the view owns a
//! [`SessionScripts`]: a script thread whose `serial.current()` writes through the
//! session's [`SessionControl`](crate::session_handle::SessionControl) and reads this
//! store. [`SessionView::run_script`] queues a run (one runs at a time; the others wait
//! and the console says so); while any is queued a foreground task polls the runs once a
//! frame, turns their events into console lines ([`SessionViewEvent::Script`]), opens the
//! dialog a `ui.prompt` asks for, shows `ui.notify` in the status line and sends the
//! saved commands `commands.send` names. The status line says what runs and for how
//! long. Disconnecting stops every run and lets the script thread go, off the main
//! thread.
//!
//! # Codecs and decoded frames
//!
//! The ingest thread also runs the session's [`CodecSlotSink`], built on that thread
//! ([`Ingest::spawn_with`]), which decodes with the codec the view selects (see
//! [`codecs`](crate::codecs)): the device profile's `plugin` at opening, then the
//! toolbar's codec menu. Codecs are plugins the user installs, and none is by default:
//! a profile that names one that is not installed connects without a codec, the status
//! line says which plugin is missing and offers to install it, and the session starts
//! decoding with it once it is (see [`SessionView::want_codec`]). The codec menu shows
//! only while a plugin is installed (or the session decodes); it also offers the bundled
//! examples not installed yet and the plugins folder (see
//! [`plugin_files`](crate::plugin_files)). Decoded frames go to a
//! [`FrameStore`](serialist_core::FrameStore)
//! whose waker rings the same doorbell, so one wake acknowledges both stores, then takes
//! both snapshots. Per wake, with the new frames, the view:
//!
//! - puts a one-line summary of each new decoded frame into the scrollback as one batch
//!   of notice lines (`display.decoded_inline`), which the terminal draws in the plugin
//!   color;
//! - with `display.hide_framed_bytes`, hands the terminal a [`FilteredText`] in place of
//!   the snapshot, which leaves out the lines of binary frames (see
//!   [`framed`](crate::framed));
//! - bumps [`decoded_generation`](SessionView::decoded_generation), which the Decoded
//!   panel follows.
//!
//! A saved command whose payload names a codec is encoded by it (and is not sent while
//! that plugin is not installed: the status line says to install it), and one whose
//! `expect` is a frame predicate waits for a matching frame: a task polls the frame store
//! each frame from the id it had before the write, until a frame matches or the timeout,
//! and reports as a line match does.
//!
//! # VT mode
//!
//! A session is in monitor mode or VT mode ([`Emulation`]; see
//! [`emulation`](crate::emulation) for the plumbing). In VT mode the ingest thread also
//! feeds a terminal screen, and the view hands the terminal the screen's snapshot as its
//! text source instead of the store's lines, with the screen's size and cursor
//! ([`TerminalView::update_screen`]), a hook through which the element resizes the
//! screen to fit, and the store's text as the log search runs over. Per ring the view
//! acknowledges the screen with the stores, takes its snapshot, and acts on its events:
//! a title goes on the tab, a bell flashes the status dot, answers the ingest thread did
//! not write and color queries (answered from the palette) are written to the session.
//! Unmodified cursor keys follow the screen's cursor key mode, and a paste is bracketed
//! when the screen asks for it. The store goes on recording everything, so raw and text
//! export, search and recording are the same in both modes; export adds the screen's rows
//! ([`ExportFormat::Screen`]). Pause holds the screen snapshot on display; Clear hides the
//! log's lines and leaves the screen, which is the device's, alone.
//!
//! Switching to VT mode starts a fresh screen, fed from the next chunk: what arrived
//! before is not replayed into it. Switching back shows the store's lines again, which
//! kept growing all along.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{Map, Value as JsonValue};
use serialist_core::{
    ChunkSink, CodecFactory, CodecInfo, CodecRegistry, Command, CommandRef, ConnectionInfo,
    ControlLine, Direction, ExpectResult, Expectation, FrameId, FrameSnapshot, FrameStore,
    FrameStoreReader, Ingest, IngestHandle, IngestPanicked, IngestStats, LineEnding, LineId,
    LineSource, LinkState, ParamValues, Payload, PortId, ReplayAddress, ReplaySpeed, SearchMatch,
    Searcher, SerialConfig, SessionEvent, SessionStats, Snapshot, Store, StyledLine, TIMING_SUFFIX,
    TextOptions, Timestamps, TransportError,
};
use serialist_script::{ScriptOutcome, ScriptSource};
use serialist_vt::{DEFAULT_SCROLLBACK, VtScreen, VtSnapshot};

use crate::actions::{self, context};
use crate::capture::{Recorder, RecorderStats, RecordingSink, RecordingSlot};
use crate::chrome;
use crate::codecs::{
    CodecSelection, CodecSlotSink, FrameTime, NO_CODEC, encode_codec_command, fill_placeholders,
    frame_matches, hides_bytes, inline_summary, is_text_frame,
};
use crate::compose::{ComposeBar, ComposeEvent};
use crate::config::Config;
use crate::dialog_footer::DialogButtons;
use crate::emulation::{Emulation, VtSlot, VtState, effects_of};
use crate::export::{ExportFormat, ExportJob, FramesFormat, SharedSource};
use crate::framed::{FilteredText, FramedFilter};
use crate::history::PersistentHistory;
use crate::inline::{
    Echo, EncodedKey, EscapeChord, InlineConfig, KeyEncoder, Mode, PasteProgress, bracketed,
    is_chord, paste_bytes, paste_echo,
};
use crate::plugin_files;
use crate::port_settings::{PortSettings, PortSettingsEvent, PortSettingsForm};
use crate::prelude::*;
use crate::script_bridge::{
    ConsoleKind, ConsoleLine, GuiScriptSession, ScriptEffect, ScriptEnv, ScriptLinkParts,
    SessionScripts, drop_host,
};
use crate::script_console::ScriptPrompt;
use crate::scrollback::{Floors, Scrollback};
use crate::session_handle::SessionHandle;
use crate::session_options::SessionOptions;
use crate::status::{
    ConnectionState, LinkKind, Notice, NoticeAction, PauseMark, RateMeter, RecordingStatus,
    ScriptStatus, StatusInputs, StatusLine, file_name, format_bytes, replay_speed_label,
    replay_speed_of, replay_speeds, shows_line_settings,
};
use crate::tabs::{TabState, TabStatus};
use crate::terminal::view::MAX_MARKS;
use crate::terminal::{
    Clock, DisplayMode, ScreenState, TerminalScreen, TerminalView, TimestampMode, TimestampModeExt,
};
use crate::toolbar::{self, ToolbarItem, ToolbarLayout, ToolbarMetrics};

/// The shortest time between two snapshots: about a frame at 120 Hz.
pub const FRAME: Duration = Duration::from_millis(8);

/// Summary lines one wake puts into the scrollback at most; a flood of frames gets a
/// count for the rest.
pub const MAX_SUMMARIES_PER_WAKE: usize = 200;

/// Text lines a selected frame marks at most.
const MAX_FRAME_LINES: usize = 64;

/// The codec a session decodes with.
#[derive(Clone)]
pub struct ActiveCodec {
    /// Its name in the registry.
    pub name: String,
    pub info: CodecInfo,
    pub factory: Arc<dyn CodecFactory>,
}

impl std::fmt::Debug for ActiveCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveCodec")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// The codecs of the installed configuration: its plugins. None without one.
fn codec_registry(cx: &App) -> Arc<CodecRegistry> {
    cx.try_global::<Config>().map_or_else(
        || Arc::new(CodecRegistry::new()),
        |config| config.codec_registry().clone(),
    )
}

/// What the codec picker lists.
fn codec_choices(cx: &App) -> Vec<String> {
    cx.try_global::<Config>().map_or_else(
        || crate::codecs::CodecSet::default().choices(),
        |config| config.codecs().choices(),
    )
}

/// The received and local lines of `snapshot` that hold stream bytes `raw`, oldest first,
/// at most [`MAX_FRAME_LINES`]: the lines a frame is shown in. A frame with no bytes
/// names the line holding its offset.
fn lines_holding(snapshot: &Snapshot, raw: Range<u64>) -> Vec<StyledLine> {
    let (first, end) = (snapshot.first_line().0, snapshot.end().0);
    // The last line starting at or before the frame: line starts only grow.
    let (mut lo, mut hi) = (first, end);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match snapshot.line(LineId(mid)) {
            Some(line) if line.raw.start <= raw.start => lo = mid + 1,
            _ => hi = mid,
        }
    }
    let mut out = Vec::new();
    let mut id = lo.saturating_sub(1).max(first);
    while id < end && out.len() < MAX_FRAME_LINES {
        let Some(line) = snapshot.line(LineId(id)) else {
            break;
        };
        if line.raw.start > raw.end || (line.raw.start == raw.end && !raw.is_empty()) {
            break;
        }
        let holds = if raw.is_empty() {
            line.raw.start <= raw.start && raw.start <= line.raw.end
        } else {
            line.raw.start < raw.end && raw.start < line.raw.end
        };
        if line.direction == Direction::Rx && holds {
            out.push(line);
        }
        id += 1;
    }
    out
}

/// How waiting for a decoded frame ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameReply {
    Matched { id: FrameId, elapsed: Duration },
    TimedOut,
    Closed,
}

/// A mark over the whole of `line`, as a matched reply is marked.
fn whole_line_mark(line: &StyledLine) -> SearchMatch {
    SearchMatch {
        line: line.id,
        range: 0..line.text.len(),
    }
}

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
    /// Lines for the Script console: what the session's scripts printed, logged and
    /// asked, and how their runs went.
    Script(Vec<ConsoleLine>),
    /// The toolbar's Connect, on a closed session: open the port again, with the view's
    /// settings, into this view (see [`SessionView::reconnect`]).
    Reconnect,
    /// The replay speed menu's choice: close the session and open this port id, the
    /// replay's own with the new `?speed=`, into this view. The capture plays again from
    /// the start, in the same tab with the same scrollback.
    Reopen { port: PortId },
}

/// How long Send break holds the line.
pub const BREAK_DURATION: Duration = Duration::from_millis(250);

/// How long the status dot shows a bell (VT mode).
pub const BELL_FLASH: Duration = Duration::from_millis(250);

/// A screen's size before the terminal has drawn a frame to measure.
const DEFAULT_SCREEN: (usize, usize) = (80, 24);

/// A new VT screen of `columns` by `rows`, stamped in `epoch` (the store's, so the two
/// agree on times), fed by `slot` under `id` from the next chunk on. With `answer`, the
/// ingest thread answers the device's queries through the slot's writer.
fn install_screen(
    slot: &VtSlot,
    id: u64,
    (columns, rows): (usize, usize),
    epoch: serialist_core::Epoch,
    answer: bool,
) -> VtState {
    let screen = VtScreen::new(columns, rows, DEFAULT_SCROLLBACK).with_epoch(epoch);
    let sink = slot.sink(screen, answer);
    let handle = sink.handle();
    let snapshot = handle.snapshot();
    slot.install(id, sink);
    VtState {
        id,
        handle,
        snapshot,
    }
}

/// How long a reconfigure may take to be confirmed before the form says so.
const PORT_CHANGE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after a control-line change or a break (past its duration) a refusal is
/// still waited for.
const PORT_CHANGE_SETTLE: Duration = Duration::from_millis(150);

/// A change sent to the port whose outcome the port settings form waits for.
#[derive(Clone, Debug)]
enum PortChangeKind {
    Serial {
        from: SerialConfig,
        to: SerialConfig,
    },
    Control(ControlLine, bool),
    Break,
}

#[derive(Clone, Debug)]
struct PortChange {
    kind: PortChangeKind,
    /// The store's end when it was sent: a `Write failed` notice after it is the
    /// writer thread's refusal.
    mark: LineId,
    sent_at: Instant,
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
        Payload::Hex(_) | Payload::Codec { .. } | Payload::Script { .. } => bytes
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

/// Echo one key into the scrollback with local echo on: text grows the `Tx` line being
/// typed, Backspace takes its last character back, Enter ends it. Control keys and
/// escape sequences echo as nothing.
fn echo_key(ingest: &IngestHandle, echo: &Echo) {
    // Nothing to do about an ingest thread that has stopped: the session is over.
    let _ = match echo {
        Echo::Text(text) => ingest.append_local_inline(text.as_str(), Direction::Tx),
        Echo::Backspace => ingest.truncate_local_line(1),
        Echo::Enter => ingest.append_local_inline("\n", Direction::Tx),
        Echo::Nothing => Ok(()),
    };
}

/// An ingest thread started for a view, and the task answering its doorbell.
struct IngestStart {
    ingest: IngestHandle,
    frames: FrameStoreReader,
    wake: Task<()>,
}

/// Start an ingest thread taking `events` into `store`, through the view's recording
/// and script-link sinks, then `extra_sinks`, then the VT slot's sink (which feeds a
/// terminal screen in VT mode), then a codec slot decoding with `selection` into a new
/// frame store (from the store's current end, so frames name its stream offsets), and a
/// task answering its doorbell.
#[allow(clippy::too_many_arguments)]
fn start_ingest(
    events: Receiver<SessionEvent>,
    store: Store,
    recording: &RecordingSlot,
    script_link: &ScriptLinkParts,
    selection: &Arc<CodecSelection>,
    vt: &VtSlot,
    extra_sinks: Vec<Box<dyn ChunkSink + Send>>,
    cx: &mut Context<SessionView>,
) -> IngestStart {
    let (doorbell, rings) = async_channel::bounded::<()>(1);
    let mut sinks: Vec<Box<dyn ChunkSink + Send>> = vec![
        Box::new(RecordingSink::new(recording.clone())),
        Box::new(script_link.sink()),
    ];
    sinks.extend(extra_sinks);
    // The codec slot is made on the ingest thread, where a codec (a Lua VM) must
    // stay; its frames ring the same doorbell.
    let frame_store = FrameStore::default();
    let frames = frame_store.reader();
    let slot_selection = selection.clone();
    let frame_doorbell = doorbell.clone();
    let vt_slot = vt.clone();
    let vt_doorbell = doorbell.clone();
    let offset = store.stats().raw_len;
    let ingest = Ingest::spawn_with(
        events,
        store,
        Box::new(move || {
            let mut sinks: Vec<Box<dyn ChunkSink>> = sinks
                .into_iter()
                .map(|sink| -> Box<dyn ChunkSink> { sink })
                .collect();
            // The screen's wakes ring the same doorbell.
            sinks.push(Box::new(vt_slot.ingest_sink(move || {
                let _ = vt_doorbell.try_send(());
            })));
            sinks.push(Box::new(
                CodecSlotSink::new(
                    slot_selection,
                    frame_store,
                    Box::new(move || {
                        let _ = frame_doorbell.try_send(());
                    }),
                )
                .starting_at(offset),
            ));
            sinks
        }),
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
    IngestStart {
        ingest,
        frames,
        wake,
    }
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
    /// What the port is: a serial line, a TCP stream or a replayed capture.
    link: LinkKind,
    /// The line settings in force: those the port was opened with, then whatever the
    /// port settings changed them to. Connecting again in this view uses them.
    serial: SerialConfig,
    /// The settings the port was opened with, which the transport's description names.
    opened_serial: SerialConfig,
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
    /// The bell and name scripts wait on, fed by one of the ingest thread's sinks.
    script_link: ScriptLinkParts,
    /// This session's scripts, once the workspace has attached them.
    scripts: Option<SessionScripts>,
    /// The dialog a script's `ui.prompt` opened, while it is open.
    script_prompt: Option<Entity<ScriptPrompt>>,
    /// The task polling the runs is alive.
    script_polling: bool,
    _script_poll: Option<Task<()>>,
    /// The reader of the store the ingest thread's codec slot fills.
    frames: FrameStoreReader,
    /// The newest frames, taken on a wake.
    frame_snapshot: FrameSnapshot,
    /// Which codec the ingest thread runs; shared with its codec slot.
    selection: Arc<CodecSelection>,
    codec: Option<ActiveCodec>,
    /// A codec the device profile (or the restored tab) asked for whose plugin is not
    /// installed; selected once it is, unless a codec is picked first.
    wanted_codec: Option<String>,
    /// Bumped whenever the frames or the codec change, for the Decoded panel.
    decoded_generation: u64,
    /// `display.decoded_inline` for this session.
    decoded_inline: bool,
    /// The first frame whose summary has not gone into the scrollback.
    inline_next: FrameId,
    /// `display.hide_framed_bytes` for this session.
    hide_framed: bool,
    /// The view-id map while framed bytes are hidden (and a codec runs).
    filter: Option<FramedFilter>,
    /// The terminal's text source made from it on the last wake.
    filtered: Option<FilteredText>,
    /// Marked replies and the selected frame's lines, in store line ids; the terminal
    /// gets them in the ids of the source it shows.
    reply_marks: Vec<SearchMatch>,
    frame_marks: Vec<SearchMatch>,
    selected_frame: Option<FrameId>,
    /// The toolbar's width as the workspace lays it out (the center's), and as last
    /// measured; the hint wins, the measure serves a view with no workspace around it.
    width_hint: Option<Pixels>,
    measured_width: Option<Pixels>,
    /// What the toolbar showed and put in its overflow menu, as last laid out.
    toolbar_layout: ToolbarLayout,
    /// The labelled toolbar controls' widths as last drawn, for the next layout.
    tool_widths: ToolWidths,
    /// Byte rates for the status line, sampled by housekeeping.
    rates: RateMeter,
    /// The terminal's toggles as the toolbar last showed them.
    terminal_tools: TerminalTools,
    focus_handle: FocusHandle,
    /// On screen: its tab is the active one. A hidden view leaves its doorbell
    /// unanswered (see "In a background tab").
    visible: bool,
    /// A ring came while hidden.
    missed_wake: bool,
    /// RX bytes counted when the view was hidden, for what its tab label says arrived
    /// since.
    rx_seen: u64,
    /// The store the last ingest thread handed back when it ended: connecting again
    /// goes on filling it, so the scrollback stays.
    retired_store: Option<Store>,
    /// The last ingest thread has ended and been joined.
    ingest_joined: bool,
    /// What `attach_scripts` was given, to attach scripts again after a reconnect.
    script_env: Option<ScriptEnv>,
    /// A disconnect confirmation is open.
    pending_disconnect: bool,
    /// DTR and RTS as last set from here; opening a port asserts both.
    dtr: bool,
    rts: bool,
    /// Monitor or VT mode.
    emulation: Emulation,
    /// Where the ingest thread finds the screen to feed; shared with its VT slot sink.
    vt_slot: VtSlot,
    /// VT mode's screen, while it is on.
    vt: Option<VtState>,
    /// The id the last screen was installed under.
    next_vt: u64,
    /// The status dot shows a bell until this task ends.
    bell: Option<Task<()>>,
    /// The port settings popover's form.
    port_form: Entity<PortSettingsForm>,
    /// A change sent to the port, waiting for the writer thread's verdict.
    port_change: Option<PortChange>,
    _port_watch: Option<Task<()>>,
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
        extra_sinks: Vec<Box<dyn ChunkSink + Send>>,
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
            // Dropping a script host joins its thread once the script has stopped.
            if let Some(mut scripts) = this.scripts.take()
                && let (Some(host), _) = scripts.detach("the session closed")
            {
                cx.background_spawn(async move { drop_host(host) }).detach();
            }
        });

        let recording = RecordingSlot::default();
        let script_link = ScriptLinkParts::default();
        let selection = Arc::new(CodecSelection::default());
        let vt_slot = VtSlot::default();
        vt_slot.set_writer(session.control());
        let store = Store::new(options.store.clone());
        // A session that starts in VT mode feeds its screen from the first chunk, so the
        // screen is in the slot before the ingest thread starts.
        let initial_screen = (options.display.emulation == Emulation::Vt).then(|| {
            install_screen(
                &vt_slot,
                1,
                DEFAULT_SCREEN,
                store.epoch(),
                vt_slot.has_writer(),
            )
        });
        let IngestStart {
            ingest,
            frames,
            wake,
        } = start_ingest(
            session.events(),
            store,
            &recording,
            &script_link,
            &selection,
            &vt_slot,
            extra_sinks,
            cx,
        );
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

        // The toolbar shows the terminal's toggles, however they are flipped (its keys
        // too), but the terminal's other changes (new lines) are no reason to repaint.
        let terminal_changes = cx.observe(&terminal, |this, terminal, cx| {
            let tools = TerminalTools::of(terminal.read(cx));
            if tools != this.terminal_tools {
                this.terminal_tools = tools;
                cx.notify();
            }
        });
        // A plugin reload reaches the codec this session runs.
        let config_changes = cx.observe_global::<Config>(|this, cx| {
            this.codecs_changed(cx);
        });
        let initial_codec = options.codec.clone();
        let decoded_inline = options.display.decoded_inline;
        let hide_framed = options.display.hide_framed_bytes;
        let link = LinkKind::of(&port);
        let port_form = cx.new(|cx| {
            let form = PortSettingsForm::new(
                PortSettings::new(serial.clone(), options.line_ending, options.local_echo),
                true,
                window,
                cx,
            );
            // A stream with no serial line behind it has no rate, framing or control
            // lines to set.
            if link.has_line_settings() {
                form
            } else {
                form.without_line_settings()
            }
        });
        let port_settings_events = cx.subscribe_in(
            &port_form,
            window,
            |this, _, event: &PortSettingsEvent, window, cx| {
                this.port_settings_changed(event, window, cx);
            },
        );

        let mut view = Self {
            port,
            link,
            opened_serial: serial.clone(),
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
            script_link,
            scripts: None,
            script_prompt: None,
            script_polling: false,
            _script_poll: None,
            frame_snapshot: frames.snapshot(),
            frames,
            selection,
            codec: None,
            wanted_codec: None,
            decoded_generation: 0,
            decoded_inline,
            inline_next: FrameId::ZERO,
            hide_framed,
            filter: None,
            filtered: None,
            reply_marks: Vec::new(),
            frame_marks: Vec::new(),
            selected_frame: None,
            width_hint: None,
            measured_width: None,
            toolbar_layout: ToolbarLayout::default(),
            tool_widths: ToolWidths::default(),
            rates: RateMeter::default(),
            terminal_tools: TerminalTools::default(),
            focus_handle: cx.focus_handle(),
            visible: true,
            missed_wake: false,
            rx_seen: 0,
            retired_store: None,
            ingest_joined: false,
            script_env: None,
            pending_disconnect: false,
            dtr: true,
            rts: true,
            emulation: Emulation::Monitor,
            vt_slot,
            vt: None,
            next_vt: 1,
            bell: None,
            port_form,
            port_change: None,
            _port_watch: None,
            _wake: wake,
            _housekeeping: housekeeping,
            _subscriptions: vec![
                compose_events,
                release,
                interceptor,
                terminal_changes,
                config_changes,
                port_settings_events,
            ],
        };
        // The device profile's codec, from the first chunk on, if its plugin is installed.
        if let Some(name) = initial_codec {
            view.want_codec(&name, cx);
        }
        if let Some(screen) = initial_screen {
            view.emulation = Emulation::Vt;
            view.vt = Some(screen);
            view.show_screen(cx);
        }
        view
    }

    // --- Reading the view ------------------------------------------------------------

    pub fn port(&self) -> &PortId {
        &self.port
    }

    /// The line settings in force (those the port was opened with, then whatever the
    /// port settings changed them to), which connecting again in this view uses.
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

    /// Say `notice` in the status line, in place of the last one.
    pub fn set_notice(&mut self, notice: Notice, cx: &mut Context<Self>) {
        self.notice = Some(notice);
        cx.notify();
    }

    /// Do what the notice's button says (see [`NoticeAction`]), and say how it went.
    pub fn run_notice_action(&mut self, action: &NoticeAction, cx: &mut Context<Self>) {
        if let Some(notice) = plugin_files::run_notice_action(action, cx) {
            self.set_notice(notice, cx);
        }
    }

    /// Install the bundled example plugin `name` (see
    /// [`plugin_files::install_example`]), and say how it went. The watcher loads it; if
    /// this session was waiting for it, it starts decoding with it then.
    pub fn install_example_plugin(&mut self, name: &str, cx: &mut Context<Self>) {
        let notice = plugin_files::install_example(name, cx);
        self.set_notice(notice, cx);
    }

    /// The codec a device profile (or a restored tab) asked for whose plugin is not
    /// installed yet.
    pub fn wanted_codec(&self) -> Option<&str> {
        self.wanted_codec.as_deref()
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
    ///
    /// A description that ends with the settings the port was opened with names the
    /// settings in force instead, once the port settings changed them.
    pub fn title(&self) -> String {
        match &self.connection.description {
            Some(description) if self.serial != self.opened_serial => description
                .strip_suffix(&self.opened_serial.summary())
                .map_or_else(
                    || description.clone(),
                    |prefix| format!("{prefix}{}", self.serial.summary()),
                ),
            Some(description) => description.clone(),
            None if self.link.has_line_settings() => {
                format!("{} @ {}", self.port, self.serial.summary())
            }
            // No line settings to name: just the endpoint or the capture.
            None => self.port.to_string(),
        }
    }

    /// What kind of port this session is on.
    pub fn link(&self) -> LinkKind {
        self.link
    }

    /// What the toolbar's settings button says: the line settings of a serial port,
    /// `TCP` for a TCP stream, the speed of a replay (`1x`, `4x`, `max`).
    pub fn connection_label(&self) -> String {
        match self.link {
            LinkKind::Serial => self.serial.summary(),
            LinkKind::Tcp => "TCP".to_owned(),
            LinkKind::Replay => {
                replay_speed_label(&self.port, self.connection.description.as_deref())
            }
        }
    }

    /// Where the link stands, as ingest last reported it.
    pub fn connection(&self) -> &ConnectionInfo {
        &self.connection
    }

    /// Whether `error`, a link's end, is a replay reaching its last byte: the transport
    /// reports it as the device going away, and the status line should not call it a
    /// lost connection.
    fn is_replay_end(&self, error: &str) -> bool {
        self.link == LinkKind::Replay && error == TransportError::Disconnected.to_string()
    }

    /// The speed this replay plays at, once its description or its id says. `None` for a
    /// port that is not a replay.
    pub fn replay_speed(&self) -> Option<ReplaySpeed> {
        (self.link == LinkKind::Replay)
            .then(|| replay_speed_of(&self.port, self.connection.description.as_deref()))
            .flatten()
    }

    /// Play this replay again from the start at `speed`: the workspace closes the session
    /// and opens the replay's id with `?speed=` set to it, into this view (see
    /// [`SessionViewEvent::Reopen`]). The replay menu's items call it. Nothing happens
    /// for a port that is not a replay.
    pub fn choose_replay_speed(&mut self, speed: ReplaySpeed, cx: &mut Context<Self>) {
        if self.link != LinkKind::Replay {
            return;
        }
        let Ok(address) = ReplayAddress::from_port_id(&self.port) else {
            return;
        };
        let port = address.with_speed(speed).port_id();
        cx.emit(SessionViewEvent::Reopen { port });
    }

    /// Name another port of the same kind as the one this view is about, before a
    /// reconnect opens it (a replay at a new speed is a new id). Call right before
    /// [`Self::reconnect`].
    pub fn retarget(&mut self, port: PortId) {
        debug_assert_eq!(
            LinkKind::of(&port),
            self.link,
            "a view keeps its kind of port"
        );
        self.port = port;
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
        let script = self.script_status();
        let codec = self.codec_name();
        StatusLine::new(StatusInputs {
            state: &self.state,
            title: self.title(),
            settings: shows_line_settings(self.link, self.connection.description.as_deref())
                .then(|| self.serial.summary()),
            session: self.stats,
            store: self.snapshot().stats(),
            paused: self.pause,
            recording: self.recording_status.as_ref(),
            notice: self.notice.as_ref(),
            mode: self.mode,
            paste: self.paste_progress(),
            script: script.as_ref(),
            codec,
            rates: self.rates.rates(),
            emulation: self.emulation,
        })
    }

    pub fn focus_compose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.compose
            .update(cx, |compose, cx| compose.focus(window, cx));
    }

    /// Focus where typing goes in the current mode: the compose bar, or the terminal in
    /// inline mode.
    pub fn focus_default(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.mode {
            Mode::Command => self.focus_compose(window, cx),
            Mode::Inline => {
                let focus = self.terminal.focus_handle(cx);
                window.focus(&focus, cx);
            }
        }
    }

    // --- Tabs ------------------------------------------------------------------------

    /// Whether the view is on screen (its tab is the active one).
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// Put the view on screen or take it off (see "In a background tab"). Showing it
    /// answers a ring missed meanwhile with one snapshot of everything that arrived.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.missed_wake = false;
            self.refresh(cx);
            cx.notify();
        } else {
            self.poll_session(cx);
            self.rx_seen = self.stats.rx_bytes;
        }
        tracing::debug!(port = %self.port, visible, "session view");
    }

    /// Whether a ring came while the view was hidden and has not been answered.
    pub fn missed_wake(&self) -> bool {
        self.missed_wake
    }

    /// What the view's tab shows: the dot (recording over paused over connected), and
    /// the bytes received since the tab was left.
    pub fn tab_status(&self) -> TabStatus {
        let state = match &self.state {
            ConnectionState::Disconnected { error: None } => TabState::Disconnected,
            ConnectionState::Disconnected { error: Some(_) } => TabState::Lost,
            ConnectionState::Connected if self.recording_status.is_some() => TabState::Recording,
            ConnectionState::Connected if self.pause.is_some() => TabState::Paused,
            ConnectionState::Connected => TabState::Connected,
        };
        let unseen_bytes = if self.visible {
            0
        } else {
            self.stats.rx_bytes.saturating_sub(self.rx_seen)
        };
        TabStatus {
            state,
            unseen_bytes,
        }
    }

    /// The store's newest snapshot, taken now: what a hidden view would show if it were
    /// on screen. `None` once the ingest thread is gone.
    pub fn latest_snapshot(&self) -> Option<Snapshot> {
        self.ingest.as_ref().map(IngestHandle::snapshot)
    }

    // --- The data path ---------------------------------------------------------------

    /// One ring of the doorbell. On screen: [`Self::refresh`]. Hidden: remember it and
    /// leave the stores unacknowledged, so the ingest thread rings no more until the
    /// view is shown.
    fn wake(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.missed_wake = true;
            return;
        }
        self.refresh(cx);
    }

    /// Acknowledge both stores, then snapshot both, then show. The frames are taken
    /// first: a chunk is stored before it is decoded, so the store snapshot taken after
    /// holds the bytes of every frame in it.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(ingest) = &self.ingest else {
            return;
        };
        ingest.acknowledge();
        self.frames.acknowledge();
        if let Some(vt) = &self.vt {
            vt.handle.acknowledge();
        }
        let frames = self.frames.snapshot();
        let snapshot = ingest.snapshot();
        let decoded = self.take_frames(frames);
        let screen = self.take_screen(cx);
        let shown = self.show(snapshot, decoded, screen, cx);
        if self.poll_session(cx) || shown || decoded {
            cx.notify();
        }
    }

    /// VT mode: take the screen's newest snapshot (unless paused, which holds the one on
    /// display) and act on its events. Returns whether the snapshot changed.
    fn take_screen(&mut self, cx: &mut Context<Self>) -> bool {
        let paused = self.pause.is_some();
        let Some(vt) = &mut self.vt else {
            return false;
        };
        let events = vt.handle.take_events();
        let mut changed = false;
        if !paused {
            let latest = vt.handle.snapshot();
            if latest.generation() != vt.snapshot.generation() {
                vt.snapshot = latest;
                changed = true;
            }
        }
        if !events.is_empty() {
            let palette = self.terminal.read(cx).palette().clone();
            let effects = effects_of(events, &palette);
            if effects.title.is_some() {
                // The tab reads the title from the snapshot on display.
                cx.notify();
            }
            if let Some(session) = &self.session {
                for bytes in effects.writes {
                    if session.write(bytes).is_err() {
                        tracing::debug!(port = %self.port, "an answer to the device was not sent");
                    }
                }
            }
            if effects.bell {
                self.ring_bell(cx);
            }
        }
        changed
    }

    /// Flash the status dot once for the device's bell.
    fn ring_bell(&mut self, cx: &mut Context<Self>) {
        // A bell during a flash starts it over.
        self.bell = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BELL_FLASH).await;
            this.update(cx, |view, cx| {
                view.bell = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Take a newer frame snapshot, and put the summaries of the new frames into the
    /// scrollback as one batch. Returns whether the frames changed.
    fn take_frames(&mut self, frames: FrameSnapshot) -> bool {
        let (before, after) = (self.frame_snapshot.stats(), frames.stats());
        if before == after {
            return false;
        }
        self.frame_snapshot = frames;
        self.decoded_generation += 1;
        let new = self.inline_next.max(after.first)..after.end;
        self.inline_next = after.end;
        if let Some(last) = self.frame_snapshot.last() {
            tracing::debug!(
                port = %self.port,
                codec = self.codec_name().unwrap_or(NO_CODEC),
                new = new.end.0 - new.start.0,
                total = after.end.0,
                last = %last.summary,
                "decoded frames"
            );
        }
        if self.decoded_inline {
            self.put_summaries(new);
        }
        true
    }

    /// One notice line per decoded frame in `ids` (text frames aside: their text is on
    /// screen), appended in one batch.
    fn put_summaries(&mut self, ids: Range<FrameId>) {
        let (Some(ingest), Some(codec)) = (&self.ingest, &self.codec) else {
            return;
        };
        let mut lines = Vec::new();
        let mut more = 0;
        for (_, frame) in self.frame_snapshot.iter(ids) {
            if is_text_frame(frame, Some(&codec.info)) {
                continue;
            }
            if lines.len() < MAX_SUMMARIES_PER_WAKE {
                lines.push(inline_summary(frame));
            } else {
                more += 1;
            }
        }
        if more > 0 {
            lines.push(format!(
                "{}\u{2026} and {more} more frames",
                crate::codecs::DECODED_MARK
            ));
        }
        if !lines.is_empty() {
            let _ = ingest.append_local(lines.join("\n"), Direction::Notice);
        }
    }

    /// The ingest thread has ended: after the session's events ran out (the session is
    /// gone), or because it panicked, which takes its store with it. Show what it
    /// published last, then join it off the main thread to learn which.
    fn ingest_ended(&mut self, cx: &mut Context<Self>) {
        self.refresh(cx);
        let Some(ingest) = self.ingest.take() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let joined = cx.background_spawn(async move { ingest.join() }).await;
            this.update(cx, |view, cx| view.ingest_joined(joined, cx))
                .ok();
        })
        .detach();
    }

    /// The ingest thread was joined: keep the store it hands back for a reconnect, or
    /// end the session if it panicked.
    fn ingest_joined(&mut self, joined: Result<Store, IngestPanicked>, cx: &mut Context<Self>) {
        self.ingest_joined = true;
        let error = match joined {
            Ok(store) => {
                self.retired_store = Some(store);
                return;
            }
            Err(error) => error,
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

    /// Hand `snapshot` to the terminal if it holds anything new, or, while framed bytes
    /// are hidden, if the frames (`decoded`) changed what is hidden, or if the screen
    /// (`screen`, VT mode) changed. Returns whether it did.
    fn show(
        &mut self,
        snapshot: Snapshot,
        decoded: bool,
        screen: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        // A snapshot of what the view already has: a wake that found nothing new. (Not
        // the stats, which cannot see a line being typed change without changing size.)
        let same = self.snapshot().is_same_publication(&snapshot);
        if same && !(decoded && self.filter.is_some()) {
            if screen {
                // Only the screen moved (a resize, a synchronized update let go).
                self.push_sources(cx, |_, cx| cx.notify());
            }
            return screen;
        }
        let after = snapshot.stats();
        // The lines that were still open (the one arriving, the ones being typed) may
        // have changed, so they count as changed along with the new ones.
        let changed = self.snapshot().committed_end().max(after.first_line)..after.end_line;
        self.scrollback = Scrollback::with_hex_row(&snapshot, self.floors, self.hex_bytes_per_row);
        self.refilter();
        let changed = match &self.filtered {
            Some(filtered) => filtered.view_at_or_after(changed.start)..filtered.end(),
            None => changed,
        };
        self.push_sources(cx, |terminal, cx| terminal.lines_appended(changed, cx));
        if self.filtered.is_some() {
            // What was marked may have moved in the view's ids.
            self.sync_marks(cx);
        }
        true
    }

    /// Bring the hidden-lines map up to the current snapshot and frames, if framed bytes
    /// are hidden and a codec runs.
    fn refilter(&mut self) {
        self.filtered = match (self.filter.as_mut(), self.codec.as_ref()) {
            (Some(filter), Some(codec)) => {
                let info = &codec.info;
                Some(
                    filter.update(&self.scrollback.text, &self.frame_snapshot, &|frame| {
                        hides_bytes(frame, Some(info))
                    }),
                )
            }
            _ => None,
        };
    }

    /// What the terminal shows as text: the screen in VT mode, else the log.
    pub fn text_source(&self) -> Arc<dyn LineSource> {
        match &self.vt {
            Some(vt) => vt.snapshot.clone(),
            None => self.log_source(),
        }
    }

    /// The store's lines as the terminal shows them in monitor mode: the scrollback, with
    /// framed bytes left out while they are hidden. What search reads in both modes.
    pub fn log_source(&self) -> Arc<dyn LineSource> {
        match &self.filtered {
            Some(filtered) => Arc::new(filtered.clone()),
            None => self.scrollback.text_source(),
        }
    }

    fn text_searcher(&self) -> Arc<dyn Searcher> {
        match &self.filtered {
            Some(filtered) => Arc::new(filtered.clone()),
            None => self.scrollback.text_searcher(),
        }
    }

    /// Give the terminal the current scrollback's sources (and the screen's, in VT mode),
    /// then run `then` on it.
    fn push_sources(
        &self,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>),
    ) {
        let scrollback = self.scrollback.clone();
        let (text, searcher) = (self.text_source(), self.text_searcher());
        let log = self.vt.is_some().then(|| self.log_source());
        let screen = self.screen_state();
        self.terminal.update(cx, |terminal, cx| {
            terminal.set_log(log);
            if let Some(screen) = screen {
                terminal.update_screen(screen, cx);
            }
            terminal.update_sources(
                text,
                Some(searcher),
                Some(scrollback.hex_source()),
                Some(scrollback.hex_searcher()),
                cx,
            );
            then(terminal, cx);
        });
    }

    /// Give the terminal a text source whose ids mean something else (framed bytes
    /// hidden or shown again, VT mode on or off): its scroll and selection start over,
    /// and the marks are given again in the new ids.
    fn replace_text_source(&mut self, cx: &mut Context<Self>) {
        let (text, searcher) = (self.text_source(), self.text_searcher());
        let log = self.vt.is_some().then(|| self.log_source());
        self.terminal.update(cx, |terminal, cx| {
            // The log first: a search that reruns on the new source reads it.
            terminal.set_log(log);
            terminal.set_searcher(Some(searcher), cx);
            terminal.set_source(text, cx);
        });
        self.sync_marks(cx);
    }

    // --- VT mode -----------------------------------------------------------------------

    /// Monitor or VT mode.
    pub fn emulation(&self) -> Emulation {
        self.emulation
    }

    /// The screen snapshot on display, in VT mode.
    pub fn vt_snapshot(&self) -> Option<&Arc<VtSnapshot>> {
        self.vt.as_ref().map(|vt| &vt.snapshot)
    }

    /// The screen's handle, in VT mode.
    pub fn vt_handle(&self) -> Option<&serialist_vt::VtHandle> {
        self.vt.as_ref().map(|vt| &vt.handle)
    }

    /// The title the device set on its screen (VT mode), as of the snapshot on display,
    /// for the tab.
    pub fn screen_title(&self) -> Option<&str> {
        self.vt.as_ref()?.snapshot.title()
    }

    /// Whether the status dot is showing a bell.
    pub fn bell_flashing(&self) -> bool {
        self.bell.is_some()
    }

    /// The toggle: the toolbar's terminal button and `terminal::ToggleEmulation`.
    pub fn toggle_emulation(&mut self, cx: &mut Context<Self>) {
        self.set_emulation(self.emulation.toggled(), cx);
    }

    /// Show the store's lines (monitor) or a terminal screen (VT). A new screen starts
    /// blank, sized to the terminal, and is fed from the next chunk: what arrived before
    /// is not replayed. The store keeps everything either way.
    pub fn set_emulation(&mut self, emulation: Emulation, cx: &mut Context<Self>) {
        if self.emulation == emulation {
            return;
        }
        self.emulation = emulation;
        match emulation {
            Emulation::Vt => {
                let size = self.terminal.read(cx).grid_size().unwrap_or(DEFAULT_SCREEN);
                self.next_vt += 1;
                let epoch = self.snapshot().epoch();
                self.vt = Some(install_screen(
                    &self.vt_slot,
                    self.next_vt,
                    size,
                    epoch,
                    self.vt_slot.has_writer(),
                ));
                self.show_screen(cx);
            }
            Emulation::Monitor => {
                if let Some(vt) = self.vt.take()
                    && let Some(sink) = self.vt_slot.take(vt.id)
                {
                    // A screen with its scrollback is a lot to free on the main thread.
                    cx.background_spawn(async move { drop((sink, vt)) })
                        .detach();
                }
                self.bell = None;
                self.terminal
                    .update(cx, |terminal, cx| terminal.set_screen(None, cx));
                self.replace_text_source(cx);
            }
        }
        tracing::debug!(port = %self.port, emulation = emulation.label(), "emulation");
        cx.notify();
    }

    /// The size and cursor of the screen snapshot on display, in VT mode.
    fn screen_state(&self) -> Option<ScreenState> {
        let snapshot = &self.vt.as_ref()?.snapshot;
        Some(ScreenState {
            columns: snapshot.columns(),
            rows: snapshot.viewport_rows(),
            cursor: snapshot.cursor().filter(|cursor| cursor.visible),
        })
    }

    /// Put the screen on the terminal: its snapshot as the text source, the log to
    /// search, and the hook the element resizes it through.
    fn show_screen(&mut self, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let screen = TerminalScreen {
            resize: std::rc::Rc::new(move |columns, rows, cx: &mut App| {
                view.update(cx, |view, cx| view.resize_screen(columns, rows, cx))
                    .ok();
            }),
        };
        let state = self.screen_state();
        self.terminal.update(cx, |terminal, cx| {
            terminal.set_screen(Some(screen), cx);
            if let Some(state) = state {
                terminal.update_screen(state, cx);
            }
        });
        self.replace_text_source(cx);
    }

    /// The element fits `columns` by `rows` cells: resize the screen to that (the device
    /// learns it when it asks, `CSI 18 t`) and show the result.
    pub fn resize_screen(&mut self, columns: usize, rows: usize, cx: &mut Context<Self>) {
        let paused = self.pause.is_some();
        let Some(vt) = &mut self.vt else {
            return;
        };
        vt.handle.resize(columns, rows);
        tracing::debug!(port = %self.port, columns, rows, "screen resized");
        if !paused {
            vt.snapshot = vt.handle.snapshot();
            self.push_sources(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    /// Hand the terminal the marked replies and the selected frame's lines, in the ids of
    /// the text source it shows. Lines that are hidden have no mark.
    fn sync_marks(&mut self, cx: &mut Context<Self>) {
        let marks: Vec<SearchMatch> = self
            .reply_marks
            .iter()
            .chain(&self.frame_marks)
            .filter_map(|mark| {
                let line = match &self.filtered {
                    Some(filtered) => filtered.view_of(mark.line)?,
                    None => mark.line,
                };
                Some(SearchMatch {
                    line,
                    range: mark.range.clone(),
                })
            })
            .collect();
        self.terminal
            .update(cx, |terminal, cx| terminal.set_marks(marks, cx));
    }

    /// Mark a reply (a matched line, a matched frame's lines), keeping the newest.
    fn mark_reply(&mut self, marks: impl IntoIterator<Item = SearchMatch>, cx: &mut Context<Self>) {
        self.reply_marks.extend(marks);
        if self.reply_marks.len() > MAX_MARKS {
            let excess = self.reply_marks.len() - MAX_MARKS;
            self.reply_marks.drain(..excess);
        }
        self.sync_marks(cx);
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
            // The link went away by itself (a local disconnect sets the state first). A
            // replay that plays its last byte ends this way by design, which is not a lost
            // connection.
            let error = error.clone().filter(|error| !self.is_replay_end(error));
            tracing::info!(port = %self.port, ?error, "session ended");
            self.state = ConnectionState::Disconnected { error };
            self.close_session(cx);
            changed = true;
        }
        changed
    }

    /// The rates as the status line words them, to repaint only when that changes.
    fn status_rates(&self) -> (Option<String>, Option<String>) {
        let rates = self.rates.rates();
        (
            crate::status::format_rate(rates.rx),
            crate::status::format_rate(rates.tx),
        )
    }

    fn housekeeping(&mut self, cx: &mut Context<Self>) {
        if !self.visible {
            self.background_housekeeping(cx);
            return;
        }
        // A running script's time in the status line moves on its own.
        let mut changed = self.poll_session(cx) | self.script_status().is_some();
        // So do the byte rates, until they settle to nothing.
        let before = self.status_rates();
        let now = cx.background_executor().now();
        self.rates
            .record(now, self.stats.rx_bytes, self.stats.tx_bytes);
        changed |= self.status_rates() != before;
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

    /// Housekeeping while hidden: read the counters and the link state, put the
    /// summaries of new frames into the scrollback, and notify only if the tab's label
    /// changed. No snapshot is taken and nothing reaches the terminal.
    fn background_housekeeping(&mut self, cx: &mut Context<Self>) {
        let before = self.tab_status();
        self.poll_session(cx);
        if self.decoded_inline && self.codec.is_some() {
            // Without acknowledging the frame store: the doorbell stays quiet.
            let frames = self.frames.snapshot();
            self.take_frames(frames);
        }
        if self.tab_status() != before {
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
        let registry = codec_registry(cx);
        // A codec payload is the codec's to encode; the rest the command's own. One whose
        // plugin is not installed is not sent, and the notice offers to install it.
        let encoded = match &command.payload {
            Payload::Codec { .. } => encode_codec_command(&registry, command, params),
            _ => command
                .encode(params, session_eol)
                .map_err(|error| error.to_string()),
        };
        let bytes = match encoded {
            Ok(bytes) => bytes,
            Err(error) => {
                let text = format!("{name}: {error}");
                self.notice = Some(match &command.payload {
                    Payload::Codec { codec, .. } if !registry.contains(codec) => {
                        plugin_files::missing_plugin_notice(text, codec)
                    }
                    _ => Notice::error(text),
                });
                cx.notify();
                return;
            }
        };
        // A frame predicate is waited for while a codec decodes the session; its strings
        // take the parameters as the payload does.
        let frame_expect = match command.expect.as_ref() {
            Some(expect) if self.codec.is_some() => match &expect.frame {
                Some(predicate) => match fill_placeholders(predicate, command, params) {
                    Ok(predicate) => Some((predicate, expect.timeout_ms)),
                    Err(error) => {
                        self.notice =
                            Some(Notice::error(format!("{name}: expected frame: {error}")));
                        cx.notify();
                        return;
                    }
                },
                None => None,
            },
            _ => None,
        };
        let line_expect = command
            .expect
            .as_ref()
            .filter(|expect| frame_expect.is_none() && !expect.pattern.is_empty());
        let needs_codec = frame_expect.is_none()
            && line_expect.is_none()
            && command.expect.as_ref().is_some_and(|e| e.frame.is_some());
        let Some((session, ingest)) = self.live() else {
            self.notice = Some(Notice::error(format!("Not connected; not sent: {name}")));
            cx.notify();
            return;
        };
        // A saved command is always echoed: it is the record of what was sent.
        let _ = ingest.append_local(command_echo(command, &bytes, session_eol), Direction::Tx);
        // Registered before the write, so the reply cannot arrive unwatched: a line
        // expectation with the matchers, a frame one as the frame store's end now.
        let expectation = line_expect.map(|expect| {
            ingest
                .matchers()
                .expect(&expect.pattern, expect.timeout())
                .map(|expectation| (expectation, expect.timeout_ms))
        });
        let frames_from = self.frames.stats().end;
        let sent_at = Instant::now();
        if session.write(bytes).is_err() {
            let _ = ingest.append_local("Not sent: the session closed", Direction::Notice);
        }
        tracing::debug!(command = %reference, "sent saved command");
        match (expectation, frame_expect) {
            (Some(Ok((expectation, timeout_ms))), _) => {
                self.await_reply(name, expectation, timeout_ms, cx);
            }
            (Some(Err(error)), _) => {
                self.notice = Some(Notice::error(format!(
                    "{name}: the expected reply is not a valid pattern: {error}"
                )));
            }
            (None, Some((predicate, timeout_ms))) => {
                self.await_frame(name, predicate, frames_from, sent_at, timeout_ms, cx);
            }
            (None, None) if needs_codec => {
                let why = if registry.is_empty() {
                    "no codec plugin is installed to decode it"
                } else {
                    "no codec is decoding"
                };
                self.notice = Some(Notice::error(format!(
                    "Sent {name}; its reply is a decoded frame, and {why}"
                )));
            }
            (None, None) => self.notice = Some(Notice::info(format!("Sent {name}"))),
        }
        cx.notify();
    }

    /// Wait for a decoded frame matching `predicate`, from frame `from` on, until
    /// `timeout_ms` after `sent_at`: a task reads the frame store once a frame (it may,
    /// from any thread) and reports as a line reply does.
    fn await_frame(
        &mut self,
        name: String,
        predicate: Map<String, JsonValue>,
        from: FrameId,
        sent_at: Instant,
        timeout_ms: u64,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(Notice::info(format!("{name}: waiting for a reply…")));
        let reader = self.frames.clone();
        let deadline = sent_at + Duration::from_millis(timeout_ms);
        cx.spawn(async move |this, cx| {
            let mut next = from;
            loop {
                let frames = reader.snapshot();
                let found = frames
                    .iter(next..frames.end())
                    .find(|(_, frame)| frame_matches(&predicate, frame))
                    .map(|(id, frame)| (id, frame.at.saturating_duration_since(sent_at)));
                next = next.max(frames.end());
                let outcome = match found {
                    Some((id, elapsed)) => Some(FrameReply::Matched { id, elapsed }),
                    None => {
                        let ended = this
                            .read_with(cx, |view, _| view.state.is_disconnected())
                            .unwrap_or(true);
                        if ended {
                            Some(FrameReply::Closed)
                        } else if Instant::now() >= deadline {
                            Some(FrameReply::TimedOut)
                        } else {
                            None
                        }
                    }
                };
                if let Some(outcome) = outcome {
                    this.update(cx, |view, cx| {
                        view.frame_reply(&name, timeout_ms, outcome, cx);
                    })
                    .ok();
                    return;
                }
                cx.background_executor().timer(EXPECT_POLL).await;
            }
        })
        .detach();
    }

    fn frame_reply(
        &mut self,
        name: &str,
        timeout_ms: u64,
        reply: FrameReply,
        cx: &mut Context<Self>,
    ) {
        match reply {
            FrameReply::Matched { id, elapsed } => {
                // The frame may be newer than the view's snapshots; the store has its
                // bytes, which were stored before they were decoded.
                let frames = self.frames.snapshot();
                let snapshot = self.ingest.as_ref().map(IngestHandle::snapshot);
                if let (Some(frame), Some(snapshot)) = (frames.get(id), snapshot) {
                    let lines = lines_holding(&snapshot, frame.raw.clone());
                    self.mark_reply(lines.iter().map(whole_line_mark), cx);
                }
                self.notice = Some(Notice::info(format!(
                    "{name}: OK in {} ms",
                    elapsed.as_millis()
                )));
            }
            FrameReply::TimedOut => {
                let message = format!("{name}: no response within {timeout_ms} ms");
                if let Some(ingest) = &self.ingest {
                    let _ = ingest.append_local(message.clone(), Direction::Notice);
                }
                self.notice = Some(Notice::error(message));
            }
            FrameReply::Closed => {
                self.notice = Some(Notice::info(format!(
                    "{name}: the session ended before a reply"
                )));
            }
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
                self.mark_reply([SearchMatch { line, range }], cx);
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

    // --- Codecs and decoded frames ----------------------------------------------------

    /// The codec decoding the session, if any.
    pub fn codec(&self) -> Option<&ActiveCodec> {
        self.codec.as_ref()
    }

    /// The name of the codec decoding the session, if any.
    pub fn codec_name(&self) -> Option<&str> {
        self.codec.as_ref().map(|codec| codec.name.as_str())
    }

    /// Decode with the codec the registry calls `name` from the next chunk on, or with
    /// none (`None` or [`NO_CODEC`]). The frames decoded so far stay. Returns whether the
    /// codec is known; an unknown one leaves decoding as it was and says so.
    pub fn set_codec(&mut self, name: Option<&str>, cx: &mut Context<Self>) -> bool {
        let name = name.filter(|name| !name.is_empty() && *name != NO_CODEC);
        // A choice made now outranks one waiting for its plugin.
        self.wanted_codec = None;
        match name {
            None => {
                self.selection.set(None);
                if self.codec.take().is_some() {
                    self.notice = Some(Notice::info("Decoding off"));
                }
            }
            Some(name) => match codec_registry(cx).get(name) {
                Some(factory) => {
                    self.selection.set(Some(factory.clone()));
                    self.codec = Some(ActiveCodec {
                        name: name.to_owned(),
                        info: factory.info(),
                        factory,
                    });
                    self.notice = Some(Notice::info(format!("Decoding with {name}")));
                }
                None => {
                    self.notice = Some(Notice::error(format!(
                        "No codec named {name} is loaded; decoding is unchanged"
                    )));
                    cx.notify();
                    return false;
                }
            },
        }
        tracing::info!(port = %self.port, codec = name.unwrap_or(NO_CODEC), "codec selected");
        // Summaries start with the frames of this codec.
        self.inline_next = self.frame_snapshot.end().max(self.frames.stats().end);
        self.decoded_generation += 1;
        self.update_filter(cx);
        cx.notify();
        true
    }

    /// Decode with `name`, as a device profile's `plugin` (or a restored tab) asks: from
    /// the next chunk if that plugin is installed. If it is not, the session goes on
    /// without a codec, the status line names the missing plugin and offers to install
    /// it (or the plugins folder, for a plugin the app has no example of), and the
    /// session starts decoding with it once it is installed, unless a codec is picked
    /// before that.
    pub fn want_codec(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.trim();
        if name.is_empty() || name == NO_CODEC {
            self.set_codec(None, cx);
            return;
        }
        if codec_registry(cx).contains(name) {
            self.set_codec(Some(name), cx);
            return;
        }
        tracing::warn!(port = %self.port, plugin = name, "the plugin is not installed; no codec");
        self.wanted_codec = Some(name.to_owned());
        self.notice = Some(plugin_files::missing_plugin_notice(
            format!("The {name} plugin is not installed; connected without a codec"),
            name,
        ));
        cx.notify();
    }

    /// The configuration changed: if the factory of the codec this session runs was
    /// replaced (a plugin reloaded), decode with the new one from the next chunk on; if
    /// the plugin this session waits for was installed, start decoding with it. (The
    /// codec menu lists the configuration's codecs each time it opens.)
    fn codecs_changed(&mut self, cx: &mut Context<Self>) {
        let registry = codec_registry(cx);
        if self.codec.is_none()
            && let Some(wanted) = self.wanted_codec.clone()
            && registry.contains(&wanted)
        {
            tracing::info!(port = %self.port, codec = %wanted, "the plugin was installed");
            self.set_codec(Some(&wanted), cx);
            return;
        }
        let Some(codec) = &self.codec else {
            return;
        };
        let Some(factory) = registry.get(&codec.name) else {
            // The plugin is gone; what runs keeps running until another is picked.
            return;
        };
        if Arc::ptr_eq(&factory, &codec.factory) {
            return;
        }
        let name = codec.name.clone();
        tracing::info!(port = %self.port, codec = %name, "codec reloaded");
        self.selection.set(Some(factory.clone()));
        self.codec = Some(ActiveCodec {
            name: name.clone(),
            info: factory.info(),
            factory,
        });
        self.decoded_generation += 1;
        self.notice = Some(Notice::info(format!("Reloaded {name}")));
        cx.notify();
    }

    /// The frames decoded so far, as of the last wake.
    pub fn frames(&self) -> &FrameSnapshot {
        &self.frame_snapshot
    }

    /// A reader of the session's frames, for looking without waiting for a wake.
    pub fn frame_reader(&self) -> &FrameStoreReader {
        &self.frames
    }

    /// Bumped whenever the frames or the codec change.
    pub fn decoded_generation(&self) -> u64 {
        self.decoded_generation
    }

    /// How frames are stamped: the terminal's timestamp mode and the configured format.
    pub fn frame_time(&self, cx: &App) -> FrameTime {
        FrameTime {
            clock: Clock::local(self.snapshot().epoch()),
            mode: self.terminal.read(cx).timestamps(),
            format: cx
                .try_global::<Config>()
                .map(|config| config.timestamp_format().to_owned()),
        }
    }

    pub fn decoded_inline(&self) -> bool {
        self.decoded_inline
    }

    /// Put a summary of each frame decoded from now on into the scrollback, or stop.
    pub fn set_decoded_inline(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.decoded_inline != on {
            self.decoded_inline = on;
            self.inline_next = self.frame_snapshot.end();
            cx.notify();
        }
    }

    pub fn hides_framed_bytes(&self) -> bool {
        self.hide_framed
    }

    /// Leave the lines of binary frames out of the text view, or show them again.
    pub fn set_hide_framed_bytes(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.hide_framed != on {
            self.hide_framed = on;
            self.update_filter(cx);
            cx.notify();
        }
    }

    /// Start or stop the hidden-lines map as the toggle and the codec say, and give the
    /// terminal the matching source when that changes what it shows.
    fn update_filter(&mut self, cx: &mut Context<Self>) {
        let wanted = self.hide_framed && self.codec.is_some();
        let had = self.filtered.is_some();
        if wanted && self.filter.is_none() {
            self.filter = Some(FramedFilter::new(self.snapshot()));
        } else if !wanted {
            self.filter = None;
        }
        self.refilter();
        if had || self.filtered.is_some() {
            self.replace_text_source(cx);
        }
    }

    /// The frame selected in the Decoded panel.
    pub fn selected_frame(&self) -> Option<FrameId> {
        self.selected_frame
    }

    /// The selected frame's lines as marked, in store ids.
    pub fn frame_marks(&self) -> &[SearchMatch] {
        &self.frame_marks
    }

    /// Show frame `id` in the terminal: mark the text lines that hold its bytes and
    /// scroll to the first, or in hex view scroll to the row of its first byte.
    pub fn select_frame(&mut self, id: FrameId, cx: &mut Context<Self>) {
        let Some(frame) = self.frame_snapshot.get(id).cloned() else {
            return;
        };
        self.selected_frame = Some(id);
        let lines = lines_holding(self.snapshot(), frame.raw.clone());
        self.frame_marks = lines.iter().map(whole_line_mark).collect();
        self.sync_marks(cx);
        let row = match self.terminal.read(cx).display_mode() {
            DisplayMode::Hex => Some(LineId(
                frame.raw.start / self.hex_bytes_per_row.max(1) as u64,
            )),
            // A screen's rows are not the store's lines: nothing to scroll to.
            DisplayMode::Text if self.vt.is_some() => None,
            DisplayMode::Text => {
                let store = lines
                    .first()
                    .map_or_else(|| self.line_at_offset(frame.raw.start), |line| line.id);
                Some(match &self.filtered {
                    Some(filtered) => filtered.view_at_or_after(store),
                    None => store,
                })
            }
        };
        if let Some(row) = row {
            self.terminal
                .update(cx, |terminal, cx| terminal.reveal(row, cx));
        }
        cx.notify();
    }

    /// The first line at or after stream offset `offset`.
    fn line_at_offset(&self, offset: u64) -> LineId {
        let snapshot = self.snapshot();
        let (mut lo, mut hi) = (snapshot.first_line().0, snapshot.end().0);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match snapshot.line(LineId(mid)) {
                Some(line) if line.raw.end <= offset => lo = mid + 1,
                _ => hi = mid,
            }
        }
        LineId(lo)
    }

    // --- Scripts ---------------------------------------------------------------------

    /// Give this session a script thread, whose `serial.current()` is this session. The
    /// workspace calls it right after connecting; a disconnected view takes none.
    pub fn attach_scripts(&mut self, env: ScriptEnv) {
        if self.scripts.is_some() || self.state.is_disconnected() {
            return;
        }
        let Some(ingest) = &self.ingest else {
            return;
        };
        let session = GuiScriptSession {
            port: self.port.clone(),
            serial: self.serial.clone(),
            control: self.session.as_ref().and_then(|session| session.control()),
            reader: ingest.reader(),
            link: self.script_link.clone(),
        };
        self.script_env = Some(env.clone());
        self.scripts = Some(SessionScripts::new(Arc::new(session), env));
    }

    /// Whether scripts can run on this session now.
    pub fn scripts_attached(&self) -> bool {
        self.scripts
            .as_ref()
            .is_some_and(SessionScripts::is_attached)
    }

    /// Queue `source` on this session's script thread; `origin` says what started it
    /// (`console`, `key binding`, `on_connect`...). Returns whether it was queued: a
    /// disconnected session says why in the console and the status line instead.
    pub fn run_script(
        &mut self,
        source: ScriptSource,
        origin: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let name = source.name.clone();
        let queued = self
            .scripts
            .as_mut()
            .and_then(|scripts| scripts.run(source, origin));
        match queued {
            Some(lines) => {
                tracing::debug!(port = %self.port, %name, origin, "script queued");
                self.emit_script_lines(lines, cx);
                self.ensure_script_poll(window, cx);
                cx.notify();
                true
            }
            None => {
                let message = format!("Not connected; {name} did not run");
                self.emit_script_lines(vec![ConsoleLine::new(ConsoleKind::Error, &message)], cx);
                self.notice = Some(Notice::error(message));
                cx.notify();
                false
            }
        }
    }

    /// Stop the running script. Returns whether one was running.
    pub fn stop_script(&mut self, cx: &mut Context<Self>) -> bool {
        let stopped = self.scripts.as_mut().and_then(SessionScripts::stop_current);
        if let Some(name) = &stopped {
            tracing::debug!(port = %self.port, %name, "stopping script");
        }
        cx.notify();
        stopped.is_some()
    }

    /// The script running on this session, for the status line.
    pub fn script_status(&self) -> Option<ScriptStatus> {
        self.scripts.as_ref()?.status()
    }

    /// The dialog a script's `ui.prompt` opened, while it is open.
    pub fn script_prompt(&self) -> Option<&Entity<ScriptPrompt>> {
        self.script_prompt.as_ref()
    }

    fn emit_script_lines(&mut self, lines: Vec<ConsoleLine>, cx: &mut Context<Self>) {
        if !lines.is_empty() {
            cx.emit(SessionViewEvent::Script(lines));
        }
    }

    /// Stop every run and let the script thread go (joined off the main thread). The
    /// runs' last lines still arrive: polling goes on until each has finished.
    fn detach_scripts(&mut self, cx: &mut Context<Self>) {
        let reason = match &self.state {
            ConnectionState::Disconnected { error: Some(error) } => {
                format!("the connection was lost: {error}")
            }
            _ => "the session was disconnected".to_owned(),
        };
        let Some(scripts) = self.scripts.as_mut() else {
            return;
        };
        let (host, lines) = scripts.detach(&reason);
        if let Some(host) = host {
            cx.background_spawn(async move { drop_host(host) }).detach();
        }
        self.emit_script_lines(lines, cx);
    }

    /// Poll the runs once a frame while any is queued.
    fn ensure_script_poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.script_polling {
            return;
        }
        self.script_polling = true;
        self._script_poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(FRAME).await;
                let active = this
                    .update_in(cx, |view, window, cx| view.poll_scripts(window, cx))
                    .unwrap_or(false);
                if !active {
                    break;
                }
            }
        }));
    }

    /// Act on what the runs reported since the last look. Returns whether any run is
    /// still queued, so polling should go on.
    fn poll_scripts(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(scripts) = self.scripts.as_mut() else {
            self.script_polling = false;
            return false;
        };
        let effects = scripts.poll();
        let active = scripts.is_active();
        if !active {
            self.script_polling = false;
        }
        if effects.is_empty() {
            return active;
        }
        let mut lines = Vec::new();
        for effect in effects {
            match effect {
                ScriptEffect::Line(line) => lines.push(line),
                ScriptEffect::Prompt {
                    label,
                    default,
                    answer,
                } => self.open_script_prompt(label, default, answer, window, cx),
                ScriptEffect::Notify(text) => self.notice = Some(Notice::info(text)),
                ScriptEffect::SendCommand { reference, params } => {
                    // What the script printed before the send shows before its echo.
                    self.emit_script_lines(std::mem::take(&mut lines), cx);
                    self.send_command_for_script(&reference, &params, cx);
                }
                ScriptEffect::Finished { name, outcome } => {
                    if let Some(prompt) = self.script_prompt.take() {
                        prompt.update(cx, |prompt, _| prompt.answer(None));
                        window.close_dialog(cx);
                        lines.push(ConsoleLine::new(
                            ConsoleKind::Answer,
                            "\u{2192} (closed: the script ended)",
                        ));
                    }
                    self.notice = Some(match outcome {
                        ScriptOutcome::Ok => Notice::info(format!("{name} finished")),
                        ScriptOutcome::Error(message) => Notice::error(format!(
                            "{name} failed: {}",
                            message.lines().next().unwrap_or_default()
                        )),
                        ScriptOutcome::Stopped => Notice::info(format!("{name} stopped")),
                    });
                }
            }
        }
        self.emit_script_lines(lines, cx);
        cx.notify();
        active
    }

    /// `commands.send` from a script: the command as the configuration has it now.
    fn send_command_for_script(
        &mut self,
        reference: &CommandRef,
        params: &ParamValues,
        cx: &mut Context<Self>,
    ) {
        let command = cx
            .try_global::<Config>()
            .and_then(|config| config.commands().get(reference).cloned());
        match command {
            Some(command) => self.send_command(reference, &command, params, cx),
            None => self.emit_script_lines(
                vec![ConsoleLine::new(
                    ConsoleKind::Error,
                    format!("commands.send: {reference} is no longer saved"),
                )],
                cx,
            ),
        }
    }

    /// Open the dialog for a script's `ui.prompt`. Its answer goes to `answer`; a
    /// second prompt while one is open (which a script cannot ask for) answers `nil`.
    fn open_script_prompt(
        &mut self,
        label: String,
        default: Option<String>,
        answer: async_channel::Sender<Option<String>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.script_prompt.is_some() {
            return;
        }
        let title = SharedString::from(match self.script_status() {
            Some(status) => format!("{} asks", status.name),
            None => "A script asks".to_owned(),
        });
        let prompt = cx.new(|cx| ScriptPrompt::new(label, default, answer, window, cx));
        self.script_prompt = Some(prompt.clone());
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = view.clone();
            let closed = view.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(prompt.clone())
                .footer(DialogButtons::new("OK"))
                .on_ok(move |_, _, cx| {
                    confirm
                        .update(cx, |view, cx| view.answer_script_prompt(true, cx))
                        .ok();
                    true
                })
                // Cancel, Escape and the close button; after OK it finds nothing to do.
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |view, cx| view.answer_script_prompt(false, cx))
                        .ok();
                })
        });
        cx.notify();
    }

    /// Answer the open prompt with its text (`confirm`) or `nil`, and echo the answer in
    /// the console.
    pub fn answer_script_prompt(&mut self, confirm: bool, cx: &mut Context<Self>) {
        let Some(prompt) = self.script_prompt.take() else {
            return;
        };
        let answer = confirm.then(|| prompt.read(cx).value(cx));
        let waiting = prompt.update(cx, |prompt, _| prompt.answer(answer.clone()));
        if waiting {
            let echo = match answer {
                Some(text) => format!("\u{2192} {text}"),
                None => "\u{2192} (dismissed)".to_owned(),
            };
            self.emit_script_lines(vec![ConsoleLine::new(ConsoleKind::Answer, echo)], cx);
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
                // What was typed and not ended with Enter stays as the line it is.
                if self.compose.read(cx).local_echo()
                    && let Some((_, ingest)) = self.live()
                {
                    let _ = ingest.append_local_inline("\n", Direction::Tx);
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

    /// The encoder for this session's keys: its line ending and Backspace byte, and in
    /// VT mode the screen's cursor key mode as of now.
    fn key_encoder(&self, cx: &App) -> KeyEncoder {
        let cursor_keys = self
            .vt
            .as_ref()
            .map(|vt| vt.handle.snapshot().modes().app_cursor_keys);
        KeyEncoder::new(self.line_ending(cx), inline_settings(cx).backspace)
            .with_cursor_keys(cursor_keys)
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

    /// Write one key's bytes, echoing it first when local echo is on: the key is typed
    /// into the scrollback's `Tx` line as it is pressed (see [`crate::inline`]). The
    /// echo is queued before the write, and ingest applies it before the next session
    /// event, so it always lands before the reply it causes.
    pub fn send_key(&mut self, key: EncodedKey, cx: &mut Context<Self>) {
        let echo = self.compose.read(cx).local_echo();
        let Some((session, ingest)) = self.live() else {
            self.notice = Some(Notice::error("Not connected; key not sent"));
            cx.notify();
            return;
        };
        if echo {
            echo_key(ingest, &key.echo);
        }
        if session.write(key.bytes).is_err() {
            let _ = ingest.append_local("Not sent: the session closed", Direction::Notice);
        }
    }

    /// Send `text` as a paste: line breaks become what Enter sends, and the bytes go
    /// out in chunks of `inline.paste_chunk_bytes`, `inline.paste_chunk_delay_ms`
    /// apart, from an async task. A paste replaces one still going; leaving inline mode
    /// stops it.
    pub fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let settings = inline_settings(cx);
        let mut bytes = paste_bytes(text, &self.key_encoder(cx).enter);
        if bytes.is_empty() {
            return;
        }
        // A screen in bracketed paste mode (VT mode) gets the paste marked as one.
        if self
            .vt
            .as_ref()
            .is_some_and(|vt| vt.handle.snapshot().modes().bracketed_paste)
        {
            bytes = bracketed(&bytes);
        }
        if self.live().is_none() {
            self.notice = Some(Notice::error("Not connected; nothing pasted"));
            cx.notify();
            return;
        }
        if self.compose.read(cx).local_echo()
            && let Some((_, ingest)) = self.live()
        {
            let _ = ingest.append_local_inline(paste_echo(text), Direction::Tx);
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
        self.refilter();
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
        if options.codec != old.codec {
            match options.codec.as_deref() {
                Some(name) => self.want_codec(name, cx),
                None => {
                    self.set_codec(None, cx);
                }
            }
        }
        let (new, old) = (options.display, old.display);
        if new.emulation != old.emulation {
            self.set_emulation(new.emulation, cx);
        }
        if new.decoded_inline != old.decoded_inline {
            self.set_decoded_inline(new.decoded_inline, cx);
        }
        if new.hide_framed_bytes != old.hide_framed_bytes {
            self.set_hide_framed_bytes(new.hide_framed_bytes, cx);
        }
        if new.hex_bytes_per_row != old.hex_bytes_per_row {
            self.hex_bytes_per_row = new.hex_bytes_per_row;
            self.rebuild_scrollback();
            self.refilter();
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
    /// Scripts running on it are stopped.
    fn close_session(&mut self, cx: &mut Context<Self>) {
        self.stop_recording(cx);
        self.detach_scripts(cx);
        self.port_change = None;
        self._port_watch = None;
        if let Some(session) = self.session.take() {
            self.stats = session.stats();
            cx.background_spawn(async move { session.close() }).detach();
        }
        self.vt_slot.set_writer(None);
        self.port_form
            .update(cx, |form, cx| form.set_live(false, cx));
    }

    /// Disconnect, as the toolbar's Disconnect and `serial::Disconnect` do: at once,
    /// unless a recording or a script is running, which a dialog asks about first.
    pub fn request_disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.is_disconnected() {
            return;
        }
        let recording = self.recording_status.is_some();
        let script = self.script_status().map(|status| status.name);
        let running = match (recording, &script) {
            (false, None) => {
                self.disconnect(cx);
                return;
            }
            (true, Some(script)) => format!("A recording and {script} are running."),
            (true, None) => "A recording is running.".to_owned(),
            (false, Some(script)) => format!("{script} is running."),
        };
        let message = SharedString::from(format!(
            "{running} Disconnecting stops the script and finishes the recording; the \
             scrollback stays."
        ));
        let title = SharedString::from(format!("Disconnect {}?", self.port));
        self.pending_disconnect = true;
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let confirm = view.clone();
            let closed = view.clone();
            dialog
                .title(title.clone())
                .w(px(420.))
                .child(div().text_sm().child(message.clone()))
                .footer(DialogButtons::new("Disconnect"))
                .on_ok(move |_, window, cx| {
                    confirm
                        .update(cx, |view, cx| view.confirm_disconnect(window, cx))
                        .ok();
                    false
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |view, _| view.pending_disconnect = false)
                        .ok();
                })
        });
        cx.notify();
    }

    /// Whether a disconnect confirmation is open.
    pub fn pending_disconnect(&self) -> bool {
        self.pending_disconnect
    }

    /// Disconnect as the confirmation's button does, closing the dialog first.
    pub fn confirm_disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.pending_disconnect) {
            window.close_dialog(cx);
            self.disconnect(cx);
        }
    }

    // --- Connecting again ------------------------------------------------------------

    /// Whether [`Self::reconnect`] can take a session now: this one is closed and its
    /// ingest thread has handed its store back.
    pub fn can_reconnect(&self) -> bool {
        self.session.is_none() && self.ingest.is_none() && self.ingest_joined
    }

    /// Stop the ingest thread of a closed session that is still draining it, and take
    /// its store back off the main thread. The thread ends by itself once the closed
    /// session's events run out; this is for when they have not a while after.
    pub fn retire_ingest(&mut self, cx: &mut Context<Self>) {
        if self.session.is_some() {
            return;
        }
        self.refresh(cx);
        let Some(ingest) = self.ingest.take() else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let joined = cx.background_spawn(async move { ingest.stop() }).await;
            this.update(cx, |view, cx| view.ingest_joined(joined, cx))
                .ok();
        })
        .detach();
    }

    /// Carry on with `session`, opened on the same port with `serial`: a new ingest
    /// thread goes on filling the same store, so the scrollback, pause, marks and
    /// display stay, and the line ending, echo and control levels set here apply again.
    /// The frames decoded start over with this connection. Scripts attach again.
    /// Call when [`Self::can_reconnect`].
    pub fn reconnect(
        &mut self,
        session: Box<dyn SessionHandle>,
        serial: SerialConfig,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.can_reconnect(), "the last session is still open");
        let store = self
            .retired_store
            .take()
            .unwrap_or_else(|| Store::new(self.options.store.clone()));
        // A closed bell stays closed: scripts get a new one.
        self.script_link = ScriptLinkParts::default();
        // The screen (VT mode) carries on, answering the device through the new session.
        self.vt_slot.set_writer(session.control());
        let IngestStart {
            ingest,
            frames,
            wake,
        } = start_ingest(
            session.events(),
            store,
            &self.recording,
            &self.script_link,
            &self.selection,
            &self.vt_slot,
            Vec::new(),
            cx,
        );
        self.ingest = Some(ingest);
        self._wake = wake;
        self.ingest_joined = false;
        self.missed_wake = false;
        self.frame_snapshot = frames.snapshot();
        self.frames = frames;
        self.inline_next = FrameId::ZERO;
        self.frame_marks.clear();
        self.selected_frame = None;
        self.decoded_generation += 1;
        if self.filter.take().is_some() {
            self.update_filter(cx);
        }
        self.opened_serial = serial.clone();
        self.serial = serial;
        self.connection = ConnectionInfo::default();
        self.state = ConnectionState::Connected;
        self.stats = session.stats();
        self.rx_seen = 0;
        self.session = Some(session);
        self.notice = None;
        self.scripts = None;
        if let Some(env) = self.script_env.clone() {
            self.attach_scripts(env);
        }
        self.apply_control_levels();
        self.port_form
            .update(cx, |form, cx| form.set_live(true, cx));
        tracing::info!(port = %self.port, serial = %self.serial.summary(), "session open again");
        cx.notify();
    }

    // --- Port settings -----------------------------------------------------------------

    /// The port settings popover's form.
    pub fn port_form(&self) -> &Entity<PortSettingsForm> {
        &self.port_form
    }

    /// The port settings in force here: the line settings, the compose bar's line
    /// ending and echo, and the control levels.
    pub fn port_settings(&self, cx: &App) -> PortSettings {
        let compose = self.compose.read(cx);
        PortSettings {
            serial: self.serial.clone(),
            line_ending: compose.line_ending(),
            local_echo: compose.local_echo(),
            dtr: self.dtr,
            rts: self.rts,
        }
    }

    /// Show the settings in force in the form, as it opens.
    pub fn sync_port_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = self.port_settings(cx);
        let live = self.control().is_some();
        self.port_form.update(cx, |form, cx| {
            form.set_settings(settings, window, cx);
            form.set_live(live, cx);
        });
    }

    /// Take `settings` (set on a Devices row for this port) as a new session's: the
    /// line ending, echo and control levels. The line settings are what it opened with.
    pub fn apply_port_settings(&mut self, settings: &PortSettings, cx: &mut Context<Self>) {
        let (ending, echo) = (settings.line_ending, settings.local_echo);
        self.compose.update(cx, |compose, cx| {
            compose.set_line_ending(ending, cx);
            compose.set_local_echo(echo, cx);
        });
        self.dtr = settings.dtr;
        self.rts = settings.rts;
        self.apply_control_levels();
        cx.notify();
    }

    /// Set the control lines the opener asserted to the levels set here.
    fn apply_control_levels(&self) {
        let Some(control) = self.control() else {
            return;
        };
        for (line, level) in [(ControlLine::Dtr, self.dtr), (ControlLine::Rts, self.rts)] {
            if !level {
                let _ = control.set_control(line, false);
            }
        }
    }

    /// The live session's control handle.
    fn control(&self) -> Option<Arc<dyn crate::session_handle::SessionControl>> {
        self.live().and_then(|(session, _)| session.control())
    }

    /// DTR and RTS as last set from here.
    pub fn control_levels(&self) -> (bool, bool) {
        (self.dtr, self.rts)
    }

    fn port_settings_changed(
        &mut self,
        event: &PortSettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PortSettingsEvent::Serial(serial) => self.reconfigure(serial.clone(), window, cx),
            PortSettingsEvent::LineEnding(ending) => {
                let ending = *ending;
                self.compose
                    .update(cx, |compose, cx| compose.set_line_ending(ending, cx));
            }
            PortSettingsEvent::LocalEcho(on) => {
                let on = *on;
                self.compose
                    .update(cx, |compose, cx| compose.set_local_echo(on, cx));
            }
            PortSettingsEvent::Control(line, on) => {
                self.set_control_line(*line, *on, window, cx);
            }
            PortSettingsEvent::SendBreak => self.send_break(window, cx),
        }
    }

    /// Change the line settings: on the open port through its writer thread (the
    /// form hears whether the transport took them), or, while closed, for the next
    /// connect in this view.
    pub fn reconfigure(
        &mut self,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(control) = self.control() else {
            self.serial = serial;
            self.port_form.update(cx, |form, cx| {
                form.set_status(Some("Used when the port is connected again".into()), cx);
            });
            cx.notify();
            return;
        };
        if control.reconfigure(serial.clone()).is_err() {
            self.port_change_failed(None, "the session closed".into(), window, cx);
            return;
        }
        tracing::debug!(port = %self.port, serial = %serial.summary(), "reconfiguring");
        self.watch_port_change(
            PortChangeKind::Serial {
                from: self.serial.clone(),
                to: serial,
            },
            window,
            cx,
        );
    }

    /// Assert or release DTR or RTS on the open port, or set its level for the next
    /// connect in this view.
    pub fn set_control_line(
        &mut self,
        line: ControlLine,
        on: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(control) = self.control() else {
            self.set_level(line, on);
            cx.notify();
            return;
        };
        if control.set_control(line, on).is_err() {
            self.port_change_failed(None, "Not connected".into(), window, cx);
            return;
        }
        self.set_level(line, on);
        self.watch_port_change(PortChangeKind::Control(line, on), window, cx);
    }

    fn set_level(&mut self, line: ControlLine, on: bool) {
        match line {
            ControlLine::Dtr => self.dtr = on,
            ControlLine::Rts => self.rts = on,
        }
    }

    /// Hold the open port's line in break for [`BREAK_DURATION`].
    pub fn send_break(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sent = self
            .control()
            .is_some_and(|control| control.send_break(BREAK_DURATION).is_ok());
        if !sent {
            self.port_form.update(cx, |form, cx| {
                form.set_error(Some("Not connected; no break sent".into()), cx);
            });
            return;
        }
        self.watch_port_change(PortChangeKind::Break, window, cx);
    }

    /// Wait a frame at a time for the writer thread's verdict on `kind`.
    fn watch_port_change(
        &mut self,
        kind: PortChangeKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mark = self
            .latest_snapshot()
            .map_or(LineId::ZERO, |snapshot| snapshot.end());
        self.port_change = Some(PortChange {
            kind,
            mark,
            sent_at: cx.background_executor().now(),
        });
        self._port_watch = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(EXPECT_POLL).await;
                let decided = this
                    .update_in(cx, |view, window, cx| view.poll_port_change(window, cx))
                    .unwrap_or(true);
                if decided {
                    break;
                }
            }
        }));
    }

    /// The writer thread's refusal stored since `mark`: a `Write failed` notice.
    fn write_failure_since(&self, mark: LineId) -> Option<String> {
        let snapshot = self.latest_snapshot()?;
        let mut lines = Vec::new();
        snapshot.lines(mark.max(snapshot.first_line())..snapshot.end(), &mut lines);
        lines.into_iter().find_map(|line| {
            (line.direction == Direction::Notice)
                .then(|| line.text.strip_prefix("Write failed: ").map(str::to_owned))
                .flatten()
        })
    }

    /// Look at the change in flight. Returns whether it is decided.
    fn poll_port_change(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(change) = self.port_change.clone() else {
            return true;
        };
        let now = cx.background_executor().now();
        let elapsed = now.saturating_duration_since(change.sent_at);
        if let Some(error) = self.write_failure_since(change.mark) {
            self.port_change = None;
            self.port_change_failed(Some(change.kind), error, window, cx);
            return true;
        }
        let status = match &change.kind {
            PortChangeKind::Serial { to, .. } => {
                let applied = self
                    .control()
                    .and_then(|control| control.serial_config())
                    .is_some_and(|serial| &serial == to);
                if applied {
                    self.serial = to.clone();
                    Some(format!("{} applied", to.summary()))
                } else if elapsed >= PORT_CHANGE_TIMEOUT {
                    self.port_change = None;
                    let error = "the port did not confirm the change".to_owned();
                    self.port_change_failed(Some(change.kind), error, window, cx);
                    return true;
                } else {
                    None
                }
            }
            PortChangeKind::Control(line, on) => (elapsed >= PORT_CHANGE_SETTLE).then(|| {
                let name = match line {
                    ControlLine::Dtr => "DTR",
                    ControlLine::Rts => "RTS",
                };
                format!("{name} {}", if *on { "asserted" } else { "released" })
            }),
            PortChangeKind::Break => {
                (elapsed >= BREAK_DURATION + PORT_CHANGE_SETTLE).then(|| "Break sent".to_owned())
            }
        };
        let Some(status) = status else {
            return false;
        };
        self.port_change = None;
        self.port_form
            .update(cx, |form, cx| form.set_status(Some(status), cx));
        cx.notify();
        true
    }

    /// The port refused a change (or closed first): say why in the form and put its
    /// controls back to what is in force.
    fn port_change_failed(
        &mut self,
        kind: Option<PortChangeKind>,
        error: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match kind {
            Some(PortChangeKind::Serial { from, to }) => {
                tracing::warn!(port = %self.port, serial = %to.summary(), %error, "reconfigure refused");
                // What the port is on now: what it was on, unless it says otherwise.
                self.serial = self
                    .control()
                    .and_then(|control| control.serial_config())
                    .unwrap_or(from);
            }
            Some(PortChangeKind::Control(line, on)) => self.set_level(line, !on),
            Some(PortChangeKind::Break) | None => {}
        }
        let settings = self.port_settings(cx);
        self.port_form.update(cx, |form, cx| {
            form.set_settings(settings, window, cx);
            form.set_error(Some(error), cx);
        });
        cx.notify();
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

    /// Follow the live tail again. In VT mode the screen shows as it is now.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.pause.take().is_none() {
            return;
        }
        if let Some(vt) = &mut self.vt {
            vt.snapshot = vt.handle.snapshot();
            self.push_sources(cx, |_, _| {});
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
    /// `preferred` is the suggestion and the fallback for unknown extensions. The screen
    /// stays the screen under any text extension.
    pub fn export(&mut self, preferred: ExportFormat, cx: &mut Context<Self>) {
        let stem = self.file_stem();
        let suggested = match preferred {
            ExportFormat::Screen => format!("{stem}-screen.txt"),
            _ => format!("{stem}.{}", preferred.extension()),
        };
        self.prompt_for_path(suggested, cx, move |view, path, cx| {
            let format = match (preferred, ExportFormat::from_path(&path)) {
                (ExportFormat::Screen, None | Some(ExportFormat::Text)) => ExportFormat::Screen,
                (_, Some(format)) => format,
                (_, None) => preferred,
            };
            view.export_to(path, format, cx).detach();
        });
    }

    /// What an export in `format` writes if taken now: the selected lines (or hex
    /// rows) if there is a selection, else what a paused view shows, else everything
    /// retained. Text follows the display: lines, or hex rows in hex view, stamped as
    /// the gutter is (absolute stamps in `display.timestamp_format`, which is read when
    /// the job is taken). Raw is the stream bytes under the same choice, whole lines at a
    /// time; with no selection it ignores Clear, which hides lines, not bytes.
    ///
    /// In VT mode the text view's selection is of screen rows: a text export of it writes
    /// those rows, and with none the export reads the store's lines (the log), cut at the
    /// pause. Raw bytes cannot be told apart by screen row, so a raw export there ignores
    /// the selection. [`ExportFormat::Screen`] writes the screen's rows as shown.
    pub fn export_job(&self, format: ExportFormat, cx: &App) -> ExportJob {
        let terminal = self.terminal.read(cx);
        let snapshot = self.snapshot().clone();
        if format == ExportFormat::Screen {
            let rows = self.vt.as_ref().map_or_else(Vec::new, |vt| {
                vt.snapshot
                    .screen_text()
                    .into_iter()
                    .map(str::to_owned)
                    .collect()
            });
            return ExportJob::Screen { rows };
        }
        let screen = self.vt.is_some() && terminal.display_mode() == DisplayMode::Text;
        let span = terminal.displayed_span();
        let selected = terminal
            .selection()
            .filter(|_| !(screen && format == ExportFormat::Raw))
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
        // Stamped as the gutter is: the same mode and, for absolute stamps, the format
        // the settings name.
        let mut options =
            TextOptions::default().with_timestamps(export_timestamps(terminal.timestamps()));
        if let Some(config) = cx.try_global::<Config>() {
            options = options.with_timestamp_format(config.timestamp_format());
        }
        if screen && format == ExportFormat::Text {
            if let Some(lines) = selected {
                return ExportJob::Lines {
                    source: SharedSource(self.text_source()),
                    lines,
                    options,
                };
            }
            // The log, as far as a pause saw it.
            let log = self.log_source();
            let end = match (self.pause, &self.filtered) {
                (None, _) => log.end(),
                (Some(mark), None) => LineId(mark.lines).min(log.end()),
                (Some(mark), Some(filtered)) => filtered.view_at_or_after(LineId(mark.lines)),
            };
            let lines = log.first_line()..end.max(log.first_line());
            return match &self.filtered {
                Some(_) => ExportJob::Lines {
                    source: SharedSource(log),
                    lines,
                    options,
                },
                None => ExportJob::Text {
                    snapshot,
                    lines,
                    options,
                },
            };
        }
        match (format, terminal.display_mode()) {
            // Decoded frames: all of them, whatever the view shows.
            (ExportFormat::Csv | ExportFormat::Json, _) => ExportJob::Frames {
                frames: self.frames.snapshot(),
                raw: snapshot,
                time: self.frame_time(cx),
                format: if format == ExportFormat::Csv {
                    FramesFormat::Csv
                } else {
                    FramesFormat::Json
                },
            },
            // With framed bytes hidden the terminal's line ids are the filter's.
            (ExportFormat::Text, DisplayMode::Text) if self.filtered.is_some() => {
                ExportJob::Lines {
                    source: SharedSource(self.log_source()),
                    lines,
                    options,
                }
            }
            (ExportFormat::Text, DisplayMode::Text) => ExportJob::Text {
                snapshot,
                lines,
                options,
            },
            (ExportFormat::Text, DisplayMode::Hex) => ExportJob::HexText {
                hex: self.scrollback.hex.inner.clone(),
                rows: lines,
                options,
            },
            (ExportFormat::Screen, _) => unreachable!("taken above"),
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
        if format.is_decoded() && self.codec.is_none() && self.frame_snapshot.is_empty() {
            self.notice = Some(Notice::error(
                "No codec decodes this session, so there are no frames to export",
            ));
            cx.notify();
            return Task::ready(());
        }
        if format == ExportFormat::Screen && self.vt.is_none() {
            self.notice = Some(Notice::error(
                "Only VT mode has a screen to export; the text export has the lines",
            ));
            cx.notify();
            return Task::ready(());
        }
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
        // A link that is already up gets its `connect` record from the recorder; one
        // that comes up later is noted by the recording sink.
        let description = match &self.connection.state {
            LinkState::Connected { .. } => self.connection.description.clone(),
            _ => None,
        };
        cx.spawn(async move |this, cx| {
            let opened = cx
                .background_spawn(async move {
                    let recorder = Recorder::create(&path, description.as_deref())
                        .map_err(|error| error.to_string())?;
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
                Some(Ok(bytes)) => Notice::info(format!(
                    "Recorded {} to {name} (timing in {name}{TIMING_SUFFIX})",
                    format_bytes(bytes)
                )),
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

    /// The width the workspace gives the view (the center's), so the toolbar is laid out
    /// for it in the frame it is drawn in. `None` falls back to measuring.
    pub fn set_width_hint(&mut self, width: Option<Pixels>) {
        self.width_hint = width;
    }

    /// What the toolbar showed and put in its overflow menu, as last drawn.
    pub fn toolbar_layout(&self) -> &ToolbarLayout {
        &self.toolbar_layout
    }

    /// The widths of the toolbar's labelled controls: as last drawn with the same label,
    /// else estimated from the label's length (the first frame, a new label). The codec
    /// menu has one only when `codec_menu` (see [`ToolState::shows_codec_menu`]).
    fn toolbar_metrics(&self, codec_menu: bool, window: &Window) -> ToolbarMetrics {
        // Button labels are small text; a character is a little over half its size.
        let char_width = f32::from(window.rem_size()) * 0.875 * 0.6;
        let text = |text: &str| text.chars().count() as f32 * char_width;
        let summary = self.connection_label();
        let codec = self.codec_label();
        let measured = |width: &Option<(String, f32)>, label: &str| {
            width
                .as_ref()
                .filter(|(was, _)| was == label)
                .map(|(_, width)| *width)
        };
        ToolbarMetrics {
            connection: measured(&self.tool_widths.connection, &summary)
                .unwrap_or_else(|| 12. + 8. + text(&summary) + 16. + toolbar::ICON),
            mode: self
                .tool_widths
                .mode
                .unwrap_or_else(|| text("Command") + text("Inline") + 34.),
            codec: codec_menu.then(|| {
                measured(&self.tool_widths.codec, &codec)
                    .unwrap_or_else(|| (text(&codec) + 44.).min(f32::from(CODEC_MAX_WIDTH)))
            }),
        }
    }

    /// What the codec menu's button says: the codec, or "Codec" with none.
    fn codec_label(&self) -> String {
        self.codec_name().unwrap_or("Codec").to_owned()
    }

    /// Keep a labelled control's drawn width for the next layout; a change lays the
    /// toolbar out again.
    fn measured(
        view: &WeakEntity<Self>,
        control: ToolControl,
        label: String,
    ) -> impl FnOnce(Bounds<Pixels>, &mut Window, &mut App) + 'static {
        let view = view.clone();
        move |bounds, _, cx| {
            let width = f32::from(bounds.size.width);
            view.update(cx, |this, cx| {
                let slot = match control {
                    ToolControl::Connection => &mut this.tool_widths.connection,
                    ToolControl::Codec => &mut this.tool_widths.codec,
                    ToolControl::Mode => {
                        if this.tool_widths.mode != Some(width) {
                            this.tool_widths.mode = Some(width);
                            cx.notify();
                        }
                        return;
                    }
                };
                if slot.as_ref() != Some(&(label.clone(), width)) {
                    *slot = Some((label, width));
                    cx.notify();
                }
            })
            .ok();
        }
    }

    /// The pressed states the menus show, as of this frame.
    fn tool_state(&self, cx: &App) -> ToolState {
        let terminal = self.terminal.read(cx);
        ToolState {
            inline: self.mode == Mode::Inline,
            paused: self.is_paused(),
            recording: self.recording_status.is_some(),
            search: terminal.is_search_open(),
            hex: terminal.display_mode() == DisplayMode::Hex,
            has_hex: terminal.has_hex_source(),
            timestamps: terminal.timestamps(),
            wrap: terminal.wrap(),
            vt: self.emulation == Emulation::Vt,
            decoding: self.codec.is_some(),
            decoded_inline: self.decoded_inline,
            hide_framed: self.hide_framed,
            codec: self.codec_name().unwrap_or(NO_CODEC).to_owned(),
            codecs: codec_choices(cx),
            examples: plugin_files::examples_to_install(cx)
                .into_iter()
                .map(|example| (example.name.to_owned(), example.title.to_owned()))
                .collect(),
        }
    }

    /// Open the search bar, or close it.
    pub fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.terminal.update(cx, |terminal, cx| {
            if terminal.is_search_open() {
                terminal.dismiss_search(window, cx);
            } else {
                terminal.deploy_search(window, cx);
            }
        });
    }

    fn render_toolbar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = self.tool_state(cx);
        let metrics = self.toolbar_metrics(state.shows_codec_menu(), window);
        let available = self
            .width_hint
            .or(self.measured_width)
            .unwrap_or_else(|| window.viewport_size().width);
        // A couple of pixels in hand for rounding.
        let layout = toolbar::lay_out(f32::from(available) - 2., &metrics);
        self.toolbar_layout = layout.clone();

        let view = cx.entity().downgrade();
        let theme = cx.theme();
        let (muted, danger, border) = (theme.muted_foreground, theme.danger, theme.border);
        let dot_color = match &self.state {
            ConnectionState::Connected => theme.success,
            ConnectionState::Disconnected { error: None } => muted,
            ConnectionState::Disconnected { error: Some(_) } => danger,
        };
        let background = Config::toolbar_background(cx);
        let open = !self.state.is_disconnected();
        let workspace = Some(context::WORKSPACE);
        let terminal = Some(context::TERMINAL);

        let label = self.connection_label();
        let port_settings = match self.link {
            // A replay's control is its speed: a menu, not the port settings.
            LinkKind::Replay => self.replay_speed_button(&label, &view).into_any_element(),
            link => {
                let form = self.port_form.clone();
                Popover::new("port-settings-popover")
                    .trigger(
                        Button::new("port-settings")
                            .label(SharedString::from(label.clone()))
                            .tooltip(if link == LinkKind::Tcp {
                                "TCP stream: line ending and local echo"
                            } else {
                                "Port settings: baud, framing, flow control, DTR and RTS"
                            })
                            .small()
                            .ghost(),
                    )
                    .content(move |_, _, _| form.clone())
                    .on_open_change(cx.listener(|this, open: &bool, window, cx| {
                        if *open {
                            this.sync_port_form(window, cx);
                        }
                    }))
                    .into_any_element()
            }
        };
        let connection = if open {
            chrome::icon_button("session-disconnect", IconName::Unplug, cx)
                .tooltip_with_action(
                    "Disconnect (the scrollback stays)",
                    &actions::Disconnect,
                    workspace,
                )
                .on_click(cx.listener(|this, _, window, cx| this.request_disconnect(window, cx)))
        } else {
            Button::new("session-connect")
                .icon(IconName::Plug)
                .small()
                .primary()
                .tooltip("Connect: open the port again with these settings")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SessionViewEvent::Reconnect)))
        };

        let mut bar = h_flex()
            .id("session-toolbar")
            .flex_none()
            .w_full()
            .h(chrome::HEADER_HEIGHT)
            .px_2()
            .items_center()
            .overflow_hidden()
            .border_b_1()
            .border_color(border)
            .when_some(background, |bar, background| bar.bg(background))
            .child(
                h_flex()
                    .id("toolbar-connection")
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .child(div().pl_1().child(chrome::state_dot(dot_color, !open)))
                    .child(port_settings)
                    .child(connection)
                    .on_prepaint(Self::measured(&view, ToolControl::Connection, label)),
            );

        for group in ToolbarItem::GROUPS {
            let items: Vec<ToolbarItem> = group
                .iter()
                .copied()
                .filter(|item| layout.shows(*item))
                .collect();
            if items.is_empty() {
                continue;
            }
            bar = bar.child(chrome::toolbar_separator(cx)).child(
                h_flex().flex_none().gap_0p5().items_center().children(
                    items
                        .into_iter()
                        .map(|item| self.render_tool(item, &state, &view, workspace, terminal, cx)),
                ),
            );
        }

        if !layout.overflow.is_empty() {
            let overflow = layout.overflow.clone();
            let menu_state = state.clone();
            let menu_view = view.clone();
            bar = bar.child(
                div().flex_none().ml_auto().pl_2().child(
                    chrome::icon_button("toolbar-overflow", IconName::Ellipsis, cx)
                        .tooltip("More")
                        .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, window, cx| {
                            overflow_menu(menu, &overflow, &menu_state, &menu_view, window, cx)
                        }),
                ),
            );
        }

        // A view with no workspace to say how wide it is measures itself.
        let measure = view.clone();
        bar.on_prepaint(move |bounds, _, cx| {
            measure
                .update(cx, |this, cx| {
                    if this.width_hint.is_none() && this.measured_width != Some(bounds.size.width) {
                        this.measured_width = Some(bounds.size.width);
                        cx.notify();
                    }
                })
                .ok();
        })
        .test_support()
        .into_any_element()
    }

    /// The replay's speed control: a button saying the speed, with a menu of the others.
    /// Choosing one plays the capture again from the start at it.
    fn replay_speed_button(&self, label: &str, view: &WeakEntity<SessionView>) -> impl IntoElement {
        let (current, menu_view) = (self.replay_speed(), view.clone());
        Button::new("replay-speed")
            .label(SharedString::from(label.to_owned()))
            .small()
            .ghost()
            .dropdown_caret(true)
            .tooltip("Replay speed: choosing one restarts the replay from the beginning")
            .dropdown_menu(move |menu, _, _| speed_items(menu, current, &menu_view))
    }

    /// One control of the toolbar.
    fn render_tool(
        &self,
        item: ToolbarItem,
        state: &ToolState,
        view: &WeakEntity<SessionView>,
        workspace: Option<&str>,
        terminal: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match item {
            ToolbarItem::Mode => {
                let chord = cx
                    .try_global::<Config>()
                    .map(|config| config.inline().escape_chord.unparse())
                    .unwrap_or_else(|| "ctrl-]".to_owned());
                let (muted, foreground) = (cx.theme().muted_foreground, cx.theme().foreground);
                let group = ButtonGroup::new("mode-toggle")
                    .small()
                    .child(
                        Button::new("mode-command")
                            .label("Command")
                            .selected(!state.inline)
                            .text_color(if state.inline { muted } else { foreground })
                            .tooltip_with_action(
                                "Command mode: the compose bar and saved commands",
                                &actions::ToggleInline,
                                workspace,
                            ),
                    )
                    .child(
                        Button::new("mode-inline")
                            .label("Inline")
                            .selected(state.inline)
                            .text_color(if state.inline { foreground } else { muted })
                            .tooltip(format!(
                                "Inline mode: every keystroke goes to the port ({chord} leaves)"
                            )),
                    )
                    .on_click(cx.listener(|this, clicked: &Vec<usize>, window, cx| {
                        let mode = if clicked.contains(&1) {
                            Mode::Inline
                        } else {
                            Mode::Command
                        };
                        this.set_mode(mode, window, cx);
                    }));
                div()
                    .flex_none()
                    .child(group)
                    .on_prepaint(Self::measured(view, ToolControl::Mode, String::new()))
                    .into_any_element()
            }
            ToolbarItem::Pause => chrome::toggle_button(
                "pause",
                if state.paused {
                    IconName::Play
                } else {
                    IconName::Pause
                },
                state.paused,
                cx,
            )
            .tooltip_with_action(
                if state.paused {
                    "Resume following the stream"
                } else {
                    "Pause the view (capture goes on)"
                },
                &actions::Pause,
                workspace,
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx)))
            .into_any_element(),
            ToolbarItem::Record => {
                chrome::toggle_button("record", IconName::CircleDot, state.recording, cx)
                    .when(state.recording, |button| {
                        button.text_color(cx.theme().danger)
                    })
                    .tooltip_with_action(
                        if state.recording {
                            "Stop recording"
                        } else {
                            "Record every received byte to a file"
                        },
                        &actions::ToggleRecord,
                        workspace,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_record(cx)))
                    .into_any_element()
            }
            ToolbarItem::Clear => chrome::icon_button("clear", IconName::Eraser, cx)
                .tooltip_with_action("Clear the scrollback", &actions::Clear, workspace)
                .on_click(cx.listener(|this, _, _, cx| this.clear(cx)))
                .into_any_element(),
            ToolbarItem::Search => {
                chrome::toggle_button("search", IconName::Search, state.search, cx)
                    .tooltip_with_action("Search the scrollback", &actions::Search, terminal)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_search(window, cx)))
                    .into_any_element()
            }
            ToolbarItem::Hex => chrome::toggle_button("hex-view", IconName::Binary, state.hex, cx)
                .disabled(!state.has_hex)
                .tooltip_with_action("Hex view", &actions::ToggleHexView, terminal)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.terminal
                        .update(cx, |terminal, cx| terminal.toggle_hex(cx));
                }))
                .into_any_element(),
            ToolbarItem::Emulation => {
                chrome::toggle_button("emulation", IconName::Terminal, state.vt, cx)
                    .tooltip_with_action(
                        if state.vt {
                            "VT mode: the device draws on a terminal screen (click for the log)"
                        } else {
                            "Monitor mode: a log of lines (click for a terminal screen)"
                        },
                        &actions::ToggleEmulation,
                        terminal,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_emulation(cx)))
                    .into_any_element()
            }
            ToolbarItem::Timestamps => chrome::toggle_button(
                "timestamps",
                IconName::Clock,
                state.timestamps != TimestampMode::Off,
                cx,
            )
            .tooltip_with_action(
                format!(
                    "Timestamps: {} (click for the next)",
                    state.timestamps.label()
                ),
                &actions::CycleTimestamps,
                terminal,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.terminal
                    .update(cx, |terminal, cx| terminal.cycle_timestamps(cx));
            }))
            .into_any_element(),
            ToolbarItem::Wrap => chrome::toggle_button("wrap", IconName::TextWrap, state.wrap, cx)
                .tooltip_with_action("Wrap long lines", &actions::ToggleWrap, terminal)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.terminal
                        .update(cx, |terminal, cx| terminal.toggle_wrap(cx));
                }))
                .into_any_element(),
            ToolbarItem::Export => {
                let (decoding, screen, view) = (state.decoding, state.vt, view.clone());
                chrome::icon_button("export", IconName::Download, cx)
                    .tooltip_with_action("Export", &actions::Export, workspace)
                    .dropdown_menu(move |menu, _, _| export_items(menu, decoding, screen, &view))
                    .into_any_element()
            }
            ToolbarItem::Codec => {
                let (menu_state, menu_view) = (state.clone(), view.clone());
                let decoding = state.decoding;
                let button = Button::new("codec-picker")
                    .label(SharedString::from(if decoding {
                        state.codec.clone()
                    } else {
                        "Codec".to_owned()
                    }))
                    .icon(IconName::Braces)
                    .small()
                    .ghost()
                    .dropdown_caret(true)
                    .max_w(CODEC_MAX_WIDTH)
                    .when(!decoding, |button| {
                        button.text_color(cx.theme().muted_foreground)
                    })
                    .tooltip("Codec: decode the stream into frames")
                    .dropdown_menu(move |menu, window, cx| {
                        codec_items(menu, &menu_state, &menu_view, window, cx)
                    });
                div()
                    .flex_none()
                    .child(button)
                    .on_prepaint(Self::measured(view, ToolControl::Codec, self.codec_label()))
                    .into_any_element()
            }
        }
    }
}

/// The terminal's toggles the toolbar shows pressed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TerminalTools {
    wrap: bool,
    timestamps: TimestampMode,
    hex: bool,
    search: bool,
}

impl TerminalTools {
    fn of(terminal: &TerminalView) -> Self {
        Self {
            wrap: terminal.wrap(),
            timestamps: terminal.timestamps(),
            hex: terminal.display_mode() == DisplayMode::Hex,
            search: terminal.is_search_open(),
        }
    }
}

/// A toolbar control whose width depends on its label.
#[derive(Clone, Copy, Debug)]
enum ToolControl {
    Connection,
    Mode,
    Codec,
}

/// The labelled controls' widths as last drawn, with the label they were drawn with.
#[derive(Clone, Debug, Default)]
struct ToolWidths {
    connection: Option<(String, f32)>,
    mode: Option<f32>,
    codec: Option<(String, f32)>,
}

/// The codec button's widest.
const CODEC_MAX_WIDTH: Pixels = px(168.);

/// What the toolbar's menus show checked, as of the frame they opened in.
#[derive(Clone, Debug)]
struct ToolState {
    inline: bool,
    paused: bool,
    recording: bool,
    search: bool,
    hex: bool,
    has_hex: bool,
    timestamps: TimestampMode,
    wrap: bool,
    /// VT mode.
    vt: bool,
    decoding: bool,
    decoded_inline: bool,
    hide_framed: bool,
    /// The codec running, or [`NO_CODEC`].
    codec: String,
    /// What the codec menu lists: [`NO_CODEC`] first.
    codecs: Vec<String>,
    /// The bundled example plugins not installed, as (folder name, title).
    examples: Vec<(String, String)>,
}

impl ToolState {
    /// Whether the toolbar has a codec menu: while a plugin is installed and loaded, or
    /// while the session decodes (with a plugin since removed), so it can be turned off.
    /// With no plugin the toolbar shows nothing about codecs; the command palette offers
    /// the examples.
    fn shows_codec_menu(&self) -> bool {
        self.codecs.len() > 1 || self.decoding
    }
}

/// A menu item's click, run on the session view.
fn on_view(
    view: &WeakEntity<SessionView>,
    run: impl Fn(&mut SessionView, &mut Window, &mut Context<SessionView>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |_, window, cx| {
        view.update(cx, |view, cx| run(view, window, cx)).ok();
    }
}

/// The Export menu: the displayed text, the screen (VT mode), the raw bytes, the
/// decoded frames.
fn export_items(
    menu: PopupMenu,
    decoding: bool,
    screen: bool,
    view: &WeakEntity<SessionView>,
) -> PopupMenu {
    menu.min_w(px(200.))
        .item(
            PopupMenuItem::new("Text\u{2026}")
                .icon(IconName::FileText)
                .action(Box::new(actions::Export))
                .on_click(on_view(view, |view, _, cx| {
                    view.export(ExportFormat::Text, cx)
                })),
        )
        .when(screen, |menu| {
            menu.item(
                PopupMenuItem::new("Screen\u{2026}")
                    .icon(IconName::Terminal)
                    .on_click(on_view(view, |view, _, cx| {
                        view.export(ExportFormat::Screen, cx)
                    })),
            )
        })
        .item(
            PopupMenuItem::new("Raw bytes\u{2026}")
                .icon(IconName::Binary)
                .on_click(on_view(view, |view, _, cx| {
                    view.export(ExportFormat::Raw, cx)
                })),
        )
        .item(
            PopupMenuItem::new("Decoded frames\u{2026}")
                .icon(IconName::Braces)
                .disabled(!decoding)
                .on_click(on_view(view, |view, _, cx| {
                    view.export(ExportFormat::Csv, cx)
                })),
        )
}

/// The replay speed menu: one item per speed, the one in force checked. Each restarts the
/// replay from the beginning at that speed.
fn speed_items(
    menu: PopupMenu,
    current: Option<ReplaySpeed>,
    view: &WeakEntity<SessionView>,
) -> PopupMenu {
    replay_speeds()
        .into_iter()
        .fold(menu.min_w(px(120.)).label("Replay at"), |menu, speed| {
            menu.item(
                PopupMenuItem::new(speed.to_string())
                    .checked(current == Some(speed))
                    .on_click(on_view(view, move |view, _, cx| {
                        view.choose_replay_speed(speed, cx)
                    })),
            )
        })
}

/// The codec menu: the codecs to decode with, what decoding does to the scrollback, the
/// bundled example plugins not installed yet, and the plugins folder.
fn codec_items(
    menu: PopupMenu,
    state: &ToolState,
    view: &WeakEntity<SessionView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut menu = menu.min_w(px(220.)).label("Decode with");
    for codec in &state.codecs {
        let name = codec.clone();
        menu = menu.item(
            PopupMenuItem::new(codec.clone())
                .checked(*codec == state.codec)
                .on_click(on_view(view, move |view, _, cx| {
                    if view.codec_name().unwrap_or(NO_CODEC) != name {
                        view.set_codec(Some(&name), cx);
                    }
                })),
        );
    }
    let (decoded_inline, hide_framed) = (state.decoded_inline, state.hide_framed);
    let mut menu = menu
        .separator()
        .item(
            PopupMenuItem::new("Summaries in the scrollback")
                .checked(decoded_inline)
                .disabled(!state.decoding)
                .on_click(on_view(view, move |view, _, cx| {
                    view.set_decoded_inline(!decoded_inline, cx)
                })),
        )
        .item(
            PopupMenuItem::new("Hide framed bytes")
                .checked(hide_framed)
                .disabled(!state.decoding)
                .on_click(on_view(view, move |view, _, cx| {
                    view.set_hide_framed_bytes(!hide_framed, cx)
                })),
        )
        .separator();
    if !state.examples.is_empty() {
        let (examples, view) = (state.examples.clone(), view.clone());
        menu = menu.submenu(INSTALL_EXAMPLE_MENU, window, cx, move |menu, _, _| {
            example_items(menu, &examples, &view)
        });
    }
    menu.item(
        PopupMenuItem::new("Open plugins folder")
            .icon(IconName::FolderOpen)
            .on_click(|_, _, cx| plugin_files::open_plugins_folder(cx)),
    )
}

/// The codec menu's submenu of the bundled example plugins not installed yet.
pub const INSTALL_EXAMPLE_MENU: &str = "Install example plugin\u{2026}";

/// One item per example plugin, `(folder name, title)`, that installs it.
fn example_items(
    menu: PopupMenu,
    examples: &[(String, String)],
    view: &WeakEntity<SessionView>,
) -> PopupMenu {
    examples
        .iter()
        .fold(menu.min_w(px(200.)), |menu, (name, title)| {
            let name = name.clone();
            menu.item(
                PopupMenuItem::new(title.clone()).on_click(on_view(view, move |view, _, cx| {
                    view.install_example_plugin(&name, cx)
                })),
            )
        })
}

/// The overflow menu: the toolbar's controls that did not fit, by group.
fn overflow_menu(
    menu: PopupMenu,
    items: &[ToolbarItem],
    state: &ToolState,
    view: &WeakEntity<SessionView>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut menu = menu.min_w(px(220.));
    let mut last_group = None;
    for item in items {
        let group = ToolbarItem::GROUPS
            .iter()
            .position(|group| group.contains(item));
        if last_group.is_some() && group != last_group {
            menu = menu.separator();
        }
        last_group = group;
        menu = match item {
            ToolbarItem::Mode => menu.item(
                PopupMenuItem::new("Inline mode")
                    .checked(state.inline)
                    .action(Box::new(actions::ToggleInline))
                    .on_click(on_view(view, |view, window, cx| {
                        view.toggle_mode(window, cx)
                    })),
            ),
            ToolbarItem::Pause => menu.item(
                PopupMenuItem::new("Pause")
                    .checked(state.paused)
                    .action(Box::new(actions::Pause))
                    .on_click(on_view(view, |view, _, cx| view.toggle_pause(cx))),
            ),
            ToolbarItem::Record => menu.item(
                PopupMenuItem::new("Record\u{2026}")
                    .checked(state.recording)
                    .action(Box::new(actions::ToggleRecord))
                    .on_click(on_view(view, |view, _, cx| view.toggle_record(cx))),
            ),
            ToolbarItem::Clear => menu.item(
                PopupMenuItem::new("Clear scrollback")
                    .action(Box::new(actions::Clear))
                    .on_click(on_view(view, |view, _, cx| view.clear(cx))),
            ),
            ToolbarItem::Search => menu.item(
                PopupMenuItem::new("Search\u{2026}")
                    .checked(state.search)
                    .on_click(on_view(view, |_, window, cx| {
                        // After the menu has given the focus back, so the field keeps it.
                        let this = cx.entity().downgrade();
                        window.defer(cx, move |window, cx| {
                            this.update(cx, |view, cx| view.toggle_search(window, cx))
                                .ok();
                        });
                    })),
            ),
            ToolbarItem::Hex => menu.item(
                PopupMenuItem::new("Hex view")
                    .checked(state.hex)
                    .disabled(!state.has_hex)
                    .on_click(on_view(view, |view, _, cx| {
                        view.terminal
                            .update(cx, |terminal, cx| terminal.toggle_hex(cx))
                    })),
            ),
            ToolbarItem::Emulation => menu.item(
                PopupMenuItem::new("VT mode")
                    .checked(state.vt)
                    .action(Box::new(actions::ToggleEmulation))
                    .on_click(on_view(view, |view, _, cx| view.toggle_emulation(cx))),
            ),
            ToolbarItem::Timestamps => menu.item(
                PopupMenuItem::new(format!("Timestamps: {}", state.timestamps.label()))
                    .checked(state.timestamps != TimestampMode::Off)
                    .on_click(on_view(view, |view, _, cx| {
                        view.terminal
                            .update(cx, |terminal, cx| terminal.cycle_timestamps(cx))
                    })),
            ),
            ToolbarItem::Wrap => menu.item(
                PopupMenuItem::new("Wrap lines")
                    .checked(state.wrap)
                    .on_click(on_view(view, |view, _, cx| {
                        view.terminal
                            .update(cx, |terminal, cx| terminal.toggle_wrap(cx))
                    })),
            ),
            ToolbarItem::Export => {
                let (decoding, screen, view) = (state.decoding, state.vt, view.clone());
                menu.submenu("Export", window, cx, move |menu, _, _| {
                    export_items(menu, decoding, screen, &view)
                })
            }
            ToolbarItem::Codec => {
                let (state, view) = (state.clone(), view.clone());
                menu.submenu("Codec", window, cx, move |menu, window, cx| {
                    codec_items(menu, &state, &view, window, cx)
                })
            }
        };
    }
    menu
}

impl Render for SessionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.render_toolbar(window, cx);
        let theme = cx.theme();
        v_flex()
            .id("session-view")
            .key_context(context::SESSION_VIEW)
            .track_focus(&self.focus_handle)
            .on_key_up(cx.listener(Self::key_up))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::toggle_inline_action))
            .on_action(cx.listener(|this, _: &actions::ToggleEmulation, _, cx| {
                this.toggle_emulation(cx);
            }))
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
        FakeFeed, TestDir, allow_engine_threads, displayed, fake_session, open_test_window,
        run_until,
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
        // Let the ingest thread take in the whole burst before the view looks at any of
        // it, waiting in real time without running the view's tasks. However slowly it
        // gets through them, nothing acknowledges its wake meanwhile, so the wake it
        // rang for the first event is the only one: the count below does not depend on
        // how the ingest thread and the test's frames happen to interleave.
        let deadline = std::time::Instant::now() + crate::test_support::ENGINE_WAIT;
        // (The wake rings when the batch it belongs to is done, so wait for that too.)
        while view.read_with(cx, |v, _| {
            v.ingest_stats()
                .is_none_or(|s| s.chunks < 200 || s.wakes < 1)
        }) {
            assert!(
                std::time::Instant::now() < deadline,
                "the ingest thread never took in the burst"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            view.read_with(cx, |v, _| v.ingest_stats().unwrap().wakes),
            1
        );
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

        // Nothing new: once the byte rates in the status line have settled to nothing
        // (a window after the burst), idle housekeeping must not repaint.
        let settle = crate::status::RATE_WINDOW.as_millis() / HOUSEKEEPING.as_millis() + 2;
        for _ in 0..settle {
            cx.executor().advance_clock(HOUSEKEEPING);
            cx.run_until_parked();
        }
        assert_eq!(view.read_with(cx, |v, _| v.status_line().rx_rate), None);
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
        // Ingest sets the link state before it stores the notice line, so the view can
        // know the link is gone a wake before its snapshot has the notice.
        run_until(cx, "the disconnect notice", |cx| {
            texts(cx, &view).last().is_some_and(|(direction, text)| {
                *direction == Direction::Notice && text == "Disconnected: device disconnected"
            })
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

    /// The lines of the file a text export job writes.
    fn export_lines(
        cx: &mut TestAppContext,
        view: &Entity<SessionView>,
        path: &std::path::Path,
    ) -> (ExportJob, Vec<String>) {
        let job = view.read_with(cx, |v, cx| v.export_job(ExportFormat::Text, cx));
        job.run(path).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        (job, text.lines().map(str::to_owned).collect())
    }

    fn set_timestamps(cx: &mut TestAppContext, view: &Entity<SessionView>, mode: TimestampMode) {
        view.update(cx, |v, cx| {
            v.terminal().update(cx, |t, cx| t.set_timestamps(mode, cx));
        });
    }

    fn install_format(cx: &mut TestAppContext, dir: &TestDir, format: &str) {
        std::fs::write(
            dir.join("settings.json"),
            format!(r#"{{ "display": {{ "timestamp_format": "{format}" }} }}"#),
        )
        .unwrap();
        let paths = serialist_core::settings::ConfigPaths::new(dir.path());
        cx.update(|cx| crate::config::install(Config::load(paths, false), cx));
    }

    #[gpui_test]
    fn an_absolute_stamped_export_uses_the_configured_format(cx: &mut TestAppContext) {
        let dir = TestDir::new("export-format");
        let (_window, view, feed) = open_session_view(cx);
        feed.connected("virtual:echo");
        feed.data(b"one\r\ntwo\r\n");
        run_until(cx, "the lines", |cx| texts(cx, &view).len() == 3);
        set_timestamps(cx, &view, TimestampMode::Absolute);
        let pattern = |pattern: &str| regex::Regex::new(pattern).unwrap();

        // With no configuration the stamps are the default format, the time of day.
        let (job, lines) = export_lines(cx, &view, &dir.join("default.txt"));
        assert!(matches!(job, ExportJob::Text { .. }));
        let default = pattern(r"^\[\d\d:\d\d:\d\d\.\d{3}\] (Connected to virtual:echo|one|two)$");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|line| default.is_match(line)), "{lines:?}");

        // The configured format is what an absolute export is stamped with.
        install_format(cx, &dir, "T%S%.3f");
        let (job, lines) = export_lines(cx, &view, &dir.join("configured.txt"));
        let ExportJob::Text { options, .. } = &job else {
            panic!("a text job");
        };
        assert_eq!(options.timestamp_format.as_deref(), Some("T%S%.3f"));
        let configured = pattern(r"^\[T\d\d\.\d{3}\] (Connected to virtual:echo|one|two)$");
        assert_eq!(lines.len(), 3);
        assert!(
            lines.iter().all(|line| configured.is_match(line)),
            "{lines:?}"
        );

        // Hex rows are stamped the same way.
        view.update(cx, |v, cx| {
            v.terminal()
                .update(cx, |t, cx| t.set_display_mode(DisplayMode::Hex, cx));
        });
        let (job, rows) = export_lines(cx, &view, &dir.join("hex.txt"));
        let ExportJob::HexText { options, .. } = &job else {
            panic!("a hex job");
        };
        assert_eq!(options.timestamp_format.as_deref(), Some("T%S%.3f"));
        let hex_row = pattern(r"^\[T\d\d\.\d{3}\] [0-9a-f]{8}  ");
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| hex_row.is_match(row)), "{rows:?}");

        // Other kinds of stamp do not use the format, and a changed format applies to
        // the next export.
        view.update(cx, |v, cx| {
            v.terminal()
                .update(cx, |t, cx| t.set_display_mode(DisplayMode::Text, cx));
        });
        set_timestamps(cx, &view, TimestampMode::Relative);
        let (_, lines) = export_lines(cx, &view, &dir.join("relative.txt"));
        let relative = pattern(r"^\[\+\d+\.\d{6}\] ");
        assert!(
            lines.iter().all(|line| relative.is_match(line)),
            "{lines:?}"
        );
        install_format(cx, &dir, "%S");
        set_timestamps(cx, &view, TimestampMode::Absolute);
        let (_, lines) = export_lines(cx, &view, &dir.join("seconds.txt"));
        let seconds = pattern(r"^\[\d\d\] ");
        assert!(lines.iter().all(|line| seconds.is_match(line)), "{lines:?}");
    }
}
