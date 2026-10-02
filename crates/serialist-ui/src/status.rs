//! What the status line says about a session, as plain data: connection state, byte
//! counters, the scrollback's retention, pause, recording, a running script and the
//! last notice.
//!
//! No GPUI here, so the wording tests as plain Rust. The session view fills a
//! [`StatusInputs`] from the session's counters and the newest store snapshot, and
//! [`StatusLine::new`] turns it into text.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialist_core::composite::scheme_of;
use serialist_core::{
    Emulation, PortAddress, PortId, REPLAY_SCHEME, ReplayAddress, ReplaySpeed, SessionStats,
    StoreStats, TCP_SCHEME,
};

use crate::capture::RecorderStats;
use crate::inline::{Mode, PasteProgress};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connected,
    Disconnected {
        /// `None` after an orderly close.
        error: Option<String>,
    },
}

impl ConnectionState {
    pub fn label(&self) -> &'static str {
        match self {
            ConnectionState::Connected => "Connected",
            ConnectionState::Disconnected { error: None } => "Disconnected",
            ConnectionState::Disconnected { error: Some(_) } => "Connection lost",
        }
    }

    pub fn is_disconnected(&self) -> bool {
        matches!(self, ConnectionState::Disconnected { .. })
    }
}

/// What a session's port is, for the parts of the window that differ by transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// An OS serial port or a simulated device: the line settings, DTR, RTS and break
    /// all mean something.
    Serial,
    /// `tcp:<host>:<port>`: a raw byte stream. It has no line settings, control lines or
    /// break (its port settings keep the line ending and local echo).
    Tcp,
    /// `replay:<file>`: a recorded capture played back, whose speed the toolbar sets.
    Replay,
}

impl LinkKind {
    /// The kind `port`'s scheme names; anything that is not `tcp:` or `replay:` is
    /// treated as a serial port.
    pub fn of(port: &PortId) -> Self {
        match scheme_of(port) {
            Some(TCP_SCHEME) => Self::Tcp,
            Some(REPLAY_SCHEME) => Self::Replay,
            _ => Self::Serial,
        }
    }

    /// Whether the line settings (baud, framing, flow control) and the control lines
    /// are the session's to set.
    pub fn has_line_settings(self) -> bool {
        self == Self::Serial
    }
}

/// How a replay's description ends when its capture has no timing sidecar, and the baud
/// rate paces it: `replay:dump.bin (4x, no timing)`.
const NO_TIMING_END: &str = "no timing)";

/// Whether the status line names the line settings beside a session. A serial port
/// always does. A TCP stream has none to show. A replay has them only without a timing
/// sidecar, where the baud is what paces it (its description says so).
pub fn shows_line_settings(kind: LinkKind, description: Option<&str>) -> bool {
    match kind {
        LinkKind::Serial => true,
        LinkKind::Tcp => false,
        LinkKind::Replay => description.is_some_and(|text| text.ends_with(NO_TIMING_END)),
    }
}

/// What a `tcp:` or `replay:` port is called in a tab: `host:port`, or the capture's
/// file name. `None` for any other port, and for an id that breaks its grammar.
pub fn transport_title(port: &PortId) -> Option<String> {
    match PortAddress::parse(port) {
        Ok(PortAddress::Tcp(address)) => Some(address.authority()),
        Ok(PortAddress::Replay(address)) => Some(file_name(address.path())),
        _ => None,
    }
}

/// The speed a replay's description names: `replay:boot.bin (4x)` and
/// `replay:dump.bin (4x, no timing)` are both 4x.
pub fn replay_speed_in(description: &str) -> Option<ReplaySpeed> {
    let (_, tail) = description.rsplit_once(" (")?;
    let inside = tail.strip_suffix(')')?;
    inside.split(',').next()?.trim().parse().ok()
}

/// The speed the replay on `port` plays at: the one its description names (the one in
/// force, whether the id or the settings chose it), else the id's own. `None` until the
/// link has connected, for an id with no `?speed=`.
pub fn replay_speed_of(port: &PortId, description: Option<&str>) -> Option<ReplaySpeed> {
    description
        .and_then(replay_speed_in)
        .or_else(|| ReplayAddress::from_port_id(port).ok()?.speed)
}

