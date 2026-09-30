//! The ingest thread: the one place received bytes turn into scrollback.
//!
//! [`Ingest::spawn`] starts a thread that owns the [`Store`]. It drains a session's
//! events, appends each `Data` chunk to the store (one parse per chunk), hands the same
//! chunk unchanged to every [`ChunkSink`] (recording is one), and turns connect,
//! disconnect and write failures into `Notice` lines. The UI never parses bytes: it gets
//! a [`StoreReader`] and a waker.
//!
//! **Waking the UI.** After new lines are published the thread sets a dirty flag and
//! calls the waker only if the flag was clear, so the UI gets at most one wake per
//! frame however many chunks arrive. The UI calls [`IngestHandle::acknowledge`] when it
//! renders, before it takes that frame's snapshot, so anything published after the
//! snapshot wakes it again.
//!
//! **Ordering.** Local lines sent through [`IngestHandle::append_local`] are applied
//! before the session event the thread is about to handle, so a sent-command echo
//! always lands before the reply it caused.
//!
//! The thread ends when the session's event channel closes (the session is gone) or on
//! [`IngestHandle::stop`]; either way the store is handed back by `stop`/`join`, so a
//! reconnect can keep the same scrollback.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, TryRecvError, select, unbounded};

use crate::session::SessionEvent;
use crate::store::{Snapshot, Store, StoreReader, StoreStats};
use crate::text::Direction;

/// Name of the ingest thread.
pub const INGEST_THREAD_NAME: &str = "serialist-ingest";

/// Events handled per batch before the UI is woken, so a flood of small chunks still
/// wakes it promptly.
const MAX_BATCH: usize = 256;

/// Receives every chunk exactly as the session delivered it, on the ingest thread.
/// Keep it quick: the ingest thread waits for it.
pub trait ChunkSink: Send {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant);
    /// The session disconnected. No more chunks will arrive from it.
    fn on_disconnect(&mut self);
}

/// Returned when the ingest thread has already ended.
#[derive(Debug, thiserror::Error)]
#[error("the ingest thread has stopped")]
pub struct IngestStopped;

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

    /// Stop now, after the event in hand, and return the store. Events still queued are
    /// not ingested.
    pub fn stop(mut self) -> Store {
        let _ = self.commands.send(Command::Stop);
        self.take_store()
    }

    /// Wait until the session's event channel closes and everything in it is ingested,
    /// then return the store.
    pub fn join(mut self) -> Store {
        self.take_store()
    }

    fn take_store(&mut self) -> Store {
        let thread = self.thread.take().expect("joined once");
        match thread.join() {
            Ok(store) => store,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}

impl Drop for IngestHandle {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.commands.send(Command::Stop);
            if thread.join().is_err() {
                tracing::error!("the ingest thread panicked");
            }
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
                        self.wake();
                        return self.store;
                    }
                },
            };
            self.wake();
            if let Flow::Stop = flow {
                return self.store;
            }
        }
    }

    /// Handle `first` and whatever else is already queued, up to a batch.
    fn batch(&mut self, first: SessionEvent) -> Flow {
        if let Flow::Stop = self.drain_commands() {
            return Flow::Stop;
        }
        self.event(first);
        for _ in 1..MAX_BATCH {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            if let Flow::Stop = self.drain_commands() {
                return Flow::Stop;
            }
            self.event(event);
        }
        Flow::Continue
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
                self.store
                    .append_local(&format!("Connected to {description}"), Direction::Notice);
            }
            SessionEvent::Disconnected { error } => {
                let text = match error {
                    Some(error) => format!("Disconnected: {error}"),
                    None => "Disconnected".to_owned(),
                };
                self.store.append_local(&text, Direction::Notice);
                for sink in &mut self.sinks {
                    sink.on_disconnect();
                }
            }
            SessionEvent::WriteFailed(error) => {
                self.store
                    .append_local(&format!("Write failed: {error}"), Direction::Notice);
            }
        }
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

    struct Recorder(Arc<Mutex<(Vec<u8>, bool)>>);

    impl ChunkSink for Recorder {
        fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
            self.0.lock().unwrap().0.extend_from_slice(bytes);
        }

        fn on_disconnect(&mut self) {
            self.0.lock().unwrap().1 = true;
        }
    }

    #[test]
    fn events_become_lines_and_sinks_see_chunks() {
        let (tx, rx) = unbounded();
        let recorded = Arc::new(Mutex::new((Vec::new(), false)));
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
        let store = handle.join();
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
        assert!(recorded.1);
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
        let store = handle.stop();
        assert_eq!(store.snapshot().line(LineId(0)).unwrap().text, "note");
        assert_eq!(reader.snapshot().line_count(), 1);
        drop(tx);
    }
}
