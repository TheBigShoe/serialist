//! The Airoha RACE codec in Rust: the reference the Lua plugin
//! (`assets/plugins/airoha-race/plugin.lua`) is checked against byte for byte.
//!
//! # Wire format
//!
//! | Field | Size | Value |
//! |---|---|---|
//! | Sync | 1 byte | `0x05` |
//! | Type | 1 byte | `0x5A` command, `0x5B` response, `0x5C` indication, `0x5D` log |
//! | Length | u16 little-endian | payload length plus 2, at most [`MAX_LEN`] |
//! | Command id | u16 little-endian | for example `0x0F15`, query version and build time |
//! | Payload | length minus 2 bytes | command-specific |
//!
//! # Decoding
//!
//! A resynchronising framer. Every received byte ends up in exactly one frame, in
//! stream order, so nothing is hidden:
//!
//! - A `0x05` followed by a known type and a length of 2 to [`MAX_LEN`] starts a frame.
//!   Once all `4 + length` bytes are in (across as many chunks as it takes), it becomes a
//!   `command`, `response`, `indication` or `log` frame with fields `type`, `cmd_id`,
//!   `cmd_id_hex`, `payload` and `payload_len`.
//! - A `0x05` and a known type followed by a length outside that range is a `malformed`
//!   frame of those two bytes (severity warning); decoding resumes at the length bytes,
//!   which may themselves start a frame.
//! - A `0x05` followed by any other byte is ordinary data.
//! - Everything else is text: a `text` frame ends at a line feed (included) or after
//!   [`MAX_TEXT`] bytes, whichever comes first, or where a frame or malformed header
//!   starts. Its `text` field is the bytes without the line ending, rendered by
//!   [`render_text`].
//!
//! Bytes that could still turn out either way (a partial header, a partial frame, a
//! text line with no line feed yet) are held until later bytes decide. The result is
//! the same however the stream is split into chunks, apart from each frame's `at`.

use std::time::Instant;

use memchr::memchr2;
use serde_json::Value as JsonValue;
use serialist_core::codec::{
    Codec, CodecError, CodecInfo, CommandInfo, EncodeRequest, FieldInfo, FieldType, Frame,
    FrameKindInfo, Severity, encode_hex, parse_hex_uint,
};

/// The codec's registry name.
pub const NAME: &str = "airoha-race";
/// The codec's version; the Lua plugin reports the same.
pub const VERSION: &str = "1.0.0";
/// First byte of every frame.
pub const SYNC: u8 = 0x05;
/// Sync, type and length.
pub const HEADER_LEN: usize = 4;
/// Largest length field accepted: payload plus the two command-id bytes.
pub const MAX_LEN: usize = 4096;
/// Largest payload a frame carries.
pub const MAX_PAYLOAD: usize = MAX_LEN - 2;
/// Longest text frame; a longer run without a line feed is split.
pub const MAX_TEXT: usize = 1024;
/// Payload bytes shown in a frame's summary.
pub const PREVIEW_BYTES: usize = 16;
/// Query version and build time.
pub const VERSION_CMD_ID: u16 = 0x0F15;

/// The four frame types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RaceType {
    Command,
    Response,
    Indication,
    Log,
}

impl RaceType {
    pub const ALL: [RaceType; 4] = [
        RaceType::Command,
        RaceType::Response,
        RaceType::Indication,
        RaceType::Log,
    ];

    /// The type byte.
    pub fn byte(self) -> u8 {
        match self {
            RaceType::Command => 0x5A,
            RaceType::Response => 0x5B,
            RaceType::Indication => 0x5C,
            RaceType::Log => 0x5D,
        }
    }

    pub fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x5A => Some(RaceType::Command),
            0x5B => Some(RaceType::Response),
            0x5C => Some(RaceType::Indication),
            0x5D => Some(RaceType::Log),
            _ => None,
        }
    }

    /// The frame kind: `command`, `response`, `indication` or `log`.
    pub fn kind(self) -> &'static str {
        match self {
            RaceType::Command => "command",
            RaceType::Response => "response",
            RaceType::Indication => "indication",
            RaceType::Log => "log",
        }
    }

    pub fn from_kind(kind: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|ty| ty.kind() == kind)
    }
}

