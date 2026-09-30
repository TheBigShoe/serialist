//! The slice of `serialist_core::Session` the views use, as a trait.
//!
//! Views hold a `Box<dyn SessionHandle>` instead of a `Session` so UI tests can feed a
//! session view from a plain channel, with no transport and no reader or writer thread
//! behind it.
//!
//! A script drives the same session from the script thread. It does that through a
//! [`SessionControl`], which the handle hands out ([`SessionHandle::control`]) and
//! which outlives nothing: once the view closes the session, every call on it fails
//! with [`SessionClosed`]. The production opener wraps its `Session` in a
//! [`SharedSession`] for that.

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use parking_lot::{Mutex, RwLock};
use serialist_core::{
    ControlLine, PortId, SerialConfig, Session, SessionClosed, SessionConfig, SessionEvent,
    SessionStats, TransportError, TransportFactory,
};

/// What a script may do to the session it runs on, from the script thread: queue
/// writes, set control lines and change line settings. Every call queues work and
/// returns at once, as [`Session`]'s do.
pub trait SessionControl: Send + Sync {
    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed>;

    fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed>;

    fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed>;

    /// The line settings in effect, while the session is open.
    fn serial_config(&self) -> Option<SerialConfig>;

    /// Hold the line in the break condition for `duration`. A session that cannot
    /// says it is closed.
    fn send_break(&self, duration: Duration) -> Result<(), SessionClosed> {
        let _ = duration;
        Err(SessionClosed)
    }
}

pub trait SessionHandle: Send + 'static {
    /// The event stream. The session view hands it to its ingest thread, which is its
    /// only consumer.
    fn events(&self) -> Receiver<SessionEvent>;

    /// Queue bytes for the writer thread; never blocks.
    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed>;

    fn stats(&self) -> SessionStats;

    fn is_connected(&self) -> bool;

    /// Stop the session's threads. Joins them, so callers run it off the main thread.
    fn close(self: Box<Self>);

    /// A handle for the script thread, or `None` if scripts cannot drive this session.
    /// Calls on it fail with [`SessionClosed`] once the session is closed.
    fn control(&self) -> Option<Arc<dyn SessionControl>> {
        None
    }
}

impl SessionHandle for Session {
    fn events(&self) -> Receiver<SessionEvent> {
        Session::events(self)
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        Session::write(self, bytes)
    }

    fn stats(&self) -> SessionStats {
        Session::stats(self)
    }

    fn is_connected(&self) -> bool {
        Session::is_connected(self)
    }

    fn close(self: Box<Self>) {
        Session::close(*self)
    }
}

/// A [`Session`] the view and the script thread share: the view owns the handle, and
/// [`SessionHandle::control`] gives scripts a [`SessionControl`] onto the same session.
/// Closing takes the session out from under both, so a script's later writes fail
/// cleanly instead of keeping the port open.
pub struct SharedSession {
    events: Receiver<SessionEvent>,
    cell: Arc<SessionCell>,
}

struct SessionCell {
    session: RwLock<Option<Session>>,
    /// The counters as they stood at close, for a view that reads them afterwards.
    last_stats: Mutex<SessionStats>,
}

impl SharedSession {
    pub fn new(session: Session) -> Self {
        Self {
            events: session.events(),
            cell: Arc::new(SessionCell {
                session: RwLock::new(Some(session)),
                last_stats: Mutex::new(SessionStats::default()),
            }),
        }
    }
}

impl SessionCell {
    fn with<T>(
        &self,
        f: impl FnOnce(&Session) -> Result<T, SessionClosed>,
    ) -> Result<T, SessionClosed> {
        match &*self.session.read() {
            Some(session) => f(session),
            None => Err(SessionClosed),
        }
    }
}

impl SessionControl for SessionCell {
    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        self.with(|session| session.write(bytes))
    }

    fn set_control(&self, line: ControlLine, asserted: bool) -> Result<(), SessionClosed> {
        self.with(|session| session.set_control(line, asserted))
    }

    fn reconfigure(&self, serial: SerialConfig) -> Result<(), SessionClosed> {
        self.with(|session| session.reconfigure(serial))
    }

    fn serial_config(&self) -> Option<SerialConfig> {
        self.with(|session| Ok(session.serial_config())).ok()
    }

    fn send_break(&self, duration: Duration) -> Result<(), SessionClosed> {
        self.with(|session| session.send_break(duration))
    }
}

impl SessionHandle for SharedSession {
    fn events(&self) -> Receiver<SessionEvent> {
        self.events.clone()
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        self.cell.write(bytes)
    }

