//! The checks every codec target shares. Not a target itself: no seeds, no `[[bin]]`.
//!
//! [`decode_chunks`] drives a [`Codec`] the way the app's ingest thread does and checks the
//! decode contract (trait [`Codec`] in `serialist-core`) after every call. A target then
//! compares what comes out against another run of the same stream: the same codec fed
//! the stream in one chunk ([`timeless`] frames equal), a Rust reference against a Lua
//! plugin, or a model written for the target. [`assert_tiled`] states the other thing a
//! framer owes: every received byte lands in exactly one frame. [`encode_request`] turns
//! fuzz bytes into the request a saved command would make.
//!
//! The chunk times follow `crates/serialist-plugins/tests/common/mod.rs`: chunk `i` arrives
//! at `t0 + i ms`, so two codecs fed the same chunks see the same times. A target takes
//! `t0` once per input and never reads the clock again.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::{Map, Value as JsonValue};
use serialist_core::codec::{Codec, EncodeRequest, Frame};
use serialist_plugins::PLUGIN_ERROR_KIND;

/// The kind of the frame `out` starts with, so a codec that clears or truncates `out` is
/// caught.
const SENTINEL_KIND: &str = "fuzz-sentinel";

/// Decode `chunks` as one stream from offset 0, chunk `i` arriving at `t0 + i ms` (the
/// convention of `crates/serialist-plugins/tests/common/mod.rs`). Checks the decode
/// contract after every call and returns the frames, in order.
///
/// `out` starts holding one sentinel frame, and after each call:
///
/// 1. `out[0]` is still the sentinel and `out` did not get shorter (a codec never clears
///    it). The sentinel is removed before returning.
/// 2. Every frame the call added has `raw.start <= raw.end`, `raw.end` no further than
///    the stream offset after this chunk, and `at` equal to this chunk's time.
/// 3. Across all frames, `raw.start` never decreases.
/// 4. Every frame's `kind` is one `codec.describe()` lists, or [`PLUGIN_ERROR_KIND`].
pub fn decode_chunks<'a>(
    codec: &mut dyn Codec,
    chunks: impl IntoIterator<Item = &'a [u8]>,
    t0: Instant,
) -> Vec<Frame> {
    let kinds: BTreeSet<String> = codec
        .describe()
        .kinds
        .into_iter()
        .map(|kind| kind.kind)
        .collect();
    let sentinel = Frame::new(SENTINEL_KIND, 0..0, t0);
    let mut out = vec![sentinel.clone()];
    let mut offset = 0u64;
    let mut last_start = 0u64;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let at = t0 + Duration::from_millis(i as u64);
        let before = out.len();
        codec.decode(chunk, at, offset, &mut out);
        offset += chunk.len() as u64;
        assert!(
            out.len() >= before,
            "decode of chunk {i} removed frames: `out` held {before}, now {}",
            out.len()
        );
        assert!(
            out[0] == sentinel,
            "decode of chunk {i} changed the frame `out` started with: {:?}",
            out[0]
        );
        for frame in &out[before..] {
            assert!(
                frame.raw.start <= frame.raw.end,
                "chunk {i}: backwards raw range {frame:?}"
            );
            assert!(
                frame.raw.end <= offset,
                "chunk {i}: raw range ends past the {offset} bytes received: {frame:?}"
            );
            assert!(
                frame.at == at,
                "chunk {i}: frame stamped with another chunk's time: {frame:?}"
            );
            assert!(
                frame.raw.start >= last_start,
                "chunk {i}: raw.start went back from {last_start}: {frame:?}"
            );
            last_start = frame.raw.start;
            assert!(
                kinds.contains(frame.kind.as_str()) || frame.kind == PLUGIN_ERROR_KIND,
                "chunk {i}: kind `{}` is not one the codec describes ({kinds:?}): {frame:?}",
                frame.kind
            );
        }
    }
    out.remove(0);
    out
}

/// The frames with every `at` set to `t0`, to compare decodes of different chunkings.
pub fn timeless(frames: &[Frame], t0: Instant) -> Vec<Frame> {
    frames
        .iter()
        .cloned()
        .map(|mut frame| {
            frame.at = t0;
            frame
        })
        .collect()
}

/// Every decoded byte is in exactly one frame: the raw ranges tile `0..decoded_up_to`, in
/// order, with no gap and no overlap.
pub fn assert_tiled(frames: &[Frame], decoded_up_to: u64) {
    let mut next = 0;
    for (i, frame) in frames.iter().enumerate() {
        assert!(
            frame.raw.start == next,
            "frame {i} starts at {} where the previous one ended at {next}: {frame:?}",
            frame.raw.start
        );
        next = frame.raw.end;
    }
    assert!(
        next == decoded_up_to,
        "the frames end at {next}, not at {decoded_up_to} (the bytes decoded so far)"
    );
}

/// An encode request from fuzz bytes: the bytes parsed as a JSON object holding a saved
/// command's fields (an optional `"command"` key names the command), through
/// [`EncodeRequest::from_payload`] with `default_command`. `None` when the bytes are not
/// a JSON object or `from_payload` rejects them.
pub fn encode_request(json: &[u8], default_command: &str) -> Option<EncodeRequest> {
    let fields: Map<String, JsonValue> = serde_json::from_slice(json).ok()?;
    EncodeRequest::from_payload(&fields, default_command).ok()
}

#[cfg(test)]
mod tests {
    use std::ops::Range;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use serialist_core::codec::{CodecError, CodecInfo, FrameKindInfo};
    use serialist_plugins::TextLines;

    use super::*;

