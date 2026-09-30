//! A session owns one open transport and the two threads that service it.
//!
//! Contract for milestone 0. The reader thread blocks in `TransportReader::read` with
//! `SessionConfig::read_timeout`, hands every chunk to the event channel unchanged, and
//! polls a stop flag between reads. The writer thread drains a queue of outgoing writes.
//! Neither thread ever runs UI code; the UI drains `events()` from its own executor.
//!
//! Bodies marked `unimplemented!` are filled in by the session implementation task;
//! other crates code against these signatures.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::config::SerialConfig;
use crate::port::PortId;
use crate::transport::{ControlLine, TransportError, TransportFactory};

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

/// One open port with its reader and writer threads. Dropping a session stops both threads.
pub struct Session {
    _private: (),
}

impl Session {
    /// Open the port through `factory` and start the reader and writer threads.
    /// The first event on `events()` is `Connected`.
    pub fn open(
        _factory: &dyn TransportFactory,
        _config: SessionConfig,
    ) -> Result<Self, TransportError> {
        unimplemented!("milestone 0: Session::open")
    }

    /// A clone of the event receiver. Events are delivered in order; `Data` chunks are never dropped.
    pub fn events(&self) -> Receiver<SessionEvent> {
        unimplemented!("milestone 0: Session::events")
    }

    /// Queue bytes for the writer thread. Returns immediately.
    pub fn write(&self, _bytes: Vec<u8>) -> Result<(), SessionClosed> {
        unimplemented!("milestone 0: Session::write")
    }

    pub fn set_control(&self, _line: ControlLine, _asserted: bool) -> Result<(), SessionClosed> {
        unimplemented!("milestone 0: Session::set_control")
    }

    /// Apply new line settings without reopening the port.
    pub fn reconfigure(&self, _serial: SerialConfig) -> Result<(), SessionClosed> {
        unimplemented!("milestone 0: Session::reconfigure")
    }

    pub fn stats(&self) -> SessionStats {
        unimplemented!("milestone 0: Session::stats")
    }

    pub fn config(&self) -> &SessionConfig {
        unimplemented!("milestone 0: Session::config")
    }

    pub fn is_connected(&self) -> bool {
        unimplemented!("milestone 0: Session::is_connected")
    }

    /// Stop both threads and join them. Emits `Disconnected { error: None }` first.
    pub fn close(self) {
        unimplemented!("milestone 0: Session::close")
    }
}
