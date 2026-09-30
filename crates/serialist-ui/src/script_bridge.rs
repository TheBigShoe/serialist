//! The app's side of the script host: what a session's scripts see, and the run queue
//! a session view drives. No GPUI here; the session view marshals everything that
//! needs the main thread (dialogs, notices, saved commands) and the console renders
//! the lines.
//!
//! # Threads
//!
//! Scripts run on the host's own thread (`serialist-script`), so every trait method
//! here is called there. None of them touches GPUI:
//!
//! - [`GuiScriptSession`] writes through the session's [`SessionControl`] (the writer
//!   thread's queue) and reads the session's store, woken by the [`LineBell`] that the
//!   ingest thread rings through [`ScriptLink`], one of its sinks. No byte of a script's
//!   reads or writes crosses the main thread.
//! - [`GuiScriptUi`] and [`GuiCommands`] only send a [`UiRequest`] down a channel. The
//!   session view drains it, with the run's own [`ScriptEvent`]s, once a frame while a
//!   script is active ([`SessionScripts::poll`]), on the main thread. A prompt's answer
//!   goes back through a one-slot channel whose receiver is the future the script
//!   awaits, so a waiting script suspends and never blocks its thread.
//!
//! # Order in the console
//!
//! A `log.info(...)` call reaches the app twice: as [`ScriptUi::log`] (with its level)
//! and then as a [`ScriptEvent::Output`] line `"[info] ..."`. The log call is made
//! first, on the same thread, so when the output line is read its log request is
//! already in the channel: the line is shown once, colored by the level. Notices are
//! shown when their request is read, which may be up to a frame's worth of output
//! earlier or later than where the script made them.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Mutex, RwLock};
use serialist_core::{
    ChunkSink, Command, CommandRef, CommandStore, ControlLine, LineEnding, ParamValues, PortId,
    PortSource, SerialConfig, SessionClosed, StoreReader,
};
use serialist_script::{
    BellSink, CommandError, CommandSender, HostServices, Limits, LineBell, LogLevel, PromptFuture,
    RunId, ScriptEvent, ScriptHost, ScriptOutcome, ScriptRun, ScriptSession, ScriptSource,
    ScriptUi,
};

use crate::session_handle::SessionControl;
use crate::status::{ScriptStatus, format_seconds};

/// The script name a REPL line runs under, as tracebacks show it.
pub const INLINE_NAME: &str = "inline";

/// The chunk a REPL line runs as: the text itself, or, for `=expr`, `print(expr)`.
pub fn inline_source(text: &str) -> ScriptSource {
    let text = text.trim();
    let code = match text.strip_prefix('=') {
        Some(expression) => format!("print({expression})"),
        None => text.to_owned(),
    };
    ScriptSource::new(INLINE_NAME, code)
}

// --- What scripts see ------------------------------------------------------------------

/// The ingest-thread end of a session's script link: rings the [`LineBell`] after each
/// stored chunk, closes it on disconnect, and remembers the transport's description.
pub struct ScriptLink {
    bell: BellSink,
    description: Arc<Mutex<Option<String>>>,
}

impl ChunkSink for ScriptLink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        self.bell.on_chunk(bytes, at);
    }

    fn on_disconnect(&mut self) {
        self.bell.on_disconnect();
    }

    fn on_connect(&mut self, description: &str) {
        *self.description.lock() = Some(description.to_owned());
    }
}

/// The session view's end of the script link: made before the ingest thread starts so
/// its [`ScriptLink`] can be one of the sinks.
#[derive(Clone, Debug, Default)]
pub struct ScriptLinkParts {
    pub bell: LineBell,
    pub description: Arc<Mutex<Option<String>>>,
}

impl ScriptLinkParts {
    pub fn sink(&self) -> ScriptLink {
        ScriptLink {
            bell: self.bell.sink(),
            description: Arc::clone(&self.description),
        }
    }
}

