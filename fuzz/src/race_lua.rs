//! `race_lua`: the bundled Lua RACE plugin (`airoha-race`, in
//! [`serialist_plugins::AIROHA_RACE_LUA`]) held to the Rust reference ([`AirohaRace`]).
//! The plugin crate claims the two agree byte for byte: the same frames for every way a
//! stream is cut into chunks, and the same bytes, or the same error, for every encode
//! request. This target lets libFuzzer look for a stream or a request where they do not.
//!
//! The input is an [`Input`]. Config bit 0 picks the mode; config 0, decode, is the one a
//! real session exercises:
//!
//! - **Bit 0 off, decode.** The stream is decoded in the chunks the input says. Bit 1
//!   adds a reset halfway.
//! - **Bit 0 on, encode.** The stream is a JSON object, a saved command's fields (see
//!   [`codec::encode_request`]); the chunk lengths and bit 1 are ignored. Anything that
//!   is not such an object is skipped, so seeds in this mode start `\x01\x00{`.
//!
//! Every input gets a fresh [`AirohaRace`] and a fresh [`LuaCodec`] (a new Lua VM): a
//! codec reused across inputs would carry state, and a crash could not be reproduced from
//! its input alone. The Lua limits are the defaults but for a 16 MiB memory cap.
//!
//! # Decode checks
//!
//! The same chunks, at the same times and stream offsets (chunk `i` at `t0 + i ms`, see
//! [`codec::decode_chunks`]), go to both codecs. Each call is held to the `Codec`
//! contract by `decode_chunks`, which also checks the Lua codec's `out` is never cleared
//! and its ranges are in bounds. On top of that:
//!
//! - The plugin never fails. No Lua frame has kind [`PLUGIN_ERROR_KIND`]: a plugin error
//!   (a raised error, the instruction budget, the memory cap, a malformed return value)
//!   on an input of a few KiB is a bug in the plugin or in the adapter, since the Rust
//!   codec handles the same bytes without trouble. The error's message is in the failure.
//! - The frames are equal, `at` included, frame for frame ([`Frame`] compares kind, raw
//!   range, severity, summary and fields).
//! - The Lua codec holds back as many bytes as the Rust one: [`LuaCodec::held_back`] is
//!   [`AirohaRace::pending`], after the last chunk.
//! - The two also describe themselves identically.
//!
//! With bit 1 on, both codecs are reset once, after `count / 2` of the `count` chunks
//! (the codec forgets what it held, and the rest of the stream arrives at the offsets it
//! would have had). The bytes held back at that moment are dropped, so the frames no
//! longer tile the stream, but the two must still agree on everything that follows.
//!
//! # Encode checks
//!
//! One request, built from the stream, goes to both. They must agree as
//! `tests/conformance.rs` of `serialist-plugins` requires: the same bytes; or both
//! [`CodecError::BadField`], naming the same field (the `reason` is prose and may be
//! worded differently); or the same error. An error from the plugin that the Rust codec
//! does not give, such as [`CodecError::Internal`] for a Lua error raised on an odd
//! value, is a failure.

use std::time::Instant;

use serialist_core::codec::{Codec, CodecError, CodecInfo, EncodeRequest, Frame};
use serialist_plugins::{
    AirohaRace, LuaCodec, LuaCodecFactory, LuaLimits, PLUGIN_ERROR_KIND, bundled_race_lua,
};

use crate::{Input, codec};

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    if input.flag(0) {
        encode(input.stream);
    } else {
        decode(&input);
    }
}

/// A fresh Lua VM running the bundled plugin.
fn lua_race() -> LuaCodec {
    let limits = LuaLimits {
        memory_bytes: 16 << 20,
        ..LuaLimits::default()
    };
    let factory: LuaCodecFactory = bundled_race_lua(limits).expect("the bundled RACE plugin loads");
    factory
        .create_lua()
        .expect("the bundled RACE plugin instantiates")
}

/// A codec that resets `inner` just before chunk number `reset_before` is decoded, if
/// that is set, then passes everything else through. The harness can then feed a reset
/// codec at the offsets the stream really has.
struct ResetAt<C> {
    inner: C,
    reset_before: Option<usize>,
    fed: usize,
}

impl<C: Codec> ResetAt<C> {
    fn new(inner: C, reset_before: Option<usize>) -> Self {
        Self {
            inner,
            reset_before,
            fed: 0,
        }
    }
}