/// What the toolbar's speed control says for the replay on `port`: its speed, or a word
/// until it is known.
pub fn replay_speed_label(port: &PortId, description: Option<&str>) -> String {
    replay_speed_of(port, description).map_or_else(|| "Speed".to_owned(), |speed| speed.to_string())
}

/// The speeds the replay menu offers, slowest first, `max` last.
pub fn replay_speeds() -> Vec<ReplaySpeed> {
    [0.25, 0.5, 1.0, 2.0, 4.0, 10.0, 100.0]
        .into_iter()
        .filter_map(ReplaySpeed::times)
        .chain([ReplaySpeed::Max])
        .collect()
}

/// A one-line outcome for the status line: an export, a recording, a failed dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub is_error: bool,
    /// What a button beside the text does, when the notice offers a way out.
    pub action: Option<NoticeAction>,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            action: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            action: None,
        }
    }

    /// This notice with a button that does `action`.
    pub fn with_action(mut self, action: NoticeAction) -> Self {
        self.action = Some(action);
        self
    }
}

/// What a notice's button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    /// Install the bundled example plugin of this name (its folder under `plugins/`).
    InstallExamplePlugin(String),
    /// Open the plugins folder.
    OpenPluginsFolder,
}

impl NoticeAction {
    /// The button's label.
    pub fn label(&self) -> &'static str {
        match self {
            NoticeAction::InstallExamplePlugin(_) => "Install",
            NoticeAction::OpenPluginsFolder => "Open plugins folder",
        }
    }
}

/// A recording in progress.
#[derive(Clone, Debug)]
pub struct RecordingStatus {
    /// Matches the recorder in the session's [`RecordingSlot`](crate::RecordingSlot).
    pub id: u64,
    pub path: PathBuf,
    /// `None` while the file is still being opened.
    pub stats: Option<Arc<RecorderStats>>,
}

impl RecordingStatus {
    pub fn file_name(&self) -> String {
        file_name(&self.path)
    }

    pub fn label(&self) -> String {
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

pub(crate) fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The script running on a session, or about to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptStatus {
    /// Its path under the scripts folder, or `inline`.
    pub name: String,
    /// How long it has run; `None` until the script thread has started it.
    pub running_for: Option<Duration>,
    /// Runs queued behind it.
    pub queued: usize,
}

impl ScriptStatus {
    /// `Script: version_probe.lua running 3.2 s`, with `(+1 queued)` when others wait.
    pub fn label(&self) -> String {
        let state = match self.running_for {
            Some(elapsed) => format!("running {}", format_seconds(elapsed)),
            None => "starting".to_owned(),
        };
        let queued = if self.queued > 0 {
            format!(" (+{} queued)", self.queued)
        } else {
            String::new()
        };
        format!("Script: {} {state}{queued}", self.name)
    }
}

/// Seconds with one decimal: `3.2 s`.
pub fn format_seconds(elapsed: Duration) -> String {
    format!("{:.1} s", elapsed.as_secs_f64())
}

/// Where the stream stood when the view was paused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PauseMark {
    /// One past the newest line then (the store's `end_line`).
    pub lines: u64,
    /// Raw bytes received by then (the store's `raw_len`).
    pub bytes: u64,
}

impl PauseMark {
    pub fn at(stats: &StoreStats) -> Self {
        Self {
            lines: stats.end_line.0,
            bytes: stats.raw_len,
        }
    }

    /// Lines and bytes the store took in since the mark.
    pub fn since(&self, stats: &StoreStats) -> (u64, u64) {
        (
            stats.end_line.0.saturating_sub(self.lines),
            stats.raw_len.saturating_sub(self.bytes),
        )
    }
}

/// How long the byte rates look back.
pub const RATE_WINDOW: Duration = Duration::from_secs(2);

/// Byte rates over the last [`RATE_WINDOW`], from counter samples taken on a timer.
#[derive(Clone, Debug, Default)]
pub struct RateMeter {
    samples: VecDeque<(Instant, u64, u64)>,
}

/// Received and sent bytes per second.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub rx: f64,
    pub tx: f64,
}

