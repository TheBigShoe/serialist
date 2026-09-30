//! The ingest thread: the one place received bytes turn into scrollback.
//!
//! [`Ingest::spawn`] starts a thread that owns the [`Store`]. It drains a session's
//! events, appends each `Data` chunk to the store (one parse per chunk), hands the same
//! chunk unchanged to every [`ChunkSink`] (recording is one), and turns connect,
//! disconnect and write failures into `Notice` lines. The UI never parses bytes: it gets
//! a [`StoreReader`], a waker and the link state.
//!
//! # Waking the UI: doorbell, acknowledge, snapshot
//!
//! After new lines are published the thread sets a dirty flag and calls the waker only
//! if the flag was clear, so the UI gets at most one wake per acknowledge however many
//! chunks arrive. The waker runs on the ingest thread and must not block. The UI's
//! session view makes it ring a doorbell of capacity one (`async_channel::bounded(1)`
//! and `try_send(())`: a full doorbell means a wake is already pending, and that one
//! will see this publication too). A foreground task waits on the doorbell, and for each
//! ring it:
//!
//! 1. calls [`IngestHandle::acknowledge`], which clears the dirty flag, so anything
//!    published from now on rings again;
//! 2. takes [`IngestHandle::snapshot`] (and, if it shows the link, reads
//!    [`IngestHandle::connection`]), which is therefore never older than the ring;
//! 3. hands the snapshot to the terminal and repaints;
//! 4. waits a frame before answering the next ring.
//!
//! Acknowledge first, then snapshot, is what makes the protocol lossless. A publication
//! that lands before the acknowledge is in the snapshot; one that lands after it finds
//! the flag clear and rings again. The other order could lose a wake: a publication
//! between the snapshot and the acknowledge would be in neither. The price of the right
//! order is one spurious ring when a publication lands between the two calls, which
//! then finds nothing new. However fast the port, that is at most one snapshot and one
//! repaint per frame, and none while idle. A UI that stops acknowledging stops being
//! woken; the store keeps filling meanwhile.
//!
//! The waker is dropped when the thread ends, so a doorbell whose only sender is the
//! waker closes then. That is how the UI learns the thread has ended, cleanly or by a
//! panic, and it then joins the handle (off its own thread) to find out which.
//!
//! # Link state
//!
//! [`IngestHandle::connection`] reports a [`ConnectionInfo`]: [`LinkState::Connecting`]
//! until the session's `Connected` is handled, [`LinkState::Connected`] with the
//! transport's own description of the link, then [`LinkState::Disconnected`] with the
//! error's message, or `None` for an orderly close. The description of the last
//! connection is kept after a disconnect. The thread sets the state *before* it stores
//! the matching notice line, so a snapshot that shows "Connected to …" or
//! "Disconnected…" can never be newer than the state read after it: the UI reads the
//! description and the reason from here instead of parsing notice text. A thread that
//! ends without the session's `Disconnected` (a stop, an event channel dropped early)
//! reports `Disconnected { error: None }`; one that panics leaves the last state.
//!
//! # Sinks
//!
//! A [`ChunkSink`] runs on the ingest thread and hears, in this order:
//!
//! - `on_connect(description)` once, when the session connects, after the notice line
//!   is stored;
//! - `on_chunk(bytes, at)` for every `Data` chunk, exactly as the session delivered it;
//! - `on_idle(now)` each time [`IDLE_INTERVAL`] (250 ms) passes with no session event
//!   and no local line, and again every interval while the quiet lasts, so a buffered
//!   recorder can flush without the UI driving it. The wait is a receive timeout on the
//!   event channel, not a separate timer. An idle tick publishes nothing and wakes
//!   nobody;
//! - `on_disconnect()` exactly once before the thread ends, from the session's
//!   `Disconnected` or, if the thread stops first, on the way out. No `on_idle` follows
//!   it.
//!
//! **Ordering.** Local lines sent through [`IngestHandle::append_local`] are applied
//! before the session event the thread is about to handle, so a sent-command echo
//! always lands before the reply it caused.
//!
//! The thread ends when the session's event channel closes (the session is gone) or on
//! [`IngestHandle::stop`]; either way the store is handed back by `stop`/`join`, so a
//! reconnect can keep the same scrollback. A panic on the thread (in a sink, say) is
//! logged and reported as [`IngestPanicked`]; it never reaches the caller's thread.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, select, unbounded};
use parking_lot::Mutex;

