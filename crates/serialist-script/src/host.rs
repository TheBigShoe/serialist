//! The script host: one dedicated thread, one script at a time, runs queued in order.

use std::any::Any;
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::{Condvar, Mutex};
use serialist_core::PortSource;
use tokio::runtime::Runtime;
use tokio::sync::watch;

use crate::services::{CommandSender, PortOpener, ScriptSession, ScriptUi};

/// Name of the thread every script of a host runs on.
pub const SCRIPT_THREAD_NAME: &str = "serialist-script";
/// Name of the worker threads that do the script thread's blocking work: opening and
/// closing ports, reading `require`d files.
pub const SCRIPT_WORKER_THREAD_NAME: &str = "serialist-script-io";
/// Most worker threads alive at once.
const MAX_WORKERS: usize = 8;
/// How long dropping a host waits for workers still busy (a port being opened).
const WORKER_SHUTDOWN: Duration = Duration::from_secs(1);

/// Resource limits for one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The Lua VM's memory cap in bytes; an allocation past it raises a Lua memory
    /// error, which ends the script unless it is caught. 0 means no cap.
    pub memory_bytes: usize,
    /// How many VM instructions run between checks of the stop flag. Smaller stops a
    /// busy loop sooner and costs more; 10 000 is well under a millisecond.
    pub instruction_check_every: u32,
}

impl Limits {
    pub const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_INSTRUCTION_CHECK_EVERY: u32 = 10_000;
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            memory_bytes: Self::DEFAULT_MEMORY_BYTES,
            instruction_check_every: Self::DEFAULT_INSTRUCTION_CHECK_EVERY,
        }
    }
}

/// Everything a host offers its scripts. Only `ui` is required; each missing service
/// makes its part of the Lua API fail with a message saying so.
#[derive(Clone)]
pub struct HostServices {
    /// What `serial.current()` returns.
    pub session: Option<Arc<dyn ScriptSession>>,
    pub ui: Arc<dyn ScriptUi>,
    /// Backs `commands.send`.
    pub commands: Option<Arc<dyn CommandSender>>,
    /// Backs `serial.open`.
    pub opener: Option<Arc<dyn PortOpener>>,
    /// Backs `serial.ports()` and `serial.open{ match = ... }`.
    pub ports: Option<Arc<dyn PortSource>>,
    /// The only directory `require` and `dofile` read from. `None` disables both.
    pub scripts_dir: Option<PathBuf>,
    pub limits: Limits,
}

impl HostServices {
    /// Only a UI: no session, commands, opener, port list or scripts directory.
    pub fn new(ui: Arc<dyn ScriptUi>) -> Self {
        Self {
            session: None,
            ui,
            commands: None,
            opener: None,
            ports: None,
            scripts_dir: None,
            limits: Limits::default(),
        }
    }
}

impl fmt::Debug for HostServices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostServices")
            .field("session", &self.session.as_ref().map(|s| s.port_id()))
            .field("commands", &self.commands.is_some())
            .field("opener", &self.opener.is_some())
            .field("ports", &self.ports.is_some())
            .field("scripts_dir", &self.scripts_dir)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// A script to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptSource {
    /// Shown in error messages and tracebacks (`probe.lua:3: ...`).
    pub name: String,
    pub code: String,
    /// Where the code came from, if a file.
    pub path: Option<PathBuf>,
}

impl ScriptSource {
    pub fn new(name: impl Into<String>, code: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            code: code.into(),
            path: None,
        }
    }

    /// Reads `path`; the name is its file name.
    pub fn from_file(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let code = std::fs::read_to_string(path)?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Ok(Self {
            name,
            code,
            path: Some(path.to_path_buf()),
        })
    }
}

/// Identifies one run within its host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub u64);

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "run {}", self.0)
    }
}

/// What a run reports, in order: `Started`, any number of `Output` and `Prompt`, then
/// exactly one `Finished`, which is always last. A run stopped while still queued
/// reports only `Finished(Stopped)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptEvent {
    Started,
    /// One line from `print` (arguments joined by tabs) or `log.<level>` (joined by
    /// spaces, prefixed `[level] `).
    Output(String),
    /// The script called `ui.prompt`; the answer comes back through
    /// [`ScriptUi::prompt`]. `id` counts prompts within the run from 1.
    Prompt {
        id: u64,
        label: String,
        default: Option<String>,
    },
    Finished(ScriptOutcome),
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptOutcome {
    /// The main chunk returned.
    Ok,
    /// A syntax error, an uncaught runtime error (message and Lua traceback), a memory
    /// error, or an error in an `on_line` callback.
    Error(String),
    /// [`ScriptRun::stop`] (or dropping the host) ended it.
    Stopped,
}