impl RateMeter {
    /// Take a sample of the counters at `now`, and forget the ones the window no longer
    /// needs (it keeps the newest sample older than the window, to measure from).
    pub fn record(&mut self, now: Instant, rx: u64, tx: u64) {
        if let Some((last, ..)) = self.samples.back()
            && *last > now
        {
            self.samples.clear();
        }
        self.samples.push_back((now, rx, tx));
        while self.samples.len() > 2
            && self
                .samples
                .get(1)
                .is_some_and(|(at, ..)| now.duration_since(*at) >= RATE_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// The rates across the samples kept: zero until two samples a tenth of a second
    /// apart, and zero again once the counters have not moved for a whole window.
    pub fn rates(&self) -> Rates {
        let (Some((first, rx0, tx0)), Some((last, rx1, tx1))) =
            (self.samples.front(), self.samples.back())
        else {
            return Rates::default();
        };
        let span = last.duration_since(*first).as_secs_f64();
        if span < 0.1 {
            return Rates::default();
        }
        Rates {
            rx: rx1.saturating_sub(*rx0) as f64 / span,
            tx: tx1.saturating_sub(*tx0) as f64 / span,
        }
    }

    /// Start over, as a reconnect does.
    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// `1.2 KiB/s`; `None` for no traffic.
pub fn format_rate(bytes_per_second: f64) -> Option<String> {
    let rounded = bytes_per_second.round();
    (rounded >= 1.0).then(|| format!("{}/s", format_bytes(rounded as u64)))
}

/// Everything the status line is made from.
#[derive(Clone, Debug)]
pub struct StatusInputs<'a> {
    pub state: &'a ConnectionState,
    /// What the session is called: `virtual:at @ 115200 8N1`.
    pub title: String,
    /// The line settings, for a session that has some to show (see
    /// [`shows_line_settings`]); left out when the title already carries them.
    pub settings: Option<String>,
    pub session: SessionStats,
    pub store: StoreStats,
    pub paused: Option<PauseMark>,
    pub recording: Option<&'a RecordingStatus>,
    pub notice: Option<&'a Notice>,
    /// Inline or command mode.
    pub mode: Mode,
    /// A paste in progress in inline mode.
    pub paste: Option<PasteProgress>,
    /// The script running on the session.
    pub script: Option<&'a ScriptStatus>,
    /// The codec decoding the session, if any.
    pub codec: Option<&'a str>,
    /// Bytes per second each way, lately.
    pub rates: Rates,
    /// Monitor or VT mode.
    pub emulation: Emulation,
}

/// The text of the status line for one session, kept apart from rendering so tests can
/// check what the user sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusLine {
    pub state: &'static str,
    /// The port and its line settings.
    pub title: String,
    /// The line settings, when the title does not already carry them.
    pub settings: Option<String>,
    /// `RX 1.2 MiB`: bytes the session read from the port.
    pub rx: String,
    /// `↓ 1.2 KiB/s`: the receive rate, while bytes arrive.
    pub rx_rate: Option<String>,
    /// `TX 4 B`: bytes the session wrote.
    pub tx: String,
    /// `↑ 12 B/s`: the send rate, while bytes go out.
    pub tx_rate: Option<String>,
    /// `10000 lines, 1.2 MiB kept`: what the scrollback retains now.
    pub retained: String,
    /// `evicted 3000 lines, 512.0 KiB` once the store's budget has dropped anything.
    pub evicted: Option<String>,
    /// `Paused, +1240 lines, +96.0 KiB` while paused.
    pub paused: Option<String>,
    /// `REC capture.bin 1.2 MiB` while recording.
    pub recording: Option<String>,
    pub notice: Option<Notice>,
    /// `INLINE` or `COMMAND`.
    pub mode: &'static str,
    /// `Pasting 128 B of 4.0 KiB` while a large paste is being sent.
    pub paste: Option<String>,
    /// `Script: version_probe.lua running 3.2 s` while a script runs.
    pub script: Option<String>,
    /// `Codec: airoha-race` while a codec decodes the session.
    pub codec: Option<String>,
    /// `VT` while the session shows a terminal screen (VT mode).
    pub emulation: Option<&'static str>,
}

impl StatusLine {
    pub fn new(inputs: StatusInputs<'_>) -> Self {
        let store = &inputs.store;
        let evicted = (store.evicted_lines > 0 || store.raw_start > 0).then(|| {
            format!(
                "evicted {} lines, {}",
                store.evicted_lines,
                format_bytes(store.raw_start)
            )
        });
        Self {
            state: inputs.state.label(),
            settings: inputs
                .settings
                .filter(|settings| !inputs.title.contains(settings)),
            title: inputs.title,
            rx: format!("RX {}", format_bytes(inputs.session.rx_bytes)),
            rx_rate: format_rate(inputs.rates.rx).map(|rate| format!("\u{2193} {rate}")),
            tx: format!("TX {}", format_bytes(inputs.session.tx_bytes)),
            tx_rate: format_rate(inputs.rates.tx).map(|rate| format!("\u{2191} {rate}")),
            retained: format!(
                "{} lines, {} kept",
                store.lines(),
                format_bytes(store.retained_bytes())
            ),
            evicted,
            paused: inputs.paused.map(|mark| {
                let (lines, bytes) = mark.since(store);
                format!("Paused, +{lines} lines, +{}", format_bytes(bytes))
            }),
            recording: inputs.recording.map(RecordingStatus::label),
            notice: inputs.notice.cloned(),
            mode: inputs.mode.label(),
            paste: inputs.paste.and_then(|paste| paste.label()),
            script: inputs.script.map(ScriptStatus::label),
            codec: inputs.codec.map(|codec| format!("Codec: {codec}")),
            emulation: (inputs.emulation == Emulation::Vt).then(|| inputs.emulation.label()),
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
    use serialist_core::LineId;

    use super::*;

    fn store(first: u64, end: u64, raw_start: u64, raw_len: u64) -> StoreStats {
        StoreStats {
            first_line: LineId(first),
            end_line: LineId(end),
            raw_start,
            raw_len,
            memory: 0,
            budget: 0,
            pages: 0,
            blocks: 0,
            text_pages: 0,
            text_bytes: 0,
            evicted_lines: first,
        }
    }

    fn inputs<'a>(state: &'a ConnectionState, store: StoreStats) -> StatusInputs<'a> {
        StatusInputs {
            state,
            title: "virtual:echo @ 115200 8N1".into(),
            settings: Some("115200 8N1".into()),
            session: SessionStats {
                rx_bytes: 2048,
                tx_bytes: 4,
                rx_chunks: 3,
            },
            store,
            paused: None,
            recording: None,
            notice: None,
            mode: Mode::Command,
            paste: None,
            script: None,
            codec: None,
            rates: Rates::default(),
            emulation: Emulation::Monitor,
        }
    }

