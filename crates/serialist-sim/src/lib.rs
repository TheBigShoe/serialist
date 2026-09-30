//! Hardware-free test doubles. Everything the app can do against a real serial port it
//! can do against these, which is how the test suite, the benches and the app's
//! developer mode run with no device attached.
//!
//! Contract (implementations are added in milestone 0):
//!
//! - [`SimDevice`]: what sits at the far end of a virtual link.
//! - `VirtualLink::connect(device, LinkConfig) -> (Transport, LinkHandle)`: an in-process
//!   pair that paces bytes to the configured baud, chunks like a real driver, adds latency
//!   and jitter, can drop or corrupt bytes, and can be unplugged and replugged.
//! - `SimPortSource`: a `PortSource` a test can plug and unplug devices into.
//! - `SimTransportFactory`: a `TransportFactory` that opens `virtual:<device>` ids.
//! - Built-in devices: echo, AT command set, firehose (configurable rate and content).

use std::time::{Duration, Instant};

use serialist_core::SerialConfig;

/// Bytes a device wants to send back, plus control over the link.
pub trait DeviceOutput {
    fn send(&mut self, bytes: &[u8]);
    /// Simulate the device going away (USB unplug, power loss).
    fn disconnect(&mut self);
}

/// A simulated device. Implementations must be deterministic for a given seed so tests
/// are reproducible.
pub trait SimDevice: Send + 'static {
    fn name(&self) -> &str;

    /// Called once when the link comes up.
    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        let _ = out;
    }

    /// Called with every chunk the host wrote.
    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput);

    /// Called at the deadline previously returned, and once after `on_connect`.
    /// Return the next time this device wants to run, or `None` to sleep until data arrives.
    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let _ = (now, out);
        None
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

/// Counters a test asserts on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub host_to_device_bytes: u64,
    pub device_to_host_bytes: u64,
    pub dropped_bytes: u64,
    pub corrupted_bytes: u64,
}
