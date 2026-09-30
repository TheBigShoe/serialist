//! Hex text to bytes and back, for `port:write_hex`, `hex.encode` and `hex.decode`.
//!
//! Decoding reads the same forms as a saved command's hex payload: pairs of digits,
//! separated or not by spaces, commas or newlines, each run optionally prefixed `0x`,
//! each run an even number of digits (`0F`, not `F`).

/// Why hex text did not decode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HexError {
    #[error("{0:?} is not a hex digit")]
    BadDigit(char),
    #[error("{0:?} has an odd number of hex digits")]
    OddDigits(String),
}

/// Bytes as upper-case hex pairs joined by `separator`: `encode_hex(b"\x05\x5a", " ")`
/// is `"05 5A"`.
pub fn encode_hex(bytes: &[u8], separator: &str) -> String {
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

/// `"05 5A 00"`, `"055A00"` and `"0x05,0x5a,0x00"` all decode to `[0x05, 0x5A, 0x00]`.
pub fn decode_hex(text: &str) -> Result<Vec<u8>, HexError> {
    let mut out = Vec::with_capacity(text.len() / 2);
    for run in text.split(|c: char| c.is_whitespace() || c == ',') {
        let digits = run
            .strip_prefix("0x")
            .or_else(|| run.strip_prefix("0X"))
            .unwrap_or(run);
        if let Some(bad) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
            return Err(HexError::BadDigit(bad));
        }
        if digits.len() % 2 != 0 {
            return Err(HexError::OddDigits(run.to_owned()));
        }
        let (pairs, _) = digits.as_bytes().as_chunks::<2>();
        for [hi, lo] in pairs {
            let hi = (*hi as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (*lo as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hi << 4 | lo);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_accepts_every_form() {
        let bytes = [0x05, 0x5A, 0x00, 0xFF];
        assert_eq!(encode_hex(&bytes, " "), "05 5A 00 FF");
        assert_eq!(encode_hex(&bytes, ""), "055A00FF");
        assert_eq!(encode_hex(&[], " "), "");
        for text in [
            "05 5A 00 FF",
            "055A00ff",
            "0x05,0x5a, 0x00\n0XfF",
            "  05  5A00 FF  ",
        ] {
            assert_eq!(decode_hex(text).unwrap(), bytes, "{text:?}");
        }
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn rejects_odd_runs_and_bad_digits() {
        assert_eq!(decode_hex("05 5"), Err(HexError::OddDigits("5".into())));
        assert_eq!(decode_hex("0G"), Err(HexError::BadDigit('G')));
        assert_eq!(decode_hex("05-5A"), Err(HexError::BadDigit('-')));
    }
}