    #[test]
    fn vt_mode_has_a_chip() {
        let state = ConnectionState::Connected;
        let mut status = inputs(&state, store(0, 3, 0, 20));
        assert_eq!(StatusLine::new(status.clone()).emulation, None);
        status.emulation = Emulation::Vt;
        assert_eq!(StatusLine::new(status).emulation, Some("VT"));
    }

    #[test]
    fn the_active_codec_is_named() {
        let state = ConnectionState::Connected;
        let mut status = inputs(&state, store(0, 3, 0, 20));
        assert_eq!(StatusLine::new(status.clone()).codec, None);
        status.codec = Some("airoha-race");
        assert_eq!(
            StatusLine::new(status).codec.as_deref(),
            Some("Codec: airoha-race")
        );
    }

    #[test]
    fn counters_retention_and_eviction() {
        let state = ConnectionState::Connected;
        let line = StatusLine::new(inputs(&state, store(0, 3, 0, 20)));
        assert_eq!(line.state, "Connected");
        assert_eq!(line.settings, None, "already in the title");
        assert_eq!(line.rx, "RX 2.0 KiB");
        assert_eq!(line.tx, "TX 4 B");
        assert_eq!(line.retained, "3 lines, 20 B kept");
        assert_eq!(line.evicted, None);
        assert_eq!(line.mode, "COMMAND");
        assert_eq!(line.paste, None);

        let line = StatusLine::new(inputs(&state, store(100, 150, 65536, 69632)));
        assert_eq!(line.retained, "50 lines, 4.0 KiB kept");
        assert_eq!(line.evicted.as_deref(), Some("evicted 100 lines, 64.0 KiB"));
    }

