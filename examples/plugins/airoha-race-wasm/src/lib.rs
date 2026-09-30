//! The Airoha RACE codec as a Serialist WebAssembly plugin (tier 2).
//!
//! It must agree byte for byte with the Rust codec (`serialist-plugins/src/race.rs`) and
//! the Lua plugin (`serialist-plugins/assets/plugins/airoha-race/plugin.lua`): the same
//! description, the same frames however the stream is cut into chunks, and the same
//! bytes or error for every encode request. `serialist-plugins/tests/conformance.rs`
//! checks it against the Rust codec with the `wasm` feature. It shares no code with
//! either; the framer is written again here against the SDK, from the wire format:
//!
//! `[0x05][type: u8][len: u16 LE][cmd_id: u16 LE][payload]`, `len = payload + 2`, with
//! types 0x5A command, 0x5B response, 0x5C indication and 0x5D log.
//!
//! Like the Lua plugin it keeps no state: whatever is not decided yet (a partial frame,
//! or a text line with no line feed) is held back, and the host presents it again with
//! the next bytes.
//!
//! # Building
//!
//! ```text
//! rustup target add wasm32-wasip2
//! cargo build -p airoha-race-wasm --target wasm32-wasip2 --profile wasm-plugin
//! ```
//!
//! The component is `target/wasm32-wasip2/wasm-plugin/airoha_race_wasm.wasm`. A plugin
//! folder, as `serialist_plugins::load_plugins` reads it, holds it as `plugin.wasm` next
//! to this crate's `plugin.toml`. `just wasm-fixtures` rebuilds the copy the host's tests
//! load (`serialist-plugins/tests/fixtures/plugins/airoha-race-wasm/`).

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use serialist_plugin_sdk::{
    CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Json, Plugin,
    Request, Severity, hex,
};

const SYNC: u8 = 0x05;
const HEADER_LEN: usize = 4;
/// Largest length field: payload plus the two command-id bytes.
const MAX_LEN: usize = 4096;
const MAX_PAYLOAD: usize = MAX_LEN - 2;
/// Longest text frame; a longer run without a line feed is cut.
const MAX_TEXT: usize = 1024;
/// Payload bytes shown in a summary.
const PREVIEW: usize = 16;
const VERSION_CMD_ID: u16 = 0x0F15;
const COMMAND: u8 = 0x5A;

/// The frame kind of a type byte.
fn kind_of(ty: u8) -> Option<&'static str> {
    match ty {
        0x5A => Some("command"),
        0x5B => Some("response"),
        0x5C => Some("indication"),
        0x5D => Some("log"),
        _ => None,
    }
}

/// The type byte of a kind name.
fn type_of(kind: &str) -> Option<u8> {
    match kind {
        "command" => Some(0x5A),
        "response" => Some(0x5B),
        "indication" => Some(0x5C),
        "log" => Some(0x5D),
        _ => None,
    }
}

pub struct AirohaRace;

serialist_plugin_sdk::export_plugin!(AirohaRace);

impl Plugin for AirohaRace {
    fn new() -> Self {
        AirohaRace
    }

    fn describe(&self) -> CodecInfo {
        let frame_kind = |kind: &str, description: &str| {
            FrameKindInfo::new(kind, description)
                .with_field(FieldInfo::new("type", FieldType::Uint, "The type byte"))
                .with_field(FieldInfo::new("cmd_id", FieldType::Uint, "The command id"))
                .with_field(FieldInfo::new(
                    "cmd_id_hex",
                    FieldType::Str,
                    "The command id as 0xNNNN",
                ))
                .with_field(FieldInfo::new(
                    "payload",
                    FieldType::Bytes,
                    "The bytes after the command id",
                ))
                .with_field(FieldInfo::new(
                    "payload_len",
                    FieldType::Uint,
                    "Payload length in bytes",
                ))
        };
        CodecInfo::new(
            "airoha-race",
            "1.0.0",
            "Airoha RACE: 0x05-framed commands, responses, indications and logs, with the \
             text between frames",
        )
        .with_kind(frame_kind("command", "A command to the device (type 0x5A)"))
        .with_kind(frame_kind(
            "response",
            "A response from the device (type 0x5B)",
        ))
        .with_kind(frame_kind(
            "indication",
            "An unsolicited indication (type 0x5C)",
        ))
        .with_kind(frame_kind("log", "Log data (type 0x5D)"))
        .with_kind(
            FrameKindInfo::new(
                "malformed",
                "A sync byte and a known type followed by an impossible length",
            )
            .with_field(FieldInfo::new("type", FieldType::Uint, "The type byte"))
            .with_field(FieldInfo::new(
                "len",
                FieldType::Uint,
                "The length field as received",
            ))
            .with_field(FieldInfo::new(
                "reason",
                FieldType::Str,
                "Why the header was rejected",
            )),
        )
        .with_kind(
            FrameKindInfo::new(
                "text",
                "Bytes between frames, a line or 1024 bytes at a time",
            )
            .with_field(FieldInfo::new(
                "text",
                FieldType::Str,
                "Printable ASCII as is, other bytes escaped, the line ending dropped",
            )),
        )
        .with_command(
            CommandInfo::new("race", "Any RACE frame")
                .with_field(
                    FieldInfo::new(
                        "type",
                        FieldType::Str,
                        "command, response, indication or log, or a type byte; default command",
                    )
                    .optional(),
                )
                .with_field(FieldInfo::new(
                    "cmd_id",
                    FieldType::Uint,
                    "The command id: an integer or hex such as 0x0F15",
                ))
                .with_field(
                    FieldInfo::new(
                        "payload",
                        FieldType::Bytes,
                        "Hex text or a list of bytes; default empty",
                    )
                    .optional(),
                ),
        )
        .with_command(CommandInfo::new(
            "race_version",
            "Query version and build time (command 0x0F15, no payload)",
        ))
    }

    fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> usize {
        let mut scan = Scan {
            input,
            frames,
            text: None,
        };
        let n = input.len();
        let mut i = 0;
        while i < n {
            if input[i] != SYNC {
                // Text up to the next sync byte or line feed.
                match input[i..].iter().position(|&b| b == SYNC || b == b'\n') {
                    None => {
                        scan.add_text(i, n);
                        i = n;
                    }
                    Some(j) if input[i + j] == b'\n' => {
                        scan.add_text(i, i + j + 1);
                        scan.flush_text();
                        i += j + 1;
                    }
                    Some(j) => {
                        scan.add_text(i, i + j);
                        i += j;
                    }
                }
                continue;
            }
            let Some(&ty) = input.get(i + 1) else {
                break; // wait for the type byte
            };
            let Some(kind) = kind_of(ty) else {
                // Not a frame: the sync byte is text.
                scan.add_text(i, i + 1);
                i += 1;
                continue;
            };
            if i + HEADER_LEN > n {
                break; // wait for the length
            }
            let len = usize::from(u16::from_le_bytes([input[i + 2], input[i + 3]]));
            if !(2..=MAX_LEN).contains(&len) {
                scan.flush_text();
                scan.frames.push(malformed(ty, len, i));
                i += 2; // the length bytes are scanned again
                continue;
            }
            let end = i + HEADER_LEN + len;
            if end > n {
                break; // wait for the rest of the frame
            }
            scan.flush_text();
            scan.frames.push(race_frame(kind, &input[i..end], i));
            i = end;
        }
        n - scan.text.map_or(i, |(start, _)| start)
    }

    fn encode(&mut self, request: &Request<'_>) -> Result<Vec<u8>, CodecError> {
        match request.command() {
            "race" => {
                request.check_fields(&["cmd_id", "payload", "type"])?;
                let ty = type_field(request)?;
                let cmd_id = request
                    .uint("cmd_id", u64::from(u16::MAX))?
                    .ok_or_else(|| CodecError::missing_field("cmd_id"))?;
                let payload = request.bytes("payload")?.unwrap_or_default();
                encode_frame(ty, cmd_id as u16, &payload)
            }
            "race_version" => {
                request.check_fields(&[])?;
                encode_frame(COMMAND, VERSION_CMD_ID, &[])
            }
            other => Err(CodecError::unknown_command(other)),
        }
    }
}

/// One `decode` call's scan: the frames so far and the open text run.
struct Scan<'a> {
    input: &'a [u8],
    frames: &'a mut Vec<Frame>,
    /// The open text run, `start..end` of the input.
    text: Option<(usize, usize)>,
}

impl Scan<'_> {
    /// Extend the open run with `first..end` (which follows it), cutting a frame every
    /// [`MAX_TEXT`] bytes.
    fn add_text(&mut self, first: usize, end: usize) {
        let mut start = self.text.map_or(first, |(start, _)| start);
        while end - start >= MAX_TEXT {
            self.text_frame(start, start + MAX_TEXT);
            start += MAX_TEXT;
        }
        self.text = (start < end).then_some((start, end));
    }

    fn flush_text(&mut self) {
        if let Some((start, end)) = self.text.take() {
            self.text_frame(start, end);
        }
    }

    fn text_frame(&mut self, start: usize, end: usize) {
        let mut body = &self.input[start..end];
        if let Some(rest) = body.strip_suffix(b"\n") {
            body = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let text = render_text(body);
        self.frames.push(
            Frame::new("text", start, end - start)
                .with_summary(text.clone())
                .with_field("text", text),
        );
    }
}

/// Printable ASCII as is; tab, CR, LF and backslash as `\t`, `\r`, `\n` and `\\`; any
/// other byte as `\xNN`.
fn render_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\r' => out.push_str("\\r"),
            b'\n' => out.push_str("\\n"),
            0x20..=0x7E => out.push(char::from(b)),
            _ => {
                out.push_str("\\x");
                out.push_str(&hex::encode(&[b], ""));
            }
        }
    }
    out
}