/// The session a GUI script runs on, as `serial.current()`: writes go to the session's
/// writer thread through its [`SessionControl`], reads to its scrollback.
pub struct GuiScriptSession {
    pub port: PortId,
    /// The settings the port was opened with, for when the session has closed.
    pub serial: SerialConfig,
    /// `None` for a session scripts cannot write to; every write then fails as closed.
    pub control: Option<Arc<dyn SessionControl>>,
    pub reader: StoreReader,
    pub link: ScriptLinkParts,
}

impl fmt::Debug for GuiScriptSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GuiScriptSession")
            .field("port", &self.port)
            .field("control", &self.control.is_some())
            .finish_non_exhaustive()
    }
}

impl GuiScriptSession {
    fn control(&self) -> Result<&dyn SessionControl, SessionClosed> {
        self.control.as_deref().ok_or(SessionClosed)
    }
}

impl ScriptSession for GuiScriptSession {
    fn port_id(&self) -> PortId {
        self.port.clone()
    }

    fn description(&self) -> String {
        self.link
            .description
            .lock()
            .clone()
            .unwrap_or_else(|| format!("{} @ {}", self.port, self.serial.summary()))
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        self.control()?.write(bytes)
    }

    fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed> {
        self.control()?.set_control(line, asserted)
    }

    fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed> {
        self.control()?.reconfigure(serial)
    }

    fn serial_config(&self) -> SerialConfig {
        self.control
            .as_ref()
            .and_then(|control| control.serial_config())
            .unwrap_or_else(|| self.serial.clone())
    }

    fn store(&self) -> StoreReader {
        self.reader.clone()
    }

    fn bell(&self) -> LineBell {
        self.link.bell.clone()
    }
}

/// What a script asks of the main thread.
pub enum UiRequest {
    /// `ui.prompt`: ask in a dialog, then send the answer (or drop `answer` for `nil`).
    Prompt {
        label: String,
        default: Option<String>,
        answer: async_channel::Sender<Option<String>>,
    },
    /// `ui.notify`.
    Notify(String),
    /// `log.<level>`; the same line follows as an output event.
    Log(LogLevel, String),
    /// `commands.send`, already resolved and checked.
    SendCommand {
        reference: CommandRef,
        params: ParamValues,
    },
}

impl fmt::Debug for UiRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UiRequest::Prompt { label, default, .. } => f
                .debug_struct("Prompt")
                .field("label", label)
                .field("default", default)
                .finish_non_exhaustive(),
            UiRequest::Notify(text) => f.debug_tuple("Notify").field(text).finish(),
            UiRequest::Log(level, text) => f.debug_tuple("Log").field(level).field(text).finish(),
            UiRequest::SendCommand { reference, params } => f
                .debug_struct("SendCommand")
                .field("reference", reference)
                .field("params", params)
                .finish(),
        }
    }
}

/// `ui.*` and `log.*` for GUI scripts: every call becomes a [`UiRequest`] for the
/// session view.
pub struct GuiScriptUi {
    requests: Sender<UiRequest>,
}

impl ScriptUi for GuiScriptUi {
    fn prompt(&self, label: &str, default: Option<&str>) -> PromptFuture {
        let (answer, answered) = async_channel::bounded(1);
        let request = UiRequest::Prompt {
            label: label.to_owned(),
            default: default.map(str::to_owned),
            answer,
        };
        // Nobody to ask once the view is gone: the dropped sender answers `nil`.
        let _ = self.requests.send(request);
        Box::pin(async move { answered.recv().await.ok().flatten() })
    }

    fn notify(&self, text: &str) {
        let _ = self.requests.send(UiRequest::Notify(text.to_owned()));
    }

    fn log(&self, level: LogLevel, text: &str) {
        let _ = self.requests.send(UiRequest::Log(level, text.to_owned()));
    }
}

/// The saved commands as a script sees them: the configuration's store, swapped in by
/// the workspace on every reload. Cheap to clone; every clone sees the swap.
#[derive(Clone, Default)]
pub struct CommandsSnapshot(Arc<RwLock<Arc<CommandStore>>>);

impl fmt::Debug for CommandsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CommandsSnapshot").finish_non_exhaustive()
    }
}