    #[test]
    fn pause_counts_what_arrived_since() {
        let state = ConnectionState::Connected;
        let mark = PauseMark::at(&store(0, 3, 0, 12));
        let mut status = inputs(&state, store(0, 5, 0, 22));
        status.paused = Some(mark);
        assert_eq!(
            StatusLine::new(status).paused.as_deref(),
            Some("Paused, +2 lines, +10 B")
        );
    }

    #[test]
    fn states_recording_and_notices() {
        let lost = ConnectionState::Disconnected {
            error: Some("device disconnected".into()),
        };
        assert_eq!(lost.label(), "Connection lost");
        assert!(lost.is_disconnected());
        assert_eq!(
            ConnectionState::Disconnected { error: None }.label(),
            "Disconnected"
        );

        let recording = RecordingStatus {
            id: 1,
            path: PathBuf::from("/tmp/capture.bin"),
            stats: None,
        };
        let notice = Notice::error("Export failed");
        let mut status = inputs(&lost, store(0, 0, 0, 0));
        status.recording = Some(&recording);
        status.notice = Some(&notice);
        status.title = "/dev/cu.usbserial".into();
        let line = StatusLine::new(status);
        assert_eq!(line.recording.as_deref(), Some("REC capture.bin opening…"));
        assert_eq!(line.notice, Some(Notice::error("Export failed")));
        assert_eq!(line.settings.as_deref(), Some("115200 8N1"));
    }

    #[test]
    fn inline_mode_and_a_large_paste() {
        let state = ConnectionState::Connected;
        let mut status = inputs(&state, store(0, 3, 0, 20));
        status.mode = Mode::Inline;
        status.paste = Some(PasteProgress {
            sent: 640,
            total: 4096,
            chunks: 64,
        });
        let line = StatusLine::new(status);
        assert_eq!(line.mode, "INLINE");
        assert_eq!(line.paste.as_deref(), Some("Pasting 640 B of 4.0 KiB"));
    }

    #[test]
    fn a_running_script_names_itself_and_its_time() {
        let state = ConnectionState::Connected;
        let running = ScriptStatus {
            name: "version_probe.lua".into(),
            running_for: Some(Duration::from_millis(3240)),
            queued: 0,
        };
        let mut status = inputs(&state, store(0, 3, 0, 20));
        status.script = Some(&running);
        assert_eq!(
            StatusLine::new(status.clone()).script.as_deref(),
            Some("Script: version_probe.lua running 3.2 s")
        );
        let starting = ScriptStatus {
            running_for: None,
            queued: 2,
            ..running.clone()
        };
        status.script = Some(&starting);
        assert_eq!(
            StatusLine::new(status).script.as_deref(),
            Some("Script: version_probe.lua starting (+2 queued)")
        );
        assert_eq!(
            StatusLine::new(inputs(&state, store(0, 3, 0, 20))).script,
            None
        );
    }

    #[test]
    fn rates_follow_the_counters_and_settle_to_nothing() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut meter = RateMeter::default();
        meter.record(at(0), 0, 0);
        assert_eq!(meter.rates(), Rates::default(), "one sample is no rate");
        meter.record(at(250), 512, 0);
        meter.record(at(500), 1024, 10);
        let rates = meter.rates();
        assert_eq!(rates.rx, 2048.0);
        assert_eq!(rates.tx, 20.0);
        let state = ConnectionState::Connected;
        let mut status = inputs(&state, store(0, 3, 0, 20));
        status.rates = rates;
        let line = StatusLine::new(status);
        assert_eq!(line.rx_rate.as_deref(), Some("\u{2193} 2.0 KiB/s"));
        assert_eq!(line.tx_rate.as_deref(), Some("\u{2191} 20 B/s"));

        // Quiet for a whole window: nothing moves, and the rate says so.
        for ms in (750..=3000).step_by(250) {
            meter.record(at(ms), 1024, 10);
        }
        assert_eq!(meter.rates(), Rates::default());
        assert_eq!(format_rate(0.4), None);
        let line = StatusLine::new(inputs(&state, store(0, 3, 0, 20)));
        assert_eq!(line.rx_rate, None);
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