use crate::session::SessionEvent;
use crate::store::{Snapshot, Store, StoreReader, StoreStats};
use crate::text::Direction;

/// Name of the ingest thread.
pub const INGEST_THREAD_NAME: &str = "serialist-ingest";

/// Events handled per batch before the UI is woken, so a flood of small chunks still
/// wakes it promptly.
const MAX_BATCH: usize = 256;

/// How long the thread waits for an event before it calls [`ChunkSink::on_idle`].
pub const IDLE_INTERVAL: Duration = Duration::from_millis(250);

/// Receives every chunk exactly as the session delivered it, on the ingest thread.
/// Keep it quick: the ingest thread waits for it.
pub trait ChunkSink: Send {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant);
    /// The session disconnected. No more chunks will arrive from it.
    fn on_disconnect(&mut self);
    /// The session connected; `description` is the transport's own name for the link (the
    /// text after "Connected to " in the notice line). Called once, before the first
    /// chunk, after [`IngestHandle::connection`] reports the link as connected and the
    /// notice is stored. The default does nothing.
    fn on_connect(&mut self, description: &str) {
        let _ = description;
    }
    /// The thread has waited [`IDLE_INTERVAL`] without a session event or a local line,
    /// at `now`: a buffered recorder can flush without anything driving it. It repeats
    /// every interval for as long as the quiet lasts, and stops after
    /// [`on_disconnect`](Self::on_disconnect). The default does nothing.
    fn on_idle(&mut self, now: Instant) {
        let _ = now;
    }
}

/// Where the link stands, as far as the ingest thread has seen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LinkState {
    /// The thread has not handled the session's `Connected` yet.
    #[default]
    Connecting,
    /// The session connected. `description` is the transport's own name for the link.
    Connected { description: String },
    /// The link is gone: the session's `Disconnected` was handled, or the thread ended
    /// without one. `error` is the transport error's message, `None` for an orderly close.
    Disconnected { error: Option<String> },
}

/// What [`IngestHandle::connection`] reports.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub state: LinkState,
    /// The description of the most recent `Connected`, kept after a disconnect, so a link
    /// that connects and drops between two looks can still be named.
    pub description: Option<String>,
}

/// Returned when the ingest thread has already ended.
#[derive(Debug, thiserror::Error)]
#[error("the ingest thread has stopped")]
pub struct IngestStopped;

/// The ingest thread panicked, so its store is gone. The panic has been logged.
#[derive(Debug, thiserror::Error)]
#[error("the ingest thread panicked: {message}")]
pub struct IngestPanicked {
    /// The panic's message, if it had one.
    pub message: String,
}

/// Counters for the status line and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IngestStats {
    /// `Data` chunks appended.
    pub chunks: u64,
    /// Bytes in those chunks.
    pub bytes: u64,
    /// Times the waker was called.
    pub wakes: u64,
    pub store: StoreStats,
}

enum Command {
    Local(String, Direction, Instant),
    Stop,
}

#[derive(Default)]
struct Shared {
    connection: Mutex<ConnectionInfo>,
    dirty: AtomicBool,
    chunks: AtomicU64,
    bytes: AtomicU64,
    wakes: AtomicU64,
}

/// Starts ingest threads.
#[derive(Debug)]
pub struct Ingest;

impl Ingest {
    /// Start the ingest thread for one session's `events`.
    pub fn spawn(
        events: Receiver<SessionEvent>,
        store: Store,
        sinks: Vec<Box<dyn ChunkSink>>,
        waker: Box<dyn Fn() + Send>,
    ) -> IngestHandle {
        let reader = store.reader();
        let shared = Arc::new(Shared::default());
        let (commands, command_rx) = unbounded();
        let worker = Worker {
            events,
            commands: command_rx,
            store,
            sinks,
            waker,
            shared: Arc::clone(&shared),
            sinks_closed: false,
        };
        let thread = thread::Builder::new()
            .name(INGEST_THREAD_NAME.into())
            .spawn(move || worker.run())
            .expect("spawn the ingest thread");
        IngestHandle {
            reader,
            commands,
            shared,
            thread: Some(thread),
        }
    }
}

/// Controls a running ingest thread. Dropping it stops the thread.
pub struct IngestHandle {
    reader: StoreReader,
    commands: Sender<Command>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Store>>,
}

