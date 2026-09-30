//! Running scripts without a UI: a session of its own, stdio for output and prompts.

use std::fmt;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use parking_lot::Mutex;
use serialist_core::LinkState;
use serialist_core::{
    ControlLine, Ingest, IngestHandle, PortId, PortSource, SerialConfig, Session, SessionClosed,
    SessionConfig, Store, StoreReader, TransportError, TransportFactory,
};
use tokio::sync::oneshot;

use crate::bell::LineBell;
use crate::host::{HostServices, ScriptEvent, ScriptHost, ScriptOutcome, ScriptSource};
use crate::services::{
    LogLevel, OpenError, OpenRequest, PortOpener, PromptFuture, ScriptSession, ScriptUi,
};

/// A [`Session`] with its own ingest thread and store, for scripts that run without the
/// app: [`run_headless`] and `serial.open` there.
pub struct HeadlessSession {
    port: PortId,
    session: Mutex<Option<Session>>,
    ingest: Mutex<Option<IngestHandle>>,
    reader: StoreReader,
    bell: LineBell,
}

impl fmt::Debug for HeadlessSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeadlessSession")
            .field("port", &self.port)
            .field("open", &self.session.lock().is_some())
            .finish_non_exhaustive()
    }
}

impl HeadlessSession {
    /// Open `port` through `factory` and start its ingest thread, with the bell's sink.
    pub fn open(
        factory: &dyn TransportFactory,
        port: PortId,
        serial: SerialConfig,
    ) -> Result<Self, TransportError> {
        let session = Session::open(factory, SessionConfig::new(port.clone(), serial))?;
        let bell = LineBell::new();
        let ingest = Ingest::spawn(
            session.events(),
            Store::default(),
            vec![Box::new(bell.sink())],
            Box::new(|| {}),
        );
        // The script may ask for the description at once; give the ingest thread a
        // moment to record the Connected event (a slow CI runner has taken longer than
        // the script's first call).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while matches!(ingest.connection().state, LinkState::Connecting)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Ok(Self {
            port,
            reader: ingest.reader(),
            session: Mutex::new(Some(session)),
            ingest: Mutex::new(Some(ingest)),
            bell,
        })
    }

    /// Close the port (queued writes go out first, for at most
    /// [`CLOSE_DRAIN_TIMEOUT`](serialist_core::session::CLOSE_DRAIN_TIMEOUT)) and wait
    /// for the ingest thread to store the rest. Idempotent. Blocks for up to about half
    /// a second.
    pub fn close_now(&self) {
        if let Some(session) = self.session.lock().take() {
            session.close();
        }
        if let Some(ingest) = self.ingest.lock().take() {
            // The session is gone, so its channel closes and the thread ends by itself.
            let _ = ingest.join();
        }
        self.bell.close();
    }

    fn with_session<T>(
        &self,
        f: impl FnOnce(&Session) -> Result<T, SessionClosed>,
    ) -> Result<T, SessionClosed> {
        match &*self.session.lock() {
            Some(session) => f(session),
            None => Err(SessionClosed),
        }
    }
}

impl Drop for HeadlessSession {
    fn drop(&mut self) {
        self.close_now();
    }
}

impl ScriptSession for HeadlessSession {
    fn port_id(&self) -> PortId {
        self.port.clone()
    }

    fn description(&self) -> String {
        self.ingest
            .lock()
            .as_ref()
            .and_then(|ingest| ingest.connection().description)
            .unwrap_or_else(|| self.port.to_string())
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        self.with_session(|session| session.write(bytes))
    }

    fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed> {
        self.with_session(|session| session.set_control(line, asserted))
    }

    fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed> {
        self.with_session(|session| session.reconfigure(serial))
    }

    fn serial_config(&self) -> SerialConfig {
        self.with_session(|session| Ok(session.serial_config()))
            .unwrap_or_default()
    }

    fn store(&self) -> StoreReader {
        self.reader.clone()
    }

    fn bell(&self) -> LineBell {
        self.bell.clone()
    }

    fn close(&self) {
        self.close_now();
    }
}

/// Opens [`HeadlessSession`]s for `serial.open`: by `port`, or by `match` against the
/// port source's list when there is one.
pub struct HeadlessOpener {
    factory: Arc<dyn TransportFactory>,
    ports: Option<Arc<dyn PortSource>>,
}

impl fmt::Debug for HeadlessOpener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HeadlessOpener")
            .field("ports", &self.ports.is_some())
            .finish_non_exhaustive()
    }
}

impl HeadlessOpener {
    pub fn new(factory: Arc<dyn TransportFactory>) -> Self {
        Self {
            factory,
            ports: None,
        }
    }

    /// Resolve `match = {...}` requests against `ports`.
    pub fn with_ports(mut self, ports: Arc<dyn PortSource>) -> Self {
        self.ports = Some(ports);
        self
    }
}

impl PortOpener for HeadlessOpener {
    fn open(&self, request: &OpenRequest) -> Result<Arc<dyn ScriptSession>, OpenError> {
        let port = match (&request.port, &request.matching) {
            (Some(port), _) => port.clone(),
            (None, Some(matching)) => {
                let Some(ports) = &self.ports else {
                    return Err(OpenError::Failed(
                        "no port list here to match against; give port = \"<id>\"".into(),
                    ));
                };
                ports
                    .snapshot()
                    .into_iter()
                    .find(|info| matching.matches(info))
                    .map(|info| info.id)
                    .ok_or_else(|| OpenError::NoMatch(format!("{matching:?}")))?
            }
            (None, None) => {
                return Err(OpenError::Failed(
                    "give port = \"<id>\" or match = {...}".into(),
                ));
            }
        };
        HeadlessSession::open(self.factory.as_ref(), port.clone(), request.serial.clone())
            .map(|session| Arc::new(session) as Arc<dyn ScriptSession>)
            .map_err(|err| OpenError::Failed(format!("could not open {port}: {err}")))
    }
}

