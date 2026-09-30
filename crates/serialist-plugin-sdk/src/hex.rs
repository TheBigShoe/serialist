//! Hex text, exactly as Serialist's built-in codecs read and write it, so a plugin's
//! fields and errors match theirs.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Bytes as upper-case hex pairs joined by `separator`: `encode(b"\x05\x5a", " ")` is
/// `"05 5A"`.
pub fn encode(bytes: &[u8], separator: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * (2 + separator.len()));
    for (i, &byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0F)] as char);
    }
    out
}

/// Why hex text did not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HexError {
    /// A character that is neither a hex digit nor a separator.
    BadDigit(char),
    /// A run of digits of odd length.
    OddDigits(String),
}

impl fmt::Display for HexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HexError::BadDigit(c) => write!(f, "{c:?} is not a hex digit"),
            HexError::OddDigits(run) => write!(f, "{run:?} has an odd number of hex digits"),
        }
    }
}

impl core::error::Error for HexError {}

/// Hex text to bytes: runs of digit pairs separated by spaces, tabs, CR, LF or commas,
/// each run optionally prefixed `0x`, each run an even number of digits. `"05 5A 00"`,
/// `"055A00"` and `"0x05,0x5a,0x00"` all decode to `[0x05, 0x5A, 0x00]`.
pub fn decode(text: &str) -> Result<Vec<u8>, HexError> {
    let mut out = Vec::with_capacity(text.len() / 2);
    for run in text.split([' ', '\t', '\r', '\n', ',']) {
        let digits = strip_0x(run);
        if let Some(bad) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(HexError::BadDigit(bad));
        }
        if !digits.len().is_multiple_of(2) {
            return Err(HexError::OddDigits(run.into()));
        }
        let (pairs, _) = digits.as_bytes().as_chunks::<2>();
        for &[hi, lo] in pairs {
            out.push(digit(hi) << 4 | digit(lo));
        }
    }
    Ok(out)
}

/// `"0x0F15"`, `"0X0f15"` or `"0F15"` as a number. No sign, no spaces, at least one
/// digit; leading zeros are fine. `None` if it is not that or does not fit a `u64`.
pub fn parse_uint(text: &str) -> Option<u64> {
    let digits = strip_0x(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn strip_0x(text: &str) -> &str {
    text.strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text)
}

fn digit(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    #[test]
    fn round_trips_and_accepts_every_form() {
        let bytes = [0x05, 0x5A, 0x00, 0xFF];
        assert_eq!(encode(&bytes, " "), "05 5A 00 FF");
        assert_eq!(encode(&bytes, ""), "055A00FF");
        for text in [
            "05 5A 00 FF",
            "055A00ff",
            "0x05,0x5a, 0x00\n0XfF",
            "  05\t5A00 FF\r",
        ] {
            assert_eq!(decode(text).unwrap(), bytes, "{text:?}");
        }
        assert_eq!(decode("").unwrap(), [0u8; 0]);
        assert_eq!(decode("05 5"), Err(HexError::OddDigits("5".into())));
        assert_eq!(decode("0G"), Err(HexError::BadDigit('G')));
        assert_eq!(decode("05\u{a0}5A"), Err(HexError::BadDigit('\u{a0}')));
        assert_eq!(
            HexError::BadDigit('G').to_string(),
            "'G' is not a hex digit"
        );
    }

    #[test]
    fn parses_hex_numbers() {
        for text in ["0x0F15", "0X0f15", "0F15", "000000000000000000000F15"] {
            assert_eq!(parse_uint(text), Some(0x0F15), "{text}");
        }
        for text in ["", "0x", "+1", " 1", "g", "10000000000000000"] {
            assert_eq!(parse_uint(text), None, "{text}");
        }
    }
}
