//! Port discovery and hotplug. The real source wraps `serialport` enumeration plus
//! `nusb` hotplug events; the simulator's source is a list a test can plug and unplug.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable identity for a port. Real ports use the OS path (`/dev/cu.usbserial-1420`,
/// `COM3`); virtual ones use `virtual:<device>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PortId(pub String);

impl PortId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PortId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbInfo {
    pub vid: u16,
    pub pid: u16,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PortKind {
    Usb(UsbInfo),
    Bluetooth,
    Pci,
    Virtual,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PortInfo {
    pub id: PortId,
    pub kind: PortKind,
    /// What the Devices panel shows: product name when known, else the path.
    pub display_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortEvent {
    Added(PortInfo),
    Removed(PortId),
    /// The full current list; always the first message on a new subscription.
    Snapshot(Vec<PortInfo>),
}

/// A live view of available ports.
pub trait PortSource: Send + Sync {
    /// The current list, deduplicated. On macOS only the `cu.*` node of each device
    /// is listed; the `tty.*` twin is dropped.
    fn snapshot(&self) -> Vec<PortInfo>;

    /// Hotplug events. The receiver first gets a `Snapshot`, then `Added`/`Removed`
    /// as devices come and go. A new device must be reported within 250 ms.
    fn subscribe(&self) -> crossbeam_channel::Receiver<PortEvent>;
}