/// The bytes of one frame.
pub fn encode_frame(ty: RaceType, cmd_id: u16, payload: &[u8]) -> Result<Vec<u8>, CodecError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(CodecError::bad_field(
            "payload",
            format!(
                "{} bytes is more than the {MAX_PAYLOAD} a frame carries",
                payload.len()
            ),
        ));
    }
    let len = (payload.len() + 2) as u16;
    let mut frame = Vec::with_capacity(HEADER_LEN + 2 + payload.len());
    frame.push(SYNC);
    frame.push(ty.byte());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&cmd_id.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// A text frame's `text`: printable ASCII as it is; tab, CR, LF and backslash as `\t`,
/// `\r`, `\n` and `\\`; any other byte as `\xNN` (upper-case hex). Simple enough that a
/// plugin in any language can match it exactly.
pub fn render_text(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            0x20..=0x7E => out.push(b as char),
            _ => {
                out.push_str("\\x");
                out.push(DIGITS[usize::from(b >> 4)] as char);
                out.push(DIGITS[usize::from(b & 0x0F)] as char);
            }
        }
    }
    out
}

/// A frame's one-line summary: kind, command id, payload length and the first
/// [`PREVIEW_BYTES`] payload bytes.
fn summary(kind: &str, cmd_id: u16, payload: &[u8]) -> String {
    let mut s = format!("{kind} 0x{cmd_id:04X} len {}", payload.len());
    if !payload.is_empty() {
        s.push_str(": ");
        s.push_str(&encode_hex(
            &payload[..payload.len().min(PREVIEW_BYTES)],
            " ",
        ));
        if payload.len() > PREVIEW_BYTES {
            s.push_str(" ...");
        }
    }
    s
}

/// What the codec produces and accepts. The Lua plugin's `describe()` returns the same.
pub fn race_info() -> CodecInfo {
    let frame_fields = |kind: FrameKindInfo| {
        kind.field(FieldInfo::new("type", FieldType::UInt, "The type byte"))
            .field(FieldInfo::new("cmd_id", FieldType::UInt, "The command id"))
            .field(FieldInfo::new(
                "cmd_id_hex",
                FieldType::Str,
                "The command id as 0xNNNN",
            ))
            .field(FieldInfo::new(
                "payload",
                FieldType::Bytes,
                "The bytes after the command id",
            ))
            .field(FieldInfo::new(
                "payload_len",
                FieldType::UInt,
                "Payload length in bytes",
            ))
    };
    CodecInfo {
        name: NAME.into(),
        version: VERSION.into(),
        description: "Airoha RACE: 0x05-framed commands, responses, indications and logs, \
                      with the text between frames"
            .into(),
        kinds: vec![
            frame_fields(FrameKindInfo::new(
                "command",
                "A command to the device (type 0x5A)",
            )),
            frame_fields(FrameKindInfo::new(
                "response",
                "A response from the device (type 0x5B)",
            )),
            frame_fields(FrameKindInfo::new(
                "indication",
                "An unsolicited indication (type 0x5C)",
            )),
            frame_fields(FrameKindInfo::new("log", "Log data (type 0x5D)")),
            FrameKindInfo::new(
                "malformed",
                "A sync byte and a known type followed by an impossible length",
            )
            .field(FieldInfo::new("type", FieldType::UInt, "The type byte"))
            .field(FieldInfo::new(
                "len",
                FieldType::UInt,
                "The length field as received",
            ))
            .field(FieldInfo::new(
                "reason",
                FieldType::Str,
                "Why the header was rejected",
            )),
            FrameKindInfo::new(
                "text",
                "Bytes between frames, a line or 1024 bytes at a time",
            )
            .field(FieldInfo::new(
                "text",
                FieldType::Str,
                "Printable ASCII as is, other bytes escaped, the line ending dropped",
            )),
        ],
        commands: vec![
            CommandInfo::new("race", "Any RACE frame")
                .field(
                    FieldInfo::new(
                        "type",
                        FieldType::Str,
                        "command, response, indication or log, or a type byte; default command",
                    )
                    .optional(),
                )
                .field(FieldInfo::new(
                    "cmd_id",
                    FieldType::UInt,
                    "The command id: an integer or hex such as 0x0F15",
                ))
                .field(
                    FieldInfo::new(
                        "payload",
                        FieldType::Bytes,
                        "Hex text or a list of bytes; default empty",
                    )
                    .optional(),
                ),
            CommandInfo::new(
                "race_version",
                "Query version and build time (command 0x0F15, no payload)",
            ),
        ],
    }
}

