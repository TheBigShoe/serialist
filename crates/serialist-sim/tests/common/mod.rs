//! Helpers shared by the integration tests. Each test binary uses a different subset.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use serialist_core::{ControlLine, SerialConfig, SessionEvent, TransportError, TransportReader};
use serialist_sim::{DeviceOutput, SimDevice};

/// Sends a fixed byte string when the link comes up, then optionally unplugs itself.
pub struct BurstDevice {
    data: Vec<u8>,
    disconnect: bool,
}

impl BurstDevice {
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            data,
            disconnect: false,
        }
    }

    pub fn then_disconnect(mut self) -> Self {
        self.disconnect = true;
        self
    }
}

impl SimDevice for BurstDevice {
    fn name(&self) -> &str {
        "burst"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(&self.data);
        if self.disconnect {
            out.disconnect();
        }
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {}
}

/// What a [`RecorderDevice`] saw, shared with the test.
#[derive(Default)]
pub struct Recording {
    pub chunks: Vec<(Instant, Vec<u8>)>,
    pub controls: Vec<(ControlLine, bool)>,
}

/// Records every chunk and control change it receives, with arrival times.
pub struct RecorderDevice(pub Arc<Mutex<Recording>>);

impl SimDevice for RecorderDevice {
    fn name(&self) -> &str {
        "recorder"
    }

    fn on_receive(&mut self, bytes: &[u8], _out: &mut dyn DeviceOutput) {
        self.0.lock().chunks.push((Instant::now(), bytes.to_vec()));
    }

    fn on_control(&mut self, line: ControlLine, asserted: bool, _out: &mut dyn DeviceOutput) {
        self.0.lock().controls.push((line, asserted));
    }
}

/// Panics on the first byte it receives.
pub struct PanicDevice;

impl SimDevice for PanicDevice {
    fn name(&self) -> &str {
        "panic"
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {
        panic!("simulated device failure");
    }
}

pub fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

/// Read until `n` bytes arrived, failing the test after `limit`. Also checks every read
/// respects `max_chunk`.
pub fn read_exactly(
    reader: &mut dyn TransportReader,
    n: usize,
    max_chunk: usize,
    limit: Duration,
) -> Vec<u8> {
    let deadline = Instant::now() + limit;
    let mut out = Vec::with_capacity(n);
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < n {
        assert!(
            Instant::now() < deadline,
            "only {} of {n} bytes after {limit:?}",
            out.len()
        );
        let got = reader
            .read(&mut buf, Duration::from_millis(20))
            .expect("link went down early");
        assert!(
            got <= max_chunk,
            "read returned {got} > max_chunk {max_chunk}"
        );
        out.extend_from_slice(&buf[..got]);
    }
    out
}

/// Read until the link reports `Disconnected`, failing the test after `limit`.
pub fn read_to_disconnect(reader: &mut dyn TransportReader, limit: Duration) -> Vec<u8> {
    let deadline = Instant::now() + limit;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        assert!(Instant::now() < deadline, "no disconnect after {limit:?}");
        match reader.read(&mut buf, Duration::from_millis(20)) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(TransportError::Disconnected) => return out,
            Err(other) => panic!("unexpected read error: {other}"),
        }
    }
}

pub fn next_event(events: &Receiver<SessionEvent>) -> SessionEvent {
    events
        .recv_timeout(Duration::from_secs(5))
        .expect("no session event within 5 s")
}

/// Collect `Data` until `done(&received)` holds, failing on any other event.
pub fn collect_until(
    events: &Receiver<SessionEvent>,
    mut done: impl FnMut(&[u8]) -> bool,
) -> Vec<u8> {
    let mut received = Vec::new();
    while !done(&received) {
        match next_event(events) {
            SessionEvent::Data { bytes, .. } => received.extend_from_slice(&bytes),
            other => panic!("unexpected event {other:?} after {received:?}"),
        }
    }
    received
}
