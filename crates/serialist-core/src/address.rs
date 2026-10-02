//! What a port id names: the grammar of `tcp:` and `replay:` ids, parsed into
//! [`PortAddress`].
//!
//! A [`PortId`] stays an opaque string everywhere else (settings, state, scripts, the
//! router). Only the transport that opens an id, and UI code that wants to label one,
//! parse it here.
//!
//! # Grammar
//!
//! | Id | Names | Opened by |
//! | --- | --- | --- |
//! | `/dev/cu.usbserial-1420`, `COM3` (no scheme) | an OS serial port | `SerialportFactory` |
//! | `virtual:<device>` | a simulated device | `serialist_sim::SimTransportFactory` |
//! | `tcp:<host>:<port>` | a raw TCP byte stream (no Telnet, no RFC 2217) | [`TcpTransportFactory`](crate::tcp::TcpTransportFactory) |
//! | `replay:<path>[?<option>&...]` | a recorded capture played back | [`ReplayTransportFactory`](crate::replay::ReplayTransportFactory) |
//!
//! **`tcp:<host>:<port>`.** `host` is a DNS name, an IPv4 address, or an IPv6 address in
//! brackets (`tcp:[::1]:4000`); `port` is 1 to 65535 in decimal. A `//` after the
//! scheme is tolerated (`tcp://host:4000`), but the canonical form, which
//! [`TcpAddress::port_id`] writes and the UI should store, has none.
//!
//! **`replay:<path>[?<options>]`.** `path` is the raw capture file (the bytes a
//! recording wrote), absolute or relative to the process's working directory; its
//! timing sidecar, if any, is found next to it (see [`crate::capture`]). Options follow
//! the *last* `?` as `key=value` pairs joined by `&`:
//!
//! - `speed=<factor>x` (`1x`, `4x`, `0.5x`; the `x` is optional) or `speed=max` (as fast
//!   as the session reads): see [`ReplaySpeed`];
//! - `end=disconnect` or `end=hold`: what happens after the last byte, see [`ReplayEnd`].
//!
//! An option left out falls back to the replay factory's defaults (the app's settings).
//! The text after the last `?` counts as options only when it has the `key=value` shape;
//! then an unknown key or a bad value is an error rather than part of the path. A file
//! whose name itself ends in `?key=value` cannot be named, which is fine: `?` cannot
//! appear in a Windows file name at all and is rare elsewhere.
//!
//! Neither kind of id is discovered by a [`PortSource`](crate::port::PortSource), so they
//! do not appear in the Devices panel. They open from `--port` (with or without
//! `--script`), `serial.open{ port = … }` in a headless `--script` run, a restored tab, and
//! the UI's own "Connect to TCP…" and "Open capture…" actions, all through the same
//! [`RoutingTransportFactory`](crate::RoutingTransportFactory). (Scripts in the app reach a
//! TCP or replay tab through `serial.current()`; the app gives scripts no `serial.open` for
//! any kind of port.)
//! A device profile still applies to them: its `match.path` is a prefix of the id, so
//! `{ "match": { "path": "tcp:10.0.0.5:4000" }, "plugin": "airoha-race" }` gives that
//! endpoint a codec, and `"path": "replay:"` matches every replay.

use std::fmt;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::composite::{VIRTUAL_SCHEME, scheme_of};
use crate::port::PortId;

/// The scheme of raw TCP ports: `tcp:<host>:<port>`.
pub const TCP_SCHEME: &str = "tcp";
/// The scheme of replayed captures: `replay:<path>[?options]`.
pub const REPLAY_SCHEME: &str = "replay";

/// A port id that does not follow its scheme's grammar.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("{id}: a tcp port id is tcp:<host>:<port>, for example tcp:192.168.1.5:4000")]
    TcpShape { id: String },
    #[error("{id}: {host:?} is not a host name or address (IPv6 goes in brackets: tcp:[::1]:4000)")]
    TcpHost { id: String, host: String },
    #[error("{id}: {port:?} is not a TCP port (1 to 65535)")]
    TcpPort { id: String, port: String },
    #[error("{id}: a replay port id names a capture file: replay:/path/to/capture.bin")]
    ReplayPath { id: String },
    #[error("{id}: unknown replay option {key:?} (known: speed, end)")]
    ReplayOption { id: String, key: String },
    #[error("{id}: bad value {value:?} for the replay option {key:?}: {reason}")]
    ReplayValue {
        id: String,
        key: String,
        value: String,
        reason: String,
    },
}

