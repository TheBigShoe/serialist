//! The slice of `serialist_core::Session` the views use, as a trait.
//!
//! Views hold a `Box<dyn SessionHandle>` instead of a `Session` so UI tests can feed a
//! session view from a plain channel, with no transport and no threads behind it.

use std::sync::Arc;

use crossbeam_channel::Receiver;
use serialist_core::{
    PortId, SerialConfig, Session, SessionClosed, SessionConfig, SessionEvent, SessionStats,
    TransportError, TransportFactory,
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
        Ok(Box::new(session))
    }
}
