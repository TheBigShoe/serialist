//! A session owns one open transport and the two threads that service it.
//!
//! Contract for milestone 0. The reader thread blocks in `TransportReader::read` with
//! `SessionConfig::read_timeout`, hands every chunk to the event channel unchanged, and
//! polls a stop flag between reads. The writer thread drains a queue of outgoing writes.
//! Neither thread ever runs UI code; the UI drains `events()` from its own executor.
//!
//! Event order is a guarantee: `Connected` is always first, and exactly one
//! `Disconnected` is always last. Whichever comes first (the reader seeing the device go
//! away, or `close`/drop) emits it; nothing is emitted after it.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;

use crate::config::SerialConfig;
use crate::port::PortId;
use crate::transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};

/// Name of the thread that blocks in `TransportReader::read`.
pub const READER_THREAD_NAME: &str = "serialist-reader";
/// Name of the thread that drains the write queue.
pub const WRITER_THREAD_NAME: &str = "serialist-writer";

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub port: PortId,
    pub serial: SerialConfig,
    /// Per-read timeout on the reader thread. Never zero: a zero timeout spins a core.
    pub read_timeout: Duration,
    /// Size of the reader's scratch buffer; one `read` never returns more than this.
    pub read_buffer_size: usize,
}

impl SessionConfig {
    pub fn new(port: PortId, serial: SerialConfig) -> Self {
        Self {
            port,
            serial,
            read_timeout: Duration::from_millis(20),
            read_buffer_size: 64 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    Connected {
        description: String,
    },
    /// A chunk exactly as the transport delivered it. The session never splits or merges chunks.
    Data {
        bytes: Arc<[u8]>,
        received_at: Instant,
    },
    /// The link is gone. `error` is `None` after an orderly `close()`.
    Disconnected {
        error: Option<TransportError>,
    },
    WriteFailed(TransportError),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Number of `read` calls that returned data; with `rx_bytes` this gives the mean chunk size.
    pub rx_chunks: u64,
}

#[derive(Debug, thiserror::Error)]
#[error("session is closed")]
pub struct SessionClosed;

enum Command {
    Write(Vec<u8>),
    Control(ControlLine, bool),
    Reconfigure(SerialConfig),
}

/// State shared by the session handle and its two threads.
struct Shared {
    stop: AtomicBool,
    connected: AtomicBool,
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_chunks: AtomicU64,
    /// The event sender. Whoever emits the one `Disconnected` takes it, which closes the
    /// gate: nothing can be emitted after `Disconnected`.
    events: Mutex<Option<Sender<SessionEvent>>>,
    /// Line settings as last applied by the writer thread.
    serial: Mutex<SerialConfig>,
}

impl Shared {
    /// Returns false once the gate is closed.
    fn emit(&self, event: SessionEvent) -> bool {
        match &*self.events.lock() {
            Some(tx) => tx.send(event).is_ok(),
            None => false,
        }
    }

    /// Emit `Disconnected` unless it already was. Marks the session disconnected before
    /// the event is visible, so a consumer that sees it also sees `is_connected() == false`.
    fn emit_disconnected(&self, error: Option<TransportError>) {
        let mut gate = self.events.lock();
        if let Some(tx) = gate.take() {
            self.connected.store(false, Ordering::Release);
            let _ = tx.send(SessionEvent::Disconnected { error });
        }
    }
}

/// One open port with its reader and writer threads. Dropping a session stops both threads.
pub struct Session {
    config: SessionConfig,
    events: Receiver<SessionEvent>,
    shared: Arc<Shared>,
    commands: Option<Sender<Command>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Session {
    /// Open the port through `factory` and start the reader and writer threads.
    /// The first event on `events()` is `Connected`.
    ///
    /// Fails with `TransportError::Config` for a zero `read_timeout` or
    /// `read_buffer_size`, and with whatever the factory returns if the port cannot open.
    pub fn open(
        factory: &dyn TransportFactory,
        config: SessionConfig,
    ) -> Result<Self, TransportError> {
        if config.read_timeout.is_zero() {
            return Err(TransportError::Config(
                "read_timeout must be greater than zero; a zero timeout spins a core".into(),
            ));
        }
        if config.read_buffer_size == 0 {
            return Err(TransportError::Config(
                "read_buffer_size must be greater than zero".into(),
            ));
        }

        let Transport {
            reader,
            writer,
            description,
        } = factory.open(&config.port, &config.serial)?;

        let (event_tx, event_rx) = unbounded();
        // Sent before either thread exists, so it is always first.
        let _ = event_tx.send(SessionEvent::Connected { description });
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_chunks: AtomicU64::new(0),
            events: Mutex::new(Some(event_tx)),
            serial: Mutex::new(config.serial.clone()),
        });

        let (command_tx, command_rx) = unbounded();
        let writer_shared = Arc::clone(&shared);
        let writer = thread::Builder::new()
            .name(WRITER_THREAD_NAME.into())
            .spawn(move || writer_loop(writer, &command_rx, &writer_shared))
            .map_err(TransportError::Io)?;

        let reader_shared = Arc::clone(&shared);
        let (timeout, buffer_size) = (config.read_timeout, config.read_buffer_size);
        let reader = match thread::Builder::new()
            .name(READER_THREAD_NAME.into())
            .spawn(move || reader_loop(reader, &reader_shared, timeout, buffer_size))
        {
            Ok(handle) => handle,
            Err(err) => {
                shared.stop.store(true, Ordering::Release);
                drop(command_tx);
                let _ = writer.join();
                return Err(TransportError::Io(err));
            }
        };

        tracing::debug!(port = %config.port, serial = %config.serial.summary(), "session open");
        Ok(Self {
            config,
            events: event_rx,
            shared,
            commands: Some(command_tx),
            reader: Some(reader),
            writer: Some(writer),
        })
    }

    /// A clone of the event receiver. Events are delivered in order; `Data` chunks are never dropped.
    pub fn events(&self) -> Receiver<SessionEvent> {
        self.events.clone()
    }

    /// Queue bytes for the writer thread. Returns immediately.
    pub fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        self.send(Command::Write(bytes))
    }