impl CommandsSnapshot {
    pub fn new(store: Arc<CommandStore>) -> Self {
        Self(Arc::new(RwLock::new(store)))
    }

    pub fn set(&self, store: Arc<CommandStore>) {
        *self.0.write() = store;
    }

    pub fn get(&self) -> Arc<CommandStore> {
        Arc::clone(&self.0.read())
    }
}

/// The saved command `name` names: `collection/group/name`, `collection/name`, or a
/// bare name, which is the first command called that (exactly, else ignoring case) in
/// the order the Commands panel lists them.
pub fn resolve_command(store: &CommandStore, name: &str) -> Result<(CommandRef, Command), String> {
    let parts: Vec<&str> = name.split('/').map(str::trim).collect();
    let found = match parts.as_slice() {
        [collection, group, command] => {
            let reference = CommandRef::new(*collection, *group, *command);
            store
                .get(&reference)
                .cloned()
                .map(|command| (reference, command))
        }
        [collection, command] => store
            .find(collection, command)
            .map(|(reference, command)| (reference, command.clone())),
        _ => {
            let exact = store
                .commands()
                .find(|(_, command)| command.name == name.trim());
            exact
                .or_else(|| {
                    store
                        .commands()
                        .find(|(_, command)| command.name.eq_ignore_ascii_case(name.trim()))
                })
                .map(|(reference, command)| (reference, command.clone()))
        }
    };
    found.ok_or_else(|| format!("no saved command named {name:?}"))
}

/// `commands.send(name, params)` for GUI scripts: resolved and checked on the script
/// thread, so a bad name or value fails the call; sent from the main thread, like a
/// click in the Commands panel (echo, expected reply and all).
pub struct GuiCommands {
    commands: CommandsSnapshot,
    requests: Sender<UiRequest>,
}

impl CommandSender for GuiCommands {
    fn send(&self, name: &str, params: &ParamValues) -> Result<(), CommandError> {
        let store = self.commands.get();
        let (reference, command) = resolve_command(&store, name).map_err(CommandError)?;
        if command.payload.script().is_some() {
            return Err(CommandError(format!(
                "{} runs a script; a script cannot start another",
                command.name
            )));
        }
        command
            .encode(params, LineEnding::None)
            .map_err(|error| CommandError(format!("{}: {error}", command.name)))?;
        self.requests
            .send(UiRequest::SendCommand {
                reference,
                params: params.clone(),
            })
            .map_err(|_| CommandError("the session has closed".to_owned()))
    }
}

/// What the workspace gives each session's scripts besides the session itself.
#[derive(Clone)]
pub struct ScriptEnv {
    /// Backs `serial.ports()`.
    pub ports: Option<Arc<dyn PortSource>>,
    /// Where `require` and `dofile` read from.
    pub scripts_dir: Option<PathBuf>,
    pub commands: CommandsSnapshot,
}

impl fmt::Debug for ScriptEnv {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptEnv")
            .field("ports", &self.ports.is_some())
            .field("scripts_dir", &self.scripts_dir)
            .finish_non_exhaustive()
    }
}

// --- The console's lines ---------------------------------------------------------------

/// What a console line is, which sets its color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleKind {
    /// `print` output.
    Output,
    /// A `log.<level>` line.
    Log(LogLevel),
    /// A `ui.prompt` question.
    Prompt,
    /// The answer to a prompt.
    Answer,
    /// A `ui.notify` notice.
    Notice,
    /// Started, queued, and other remarks of the app's.
    Info,
    /// A run that ended well.
    Finished,
    /// A run that failed, and its error and traceback.
    Error,
    /// A run that was stopped.
    Stopped,
}

/// One line of the Script console.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleLine {
    pub kind: ConsoleKind,
    pub text: String,
}

impl ConsoleLine {
    pub fn new(kind: ConsoleKind, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
        }
    }
}

/// `text` as lines of `kind`, one per line of text (a traceback is several).
fn lines_of(kind: ConsoleKind, text: &str) -> impl Iterator<Item = ConsoleLine> + '_ {
    text.lines().map(move |line| ConsoleLine::new(kind, line))
}

// --- The run queue ---------------------------------------------------------------------

