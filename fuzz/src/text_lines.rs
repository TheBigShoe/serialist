//! `text_lines`: the one-frame-per-line reference codec ([`TextLines`]), decoding and
//! encoding, on arbitrary bytes in arbitrary chunks.
//!
//! The input is an [`Input`]. Config bit 0 picks the mode; config 0, decode, is the one a
//! real session exercises:
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
//! contract. On top of that:
//!
//! - Chunking does not matter: the frames of the chunked run equal those of the one-chunk
//!   run ([`codec::timeless`], since `at` differs).
//! - No byte is lost or counted twice: the frames tile the stream up to the bytes held
//!   back ([`codec::assert_tiled`]). `TextLines` has no public count of what it holds,
//!   so that is `stream.len()` minus the end of the last frame, and it is below
//!   [`MAX_LINE`]: a line is cut into a frame as soon as it reaches that many bytes.
//! - The frames are the ones a plain model of the documented behavior gives: one frame
//!   for each line feed, and for each [`MAX_LINE`] bytes with none, whose `text` is its
//!   bytes without the line ending (LF or CRLF; a lone CR stays), invalid UTF-8
//!   replaced, and whose `summary` is that text. Whatever follows the last cut is held
//!   back.
//! - `reset` forgets everything: decoding the same chunks again from offset 0 gives the
//!   very same frames (`at` included).
//!
//! # Encode checks
//!
//! Whatever the request, `encode` does not panic and gives the same answer from a fresh
//! codec. The answer is the one the `TextLines` docs describe, worked out again here: the
//! command must be `line`, the fields only `text` and `eol`, `text` a string that is
//! present, `eol` absent or one of `crlf` (the default), `lf`, `cr`, `none`; checked in
//! that order, the first failure being the error. On success the bytes are the text
//! followed by the line ending.

use std::ops::Range;
use std::time::Instant;

use serde_json::Value as JsonValue;
use serialist_core::codec::{Codec, CodecError, EncodeRequest, Severity, Value};
use serialist_plugins::TextLines;
use serialist_plugins::text_lines::MAX_LINE;

use crate::{Input, codec};

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    if input.flag(0) {
        encode(input.stream);
    } else {
        decode(&input);
    }
}

fn decode(input: &Input) {
    let t0 = Instant::now();
    let stream = input.stream;

    let mut split = TextLines::new();
    let split_frames = codec::decode_chunks(&mut split, input.chunks(), t0);
    let whole_frames = codec::decode_chunks(&mut TextLines::new(), std::iter::once(stream), t0);
    let split_flat = codec::timeless(&split_frames, t0);
    assert_eq!(
        split_flat,
        codec::timeless(&whole_frames, t0),
        "chunking changed the frames"
    );

    let decoded = split_frames.last().map_or(0, |frame| frame.raw.end);
    codec::assert_tiled(&split_frames, decoded);
    let pending = stream.len() - decoded as usize;
    assert!(
        pending < MAX_LINE,
        "{pending} bytes held back, at the {MAX_LINE} a line is cut at"
    );

    let (lines, held) = model(stream);
    assert_eq!(held, pending, "the model holds back a different count");
    assert_eq!(split_frames.len(), lines.len(), "{split_frames:?}");
    for (frame, (raw, text)) in split_frames.iter().zip(&lines) {
        assert_eq!(frame.kind, "line", "{frame:?}");
        assert_eq!(&frame.raw, raw, "{frame:?}");
        assert_eq!(frame.severity, Severity::Info, "{frame:?}");
        assert_eq!(frame.fields.len(), 1, "{frame:?}");
        assert_eq!(frame.field("text"), Some(&Value::Str(text.clone())));
        assert_eq!(&frame.summary, text, "{frame:?}");
    }

    split.reset();
    let again = codec::decode_chunks(&mut split, input.chunks(), t0);
    assert_eq!(again, split_frames, "a reset codec decoded differently");
}

