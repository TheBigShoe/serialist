//! The real transport: [`SerialportFactory`] opens OS serial ports through the
//! `serialport` crate and hands the session a reader half and a writer half.
//!
//! Platform notes, all verified against `serialport` 4.10.1:
//!
//! * **Custom baud rates** are passed straight through. The crate applies `IOSSIOSPEED`
//!   on macOS (after every `tcsetattr`, so the OS-reported rate is never trustworthy),
//!   `termios2`/`BOTHER` on Linux and a `DCB` rate on Windows. On Linux it uses `BOTHER`
//!   for every rate, so rates past the highest `B` constant (`B4000000`), 12 Mbaud
//!   included, reach the driver; only on powerpc Linux and illumos does it map rates
//!   through the `B` constants instead, and there anything off that table fails to open
//!   with `TransportError::Config`. Whether the adapter runs at the rate is the driver's
//!   business (FTDI's FT232H and FT2232H go to 12 Mbaud) and needs hardware to check:
//!   see `tests/hardware.rs`.
//! * **Exclusive access** is the crate default on Unix (`TIOCEXCL` plus a non-blocking
//!   `flock`) and is requested explicitly here. Windows ports are always opened with a
//!   share mode of zero, so they are exclusive by construction.
//! * **Mark and space parity** do not exist in the crate, so they are rejected with
//!   [`TransportError::Unsupported`] before any device is touched.
//! * **Timeouts** are per object on Unix (each `try_clone` has its own) but device-wide
//!   on Windows (`SetCommTimeouts`). The reader owns the timeout on both; the writer
//!   never touches it on Windows and tolerates short write timeouts everywhere.
//! * **Windows I/O is serialised per handle.** `serialport` opens the port without
//!   `FILE_FLAG_OVERLAPPED`, and `try_clone` (`DuplicateHandle`) shares the one file
//!   object, so Windows queues synchronous I/O on it: a `ReadFile` that is waiting for
//!   data can hold up a `WriteFile` for up to the read timeout. Keep read timeouts short
//!   (10 to 50 ms, as the session does) and expect writes on Windows to sometimes wait
//!   that long. Removing the coupling needs an overlapped Windows transport that owns
//!   the handle itself; that is future work, not something this layer can fix.
//! * **The output queue is never drained with a blocking call.** `tcdrain` and
//!   `FlushFileBuffers` have no timeout, and with RTS/CTS flow control and CTS held low
//!   they never return, which would strand the writer thread and keep the port's
//!   exclusive lock. The writer instead polls `bytes_to_write()` against a stall limit
//!   (see `SerialWriter`). That counts bytes still queued in the driver; bytes already
//!   handed to an adapter's hardware FIFO may still be shifting out when it reads zero.

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

/// A write, or a drain of queued output, that makes no progress at all for this long
/// fails with `TimedOut` instead of blocking the writer thread forever (for example CTS
/// held low by the far end).
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(5);

/// Bytes per `write` call on Unix (see `SerialWriter::write_chunk`): small below
/// [`FAST_LINK_BAUD`], where a call must not outgrow the queue's room, larger above it.
const SLOW_WRITE_CHUNK: usize = 64;
const FAST_WRITE_CHUNK: usize = 512;
const FAST_LINK_BAUD: u32 = 921_600;

/// How often a drain looks at the driver's output queue.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Longest the writer waits for queued output when it is dropped. Closing a tty with
/// output still queued blocks in the kernel until it drains, which never happens under
/// stalled flow control, so whatever is left after this is purged.
const CLOSE_DRAIN_LIMIT: Duration = Duration::from_secs(1);

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