/// What a port id names, by its scheme. See the module docs for the grammar.
#[derive(Clone, Debug, PartialEq)]
pub enum PortAddress {
    /// No scheme: an OS serial port path (`/dev/cu.usbserial-1420`, `COM3`).
    Serial,
    /// `virtual:<device>`.
    Virtual { device: String },
    /// `tcp:<host>:<port>`.
    Tcp(TcpAddress),
    /// `replay:<path>[?options]`.
    Replay(ReplayAddress),
    /// Another `<scheme>:` this build has no grammar for. Whether it opens is up to the
    /// router.
    Other { scheme: String },
}

impl PortAddress {
    /// Parse `port`. Fails only for a `tcp:` or `replay:` id that breaks its grammar.
    pub fn parse(port: &PortId) -> Result<Self, AddressError> {
        Ok(match scheme_of(port) {
            None => Self::Serial,
            Some(VIRTUAL_SCHEME) => Self::Virtual {
                device: rest_of(port).to_owned(),
            },
            Some(TCP_SCHEME) => Self::Tcp(TcpAddress::from_port_id(port)?),
            Some(REPLAY_SCHEME) => Self::Replay(ReplayAddress::from_port_id(port)?),
            Some(other) => Self::Other {
                scheme: other.to_owned(),
            },
        })
    }

    /// The id's scheme, `None` for a serial port path.
    pub fn scheme(&self) -> Option<&str> {
        match self {
            Self::Serial => None,
            Self::Virtual { .. } => Some(VIRTUAL_SCHEME),
            Self::Tcp(_) => Some(TCP_SCHEME),
            Self::Replay(_) => Some(REPLAY_SCHEME),
            Self::Other { scheme } => Some(scheme),
        }
    }
}

/// The text after `<scheme>:`.
fn rest_of(port: &PortId) -> &str {
    port.as_str()
        .split_once(':')
        .map_or(port.as_str(), |(_, rest)| rest)
}

// ---------------------------------------------------------------------------------
// tcp:
// ---------------------------------------------------------------------------------

/// A raw TCP endpoint, `tcp:<host>:<port>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TcpAddress {
    /// A DNS name or an IP address, IPv6 without brackets.
    pub host: String,
    pub port: u16,
}

impl TcpAddress {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// Parse a `tcp:` id. Fails with [`AddressError`] if it is not one or breaks the
    /// grammar.
    pub fn from_port_id(port: &PortId) -> Result<Self, AddressError> {
        let id = port.as_str();
        let shape = || AddressError::TcpShape { id: id.to_owned() };
        if scheme_of(port) != Some(TCP_SCHEME) {
            return Err(shape());
        }
        let rest = rest_of(port);
        let rest = rest.strip_prefix("//").unwrap_or(rest);
        let (host, port_text) = rest.rsplit_once(':').ok_or_else(shape)?;
        if host.is_empty() || port_text.is_empty() {
            return Err(shape());
        }
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0 && port_text.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| AddressError::TcpPort {
                id: id.to_owned(),
                port: port_text.to_owned(),
            })?;
        let bad_host = || AddressError::TcpHost {
            id: id.to_owned(),
            host: host.to_owned(),
        };
        let host = match host.strip_prefix('[') {
            Some(bracketed) => {
                let inner = bracketed.strip_suffix(']').ok_or_else(bad_host)?;
                inner.parse::<Ipv6Addr>().map_err(|_| bad_host())?;
                inner.to_owned()
            }
            None => {
                let valid = host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'));
                if !valid {
                    return Err(bad_host());
                }
                host.to_owned()
            }
        };
        Ok(Self { host, port })
    }

    /// `host:port`, with an IPv6 host in brackets: what `ToSocketAddrs` takes.
    pub fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The canonical id, `tcp:<host>:<port>`.
    pub fn port_id(&self) -> PortId {
        PortId::new(self.to_string())
    }
}

impl fmt::Display for TcpAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{TCP_SCHEME}:{}", self.authority())
    }
}

// ---------------------------------------------------------------------------------
// replay:
// ---------------------------------------------------------------------------------

/// A recorded capture to play back, `replay:<path>[?speed=…&end=…]`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayAddress {
    /// The raw capture file.
    pub path: PathBuf,
    /// `speed=`, or `None` for the factory's default.
    pub speed: Option<ReplaySpeed>,
    /// `end=`, or `None` for the factory's default.
    pub end: Option<ReplayEnd>,
}