/// The RACE codec. See the [module docs](self).
#[derive(Debug, Default)]
pub struct AirohaRace {
    /// The open text run and the stream offset of its first byte.
    text: Vec<u8>,
    text_start: u64,
    /// A candidate frame (starting with [`SYNC`]) waiting for more bytes, and its offset.
    frame: Vec<u8>,
    frame_start: u64,
}

impl AirohaRace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes held back, waiting for later ones to decide what they are.
    pub fn pending(&self) -> usize {
        self.text.len() + self.frame.len()
    }

    fn declared_len(&self) -> usize {
        usize::from(u16::from_le_bytes([self.frame[2], self.frame[3]]))
    }

    fn feed(&mut self, data: &[u8], offset: u64, at: Instant, out: &mut Vec<Frame>) {
        let mut i = 0;
        while i < data.len() {
            if self.frame.is_empty() {
                // Text up to the next sync byte or line feed.
                let rest = &data[i..];
                let start = offset + i as u64;
                match memchr2(SYNC, b'\n', rest) {
                    None => {
                        self.push_text(rest, start, at, out);
                        i = data.len();
                    }
                    Some(j) if rest[j] == b'\n' => {
                        self.push_text(&rest[..=j], start, at, out);
                        self.flush_text(at, out);
                        i += j + 1;
                    }
                    Some(j) => {
                        self.push_text(&rest[..j], start, at, out);
                        self.frame.push(SYNC);
                        self.frame_start = start + j as u64;
                        i += j + 1;
                    }
                }
                continue;
            }
            if self.frame.len() == 1 {
                if RaceType::from_byte(data[i]).is_none() {
                    // The sync byte starts no frame: it is text, and `data[i]` is looked
                    // at afresh.
                    self.frame.clear();
                    self.push_text(&[SYNC], self.frame_start, at, out);
                    continue;
                }
                self.frame.push(data[i]);
                i += 1;
                continue;
            }
            if self.frame.len() < HEADER_LEN {
                let take = (HEADER_LEN - self.frame.len()).min(data.len() - i);
                self.frame.extend_from_slice(&data[i..i + take]);
                i += take;
                if self.frame.len() < HEADER_LEN {
                    break;
                }
                let len = self.declared_len();
                if !(2..=MAX_LEN).contains(&len) {
                    let ty = self.frame[1];
                    let length_bytes = [self.frame[2], self.frame[3]];
                    let start = self.frame_start;
                    self.frame.clear();
                    self.flush_text(at, out);
                    out.push(malformed(ty, len, start, at));
                    // The length bytes are decoded again: they may start a frame.
                    self.feed(&length_bytes, start + 2, at, out);
                    continue;
                }
            }
            let total = HEADER_LEN + self.declared_len();
            let take = (total - self.frame.len()).min(data.len() - i);
            self.frame.extend_from_slice(&data[i..i + take]);
            i += take;
            if self.frame.len() == total {
                self.flush_text(at, out);
                out.push(self.complete(at));
                self.frame.clear();
            }
        }
    }

    /// Add text bytes starting at stream offset `start` to the open run, cutting a frame
    /// every [`MAX_TEXT`] bytes.
    fn push_text(&mut self, mut bytes: &[u8], mut start: u64, at: Instant, out: &mut Vec<Frame>) {
        while !bytes.is_empty() {
            if self.text.is_empty() {
                self.text_start = start;
            }
            let take = (MAX_TEXT - self.text.len()).min(bytes.len());
            self.text.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            start += take as u64;
            if self.text.len() == MAX_TEXT {
                self.flush_text(at, out);
            }
        }
    }

    fn flush_text(&mut self, at: Instant, out: &mut Vec<Frame>) {
        if self.text.is_empty() {
            return;
        }
        let raw = self.text_start..self.text_start + self.text.len() as u64;
        let mut body = &self.text[..];
        if let Some(rest) = body.strip_suffix(b"\n") {
            body = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let text = render_text(body);
        out.push(
            Frame::new("text", raw, at)
                .with_summary(text.clone())
                .with_field("text", text),
        );
        self.text.clear();
    }

    fn complete(&self, at: Instant) -> Frame {
        let ty = RaceType::from_byte(self.frame[1]).expect("a candidate has a known type");
        let cmd_id = u16::from_le_bytes([self.frame[4], self.frame[5]]);
        let payload = &self.frame[HEADER_LEN + 2..];
        let raw = self.frame_start..self.frame_start + self.frame.len() as u64;
        Frame::new(ty.kind(), raw, at)
            .with_summary(summary(ty.kind(), cmd_id, payload))
            .with_field("type", u64::from(ty.byte()))
            .with_field("cmd_id", u64::from(cmd_id))
            .with_field("cmd_id_hex", format!("0x{cmd_id:04X}"))
            .with_field("payload", payload)
            .with_field("payload_len", payload.len() as u64)
    }
}