    pub fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed> {
        self.send(Command::Control(line, asserted))
    }

    /// Apply new line settings without reopening the port.
    pub fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed> {
        self.send(Command::Reconfigure(serial))
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats {
            rx_bytes: self.shared.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.shared.tx_bytes.load(Ordering::Relaxed),
            rx_chunks: self.shared.rx_chunks.load(Ordering::Relaxed),
        }
    }

    /// The configuration the session was opened with. After `reconfigure`, see
    /// [`Session::serial_config`] for the line settings in effect.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The line settings in effect: the opening settings, or the last ones the transport
    /// accepted through `reconfigure`.
    pub fn serial_config(&self) -> SerialConfig {
        self.shared.serial.lock().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// Stop both threads and join them. Emits `Disconnected { error: None }` first, unless
    /// the link already reported its own `Disconnected`. Takes at most about one read
    /// timeout. Writes still queued are discarded.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn send(&self, command: Command) -> Result<(), SessionClosed> {
        if !self.is_connected() {
            return Err(SessionClosed);
        }
        let commands = self.commands.as_ref().ok_or(SessionClosed)?;
        commands.send(command).map_err(|_| SessionClosed)
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.emit_disconnected(None);
        // Dropping the only sender wakes the writer if it is idle.
        self.commands = None;
        for handle in [self.writer.take(), self.reader.take()]
            .into_iter()
            .flatten()
        {
            if handle.join().is_err() {
                tracing::error!("a session thread panicked");
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("port", &self.config.port)
            .field("connected", &self.is_connected())
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

fn is_transient(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::Interrupted | io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

/// Reports a panicking reader as a disconnect, so the UI is never left waiting.
struct ReaderExit<'a>(&'a Shared);

impl Drop for ReaderExit<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.0
                .emit_disconnected(Some(TransportError::Io(io::Error::other(
                    "reader thread panicked",
                ))));
        }
    }
}

fn reader_loop(
    mut reader: Box<dyn TransportReader>,
    shared: &Shared,
    timeout: Duration,
    buffer_size: usize,
) {
    let _exit = ReaderExit(shared);
    let mut buf = vec![0u8; buffer_size];
    while !shared.stop.load(Ordering::Acquire) {
        match reader.read(&mut buf, timeout) {
            Ok(0) => {}
            Ok(n) => {
                let received_at = Instant::now();
                let n = n.min(buf.len());
                shared.rx_bytes.fetch_add(n as u64, Ordering::Relaxed);
                shared.rx_chunks.fetch_add(1, Ordering::Relaxed);
                let bytes: Arc<[u8]> = Arc::from(&buf[..n]);
                if !shared.emit(SessionEvent::Data { bytes, received_at }) {
                    break;
                }
            }
            Err(TransportError::Io(err)) if is_transient(&err) => {}
            Err(err) => {
                tracing::debug!(%err, "reader stopped");
                shared.emit_disconnected(Some(err));
                break;
            }
        }
    }
}

fn writer_loop(
    mut writer: Box<dyn TransportWriter>,
    commands: &Receiver<Command>,
    shared: &Shared,
) {
    for command in commands {
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let result = match command {
            Command::Write(bytes) => writer.write_all(&bytes).map(|()| {
                shared
                    .tx_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }),
            Command::Control(line, asserted) => writer.set_control(line, asserted),
            Command::Reconfigure(serial) => writer
                .reconfigure(&serial)
                .map(|()| *shared.serial.lock() = serial),
        };
        if let Err(err) = result {
            tracing::warn!(%err, "write failed");
            shared.emit(SessionEvent::WriteFailed(err));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crossbeam_channel::RecvTimeoutError;

    enum Step {
        Data(Vec<u8>),
        Fail(TransportError),
        Panic,
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Op {
        Write(Vec<u8>),
        Control(ControlLine, bool),
        Reconfigure(u32),
    }

    #[derive(Default)]
    struct Probe {
        ops: Mutex<Vec<Op>>,
        fail_writes: AtomicBool,
        reads: AtomicU64,
        reader_dropped: AtomicBool,
        writer_dropped: AtomicBool,
        reader_thread: Mutex<Option<String>>,
        writer_thread: Mutex<Option<String>>,
    }

    fn thread_name() -> Option<String> {
        thread::current().name().map(str::to_owned)
    }

    struct MockReader {
        steps: Receiver<Step>,
        probe: Arc<Probe>,
    }

    impl TransportReader for MockReader {
        fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
            self.probe.reads.fetch_add(1, Ordering::Relaxed);
            *self.probe.reader_thread.lock() = thread_name();
            match self.steps.recv_timeout(timeout) {
                Ok(Step::Data(bytes)) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                Ok(Step::Fail(err)) => Err(err),
                Ok(Step::Panic) => panic!("mock reader told to panic"),
                Err(RecvTimeoutError::Timeout) => Ok(0),
                Err(RecvTimeoutError::Disconnected) => {
                    thread::sleep(timeout);
                    Ok(0)
                }
            }
        }
    }

    impl Drop for MockReader {
        fn drop(&mut self) {
            self.probe.reader_dropped.store(true, Ordering::SeqCst);
        }
    }

    struct MockWriter {
        probe: Arc<Probe>,
    }

    impl MockWriter {
        fn record(&self, op: Op) -> Result<(), TransportError> {
            *self.probe.writer_thread.lock() = thread_name();
            if self.probe.fail_writes.load(Ordering::SeqCst) {
                return Err(TransportError::Io(io::Error::other("mock write failure")));
            }
            self.probe.ops.lock().push(op);
            Ok(())
        }
    }

    impl TransportWriter for MockWriter {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.record(Op::Write(bytes.to_vec()))
        }

        fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError> {
            self.record(Op::Control(line, asserted))
        }

        fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
            self.record(Op::Reconfigure(config.baud))
        }
    }

    impl Drop for MockWriter {
        fn drop(&mut self) {
            self.probe.writer_dropped.store(true, Ordering::SeqCst);
        }
    }

    /// Hands out one mock transport, driven through the returned sender.
    struct MockFactory {
        steps: Mutex<Option<Receiver<Step>>>,
        probe: Arc<Probe>,
    }

    impl TransportFactory for MockFactory {
        fn open(&self, port: &PortId, _config: &SerialConfig) -> Result<Transport, TransportError> {
            let steps = self
                .steps
                .lock()
                .take()
                .ok_or_else(|| TransportError::NotFound(port.clone()))?;
            Ok(Transport {
                reader: Box::new(MockReader {
                    steps,
                    probe: Arc::clone(&self.probe),
                }),
                writer: Box::new(MockWriter {
                    probe: Arc::clone(&self.probe),
                }),
                description: format!("mock:{port}"),
            })
        }
    }

    fn mock() -> (MockFactory, Sender<Step>, Arc<Probe>) {
        let (tx, rx) = unbounded();
        let probe = Arc::new(Probe::default());
        let factory = MockFactory {
            steps: Mutex::new(Some(rx)),
            probe: Arc::clone(&probe),
        };
        (factory, tx, probe)
    }

    fn config() -> SessionConfig {
        let mut cfg = SessionConfig::new(PortId::new("test"), SerialConfig::default());
        cfg.read_timeout = Duration::from_millis(10);
        cfg
    }

    fn next(events: &Receiver<SessionEvent>) -> SessionEvent {
        events
            .recv_timeout(Duration::from_secs(5))
            .expect("expected an event")
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "condition not met in time");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn session_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Session>();
        assert_send_sync::<SessionEvent>();
    }

