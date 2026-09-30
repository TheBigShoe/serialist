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
//!   and content, verified end to end by [`FirehoseVerifier`]).

use std::time::{Duration, Instant};

use serialist_core::{ControlLine, SerialConfig};

mod crc;
mod devices;
mod factory;
mod firehose;
mod link;
mod source;
mod world;

pub use crc::crc32;
pub use devices::{AtDevice, CaptureOutput, EchoDevice};
pub use factory::{DeviceConstructor, SimTransportFactory};
pub use firehose::{
    FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator, FirehoseReport,
    FirehoseVerifier, SeqGap,
};
pub use link::{LinkHandle, PACKET_INTERVAL, VirtualLink};
pub use source::{SimPortSource, virtual_port, virtual_port_id};
pub use world::SimWorld;

/// Bytes a device wants to send back, plus control over the link.
pub trait DeviceOutput {
    fn send(&mut self, bytes: &[u8]);
    /// Simulate the device going away (USB unplug, power loss).
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
    /// A device that returns `Some(now)` runs as fast as the link accepts: the link stops
    /// ticking it while its outgoing queue is full (about 20 ms of data on a paced link,
    /// 1 MiB on an unpaced one), so an unlimited producer never runs away with memory.
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
/// Byte counts are taken where bytes enter the link, before faults: bytes the host
/// read equal `device_to_host_bytes` minus the bytes dropped in that direction.
/// Drops and corruptions are counted across both directions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub host_to_device_bytes: u64,
    pub device_to_host_bytes: u64,
    pub dropped_bytes: u64,
    pub corrupted_bytes: u64,
    /// Calls to the host reader's `read`, including those that timed out. A reader that
    /// spins shows up here.
    pub host_read_calls: u64,
}
