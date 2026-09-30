//! A session owns one open transport and the two threads that service it.
//!
//! Contract for milestone 0. The reader thread blocks in `TransportReader::read` with
//! `SessionConfig::read_timeout`, hands every chunk to the event channel unchanged, and
//! polls a stop flag between reads. The writer thread drains a queue of outgoing writes.
//! Neither thread ever runs UI code; the UI drains `events()` from its own executor.
//!
//! Event order is a guarantee: `Connected` is always first, and exactly one
//! `Disconnected` is always last. Whichever comes first (the reader seeing the device go
//! away, a session thread failing, or `close`/`abort`/drop) emits it; nothing is emitted
//! after it.
//!
//! Once the link reports an error, both threads stop and both transport halves are
//! dropped straight away, not when the `Session` is dropped: on Linux a tty descriptor
//! held open makes a replugged adapter come back under a different path.

use std::fmt;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
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
/// The shortest `read_timeout` `Session::open` accepts.
pub const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);
/// How long `Session::close` waits for queued writes to go out before dropping the rest.
pub const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub port: PortId,
    pub serial: SerialConfig,
    /// Per-read timeout on the reader thread. 10 to 50 ms is the recommended range:
    /// shorter wakes the thread for nothing, longer delays `close`. Anything under
    /// [`MIN_READ_TIMEOUT`] is rejected, since a zero or near-zero timeout spins a core.
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
    /// Queued by `close`: everything before it goes out, then the writer exits.
    Close,
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
    /// Never sent on. Dropping it wakes the writer from its queue wait so it exits.
    writer_stop: Mutex<Option<Sender<()>>>,
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

    /// Stop both threads now: the writer after the operation in progress, the reader
    /// within one read timeout. Queued writes are dropped.
    fn stop_now(&self) {
        self.stop.store(true, Ordering::Release);
        self.writer_stop.lock().take();
    }

    /// A session thread failed: report it and bring the other thread down too.
    fn fail(&self, error: TransportError) {
        self.emit_disconnected(Some(error));
        self.stop_now();
    }
}