    fn stats(&self) -> SessionStats {
        match &*self.cell.session.read() {
            Some(session) => session.stats(),
            None => *self.cell.last_stats.lock(),
        }
    }

    fn is_connected(&self) -> bool {
        self.cell
            .session
            .read()
            .as_ref()
            .is_some_and(Session::is_connected)
    }

    fn close(self: Box<Self>) {
        // Taken out under the lock, closed outside it: closing drains queued writes for
        // a while, and a script's write must not wait on that.
        let session = self.cell.session.write().take();
        if let Some(session) = session {
            *self.cell.last_stats.lock() = session.stats();
            session.close();
        }
    }

    fn control(&self) -> Option<Arc<dyn SessionControl>> {
        Some(self.cell.clone())
    }
}

/// Opens sessions for the workspace. Opening can block on the OS (a real port's open and
/// line setup), so the workspace always calls it from the background executor.
pub trait SessionOpener: Send + Sync + 'static {
    fn open(
        &self,
        port: &PortId,
        serial: &SerialConfig,
    ) -> Result<Box<dyn SessionHandle>, TransportError>;
}

/// The production opener: `Session::open` over a transport factory, shared with
/// scripts through a [`SharedSession`].
pub struct CoreSessionOpener {
    factory: Arc<dyn TransportFactory>,
}

impl CoreSessionOpener {
    pub fn new(factory: Arc<dyn TransportFactory>) -> Self {
        Self { factory }
    }
}

impl SessionOpener for CoreSessionOpener {
    fn open(
        &self,
        port: &PortId,
        serial: &SerialConfig,
    ) -> Result<Box<dyn SessionHandle>, TransportError> {
        let config = SessionConfig::new(port.clone(), serial.clone());
        let session = Session::open(self.factory.as_ref(), config)?;
        // The serial layer leaves DTR and RTS as the OS had them, and many devices
        // (Windows CDC ACM ones in particular) stay silent until DTR is high. Asserting
        // both is the default until per-device settings arrive in milestone 2.
        for line in [ControlLine::Dtr, ControlLine::Rts] {
            if session.set_control(line, true).is_err() {
                tracing::debug!(%port, ?line, "session closed before the control line was set");
            }
        }
        Ok(Box::new(SharedSession::new(session)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;
    use serialist_sim::{DeviceOutput, LinkConfig, SimDevice, SimWorld};

    use super::*;

    /// Records every control-line change the host makes.
    struct ControlProbe(Arc<Mutex<Vec<(ControlLine, bool)>>>);

    impl SimDevice for ControlProbe {
        fn name(&self) -> &str {
            "probe"
        }

        fn on_receive(&mut self, _: &[u8], _: &mut dyn DeviceOutput) {}

        fn on_control(&mut self, line: ControlLine, asserted: bool, _: &mut dyn DeviceOutput) {
            self.0.lock().push((line, asserted));
        }
    }

    #[test]
    fn opening_asserts_dtr_and_rts() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let world = SimWorld::empty();
        let probe_seen = seen.clone();
        let id = world.add_virtual("probe", "Probe", LinkConfig::unpaced(), move || {
            Box::new(ControlProbe(probe_seen.clone()))
        });
        let opener = CoreSessionOpener::new(world.transport_factory());
        let session = opener.open(&id, &SerialConfig::default()).expect("open");

        // The writer thread applies the changes; wait for the device to see both.
        let deadline = Instant::now() + Duration::from_secs(2);
        while seen.lock().len() < 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            *seen.lock(),
            [(ControlLine::Dtr, true), (ControlLine::Rts, true)]
        );
        session.close();
    }

    #[test]
    fn a_script_control_drives_the_session_and_fails_once_it_closes() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let world = SimWorld::empty();
        let probe_seen = seen.clone();
        let id = world.add_virtual("probe", "Probe", LinkConfig::unpaced(), move || {
            Box::new(ControlProbe(probe_seen.clone()))
        });
        let opener = CoreSessionOpener::new(world.transport_factory());
        let session = opener.open(&id, &SerialConfig::default()).expect("open");
        let control = session
            .control()
            .expect("a shared session hands out control");
        control.set_control(ControlLine::Dtr, false).unwrap();
        control.write(b"hi".to_vec()).unwrap();
        assert_eq!(control.serial_config(), Some(SerialConfig::default()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while seen.lock().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(seen.lock()[2], (ControlLine::Dtr, false));
        session.close();
        assert!(control.write(b"late".to_vec()).is_err());
        assert!(control.set_control(ControlLine::Rts, false).is_err());
        assert_eq!(control.serial_config(), None);
    }
}
