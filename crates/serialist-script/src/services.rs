//! What a script host needs from the app: the traits the UI and the headless runner
//! implement. Everything here is `Send + Sync` and may be called from the script thread.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serialist_core::{
    ControlLine, DeviceMatch, ParamValues, PortId, SerialConfig, SessionClosed, StoreReader,
};

use crate::bell::LineBell;

/// One open session as a script sees it: the port that `serial.current()` (or
/// `serial.open`) returns.
///
/// Reads never go to the transport. They go to the session's scrollback through
/// [`store`](Self::store), woken by [`bell`](Self::bell), so a script sees exactly the
/// lines the terminal shows and never takes bytes away from it.
///
/// # Contract for implementors
///
/// - Pass [`LineBell::sink`] among the sinks when spawning the session's
///   [`Ingest`](serialist_core::Ingest), so the bell rings after every received chunk is
///   stored and closes when the session disconnects. A bell that never rings makes every
///   wait end at its timeout; nothing hangs.
/// - `write`, `set_control` and `reconfigure` queue work and return at once (as
///   [`Session`](serialist_core::Session) does); they are called on the script thread.
/// - A session that follows reconnects may swap the store and bell underneath: scripts
///   call `store()` and `bell()` for every wait and never cache them. Read positions are
///   stream offsets, so the new store should continue the old one (the ingest thread
///   hands its store back for exactly that).
pub trait ScriptSession: Send + Sync {
    /// The port this session is open on.
    fn port_id(&self) -> PortId;

    /// A one-line description for `port:description()`, such as the transport's
    /// `/dev/cu.usbserial-1420 @ 921600 8N1`.
    fn description(&self) -> String;

    /// Queue bytes for the device.
    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed>;

    /// Assert or release DTR or RTS.
    fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed>;

    /// Apply new line settings without reopening the port.
    fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed>;

    /// The line settings in effect.
    fn serial_config(&self) -> SerialConfig;

    /// The session's scrollback, for `read`, `read_line`, `expect` and `on_line`.
    fn store(&self) -> StoreReader;

    /// Rung after each received chunk is stored; closed when no more can arrive.
    fn bell(&self) -> LineBell;

    /// Close the session. Called only for ports a script opened with `serial.open`,
    /// when the script calls `port:close()` or ends, on a thread that may block (a
    /// worker, or the script thread between runs). The default does nothing.
    fn close(&self) {}
}

/// Severity of a `log.*` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// `debug`, `info`, `warn` or `error`: the Lua function's name.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The answer to a prompt: `Some(text)`, or `None` if the user dismissed it.
pub type PromptFuture = Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>;

/// The app's side of `ui.*`, `log.*` and the headless runner's output.
///
/// Every method is called on the script thread and must return promptly: `notify`,
/// `log` and `output` must not block, and `prompt` returns a future the script thread
/// awaits (typically the receiving end of a one-shot channel the UI answers), so a
/// script waiting for an answer suspends instead of blocking the thread. Stopping the
/// script drops the future.
pub trait ScriptUi: Send + Sync {
    /// Ask the user for a line of text, pre-filled with `default`. Before calling it the
    /// host sends [`ScriptEvent::Prompt`](crate::ScriptEvent::Prompt) with the same
    /// label, so a console can show the question in line with the script's output.
    fn prompt(&self, label: &str, default: Option<&str>) -> PromptFuture;

    /// Show a transient notification.
    fn notify(&self, text: &str);

    /// A `log.<level>(...)` call, already joined into one line. The same line also
    /// arrives as [`ScriptEvent::Output`](crate::ScriptEvent::Output).
    fn log(&self, level: LogLevel, text: &str);

    /// A line of script output, from `print` or `log`. Only
    /// [`run_headless`](crate::run_headless) calls this, for each
    /// [`ScriptEvent::Output`](crate::ScriptEvent::Output); an app that reads
    /// [`ScriptRun::events`](crate::ScriptRun::events) itself never sees it.
    fn output(&self, text: &str) {
        let _ = text;
    }
}

/// Why `commands.send` failed, in words for the script.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CommandError(pub String);

/// Sends saved commands by name, as the Commands panel does.
pub trait CommandSender: Send + Sync {
    /// Queue the command `name` (resolved the way the app resolves names) with `params`.
    /// Must not block: it is called on the script thread.
    fn send(&self, name: &str, params: &ParamValues) -> Result<(), CommandError>;
}

/// What `serial.open{...}` asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRequest {
    /// `port = "..."`: open this id.
    pub port: Option<PortId>,
    /// `match = {...}`: open the first listed port that matches.
    pub matching: Option<DeviceMatch>,
    /// Default line settings with `baud` overridden if the script gave one.
    pub serial: SerialConfig,
}

/// Why `serial.open` failed.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// This host cannot open ports for scripts.
    #[error("serial.open is not supported here")]
    Unsupported,
    /// No listed port matched the request.
    #[error("no port matches {0}")]
    NoMatch(String),
    /// The port was found but did not open, or the request was incomplete.
    #[error("{0}")]
    Failed(String),
}

/// Opens ports for `serial.open`. Called on a worker thread that may block, for as long
/// as opening a real port takes.
///
/// A host that should not let scripts open ports passes no opener, or one that returns
/// [`OpenError::Unsupported`]; the first UI version does that. The headless runner uses
/// [`HeadlessOpener`](crate::HeadlessOpener).
pub trait PortOpener: Send + Sync {
    fn open(&self, request: &OpenRequest) -> Result<Arc<dyn ScriptSession>, OpenError>;
}
