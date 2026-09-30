//! The real transport: [`SerialportFactory`] opens OS serial ports through the
//! `serialport` crate and hands the session a reader half and a writer half.
//!
//! Platform notes, all verified against `serialport` 4.10.1:
//!
//! * **Custom baud rates** are passed straight through. The crate applies `IOSSIOSPEED`
//!   on macOS (after every `tcsetattr`, so the OS-reported rate is never trustworthy),
//!   `termios2`/`BOTHER` on Linux and a `DCB` rate on Windows.
//! * **Exclusive access** is the crate default on Unix (`TIOCEXCL` plus a non-blocking
//!   `flock`) and is requested explicitly here. Windows ports are always opened with a
//!   share mode of zero, so they are exclusive by construction.
//! * **Mark and space parity** do not exist in the crate, so they are rejected with
//!   [`TransportError::Unsupported`] before any device is touched.
//! * **Timeouts** are per object on Unix (each `try_clone` has its own) but device-wide
//!   on Windows (`SetCommTimeouts`). The reader owns the timeout on both; the writer
//!   never touches it on Windows and tolerates short write timeouts everywhere.

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use serialport::SerialPort;

use crate::config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
use crate::port::PortId;
use crate::transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};

/// Timeout the port is opened with, before the first `read` picks its own.
const INITIAL_READ_TIMEOUT: Duration = Duration::from_millis(20);

/// Floor for the caller's read timeout. A zero timeout makes `poll()` return at once
/// and spins a core, and macOS truncates sub-millisecond timeouts to zero.
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);

/// How long one blocked write attempt waits for the driver to accept bytes (Unix only;
/// on Windows the device-wide read timeout applies instead).
#[cfg(unix)]
const WRITE_POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// A write that makes no progress at all for this long fails with `TimedOut` instead of
/// blocking the writer thread forever (for example CTS held low by the far end).
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(5);

/// Opens real serial ports. Stateless, so one instance can be shared freely.
#[derive(Clone, Copy, Debug, Default)]
pub struct SerialportFactory;

impl SerialportFactory {
    pub fn new() -> Self {
        Self
    }
}

impl TransportFactory for SerialportFactory {
    fn open(&self, port: &PortId, config: &SerialConfig) -> Result<Transport, TransportError> {
        let parity = validate(config)?;

        #[allow(unused_mut)] // `exclusive` only exists on Unix.
        let mut builder = serialport::new(port.as_str(), config.baud)
            .data_bits(map_data_bits(config.data_bits))
            .parity(parity)
            .stop_bits(map_stop_bits(config.stop_bits))
            .flow_control(map_flow_control(config.flow_control))
            .timeout(INITIAL_READ_TIMEOUT);
        #[cfg(unix)]
        {
            builder = builder.exclusive(true);
        }

        let reader_port = builder.open().map_err(|e| open_error(port, e))?;
        let writer_port = reader_port.try_clone().map_err(|e| open_error(port, e))?;

        let reader = SerialReader::new(reader_port);
        let writer = SerialWriter::new(writer_port, config.clone())?;
        tracing::debug!(port = %port, config = %config.summary(), "opened serial port");

        Ok(Transport {
            reader: Box::new(reader),
            writer: Box::new(writer),
            description: describe(port, config),
        })
    }
}

/// `/dev/cu.usbserial-1420 @ 921600 8N1`.
fn describe(port: &PortId, config: &SerialConfig) -> String {
    format!("{port} @ {}", config.summary())
}

/// Checks everything that can be rejected without touching a device and returns the
/// crate's parity value.
fn validate(config: &SerialConfig) -> Result<serialport::Parity, TransportError> {
    if config.baud == 0 {
        // Baud 0 means "hang up" to termios, and macOS uses it to skip the speed ioctl.
        return Err(TransportError::Config(
            "baud rate must be greater than zero".to_owned(),
        ));
    }
    map_parity(config.parity)
}

fn map_data_bits(bits: DataBits) -> serialport::DataBits {
    match bits {
        DataBits::Five => serialport::DataBits::Five,
        DataBits::Six => serialport::DataBits::Six,
        DataBits::Seven => serialport::DataBits::Seven,
        DataBits::Eight => serialport::DataBits::Eight,
    }
}

fn map_parity(parity: Parity) -> Result<serialport::Parity, TransportError> {
    match parity {
        Parity::None => Ok(serialport::Parity::None),
        Parity::Odd => Ok(serialport::Parity::Odd),
        Parity::Even => Ok(serialport::Parity::Even),
        Parity::Mark | Parity::Space => Err(TransportError::Unsupported(
            "mark and space parity (not available in the serialport crate)",
        )),
    }
}