/// A run queued or running on a session.
struct ActiveRun {
    id: RunId,
    run: ScriptRun,
    events: Receiver<ScriptEvent>,
    name: String,
    /// What started it: `console`, `key binding`, `command Version`, `on_connect`.
    origin: String,
    queued_at: Instant,
    started: Option<Instant>,
}

/// What [`SessionScripts::poll`] found for the session view to act on.
#[derive(Debug)]
pub enum ScriptEffect {
    /// A line for the console.
    Line(ConsoleLine),
    /// Open a prompt dialog.
    Prompt {
        label: String,
        default: Option<String>,
        answer: async_channel::Sender<Option<String>>,
    },
    /// `ui.notify`: say it in the status line (the console line comes separately).
    Notify(String),
    /// `commands.send`: send this saved command.
    SendCommand {
        reference: CommandRef,
        params: ParamValues,
    },
    /// A run ended.
    Finished {
        name: String,
        outcome: ScriptOutcome,
    },
}

/// One session's scripts: its [`ScriptHost`] (one thread, one script at a time) and
/// the runs queued on it, oldest first. Owned by the session view, which creates it on
/// connect and detaches it on disconnect, which stops whatever runs.
pub struct SessionScripts {
    /// `None` once detached; runs still here then are finishing.
    host: Option<ScriptHost>,
    requests: Receiver<UiRequest>,
    runs: VecDeque<ActiveRun>,
    /// Log requests whose output line has not been read yet.
    pending_logs: VecDeque<(LogLevel, String)>,
    /// Why the host went away, for the lines of runs it stopped.
    detached: Option<String>,
}

impl fmt::Debug for SessionScripts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionScripts")
            .field("attached", &self.host.is_some())
            .field("runs", &self.runs.len())
            .finish_non_exhaustive()
    }
}

/// Log requests kept waiting for their output line at most; more means lines were lost.
const MAX_PENDING_LOGS: usize = 1024;

impl SessionScripts {
    /// Start a script thread for `session`.
    pub fn new(session: Arc<dyn ScriptSession>, env: ScriptEnv) -> Self {
        let (requests_tx, requests) = unbounded();
        let services = HostServices {
            session: Some(session),
            ui: Arc::new(GuiScriptUi {
                requests: requests_tx.clone(),
            }),
            commands: Some(Arc::new(GuiCommands {
                commands: env.commands,
                requests: requests_tx,
            })),
            // The first UI version opens no ports for scripts: `serial.open` says so.
            opener: None,
            ports: env.ports,
            scripts_dir: env.scripts_dir,
            limits: Limits::default(),
        };
        Self {
            host: Some(ScriptHost::new(services)),
            requests,
            runs: VecDeque::new(),
            pending_logs: VecDeque::new(),
            detached: None,
        }
    }

    /// Whether scripts can still be queued.
    pub fn is_attached(&self) -> bool {
        self.host.is_some()
    }

    /// Whether a run is queued or running, so the view keeps polling.
    pub fn is_active(&self) -> bool {
        !self.runs.is_empty()
    }

    /// Queue `source`, started by `origin`. Returns the console's line about it: queued
    /// behind the running script, if one runs. `None` if detached.
    pub fn run(&mut self, source: ScriptSource, origin: &str) -> Option<Vec<ConsoleLine>> {
        let host = self.host.as_ref()?;
        let name = source.name.clone();
        let run = host.run(source);
        let mut lines = Vec::new();
        if let Some(current) = self.runs.front() {
            let ahead = self.runs.len();
            lines.push(ConsoleLine::new(
                ConsoleKind::Info,
                format!(
                    "Queued {name} ({origin}) behind {}{}",
                    current.name,
                    if ahead > 1 {
                        format!(" and {} more", ahead - 1)
                    } else {
                        String::new()
                    }
                ),
            ));
        }
        self.runs.push_back(ActiveRun {
            id: run.id(),
            events: run.events(),
            run,
            name,
            origin: origin.to_owned(),
            queued_at: Instant::now(),
            started: None,
        });
        Some(lines)
    }