impl fmt::Debug for IngestHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IngestHandle")
            .field("stats", &self.stats())
            .field("finished", &self.is_finished())
            .finish()
    }
}

impl IngestHandle {
    /// A reader for the UI: snapshots, stats.
    pub fn reader(&self) -> StoreReader {
        self.reader.clone()
    }

    pub fn snapshot(&self) -> Snapshot {
        self.reader.snapshot()
    }

    /// Queue local lines (a `Tx` echo of what was sent, or a `Notice`), stamped now.
    pub fn append_local(
        &self,
        text: impl Into<String>,
        direction: Direction,
    ) -> Result<(), IngestStopped> {
        self.commands
            .send(Command::Local(text.into(), direction, Instant::now()))
            .map_err(|_| IngestStopped)
    }

    /// Where the link stands now. The ingest thread updates it before it stores the
    /// notice line that announces the change, so a snapshot holding "Connected to …" or
    /// "Disconnected…" is never newer than this. Costs one uncontended lock and a clone;
    /// read it after [`acknowledge`](Self::acknowledge), like the snapshot.
    pub fn connection(&self) -> ConnectionInfo {
        self.shared.connection.lock().clone()
    }

    /// The UI has rendered (or is about to take its snapshot): the next publication may
    /// wake it again.
    pub fn acknowledge(&self) {
        self.shared.dirty.store(false, Ordering::Release);
    }

    pub fn stats(&self) -> IngestStats {
        IngestStats {
            chunks: self.shared.chunks.load(Ordering::Relaxed),
            bytes: self.shared.bytes.load(Ordering::Relaxed),
            wakes: self.shared.wakes.load(Ordering::Relaxed),
            store: self.reader.stats(),
        }
    }

    /// The thread has ended (its session's channel closed, or it was stopped).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Stop now and return the store. The event the thread has in hand is still
    /// ingested; events still queued are not. Sinks hear `on_disconnect` if they have
    /// not already.
    pub fn stop(mut self) -> Result<Store, IngestPanicked> {
        let _ = self.commands.send(Command::Stop);
        self.take_store()
    }

    /// Wait until the session's event channel closes and everything in it is ingested,
    /// then return the store.
    pub fn join(mut self) -> Result<Store, IngestPanicked> {
        self.take_store()
    }

    fn take_store(&mut self) -> Result<Store, IngestPanicked> {
        let thread = self.thread.take().expect("the thread is joined once");
        thread.join().map_err(|payload| {
            let message = panic_message(payload.as_ref());
            tracing::error!(%message, "the ingest thread panicked");
            IngestPanicked { message }
        })
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

impl Drop for IngestHandle {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = self.commands.send(Command::Stop);
            // A panic is logged by take_store; a drop has nobody to report it to.
            let _ = self.take_store();
        }
    }
}

struct Worker {
    events: Receiver<SessionEvent>,
    commands: Receiver<Command>,
    store: Store,
    sinks: Vec<Box<dyn ChunkSink>>,
    waker: Box<dyn Fn() + Send>,
    shared: Arc<Shared>,
    /// Every sink has heard `on_disconnect`.
    sinks_closed: bool,
}

/// Why the loop should end.
enum Flow {
    Continue,
    Stop,
}

impl Worker {
    fn run(mut self) -> Store {
        loop {
            let flow = select! {
                recv(self.commands) -> command => match command {
                    Ok(command) => self.command(command),
                    // Every handle is gone: nobody can read the store any more.
                    Err(_) => Flow::Stop,
                },
                recv(self.events) -> event => match event {
                    Ok(event) => self.batch(event),
                    Err(_) => {
                        // The session is gone. Apply queued local lines, then finish.
                        self.drain_commands();
                        Flow::Stop
                    }
                },
                // Nothing arrived for a while. Nothing was published either, so there
                // is nobody to wake.
                default(IDLE_INTERVAL) => {
                    self.idle();
                    continue;
                }
            };
            self.wake();
            if let Flow::Stop = flow {
                break;
            }
        }
        // A thread that ends without the session's `Disconnected` (a stop, or an event
        // channel dropped early) leaves nothing connected.
        let ended = !matches!(
            self.shared.connection.lock().state,
            LinkState::Disconnected { .. }
        );
        if ended {
            self.set_state(LinkState::Disconnected { error: None });
        }
        self.close_sinks();
        self.store
    }

