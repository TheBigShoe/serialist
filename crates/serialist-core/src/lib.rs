//! Serialist core: everything that does not need a window.
//!
//! Dependency rule: this crate never depends on GPUI. It is built and tested headless,
//! and every abstraction here has an in-process implementation in `serialist-sim`
//! so the whole engine can be exercised without hardware.

pub mod config;
pub mod port;
pub mod transport;

pub use config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
pub use port::{PortEvent, PortId, PortInfo, PortKind, PortSource, UsbInfo};
pub use transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};