    /// How a test breaks the frame the codec just produced.
    type Spoil = fn(&mut Frame);

    /// A codec whose `decode` is any function, to show each check can fail. It describes
    /// one kind, `k`.
    struct Scripted<F>(F);

    impl<F: FnMut(&[u8], Instant, u64, &mut Vec<Frame>)> Codec for Scripted<F> {
        fn describe(&self) -> CodecInfo {
            CodecInfo {
                kinds: vec![FrameKindInfo::new("k", "")],
                ..CodecInfo::default()
            }
        }

        fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
            (self.0)(chunk, at, raw_offset, out);
        }

        fn encode(&mut self, _: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
            Err(CodecError::Internal("not a codec".into()))
        }

        fn reset(&mut self) {}
    }

    /// Run `decode` over three chunks and return the panic message, if any.
    fn failure(decode: impl FnMut(&[u8], Instant, u64, &mut Vec<Frame>)) -> Option<String> {
        let t0 = Instant::now();
        let mut codec = Scripted(decode);
        let result = catch_unwind(AssertUnwindSafe(|| {
            decode_chunks(&mut codec, [&b"ab"[..], b"cd", b"ef"], t0)
        }));
        result.err().map(|payload| {
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default()
        })
    }

    /// One frame per chunk covering exactly the chunk.
    fn per_chunk(chunk: &[u8], at: Instant, offset: u64, out: &mut Vec<Frame>) {
        out.push(Frame::new("k", offset..offset + chunk.len() as u64, at));
    }

    #[test]
    fn a_well_behaved_codec_passes_and_the_sentinel_is_removed() {
        let t0 = Instant::now();
        let frames = decode_chunks(&mut TextLines::new(), [&b"one\ntw"[..], b"o\n"], t0);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].raw, 0..4);
        assert_eq!(frames[1].raw, 4..8);
        assert_eq!(frames[1].at, t0 + Duration::from_millis(1));
        assert_tiled(&frames, 8);
        let flat = timeless(&frames, t0);
        assert!(flat.iter().all(|frame| frame.at == t0));
        assert_eq!(flat[0].raw, frames[0].raw);

        assert!(failure(per_chunk).is_none());
        assert_eq!(
            decode_chunks(&mut Scripted(per_chunk), std::iter::empty(), t0),
            []
        );
    }

    #[test]
    fn a_codec_that_clears_or_shortens_out_is_caught() {
        let message = failure(|_, _, _, out| out.clear()).expect("a failure");
        assert!(message.contains("removed frames"), "{message}");
        let message = failure(|_, at, offset, out| {
            out[0] = Frame::new("k", offset..offset, at);
        })
        .expect("a failure");
        assert!(message.contains("changed the frame"), "{message}");
    }

    #[test]
    fn frames_that_break_the_contract_are_caught() {
        let cases: [(&str, Spoil); 5] = [
            ("backwards raw range", |f| {
                f.raw = Range { start: 2, end: 1 }
            }),
            ("past the", |f| f.raw.end += 1),
            ("another chunk's time", |f| f.at += Duration::from_secs(1)),
            ("not one the codec describes", |f| f.kind = "other".into()),
            ("went back", |f| f.raw = 0..0),
        ];
        for (expect, spoil) in cases {
            let message = failure(|chunk, at, offset, out| {
                per_chunk(chunk, at, offset, out);
                // Spoil the last chunk's frame only, so `went back` has frames before it.
                if offset == 4 {
                    spoil(out.last_mut().expect("a frame"));
                }
            })
            .unwrap_or_else(|| panic!("`{expect}` was not caught"));
            assert!(
                message.contains(expect),
                "wanted `{expect}`, got: {message}"
            );
        }
    }

    #[test]
    fn plugin_errors_are_a_kind_every_codec_may_produce() {
        let failed = failure(|chunk, at, offset, out| {
            out.push(Frame::new(
                PLUGIN_ERROR_KIND,
                offset..offset + chunk.len() as u64,
                at,
            ));
        });
        assert!(failed.is_none(), "{failed:?}");
    }

    #[test]
    fn tiling_needs_no_gap_no_overlap_and_the_right_end() {
        let t0 = Instant::now();
        let frame = |raw: Range<u64>| Frame::new("k", raw, t0);
        assert_tiled(&[], 0);
        assert_tiled(&[frame(0..3), frame(3..3), frame(3..9)], 9);
        for (frames, end) in [
            (vec![], 1),
            (vec![frame(1..3)], 3),
            (vec![frame(0..3), frame(4..5)], 5),
            (vec![frame(0..3), frame(2..5)], 5),
            (vec![frame(0..3)], 4),
        ] {
            assert!(
                catch_unwind(AssertUnwindSafe(|| assert_tiled(&frames, end))).is_err(),
                "{frames:?} to {end}"
            );
        }
    }

    #[test]
    fn requests_come_from_json_objects_only() {
        let req = encode_request(br#"{"command":"race","cmd_id":1}"#, "dflt").expect("a request");
        assert_eq!(req.command, "race");
        assert_eq!(req.fields.len(), 1);
        let req = encode_request(br#"{"text":"AT"}"#, "line").expect("a request");
        assert_eq!(req.command, "line");
        assert_eq!(req.field("text"), Some(&JsonValue::from("AT")));
        for bad in [
            &b""[..],
            b"[]",
            b"3",
            b"null",
            b"{\"a\":",
            b"\xff\xfe",
            br#"{"command":5}"#,
        ] {
            assert!(encode_request(bad, "line").is_none(), "{bad:?}");
        }
    }
}