    #[test]
    fn connected_first_then_chunks_unchanged() {
        let (factory, steps, _probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        match next(&events) {
            SessionEvent::Connected { description } => assert_eq!(description, "mock:test"),
            other => panic!("expected Connected, got {other:?}"),
        }
        steps.send(Step::Data(b"abc".to_vec())).unwrap();
        steps.send(Step::Data(b"defg".to_vec())).unwrap();
        for expected in [&b"abc"[..], b"defg"] {
            match next(&events) {
                SessionEvent::Data { bytes, .. } => assert_eq!(&*bytes, expected),
                other => panic!("expected Data, got {other:?}"),
            }
        }
        assert_eq!(
            session.stats(),
            SessionStats {
                rx_bytes: 7,
                tx_bytes: 0,
                rx_chunks: 2
            }
        );
        assert!(session.is_connected());
        assert_eq!(session.config().port, PortId::new("test"));
    }

    #[test]
    fn commands_reach_the_writer_in_order() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let slow = SerialConfig {
            baud: 9600,
            ..SerialConfig::default()
        };
        session.write(b"a".to_vec()).unwrap();
        session.set_control(ControlLine::Dtr, false).unwrap();
        session.reconfigure(slow.clone()).unwrap();
        session.write(b"bc".to_vec()).unwrap();
        wait_for(|| probe.ops.lock().len() == 4);
        assert_eq!(
            *probe.ops.lock(),
            [
                Op::Write(b"a".to_vec()),
                Op::Control(ControlLine::Dtr, false),
                Op::Reconfigure(9600),
                Op::Write(b"bc".to_vec()),
            ]
        );
        assert_eq!(session.stats().tx_bytes, 3);
        assert_eq!(session.serial_config(), slow);
        assert_eq!(session.config().serial.baud, 115_200);
    }