type Input = Box<dyn BufRead + Send>;
type Output = Box<dyn Write + Send>;

/// A [`ScriptUi`] on standard streams, for `--script`: output lines to stdout,
/// notifications and prompt labels to stderr, prompt answers from stdin, and `log`
/// lines to `tracing` as well (they reach stdout as output already).
///
/// A prompt reads one line on a helper thread; an empty line takes the default and end
/// of input answers `None`. A script stopped while waiting leaves that thread blocked
/// on stdin until a line arrives.
pub struct StdioUi {
    input: Arc<Mutex<Input>>,
    out: Mutex<Output>,
    err: Mutex<Output>,
}

impl fmt::Debug for StdioUi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StdioUi").finish_non_exhaustive()
    }
}

impl Default for StdioUi {
    fn default() -> Self {
        Self::new()
    }
}

impl StdioUi {
    /// On the process's stdin, stdout and stderr.
    pub fn new() -> Self {
        Self::with_io(
            Box::new(io::BufReader::new(io::stdin())),
            Box::new(io::stdout()),
            Box::new(io::stderr()),
        )
    }

    /// On other streams, for tests.
    pub fn with_io(input: Input, out: Output, err: Output) -> Self {
        Self {
            input: Arc::new(Mutex::new(input)),
            out: Mutex::new(out),
            err: Mutex::new(err),
        }
    }

    fn line(stream: &Mutex<Output>, text: &str) {
        let mut stream = stream.lock();
        // Nowhere to report a broken pipe to; the script carries on.
        let _ = writeln!(stream, "{text}");
        let _ = stream.flush();
    }
}

impl ScriptUi for StdioUi {
    fn prompt(&self, label: &str, default: Option<&str>) -> PromptFuture {
        {
            let mut err = self.err.lock();
            let _ = match default {
                Some(default) => write!(err, "{label} [{default}]: "),
                None => write!(err, "{label}: "),
            };
            let _ = err.flush();
        }
        let (answer_tx, answer_rx) = oneshot::channel();
        let input = Arc::clone(&self.input);
        let default = default.map(str::to_owned);
        let spawned = thread::Builder::new()
            .name("serialist-script-prompt".into())
            .spawn(move || {
                let mut line = String::new();
                let answer = match input.lock().read_line(&mut line) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => {
                        let text = line.trim_end_matches(['\r', '\n']);
                        if text.is_empty() {
                            default
                        } else {
                            Some(text.to_owned())
                        }
                    }
                };
                let _ = answer_tx.send(answer);
            });
        if let Err(err) = spawned {
            tracing::warn!(%err, "could not start the prompt reader");
        }
        Box::pin(async move { answer_rx.await.ok().flatten() })
    }

    fn notify(&self, text: &str) {
        Self::line(&self.err, &format!("notice: {text}"));
    }

    fn log(&self, level: LogLevel, text: &str) {
        match level {
            LogLevel::Debug => tracing::debug!(target: "serialist_script::lua", "{text}"),
            LogLevel::Info => tracing::info!(target: "serialist_script::lua", "{text}"),
            LogLevel::Warn => tracing::warn!(target: "serialist_script::lua", "{text}"),
            LogLevel::Error => tracing::error!(target: "serialist_script::lua", "{text}"),
        }
    }

    fn output(&self, text: &str) {
        Self::line(&self.out, text);
    }
}

/// Run one script against `port` with no app around it, for `--script`: open a
/// [`HeadlessSession`], run the script on a [`ScriptHost`] with that session as
/// `serial.current()`, hand every output line to [`ScriptUi::output`], then close the
/// port (letting queued writes out) and return the outcome.
///
/// `serial.open` works through `factory`, by `port` only. `require` and `dofile` read
/// from the script file's directory, if the script came from a file.
pub fn run_headless(
    port: PortId,
    config: SerialConfig,
    factory: Arc<dyn TransportFactory>,
    script: ScriptSource,
    ui: Arc<dyn ScriptUi>,
) -> ScriptOutcome {
    let session = match HeadlessSession::open(factory.as_ref(), port.clone(), config) {
        Ok(session) => Arc::new(session),
        Err(err) => return ScriptOutcome::Error(format!("could not open {port}: {err}")),
    };
    let mut services = HostServices::new(Arc::clone(&ui));
    services.session = Some(Arc::clone(&session) as Arc<dyn ScriptSession>);
    services.opener = Some(Arc::new(HeadlessOpener::new(factory)));
    services.scripts_dir = script.path.as_deref().map(script_dir);
    let host = ScriptHost::new(services);
    let run = host.run(script);
    let events = run.events();
    let outcome = loop {
        match events.recv() {
            Ok(ScriptEvent::Output(text)) => ui.output(&text),
            Ok(ScriptEvent::Finished(outcome)) => break outcome,
            Ok(ScriptEvent::Started | ScriptEvent::Prompt { .. }) => {}
            Err(_) => break run.wait(),
        }
    };
    drop(host);
    session.close_now();
    outcome
}

fn script_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}
