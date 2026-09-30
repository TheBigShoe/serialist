//! Hardware-free test doubles. Everything the app can do against a real serial port it
//! can do against these, which is how the test suite, the benches and the app's
//! developer mode run with no device attached.
//!
//! Contract (implemented in milestone 0):
//!
//! - [`SimDevice`]: what sits at the far end of a virtual link.
//! - [`VirtualLink::connect`]`(device, LinkConfig) -> (Transport, LinkHandle)`: an in-process
//!   pair that paces bytes to the configured baud, chunks like a real driver, adds latency
//!   and jitter, can drop or corrupt bytes, and can be unplugged and replugged.
//! - [`SimPortSource`]: a `PortSource` a test can plug and unplug devices into.
//! - [`SimTransportFactory`]: a `TransportFactory` that opens `virtual:<device>` ids.
//! - [`SimWorld`]: a source and a factory wired together, with the built-in devices plugged in.
//! - Built-in devices: [`EchoDevice`], [`AtDevice`], [`FirehoseDevice`] (configurable rate
//!   and content, verified end to end by [`FirehoseVerifier`]), [`RaceDevice`] (Airoha RACE
//!   binary frames mixed with text, for the codec plugins), [`MenuDevice`] (a U-Boot style
//!   boot menu redrawn in place with cursor addressing, for the VT screen; added to a
//!   world with [`SimWorld::add_menu`]).
//! - [`Clock`]: where a link gets its time. [`SystemClock`] is real time; a
//!   [`ManualClock`] moves only when a test moves it, so timing assertions are exact.
//!   [`VirtualLink::connect_with_clock`] and [`SimWorld::with_clock`] take one.

use std::time::{Duration, Instant};

use serialist_core::{ControlLine, SerialConfig};

mod clock;
mod crc;
mod devices;
mod factory;
mod firehose;
mod link;
mod menu;
mod race;
mod source;
mod world;

pub use clock::{Clock, ManualClock, SystemClock, Wakeup};
pub use crc::crc32;
pub use devices::{AtDevice, CaptureOutput, EchoDevice};
pub use factory::{DeviceConstructor, SimTransportFactory};
pub use firehose::{
    FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator, FirehoseReport,
    FirehoseVerifier, MIN_TICK, SeqGap,
};
pub use link::{HOST_BACKLOG_LIMIT, LinkHandle, PACED_LOOKAHEAD, PACKET_INTERVAL, VirtualLink};
pub use menu::MenuDevice;
pub use race::{
    RACE_COMMAND, RACE_INDICATION, RACE_LOG, RACE_MAX_LEN, RACE_RESPONSE, RACE_SYNC, RaceDevice,
    race_frame,
};
pub use source::{SimPortSource, virtual_port, virtual_port_id};
pub use world::SimWorld;

/// Bytes a device wants to send back, plus control over the link.
pub trait DeviceOutput {
    fn send(&mut self, bytes: &[u8]);
    /// The device hangs up: everything it already sent still goes out at wire pace,
    /// then the host sees `Disconnected`. Host writes fail from this call on. For an
    /// abrupt cable pull that loses bytes in flight, use `LinkHandle::unplug`.
    fn disconnect(&mut self);
}

/// A simulated device. Implementations must be deterministic for a given seed so tests
/// are reproducible.
///
/// Every callback runs on the link's device thread, never on a host thread.
pub trait SimDevice: Send + 'static {
    fn name(&self) -> &str;

    /// Called once when the link comes up.
    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        let _ = out;
    }

    /// Called with every chunk the host wrote, as the (paced) wire delivered it.
    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput);

    /// Called at the deadline previously returned, and once after `on_connect`.
    /// Return the next time this device wants to run, or `None` to sleep until data arrives;
    /// a sleeping device is ticked once more after its next `on_receive`.
    ///
    /// `now` and the deadline are on the link's [`Clock`], so a device on a
    /// [`ManualClock`] runs on manual time.
    ///
    /// A device that returns `Some(now)` runs as fast as the link accepts. Output goes
    /// into the device's transmit FIFO, and the link does not tick the device again until
    /// that FIFO has drained onto the wire, which is kept at most [`PACED_LOOKAHEAD`]
    /// ahead on a paced link and under [`HOST_BACKLOG_LIMIT`] unread bytes on any link.
    /// So an unlimited producer never runs away with memory, as long as each callback
    /// sends a bounded batch.
    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let _ = (now, out);
        None
    }

    /// Called when the host changes DTR or RTS. Devices that reset on a DTR/RTS toggle
    /// (ESP32 style auto-reset) override this.
    fn on_control(&mut self, line: ControlLine, asserted: bool, out: &mut dyn DeviceOutput) {
        let _ = (line, asserted, out);
    }
}

/// How a virtual link misbehaves. Defaults model a clean USB-serial adapter.
#[derive(Clone, Debug)]
pub struct LinkConfig {
    pub serial: SerialConfig,
    /// Pace bytes to `serial.bytes_per_second()`; off means as fast as possible.
    pub pace_to_baud: bool,
    /// Largest chunk a single `read` returns, like a driver's FIFO.
    pub max_chunk: usize,
    pub latency: Duration,
    pub jitter: Duration,
    /// Probability per byte, 0.0 to 1.0.
    pub drop_probability: f64,
    /// Probability per byte, 0.0 to 1.0.
    pub corrupt_probability: f64,
    pub seed: u64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            serial: SerialConfig::default(),
            pace_to_baud: true,
            max_chunk: 4096,
            latency: Duration::from_millis(1),
            jitter: Duration::ZERO,
            drop_probability: 0.0,
            corrupt_probability: 0.0,
            seed: 0,
        }
    }
}

impl LinkConfig {
    /// A link that moves bytes as fast as the host reads them, with no latency.
    /// What throughput tests and benches want.
    pub fn unpaced() -> Self {
        Self {
            pace_to_baud: false,
            latency: Duration::ZERO,
            ..Self::default()
        }
    }
}

/// Counters a test asserts on.
///
/// Byte counts are taken where bytes enter the wire, before faults. Bytes the host read
/// equal `device_to_host_bytes` minus the bytes dropped in that direction minus
/// `lost_on_unplug`. Drops and corruptions are counted across both directions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub host_to_device_bytes: u64,
    pub device_to_host_bytes: u64,
    pub dropped_bytes: u64,
    pub corrupted_bytes: u64,
    /// Calls to the host reader's `read`, including those that timed out. A reader that
    /// spins shows up here.
    pub host_read_calls: u64,
    /// Device-to-host bytes that were on the wire but not yet delivered when the link
    /// was unplugged. Output still in the device's FIFO is not counted anywhere.
    pub lost_on_unplug: u64,
}
