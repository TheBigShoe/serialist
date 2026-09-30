//! Serial line configuration, serializable so it can live in settings and device profiles.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataBits {
    Five,
    Six,
    Seven,
    Eight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Parity {
    None,
    Odd,
    Even,
    Mark,
    Space,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopBits {
    One,
    Two,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowControl {
    None,
    /// RTS/CTS.
    Hardware,
    /// XON/XOFF.
    Software,
}

/// Full line configuration. `baud` is any positive integer; non-standard rates are the
/// transport's problem, not the config's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SerialConfig {
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
}

impl Default for SerialConfig {
    fn default() -> Self {
        Self {
            baud: 115_200,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    }
}

impl SerialConfig {
    /// Bits on the wire per byte: start bit + data bits + parity bit + stop bits.
    pub fn bits_per_byte(&self) -> u32 {
        let data = match self.data_bits {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        };
        let parity = if matches!(self.parity, Parity::None) {
            0
        } else {
            1
        };
        let stop = match self.stop_bits {
            StopBits::One => 1,
            StopBits::Two => 2,
        };
        1 + data + parity + stop
    }

    /// Payload throughput implied by the line settings, in bytes per second.
    pub fn bytes_per_second(&self) -> f64 {
        f64::from(self.baud) / f64::from(self.bits_per_byte())
    }

    /// Short human form such as `115200 8N1`.
    pub fn summary(&self) -> String {
        let data = match self.data_bits {
            DataBits::Five => '5',
            DataBits::Six => '6',
            DataBits::Seven => '7',
            DataBits::Eight => '8',
        };
        let parity = match self.parity {
            Parity::None => 'N',
            Parity::Odd => 'O',
            Parity::Even => 'E',
            Parity::Mark => 'M',
            Parity::Space => 'S',
        };
        let stop = match self.stop_bits {
            StopBits::One => '1',
            StopBits::Two => '2',
        };
        format!("{} {data}{parity}{stop}", self.baud)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_115200_8n1() {
        let cfg = SerialConfig::default();
        assert_eq!(cfg.summary(), "115200 8N1");
        assert_eq!(cfg.bits_per_byte(), 10);
        assert!((cfg.bytes_per_second() - 11_520.0).abs() < f64::EPSILON);
    }
}