fn map_stop_bits(bits: StopBits) -> serialport::StopBits {
    match bits {
        StopBits::One => serialport::StopBits::One,
        StopBits::Two => serialport::StopBits::Two,
    }
}

fn map_flow_control(flow: FlowControl) -> serialport::FlowControl {
    match flow {
        FlowControl::None => serialport::FlowControl::None,
        FlowControl::Hardware => serialport::FlowControl::Hardware,
        FlowControl::Software => serialport::FlowControl::Software,
    }
}

// ---------------------------------------------------------------------------------
// Error classification
// ---------------------------------------------------------------------------------

/// Turns an error from `serialport::SerialPortBuilder::open` into a transport error.
fn open_error(port: &PortId, err: serialport::Error) -> TransportError {
    use serialport::ErrorKind as K;

    let context = |detail: &str| format!("{port}: {detail}");
    match err.kind() {
        // Unix: EBUSY or a lost flock race, so the port is held by someone else.
        // Windows: FILE_NOT_FOUND, PATH_NOT_FOUND and ACCESS_DENIED all land here, so
        // ask the enumerator whether the port exists at all.
        K::NoDevice => {
            if cfg!(windows) && !is_listed(port) {
                TransportError::NotFound(port.clone())
            } else {
                TransportError::Io(io::Error::new(
                    io::ErrorKind::ResourceBusy,
                    context(&format!("port is in use by another process ({err})")),
                ))
            }
        }
        K::InvalidInput => TransportError::Config(context(&err.description)),
        K::Io(io::ErrorKind::NotFound) => TransportError::NotFound(port.clone()),
        K::Io(kind) => TransportError::Io(io::Error::new(kind, context(&err.description))),
        // ENXIO and ENODEV arrive as `Unknown` with only their description intact: the
        // node exists but nothing is behind it, which is as good as not found.
        K::Unknown if description_means_device_gone(&err.description) => {
            TransportError::NotFound(port.clone())
        }
        K::Unknown => TransportError::Io(io::Error::other(context(&err.description))),
    }
}

/// True when the OS still lists `port`. Only used to disambiguate Windows open errors.
fn is_listed(port: &PortId) -> bool {
    serialport::available_ports()
        .map(|ports| {
            ports
                .iter()
                .any(|p| p.port_name.eq_ignore_ascii_case(port.as_str()))
        })
        .unwrap_or(true)
}

/// Maps an error from an already-open port. Anything that means the device went away
/// becomes [`TransportError::Disconnected`].
fn map_port_error(err: serialport::Error) -> TransportError {
    use serialport::ErrorKind as K;

    match err.kind() {
        // After open, "no device" can only mean the device vanished (serialport's own
        // docs for the control-line and ioctl calls say the same).
        K::NoDevice => TransportError::Disconnected,
        K::InvalidInput => TransportError::Config(err.description),
        K::Io(kind) => map_io_error(io::Error::new(kind, err.description)),
        K::Unknown => map_io_error(io::Error::other(err.description)),
    }
}

fn map_io_error(err: io::Error) -> TransportError {
    if is_disconnect(&err) {
        TransportError::Disconnected
    } else {
        TransportError::Io(err)
    }
}

/// Does this I/O error mean the device is permanently gone?
///
/// `serialport` flattens Unix errno values into `io::Error::new(kind, errno.desc())`, so
/// after a read or write the raw OS error is usually lost. Three signals are checked:
/// the error kind, the raw OS code (kept on Windows and by std), and, as a last resort
/// on Unix, the errno description for EIO, ENXIO and ENODEV.
fn is_disconnect(err: &io::Error) -> bool {
    use io::ErrorKind as K;

    // NotFound is only produced after open when the device node vanished.
    if matches!(
        err.kind(),
        K::BrokenPipe | K::NotConnected | K::ConnectionAborted | K::ConnectionReset | K::NotFound
    ) {
        return true;
    }
    if let Some(code) = err.raw_os_error() {
        return os_code_means_device_gone(code);
    }
    description_means_device_gone(&err.to_string())
}

/// Raw OS error codes that mean the device went away.
#[cfg(unix)]
fn os_code_means_device_gone(code: i32) -> bool {
    // EIO, ENXIO and ENODEV have the same values on Linux, macOS and the BSDs.
    matches!(code, 5 | 6 | 19)
}

