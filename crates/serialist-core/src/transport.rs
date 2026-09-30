//! The transport contract. Every way bytes reach Serialist (a real serial port, a TCP
//! socket, a replayed capture, an in-process virtual link) implements these traits.
//!
//! The reader and writer are separate objects on purpose: the session runs them on two
//! threads, and a blocking read must never delay a write.

use std::time::Duration;

use crate::config::SerialConfig;
use crate::port::PortId;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The device is gone for good; the session should move to the disconnected state.
    #[error("device disconnected")]
    Disconnected,
    #[error("unsupported on this transport: {0}")]
    Unsupported(&'static str),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("port not found: {0}")]
    NotFound(PortId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControlLine {
    Dtr,
    Rts,
}

/// The receive half. Lives on the session's reader thread.
pub trait TransportReader: Send {
    /// Block until at least one byte is available or `timeout` elapses.
    ///
    /// Returns `Ok(0)` on timeout so the caller can poll its stop flag, and
    /// `Err(TransportError::Disconnected)` once the device is permanently gone.
    /// Must never busy-wait.
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError>;
}

/// The transmit and control half. Lives on the session's writer thread.
pub trait TransportWriter: Send {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError>;

    /// Change line settings on an open port (baud, framing, flow control).
    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError>;

    fn send_break(&mut self, duration: Duration) -> Result<(), TransportError> {
        let _ = duration;
        Err(TransportError::Unsupported("break"))
    }
}

/// An open link: both halves plus a label for the status line.
pub struct Transport {
    pub reader: Box<dyn TransportReader>,
    pub writer: Box<dyn TransportWriter>,
    /// For example `/dev/cu.usbserial-1420 @ 921600 8N1` or `virtual:echo`.
    pub description: String,
}

/// Opens transports for port ids. The real factory wraps `serialport`; the simulator's
/// factory hands out virtual links keyed by `virtual:<device>` ids.
pub trait TransportFactory: Send + Sync {
    fn open(&self, port: &PortId, config: &SerialConfig) -> Result<Transport, TransportError>;
}