    /// Stop the running script; queued ones still run. Returns its name.
    pub fn stop_current(&mut self) -> Option<String> {
        let current = self.runs.front()?;
        current.run.stop();
        Some(current.name.clone())
    }

    /// Stop everything and let the script thread go, because of `reason`. Returns the
    /// host, for the caller to drop off the main thread (dropping joins the thread), and
    /// the console lines saying what was stopped.
    pub fn detach(&mut self, reason: &str) -> (Option<ScriptHost>, Vec<ConsoleLine>) {
        let host = self.host.take();
        let mut lines = Vec::new();
        if host.is_some() {
            self.detached = Some(reason.to_owned());
            for run in &self.runs {
                run.run.stop();
                lines.push(ConsoleLine::new(
                    ConsoleKind::Stopped,
                    format!("Stopping {}: {reason}", run.name),
                ));
            }
        }
        (host, lines)
    }

    /// The running script, for the status line.
    pub fn status(&self) -> Option<ScriptStatus> {
        let current = self.runs.front()?;
        Some(ScriptStatus {
            name: current.name.clone(),
            running_for: current.started.map(|started| started.elapsed()),
            queued: self.runs.len() - 1,
        })
    }

    /// Read what the script thread sent since the last poll: every run's events in
    /// order, and the requests of the script running.
    pub fn poll(&mut self) -> Vec<ScriptEffect> {
        let mut effects = Vec::new();
        let mut index = 0;
        while index < self.runs.len() {
            let mut finished = None;
            while let Ok(event) = self.runs[index].events.try_recv() {
                self.drain_requests(&mut effects);
                match event {
                    ScriptEvent::Started => {
                        let run = &mut self.runs[index];
                        run.started = Some(Instant::now());
                        effects.push(ScriptEffect::Line(ConsoleLine::new(
                            ConsoleKind::Info,
                            format!("\u{25b6} {} ({})", run.name, run.origin),
                        )));
                    }
                    ScriptEvent::Output(text) => {
                        let kind = self
                            .take_log(&text)
                            .map_or(ConsoleKind::Output, ConsoleKind::Log);
                        effects.extend(lines_of(kind, &text).map(ScriptEffect::Line));
                    }
                    ScriptEvent::Prompt { label, default, .. } => {
                        let text = match default {
                            Some(default) => format!("? {label} [{default}]"),
                            None => format!("? {label}"),
                        };
                        effects.push(ScriptEffect::Line(ConsoleLine::new(
                            ConsoleKind::Prompt,
                            text,
                        )));
                    }
                    ScriptEvent::Finished(outcome) => {
                        finished = Some(outcome);
                        break;
                    }
                }
            }
            match finished {
                Some(outcome) => {
                    let run = self.runs.remove(index).expect("the run is in the queue");
                    effects.extend(self.finish_lines(&run, &outcome).map(ScriptEffect::Line));
                    tracing::debug!(run = %run.id, name = %run.name, ?outcome, "script finished");
                    effects.push(ScriptEffect::Finished {
                        name: run.name,
                        outcome,
                    });
                }
                None => index += 1,
            }
        }
        self.drain_requests(&mut effects);
        effects
    }

    /// Move the requests waiting in the channel into `effects`, keeping log requests
    /// for their output lines.
    fn drain_requests(&mut self, effects: &mut Vec<ScriptEffect>) {
        while let Ok(request) = self.requests.try_recv() {
            match request {
                UiRequest::Log(level, text) => {
                    if self.pending_logs.len() == MAX_PENDING_LOGS {
                        self.pending_logs.pop_front();
                    }
                    self.pending_logs.push_back((level, text));
                }
                UiRequest::Notify(text) => {
                    effects.extend(
                        lines_of(ConsoleKind::Notice, &format!("notice: {text}"))
                            .map(ScriptEffect::Line),
                    );
                    effects.push(ScriptEffect::Notify(text));
                }
                UiRequest::Prompt {
                    label,
                    default,
                    answer,
                } => effects.push(ScriptEffect::Prompt {
                    label,
                    default,
                    answer,
                }),
                UiRequest::SendCommand { reference, params } => {
                    effects.push(ScriptEffect::SendCommand { reference, params });
                }
            }
        }
    }