impl ReplayAddress {
    /// The capture at `path`, with the factory's default speed and end.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            speed: None,
            end: None,
        }
    }

    pub fn with_speed(mut self, speed: ReplaySpeed) -> Self {
        self.speed = Some(speed);
        self
    }

    pub fn with_end(mut self, end: ReplayEnd) -> Self {
        self.end = Some(end);
        self
    }

    /// Parse a `replay:` id. Fails with [`AddressError`] if it is not one or breaks the
    /// grammar.
    pub fn from_port_id(port: &PortId) -> Result<Self, AddressError> {
        let id = port.as_str();
        if scheme_of(port) != Some(REPLAY_SCHEME) {
            return Err(AddressError::ReplayPath { id: id.to_owned() });
        }
        let rest = rest_of(port);
        let (path, options) = match rest.rsplit_once('?') {
            Some((path, options)) if looks_like_options(options) => (path, Some(options)),
            _ => (rest, None),
        };
        if path.is_empty() {
            return Err(AddressError::ReplayPath { id: id.to_owned() });
        }
        let mut address = Self::new(path);
        for pair in options.into_iter().flat_map(|o| o.split('&')) {
            // `looks_like_options` guarantees every pair has an `=`.
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let bad_value = |reason: String| AddressError::ReplayValue {
                id: id.to_owned(),
                key: key.to_owned(),
                value: value.to_owned(),
                reason,
            };
            match key {
                "speed" => address.speed = Some(value.parse().map_err(bad_value)?),
                "end" => address.end = Some(value.parse().map_err(bad_value)?),
                _ => {
                    return Err(AddressError::ReplayOption {
                        id: id.to_owned(),
                        key: key.to_owned(),
                    });
                }
            }
        }
        Ok(address)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where the capture's timing sidecar would be. See [`crate::capture::timing_path`].
    pub fn timing_path(&self) -> PathBuf {
        crate::capture::timing_path(&self.path)
    }

    /// The canonical id: `replay:<path>`, then `?speed=…&end=…` for the options set.
    pub fn port_id(&self) -> PortId {
        PortId::new(self.to_string())
    }
}

impl fmt::Display for ReplayAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{REPLAY_SCHEME}:{}", self.path.display())?;
        let mut sep = '?';
        if let Some(speed) = self.speed {
            write!(f, "{sep}speed={speed}")?;
            sep = '&';
        }
        if let Some(end) = self.end {
            write!(f, "{sep}end={end}")?;
        }
        Ok(())
    }
}

/// `key=value(&key=value)*` with lowercase ASCII keys: the shape of replay options.
fn looks_like_options(text: &str) -> bool {
    !text.is_empty()
        && text.split('&').all(|pair| {
            pair.split_once('=').is_some_and(|(key, value)| {
                !key.is_empty()
                    && !value.is_empty()
                    && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
        })
}

/// How fast a capture plays back.
///
/// With a timing sidecar, chunk `i` is due `(t_i - t_0) / factor` after the replay
/// starts. Without one, bytes go out at the session's baud rate times `factor`. `Max`
/// ignores time in both cases and hands over each chunk as soon as the session reads.
///
/// Text form (ids, settings): `1x`, `4x`, `0.5x` (the `x` is optional) or `max`. In a
/// settings file it may also be a bare number.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReplaySpeed {
    /// Real time times this factor, from [`ReplaySpeed::MIN_FACTOR`] to
    /// [`ReplaySpeed::MAX_FACTOR`]. Build it with [`ReplaySpeed::times`].
    Times(f64),
    /// As fast as the session reads.
    Max,
}

impl ReplaySpeed {
    /// Real time.
    pub const REALTIME: Self = Self::Times(1.0);
    pub const MIN_FACTOR: f64 = 0.001;
    pub const MAX_FACTOR: f64 = 1_000_000.0;

    /// `factor` times real time, or `None` outside
    /// [`MIN_FACTOR`](Self::MIN_FACTOR)..=[`MAX_FACTOR`](Self::MAX_FACTOR) or not finite.
    pub fn times(factor: f64) -> Option<Self> {
        (factor.is_finite() && (Self::MIN_FACTOR..=Self::MAX_FACTOR).contains(&factor))
            .then_some(Self::Times(factor))
    }

    /// The factor, `None` for [`ReplaySpeed::Max`].
    pub fn factor(self) -> Option<f64> {
        match self {
            Self::Times(factor) => Some(factor),
            Self::Max => None,
        }
    }

    /// How long `recorded` takes at this speed: `recorded / factor` rounded to the
    /// nanosecond, `None` for `Max` (no wait at all). Exact at 1x and whenever the
    /// quotient is a whole number of nanoseconds (below about 104 days), so a replay
    /// schedule on a manual clock lands on exact instants. Saturates at
    /// [`Duration::MAX`] rather than overflowing.
    pub fn scale(self, recorded: Duration) -> Option<Duration> {
        let factor = self.factor()?;
        if factor == 1.0 {
            return Some(recorded);
        }
        let nanos = (recorded.as_nanos() as f64 / factor).round();
        Some(if nanos < u64::MAX as f64 {
            Duration::from_nanos(nanos as u64)
        } else {
            Duration::MAX
        })
    }
}

impl Default for ReplaySpeed {
    fn default() -> Self {
        Self::REALTIME
    }
}

impl fmt::Display for ReplaySpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Times(factor) => write!(f, "{factor}x"),
            Self::Max => f.write_str("max"),
        }
    }
}