    #[test]
    fn the_port_id_says_what_kind_of_link_it_is() {
        let kind = |id: &str| LinkKind::of(&PortId::new(id));
        assert_eq!(kind("/dev/cu.usbserial-1420"), LinkKind::Serial);
        assert_eq!(kind("COM3"), LinkKind::Serial);
        assert_eq!(kind("virtual:at"), LinkKind::Serial);
        assert_eq!(kind("tcp:10.0.0.5:4000"), LinkKind::Tcp);
        assert_eq!(kind("replay:/tmp/boot.bin?speed=4x"), LinkKind::Replay);
        assert!(LinkKind::Serial.has_line_settings());
        assert!(!LinkKind::Tcp.has_line_settings());
        assert!(!LinkKind::Replay.has_line_settings());
    }

    #[test]
    fn line_settings_show_for_serial_ports_and_replays_without_timing() {
        let shows = shows_line_settings;
        assert!(shows(LinkKind::Serial, Some("virtual:at @ 115200 8N1")));
        assert!(shows(LinkKind::Serial, None));
        assert!(!shows(LinkKind::Tcp, Some("tcp:host:4000 (10.0.0.5:4000)")));
        assert!(!shows(LinkKind::Tcp, None));
        assert!(!shows(LinkKind::Replay, Some("replay:boot.bin (4x)")));
        assert!(!shows(LinkKind::Replay, None));
        assert!(shows(
            LinkKind::Replay,
            Some("replay:dump.bin (4x, no timing)")
        ));
    }

    #[test]
    fn tcp_and_replay_tabs_are_named_for_their_endpoint_and_file() {
        let title = |id: &str| transport_title(&PortId::new(id));
        assert_eq!(title("tcp:10.0.0.5:4000").as_deref(), Some("10.0.0.5:4000"));
        assert_eq!(title("tcp://host:4000").as_deref(), Some("host:4000"));
        assert_eq!(title("tcp:[::1]:4000").as_deref(), Some("[::1]:4000"));
        assert_eq!(
            title("replay:/captures/boot.bin?speed=4x").as_deref(),
            Some("boot.bin")
        );
        assert_eq!(title("tcp:host"), None, "a malformed id has no title");
        assert_eq!(title("replay:"), None);
        assert_eq!(title("/dev/cu.usbserial-1420"), None);
        assert_eq!(title("virtual:at"), None);
    }

    #[test]
    fn a_replays_speed_is_read_from_its_description_then_its_id() {
        assert_eq!(
            replay_speed_in("replay:boot.bin (4x)"),
            ReplaySpeed::times(4.0)
        );
        assert_eq!(
            replay_speed_in("replay:dump.bin (0.5x, no timing)"),
            ReplaySpeed::times(0.5)
        );
        assert_eq!(
            replay_speed_in("replay:boot (1).bin (max)"),
            Some(ReplaySpeed::Max),
            "a file name with parentheses does not confuse it"
        );
        assert_eq!(replay_speed_in("replay:boot.bin"), None);
        assert_eq!(replay_speed_in("tcp:host:4000 (10.0.0.5:4000)"), None);

        let port = PortId::new("replay:/c/boot.bin?speed=2x");
        assert_eq!(
            replay_speed_label(&port, Some("replay:boot.bin (max)")),
            "max"
        );
        assert_eq!(replay_speed_label(&port, None), "2x");
        assert_eq!(
            replay_speed_label(&PortId::new("replay:/c/boot.bin"), None),
            "Speed",
            "nothing says which until the link connects"
        );
        let labels: Vec<String> = replay_speeds().iter().map(ToString::to_string).collect();
        assert_eq!(
            labels,
            ["0.25x", "0.5x", "1x", "2x", "4x", "10x", "100x", "max"]
        );
    }

    #[test]
    fn a_session_without_line_settings_names_none() {
        let state = ConnectionState::Connected;
        let mut status = inputs(&state, store(0, 3, 0, 20));
        status.title = "tcp:10.0.0.5:4000".into();
        status.settings = None;
        let line = StatusLine::new(status);
        assert_eq!(line.title, "tcp:10.0.0.5:4000");
        assert_eq!(line.settings, None);
    }
}
