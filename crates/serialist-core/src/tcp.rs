//! The raw TCP transport: `tcp:<host>:<port>` ids open a plain TCP connection whose
//! bytes are the port's bytes, for serial-to-network bridges (ser2net in raw mode,
//! ESP-Link, a terminal server's raw port) and for anything that speaks a byte stream.
//! There is no Telnet negotiation and no RFC 2217; those would be schemes of their own.
//!
//! # What each operation means here
//!
//! - **Open** resolves the host (blocking DNS; the UI opens ports off the main thread)
//!   and tries each address for at most [`TCP_CONNECT_TIMEOUT`]. `TCP_NODELAY` is set,
//!   because a terminal sends a keystroke at a time and Nagle's delay would be felt.
//!   The [`SerialConfig`] is accepted and ignored: a raw socket has no line settings.
//! - **Read** blocks for at most the session's read timeout and returns what arrived,
//!   one `recv` per call. The peer closing the connection (end of stream) or resetting
//!   it is [`TransportError::Disconnected`].
//! - **Write** sends everything or fails; a peer that stops reading makes it fail with
//!   `TimedOut` after [`TCP_WRITE_TIMEOUT`] rather than wedge the writer thread.
//! - **DTR and RTS** are accepted and do nothing: the session asserts both on every
//!   open, and an error there would be noise on every connect.
//! - **Reconfigure** (baud, framing, flow control) is accepted and does nothing, so the
//!   session's settings stay what the user chose; a bridge's own serial side is
//!   configured on the bridge.
//! - **Break** is [`TransportError::Unsupported`]: raw TCP has no out-of-band signal.
//! - **Disconnect** is the session dropping both halves, which closes the socket.
//!   **Reconnect** opens a new connection to the same endpoint.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::address::TcpAddress;
use crate::config::SerialConfig;
use crate::port::PortId;
use crate::transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};

/// Longest `open` waits for one address to accept the connection.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Longest one blocked write waits for the peer to make room before failing.
pub const TCP_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// A socket read timeout of zero means "block forever" to the OS, so the reader never
/// asks for less than this.
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(1);

/// Opens `tcp:<host>:<port>` ids. Stateless, so one instance can be shared freely.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpTransportFactory;

impl TcpTransportFactory {
    pub fn new() -> Self {
        Self
    }
}

impl TransportFactory for TcpTransportFactory {
    fn open(&self, port: &PortId, _config: &SerialConfig) -> Result<Transport, TransportError> {
        let address = TcpAddress::from_port_id(port)
            .map_err(|err| TransportError::Config(err.to_string()))?;
        let stream = connect(port, &address)?;
        let peer = stream
            .peer_addr()
            .map_or_else(|_| address.authority(), |peer| peer.to_string());
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(TCP_WRITE_TIMEOUT))?;
        let reader_stream = stream.try_clone()?;
        tracing::debug!(%port, %peer, "opened tcp connection");

        Ok(Transport {
            reader: Box::new(TcpReader {
                stream: reader_stream,
                timeout: None,
            }),
            writer: Box::new(TcpWriter { stream }),
            description: describe(&address, &peer),
        })
    }
}

/// `tcp:localhost:4000 (127.0.0.1:4000)`, or just the id when the peer is the same text.
fn describe(address: &TcpAddress, peer: &str) -> String {
    if address.authority() == peer {
        address.to_string()
    } else {
        format!("{address} ({peer})")
    }
}

/// Resolves the host and connects to the first address that accepts.
fn connect(port: &PortId, address: &TcpAddress) -> Result<TcpStream, TransportError> {
    let context = |err: io::Error, what: &str| {
        TransportError::Io(io::Error::new(err.kind(), format!("{port}: {what}: {err}")))
    };
    let addrs: Vec<SocketAddr> = address
        .authority()
        .to_socket_addrs()
        .map_err(|err| context(err, "could not resolve the host"))?
        .collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, "the host has no addresses");
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                tracing::debug!(%port, %addr, %err, "tcp connect failed");
                last = err;
            }
        }
    }
    Err(context(last, "could not connect"))
}