    #[test]
    fn write_failure_is_an_event() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        let _connected = next(&events);
        probe.fail_writes.store(true, Ordering::SeqCst);
        session.write(b"x".to_vec()).unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::WriteFailed(TransportError::Io(_))
        ));
        assert_eq!(session.stats().tx_bytes, 0);
        assert!(session.is_connected());
    }

    #[test]
    fn reader_disconnect_is_final() {
        let (factory, steps, _probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        let _connected = next(&events);
        steps.send(Step::Data(b"last".to_vec())).unwrap();
        steps
            .send(Step::Fail(TransportError::Disconnected))
            .unwrap();
        assert!(matches!(next(&events), SessionEvent::Data { .. }));
        match next(&events) {
            SessionEvent::Disconnected {
                error: Some(TransportError::Disconnected),
            } => {}
            other => panic!("expected Disconnected, got {other:?}"),
        }
        assert!(!session.is_connected());
        assert!(session.write(b"late".to_vec()).is_err());
        assert!(session.set_control(ControlLine::Rts, true).is_err());
        session.close();
        // No second Disconnected, and every sender is gone.
        assert!(events.recv_timeout(Duration::from_secs(1)).is_err());
    }

    #[test]
    fn close_emits_disconnected_last_and_is_prompt() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        wait_for(|| probe.reads.load(Ordering::Relaxed) > 0);
        let started = Instant::now();
        session.close();
        let took = started.elapsed();
        assert!(took < Duration::from_millis(500), "close took {took:?}");
        assert!(matches!(next(&events), SessionEvent::Connected { .. }));
        assert!(matches!(
            next(&events),
            SessionEvent::Disconnected { error: None }
        ));
        assert!(events.recv_timeout(Duration::from_secs(1)).is_err());
        assert!(probe.reader_dropped.load(Ordering::SeqCst));
        assert!(probe.writer_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn drop_stops_both_threads() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        drop(session);
        assert!(probe.reader_dropped.load(Ordering::SeqCst));
        assert!(probe.writer_dropped.load(Ordering::SeqCst));
        let tail: Vec<_> = events.try_iter().collect();
        assert!(matches!(
            tail.last(),
            Some(SessionEvent::Disconnected { error: None })
        ));
    }

    #[test]
    fn threads_are_named() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        session.write(b"x".to_vec()).unwrap();
        wait_for(|| probe.writer_thread.lock().is_some() && probe.reader_thread.lock().is_some());
        assert_eq!(
            probe.reader_thread.lock().as_deref(),
            Some(READER_THREAD_NAME)
        );
        assert_eq!(
            probe.writer_thread.lock().as_deref(),
            Some(WRITER_THREAD_NAME)
        );
    }

    #[test]
    fn idle_reader_does_not_spin() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        thread::sleep(Duration::from_millis(300));
        let reads = probe.reads.load(Ordering::Relaxed);
        // 10 ms timeout over 300 ms is about 30 reads.
        assert!((10..=60).contains(&reads), "{reads} reads in 300 ms");
        drop(session);
    }

    #[test]
    fn reader_panic_is_reported_as_disconnect() {
        let (factory, steps, _probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        let _connected = next(&events);
        steps.send(Step::Panic).unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::Disconnected {
                error: Some(TransportError::Io(_))
            }
        ));
        assert!(!session.is_connected());
    }

    #[test]
    fn open_validates_config_and_propagates_factory_errors() {
        let (factory, _steps, _probe) = mock();
        let mut zero = config();
        zero.read_timeout = Duration::ZERO;
        assert!(matches!(
            Session::open(&factory, zero),
            Err(TransportError::Config(_))
        ));
        let mut empty = config();
        empty.read_buffer_size = 0;
        assert!(matches!(
            Session::open(&factory, empty),
            Err(TransportError::Config(_))
        ));
        let first = Session::open(&factory, config()).unwrap();
        // The mock hands out one transport only.
        assert!(matches!(
            Session::open(&factory, config()),
            Err(TransportError::NotFound(_))
        ));
        drop(first);
    }
}
