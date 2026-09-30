//! A simulated Airoha RACE device: the far end the RACE codec is tested against.
//!
//! RACE frames are `[0x05][type][len: u16 LE][cmd_id: u16 LE][payload]` with
//! `len = payload length + 2`; type `0x5A` is a command to the device, `0x5B` a
//! response, `0x5C` an indication and `0x5D` log data. This framer and encoder are the
//! simulator's own, written from that layout and deliberately independent of the codec
//! plugins, so a test that talks to this device through a codec checks one
//! implementation against another.

use std::time::{Duration, Instant};

use crate::{DeviceOutput, SimDevice};

/// First byte of every frame.
pub const RACE_SYNC: u8 = 0x05;
/// Frame type of a command, host to device.
pub const RACE_COMMAND: u8 = 0x5A;
/// Frame type of a response, device to host.
pub const RACE_RESPONSE: u8 = 0x5B;
/// Frame type of an unsolicited indication.
pub const RACE_INDICATION: u8 = 0x5C;
/// Frame type of log data.
pub const RACE_LOG: u8 = 0x5D;
/// Largest `len` field the device accepts (payload plus the two command-id bytes).
pub const RACE_MAX_LEN: usize = 4096;

/// One RACE frame's bytes.
///
/// # Panics
///
/// If `payload` is longer than [`RACE_MAX_LEN`] minus 2.
pub fn race_frame(ty: u8, cmd_id: u16, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() + 2 <= RACE_MAX_LEN, "RACE payload too long");
    let len = (payload.len() + 2) as u16;
    let mut frame = Vec::with_capacity(6 + payload.len());
    frame.push(RACE_SYNC);
    frame.push(ty);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&cmd_id.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// A device that speaks RACE.
///
/// | Host sends (type `0x5A`) | Device answers (type `0x5B`, same command id) |
/// |---|---|
/// | `0x0F15` query version and build time | status `0x00`, the version, a NUL, the build time |
/// | any other command id | status [`RaceDevice::STATUS_UNSUPPORTED`] alone |
///
/// Frames of other types from the host are ignored, and so are bytes that are not a
/// frame (the framer skips to the next `0x05` with a known type and a length of 2 to
/// 4096). Unprompted, the device sends a text banner line when the link comes up, a log
/// frame (type `0x5D`, command id [`RaceDevice::LOG_CMD_ID`], ASCII text payload)
/// every [`log interval`](RaceDevice::with_log_interval), and after every
/// [`text_every`](RaceDevice::with_text_every)th log a plain-text line, the way real
/// firmware mixes a text console with binary frames on one UART.
#[derive(Debug)]
pub struct RaceDevice {
    version: String,
    build_time: String,
    log_interval: Option<Duration>,
    text_every: u32,
    rx: Vec<u8>,
    next_log: Option<Instant>,
    logs: u64,
}

impl RaceDevice {
    /// The version query.
    pub const VERSION_CMD_ID: u16 = 0x0F15;
    /// Command id of the log frames the device sends.
    pub const LOG_CMD_ID: u16 = 0x0F40;
    /// Status byte of a successful response.
    pub const STATUS_OK: u8 = 0x00;
    /// Status byte of the response to a command the device does not know.
    pub const STATUS_UNSUPPORTED: u8 = 0x01;
    pub const DEFAULT_VERSION: &'static str = "SIM-RACE 1.4.2";
    pub const DEFAULT_BUILD_TIME: &'static str = "2026-09-30T12:00:00Z";
    pub const DEFAULT_LOG_INTERVAL: Duration = Duration::from_millis(200);
    pub const DEFAULT_TEXT_EVERY: u32 = 4;

    pub fn new() -> Self {
        Self {
            version: Self::DEFAULT_VERSION.into(),
            build_time: Self::DEFAULT_BUILD_TIME.into(),
            log_interval: Some(Self::DEFAULT_LOG_INTERVAL),
            text_every: Self::DEFAULT_TEXT_EVERY,
            rx: Vec::new(),
            next_log: None,
            logs: 0,
        }
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn with_build_time(mut self, build_time: impl Into<String>) -> Self {
        self.build_time = build_time.into();
        self
    }

    /// How often a log frame goes out; `None` for no logs (and no text lines).
    pub fn with_log_interval(mut self, interval: Option<Duration>) -> Self {
        self.log_interval = interval.filter(|d| !d.is_zero());
        self
    }

    /// A text line after every `n`th log frame; 0 for none.
    pub fn with_text_every(mut self, n: u32) -> Self {
        self.text_every = n;
        self
    }

    /// The payload of the answer to [`VERSION_CMD_ID`](Self::VERSION_CMD_ID).
    pub fn version_payload(&self) -> Vec<u8> {
        let mut payload = vec![Self::STATUS_OK];
        payload.extend_from_slice(self.version.as_bytes());
        payload.push(0);
        payload.extend_from_slice(self.build_time.as_bytes());
        payload
    }

    /// The text of log frame `n` (counting from 1).
    pub fn log_text(n: u64) -> String {
        // Varied but deterministic: a heap figure and a signal level that wander.
        let heap = 48_000 + (n * 7919) % 4096;
        let rssi = -40 - (n * 13 % 37) as i64;
        format!("[{n:06}] bt: link ok, rssi {rssi} dBm, heap {heap} B")
    }

    /// The plain-text line that follows log frame `n` when it is a `text_every`th one.
    pub fn text_line(n: u64) -> String {
        format!("sim: heartbeat {n}\r\n")
    }

    fn handle(&mut self, ty: u8, cmd_id: u16, out: &mut dyn DeviceOutput) {
        if ty != RACE_COMMAND {
            return;
        }
        let payload = match cmd_id {
            Self::VERSION_CMD_ID => self.version_payload(),
            _ => vec![Self::STATUS_UNSUPPORTED],
        };
        out.send(&race_frame(RACE_RESPONSE, cmd_id, &payload));
    }
}

impl Default for RaceDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice for RaceDevice {
    fn name(&self) -> &str {
        "race"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(format!("Airoha RACE simulator {}\r\n", self.version).as_bytes());
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        self.rx.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            let Some(sync) = self.rx[at..].iter().position(|&b| b == RACE_SYNC) else {
                at = self.rx.len();
                break;
            };
            at += sync;
            let rest = &self.rx[at..];
            if rest.len() < 4 {
                break;
            }
            let ty = rest[1];
            let len = usize::from(u16::from_le_bytes([rest[2], rest[3]]));
            if !(RACE_COMMAND..=RACE_LOG).contains(&ty) || !(2..=RACE_MAX_LEN).contains(&len) {
                // Not a frame start after all: resynchronise on the next byte.
                at += 1;
                continue;
            }
            if rest.len() < 4 + len {
                break;
            }
            let cmd_id = u16::from_le_bytes([rest[4], rest[5]]);
            at += 4 + len;
            self.handle(ty, cmd_id, out);
        }
        self.rx.drain(..at);
    }

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let interval = self.log_interval?;
        let due = *self.next_log.get_or_insert(now + interval);
        if now >= due {
            self.logs += 1;
            let n = self.logs;
            out.send(&race_frame(
                RACE_LOG,
                Self::LOG_CMD_ID,
                Self::log_text(n).as_bytes(),
            ));
            if self.text_every > 0 && n.is_multiple_of(u64::from(self.text_every)) {
                out.send(Self::text_line(n).as_bytes());
            }
            // Keep the rate, but do not burst to catch up after a stall.
            let next = due + interval;
            self.next_log = Some(if next <= now { now + interval } else { next });
        }
        self.next_log
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureOutput;

