//! Test doubles and helpers shared by the script host's integration tests.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::future;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::{ParamValues, PortId, SerialConfig};
use serialist_script::{
    CommandError, CommandSender, HeadlessSession, HostServices, LogLevel, PromptFuture,
    ScriptEvent, ScriptHost, ScriptOutcome, ScriptRun, ScriptSource, ScriptUi,
};
use serialist_sim::{EchoDevice, LinkConfig, SimWorld};

/// An echo device on an unpaced, zero-latency link: round trips cost only the host.
pub const FAST_ECHO: &str = "virtual:echo-fast";

/// The built-in devices plus [`FAST_ECHO`].
pub fn world() -> SimWorld {
    let world = SimWorld::new();
    world.add_virtual("echo-fast", "Fast echo", LinkConfig::unpaced(), || {
        Box::new(EchoDevice::new())
    });
    world
}

pub fn open(world: &SimWorld, id: &str) -> Arc<HeadlessSession> {
    Arc::new(
        HeadlessSession::open(world.factory(), PortId::new(id), SerialConfig::default())
            .expect("the simulated port opens"),
    )
}

/// How the test UI answers the next prompt.
pub enum Answer {
    Now(Option<String>),
    Never,
}

/// Records everything a script sends to the UI and answers prompts from a queue.
#[derive(Default)]
pub struct TestUi {
    pub answers: Mutex<VecDeque<Answer>>,
    pub prompts: Mutex<Vec<(String, Option<String>)>>,
    pub notes: Mutex<Vec<String>>,
    pub logs: Mutex<Vec<(LogLevel, String)>>,
}

impl TestUi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn answering(answers: impl IntoIterator<Item = Answer>) -> Arc<Self> {
        let ui = Self::default();
        ui.answers.lock().extend(answers);
        Arc::new(ui)
    }
}

impl ScriptUi for TestUi {
    fn prompt(&self, label: &str, default: Option<&str>) -> PromptFuture {
        self.prompts
            .lock()
            .push((label.to_owned(), default.map(str::to_owned)));
        match self.answers.lock().pop_front() {
            Some(Answer::Now(answer)) => Box::pin(future::ready(answer)),
            Some(Answer::Never) => Box::pin(future::pending()),
            None => Box::pin(future::ready(None)),
        }
    }

    fn notify(&self, text: &str) {
        self.notes.lock().push(text.to_owned());
    }

    fn log(&self, level: LogLevel, text: &str) {
        self.logs.lock().push((level, text.to_owned()));
    }
}

/// Records sent commands; a command named `missing` fails.
#[derive(Default)]
pub struct TestCommands {
    pub sent: Mutex<Vec<(String, ParamValues)>>,
}

impl CommandSender for TestCommands {
    fn send(&self, name: &str, params: &ParamValues) -> Result<(), CommandError> {
        if name == "missing" {
            return Err(CommandError(format!("no command named {name}")));
        }
        self.sent.lock().push((name.to_owned(), params.clone()));
        Ok(())
    }
}

pub fn services(session: Option<Arc<HeadlessSession>>, ui: Arc<TestUi>) -> HostServices {
    let mut services = HostServices::new(ui);
    services.session = session.map(|s| s as Arc<dyn serialist_script::ScriptSession>);
    services
}

/// Everything a run reported.
#[derive(Debug)]
pub struct Finished {
    pub outcome: ScriptOutcome,
    pub output: Vec<String>,
    pub events: Vec<ScriptEvent>,
}

impl Finished {
    pub fn assert_ok(&self) -> &Self {
        assert_eq!(
            self.outcome,
            ScriptOutcome::Ok,
            "output: {:#?}",
            self.output
        );
        self
    }

    pub fn error(&self) -> &str {
        match &self.outcome {
            ScriptOutcome::Error(message) => message,
            other => panic!(
                "expected an error, got {other:?}; output: {:#?}",
                self.output
            ),
        }
    }
}

/// Drain a run's events until `Finished`, failing the test after `limit`.
pub fn collect(run: &ScriptRun, limit: Duration) -> Finished {
    let events = run.events();
    let deadline = Instant::now() + limit;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(event) => {
                let done = matches!(event, ScriptEvent::Finished(_));
                seen.push(event);
                if done {
                    break;
                }
            }
            Err(_) => panic!("the script did not finish within {limit:?}: {seen:#?}"),
        }
    }
    let outcome = match seen.last() {
        Some(ScriptEvent::Finished(outcome)) => outcome.clone(),
        _ => unreachable!(),
    };
    let output = seen
        .iter()
        .filter_map(|event| match event {
            ScriptEvent::Output(line) => Some(line.clone()),
            _ => None,
        })
        .collect();
    Finished {
        outcome,
        output,
        events: seen,
    }
}

pub fn run(host: &ScriptHost, name: &str, code: &str) -> Finished {
    let run = host.run(ScriptSource::new(name, code));
    collect(&run, Duration::from_secs(30))
}

/// A host with `session` as `serial.current()` and a fresh test UI.
pub fn host_on(session: &Arc<HeadlessSession>) -> ScriptHost {
    ScriptHost::new(services(Some(Arc::clone(session)), TestUi::new()))
}

/// Wait until the run reports `Started`, returning the events seen so far.
pub fn wait_started(run: &ScriptRun) {
    let events = run.events();
    match events.recv_timeout(Duration::from_secs(10)) {
        Ok(ScriptEvent::Started) => {}
        other => panic!("expected Started, got {other:?}"),
    }
}

/// A fresh empty directory under the system temp dir, removed on drop.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "serialist-script-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create a temp dir");
        Self(path)
    }

    pub fn write(&self, relative: &str, contents: &str) {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create a temp subdir");
        }
        std::fs::write(path, contents).expect("write a temp file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `Write` whose bytes a test can read back.
#[derive(Clone, Default)]
pub struct SharedBuf(pub Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