fn malformed(ty: u8, len: usize, start: u64, at: Instant) -> Frame {
    let reason = format!("length {len} is outside 2..={MAX_LEN}");
    Frame::new("malformed", start..start + 2, at)
        .with_severity(Severity::Warning)
        .with_summary(format!("malformed 0x{ty:02X} header: {reason}"))
        .with_field("type", u64::from(ty))
        .with_field("len", len as u64)
        .with_field("reason", reason)
}

/// The `type` field of a `race` command: a kind name, or a type byte as an integer or hex.
fn type_field(request: &EncodeRequest) -> Result<RaceType, CodecError> {
    let ty = match request.field("type") {
        None => return Ok(RaceType::Command),
        Some(JsonValue::String(s)) => RaceType::from_kind(s).or_else(|| {
            parse_hex_uint(s)
                .and_then(|n| u8::try_from(n).ok())
                .and_then(RaceType::from_byte)
        }),
        Some(JsonValue::Number(n)) => n
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .and_then(RaceType::from_byte),
        Some(_) => None,
    };
    ty.ok_or_else(|| {
        CodecError::bad_field(
            "type",
            "must be command, response, indication or log, or a byte from 0x5A to 0x5D",
        )
    })
}

impl Codec for AirohaRace {
    fn describe(&self) -> CodecInfo {
        race_info()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        self.feed(chunk, raw_offset, at, out);
    }

