//! `race_rust`: the Rust Airoha RACE codec ([`AirohaRace`]), decoding and encoding, on
//! arbitrary bytes in arbitrary chunks. It is the reference the Lua and WebAssembly
//! plugins are held to, so it gets the strictest checks.
//!
//! The input is an [`Input`]. Config bit 0 picks the mode; config 0, decode, is the one
//! a real session exercises:
//!
//! - **Bit 0 off, decode.** The stream is decoded in the chunks the input says, and as
//!   one chunk.
//! - **Bit 0 on, encode.** The stream is a JSON object, a saved command's fields (see
//!   [`codec::encode_request`]); the chunk lengths are ignored. Anything that is not
//!   such an object is skipped, so seeds in this mode start `\x01\x00{`.
//!
//! # Decode checks
//!
//! Every call goes through [`codec::decode_chunks`], which holds it to the `Codec`
//! contract (`out` never cleared, `raw` ranges in bounds and in order, `at` the
//! chunk's time, only the kinds `describe` lists). On top of that:
//!
//! - Chunking does not matter: the frames of the chunked run equal those of the one-chunk
//!   run ([`codec::timeless`], since `at` differs), and both hold back the same number of
//!   bytes ([`AirohaRace::pending`]).
//! - No byte is lost or counted twice: the frames' raw ranges tile the stream up to the
//!   held-back bytes ([`codec::assert_tiled`]), checked after every chunk as well as at
//!   the end.
//! - Held-back bytes are bounded. After every chunk [`AirohaRace::pending`] is at most
//!   [`MAX_PENDING`]: an open text run is cut as soon as it reaches [`MAX_TEXT`] bytes, so
//!   it holds at most `MAX_TEXT - 1`; a frame candidate is complete, and becomes a
//!   frame, as soon as it holds `HEADER_LEN + length` bytes with `length` at most
//!   [`MAX_LEN`], so it holds at most `HEADER_LEN + MAX_LEN - 1`; and the two are held at
//!   once (text, then a sync byte starts a candidate that has not finished). Both ends
//!   are reachable, though not with libFuzzer's default 4 KiB input: the unit test
//!   `the_pending_bound_is_reached` builds the input.
//! - Every frame agrees with its own bytes in the stream: a RACE frame is a sync byte,
//!   its type, a length that matches, the command id and the payload, its fields say the
//!   same, and `encode_frame` rebuilds the bytes exactly; a malformed frame is a sync
//!   byte and a known type with a length field (the next two bytes) outside `2..=MAX_LEN`;
//!   a text frame is at most [`MAX_TEXT`] bytes with a line feed only at its end, no
//!   sync byte followed by a type (that would have been a frame), ends at a line feed, at
//!   [`MAX_TEXT`] bytes or where a frame begins, and its `text` is [`render_text`] of its
//!   bytes without the line ending.
//! - `reset` forgets everything: afterwards nothing is held back, and decoding the same
//!   chunks again from offset 0 gives the very same frames (`at` included).
//!
//! # Encode checks
//!
//! Whatever the request, `encode` does not panic and gives the same answer from a fresh
//! codec. When it succeeds the bytes are one whole frame: at most `HEADER_LEN + MAX_LEN`
//! long, starting with the sync byte, and decoding them gives exactly one frame covering
//! all of them whose kind is the one the type byte names (`race.rs` promises this: every
//! length `encode_frame` writes is `payload + 2`, and a payload over `MAX_PAYLOAD` is
//! refused).

use std::time::Instant;

use serialist_core::codec::{Codec, CodecError, CodecInfo, EncodeRequest, Frame, Severity, Value};
use serialist_plugins::AirohaRace;
use serialist_plugins::race::{
    HEADER_LEN, MAX_LEN, MAX_TEXT, RaceType, SYNC, encode_frame, render_text,
};

use crate::{Input, codec};

/// The most bytes [`AirohaRace::pending`] can be after a chunk: an open text run of
/// `MAX_TEXT - 1` and an unfinished frame of `HEADER_LEN + MAX_LEN - 1`. See the module
/// docs for why both are held together.
pub const MAX_PENDING: usize = (MAX_TEXT - 1) + (HEADER_LEN + MAX_LEN - 1);

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    if input.flag(0) {
        encode(input.stream);
    } else {
        decode(&input);
    }
}

/// An [`AirohaRace`] that checks what it holds back after every call, which only the
/// concrete type can say.
struct Probe(AirohaRace);

