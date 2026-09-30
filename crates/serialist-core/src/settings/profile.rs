//! Device profiles: per-device settings keyed by USB identity or port path.

use std::fmt;
use std::path::PathBuf;

use serde::de::{Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};

use crate::config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
use crate::port::{PortInfo, PortKind};

use super::de::{Raw, opt_baud_rate, optional_scalar, scalar};
use super::types::LineEnding;

/// A USB vendor or product id.
///
/// The file form is an integer (`3725`) or a hex string (`"0x0e8d"`, or `"0e8d"` as
/// `lsusb` prints it). Strings are always hex; integers are decimal as JSON writes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UsbId(pub u16);

impl fmt::Display for UsbId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "0x{:04x}", self.0)
    }
}

fn usb_id(raw: Raw<'_>) -> Result<UsbId, String> {
    const EXPECTED: &str = "a USB id as a hex string such as \"0x0e8d\" or an integer";
    match raw {
        Raw::Str(s) => {
            let digits = s
                .trim()
                .strip_prefix("0x")
                .or_else(|| s.trim().strip_prefix("0X"))
                .unwrap_or_else(|| s.trim());
            u16::from_str_radix(digits, 16)
                .map(UsbId)
                .map_err(|_| format!("{s:?} is not a USB id, expected {EXPECTED} up to 0xffff"))
        }
        other => match other.unsigned() {
            Some(v) => u16::try_from(v)
                .map(UsbId)
                .map_err(|_| format!("USB id {v} is above 65535")),
            None => Err(format!("invalid value, expected {EXPECTED}")),
        },
    }
}

impl<'de> Deserialize<'de> for UsbId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        scalar(d, "a USB id as a hex string or an integer", usb_id)
    }
}

impl Serialize for UsbId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Which ports a profile applies to. Every key that is set must match; a profile with
/// no keys matches every port, which makes a catch-all profile at the end of the list.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceMatch {
    #[serde(default)]
    pub vid: Option<UsbId>,
    #[serde(default)]
    pub pid: Option<UsbId>,
    /// Case-insensitive substring of the USB product string.
    #[serde(default)]
    pub product: Option<String>,
    /// Case-insensitive substring of the USB manufacturer string.
    #[serde(default)]
    pub manufacturer: Option<String>,
    /// Case-insensitive substring of the USB serial number.
    #[serde(default)]
    pub serial_number: Option<String>,
    /// The port path exactly, or a prefix of it such as `/dev/cu.usbserial`. There is
    /// no glob syntax.
    #[serde(default)]
    pub path: Option<String>,
}

