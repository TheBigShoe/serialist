//! What a session view shows, as plain data: connection state, the live scrollback, the
//! raw capture, the paused snapshot, recording state and the status line.
//!
//! No GPUI here, so all of it tests as plain Rust, and milestone 1 can move storage onto
//! the page store without touching rendering.
//!
//! Pause is purely a display matter. The port, the session and the drain keep running
//! and the live buffer keeps filling and capping exactly as before; pausing pins a copy
//! of the rows shown at that moment, and the view renders that copy until resumed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serialist_core::{PortId, SerialConfig, SessionEvent, SessionStats, TransportError};

use crate::capture::{RawRing, RecorderStats, RecordingSlot};
use crate::line_buffer::{Line, LineBuffer, LineKind, LineSplitter, Row, RxText};

/// A session event after the drain worker has handled its bytes; this, not
/// [`SessionEvent`], is what reaches the main thread.
#[derive(Debug)]
pub enum SessionUpdate {
    Connected {
        description: String,
    },
    /// Every `Data` chunk between two other events: split into lines for display, and
    /// the chunks themselves (shared, not copied) for the raw capture.
    Received {
        text: RxText,
        chunks: Vec<Arc<[u8]>>,
    },
    Disconnected {
        error: Option<TransportError>,
    },
    WriteFailed(TransportError),
}

/// What the drain worker keeps between batches.
#[derive(Default)]
pub(crate) struct DrainState {
    pub splitter: LineSplitter,
    pub recording: RecordingSlot,
}