impl Codec for Probe {
    fn describe(&self) -> CodecInfo {
        self.0.describe()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        self.0.decode(chunk, at, raw_offset, out);
        let received = raw_offset + chunk.len() as u64;
        let pending = self.0.pending();
        assert!(
            pending <= MAX_PENDING,
            "{pending} bytes held back, past the bound of {MAX_PENDING}"
        );
        // `out[0]` is the harness's sentinel (0..0), so the last frame always exists.
        let decoded = out.last().expect("the sentinel").raw.end;
        assert_eq!(
            decoded + pending as u64,
            received,
            "the frames end at {decoded} and {pending} bytes are held back, \
             but {received} bytes have arrived"
        );
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        self.0.encode(request)
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

fn decode(input: &Input) {
    let t0 = Instant::now();
    let stream = input.stream;

    let mut split = Probe(AirohaRace::new());
    let split_frames = codec::decode_chunks(&mut split, input.chunks(), t0);
    let mut whole = Probe(AirohaRace::new());
    let whole_frames = codec::decode_chunks(&mut whole, std::iter::once(stream), t0);

    assert_eq!(
        codec::timeless(&split_frames, t0),
        codec::timeless(&whole_frames, t0),
        "chunking changed the frames"
    );
    let pending = split.0.pending();
    assert_eq!(
        pending,
        whole.0.pending(),
        "chunking changed how much is held back"
    );
    assert!(pending <= stream.len(), "more held back than received");
    codec::assert_tiled(&split_frames, (stream.len() - pending) as u64);

    for (i, frame) in whole_frames.iter().enumerate() {
        check_frame(stream, frame, whole_frames.get(i + 1));
    }

    split.0.reset();
    assert_eq!(split.0.pending(), 0, "reset left bytes held back");
    let again = codec::decode_chunks(&mut split, input.chunks(), t0);
    assert_eq!(again, split_frames, "a reset codec decoded differently");
    assert_eq!(split.0.pending(), pending, "a reset codec held back more");
}

/// `frame` against the bytes it names, `next` being the frame after it.
fn check_frame(stream: &[u8], frame: &Frame, next: Option<&Frame>) {
    let raw = &stream[frame.raw.start as usize..frame.raw.end as usize];
    match frame.kind.as_str() {
        "text" => check_text(stream, frame, raw, next),
        "malformed" => check_malformed(stream, frame, raw),
        kind => {
            let ty = RaceType::from_kind(kind)
                .unwrap_or_else(|| panic!("`{kind}` is not a RACE frame kind: {frame:?}"));
            check_race(frame, raw, ty);
        }
    }
}

fn check_race(frame: &Frame, raw: &[u8], ty: RaceType) {
    assert!(
        (HEADER_LEN + 2..=HEADER_LEN + MAX_LEN).contains(&raw.len()),
        "a RACE frame of {} bytes: {frame:?}",
        raw.len()
    );
    assert_eq!(raw[0], SYNC, "{frame:?}");
    assert_eq!(raw[1], ty.byte(), "type byte and kind disagree: {frame:?}");
    let len = usize::from(u16::from_le_bytes([raw[2], raw[3]]));
    assert_eq!(raw.len(), HEADER_LEN + len, "length field: {frame:?}");
    let cmd_id = u16::from_le_bytes([raw[4], raw[5]]);
    let payload = &raw[HEADER_LEN + 2..];
    assert_eq!(
        encode_frame(ty, cmd_id, payload).as_deref(),
        Ok(raw),
        "the frame does not re-encode to its bytes: {frame:?}"
    );
    assert_eq!(frame.severity, Severity::Info, "{frame:?}");
    let fields = [
        ("type", Value::UInt(u64::from(raw[1]))),
        ("cmd_id", Value::UInt(u64::from(cmd_id))),
        ("cmd_id_hex", Value::Str(format!("0x{cmd_id:04X}"))),
        ("payload", Value::Bytes(payload.to_vec())),
        ("payload_len", Value::UInt(payload.len() as u64)),
    ];
    assert_eq!(frame.fields.len(), fields.len(), "{frame:?}");
    for (name, want) in &fields {
        assert_eq!(frame.field(name), Some(want), "field `{name}`: {frame:?}");
    }
}

fn check_malformed(stream: &[u8], frame: &Frame, raw: &[u8]) {
    assert_eq!(raw.len(), 2, "{frame:?}");
    assert_eq!(raw[0], SYNC, "{frame:?}");
    assert!(RaceType::from_byte(raw[1]).is_some(), "{frame:?}");
    let end = frame.raw.end as usize;
    let length_bytes = stream
        .get(end..end + 2)
        .unwrap_or_else(|| panic!("a malformed header without its length bytes: {frame:?}"));
    let len = usize::from(u16::from_le_bytes([length_bytes[0], length_bytes[1]]));
    assert!(
        !(2..=MAX_LEN).contains(&len),
        "length {len} is fine, but the header is malformed: {frame:?}"
    );
    assert_eq!(frame.severity, Severity::Warning, "{frame:?}");
    assert_eq!(frame.field("type"), Some(&Value::UInt(u64::from(raw[1]))));
    assert_eq!(frame.field("len"), Some(&Value::UInt(len as u64)));
}

fn check_text(stream: &[u8], frame: &Frame, raw: &[u8], next: Option<&Frame>) {
    assert!(
        (1..=MAX_TEXT).contains(&raw.len()),
        "a text frame of {} bytes: {frame:?}",
        raw.len()
    );
    assert_eq!(frame.severity, Severity::Info, "{frame:?}");
    let (last, body) = raw.split_last().expect("not empty");
    assert!(
        !body.contains(&b'\n'),
        "a line feed inside a text frame: {frame:?}"
    );
    // A sync byte followed by a known type starts a frame (or a malformed header), so it
    // is never text, not even when the type byte is the first byte after the text frame.
    let end = (frame.raw.end as usize + 1).min(stream.len());
    assert!(
        !stream[frame.raw.start as usize..end]
            .windows(2)
            .any(|pair| pair[0] == SYNC && RaceType::from_byte(pair[1]).is_some()),
        "a frame start inside a text frame: {frame:?}"
    );
    // Text ends at a line feed or at MAX_TEXT bytes, or where a frame begins.
    if *last != b'\n' && raw.len() < MAX_TEXT {
        let next = next.unwrap_or_else(|| panic!("a text frame was cut for nothing: {frame:?}"));
        assert_ne!(
            next.kind, "text",
            "text cut short before more text: {frame:?}"
        );
    }
    let line = raw
        .strip_suffix(b"\n")
        .map_or(raw, |rest| rest.strip_suffix(b"\r").unwrap_or(rest));
    assert_eq!(
        frame.field("text"),
        Some(&Value::Str(render_text(line))),
        "{frame:?}"
    );
}

fn encode(json: &[u8]) {
    let mut codec = AirohaRace::new();
    let Some(default) = codec.describe().default_command().map(|c| c.name.clone()) else {
        return;
    };
    let Some(request) = codec::encode_request(json, &default) else {
        return;
    };
    let result = codec.encode(&request);
    assert_eq!(
        AirohaRace::new().encode(&request),
        result,
        "encode depends on state: {request:?}"
    );
    let Ok(bytes) = result else {
        return;
    };

    assert!(
        bytes.len() <= HEADER_LEN + MAX_LEN,
        "{} bytes is more than a frame: {request:?}",
        bytes.len()
    );
    assert_eq!(bytes.first(), Some(&SYNC), "{request:?}");
    let t0 = Instant::now();
    let frames = codec::decode_chunks(&mut AirohaRace::new(), [bytes.as_slice()], t0);
    assert_eq!(frames.len(), 1, "{bytes:02X?} decodes to {frames:?}");
    assert_eq!(frames[0].raw, 0..bytes.len() as u64, "{frames:?}");
    let kind = bytes
        .get(1)
        .and_then(|&b| RaceType::from_byte(b))
        .map(RaceType::kind);
    assert_eq!(Some(frames[0].kind.as_str()), kind, "{frames:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_replay() {
        crate::replay_seeds("race_rust", super::run);
    }

    /// The most text and the most of a frame at once: 1023 text bytes, then a header
    /// declaring the longest length and all of the frame but its last byte.
    fn worst_case() -> Vec<u8> {
        let mut stream = vec![b'x'; MAX_TEXT - 1];
        stream.extend_from_slice(&[SYNC, RaceType::Log.byte()]);
        stream.extend_from_slice(&(MAX_LEN as u16).to_le_bytes());
        stream.resize(MAX_TEXT - 1 + HEADER_LEN + MAX_LEN - 1, 0xA5);
        stream
    }

    #[test]
    fn the_pending_bound_is_reached() {
        let stream = worst_case();
        let mut codec = AirohaRace::new();
        let mut out = Vec::new();
        codec.decode(&stream, Instant::now(), 0, &mut out);
        assert!(out.is_empty());
        assert_eq!(codec.pending(), MAX_PENDING);
        assert_eq!(MAX_PENDING, 5122);

        // The last byte completes the frame, and the text goes out in front of it.
        codec.decode(&[0xA5], Instant::now(), stream.len() as u64, &mut out);
        assert_eq!(codec.pending(), 0);
        let kinds: Vec<_> = out.iter().map(|f| f.kind.as_str()).collect();
        assert_eq!(kinds, ["text", "log"]);

        // And the harness accepts it, whole and in pieces.
        let mut data = vec![0, 0];
        data.extend_from_slice(&stream);
        run(&data);
        let mut data = vec![0, 3, 1, 255, 7];
        data.extend_from_slice(&stream);
        run(&data);
    }
}