/// Stop signal shared by a run's handle and the script thread.
pub(crate) struct RunControl {
    stopped: AtomicBool,
    signal: watch::Sender<bool>,
}

impl RunControl {
    fn new() -> Self {
        let (signal, _) = watch::channel(false);
        Self {
            stopped: AtomicBool::new(false),
            signal,
        }
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.signal.send_replace(true);
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Resolves once `stop` has been called.
    pub(crate) async fn stopped(&self) {
        let mut rx = self.signal.subscribe();
        // The sender lives as long as `self`, so this cannot fail.
        let _ = rx.wait_for(|stopped| *stopped).await;
    }
}

#[derive(Default)]
struct Done {
    outcome: Mutex<Option<ScriptOutcome>>,
    finished: Condvar,
}

/// A handle to one queued or running script. Dropping it does not stop the script.
pub struct ScriptRun {
    id: RunId,
    control: Arc<RunControl>,
    events: Receiver<ScriptEvent>,
    done: Arc<Done>,
}

impl fmt::Debug for ScriptRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptRun")
            .field("id", &self.id)
            .field("outcome", &self.outcome())
            .finish()
    }
}

impl ScriptRun {
    pub fn id(&self) -> RunId {
        self.id
    }

    /// Ask the script to stop. A script waiting (`sleep`, `expect`, `ui.prompt` ...)
    /// stops at once; one computing stops within one instruction check (about 10 000
    /// VM instructions); one still queued never starts. Its outcome is `Stopped`.
    pub fn stop(&self) {
        self.control.stop();
    }

    /// The run's events. Every call returns the same channel, so give it to one
    /// consumer. Unbounded: a consumer that stops reading lets output pile up.
    pub fn events(&self) -> Receiver<ScriptEvent> {
        self.events.clone()
    }

    /// Blocks until the run ends. Not for a UI thread.
    pub fn wait(&self) -> ScriptOutcome {
        let mut outcome = self.done.outcome.lock();
        loop {
            if let Some(outcome) = &*outcome {
                return outcome.clone();
            }
            self.done.finished.wait(&mut outcome);
        }
    }

    /// [`wait`](Self::wait) for at most `limit`.
    pub fn wait_timeout(&self, limit: Duration) -> Option<ScriptOutcome> {
        let deadline = Instant::now().checked_add(limit);
        let mut outcome = self.done.outcome.lock();
        loop {
            if let Some(outcome) = &*outcome {
                return Some(outcome.clone());
            }
            match deadline {
                Some(deadline) => {
                    if self
                        .done
                        .finished
                        .wait_until(&mut outcome, deadline)
                        .timed_out()
                    {
                        return outcome.clone();
                    }
                }
                None => self.done.finished.wait(&mut outcome),
            }
        }
    }

    /// The outcome, if the run has ended. Never blocks.
    pub fn outcome(&self) -> Option<ScriptOutcome> {
        self.done.outcome.lock().clone()
    }
}

/// One queued run, as the script thread sees it.
pub(crate) struct Job {
    pub(crate) id: RunId,
    pub(crate) source: ScriptSource,
    pub(crate) services: HostServices,
    pub(crate) control: Arc<RunControl>,
    pub(crate) events: Sender<ScriptEvent>,
    done: Arc<Done>,
    finished: bool,
}

impl Job {
    /// Report the outcome: `Finished` goes out before [`ScriptRun::wait`] returns, so a
    /// waiter that then drains the events sees it.
    fn finish(&mut self, outcome: ScriptOutcome) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        let _ = self.events.send(ScriptEvent::Finished(outcome.clone()));
        *self.done.outcome.lock() = Some(outcome);
        self.done.finished.notify_all();
    }
}

impl Drop for Job {
    /// A job dropped unfinished never ran: its host is gone.
    fn drop(&mut self) {
        self.finish(ScriptOutcome::Stopped);
    }
}

struct HostShared {
    closing: AtomicBool,
    /// The running job's stop signal.
    current: Mutex<Option<Arc<RunControl>>>,
}

