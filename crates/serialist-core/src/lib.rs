//! Serialist core: everything that does not need a window.
//!
//! Dependency rule: this crate never depends on GPUI. It is built and tested headless,
//! and every abstraction here has an in-process implementation in `serialist-sim`
//! so the whole engine can be exercised without hardware.

pub mod ansi;
pub mod composite;
pub mod config;
pub mod discovery;
pub mod port;
pub mod serial;
pub mod session;
pub mod store;
pub mod text;
pub mod transport;

pub use ansi::{AnsiParser, OwnedLine, ParsedLine};
pub use composite::{MergedPortSource, RoutingTransportFactory, VIRTUAL_SCHEME};
pub use config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
pub use discovery::RealPortSource;
pub use port::{PortEvent, PortId, PortInfo, PortKind, PortSource, UsbInfo};
pub use serial::SerialportFactory;
pub use session::{Session, SessionClosed, SessionConfig, SessionEvent, SessionStats};
pub use store::{
    AppendReport, HexStyles, HexView, Snapshot, Store, StoreConfig, StoreReader, StoreStats,
    TextOptions, Timestamps,
};
pub use text::{
    Color, Direction, Epoch, LineId, LineSource, SearchMatch, Searcher, Style, StyleFlags,
    StyleRun, StyledLine,
};
pub use transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};