    fn quiet() -> RaceDevice {
        RaceDevice::new().with_log_interval(None)
    }

    #[test]
    fn answers_the_version_query() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        dev.on_receive(&[0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F], &mut out);
        let mut expected = vec![0x05, 0x5B];
        let payload = b"\x00SIM-RACE 1.4.2\x002026-09-30T12:00:00Z";
        expected.extend_from_slice(&((payload.len() + 2) as u16).to_le_bytes());
        expected.extend_from_slice(&[0x15, 0x0F]);
        expected.extend_from_slice(payload);
        assert_eq!(out.take(), expected);
        assert_eq!(dev.version_payload(), payload);
    }

    #[test]
    fn answers_unknown_commands_with_an_error_status() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        dev.on_receive(&race_frame(RACE_COMMAND, 0x1234, &[1, 2, 3]), &mut out);
        assert_eq!(
            out.take(),
            race_frame(RACE_RESPONSE, 0x1234, &[RaceDevice::STATUS_UNSUPPORTED])
        );
        // Other frame types from the host get no answer.
        dev.on_receive(&race_frame(RACE_RESPONSE, 0x0F15, &[]), &mut out);
        assert!(out.take().is_empty());
    }

    #[test]
    fn frames_split_across_chunks_and_junk_between_them() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        let mut stream = b"junk\x05\x99\x05\x5A\xFF\xFF".to_vec();
        stream.extend_from_slice(&race_frame(RACE_COMMAND, 0x0F15, &[]));
        stream.extend_from_slice(b"\r\n");
        stream.extend_from_slice(&race_frame(RACE_COMMAND, 0x0001, &[9; 300]));
        for byte in &stream {
            dev.on_receive(std::slice::from_ref(byte), &mut out);
        }
        let mut expected = race_frame(RACE_RESPONSE, 0x0F15, &dev.version_payload());
        expected.extend_from_slice(&race_frame(RACE_RESPONSE, 0x0001, &[0x01]));
        assert_eq!(out.take(), expected);
        assert!(dev.rx.is_empty());
    }

    #[test]
    fn logs_at_the_configured_rate_with_text_lines_between() {
        let interval = Duration::from_millis(100);
        let mut dev = RaceDevice::new()
            .with_log_interval(Some(interval))
            .with_text_every(2);
        let mut out = CaptureOutput::new();
        dev.on_connect(&mut out);
        assert_eq!(out.take(), b"Airoha RACE simulator SIM-RACE 1.4.2\r\n");
        let t0 = Instant::now();
        assert_eq!(dev.on_tick(t0, &mut out), Some(t0 + interval));
        assert!(out.take().is_empty());
        assert_eq!(
            dev.on_tick(t0 + interval, &mut out),
            Some(t0 + 2 * interval)
        );
        assert_eq!(
            out.take(),
            race_frame(
                RACE_LOG,
                RaceDevice::LOG_CMD_ID,
                RaceDevice::log_text(1).as_bytes()
            )
        );
        dev.on_tick(t0 + 2 * interval, &mut out);
        let mut expected = race_frame(
            RACE_LOG,
            RaceDevice::LOG_CMD_ID,
            RaceDevice::log_text(2).as_bytes(),
        );
        expected.extend_from_slice(b"sim: heartbeat 2\r\n");
        assert_eq!(out.take(), expected);
        // A long stall does not produce a burst.
        let late = t0 + 50 * interval;
        assert_eq!(dev.on_tick(late, &mut out), Some(late + interval));
        assert_eq!(out.take().iter().filter(|&&b| b == RACE_SYNC).count(), 1);
        assert_eq!(quiet().on_tick(t0, &mut out), None);
    }
}