/// Runs Lua scripts on a dedicated thread, one at a time, never blocking the caller.
///
/// `run` queues a script and returns a [`ScriptRun`] at once. The thread takes runs in
/// order; each gets a fresh Lua VM with the sandbox and API described in the
/// [crate docs](crate), driven by a current-thread tokio executor, so a script waiting
/// on the device suspends a coroutine rather than the thread.
///
/// `ScriptHost` is `Send + Sync`. Dropping it stops the running script, finishes the
/// queued ones as `Stopped`, and joins the thread.
pub struct ScriptHost {
    jobs: Option<Sender<Job>>,
    shared: Arc<HostShared>,
    services: Mutex<HostServices>,
    next_id: AtomicU64,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for ScriptHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptHost")
            .field("services", &*self.services.lock())
            .field("busy", &self.shared.current.lock().is_some())
            .finish()
    }
}

impl ScriptHost {
    /// Start the script thread.
    ///
    /// # Panics
    ///
    /// If the OS will not start a thread, as [`Ingest::spawn`](serialist_core::Ingest::spawn) does.
    pub fn new(services: HostServices) -> Self {
        let (jobs, job_rx) = unbounded::<Job>();
        let shared = Arc::new(HostShared {
            closing: AtomicBool::new(false),
            current: Mutex::new(None),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name(SCRIPT_THREAD_NAME.into())
            .spawn(move || script_thread(&job_rx, &thread_shared))
            .expect("spawn the script thread");
        Self {
            jobs: Some(jobs),
            shared,
            services: Mutex::new(services),
            next_id: AtomicU64::new(1),
            thread: Some(thread),
        }
    }

    /// Queue `source` to run after any scripts already queued, with the services as they
    /// are now. Returns at once.
    pub fn run(&self, source: ScriptSource) -> ScriptRun {
        let id = RunId(self.next_id.fetch_add(1, Ordering::Relaxed));
        let control = Arc::new(RunControl::new());
        let done = Arc::new(Done::default());
        let (events, event_rx) = unbounded();
        let job = Job {
            id,
            source,
            services: self.services.lock().clone(),
            control: Arc::clone(&control),
            events,
            done: Arc::clone(&done),
            finished: false,
        };
        if let Some(jobs) = &self.jobs {
            // A dead thread drops the job, which finishes it.
            let _ = jobs.send(job);
        }
        ScriptRun {
            id,
            control,
            events: event_rx,
            done,
        }
    }

    /// The session `serial.current()` returns in runs queued from now on, for when the
    /// user connects, reconnects or switches tabs.
    pub fn set_session(&self, session: Option<Arc<dyn ScriptSession>>) {
        self.services.lock().session = session;
    }

    /// Stop the running script, if any. Queued ones still run.
    pub fn stop_current(&self) {
        if let Some(control) = &*self.shared.current.lock() {
            control.stop();
        }
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        {
            // Under the lock the thread takes to start a job, so a job starting now
            // either sees `closing` or is here to be stopped.
            let current = self.shared.current.lock();
            self.shared.closing.store(true, Ordering::SeqCst);
            if let Some(control) = &*current {
                control.stop();
            }
        }
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            if thread.thread().id() == thread::current().id() {
                // Dropped from a callback on its own thread: it cannot join itself. It
                // ends once the job in hand finishes.
                return;
            }
            if thread.join().is_err() {
                tracing::error!("the script thread panicked");
            }
        }
    }
}

fn script_thread(jobs: &Receiver<Job>, shared: &HostShared) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .max_blocking_threads(MAX_WORKERS)
        .thread_name(SCRIPT_WORKER_THREAD_NAME)
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(%err, "could not start the script executor");
            for mut job in jobs.iter() {
                job.finish(ScriptOutcome::Error(format!(
                    "could not start the script executor: {err}"
                )));
            }
            return;
        }
    };
    for mut job in jobs.iter() {
        {
            let mut current = shared.current.lock();
            if shared.closing.load(Ordering::SeqCst) {
                job.control.stop();
            }
            *current = Some(Arc::clone(&job.control));
        }
        let outcome = if job.control.is_stopped() {
            ScriptOutcome::Stopped
        } else {
            run_guarded(&runtime, &job)
        };
        *shared.current.lock() = None;
        tracing::debug!(run = %job.id, name = %job.source.name, ?outcome, "script finished");
        job.finish(outcome);
    }
    runtime.shutdown_timeout(WORKER_SHUTDOWN);
}

/// Runs one job; a panic in the host's own code becomes an error outcome instead of
/// taking the thread down.
fn run_guarded(runtime: &Runtime, job: &Job) -> ScriptOutcome {
    match catch_unwind(AssertUnwindSafe(|| crate::vm::run(runtime, job))) {
        Ok(outcome) => outcome,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(%message, "a script run panicked");
            ScriptOutcome::Error(format!("the script host failed: {message}"))
        }
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic without a message".to_owned()
    }
}