    /// Handle `first` and whatever else is already queued, up to a batch. Local lines
    /// queued before an event are applied before it, and an event taken off the queue
    /// is always ingested, even when a stop was queued with those lines.
    fn batch(&mut self, first: SessionEvent) -> Flow {
        let mut flow = self.drain_commands();
        self.event(first);
        for _ in 1..MAX_BATCH {
            if let Flow::Stop = flow {
                break;
            }
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            flow = self.drain_commands();
            self.event(event);
        }
        flow
    }

    /// Tell the sinks that still expect chunks that the thread has been idle.
    fn idle(&mut self) {
        if self.sinks_closed {
            return;
        }
        let now = Instant::now();
        for sink in &mut self.sinks {
            sink.on_idle(now);
        }
    }

    /// Tell every sink the stream has ended, once.
    fn close_sinks(&mut self) {
        if !self.sinks_closed {
            self.sinks_closed = true;
            for sink in &mut self.sinks {
                sink.on_disconnect();
            }
        }
    }

    fn drain_commands(&mut self) -> Flow {
        loop {
            match self.commands.try_recv() {
                Ok(command) => {
                    if let Flow::Stop = self.command(command) {
                        return Flow::Stop;
                    }
                }
                Err(TryRecvError::Empty) => return Flow::Continue,
                Err(TryRecvError::Disconnected) => return Flow::Stop,
            }
        }
    }

    fn command(&mut self, command: Command) -> Flow {
        match command {
            Command::Local(text, direction, at) => {
                self.store.append_local_at(&text, direction, at);
                Flow::Continue
            }
            Command::Stop => Flow::Stop,
        }
    }