    /// The level of the log call that produced output `text`, if one did.
    fn take_log(&mut self, text: &str) -> Option<LogLevel> {
        let (level, logged) = self.pending_logs.front()?;
        let matches = text
            .strip_prefix('[')
            .and_then(|rest| rest.strip_prefix(level.as_str()))
            .and_then(|rest| rest.strip_prefix("] "))
            .is_some_and(|rest| rest == logged);
        if !matches {
            return None;
        }
        self.pending_logs.pop_front().map(|(level, _)| level)
    }

    fn finish_lines(
        &self,
        run: &ActiveRun,
        outcome: &ScriptOutcome,
    ) -> impl Iterator<Item = ConsoleLine> + use<> {
        let elapsed = run
            .started
            .map_or_else(|| run.queued_at.elapsed(), |started| started.elapsed());
        let took = format_seconds(elapsed);
        let lines: Vec<ConsoleLine> = match outcome {
            ScriptOutcome::Ok => vec![ConsoleLine::new(
                ConsoleKind::Finished,
                format!("\u{2713} {} finished in {took}", run.name),
            )],
            ScriptOutcome::Error(message) => {
                let mut lines = vec![ConsoleLine::new(
                    ConsoleKind::Error,
                    format!("\u{2717} {} failed after {took}:", run.name),
                )];
                lines.extend(lines_of(ConsoleKind::Error, message));
                lines
            }
            ScriptOutcome::Stopped => {
                let why = match (&self.detached, run.started) {
                    (Some(reason), _) => format!(" ({reason})"),
                    (None, None) => " before it started".to_owned(),
                    (None, Some(_)) => String::new(),
                };
                vec![ConsoleLine::new(
                    ConsoleKind::Stopped,
                    format!("\u{25a0} {} stopped after {took}{why}", run.name),
                )]
            }
        };
        lines.into_iter()
    }
}

/// How long a detached host may take to let go of its thread before dropping it is
/// logged as slow: a script is stopped within one instruction check or at once in a
/// wait, so this is generous.
pub const DETACH_WARN_AFTER: Duration = Duration::from_millis(500);

/// Drop a detached host, joining its thread, and log it if that was slow.
pub fn drop_host(host: ScriptHost) {
    let started = Instant::now();
    drop(host);
    let took = started.elapsed();
    if took > DETACH_WARN_AFTER {
        tracing::warn!(?took, "the script thread was slow to stop");
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::{CollectionSource, CommandCollection, CommandGroup};

    use super::*;

    #[test]
    fn a_repl_line_runs_as_is_and_equals_prints() {
        assert_eq!(inline_source(" print(1) ").code, "print(1)");
        assert_eq!(inline_source("=1 + 1").code, "print(1 + 1)");
        assert_eq!(inline_source("x").name, INLINE_NAME);
    }

    #[test]
    fn command_names_resolve_by_path_or_first_match() {
        let mut store = CommandStore::empty();
        let mut collection = CommandCollection::new(
            "Bench",
            CollectionSource::User(PathBuf::from("/cfg/commands/bench.json")),
        );
        collection.groups.push(CommandGroup {
            name: "Modem".into(),
            commands: vec![
                Command::text("Version", "AT+VER?"),
                Command::text("Reset", "ATZ"),
            ],
        });
        store.set_collection(collection);
        let name = |text: &str| resolve_command(&store, text).map(|(reference, _)| reference);
        let version = CommandRef::new("Bench", "Modem", "Version");
        assert_eq!(name("Version"), Ok(version.clone()));
        assert_eq!(
            name("version"),
            Ok(version.clone()),
            "any case as a fallback"
        );
        assert_eq!(name("Bench/Version"), Ok(version.clone()));
        assert_eq!(name("Bench/Modem/Version"), Ok(version));
        assert_eq!(
            name("Nope"),
            Err("no saved command named \"Nope\"".to_owned())
        );
        assert!(name("Other/Version").is_err());
    }
}