/// The transmit and control half.
///
/// * `write_all` returns once the driver has accepted every byte; it does not wait for
///   them to leave the wire. `set_control`, `reconfigure` and `send_break` first drain
///   the driver's output queue so they take effect after everything already written.
/// * Draining never blocks in the OS: it polls `bytes_to_write()` and gives up after
///   `stall_limit` without the queue shrinking. On giving up it purges the stuck output
///   (so a retry, and closing the port, cannot hang behind it) and reports `TimedOut`.
/// * `reconfigure` is all-or-nothing as far as the hardware allows; see its docs.
/// * On Windows the handle is shared with the reader and I/O on it is serialised, so a
///   write can wait behind a pending read for up to the read timeout (see the module
///   docs; an overlapped Windows transport is future work).
struct SerialWriter {
    port: Box<dyn SerialPort>,
    /// What the line is currently set to, so `reconfigure` only touches what changed.
    /// This matters on macOS, where every termios change briefly re-applies 9600 baud.
    applied: SerialConfig,
    /// No-progress limit for writes and drains. A field so tests need not wait 5 s.
    stall_limit: Duration,
    /// Longest wait for queued output on drop. A field for the same reason.
    close_limit: Duration,
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
        Ok(Self {
            port,
            applied,
            stall_limit: WRITE_STALL_LIMIT,
            close_limit: CLOSE_DRAIN_LIMIT,
        })
    }

    /// How many bytes to hand to one `write` call.
    ///
    /// On Unix the port is a blocking fd, so `write(2)` does not return until the whole
    /// buffer is queued, and it sleeps in the kernel whenever the queue is full. The
    /// `poll` before it only proves there is room for a little, so a large buffer can
    /// block for as long as flow control holds the queue full, which the stall limit
    /// can never interrupt. Small chunks keep every call inside the room `poll` promised:
    /// BSD ttys (macOS) make a writer sleep once the queue passes a high-water mark of
    /// roughly 100 bytes at very low baud rates up to 2 KB at fast ones, and Linux
    /// reports the port writable with the queue under 256 bytes and kilobytes of room.
    /// 64 bytes fits inside both at any baud rate; fast links get bigger chunks to keep
    /// the syscall rate down.
    ///
    /// Windows bounds a blocked `WriteFile` with the comm write timeout, so it takes
    /// whatever it is given.
    fn write_chunk(&self) -> usize {
        if !cfg!(unix) {
            usize::MAX
        } else if self.applied.baud >= FAST_LINK_BAUD {
            FAST_WRITE_CHUNK
        } else {
            SLOW_WRITE_CHUNK
        }
    }

    /// Gives up on a write that has made no progress for `stall_limit`.
    fn check_stall(&self, last_progress: Instant) -> Result<(), TransportError> {
        if last_progress.elapsed() >= self.stall_limit {
            Err(TransportError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "write stalled: the device is not accepting data (flow control?)",
            )))
        } else {
            Ok(())
        }
    }

    /// Waits until the driver's output queue is empty. Progress means the queue got
    /// smaller; `stall_limit` without it purges the queue and fails with `TimedOut`.
    /// Errors from the poll go through [`map_port_error`], so a vanished device is
    /// `Disconnected`.
    fn drain(&mut self) -> Result<(), TransportError> {
        let mut last_progress = Instant::now();
        let mut last_pending: Option<u32> = None;
        loop {
            let pending = self.port.bytes_to_write().map_err(map_port_error)?;
            if pending == 0 {
                return Ok(());
            }
            if last_pending.is_none_or(|previous| pending < previous) {
                last_progress = Instant::now();
            } else if last_progress.elapsed() >= self.stall_limit {
                // Best effort: if the purge fails the device is going away anyway.
                let _ = self.port.clear(serialport::ClearBuffer::Output);
                return Err(TransportError::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "output stalled: {pending} bytes stayed queued for {:?} \
                         (flow control?); the queued output was discarded",
                        self.stall_limit
                    ),
                )));
            }
            last_pending = Some(pending);
            thread::sleep(DRAIN_POLL_INTERVAL);
        }
    }

    /// Moves the line to `target`, touching only the settings that differ from
    /// `applied` unless `force` is set, and recording each success in `applied`.
    fn apply_config(&mut self, target: &SerialConfig, force: bool) -> Result<(), TransportError> {
        let parity = map_parity(target.parity)?;

        // Baud first: on macOS each later setter re-applies the port's cached rate, so
        // updating the cache first means they all re-apply the new one.
        if force || target.baud != self.applied.baud {
            self.port
                .set_baud_rate(target.baud)
                .map_err(map_port_error)?;
            self.applied.baud = target.baud;
        }
        if force || target.data_bits != self.applied.data_bits {
            self.port
                .set_data_bits(map_data_bits(target.data_bits))
                .map_err(map_port_error)?;
            self.applied.data_bits = target.data_bits;
        }
        if force || target.parity != self.applied.parity {
            self.port.set_parity(parity).map_err(map_port_error)?;
            self.applied.parity = target.parity;
        }
        if force || target.stop_bits != self.applied.stop_bits {
            self.port
                .set_stop_bits(map_stop_bits(target.stop_bits))
                .map_err(map_port_error)?;
            self.applied.stop_bits = target.stop_bits;
        }
        if force || target.flow_control != self.applied.flow_control {
            self.port
                .set_flow_control(map_flow_control(target.flow_control))
                .map_err(map_port_error)?;
            self.applied.flow_control = target.flow_control;
        }
        Ok(())
    }
}