impl FromStr for ReplaySpeed {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("max") {
            return Ok(Self::Max);
        }
        let number = text.strip_suffix(['x', 'X']).unwrap_or(text).trim_end();
        let factor: f64 = number
            .parse()
            .map_err(|_| format!("{text:?} is not a speed (1x, 4x, 0.5x or max)"))?;
        Self::times(factor).ok_or_else(|| {
            format!(
                "{text:?} is out of range ({}x to {}x, or max)",
                Self::MIN_FACTOR,
                Self::MAX_FACTOR
            )
        })
    }
}

impl Serialize for ReplaySpeed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ReplaySpeed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SpeedVisitor;

        impl Visitor<'_> for SpeedVisitor {
            type Value = ReplaySpeed;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a replay speed such as \"1x\", \"4x\", \"0.5x\", \"max\" or a number")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<ReplaySpeed, E> {
                text.parse().map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, factor: f64) -> Result<ReplaySpeed, E> {
                ReplaySpeed::times(factor)
                    .ok_or_else(|| E::custom(format!("replay speed {factor} is out of range")))
            }

            fn visit_u64<E: de::Error>(self, factor: u64) -> Result<ReplaySpeed, E> {
                self.visit_f64(factor as f64)
            }

            fn visit_i64<E: de::Error>(self, factor: i64) -> Result<ReplaySpeed, E> {
                self.visit_f64(factor as f64)
            }
        }

        deserializer.deserialize_any(SpeedVisitor)
    }
}

/// What a replay does after its last byte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayEnd {
    /// The next read fails with [`TransportError::Disconnected`](crate::TransportError),
    /// so the session ends as if the device went away. The default.
    #[default]
    Disconnect,
    /// The port stays open and silent until the session closes it.
    Hold,
}

impl fmt::Display for ReplayEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disconnect => "disconnect",
            Self::Hold => "hold",
        })
    }
}