    /// `race` builds any frame from `type` (default `command`), `cmd_id` and `payload`
    /// (default empty); `race_version` is the `0x0F15` query. Fields are checked in a
    /// fixed order (unknown fields, then `type`, `cmd_id`, `payload`) so the first error
    /// is the same in every implementation.
    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        match request.command.as_str() {
            "race" => {
                request.check_fields(&["cmd_id", "payload", "type"])?;
                let ty = type_field(request)?;
                let cmd_id = request
                    .uint("cmd_id", u64::from(u16::MAX))?
                    .ok_or_else(|| CodecError::MissingField("cmd_id".into()))?;
                let payload = request.bytes("payload")?.unwrap_or_default();
                encode_frame(ty, cmd_id as u16, &payload)
            }
            "race_version" => {
                request.check_fields(&[])?;
                encode_frame(RaceType::Command, VERSION_CMD_ID, &[])
            }
            other => Err(CodecError::UnknownCommand(other.to_owned())),
        }
    }

    fn reset(&mut self) {
        self.text.clear();
        self.frame.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use serialist_core::Value;

    fn decode_all(chunks: &[&[u8]]) -> Vec<Frame> {
        let mut codec = AirohaRace::new();
        let at = Instant::now();
        let mut out = Vec::new();
        let mut offset = 0;
        for chunk in chunks {
            codec.decode(chunk, at, offset, &mut out);
            offset += chunk.len() as u64;
        }
        out
    }

    fn kinds(frames: &[Frame]) -> Vec<(&str, std::ops::Range<u64>)> {
        frames
            .iter()
            .map(|f| (f.kind.as_str(), f.raw.clone()))
            .collect()
    }

    #[test]
    fn decodes_a_version_response() {
        let frame = [0x05, 0x5B, 0x05, 0x00, 0x15, 0x0F, 0x00, 0x01, 0x02];
        let frames = decode_all(&[&frame]);
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(f.kind, "response");
        assert_eq!(f.raw, 0..9);
        assert_eq!(f.field("cmd_id"), Some(&Value::UInt(0x0F15)));
        assert_eq!(f.field("cmd_id_hex"), Some(&Value::Str("0x0F15".into())));
        assert_eq!(f.field("payload"), Some(&Value::Bytes(vec![0, 1, 2])));
        assert_eq!(f.field("payload_len"), Some(&Value::UInt(3)));
        assert_eq!(f.field("type"), Some(&Value::UInt(0x5B)));
        assert_eq!(f.summary, "response 0x0F15 len 3: 00 01 02");
    }

    #[test]
    fn waits_for_frames_split_across_chunks() {
        let frame = encode_frame(RaceType::Log, 0x0F40, b"hello").unwrap();
        for split in 0..=frame.len() {
            let frames = decode_all(&[&frame[..split], &frame[split..]]);
            assert_eq!(kinds(&frames), [("log", 0..11)], "split at {split}");
        }
    }

    #[test]
    fn text_between_frames_is_kept_as_text_frames() {
        let mut stream = b"boot ok\r\nready> ".to_vec();
        stream.extend_from_slice(&encode_frame(RaceType::Indication, 1, &[]).unwrap());
        stream.extend_from_slice(b"tail\\\t\x01\n");
        let frames = decode_all(&[&stream]);
        assert_eq!(
            kinds(&frames),
            [
                ("text", 0..9),
                ("text", 9..16),
                ("indication", 16..22),
                ("text", 22..30)
            ]
        );
        let texts: Vec<_> = frames
            .iter()
            .filter_map(|f| f.field("text").and_then(Value::as_str))
            .collect();
        assert_eq!(texts, ["boot ok", "ready> ", "tail\\\\\\t\\x01"]);
    }

    #[test]
    fn bad_lengths_are_malformed_and_decoding_resumes_after_the_type() {
        // 05 5A 05 5A: length 0x5A05 is too long; the length bytes start a real frame.
        let mut stream = vec![0x05, 0x5A];
        stream.extend_from_slice(&encode_frame(RaceType::Command, 0x0F15, &[]).unwrap());
        let frames = decode_all(&[&stream]);
        assert_eq!(kinds(&frames), [("malformed", 0..2), ("command", 2..8)]);
        assert_eq!(frames[0].severity, Severity::Warning);
        assert_eq!(frames[0].field("len"), Some(&Value::UInt(0x5A05)));
        assert_eq!(
            frames[0].summary,
            "malformed 0x5A header: length 23045 is outside 2..=4096"
        );
        // A length of 1 is too short.
        let frames = decode_all(&[&[0x05, 0x5D, 0x01, 0x00, b'x', b'\n']]);
        assert_eq!(kinds(&frames), [("malformed", 0..2), ("text", 2..6)]);
    }

    #[test]
    fn a_sync_byte_with_an_unknown_type_is_text() {
        let frames = decode_all(&[b"a\x05", b"\x05\x5A\x02\x00\x01\x00b\n"]);
        assert_eq!(
            kinds(&frames),
            [("text", 0..2), ("command", 2..8), ("text", 8..10)]
        );
        assert_eq!(frames[0].field("text").unwrap().as_str(), Some("a\\x05"));
    }

    #[test]
    fn long_text_is_cut_every_max_text_bytes() {
        let stream = vec![b'x'; MAX_TEXT * 2 + 10];
        let frames = decode_all(&[&stream]);
        assert_eq!(
            kinds(&frames),
            [("text", 0..1024), ("text", 1024..2048)],
            "the last 10 bytes wait for a line feed"
        );
    }

    #[test]
    fn reset_drops_what_was_held_back() {
        let mut codec = AirohaRace::new();
        let mut out = Vec::new();
        let at = Instant::now();
        codec.decode(b"abc\x05\x5A\x10", at, 0, &mut out);
        assert_eq!(codec.pending(), 6);
        codec.reset();
        assert_eq!(codec.pending(), 0);
        codec.decode(b"\n", at, 6, &mut out);
        assert_eq!(kinds(&out), [("text", 6..7)]);
    }

    fn encode(request: EncodeRequest) -> Result<Vec<u8>, CodecError> {
        AirohaRace::new().encode(&request)
    }

    #[test]
    fn encodes_frames_from_fields() {
        let version = vec![0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F];
        assert_eq!(
            encode(EncodeRequest::new("race_version")),
            Ok(version.clone())
        );
        assert_eq!(
            encode(EncodeRequest::new("race").with("cmd_id", "0x0F15")),
            Ok(version.clone())
        );
        assert_eq!(
            encode(
                EncodeRequest::new("race")
                    .with("type", "command")
                    .with("cmd_id", 3861)
            ),
            Ok(version)
        );
        assert_eq!(
            encode(
                EncodeRequest::new("race")
                    .with("type", 0x5D)
                    .with("cmd_id", "0f40")
                    .with("payload", "01 02")
            ),
            Ok(vec![0x05, 0x5D, 0x04, 0x00, 0x40, 0x0F, 0x01, 0x02])
        );
        assert_eq!(
            encode(
                EncodeRequest::new("race")
                    .with("type", "0x5B")
                    .with("cmd_id", 1)
                    .with("payload", json!([255]))
            ),
            Ok(vec![0x05, 0x5B, 0x03, 0x00, 0x01, 0x00, 0xFF])
        );
    }

    #[test]
    fn encode_errors_come_in_a_fixed_order() {
        assert_eq!(
            encode(EncodeRequest::new("nope")),
            Err(CodecError::UnknownCommand("nope".into()))
        );
        assert_eq!(
            encode(EncodeRequest::new("race")),
            Err(CodecError::MissingField("cmd_id".into()))
        );
        let bad = |r: EncodeRequest| match encode(r) {
            Err(CodecError::BadField { field, .. }) => field,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            bad(EncodeRequest::new("race")
                .with("zz", 1)
                .with("aa", 1)
                .with("type", "x")),
            "aa"
        );
        assert_eq!(
            bad(EncodeRequest::new("race")
                .with("type", "x")
                .with("cmd_id", -1)),
            "type"
        );
        assert_eq!(
            bad(EncodeRequest::new("race")
                .with("type", 0x5E)
                .with("cmd_id", 1)),
            "type"
        );
        assert_eq!(
            bad(EncodeRequest::new("race").with("cmd_id", 0x10000)),
            "cmd_id"
        );
        assert_eq!(
            bad(EncodeRequest::new("race")
                .with("cmd_id", 1)
                .with("payload", "0")),
            "payload"
        );
        assert_eq!(
            bad(EncodeRequest::new("race")
                .with("cmd_id", 1)
                .with("payload", json!(vec![0; MAX_PAYLOAD + 1]))),
            "payload"
        );
        assert_eq!(
            bad(EncodeRequest::new("race_version").with("cmd_id", 1)),
            "cmd_id"
        );
    }
}
