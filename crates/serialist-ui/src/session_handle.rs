//! The slice of `serialist_core::Session` the views use, as a trait.
//!
//! Views hold a `Box<dyn SessionHandle>` instead of a `Session` so UI tests can feed a
//! session view from a plain channel, with no transport and no threads behind it.

use std::sync::Arc;

use crossbeam_channel::Receiver;
use serialist_core::{
    ControlLine, PortId, SerialConfig, Session, SessionClosed, SessionConfig, SessionEvent,
    SessionStats, TransportError, TransportFactory,
};

pub trait SessionHandle: Send + 'static {
    /// The event stream. The view is its only consumer.
    fn events(&self) -> Receiver<SessionEvent>;

    /// Queue bytes for the writer thread; never blocks.
    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed>;

    fn stats(&self) -> SessionStats;

    fn is_connected(&self) -> bool;

    /// Stop the session's threads. Joins them, so callers run it off the main thread.
    fn close(self: Box<Self>);
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

/// Opens sessions for the workspace. Opening can block on the OS (a real port's open and
/// line setup), so the workspace always calls it from the background executor.
pub trait SessionOpener: Send + Sync + 'static {
    fn open(
        &self,
        port: &PortId,
        serial: &SerialConfig,
    ) -> Result<Box<dyn SessionHandle>, TransportError>;
}

/// The production opener: `Session::open` over a transport factory.
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
        Ok(Box::new(session))
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
}