#[cfg(windows)]
fn os_code_means_device_gone(code: i32) -> bool {
    matches!(
        code,
        2      // ERROR_FILE_NOT_FOUND
        | 5    // ERROR_ACCESS_DENIED, what a surprise-removed USB serial port reports
        | 6    // ERROR_INVALID_HANDLE
        | 21   // ERROR_NOT_READY
        | 22   // ERROR_BAD_COMMAND
        | 31   // ERROR_GEN_FAILURE
        | 55   // ERROR_DEV_NOT_EXIST
        | 433  // ERROR_NO_SUCH_DEVICE
        | 995  // ERROR_OPERATION_ABORTED, pending I/O cancelled by removal
        | 1167 // ERROR_DEVICE_NOT_CONNECTED
    )
}

#[cfg(not(any(unix, windows)))]
fn os_code_means_device_gone(_code: i32) -> bool {
    false
}

/// The descriptions `nix` gives EIO ("I/O error"), ENXIO ("No such device or address")
/// and ENODEV ("No such device"), plus the libc spellings of the same errors.
#[cfg(unix)]
fn description_means_device_gone(desc: &str) -> bool {
    let desc = desc.to_ascii_lowercase();
    desc.contains("i/o error")
        || desc.contains("input/output error")
        || desc.contains("no such device")
        || desc.contains("device not configured")
}

/// Windows errors keep their raw code, and their text is localised, so text is useless.
#[cfg(not(unix))]
fn description_means_device_gone(_desc: &str) -> bool {
    false
}

// ---------------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------------

struct SerialReader {
    port: Box<dyn SerialPort>,
    /// The timeout last handed to the port, so `set_timeout` only runs on change.
    timeout: Duration,
}

impl SerialReader {
    fn new(port: Box<dyn SerialPort>) -> Self {
        let timeout = port.timeout();
        Self { port, timeout }
    }
}

impl TransportReader for SerialReader {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let timeout = timeout.max(MIN_READ_TIMEOUT);
        if timeout != self.timeout {
            self.port.set_timeout(timeout).map_err(map_port_error)?;
            self.timeout = timeout;
        }

        match self.port.read(buf) {
            // `serialport` turns an empty read into `TimedOut` on Windows, and on Unix a
            // readable tty that yields zero bytes is a hangup, never a timeout.
            Ok(0) => Err(TransportError::Disconnected),
            Ok(n) => Ok(n),
            Err(err) => match err.kind() {
                // A signal or a spurious wakeup is just an early "nothing yet".
                io::ErrorKind::TimedOut
                | io::ErrorKind::WouldBlock
                | io::ErrorKind::Interrupted => Ok(0),
                _ => Err(map_io_error(err)),
            },
        }
    }
}

// ---------------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------------

struct SerialWriter {
    port: Box<dyn SerialPort>,
    /// What the line is currently set to, so `reconfigure` only touches what changed.
    /// This matters on macOS, where every termios change briefly re-applies 9600 baud.
    applied: SerialConfig,
}

impl SerialWriter {
    fn new(mut port: Box<dyn SerialPort>, applied: SerialConfig) -> Result<Self, TransportError> {
        // Per-object timeout on Unix: the writer wants its own, not the reader's. On
        // Windows the timeout is shared with the reader, so it is left alone.
        #[cfg(unix)]
        port.set_timeout(WRITE_POLL_TIMEOUT)
            .map_err(map_port_error)?;
        #[cfg(not(unix))]
        let _ = &mut port;
        Ok(Self { port, applied })
    }

    /// Gives up on a write that has made no progress for [`WRITE_STALL_LIMIT`].
    fn check_stall(last_progress: Instant) -> Result<(), TransportError> {
        if last_progress.elapsed() >= WRITE_STALL_LIMIT {
            Err(TransportError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "write stalled: the device is not accepting data (flow control?)",
            )))
        } else {
            Ok(())
        }
    }
}

impl TransportWriter for SerialWriter {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let mut rest = bytes;
        let mut last_progress = Instant::now();
        while !rest.is_empty() {
            match self.port.write(rest) {
                Ok(0) => {
                    // Windows reports a write timeout as a zero-byte success. The call
                    // already blocked for the timeout, this only stops a 1 ms timeout
                    // from turning into a tight loop.
                    Self::check_stall(last_progress)?;
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(n) => {
                    rest = &rest[n..];
                    last_progress = Instant::now();
                }
                Err(err) => match err.kind() {
                    io::ErrorKind::Interrupted => {}
                    // The poll already waited out the timeout; try again until stalled.
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => {
                        Self::check_stall(last_progress)?;
                    }
                    _ => return Err(map_io_error(err)),
                },
            }
        }
        self.port.flush().map_err(map_io_error)
    }

    fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError> {
        match line {
            ControlLine::Dtr => self.port.write_data_terminal_ready(asserted),
            ControlLine::Rts => self.port.write_request_to_send(asserted),
        }
        .map_err(map_port_error)
    }

    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
        let parity = validate(config)?;

        // Baud first: on macOS each later setter re-applies the port's cached rate, so
        // updating the cache first means they all re-apply the new one.
        if config.baud != self.applied.baud {
            self.port
                .set_baud_rate(config.baud)
                .map_err(map_port_error)?;
            self.applied.baud = config.baud;
        }
        if config.data_bits != self.applied.data_bits {
            self.port
                .set_data_bits(map_data_bits(config.data_bits))
                .map_err(map_port_error)?;
            self.applied.data_bits = config.data_bits;
        }
        if config.parity != self.applied.parity {
            self.port.set_parity(parity).map_err(map_port_error)?;
            self.applied.parity = config.parity;
        }
        if config.stop_bits != self.applied.stop_bits {
            self.port
                .set_stop_bits(map_stop_bits(config.stop_bits))
                .map_err(map_port_error)?;
            self.applied.stop_bits = config.stop_bits;
        }
        if config.flow_control != self.applied.flow_control {
            self.port
                .set_flow_control(map_flow_control(config.flow_control))
                .map_err(map_port_error)?;
            self.applied.flow_control = config.flow_control;
        }
        Ok(())
    }

    fn send_break(&mut self, duration: Duration) -> Result<(), TransportError> {
        self.port.set_break().map_err(map_port_error)?;
        thread::sleep(duration);
        // Always release the line, even if the sleep was long enough for the device to go.
        self.port.clear_break().map_err(map_port_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(f: impl FnOnce(&mut SerialConfig)) -> SerialConfig {
        let mut config = SerialConfig::default();
        f(&mut config);
        config
    }

    #[test]
    fn description_uses_path_and_summary() {
        let config = cfg_with(|c| {
            c.baud = 921_600;
            c.parity = Parity::Even;
            c.stop_bits = StopBits::Two;
        });
        assert_eq!(
            describe(&PortId::new("/dev/cu.usbserial-1420"), &config),
            "/dev/cu.usbserial-1420 @ 921600 8E2"
        );
        assert_eq!(
            describe(&PortId::new("COM3"), &SerialConfig::default()),
            "COM3 @ 115200 8N1"
        );
    }

    #[test]
    fn framing_maps_one_to_one() {
        assert_eq!(map_data_bits(DataBits::Five), serialport::DataBits::Five);
        assert_eq!(map_data_bits(DataBits::Six), serialport::DataBits::Six);
        assert_eq!(map_data_bits(DataBits::Seven), serialport::DataBits::Seven);
        assert_eq!(map_data_bits(DataBits::Eight), serialport::DataBits::Eight);
        assert_eq!(map_stop_bits(StopBits::One), serialport::StopBits::One);
        assert_eq!(map_stop_bits(StopBits::Two), serialport::StopBits::Two);
        assert_eq!(
            map_flow_control(FlowControl::None),
            serialport::FlowControl::None
        );
        assert_eq!(
            map_flow_control(FlowControl::Hardware),
            serialport::FlowControl::Hardware
        );
        assert_eq!(
            map_flow_control(FlowControl::Software),
            serialport::FlowControl::Software
        );
        assert_eq!(map_parity(Parity::None).unwrap(), serialport::Parity::None);
        assert_eq!(map_parity(Parity::Odd).unwrap(), serialport::Parity::Odd);
        assert_eq!(map_parity(Parity::Even).unwrap(), serialport::Parity::Even);
    }

    #[test]
    fn mark_and_space_parity_are_unsupported() {
        for parity in [Parity::Mark, Parity::Space] {
            assert!(matches!(
                map_parity(parity),
                Err(TransportError::Unsupported(_))
            ));
        }
    }

    #[test]
    fn open_rejects_bad_config_before_touching_the_device() {
        let factory = SerialportFactory::new();
        let port = PortId::new("/dev/cu.serialist-does-not-exist");

        let zero_baud = cfg_with(|c| c.baud = 0);
        assert!(matches!(
            factory.open(&port, &zero_baud),
            Err(TransportError::Config(_))
        ));

        let mark = cfg_with(|c| c.parity = Parity::Mark);
        assert!(matches!(
            factory.open(&port, &mark),
            Err(TransportError::Unsupported(_))
        ));
    }

    #[test]
    fn opening_a_missing_port_fails_cleanly() {
        let name = if cfg!(windows) {
            "COM99999"
        } else {
            "/dev/cu.serialist-does-not-exist"
        };
        match SerialportFactory::new().open(&PortId::new(name), &SerialConfig::default()) {
            Err(TransportError::NotFound(id)) => assert_eq!(id.as_str(), name),
            Err(TransportError::Io(_)) => {}
            Err(other) => panic!("expected NotFound or Io, got {other}"),
            Ok(_) => panic!("a port that does not exist opened"),
        }
    }

    #[test]
    fn disconnect_classification_by_kind() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::NotConnected,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::NotFound,
        ] {
            assert!(
                matches!(
                    map_io_error(io::Error::new(kind, "x")),
                    TransportError::Disconnected
                ),
                "{kind:?} should mean disconnected"
            );
        }
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::TimedOut,
        ] {
            assert!(
                matches!(
                    map_io_error(io::Error::new(kind, "x")),
                    TransportError::Io(_)
                ),
                "{kind:?} should stay an I/O error"
            );
        }
        assert!(matches!(
            map_io_error(io::Error::other("something unrelated")),
            TransportError::Io(_)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn disconnect_classification_by_errno() {
        // The shapes serialport produces on Unix: kind Other, description from nix.
        for desc in ["I/O error", "No such device or address", "No such device"] {
            assert!(
                matches!(
                    map_io_error(io::Error::other(desc)),
                    TransportError::Disconnected
                ),
                "{desc:?} should mean disconnected"
            );
        }
        // Raw codes, as std or another crate would report them.
        for code in [5, 6, 19] {
            assert!(matches!(
                map_io_error(io::Error::from_raw_os_error(code)),
                TransportError::Disconnected
            ));
        }
        // EACCES is a real error, not a disconnect.
        assert!(matches!(
            map_io_error(io::Error::from_raw_os_error(13)),
            TransportError::Io(_)
        ));
    }

    #[test]
    fn serialport_errors_map_by_kind() {
        use serialport::ErrorKind as K;

        let gone = serialport::Error::new(K::NoDevice, "gone");
        assert!(matches!(map_port_error(gone), TransportError::Disconnected));

        let bad = serialport::Error::new(K::InvalidInput, "bad baud");
        assert!(matches!(map_port_error(bad), TransportError::Config(_)));

        let timed_out = serialport::Error::new(K::Io(io::ErrorKind::TimedOut), "slow");
        assert!(matches!(map_port_error(timed_out), TransportError::Io(_)));
    }

    #[test]
    fn open_errors_map_by_kind() {
        use serialport::ErrorKind as K;
        let port = PortId::new("/dev/cu.test");

        let missing = serialport::Error::new(K::Io(io::ErrorKind::NotFound), "No such file");
        assert!(matches!(
            open_error(&port, missing),
            TransportError::NotFound(_)
        ));

        let denied = serialport::Error::new(K::Io(io::ErrorKind::PermissionDenied), "denied");
        match open_error(&port, denied) {
            TransportError::Io(e) => {
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
                assert!(e.to_string().contains("/dev/cu.test"));
            }
            other => panic!("expected Io, got {other}"),
        }

        #[cfg(unix)]
        {
            let busy = serialport::Error::new(K::NoDevice, "Device or resource busy");
            match open_error(&port, busy) {
                TransportError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::ResourceBusy),
                other => panic!("expected Io(ResourceBusy), got {other}"),
            }

            let stale = serialport::Error::new(K::Unknown, "No such device or address");
            assert!(matches!(
                open_error(&port, stale),
                TransportError::NotFound(_)
            ));
        }
    }

    /// Reader and writer against a pseudo-terminal pair, which needs no hardware. The
    /// factory is not used because the crate's macOS open path applies `IOSSIOSPEED`
    /// unconditionally and a pty rejects it; the halves are what carries the logic.
    #[cfg(unix)]
    mod pty {
        use super::*;
        use serialport::TTYPort;
        use std::io::{Read, Write};
        use std::sync::Mutex;

        /// `TTYPort::pair` calls the non-reentrant `ptsname` on macOS.
        static PTY_LOCK: Mutex<()> = Mutex::new(());

        struct Halves {
            reader: SerialReader,
            writer: SerialWriter,
            master: TTYPort,
        }

        fn halves() -> Halves {
            let (master, slave) = {
                let _guard = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                TTYPort::pair().expect("pty pair")
            };
            let writer_port = slave.try_clone_native().expect("clone slave");
            Halves {
                reader: SerialReader::new(Box::new(slave)),
                writer: SerialWriter::new(Box::new(writer_port), SerialConfig::default())
                    .expect("writer"),
                master,
            }
        }

        #[test]
        fn read_times_out_with_ok_zero_and_blocks_for_the_timeout() {
            let mut h = halves();
            let mut buf = [0u8; 16];
            let started = Instant::now();
            let n = h
                .reader
                .read(&mut buf, Duration::from_millis(30))
                .expect("timeout is not an error");
            let elapsed = started.elapsed();
            assert_eq!(n, 0);
            assert!(
                elapsed >= Duration::from_millis(20),
                "returned after {elapsed:?}, so the read did not block"
            );
        }

        #[test]
        fn zero_timeout_is_clamped_and_does_not_spin() {
            let mut h = halves();
            let mut buf = [0u8; 16];
            assert_eq!(h.reader.read(&mut buf, Duration::ZERO).unwrap(), 0);
            assert_eq!(h.reader.timeout, MIN_READ_TIMEOUT);
        }

        #[test]
        fn read_returns_data_written_by_the_far_end() {
            let mut h = halves();
            h.master.write_all(b"hello").unwrap();
            let mut buf = [0u8; 16];
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(2);
            while got.len() < 5 && Instant::now() < deadline {
                let n = h.reader.read(&mut buf, Duration::from_millis(20)).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            assert_eq!(got, b"hello");
        }

        #[test]
        fn timeout_is_only_pushed_to_the_port_when_it_changes() {
            let mut h = halves();
            let mut buf = [0u8; 4];
            h.reader.read(&mut buf, Duration::from_millis(5)).unwrap();
            assert_eq!(h.reader.port.timeout(), Duration::from_millis(5));
            // Poke the port's own timeout behind the reader's back: an unchanged
            // request must not overwrite it, a changed one must.
            h.reader.port.set_timeout(Duration::from_millis(7)).unwrap();
            h.reader.read(&mut buf, Duration::from_millis(5)).unwrap();
            assert_eq!(h.reader.port.timeout(), Duration::from_millis(7));
            h.reader.read(&mut buf, Duration::from_millis(6)).unwrap();
            assert_eq!(h.reader.port.timeout(), Duration::from_millis(6));
        }

        /// Reads `want` bytes from the far end on its own thread. Needed because `flush`
        /// is `tcdrain`, and on a pty that blocks until the master side consumes the data.
        fn drain_far_end(mut master: TTYPort, want: usize) -> thread::JoinHandle<Vec<u8>> {
            master
                .set_timeout(Duration::from_millis(50))
                .expect("master timeout");
            thread::spawn(move || {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                let deadline = Instant::now() + Duration::from_secs(20);
                while got.len() < want && Instant::now() < deadline {
                    match master.read(&mut buf) {
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                        Err(e) => panic!("master read: {e}"),
                    }
                }
                got
            })
        }

        #[test]
        fn writer_delivers_bytes_to_the_far_end() {
            let mut h = halves();
            let drain = drain_far_end(h.master, 6);
            h.writer.write_all(b"ping\r\n").unwrap();
            assert_eq!(drain.join().unwrap(), b"ping\r\n");
        }

        #[test]
        fn large_write_survives_a_slow_reader() {
            // Far more than the pty buffer, so write() must return short and be retried.
            let mut h = halves();
            let payload: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
            let drain = drain_far_end(h.master, payload.len());
            h.writer.write_all(&payload).unwrap();
            let got = drain.join().unwrap();
            assert_eq!(got.len(), payload.len());
            assert!(got == payload, "payload was corrupted or reordered");
        }

        #[test]
        fn dropping_the_far_end_reads_as_disconnected() {
            let mut h = halves();
            drop(h.master);
            let mut buf = [0u8; 16];
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match h.reader.read(&mut buf, Duration::from_millis(20)) {
                    Err(TransportError::Disconnected) => break,
                    Ok(_) if Instant::now() < deadline => {}
                    other => panic!("expected Disconnected, got {other:?}"),
                }
            }
        }
    }
}