/// Does this error mean the connection is gone for good?
fn is_disconnect(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

fn map_io(err: io::Error) -> TransportError {
    if is_disconnect(&err) {
        TransportError::Disconnected
    } else {
        TransportError::Io(err)
    }
}

struct TcpReader {
    stream: TcpStream,
    /// The timeout last handed to the socket, so it is only set on change.
    timeout: Option<Duration>,
}

impl TransportReader for TcpReader {
    fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, TransportError> {
        if buf.is_empty() {
            return Ok(0);
        }
        let timeout = timeout.max(MIN_READ_TIMEOUT);
        if self.timeout != Some(timeout) {
            self.stream.set_read_timeout(Some(timeout))?;
            self.timeout = Some(timeout);
        }
        match self.stream.read(buf) {
            // End of stream: the peer closed its side.
            Ok(0) => Err(TransportError::Disconnected),
            Ok(n) => Ok(n),
            Err(err) => match err.kind() {
                // Unix reports a receive timeout as WouldBlock, Windows as TimedOut.
                io::ErrorKind::WouldBlock
                | io::ErrorKind::TimedOut
                | io::ErrorKind::Interrupted => Ok(0),
                _ => Err(map_io(err)),
            },
        }
    }
}

struct TcpWriter {
    stream: TcpStream,
}

impl TransportWriter for TcpWriter {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.stream.write_all(bytes).map_err(map_io)
    }

    /// No control lines on a socket: accepted, does nothing. See the module docs.
    fn set_control(&mut self, _line: ControlLine, _asserted: bool) -> Result<(), TransportError> {
        Ok(())
    }

    /// No line settings on a socket: accepted, does nothing. See the module docs.
    fn reconfigure(&mut self, _config: &SerialConfig) -> Result<(), TransportError> {
        Ok(())
    }

    // `send_break` keeps the trait's default: `Unsupported("break")`.
    //
    // No `shutdown` on drop: on `Session::close` the writer goes first, and a peer that
    // answers our end of stream by closing would make the reader report `Disconnected`
    // before the session's orderly one. The socket closes when the reader half, the
    // last clone, is dropped within one read timeout.
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::time::Instant;

    use super::*;

    fn listener() -> (TcpListener, PortId) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, PortId::new(format!("tcp:127.0.0.1:{port}")))
    }

    #[test]
    fn malformed_ids_are_config_errors() {
        for id in ["tcp:", "tcp:host", "tcp:host:0", "tcp:[x]:1"] {
            let result = TcpTransportFactory.open(&PortId::new(id), &SerialConfig::default());
            assert!(
                matches!(result, Err(TransportError::Config(_))),
                "{id}: {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn a_refused_connection_is_an_io_error_not_not_found() {
        // Bind, note the port, then free it: nothing listens there now.
        let (listener, id) = listener();
        drop(listener);
        let err = TcpTransportFactory
            .open(&id, &SerialConfig::default())
            .err()
            .expect("nothing is listening");
        match err {
            TransportError::Io(err) => {
                assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
                assert!(err.to_string().starts_with(id.as_str()), "{err}");
            }
            other => panic!("expected Io(ConnectionRefused), got {other:?}"),
        }
    }

    #[test]
    fn halves_read_write_and_ignore_line_controls() {
        let (listener, id) = listener();
        let Transport {
            mut reader,
            mut writer,
            description,
        } = TcpTransportFactory
            .open(&id, &SerialConfig::default())
            .unwrap();
        assert_eq!(description, id.as_str(), "the peer is the address itself");
        let (mut peer, _) = listener.accept().unwrap();

        let mut buf = [0u8; 64];
        let started = Instant::now();
        assert_eq!(
            reader.read(&mut buf, Duration::from_millis(20)).unwrap(),
            0,
            "a timeout is Ok(0)"
        );
        assert!(started.elapsed() >= Duration::from_millis(15));

        peer.write_all(b"hello").unwrap();
        let mut got = Vec::new();
        while got.len() < 5 {
            let n = reader.read(&mut buf, Duration::from_millis(50)).unwrap();
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, b"hello");

        writer.write_all(b"AT\r\n").unwrap();
        writer.set_control(ControlLine::Dtr, true).unwrap();
        writer.set_control(ControlLine::Rts, false).unwrap();
        writer
            .reconfigure(&SerialConfig {
                baud: 9600,
                ..SerialConfig::default()
            })
            .unwrap();
        assert!(matches!(
            writer.send_break(Duration::from_millis(1)),
            Err(TransportError::Unsupported(_))
        ));
        let mut sent = [0u8; 4];
        peer.read_exact(&mut sent).unwrap();
        assert_eq!(&sent, b"AT\r\n");

        drop(peer);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match reader.read(&mut buf, Duration::from_millis(20)) {
                Ok(0) => assert!(Instant::now() < deadline, "no end of stream"),
                Ok(n) => panic!("unexpected {n} bytes"),
                Err(TransportError::Disconnected) => break,
                Err(other) => panic!("expected Disconnected, got {other:?}"),
            }
        }
    }
}