/// The lines of `stream` as the docs describe them, and the bytes left over: a line is
/// cut at a line feed (kept in it) or at [`MAX_LINE`] bytes.
fn model(stream: &[u8]) -> (Vec<(Range<u64>, String)>, usize) {
    let mut lines = Vec::new();
    let mut start = 0;
    while start < stream.len() {
        let window = &stream[start..stream.len().min(start + MAX_LINE)];
        let end = match window.iter().position(|&b| b == b'\n') {
            Some(lf) => start + lf + 1,
            None if window.len() == MAX_LINE => start + MAX_LINE,
            None => break,
        };
        let mut body = &stream[start..end];
        if let Some(rest) = body.strip_suffix(b"\n") {
            body = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let text = String::from_utf8_lossy(body).into_owned();
        lines.push((start as u64..end as u64, text));
        start = end;
    }
    (lines, stream.len() - start)
}

fn encode(json: &[u8]) {
    let mut codec = TextLines::new();
    let Some(default) = codec.describe().default_command().map(|c| c.name.clone()) else {
        return;
    };
    let Some(request) = codec::encode_request(json, &default) else {
        return;
    };
    let got = codec.encode(&request);
    assert_eq!(
        TextLines::new().encode(&request),
        got,
        "encode depends on state: {request:?}"
    );
    let got = got.map_err(|err| shape(&err));
    assert_eq!(
        got,
        expected_encoding(&request),
        "encode differs from the documented behavior: {request:?}"
    );
}

/// An error without its wording: which kind, and the command or field it names.
type Shape = (&'static str, String);

fn shape(err: &CodecError) -> Shape {
    match err {
        CodecError::UnknownCodec(name) => ("unknown_codec", name.clone()),
        CodecError::UnknownCommand(name) => ("unknown_command", name.clone()),
        CodecError::MissingField(field) => ("missing_field", field.clone()),
        CodecError::BadField { field, .. } => ("bad_field", field.clone()),
        CodecError::Internal(message) => ("internal", message.clone()),
    }
}

/// What `encode` must answer, from the `TextLines` docs.
fn expected_encoding(request: &EncodeRequest) -> Result<Vec<u8>, Shape> {
    let bad = |field: &str| ("bad_field", field.to_owned());
    if request.command != "line" {
        return Err(("unknown_command", request.command.clone()));
    }
    if let Some(field) = request
        .fields
        .keys()
        .filter(|key| !["eol", "text"].contains(&key.as_str()))
        .min()
    {
        return Err(bad(field));
    }
    let text = match request.fields.get("text") {
        None => return Err(("missing_field", "text".to_owned())),
        Some(JsonValue::String(text)) => text,
        Some(_) => return Err(bad("text")),
    };
    let eol: &[u8] = match request.fields.get("eol") {
        None => b"\r\n",
        Some(JsonValue::String(eol)) => match eol.as_str() {
            "crlf" => b"\r\n",
            "lf" => b"\n",
            "cr" => b"\r",
            "none" => b"",
            _ => return Err(bad("eol")),
        },
        Some(_) => return Err(bad("eol")),
    };
    Ok([text.as_bytes(), eol].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_replay() {
        crate::replay_seeds("text_lines", super::run);
    }

    #[test]
    fn the_model_cuts_at_line_feeds_and_at_max_line() {
        let (lines, held) = model(b"a\r\nb\n\nc");
        let got: Vec<_> = lines.iter().map(|(r, t)| (r.clone(), t.as_str())).collect();
        assert_eq!(got, [(0..3, "a"), (3..5, "b"), (5..6, ""),]);
        assert_eq!(held, 1);

        let long = vec![b'z'; MAX_LINE * 2 + 3];
        let (lines, held) = model(&long);
        let ranges: Vec<_> = lines.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(ranges, [0..4096, 4096..8192]);
        assert_eq!(held, 3);
    }

    #[test]
    fn the_harness_accepts_the_longest_held_line() {
        // MAX_LINE - 1 bytes with no line feed are held, one more is cut.
        for len in [MAX_LINE - 1, MAX_LINE, MAX_LINE + 1, 2 * MAX_LINE + 7] {
            let mut data = vec![0, 2, 100, 255];
            data.extend(std::iter::repeat_n(b'q', len));
            run(&data);
        }
    }

    #[test]
    fn a_cut_inside_a_character_replaces_it_in_both_halves() {
        // "é" is C3 A9. After 4095 "a" the line is cut between the two bytes, so each half
        // is a broken character and shows as U+FFFD.
        let mut stream = vec![b'a'; MAX_LINE - 1];
        stream.extend_from_slice("é\n".as_bytes());
        let (lines, held) = model(&stream);
        assert_eq!(held, 0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].1.chars().last(), Some('\u{fffd}'));
        assert_eq!(lines[1].1, "\u{fffd}");
        let mut data = vec![0, 0];
        data.extend_from_slice(&stream);
        run(&data);
    }
}