/// The drain worker's `prepare` step, on the background executor: record every chunk in
/// arrival order, then turn runs of `Data` into one `Received` each, keeping every other
/// event in its place.
pub(crate) fn prepare_updates(
    state: &mut DrainState,
    events: Vec<SessionEvent>,
) -> Vec<SessionUpdate> {
    let data = events.iter().filter_map(|event| match event {
        SessionEvent::Data { bytes, .. } => Some(&bytes[..]),
        _ => None,
    });
    // Called on idle wakes too, which keeps the recording's flush cadence.
    state.recording.record(data, Instant::now());

    fn flush(
        splitter: &mut LineSplitter,
        chunks: &mut Vec<Arc<[u8]>>,
        out: &mut Vec<SessionUpdate>,
    ) {
        if !chunks.is_empty() {
            let text = splitter.split(chunks.iter().map(|chunk| &chunk[..]));
            out.push(SessionUpdate::Received {
                text,
                chunks: std::mem::take(chunks),
            });
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
        flush(&mut state.splitter, &mut chunks, &mut out);
        out.push(update);
    }
    flush(&mut state.splitter, &mut chunks, &mut out);
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

/// A one-line outcome for the status line: an export, a recording, a failed dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// A recording in progress.
#[derive(Clone, Debug)]
pub struct RecordingStatus {
    /// Matches the recorder in the drain's [`RecordingSlot`].
    pub id: u64,
    pub path: PathBuf,
    /// `None` while the file is still being opened.
    pub stats: Option<Arc<RecorderStats>>,
}

impl RecordingStatus {
    pub fn file_name(&self) -> String {
        file_name(&self.path)
    }

    fn label(&self) -> String {
        let name = self.file_name();
        match &self.stats {
            None => format!("REC {name} opening…"),
            Some(stats) => match stats.error() {
                Some(error) => format!("REC {name} failed: {error}"),
                None => format!("REC {name} {}", format_bytes(stats.bytes())),
            },
        }
    }
}

pub(crate) fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The text of the status line for one session, kept apart from rendering so tests can
/// check what the user sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub state: &'static str,
    /// The transport's description, or the port and settings before it is known.
    pub title: String,
    /// The line settings, when the title does not already carry them.
    pub settings: Option<String>,
    pub rx: String,
    pub tx: String,
    /// `Paused, +1240 lines, +96.0 KiB` while paused.
    pub paused: Option<String>,
    /// `REC capture.bin 1.2 MiB` while recording.
    pub recording: Option<String>,
    pub notice: Option<Notice>,
}

/// The rows pinned by a pause, and where the stream stood at that moment.
#[derive(Clone, Debug)]
struct Frozen {
    rows: Vec<Line>,
    rx_lines_at: u64,
    rx_bytes_at: u64,
}

/// Everything the view shows, without GPUI.
#[derive(Clone, Debug)]
pub struct SessionModel {
    pub port: PortId,
    pub serial: SerialConfig,
    /// The transport's own label, known once `Connected` arrives.
    pub description: Option<String>,
    pub state: ConnectionState,
    pub stats: SessionStats,
    /// The live scrollback. It keeps filling while paused.
    pub buffer: LineBuffer,
    /// Received bytes as delivered, for raw export.
    pub raw: RawRing,
    pub recording: Option<RecordingStatus>,
    pub notice: Option<Notice>,
    /// Received lines completed since the session started, evicted ones included.
    rx_lines: u64,
    frozen: Option<Frozen>,
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
            raw: RawRing::default(),
            recording: None,
            notice: None,
            rx_lines: 0,
            frozen: None,
        }
    }

    /// Replace the raw capture with an empty one of `capacity` bytes.
    pub fn with_raw_capacity(mut self, capacity: usize) -> Self {
        self.raw = RawRing::new(capacity);
        self
    }

    /// What the status line names the session by.
    pub fn title(&self) -> String {
        self.description
            .clone()
            .unwrap_or_else(|| format!("{} @ {}", self.port, self.serial.summary()))
    }

    /// Apply one update. Returns whether anything visible (rows or the status line)
    /// changed.
    pub fn apply(&mut self, update: SessionUpdate) -> bool {
        match update {
            SessionUpdate::Connected { description } => {
                self.buffer
                    .push_line(LineKind::Info, &format!("Connected to {description}"));
                self.description = Some(description);
                self.state = ConnectionState::Connected;
            }
            SessionUpdate::Received { text, chunks } => {
                let before = self.raw.end_offset();
                for chunk in chunks {
                    self.raw.push(chunk);
                }
                self.rx_lines += text.lines.len() as u64;
                let rows_changed = self.buffer.apply_rx(text);
                // While paused the rows stand still, but the paused counters move.
                let counters_changed = self.frozen.is_some() && self.raw.end_offset() > before;
                return rows_changed || counters_changed;
            }
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

    /// Pin the rows shown now. Returns false if already paused.
    pub fn pause(&mut self) -> bool {
        if self.frozen.is_some() {
            return false;
        }
        let rows = self
            .buffer
            .rows()
            .map(|row| Line {
                kind: row.kind,
                text: row.text.to_owned(),
            })
            .collect();
        self.frozen = Some(Frozen {
            rows,
            rx_lines_at: self.rx_lines,
            rx_bytes_at: self.raw.end_offset(),
        });
        true
    }

    /// Drop the pinned rows and show the live buffer again. Returns false if not paused.
    pub fn resume(&mut self) -> bool {
        self.frozen.take().is_some()
    }

    pub fn is_paused(&self) -> bool {
        self.frozen.is_some()
    }

    /// Lines and bytes received since the pause, or `None` when not paused.
    pub fn received_since_pause(&self) -> Option<(u64, u64)> {
        self.frozen.as_ref().map(|frozen| {
            (
                self.rx_lines - frozen.rx_lines_at,
                self.raw.end_offset() - frozen.rx_bytes_at,
            )
        })
    }

    /// Received lines completed since the session started, evicted ones included.
    pub fn rx_lines(&self) -> u64 {
        self.rx_lines
    }

    /// Rows on screen: the pinned rows when paused, else the live buffer.
    pub fn displayed_len(&self) -> usize {
        match &self.frozen {
            Some(frozen) => frozen.rows.len(),
            None => self.buffer.len(),
        }
    }

    pub fn displayed_row(&self, ix: usize) -> Option<Row<'_>> {
        match &self.frozen {
            Some(frozen) => frozen.rows.get(ix).map(|line| Row {
                kind: line.kind,
                text: &line.text,
            }),
            None => self.buffer.row(ix),
        }
    }

    pub fn displayed_rows(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        (0..self.displayed_len()).filter_map(|ix| self.displayed_row(ix))
    }

    /// The displayed rows as text, which is what a text export writes.
    pub fn displayed_text(&self) -> Vec<String> {
        self.displayed_rows()
            .map(|row| row.text.to_owned())
            .collect()
    }

    /// Clear the scrollback: the live buffer and, when paused, the pinned rows. The raw
    /// capture is left alone; it is a record of the stream, not of the screen.
    pub fn clear(&mut self) {
        self.buffer.clear();
        if let Some(frozen) = &mut self.frozen {
            frozen.rows.clear();
        }
    }

    /// The status line's text for this session.
    pub fn status_line(&self) -> StatusLine {
        let title = self.title();
        let settings = self.serial.summary();
        StatusLine {
            state: self.state.label(),
            // Serial transports already put the line settings in their description.
            settings: (!title.contains(&settings)).then_some(settings),
            title,
            rx: format!("RX {}", format_bytes(self.stats.rx_bytes)),
            tx: format!("TX {}", format_bytes(self.stats.tx_bytes)),
            paused: self
                .received_since_pause()
                .map(|(lines, bytes)| format!("Paused, +{lines} lines, +{}", format_bytes(bytes))),
            recording: self.recording.as_ref().map(RecordingStatus::label),
            notice: self.notice.clone(),
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(model: &SessionModel) -> Vec<(LineKind, String)> {
        model
            .buffer
            .rows()
            .map(|r| (r.kind, r.text.to_owned()))
            .collect()
    }

    fn shown(model: &SessionModel) -> Vec<String> {
        model.displayed_text()
    }

    fn data(bytes: &[u8]) -> SessionEvent {
        SessionEvent::Data {
            bytes: Arc::from(bytes),
            received_at: Instant::now(),
        }
    }

    /// Run events through the worker step and the model, as the drain loop does.
    fn apply(model: &mut SessionModel, state: &mut DrainState, events: Vec<SessionEvent>) -> bool {
        prepare_updates(state, events)
            .into_iter()
            .fold(false, |changed, update| model.apply(update) | changed)
    }

    fn model() -> SessionModel {
        SessionModel::new(PortId::new("virtual:echo"), SerialConfig::default())
    }

    #[test]
    fn prepare_merges_data_runs_and_keeps_event_order() {
        let mut state = DrainState::default();
        let updates = prepare_updates(
            &mut state,
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
                SessionUpdate::Received { text, chunks } => format!(
                    "rx {:?} {:?} in {} chunks",
                    text.lines,
                    text.partial,
                    chunks.len()
                ),
                SessionUpdate::Disconnected { .. } => "disconnected".to_owned(),
                SessionUpdate::WriteFailed(_) => "write failed".to_owned(),
            })
            .collect();
        assert_eq!(
            summary,
            [
                "connected",
                r#"rx ["a", "bc"] "" in 2 chunks"#,
                "write failed",
                r#"rx [] "tail" in 1 chunks"#,
            ]
        );
    }

    #[test]
    fn model_tracks_connection_state() {
        let mut state = DrainState::default();
        let mut model = model();
        assert_eq!(model.title(), "virtual:echo @ 115200 8N1");
        assert!(apply(
            &mut model,
            &mut state,
            vec![SessionEvent::Connected {
                description: "virtual:echo".into()
            }]
        ));
        assert_eq!(model.state, ConnectionState::Connected);
        assert_eq!(model.title(), "virtual:echo");
        assert!(apply(&mut model, &mut state, vec![data(b"hi\r\n")]));
        assert!(
            !apply(&mut model, &mut state, vec![data(b"")]),
            "empty chunks change nothing"
        );
        assert!(apply(
            &mut model,
            &mut state,
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
                &mut state,
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
    fn received_chunks_land_in_the_raw_capture_unchanged() {
        let mut state = DrainState::default();
        let mut model = model();
        apply(
            &mut model,
            &mut state,
            vec![data(b"\x00ab\r"), data(b"\ncd\xff")],
        );
        let raw: Vec<u8> = model.raw.chunks().flat_map(|c| c.to_vec()).collect();
        assert_eq!(raw, b"\x00ab\r\ncd\xff");
        assert_eq!(model.raw.end_offset(), 8);
        assert_eq!(model.rx_lines(), 1);
    }

    #[test]
    fn pause_pins_the_rows_while_the_live_buffer_keeps_going() {
        let mut state = DrainState::default();
        let mut model = model();
        apply(&mut model, &mut state, vec![data(b"one\ntwo\nthr")]);
        assert!(model.pause());
        assert!(!model.pause(), "already paused");
        let pinned = shown(&model);
        assert_eq!(pinned, ["one", "two", "thr"]);

        assert!(
            apply(&mut model, &mut state, vec![data(b"ee\nfour\nfi")]),
            "the paused counters count as a visible change"
        );
        assert_eq!(shown(&model), pinned, "the screen stands still");
        assert_eq!(
            model.buffer.rows().map(|r| r.text).collect::<Vec<_>>(),
            ["one", "two", "three", "four", "fi"],
            "the live buffer does not"
        );
        assert_eq!(model.received_since_pause(), Some((2, 10)));
        assert_eq!(
            model.status_line().paused.as_deref(),
            Some("Paused, +2 lines, +10 B")
        );

        assert!(model.resume());
        assert!(!model.resume());
        assert_eq!(model.received_since_pause(), None);
        assert_eq!(model.status_line().paused, None);
        assert_eq!(shown(&model), ["one", "two", "three", "four", "fi"]);
    }

    #[test]
    fn the_pinned_rows_are_capped_like_the_buffer() {
        let mut state = DrainState::default();
        let mut model = model();
        model.buffer = LineBuffer::new(3);
        apply(&mut model, &mut state, vec![data(b"1\n2\n3\n4\n5\n")]);
        model.pause();
        apply(&mut model, &mut state, vec![data(b"6\n7\n")]);
        assert_eq!(shown(&model), ["3", "4", "5"]);
        model.resume();
        assert_eq!(shown(&model), ["5", "6", "7"]);
    }

    #[test]
    fn clearing_while_paused_empties_the_screen_but_not_the_capture() {
        let mut state = DrainState::default();
        let mut model = model();
        apply(&mut model, &mut state, vec![data(b"a\nb\n")]);
        model.pause();
        model.clear();
        assert!(shown(&model).is_empty());
        assert!(model.is_paused());
        assert_eq!(model.raw.end_offset(), 4);
    }

    #[test]
    fn status_line_shows_recording_and_notices() {
        let mut model = model();
        model.recording = Some(RecordingStatus {
            id: 1,
            path: PathBuf::from("/tmp/capture.bin"),
            stats: None,
        });
        assert_eq!(
            model.status_line().recording.as_deref(),
            Some("REC capture.bin opening…")
        );
        model.notice = Some(Notice::error("Export failed"));
        assert_eq!(
            model.status_line().notice,
            Some(Notice::error("Export failed"))
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
}