fn contains_ignore_case(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

fn text_matches(wanted: &Option<String>, actual: Option<&str>) -> bool {
    match wanted {
        None => true,
        Some(needle) => actual.is_some_and(|value| contains_ignore_case(value, needle)),
    }
}

impl DeviceMatch {
    pub fn matches(&self, port: &PortInfo) -> bool {
        if let Some(path) = &self.path
            && !port.id.as_str().starts_with(path.as_str())
        {
            return false;
        }
        let usb = match &port.kind {
            PortKind::Usb(usb) => Some(usb),
            _ => None,
        };
        // The USB keys need a USB port to look at.
        let needs_usb = self.vid.is_some()
            || self.pid.is_some()
            || self.product.is_some()
            || self.manufacturer.is_some()
            || self.serial_number.is_some();
        let Some(usb) = usb else {
            return !needs_usb;
        };
        self.vid.is_none_or(|vid| vid.0 == usb.vid)
            && self.pid.is_none_or(|pid| pid.0 == usb.pid)
            && text_matches(&self.product, usb.product.as_deref())
            && text_matches(&self.manufacturer, usb.manufacturer.as_deref())
            && text_matches(&self.serial_number, usb.serial_number.as_deref())
    }
}

fn data_bits(raw: Raw<'_>) -> Result<DataBits, String> {
    match raw {
        Raw::Str("five") => Ok(DataBits::Five),
        Raw::Str("six") => Ok(DataBits::Six),
        Raw::Str("seven") => Ok(DataBits::Seven),
        Raw::Str("eight") => Ok(DataBits::Eight),
        other => match other.unsigned() {
            Some(5) => Ok(DataBits::Five),
            Some(6) => Ok(DataBits::Six),
            Some(7) => Ok(DataBits::Seven),
            Some(8) => Ok(DataBits::Eight),
            _ => Err("data_bits must be 5, 6, 7 or 8".to_string()),
        },
    }
}

fn stop_bits(raw: Raw<'_>) -> Result<StopBits, String> {
    match raw {
        Raw::Str("one") => Ok(StopBits::One),
        Raw::Str("two") => Ok(StopBits::Two),
        other => match other.unsigned() {
            Some(1) => Ok(StopBits::One),
            Some(2) => Ok(StopBits::Two),
            _ => Err("stop_bits must be 1 or 2".to_string()),
        },
    }
}

fn parity(raw: Raw<'_>) -> Result<Parity, String> {
    const EXPECTED: &str = "parity must be \"none\", \"odd\", \"even\", \"mark\" or \"space\"";
    match raw {
        Raw::Str("none" | "n" | "N") => Ok(Parity::None),
        Raw::Str("odd" | "o" | "O") => Ok(Parity::Odd),
        Raw::Str("even" | "e" | "E") => Ok(Parity::Even),
        Raw::Str("mark" | "m" | "M") => Ok(Parity::Mark),
        Raw::Str("space" | "s" | "S") => Ok(Parity::Space),
        _ => Err(EXPECTED.to_string()),
    }
}

fn flow_control(raw: Raw<'_>) -> Result<FlowControl, String> {
    match raw {
        Raw::Str("none") => Ok(FlowControl::None),
        Raw::Str("hardware" | "rtscts") => Ok(FlowControl::Hardware),
        Raw::Str("software" | "xonxoff") => Ok(FlowControl::Software),
        _ => Err("flow_control must be \"none\", \"hardware\" or \"software\"".to_string()),
    }
}

fn opt_data_bits<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DataBits>, D::Error> {
    optional_scalar(d, "5, 6, 7 or 8", data_bits)
}

fn opt_stop_bits<'de, D: Deserializer<'de>>(d: D) -> Result<Option<StopBits>, D::Error> {
    optional_scalar(d, "1 or 2", stop_bits)
}

fn opt_parity<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Parity>, D::Error> {
    optional_scalar(d, "a parity name", parity)
}

fn opt_flow_control<'de, D: Deserializer<'de>>(d: D) -> Result<Option<FlowControl>, D::Error> {
    optional_scalar(d, "a flow control name", flow_control)
}

/// Settings applied to a device when it matches. Keys left out fall back to the
/// global settings.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeviceProfile {
    /// Shown in the Devices panel in place of the USB product string.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "match")]
    pub r#match: DeviceMatch,
    #[serde(default, deserialize_with = "opt_baud_rate")]
    pub baud: Option<u32>,
    /// 5 to 8, or `"five"` to `"eight"`.
    #[serde(default, deserialize_with = "opt_data_bits")]
    pub data_bits: Option<DataBits>,
    #[serde(default, deserialize_with = "opt_parity")]
    pub parity: Option<Parity>,
    /// 1 or 2, or `"one"` or `"two"`.
    #[serde(default, deserialize_with = "opt_stop_bits")]
    pub stop_bits: Option<StopBits>,
    /// `"none"`, `"hardware"` (RTS/CTS) or `"software"` (XON/XOFF).
    #[serde(default, deserialize_with = "opt_flow_control")]
    pub flow_control: Option<FlowControl>,
    /// Name of the codec plugin to activate, such as `airoha-race`.
    #[serde(default)]
    pub plugin: Option<String>,
    /// The line ending Enter sends for this device.
    #[serde(default)]
    pub eol: Option<LineEnding>,
    /// A script to run when the device connects.
    #[serde(default)]
    pub on_connect: Option<PathBuf>,
}

impl DeviceProfile {
    /// Whether this profile applies to `port`.
    pub fn matches(&self, port: &PortInfo) -> bool {
        self.r#match.matches(port)
    }

    /// The line configuration this profile asks for, on top of `base`.
    pub fn apply_to(&self, base: &SerialConfig) -> SerialConfig {
        SerialConfig {
            baud: self.baud.unwrap_or(base.baud),
            data_bits: self.data_bits.unwrap_or(base.data_bits),
            parity: self.parity.unwrap_or(base.parity),
            stop_bits: self.stop_bits.unwrap_or(base.stop_bits),
            flow_control: self.flow_control.unwrap_or(base.flow_control),
        }
    }
}