impl FromStr for ReplayEnd {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim().to_ascii_lowercase().as_str() {
            "disconnect" => Ok(Self::Disconnect),
            "hold" => Ok(Self::Hold),
            _ => Err(format!("{text:?} is not disconnect or hold")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(id: &str) -> Result<PortAddress, AddressError> {
        PortAddress::parse(&PortId::new(id))
    }

    fn tcp(id: &str) -> Result<TcpAddress, AddressError> {
        TcpAddress::from_port_id(&PortId::new(id))
    }

    fn replay(id: &str) -> Result<ReplayAddress, AddressError> {
        ReplayAddress::from_port_id(&PortId::new(id))
    }

    #[test]
    fn schemes_pick_the_address_kind() {
        assert_eq!(parse("/dev/cu.usbserial-1420"), Ok(PortAddress::Serial));
        assert_eq!(parse("COM3"), Ok(PortAddress::Serial));
        assert_eq!(
            parse("/dev/serial/by-path/pci-0000:00:14.0-usb-0:2:1.0-port0"),
            Ok(PortAddress::Serial)
        );
        assert_eq!(
            parse("virtual:race"),
            Ok(PortAddress::Virtual {
                device: "race".into()
            })
        );
        assert_eq!(
            parse("tcp:localhost:23"),
            Ok(PortAddress::Tcp(TcpAddress::new("localhost", 23)))
        );
        assert_eq!(
            parse("replay:/tmp/boot.bin"),
            Ok(PortAddress::Replay(ReplayAddress::new("/tmp/boot.bin")))
        );
        let other = parse("rfc2217:host:1").unwrap();
        assert_eq!(other.scheme(), Some("rfc2217"));
        assert_eq!(PortAddress::Serial.scheme(), None);
        assert_eq!(parse("tcp:h:1").unwrap().scheme(), Some(TCP_SCHEME));
        assert_eq!(parse("replay:x").unwrap().scheme(), Some(REPLAY_SCHEME));
        assert!(parse("tcp:nope").is_err(), "a malformed tcp id is an error");
    }

    #[test]
    fn tcp_ids() {
        assert_eq!(
            tcp("tcp:192.168.1.5:4000"),
            Ok(TcpAddress::new("192.168.1.5", 4000))
        );
        assert_eq!(
            tcp("tcp:esp-link.local:23"),
            Ok(TcpAddress::new("esp-link.local", 23))
        );
        assert_eq!(tcp("tcp:[::1]:4000"), Ok(TcpAddress::new("::1", 4000)));
        assert_eq!(
            tcp("tcp://localhost:65535"),
            Ok(TcpAddress::new("localhost", 65535)),
            "a URL-style // is tolerated"
        );

        for shape in ["tcp:", "tcp:host", "tcp::4000", "tcp:host:", "virtual:x"] {
            assert!(
                matches!(tcp(shape), Err(AddressError::TcpShape { .. })),
                "{shape}: {:?}",
                tcp(shape)
            );
        }
        for port in ["tcp:host:0", "tcp:host:65536", "tcp:host:+1", "tcp:host:x"] {
            assert!(
                matches!(tcp(port), Err(AddressError::TcpPort { .. })),
                "{port}: {:?}",
                tcp(port)
            );
        }
        for host in [
            "tcp:::1:4000",
            "tcp:[::1:4000",
            "tcp:[nope]:4000",
            "tcp:a b:1",
            "tcp:a/b:1",
        ] {
            assert!(
                matches!(tcp(host), Err(AddressError::TcpHost { .. })),
                "{host}: {:?}",
                tcp(host)
            );
        }
        assert_eq!(
            tcp("tcp:h:0").unwrap_err().to_string(),
            "tcp:h:0: \"0\" is not a TCP port (1 to 65535)"
        );
    }

    #[test]
    fn tcp_ids_round_trip_in_canonical_form() {
        for (id, canonical) in [
            ("tcp:192.168.1.5:4000", "tcp:192.168.1.5:4000"),
            ("tcp:[::1]:4000", "tcp:[::1]:4000"),
            ("tcp://localhost:23", "tcp:localhost:23"),
        ] {
            let address = tcp(id).unwrap();
            assert_eq!(address.port_id().as_str(), canonical);
            assert_eq!(tcp(canonical).unwrap(), address);
        }
        assert_eq!(TcpAddress::new("::1", 7).authority(), "[::1]:7");
        assert_eq!(TcpAddress::new("10.0.0.5", 7).authority(), "10.0.0.5:7");
    }

    #[test]
    fn replay_ids() {
        assert_eq!(
            replay("replay:/captures/boot.bin"),
            Ok(ReplayAddress::new("/captures/boot.bin"))
        );
        assert_eq!(
            replay(r"replay:C:\captures\boot.bin"),
            Ok(ReplayAddress::new(r"C:\captures\boot.bin")),
            "a drive letter's colon is part of the path"
        );
        assert_eq!(
            replay("replay:caps/rel.bin?speed=4x&end=hold"),
            Ok(ReplayAddress::new("caps/rel.bin")
                .with_speed(ReplaySpeed::Times(4.0))
                .with_end(ReplayEnd::Hold))
        );
        assert_eq!(
            replay("replay:/a.bin?speed=max"),
            Ok(ReplayAddress::new("/a.bin").with_speed(ReplaySpeed::Max))
        );
        assert_eq!(
            replay("replay:/what?.bin"),
            Ok(ReplayAddress::new("/what?.bin")),
            "a ? not followed by options is part of the path"
        );
        assert_eq!(
            replay("replay:/a?b.bin?end=disconnect"),
            Ok(ReplayAddress::new("/a?b.bin").with_end(ReplayEnd::Disconnect)),
            "options follow the last ?"
        );

        assert!(matches!(
            replay("replay:"),
            Err(AddressError::ReplayPath { .. })
        ));
        assert!(matches!(
            replay("replay:?speed=2x"),
            Err(AddressError::ReplayPath { .. })
        ));
        assert!(matches!(
            replay("replay:/a.bin?sped=2x"),
            Err(AddressError::ReplayOption { key, .. }) if key == "sped"
        ));
        assert!(matches!(
            replay("replay:/a.bin?speed=0x"),
            Err(AddressError::ReplayValue { key, .. }) if key == "speed"
        ));
        assert!(matches!(
            replay("replay:/a.bin?end=loop"),
            Err(AddressError::ReplayValue { key, .. }) if key == "end"
        ));
    }

    #[test]
    fn replay_ids_round_trip() {
        for id in [
            "replay:/captures/boot.bin",
            "replay:/captures/boot.bin?speed=4x",
            "replay:/captures/boot.bin?end=hold",
            "replay:/captures/boot.bin?speed=0.5x&end=hold",
            "replay:/captures/boot.bin?speed=max&end=disconnect",
        ] {
            assert_eq!(replay(id).unwrap().port_id().as_str(), id);
        }
        assert_eq!(
            replay("replay:/c/boot.bin").unwrap().timing_path(),
            PathBuf::from("/c/boot.bin.timing")
        );
    }

    #[test]
    fn speeds() {
        let speed = |text: &str| text.parse::<ReplaySpeed>();
        assert_eq!(speed("1x"), Ok(ReplaySpeed::REALTIME));
        assert_eq!(speed("4"), Ok(ReplaySpeed::Times(4.0)));
        assert_eq!(speed(" 0.5X "), Ok(ReplaySpeed::Times(0.5)));
        assert_eq!(speed("MAX"), Ok(ReplaySpeed::Max));
        for bad in [
            "0", "-1x", "nanx", "infx", "x", "", "fast", "0.0001x", "2000000x",
        ] {
            assert!(speed(bad).is_err(), "{bad} accepted");
        }
        assert_eq!(ReplaySpeed::Times(4.0).to_string(), "4x");
        assert_eq!(ReplaySpeed::Times(0.25).to_string(), "0.25x");
        assert_eq!(ReplaySpeed::Max.to_string(), "max");
        assert_eq!(ReplaySpeed::default(), ReplaySpeed::REALTIME);

        let second = Duration::from_secs(1);
        assert_eq!(
            ReplaySpeed::Times(4.0).scale(second),
            Some(Duration::from_millis(250))
        );
        assert_eq!(ReplaySpeed::Times(0.5).scale(second), Some(2 * second));
        assert_eq!(ReplaySpeed::Max.scale(second), None);
        let odd = Duration::new(3, 123_456_789);
        assert_eq!(ReplaySpeed::REALTIME.scale(odd), Some(odd), "1x is exact");
        assert_eq!(
            ReplaySpeed::Times(4.0).scale(Duration::from_millis(10)),
            Some(Duration::from_micros(2500)),
            "a whole number of nanoseconds is exact"
        );
        assert_eq!(
            ReplaySpeed::Times(3.0).scale(Duration::from_nanos(10)),
            Some(Duration::from_nanos(3)),
            "rounded to the nanosecond"
        );
        assert_eq!(
            ReplaySpeed::Times(ReplaySpeed::MIN_FACTOR).scale(Duration::MAX),
            Some(Duration::MAX),
            "saturates"
        );
    }

    #[test]
    fn speeds_and_ends_in_settings_json() {
        let speed =
            |json: &str| serde_json::from_str::<ReplaySpeed>(json).map_err(|e| e.to_string());
        assert_eq!(speed("\"4x\""), Ok(ReplaySpeed::Times(4.0)));
        assert_eq!(speed("\"max\""), Ok(ReplaySpeed::Max));
        assert_eq!(speed("2"), Ok(ReplaySpeed::Times(2.0)));
        assert_eq!(speed("0.5"), Ok(ReplaySpeed::Times(0.5)));
        assert!(speed("0").is_err());
        assert!(speed("\"warp\"").is_err());
        assert_eq!(
            serde_json::to_string(&ReplaySpeed::Times(2.5)).unwrap(),
            "\"2.5x\""
        );

        assert_eq!(
            serde_json::from_str::<ReplayEnd>("\"hold\"").unwrap(),
            ReplayEnd::Hold
        );
        assert_eq!(
            serde_json::to_string(&ReplayEnd::Disconnect).unwrap(),
            "\"disconnect\""
        );
        assert_eq!("Hold".parse::<ReplayEnd>(), Ok(ReplayEnd::Hold));
        assert!("loop".parse::<ReplayEnd>().is_err());
    }
}