impl<C: Codec> Codec for ResetAt<C> {
    fn describe(&self) -> CodecInfo {
        self.inner.describe()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        if self.reset_before == Some(self.fed) {
            self.inner.reset();
        }
        self.fed += 1;
        self.inner.decode(chunk, at, raw_offset, out);
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        self.inner.encode(request)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

fn decode(input: &Input) {
    let t0 = Instant::now();
    let chunks: Vec<&[u8]> = input.chunks().collect();
    let reset_before = input.flag(1).then_some(chunks.len() / 2);

    let mut rust = ResetAt::new(AirohaRace::new(), reset_before);
    let rust_frames = codec::decode_chunks(&mut rust, chunks.iter().copied(), t0);
    let mut lua = ResetAt::new(lua_race(), reset_before);
    let lua_frames = codec::decode_chunks(&mut lua, chunks.iter().copied(), t0);

    assert_eq!(
        lua.describe(),
        rust.describe(),
        "the plugin describes a different codec"
    );
    if let Some(failed) = lua_frames.iter().find(|f| f.kind == PLUGIN_ERROR_KIND) {
        panic!(
            "the plugin failed on {} bytes: {failed:?}",
            input.stream.len()
        );
    }
    assert_same_frames(&rust_frames, &lua_frames);
    assert_eq!(
        lua.inner.held_back(),
        rust.inner.pending(),
        "Lua and Rust hold back different amounts"
    );
}

/// Panics at the first frame where the two decodes differ, showing both.
fn assert_same_frames(rust: &[Frame], lua: &[Frame]) {
    for (i, (r, l)) in rust.iter().zip(lua).enumerate() {
        assert!(r == l, "frame {i} differs:\n  Rust: {r:?}\n  Lua:  {l:?}");
    }
    assert!(
        rust.len() == lua.len(),
        "Rust decoded {} frames and Lua {}; the first extra: {:?}",
        rust.len(),
        lua.len(),
        rust.get(lua.len()).or(lua.get(rust.len()))
    );
}

fn encode(json: &[u8]) {
    let Some(default) = AirohaRace::new()
        .describe()
        .default_command()
        .map(|c| c.name.clone())
    else {
        return;
    };
    let Some(request) = codec::encode_request(json, &default) else {
        return;
    };
    let rust = AirohaRace::new().encode(&request);
    let lua = lua_race().encode(&request);
    assert_encodes_alike(&request, &rust, &lua);
}

/// The same bytes, or errors of the same kind about the same field.
fn assert_encodes_alike(
    request: &EncodeRequest,
    rust: &Result<Vec<u8>, CodecError>,
    lua: &Result<Vec<u8>, CodecError>,
) {
    match (rust, lua) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{request:?}"),
        (
            Err(CodecError::BadField { field: a, .. }),
            Err(CodecError::BadField { field: b, .. }),
        ) => assert_eq!(a, b, "{request:?}: Rust gave {rust:?}, Lua gave {lua:?}"),
        (Err(a), Err(b)) => assert_eq!(a, b, "{request:?}"),
        _ => panic!("{request:?}: Rust gave {rust:?}, Lua gave {lua:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn seeds_replay() {
        crate::replay_seeds("race_lua", super::run);
    }

    /// `[config, n, len_1 ..., stream]`.
    fn input(config: u8, lens: &[u8], stream: &[u8]) -> Vec<u8> {
        let mut data = vec![config, lens.len() as u8];
        data.extend_from_slice(lens);
        data.extend_from_slice(stream);
        data
    }

    /// The reset in the middle really drops what was held back: the text `hello` is cut
    /// off after chunk 0, so the first frame after the reset starts at offset 5, in both
    /// codecs, which still agree.
    #[test]
    fn a_reset_forgets_the_held_back_bytes_and_keeps_the_offsets() {
        let stream = b"hello world\nfoo\n";
        let t0 = Instant::now();
        let chunks: [&[u8]; 3] = [&stream[..5], &stream[5..12], &stream[12..]];
        for reset_before in [None, Some(1)] {
            let mut rust = ResetAt::new(AirohaRace::new(), reset_before);
            let mut lua = ResetAt::new(lua_race(), reset_before);
            let a = codec::decode_chunks(&mut rust, chunks, t0);
            let b = codec::decode_chunks(&mut lua, chunks, t0);
            assert_same_frames(&a, &b);
            let first = a[0].raw.clone();
            assert_eq!(first, if reset_before.is_some() { 5..12 } else { 0..12 });
        }
        run(&input(2, &[5, 7, 4], stream));
    }

    #[test]
    fn a_difference_is_caught() {
        let t0 = Instant::now();
        let frames = |text: &str| {
            let mut codec = AirohaRace::new();
            codec::decode_chunks(&mut codec, [text.as_bytes()], t0)
        };
        let (a, b) = (frames("one\n"), frames("two\n"));
        assert!(catch_unwind(AssertUnwindSafe(|| assert_same_frames(&a, &b))).is_err());
        assert!(catch_unwind(AssertUnwindSafe(|| assert_same_frames(&a, &[]))).is_err());

        let request = EncodeRequest::new("race_version");
        let bytes = Ok(vec![5, 0x5A]);
        let other = Ok(vec![5, 0x5B]);
        let bad = |field: &str| Err(CodecError::bad_field(field, "words"));
        assert_encodes_alike(&request, &bytes, &bytes.clone());
        assert_encodes_alike(&request, &bad("type"), &bad("type"));
        for (rust, lua) in [
            (bytes.clone(), other),
            (bytes.clone(), bad("type")),
            (bad("type"), bad("payload")),
            (bad("type"), Err(CodecError::Internal("raised".into()))),
        ] {
            let caught = catch_unwind(AssertUnwindSafe(|| {
                assert_encodes_alike(&request, &rust, &lua)
            }));
            assert!(caught.is_err(), "{rust:?} vs {lua:?}");
        }
    }
}