/// One open port with its reader and writer threads. Dropping a session stops both
/// threads at once (see [`Session::abort`]); [`Session::close`] lets queued writes out first.
pub struct Session {
    config: SessionConfig,
    events: Receiver<SessionEvent>,
    shared: Arc<Shared>,
    commands: Option<Sender<Command>>,
    /// Disconnects when the writer thread has exited.
    writer_done: Receiver<()>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Session {
    /// Open the port through `factory` and start the reader and writer threads.
    /// The first event on `events()` is `Connected`.
    ///
    /// Fails with `TransportError::Config` for a `read_timeout` under
    /// [`MIN_READ_TIMEOUT`] or a zero `read_buffer_size`, and with whatever the factory
    /// returns if the port cannot open.
    pub fn open(
        factory: &dyn TransportFactory,
        config: SessionConfig,
    ) -> Result<Self, TransportError> {
        if config.read_timeout < MIN_READ_TIMEOUT {
            return Err(TransportError::Config(format!(
                "read_timeout must be at least {MIN_READ_TIMEOUT:?} (10-50 ms recommended); \
                 a zero or near-zero timeout spins a core"
            )));
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
        let (stop_tx, stop_rx) = bounded::<()>(0);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            connected: AtomicBool::new(true),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            rx_chunks: AtomicU64::new(0),
            events: Mutex::new(Some(event_tx)),
            serial: Mutex::new(config.serial.clone()),
            writer_stop: Mutex::new(Some(stop_tx)),
        });

        let (command_tx, command_rx) = unbounded();
        let (done_tx, done_rx) = bounded::<()>(0);
        let writer_shared = Arc::clone(&shared);
        let writer = thread::Builder::new()
            .name(WRITER_THREAD_NAME.into())
            .spawn(move || {
                // Dropped when the thread ends, however it ends.
                let _done = done_tx;
                writer_loop(writer, &command_rx, &stop_rx, &writer_shared);
            })
            .map_err(TransportError::Io)?;

        let reader_shared = Arc::clone(&shared);
        let (timeout, buffer_size) = (config.read_timeout, config.read_buffer_size);
        let reader = match thread::Builder::new()
            .name(READER_THREAD_NAME.into())
            .spawn(move || reader_loop(reader, &reader_shared, timeout, buffer_size))
        {
            Ok(handle) => handle,
            Err(err) => {
                shared.stop_now();
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
            writer_done: done_rx,
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

    /// Apply new line settings without reopening the port. [`Session::serial_config`]
    /// reflects them once the transport has accepted them.
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

    /// The configuration the session was opened with. Its `serial` field does not follow
    /// `reconfigure`; [`Session::serial_config`] is the live value.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }

    /// The port this session is open on.
    pub fn port(&self) -> &PortId {
        &self.config.port
    }

    /// The line settings in effect: the opening settings, or the last ones the transport
    /// accepted through `reconfigure`.
    pub fn serial_config(&self) -> SerialConfig {
        self.shared.serial.lock().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.shared.connected.load(Ordering::Acquire)
    }

    /// Close in order: writes, control changes and reconfigures already queued go out
    /// first (for at most [`CLOSE_DRAIN_TIMEOUT`]; whatever is left then is dropped),
    /// `Data` keeps flowing meanwhile, then `Disconnected { error: None }` is emitted
    /// (unless the link already reported its own) and both threads are joined. Takes at
    /// most the drain timeout plus about one read timeout.
    pub fn close(mut self) {
        self.shutdown(true);
    }

    /// Stop at once: queued writes are dropped, `Disconnected { error: None }` is emitted
    /// (unless the link already reported its own), and both threads are joined within
    /// about one read timeout. This is what dropping a session does.
    pub fn abort(mut self) {
        self.shutdown(false);
    }

    fn send(&self, command: Command) -> Result<(), SessionClosed> {
        if !self.is_connected() {
            return Err(SessionClosed);
        }
        let commands = self.commands.as_ref().ok_or(SessionClosed)?;
        commands.send(command).map_err(|_| SessionClosed)
    }

    fn shutdown(&mut self, drain: bool) {
        if self.reader.is_none() && self.writer.is_none() {
            return;
        }
        if drain
            && self.is_connected()
            && let Some(commands) = &self.commands
            && commands.send(Command::Close).is_ok()
        {
            // Returns as soon as the writer exits (the channel disconnects), or at the
            // deadline if the queue is still draining.
            let _ = self.writer_done.recv_timeout(CLOSE_DRAIN_TIMEOUT);
        }
        self.shared.emit_disconnected(None);
        self.shared.stop_now();
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
        self.shutdown(false);
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

/// Reports a panicking session thread as a disconnect and stops the other thread, so
/// the UI is never left waiting and `is_connected()` turns false.
struct PanicGuard<'a> {
    shared: &'a Shared,
    thread: &'static str,
}

impl Drop for PanicGuard<'_> {
    fn drop(&mut self) {
        if thread::panicking() {
            self.shared
                .fail(TransportError::Io(io::Error::other(format!(
                    "{} thread panicked",
                    self.thread
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
    let _guard = PanicGuard {
        shared,
        thread: "reader",
    };
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
                // Also stops the writer, which drops its half of the transport.
                shared.fail(err);
                break;
            }
        }
    }
}

fn writer_loop(
    mut writer: Box<dyn TransportWriter>,
    commands: &Receiver<Command>,
    stop: &Receiver<()>,
    shared: &Shared,
) {
    let _guard = PanicGuard {
        shared,
        thread: "writer",
    };
    loop {
        let command = select! {
            recv(stop) -> _ => break,
            recv(commands) -> command => match command {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        // A stop wins over anything still queued, even if `select!` picked the queue.
        if shared.stop.load(Ordering::Acquire) {
            break;
        }
        let result = match command {
            Command::Close => break,
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

    /// Makes the mock writer panic.
    const PANIC_PAYLOAD: &[u8] = b"panic!";

    #[derive(Default)]
    struct Probe {
        ops: Mutex<Vec<Op>>,
        fail_writes: AtomicBool,
        write_delay: Mutex<Duration>,
        reads: AtomicU64,
        reader_dropped: AtomicBool,
        writer_dropped: AtomicBool,
        reader_thread: Mutex<Option<String>>,
        writer_thread: Mutex<Option<String>>,
    }

    impl Probe {
        fn writes(&self) -> usize {
            self.ops
                .lock()
                .iter()
                .filter(|op| matches!(op, Op::Write(_)))
                .count()
        }
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
            assert!(bytes != PANIC_PAYLOAD, "mock writer told to panic");
            let delay = *self.probe.write_delay.lock();
            thread::sleep(delay);
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

    const READ_TIMEOUT: Duration = Duration::from_millis(10);

    fn config() -> SessionConfig {
        let mut cfg = SessionConfig::new(PortId::new("test"), SerialConfig::default());
        cfg.read_timeout = READ_TIMEOUT;
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

    fn assert_last_is_orderly_disconnect(events: &Receiver<SessionEvent>) {
        let tail: Vec<_> = events.try_iter().collect();
        assert!(
            matches!(
                tail.last(),
                Some(SessionEvent::Disconnected { error: None })
            ),
            "{tail:?}"
        );
        assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
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
        assert_eq!(session.port(), &PortId::new("test"));
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
        // `serial_config` is live; `config().serial` is what the session opened with.
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
    fn reader_disconnect_releases_both_halves_at_once() {
        let (factory, steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        steps
            .send(Step::Fail(TransportError::Disconnected))
            .unwrap();
        // Both halves go while the Session itself is still alive.
        wait_for(|| {
            probe.reader_dropped.load(Ordering::SeqCst)
                && probe.writer_dropped.load(Ordering::SeqCst)
        });
        assert!(!session.is_connected());
        drop(session);
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
        assert_last_is_orderly_disconnect(&events);
        assert!(probe.reader_dropped.load(Ordering::SeqCst));
        assert!(probe.writer_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn close_sends_queued_writes_first() {
        let (factory, _steps, probe) = mock();
        *probe.write_delay.lock() = Duration::from_millis(20);
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        for i in 0..5u8 {
            session.write(vec![i]).unwrap();
        }
        // "Send reboot, then disconnect" must not lose the reboot.
        session.close();
        assert_eq!(probe.writes(), 5);
        assert_last_is_orderly_disconnect(&events);
    }

    #[test]
    fn close_gives_up_on_the_queue_after_the_drain_timeout() {
        let (factory, _steps, probe) = mock();
        let delay = Duration::from_millis(100);
        *probe.write_delay.lock() = delay;
        let session = Session::open(&factory, config()).unwrap();
        for i in 0..50u8 {
            session.write(vec![i]).unwrap();
        }
        let started = Instant::now();
        session.close();
        let took = started.elapsed();
        // The drain timeout, plus the write in progress, plus one read timeout, plus slack.
        let bound = CLOSE_DRAIN_TIMEOUT + delay + READ_TIMEOUT + Duration::from_millis(400);
        assert!(took < bound, "close took {took:?}");
        assert!(took >= CLOSE_DRAIN_TIMEOUT, "close gave up early: {took:?}");
        let written = probe.writes();
        assert!((2..50).contains(&written), "{written} writes went out");
    }

    #[test]
    fn abort_drops_queued_writes_and_is_prompt() {
        let (factory, _steps, probe) = mock();
        let delay = Duration::from_millis(50);
        *probe.write_delay.lock() = delay;
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        for i in 0..20u8 {
            session.write(vec![i]).unwrap();
        }
        let started = Instant::now();
        session.abort();
        let took = started.elapsed();
        assert!(
            took < delay + READ_TIMEOUT + Duration::from_millis(300),
            "abort took {took:?}"
        );
        assert!(probe.writes() <= 2, "{} writes went out", probe.writes());
        assert_last_is_orderly_disconnect(&events);
    }

    #[test]
    fn drop_stops_both_threads() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        drop(session);
        assert!(probe.reader_dropped.load(Ordering::SeqCst));
        assert!(probe.writer_dropped.load(Ordering::SeqCst));
        assert_last_is_orderly_disconnect(&events);
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
        let started = Instant::now();
        let before = probe.reads.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(300));
        let reads = probe.reads.load(Ordering::Relaxed) - before;
        let per_second = reads as f64 / started.elapsed().as_secs_f64();
        // A 10 ms timeout is about 100 reads/s; 64/s on Windows' 15.6 ms timer tick.
        assert!(
            (20.0..200.0).contains(&per_second),
            "{per_second:.0} reads/s"
        );
        drop(session);
    }

    #[test]
    fn reader_panic_is_reported_as_disconnect() {
        let (factory, steps, probe) = mock();
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
        wait_for(|| probe.writer_dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn writer_panic_is_reported_as_disconnect() {
        let (factory, _steps, probe) = mock();
        let session = Session::open(&factory, config()).unwrap();
        let events = session.events();
        let _connected = next(&events);
        session.write(PANIC_PAYLOAD.to_vec()).unwrap();
        assert!(matches!(
            next(&events),
            SessionEvent::Disconnected {
                error: Some(TransportError::Io(_))
            }
        ));
        assert!(!session.is_connected());
        assert!(session.write(b"x".to_vec()).is_err());
        // The reader stops too.
        wait_for(|| probe.reader_dropped.load(Ordering::SeqCst));
        session.close();
    }

    #[test]
    fn open_validates_config_and_propagates_factory_errors() {
        let (factory, _steps, _probe) = mock();
        for bad in [Duration::ZERO, Duration::from_micros(500)] {
            let mut cfg = config();
            cfg.read_timeout = bad;
            assert!(
                matches!(Session::open(&factory, cfg), Err(TransportError::Config(_))),
                "{bad:?} accepted"
            );
        }
        let mut empty = config();
        empty.read_buffer_size = 0;
        assert!(matches!(
            Session::open(&factory, empty),
            Err(TransportError::Config(_))
        ));
        let mut shortest = config();
        shortest.read_timeout = MIN_READ_TIMEOUT;
        let first = Session::open(&factory, shortest).unwrap();
        // The mock hands out one transport only.
        assert!(matches!(
            Session::open(&factory, config()),
            Err(TransportError::NotFound(_))
        ));
        drop(first);
    }
}