    fn event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::Data { bytes, received_at } => {
                self.store.append(&bytes, received_at);
                for sink in &mut self.sinks {
                    sink.on_chunk(&bytes, received_at);
                }
                self.shared.chunks.fetch_add(1, Ordering::Relaxed);
                self.shared
                    .bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
            SessionEvent::Connected { description } => {
                self.set_state(LinkState::Connected {
                    description: description.clone(),
                });
                self.store
                    .append_local(&format!("Connected to {description}"), Direction::Notice);
                for sink in &mut self.sinks {
                    sink.on_connect(&description);
                }
            }
            SessionEvent::Disconnected { error } => {
                let error = error.map(|error| error.to_string());
                let text = match &error {
                    Some(error) => format!("Disconnected: {error}"),
                    None => "Disconnected".to_owned(),
                };
                self.set_state(LinkState::Disconnected { error });
                self.store.append_local(&text, Direction::Notice);
                self.close_sinks();
            }
            SessionEvent::WriteFailed(error) => {
                self.store
                    .append_local(&format!("Write failed: {error}"), Direction::Notice);
            }
        }
    }

    /// Publish a new link state. Called before the matching notice line is stored.
    fn set_state(&self, state: LinkState) {
        let mut info = self.shared.connection.lock();
        if let LinkState::Connected { description } = &state {
            info.description = Some(description.clone());
        }
        info.state = state;
    }

    /// Mark the store dirty and wake the UI unless a wake is already pending.
    fn wake(&self) {
        if !self.shared.dirty.swap(true, Ordering::AcqRel) {
            self.shared.wakes.fetch_add(1, Ordering::Relaxed);
            (self.waker)();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::*;
    use crate::text::{LineId, LineSource};
    use crate::transport::TransportError;

    /// Records chunks and counts `on_disconnect` calls.
    struct Recorder(Arc<Mutex<(Vec<u8>, usize)>>);

    impl ChunkSink for Recorder {
        fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
            self.0.lock().unwrap().0.extend_from_slice(bytes);
        }

        fn on_disconnect(&mut self) {
            self.0.lock().unwrap().1 += 1;
        }
    }

    /// Panics on the first chunk.
    struct Exploding;

    impl ChunkSink for Exploding {
        fn on_chunk(&mut self, _bytes: &[u8], _at: Instant) {
            panic!("sink exploded");
        }

        fn on_disconnect(&mut self) {}
    }

    fn data(bytes: &[u8]) -> SessionEvent {
        SessionEvent::Data {
            bytes: Arc::from(bytes),
            received_at: Instant::now(),
        }
    }

    #[test]
    fn events_become_lines_and_sinks_see_chunks() {
        let (tx, rx) = unbounded();
        let recorded = Arc::new(Mutex::new((Vec::new(), 0)));
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(Recorder(Arc::clone(&recorded)))],
            Box::new(|| {}),
        );
        let now = Instant::now();
        tx.send(SessionEvent::Connected {
            description: "virtual:x".into(),
        })
        .unwrap();
        tx.send(SessionEvent::Data {
            bytes: Arc::from(&b"hello\r\nwor"[..]),
            received_at: now,
        })
        .unwrap();
        tx.send(SessionEvent::Data {
            bytes: Arc::from(&b"ld\n"[..]),
            received_at: now,
        })
        .unwrap();
        tx.send(SessionEvent::Disconnected { error: None }).unwrap();
        drop(tx);
        let store = handle.join().expect("the ingest thread ran cleanly");
        let snap = store.snapshot();
        let mut lines = Vec::new();
        snap.lines(LineId(0)..snap.end(), &mut lines);
        let got: Vec<_> = lines
            .iter()
            .map(|l| (l.direction, l.text.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (Direction::Notice, "Connected to virtual:x"),
                (Direction::Rx, "hello"),
                (Direction::Rx, "world"),
                (Direction::Notice, "Disconnected"),
            ]
        );
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.0, b"hello\r\nworld\n");
        assert_eq!(recorded.1, 1, "exactly one on_disconnect");
    }

    #[test]
    fn stop_returns_the_store_and_append_local_then_fails() {
        let (tx, rx) = unbounded::<SessionEvent>();
        let handle = Ingest::spawn(rx, Store::default(), Vec::new(), Box::new(|| {}));
        handle.append_local("note", Direction::Notice).unwrap();
        // Wait for it to land, then stop.
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.snapshot().line_count() == 0 {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        let reader = handle.reader();
        let store = handle.stop().expect("the ingest thread ran cleanly");
        assert_eq!(store.snapshot().line(LineId(0)).unwrap().text, "note");
        assert_eq!(reader.snapshot().line_count(), 1);
        drop(tx);
    }

    /// A stop racing the session's `Disconnected`: however the race falls, the store
    /// comes back and every sink hears `on_disconnect` exactly once.
    #[test]
    fn stop_racing_a_disconnect_still_closes_sinks() {
        for round in 0..300 {
            let (tx, rx) = unbounded();
            let recorded = Arc::new(Mutex::new((Vec::new(), 0)));
            let handle = Ingest::spawn(
                rx,
                Store::default(),
                vec![Box::new(Recorder(Arc::clone(&recorded)))],
                Box::new(|| {}),
            );
            tx.send(data(b"last words\n")).unwrap();
            tx.send(SessionEvent::Disconnected { error: None }).unwrap();
            if round % 3 == 0 {
                thread::yield_now();
            }
            let store = handle.stop().expect("the ingest thread ran cleanly");
            let (bytes, disconnects) = recorded.lock().unwrap().clone();
            assert_eq!(disconnects, 1, "round {round}");
            // Whatever reached the sink also reached the store.
            assert_eq!(store.stats().raw_len, bytes.len() as u64, "round {round}");
        }
    }

    /// The event taken off the queue is ingested even when a stop was queued with the
    /// local lines ahead of it.
    #[test]
    fn the_event_in_hand_is_ingested_despite_a_queued_stop() {
        let (tx, rx) = unbounded::<SessionEvent>();
        let (cmd_tx, cmd_rx) = unbounded();
        let recorded = Arc::new(Mutex::new((Vec::new(), 0)));
        let mut worker = Worker {
            events: rx,
            commands: cmd_rx,
            store: Store::default(),
            sinks: vec![Box::new(Recorder(Arc::clone(&recorded)))],
            waker: Box::new(|| {}),
            shared: Arc::new(Shared::default()),
            sinks_closed: false,
        };
        cmd_tx
            .send(Command::Local("echo".into(), Direction::Tx, Instant::now()))
            .unwrap();
        cmd_tx.send(Command::Stop).unwrap();
        tx.send(SessionEvent::Disconnected { error: None }).unwrap();
        let first = worker.events.try_recv().unwrap();
        assert!(matches!(worker.batch(first), Flow::Stop));
        let snap = worker.store.snapshot();
        let mut lines = Vec::new();
        snap.lines(LineId(0)..snap.end(), &mut lines);
        let got: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(got, vec!["echo", "Disconnected"]);
        assert_eq!(recorded.lock().unwrap().1, 1);
    }

    /// A panic on the ingest thread is reported, not re-raised on the caller.
    #[test]
    fn a_panicking_sink_is_reported_not_propagated() {
        let (tx, rx) = unbounded();
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(Exploding)],
            Box::new(|| {}),
        );
        tx.send(data(b"boom\n")).unwrap();
        let err = handle.join().expect_err("the sink panicked");
        assert!(err.message.contains("sink exploded"), "{err}");
        // Stopping or dropping a handle whose thread panicked is quiet too.
        let (tx, rx) = unbounded();
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(Exploding)],
            Box::new(|| {}),
        );
        tx.send(data(b"boom\n")).unwrap();
        while !handle.is_finished() {
            thread::yield_now();
        }
        assert!(handle.stop().is_err());
        let (tx, rx) = unbounded();
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(Exploding)],
            Box::new(|| {}),
        );
        tx.send(data(b"boom\n")).unwrap();
        drop(handle);
    }

    /// Logs the link events a sink hears, in order.
    struct LinkLog(Arc<Mutex<Vec<String>>>);

    impl ChunkSink for LinkLog {
        fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
            self.0
                .lock()
                .unwrap()
                .push(format!("chunk {}", bytes.len()));
        }

        fn on_connect(&mut self, description: &str) {
            self.0
                .lock()
                .unwrap()
                .push(format!("connect {description}"));
        }

        fn on_disconnect(&mut self) {
            self.0.lock().unwrap().push("disconnect".to_owned());
        }
    }

    fn wait_for_lines(handle: &IngestHandle, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.snapshot().line_count() < count {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {count} lines"
            );
            thread::yield_now();
        }
    }

    fn connected(description: &str) -> SessionEvent {
        SessionEvent::Connected {
            description: description.into(),
        }
    }

    #[test]
    fn connection_follows_the_session_and_sinks_hear_the_connect() {
        let (tx, rx) = unbounded();
        let log = Arc::new(Mutex::new(Vec::new()));
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(LinkLog(Arc::clone(&log)))],
            Box::new(|| {}),
        );
        assert_eq!(handle.connection(), ConnectionInfo::default());
        assert_eq!(handle.connection().state, LinkState::Connecting);

        tx.send(connected("virtual:x @ 115200 8N1")).unwrap();
        wait_for_lines(&handle, 1);
        let info = handle.connection();
        assert_eq!(
            info.state,
            LinkState::Connected {
                description: "virtual:x @ 115200 8N1".into()
            }
        );
        assert_eq!(info.description.as_deref(), Some("virtual:x @ 115200 8N1"));

        tx.send(data(b"hi\n")).unwrap();
        tx.send(SessionEvent::Disconnected {
            error: Some(TransportError::Disconnected),
        })
        .unwrap();
        wait_for_lines(&handle, 3);
        let info = handle.connection();
        assert_eq!(
            info.state,
            LinkState::Disconnected {
                error: Some("device disconnected".into())
            }
        );
        assert_eq!(
            info.description.as_deref(),
            Some("virtual:x @ 115200 8N1"),
            "the description outlives the connection"
        );
        drop(tx);
        let store = handle.join().expect("the ingest thread ran cleanly");
        let last = store.snapshot().line(LineId(2)).unwrap();
        assert_eq!(last.text, "Disconnected: device disconnected");
        assert_eq!(
            *log.lock().unwrap(),
            ["connect virtual:x @ 115200 8N1", "chunk 3", "disconnect"]
        );
    }

    /// The state is stored before the notice line is published, so a reader that sees
    /// the line can never read an older state.
    #[test]
    fn the_state_leads_its_notice() {
        for round in 0..200 {
            let (tx, rx) = unbounded();
            let handle = Ingest::spawn(rx, Store::default(), Vec::new(), Box::new(|| {}));
            tx.send(connected("virtual:x")).unwrap();
            while handle.snapshot().line_count() < 1 {
                thread::yield_now();
            }
            assert!(
                matches!(handle.connection().state, LinkState::Connected { .. }),
                "round {round}: {:?}",
                handle.connection()
            );
            tx.send(SessionEvent::Disconnected { error: None }).unwrap();
            while handle.snapshot().line_count() < 2 {
                thread::yield_now();
            }
            assert_eq!(
                handle.connection().state,
                LinkState::Disconnected { error: None },
                "round {round}"
            );
        }
    }

    /// A thread that ends without the session's `Disconnected` leaves nothing connected.
    #[test]
    fn a_thread_that_ends_early_reports_disconnected() {
        for send_connected in [false, true] {
            let (tx, rx) = unbounded::<SessionEvent>();
            let (_cmd_tx, cmd_rx) = unbounded();
            let shared = Arc::new(Shared::default());
            let worker = Worker {
                events: rx,
                commands: cmd_rx,
                store: Store::default(),
                sinks: Vec::new(),
                waker: Box::new(|| {}),
                shared: Arc::clone(&shared),
                sinks_closed: false,
            };
            if send_connected {
                tx.send(connected("virtual:x")).unwrap();
            }
            // The event channel closes with no `Disconnected` in it.
            drop(tx);
            let _store = worker.run();
            let info = shared.connection.lock().clone();
            assert_eq!(info.state, LinkState::Disconnected { error: None });
            assert_eq!(info.description.is_some(), send_connected);
        }
    }

    #[derive(Default)]
    struct IdleState {
        idles: Vec<Instant>,
        disconnected: bool,
        idles_after_disconnect: usize,
    }

    /// Records every `on_idle`, and any that come after `on_disconnect`.
    struct IdleSink(Arc<Mutex<IdleState>>);

    impl ChunkSink for IdleSink {
        fn on_chunk(&mut self, _bytes: &[u8], _at: Instant) {}

        fn on_idle(&mut self, now: Instant) {
            let mut state = self.0.lock().unwrap();
            state.idles.push(now);
            if state.disconnected {
                state.idles_after_disconnect += 1;
            }
        }

        fn on_disconnect(&mut self) {
            self.0.lock().unwrap().disconnected = true;
        }
    }

    #[test]
    fn a_quiet_thread_tells_its_sinks_it_is_idle() {
        let (tx, rx) = unbounded::<SessionEvent>();
        let state = Arc::new(Mutex::new(IdleState::default()));
        let wakes = Arc::new(AtomicU64::new(0));
        let waker_wakes = Arc::clone(&wakes);
        let started = Instant::now();
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(IdleSink(Arc::clone(&state)))],
            Box::new(move || {
                waker_wakes.fetch_add(1, Ordering::Relaxed);
            }),
        );
        let deadline = started + Duration::from_secs(10);
        while state.lock().unwrap().idles.len() < 3 {
            assert!(Instant::now() < deadline, "no idle calls arrived");
            thread::sleep(Duration::from_millis(10));
        }
        let idles = state.lock().unwrap().idles.clone();
        // Each call comes after a full quiet interval (allowing for clock granularity).
        let slack = Duration::from_millis(15);
        assert!(idles[0] - started + slack >= IDLE_INTERVAL, "{idles:?}");
        for pair in idles.windows(2) {
            assert!(pair[1] - pair[0] + slack >= IDLE_INTERVAL, "{idles:?}");
        }
        // Idling publishes nothing, so it wakes nobody.
        assert_eq!(wakes.load(Ordering::Relaxed), 0);
        drop(tx);
        handle.join().expect("the ingest thread ran cleanly");
    }

    #[test]
    fn no_idle_call_follows_the_disconnect() {
        let (tx, rx) = unbounded::<SessionEvent>();
        let state = Arc::new(Mutex::new(IdleState::default()));
        let handle = Ingest::spawn(
            rx,
            Store::default(),
            vec![Box::new(IdleSink(Arc::clone(&state)))],
            Box::new(|| {}),
        );
        tx.send(SessionEvent::Disconnected { error: None }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !state.lock().unwrap().disconnected {
            assert!(Instant::now() < deadline, "no disconnect");
            thread::sleep(Duration::from_millis(2));
        }
        // The thread lives on until the channel closes; it must stay quiet meanwhile.
        thread::sleep(IDLE_INTERVAL * 2 + Duration::from_millis(100));
        assert_eq!(state.lock().unwrap().idles_after_disconnect, 0);
        drop(tx);
        handle.join().expect("the ingest thread ran cleanly");
    }
}