impl Drop for SerialWriter {
    fn drop(&mut self) {
        // Give queued output a moment to leave, then purge what is left. The last
        // close of a tty waits for its output queue, and under stalled flow control
        // that wait has no end, which would leave the port locked until the app exits.
        let queued = |port: &dyn SerialPort| port.bytes_to_write().is_ok_and(|n| n > 0);
        let deadline = Instant::now() + self.close_limit;
        while queued(&*self.port) && Instant::now() < deadline {
            thread::sleep(DRAIN_POLL_INTERVAL);
        }
        if queued(&*self.port) {
            tracing::debug!("discarding output still queued at close");
            let _ = self.port.clear(serialport::ClearBuffer::Output);
        }
    }
}

impl TransportWriter for SerialWriter {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        let mut rest = bytes;
        let mut last_progress = Instant::now();
        while !rest.is_empty() {
            let chunk = &rest[..rest.len().min(self.write_chunk())];
            match self.port.write(chunk) {
                Ok(0) => {
                    // Windows reports a write timeout as a zero-byte success. The call
                    // already blocked for the timeout, this only stops a 1 ms timeout
                    // from turning into a tight loop.
                    self.check_stall(last_progress)?;
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
                        self.check_stall(last_progress)?;
                    }
                    _ => return Err(map_io_error(err)),
                },
            }
        }
        // No flush here: `tcdrain` and `FlushFileBuffers` cannot be given a timeout.
        Ok(())
    }

    fn set_control(&mut self, line: ControlLine, asserted: bool) -> Result<(), TransportError> {
        // Whatever was written before the toggle must leave before the line moves.
        self.drain()?;
        match line {
            ControlLine::Dtr => self.port.write_data_terminal_ready(asserted),
            ControlLine::Rts => self.port.write_request_to_send(asserted),
        }
        .map_err(map_port_error)
    }

    /// Changes the line settings after draining output, so queued bytes go out at the
    /// old settings.
    ///
    /// Errors that are known before touching the port (`Unsupported`, `Config`) and a
    /// failed drain leave it unchanged. If a setter fails partway, every setting is
    /// re-applied from the previous configuration and the original error is returned,
    /// so the port still matches what the session believes. If that rollback fails too,
    /// the result is a `TransportError::Config` naming both failures and the settings
    /// last known to be in effect, and the port state should be treated as suspect
    /// (close and reopen). A vanished device is `Disconnected` and is not rolled back.
    fn reconfigure(&mut self, config: &SerialConfig) -> Result<(), TransportError> {
        validate(config)?;
        self.drain()?;

        let previous = self.applied.clone();
        let err = match self.apply_config(config, false) {
            Ok(()) => return Ok(()),
            Err(TransportError::Disconnected) => return Err(TransportError::Disconnected),
            Err(err) => err,
        };
        tracing::warn!(error = %err, "reconfigure failed; restoring {}", previous.summary());
        match self.apply_config(&previous, true) {
            Ok(()) => Err(err),
            Err(TransportError::Disconnected) => Err(TransportError::Disconnected),
            Err(rollback) => Err(TransportError::Config(format!(
                "could not change the port to {} ({err}), and restoring {} failed ({rollback}); \
                 settings last known to be in effect: {}",
                config.summary(),
                previous.summary(),
                self.applied.summary(),
            ))),
        }
    }

    fn send_break(&mut self, duration: Duration) -> Result<(), TransportError> {
        // Data queued before the break must precede it on the wire.
        self.drain()?;
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

    /// A scripted `SerialPort` that records every call together with the size of its
    /// output queue at that moment, so the writer's ordering, stall handling and rollback
    /// are tested identically on every platform.
    mod mock {
        use super::*;
        use std::io::{Read, Write};
        use std::sync::{Arc, Mutex, MutexGuard};

        type PendingFn = Box<dyn Fn(Duration) -> serialport::Result<u32> + Send>;

        pub struct State {
            /// Every action as `"<what>@<queue length when it happened>"`.
            pub log: Vec<String>,
            /// Number of `bytes_to_write` polls.
            pub polls: usize,
            created: Instant,
            /// Output queue length as a function of time since creation.
            pending: PendingFn,
            cleared: bool,
            /// 0-based indices, counted over the framing setters, that fail.
            pub failing_setters: Vec<usize>,
            pub fail_kind: serialport::ErrorKind,
            setter_calls: usize,
            pub baud: u32,
            pub parity: serialport::Parity,
            pub stop_bits: serialport::StopBits,
        }

        impl State {
            fn queue(&self) -> serialport::Result<u32> {
                if self.cleared {
                    Ok(0)
                } else {
                    (self.pending)(self.created.elapsed())
                }
            }

            fn push(&mut self, what: impl std::fmt::Display) {
                let queue = self.queue().unwrap_or(0);
                self.log.push(format!("{what}@{queue}"));
            }

            /// Runs a framing setter, failing it if its index is scripted to fail.
            fn setter(
                &mut self,
                what: impl std::fmt::Display,
                apply: impl FnOnce(&mut State),
            ) -> serialport::Result<()> {
                let index = self.setter_calls;
                self.setter_calls += 1;
                if self.failing_setters.contains(&index) {
                    self.push(format!("{what} failed"));
                    return Err(serialport::Error::new(self.fail_kind, "scripted failure"));
                }
                self.push(what);
                apply(self);
                Ok(())
            }
        }

        #[derive(Clone)]
        pub struct MockPort(Arc<Mutex<State>>);

        impl MockPort {
            pub fn new(
                pending: impl Fn(Duration) -> serialport::Result<u32> + Send + 'static,
            ) -> Self {
                Self(Arc::new(Mutex::new(State {
                    log: Vec::new(),
                    polls: 0,
                    created: Instant::now(),
                    pending: Box::new(pending),
                    cleared: false,
                    failing_setters: Vec::new(),
                    fail_kind: serialport::ErrorKind::Unknown,
                    setter_calls: 0,
                    baud: 115_200,
                    parity: serialport::Parity::None,
                    stop_bits: serialport::StopBits::One,
                })))
            }

            /// A port whose output queue is always empty.
            pub fn idle() -> Self {
                Self::new(|_| Ok(0))
            }

            /// A port whose output queue never moves, like CTS held low.
            pub fn stuck(queued: u32) -> Self {
                Self::new(move |_| Ok(queued))
            }

            pub fn state(&self) -> MutexGuard<'_, State> {
                self.0.lock().unwrap()
            }

            pub fn log(&self) -> Vec<String> {
                self.state().log.clone()
            }
        }

        impl Read for MockPort {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::TimedOut))
            }
        }

        impl Write for MockPort {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.state().push(format!("write {}", buf.len()));
                Ok(buf.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                self.state().push("flush");
                Ok(())
            }
        }

        impl SerialPort for MockPort {
            fn name(&self) -> Option<String> {
                Some("mock".to_owned())
            }
            fn baud_rate(&self) -> serialport::Result<u32> {
                Ok(self.state().baud)
            }
            fn data_bits(&self) -> serialport::Result<serialport::DataBits> {
                Ok(serialport::DataBits::Eight)
            }
            fn flow_control(&self) -> serialport::Result<serialport::FlowControl> {
                Ok(serialport::FlowControl::None)
            }
            fn parity(&self) -> serialport::Result<serialport::Parity> {
                Ok(self.state().parity)
            }
            fn stop_bits(&self) -> serialport::Result<serialport::StopBits> {
                Ok(self.state().stop_bits)
            }
            fn timeout(&self) -> Duration {
                Duration::from_millis(20)
            }
            fn set_baud_rate(&mut self, baud_rate: u32) -> serialport::Result<()> {
                self.state()
                    .setter(format!("baud {baud_rate}"), |s| s.baud = baud_rate)
            }
            fn set_data_bits(&mut self, data_bits: serialport::DataBits) -> serialport::Result<()> {
                self.state()
                    .setter(format!("data_bits {data_bits}"), |_| {})
            }
            fn set_flow_control(
                &mut self,
                flow_control: serialport::FlowControl,
            ) -> serialport::Result<()> {
                self.state().setter(format!("flow {flow_control}"), |_| {})
            }
            fn set_parity(&mut self, parity: serialport::Parity) -> serialport::Result<()> {
                self.state()
                    .setter(format!("parity {parity}"), |s| s.parity = parity)
            }
            fn set_stop_bits(&mut self, stop_bits: serialport::StopBits) -> serialport::Result<()> {
                self.state().setter(format!("stop_bits {stop_bits}"), |s| {
                    s.stop_bits = stop_bits
                })
            }
            fn set_timeout(&mut self, _timeout: Duration) -> serialport::Result<()> {
                Ok(())
            }
            fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
                self.state().push(format!("rts {level}"));
                Ok(())
            }
            fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
                self.state().push(format!("dtr {level}"));
                Ok(())
            }
            fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
                Ok(false)
            }
            fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
                Ok(false)
            }
            fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
                Ok(false)
            }
            fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
                Ok(false)
            }
            fn bytes_to_read(&self) -> serialport::Result<u32> {
                Ok(0)
            }
            fn bytes_to_write(&self) -> serialport::Result<u32> {
                let mut state = self.state();
                state.polls += 1;
                state.queue()
            }
            fn clear(&self, buffer_to_clear: serialport::ClearBuffer) -> serialport::Result<()> {
                let mut state = self.state();
                state.push(format!("clear {buffer_to_clear:?}"));
                if !matches!(buffer_to_clear, serialport::ClearBuffer::Input) {
                    state.cleared = true;
                }
                Ok(())
            }
            fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
                Ok(Box::new(self.clone()))
            }
            fn set_break(&self) -> serialport::Result<()> {
                self.state().push("break on");
                Ok(())
            }
            fn clear_break(&self) -> serialport::Result<()> {
                self.state().push("break off");
                Ok(())
            }
        }
    }

    use mock::MockPort;

    /// A writer over `port` with limits short enough for tests.
    fn mock_writer(port: &MockPort) -> SerialWriter {
        let mut writer = SerialWriter::new(Box::new(port.clone()), SerialConfig::default())
            .expect("mock writer");
        writer.stall_limit = Duration::from_millis(150);
        writer.close_limit = Duration::from_millis(50);
        writer
    }

    /// A queue that shrinks by one byte every `step`, starting from `start`.
    fn shrinking_queue(start: u32, step: Duration) -> MockPort {
        MockPort::new(move |elapsed| {
            let gone =
                u32::try_from(elapsed.as_millis() / step.as_millis().max(1)).unwrap_or(u32::MAX);
            Ok(start.saturating_sub(gone))
        })
    }

    #[test]
    fn write_all_neither_flushes_nor_waits_for_the_queue() {
        // A queue that never empties would hang a flush and stall a drain.
        let port = MockPort::stuck(99);
        let mut writer = mock_writer(&port);
        let started = Instant::now();
        writer.write_all(b"abc").expect("accepted by the driver");
        assert!(started.elapsed() < Duration::from_millis(100));
        assert_eq!(port.log(), ["write 3@99"]);
        assert_eq!(port.state().polls, 0, "write_all must not poll the queue");
    }

    #[test]
    fn unix_writes_are_chunked_so_no_call_outgrows_the_queue() {
        let port = MockPort::idle();
        let mut writer = mock_writer(&port);
        writer.write_all(&[0; 200]).unwrap();
        let expected: &[&str] = if cfg!(unix) {
            &["write 64@0", "write 64@0", "write 64@0", "write 8@0"]
        } else {
            &["write 200@0"]
        };
        assert_eq!(port.log(), expected);

        // Fast links take bigger chunks; an empty write is a no-op.
        let port = MockPort::idle();
        let mut writer = mock_writer(&port);
        writer.applied.baud = 3_000_000;
        writer.write_all(&[0; 1100]).unwrap();
        writer.write_all(&[]).unwrap();
        let expected: &[&str] = if cfg!(unix) {
            &["write 512@0", "write 512@0", "write 76@0"]
        } else {
            &["write 1100@0"]
        };
        assert_eq!(port.log(), expected);

        // 12 Mbaud, the top of the standard list, is a fast link too.
        writer.applied.baud = 12_000_000;
        assert_eq!(
            writer.write_chunk(),
            if cfg!(unix) {
                FAST_WRITE_CHUNK
            } else {
                usize::MAX
            }
        );
    }

    #[test]
    fn twelve_mbaud_passes_straight_through_to_the_port() {
        // No rate table and no rounding: the crate gets the integer, and applies it with
        // IOSSIOSPEED on macOS, termios2/BOTHER on Linux and a DCB rate on Windows.
        let port = MockPort::idle();
        let mut writer = mock_writer(&port);
        writer
            .reconfigure(&cfg_with(|c| c.baud = 12_000_000))
            .unwrap();
        assert_eq!(port.log(), ["baud 12000000@0"]);
        assert_eq!(port.state().baud, 12_000_000);
        assert_eq!(writer.applied.baud, 12_000_000);
    }

    #[test]
    fn control_operations_wait_for_queued_output_first() {
        let port = shrinking_queue(3, Duration::from_millis(10));
        let mut writer = mock_writer(&port);
        writer.set_control(ControlLine::Dtr, true).unwrap();
        assert_eq!(port.log(), ["dtr true@0"]);

        let port = shrinking_queue(3, Duration::from_millis(10));
        let mut writer = mock_writer(&port);
        writer.reconfigure(&cfg_with(|c| c.baud = 9600)).unwrap();
        assert_eq!(
            port.log(),
            ["baud 9600@0"],
            "only the changed setting is touched"
        );

        let port = shrinking_queue(3, Duration::from_millis(10));
        let mut writer = mock_writer(&port);
        writer.send_break(Duration::from_millis(1)).unwrap();
        assert_eq!(port.log(), ["break on@0", "break off@0"]);
    }

    #[test]
    fn a_slow_but_moving_queue_is_not_a_stall() {
        // Takes 320 ms to empty against a 150 ms limit: progress keeps resetting it.
        let port = shrinking_queue(8, Duration::from_millis(40));
        let mut writer = mock_writer(&port);
        writer
            .set_control(ControlLine::Rts, false)
            .expect("still draining");
        assert_eq!(port.log(), ["rts false@0"]);
    }

    #[test]
    fn a_stalled_drain_fails_within_the_limit_and_purges() {
        type Op = fn(&mut SerialWriter) -> Result<(), TransportError>;
        let ops: [(&str, Op); 3] = [
            ("set_control", |w| w.set_control(ControlLine::Dtr, true)),
            ("reconfigure", |w| {
                w.reconfigure(&cfg_with(|c| c.baud = 9600))
            }),
            ("send_break", |w| w.send_break(Duration::from_millis(1))),
        ];
        for (name, op) in ops {
            let port = MockPort::stuck(10);
            let mut writer = mock_writer(&port);
            let started = Instant::now();
            let err = op(&mut writer).expect_err(name);
            let elapsed = started.elapsed();
            assert!(
                elapsed >= Duration::from_millis(100) && elapsed < Duration::from_secs(2),
                "{name} gave up after {elapsed:?}"
            );
            match err {
                TransportError::Io(e) => {
                    assert_eq!(e.kind(), io::ErrorKind::TimedOut, "{name}");
                    let text = e.to_string();
                    assert!(
                        text.contains("stalled") && text.contains("discarded"),
                        "{text}"
                    );
                }
                other => panic!("{name}: expected Io(TimedOut), got {other:?}"),
            }
            // Nothing was applied, and the stuck output was thrown away.
            assert_eq!(port.log(), ["clear Output@10"], "{name}");
            assert_eq!(port.state().baud, 115_200, "{name} changed the baud rate");

            // With the queue purged, a retry goes straight through.
            op(&mut writer).unwrap_or_else(|e| panic!("{name} retry failed: {e:?}"));
        }
    }

    #[test]
    fn a_drain_error_that_means_gone_is_disconnected() {
        use serialport::ErrorKind as K;
        for kind in [K::NoDevice, K::Unknown] {
            let description = if kind == K::Unknown {
                "I/O error"
            } else {
                "gone"
            };
            // errno descriptions are only interpreted on Unix.
            if kind == K::Unknown && !cfg!(unix) {
                continue;
            }
            let port = MockPort::new(move |_| Err(serialport::Error::new(kind, description)));
            let mut writer = mock_writer(&port);
            assert!(
                matches!(
                    writer.set_control(ControlLine::Dtr, true),
                    Err(TransportError::Disconnected)
                ),
                "{kind:?}"
            );
            assert!(port.log().is_empty(), "the line must not be touched");
        }
    }

    #[test]
    fn dropping_the_writer_purges_output_that_cannot_drain() {
        let port = MockPort::stuck(5);
        let started = Instant::now();
        drop(mock_writer(&port));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(port.log(), ["clear Output@5"]);

        let idle = MockPort::idle();
        drop(mock_writer(&idle));
        assert!(idle.log().is_empty(), "nothing queued, nothing to purge");
    }

    #[test]
    fn reconfigure_touches_only_what_changed() {
        let port = MockPort::idle();
        let mut writer = mock_writer(&port);
        writer.reconfigure(&SerialConfig::default()).unwrap();
        assert!(port.log().is_empty());

        writer
            .reconfigure(&cfg_with(|c| {
                c.parity = Parity::Odd;
                c.stop_bits = StopBits::Two;
            }))
            .unwrap();
        assert_eq!(port.log(), ["parity Odd@0", "stop_bits Two@0"]);
    }

    #[test]
    fn unsupported_reconfigure_changes_nothing() {
        let port = MockPort::idle();
        let mut writer = mock_writer(&port);
        let err = writer
            .reconfigure(&cfg_with(|c| c.parity = Parity::Mark))
            .expect_err("mark parity");
        assert!(matches!(err, TransportError::Unsupported(_)));
        assert!(port.log().is_empty());
        assert_eq!(port.state().polls, 0);
    }

    #[test]
    fn failed_reconfigure_rolls_back_to_the_previous_settings() {
        let port = MockPort::idle();
        // Setter calls: baud (0) ok, parity (1) fails, then the rollback re-applies all.
        port.state().failing_setters = vec![1];
        let mut writer = mock_writer(&port);
        let wanted = cfg_with(|c| {
            c.baud = 9600;
            c.parity = Parity::Even;
        });

        let err = writer
            .reconfigure(&wanted)
            .expect_err("parity setter fails");
        assert!(
            matches!(err, TransportError::Io(_)),
            "the original error: {err:?}"
        );

        assert_eq!(writer.applied, SerialConfig::default());
        assert_eq!(port.state().baud, 115_200, "baud was rolled back");
        assert_eq!(port.state().parity, serialport::Parity::None);
        assert_eq!(
            port.log(),
            [
                "baud 9600@0",
                "parity Even failed@0",
                "baud 115200@0",
                "data_bits Eight@0",
                "parity None@0",
                "stop_bits One@0",
                "flow None@0",
            ]
        );

        // The writer is still usable and now succeeds.
        writer.reconfigure(&wanted).expect("second attempt");
        assert_eq!(writer.applied, wanted);
    }

    #[test]
    fn failed_rollback_reports_the_settings_in_effect() {
        let port = MockPort::idle();
        // parity (1) fails, and so does restoring the baud rate (2).
        port.state().failing_setters = vec![1, 2];
        let mut writer = mock_writer(&port);
        let err = writer
            .reconfigure(&cfg_with(|c| {
                c.baud = 9600;
                c.parity = Parity::Even;
            }))
            .expect_err("both fail");
        match err {
            TransportError::Config(message) => {
                assert!(message.contains("9600 8E1"), "target missing: {message}");
                assert!(message.contains("restoring 115200 8N1 failed"), "{message}");
                assert!(
                    message.contains("last known to be in effect: 9600 8N1"),
                    "{message}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
        assert_eq!(writer.applied.summary(), "9600 8N1");
    }

    #[test]
    fn a_vanished_device_is_not_rolled_back() {
        let port = MockPort::idle();
        {
            let mut state = port.state();
            state.failing_setters = vec![0];
            state.fail_kind = serialport::ErrorKind::NoDevice;
        }
        let mut writer = mock_writer(&port);
        let err = writer
            .reconfigure(&cfg_with(|c| c.baud = 9600))
            .expect_err("device is gone");
        assert!(matches!(err, TransportError::Disconnected));
        assert_eq!(port.log(), ["baud 9600 failed@0"], "no rollback attempts");
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

        /// Reads `want` bytes from the far end on its own thread, standing in for a device
        /// that consumes what the writer sends.
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
        fn write_gives_up_instead_of_hanging_when_nobody_reads() {
            // Far more than any pty buffer and nobody on the master side: the stand-in
            // for CTS held low. The write must fail at the stall limit, not block.
            let mut h = halves();
            h.writer.stall_limit = Duration::from_millis(300);
            h.writer.close_limit = Duration::from_millis(20);
            let started = Instant::now();
            let result = h.writer.write_all(&vec![0x55; 1024 * 1024]);
            let elapsed = started.elapsed();
            match result {
                Err(TransportError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::TimedOut),
                other => panic!("expected a TimedOut stall, got {other:?}"),
            }
            assert!(
                elapsed < Duration::from_secs(5),
                "write took {elapsed:?} to give up"
            );
        }

        #[test]
        fn drain_returns_within_the_limit_when_the_far_end_never_reads() {
            // On macOS a pty holds queued output until the master reads it, which is
            // what made `tcdrain` hang. Elsewhere the small write may drain at once, so
            // both outcomes are acceptable; hanging is not.
            let mut h = halves();
            h.writer.stall_limit = Duration::from_millis(300);
            h.writer.close_limit = Duration::from_millis(20);
            // Small enough to be accepted without a reader (a pty only reports itself
            // writable while it holds under a few hundred bytes).
            h.writer.write_all(&[0xAA; 128]).unwrap();

            let started = Instant::now();
            let first = h.writer.drain();
            let elapsed = started.elapsed();
            assert!(
                elapsed < Duration::from_secs(5),
                "drain took {elapsed:?} to give up"
            );
            if let Err(err) = first {
                match err {
                    TransportError::Io(e) => {
                        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
                        assert!(e.to_string().contains("discarded"), "{e}");
                    }
                    other => panic!("expected a TimedOut stall, got {other:?}"),
                }
                // The stuck output was purged, so the next drain finds nothing.
                h.writer.drain().expect("queue was purged");
            }
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