/// A complete frame, `bytes` from its sync byte, starting at `offset`.
fn race_frame(kind: &str, bytes: &[u8], offset: usize) -> Frame {
    let cmd_id = u16::from_le_bytes([bytes[4], bytes[5]]);
    let payload = &bytes[HEADER_LEN + 2..];
    let mut summary = format!("{kind} 0x{cmd_id:04X} len {}", payload.len());
    if !payload.is_empty() {
        summary.push_str(": ");
        summary.push_str(&hex::encode(&payload[..payload.len().min(PREVIEW)], " "));
        if payload.len() > PREVIEW {
            summary.push_str(" ...");
        }
    }
    Frame::new(kind, offset, bytes.len())
        .with_summary(summary)
        .with_field("type", bytes[1])
        .with_field("cmd_id", cmd_id)
        .with_field("cmd_id_hex", format!("0x{cmd_id:04X}"))
        .with_field("payload", payload)
        .with_field("payload_len", payload.len())
}

/// The sync and type bytes of a header whose length is impossible.
fn malformed(ty: u8, len: usize, offset: usize) -> Frame {
    let reason = format!("length {len} is outside 2..={MAX_LEN}");
    Frame::new("malformed", offset, 2)
        .with_severity(Severity::Warning)
        .with_summary(format!("malformed 0x{ty:02X} header: {reason}"))
        .with_field("type", ty)
        .with_field("len", len)
        .with_field("reason", reason)
}

/// The `type` field of a `race` command: a kind name, or a type byte as an integer or hex.
fn type_field(request: &Request<'_>) -> Result<u8, CodecError> {
    let ty = match request.field("type") {
        None => return Ok(COMMAND),
        Some(Json::Str(s)) => type_of(s).or_else(|| {
            hex::parse_uint(s)
                .and_then(|n| u8::try_from(n).ok())
                .filter(|&b| kind_of(b).is_some())
        }),
        Some(value @ (Json::Int(_) | Json::UInt(_) | Json::Float(_))) => value
            .as_u64()
            .and_then(|n| u8::try_from(n).ok())
            .filter(|&b| kind_of(b).is_some()),
        Some(_) => None,
    };
    ty.ok_or_else(|| {
        CodecError::bad_field(
            "type",
            "must be command, response, indication or log, or a byte from 0x5A to 0x5D",
        )
    })
}

fn encode_frame(ty: u8, cmd_id: u16, payload: &[u8]) -> Result<Vec<u8>, CodecError> {
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
    frame.push(ty);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&cmd_id.to_le_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialist_plugin_sdk::Value;

    fn decode(chunks: &[&[u8]]) -> (Vec<(String, core::ops::Range<usize>)>, usize) {
        let mut race = AirohaRace::new();
        let mut held = Vec::new();
        let mut base = 0;
        let mut out = Vec::new();
        for chunk in chunks {
            let mut input = core::mem::take(&mut held);
            input.extend_from_slice(chunk);
            let mut frames = Vec::new();
            let keep = race.decode(&input, &mut frames);
            out.extend(frames.into_iter().map(|f| {
                let range = f.range();
                (f.kind, base + range.start..base + range.end)
            }));
            base += input.len() - keep;
            held = input[input.len() - keep..].to_vec();
        }
        (out, held.len())
    }

    #[test]
    fn frames_split_anywhere_decode_the_same() {
        let frame = encode_frame(0x5D, 0x0F40, b"hello").unwrap();
        for split in 0..=frame.len() {
            let (frames, held) = decode(&[&frame[..split], &frame[split..]]);
            assert_eq!(frames, [("log".into(), 0..11)], "split at {split}");
            assert_eq!(held, 0);
        }
    }

    #[test]
    fn text_malformed_headers_and_frames() {
        let mut stream = b"boot\r\n\x05\x5A\x05\x5A".to_vec();
        stream.extend_from_slice(&encode_frame(COMMAND, 0x0F15, &[]).unwrap()[2..]);
        stream.extend_from_slice(b"tail");
        let (frames, held) = decode(&[&stream]);
        let kinds: Vec<_> = frames
            .iter()
            .map(|(k, r)| (k.as_str(), r.clone()))
            .collect();
        assert_eq!(
            kinds,
            [("text", 0..6), ("malformed", 6..8), ("command", 8..14)]
        );
        assert_eq!(held, 4, "the unterminated text waits");
    }

    #[test]
    fn frames_carry_the_documented_fields() {
        let mut frames = Vec::new();
        AirohaRace.decode(&[0x05, 0x5B, 0x05, 0x00, 0x15, 0x0F, 0, 1, 2], &mut frames);
        let f = &frames[0];
        assert_eq!(f.summary, "response 0x0F15 len 3: 00 01 02");
        assert_eq!(f.field("cmd_id"), Some(&Value::UInt(0x0F15)));
        assert_eq!(f.field("payload"), Some(&Value::Bytes(vec![0, 1, 2])));
        assert_eq!(f.field("cmd_id_hex"), Some(&Value::Str("0x0F15".into())));
    }
}
