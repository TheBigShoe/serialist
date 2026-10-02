//! `race_wasm`: the WebAssembly RACE plugin ([`WasmCodec`] running the committed
//! `crates/serialist-plugins/tests/fixtures/plugins/airoha-race-wasm/plugin.wasm`) held
//! to the Rust reference ([`AirohaRace`]) on arbitrary bytes in arbitrary chunks. The
//! plugin crate's conformance claim is identical frames for every chunking and identical
//! bytes or errors for every encode request; this is that claim under a fuzzer.
//!
//! Built only with the `wasm` feature (`cargo fuzz run --features wasm race_wasm`; the
//! justfile and CI add it): wasmtime is most of a clean build.
//!
//! The input is an [`Input`]. Config bit 0 picks the mode; config 0, plain decode, is the
//! one a real session exercises:
//!
//! - **Bit 0 off, decode.** The stream goes to both codecs in the chunks the input says.
//!   Bit 1 on: both are `reset()` after the first `chunks / 2` chunks (at least one),
//!   and the rest of the chunks follow at the stream offsets they would have had, so
//!   the bytes held back at the reset are dropped.
//! - **Bit 0 on, encode.** The stream is a JSON object, a saved command's fields (see
//!   [`codec::encode_request`]); the chunk lengths and bit 1 are ignored. Anything that
//!   is not such an object is skipped, so seeds in this mode start `\x01\x00{`.
//!
//! # Checks
//!
//! Both codecs are driven by one `Differential` codec, so a difference is reported at
//! the call where it first shows, and the WebAssembly codec's output goes through
//! [`codec::decode_chunks`] and its `Codec` contract checks (`out` never cleared, `raw`
//! ranges in bounds and in order, `at` the chunk's time, only the kinds `describe`
//! lists). On top of that:
//!
//! - After every chunk the frames the plugin added equal those the Rust codec added, `at`
//!   included, and [`WasmCodec::held_back`] equals [`AirohaRace::pending`]. (The Rust
//!   codec's own invariants, such as chunking not mattering and the bound on what it
//!   holds back, are `race_rust`'s checks; equality carries them over.)
//! - No frame is a [`PLUGIN_ERROR_KIND`] frame. On an input of at most a few KiB under
//!   a 5 s call limit, a trap, a time-out or a bad frame is a bug in the plugin or the
//!   adapter, never a finding to relax: the plugin error's message is in the panic.
//! - Without a reset, the frames tile the stream up to the held-back bytes
//!   ([`codec::assert_tiled`]). With one, the bytes held back at the reset are in no
//!   frame, so only the checks above apply.
//! - After a reset nothing is held back, in either codec.
//!
//! Encode mode gives the same request to both. The results are the same bytes, or
//! `BadField` errors naming the same field, or equal errors (an error's `reason` is prose
//! and may be worded differently), as `assert_encodes_like_rust` in
//! `crates/serialist-plugins/tests/conformance.rs`. One difference is the host's, not the
//! plugin's: the adapter refuses a request nested more than 32 deep before the plugin
//! sees it ([`HOST_DEPTH_LIMIT`]; the Lua adapter has the same limit), where the Rust
//! codec answers `BadField`, so that answer is accepted for a request that deep. On top
//! of that, encoding the request a second time on the same plugin instance gives the
//! same answer (the instance keeps its memory between calls, so state could leak), and
//! when it succeeds the bytes are one whole frame, as in `race_rust`: at most
//! `HEADER_LEN + MAX_LEN` long, starting with the sync byte, and decoding them (through
//! the same codecs, differentially) gives exactly one frame covering all of them whose
//! kind is the one the type byte names.
//!
//! # The engine
//!
//! One engine and one compiled component serve the whole process (`factory`), as the
//! app does; each input makes a fresh codec (a new instance, microseconds), so no state
//! outlives an input and a crash reproduces from its input alone. The call limit is 5 s,
//! not the default 50 ms: under instrumentation 50 ms trips and would turn into
//! plugin-error frames, which would show up as false differences. The engine's epoch
//! ticker is a thread of the plugin crate, not of this target; it parks while no call
//! runs, and a call that stays under the limit does not depend on it.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serialist_core::codec::{Codec, CodecError, CodecFactory, CodecInfo, EncodeRequest, Frame};
use serialist_plugins::race::{AirohaRace, HEADER_LEN, MAX_LEN, RaceType, SYNC, race_info};
use serialist_plugins::{PLUGIN_ERROR_KIND, WasmCodec, WasmCodecFactory, WasmEngine, WasmLimits};

use crate::{Input, codec};

/// The committed build of `examples/plugins/airoha-race-wasm`, which
/// `crates/serialist-plugins/tests/wasm_plugin.rs` keeps in step with its source.
const PLUGIN: &[u8] = include_bytes!(
    "../../crates/serialist-plugins/tests/fixtures/plugins/airoha-race-wasm/plugin.wasm"
);

/// How long one call into the plugin may run. See the module docs for why not the
/// default.
const TIME_PER_CALL: Duration = Duration::from_secs(5);

/// Requests nested deeper than this are refused by the WebAssembly adapter, as by the Lua
/// one (`MAX_DEPTH` in `crates/serialist-plugins/src/{wasm,lua}/convert.rs`, private).
/// Used only to accept the adapter's refusal; a request this shallow or shallower must
/// agree with the Rust codec, whatever the adapter's limit is.
pub const HOST_DEPTH_LIMIT: usize = 32;

/// The compiled plugin, made once per process.
fn factory() -> &'static WasmCodecFactory {
    static FACTORY: OnceLock<WasmCodecFactory> = OnceLock::new();
    FACTORY.get_or_init(|| {
        let engine = WasmEngine::shared().expect("the WebAssembly engine starts");
        let limits = WasmLimits {
            time_per_call: TIME_PER_CALL,
            ..WasmLimits::default()
        };
        let factory = WasmCodecFactory::from_bytes("airoha-race-wasm", PLUGIN, &engine, limits)
            .expect("the committed RACE plugin loads");
        assert_eq!(
            factory.info(),
            race_info(),
            "the plugin describes another codec than AirohaRace"
        );
        factory
    })
}

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    if input.flag(0) {
        encode(input.stream);
    } else {
        decode(&input);
    }
}

/// The plugin and the Rust codec side by side, as one [`Codec`] that panics where they
/// part ways. `describe`, `decode` and `encode` come from the plugin, so
/// [`codec::decode_chunks`] checks its output.
struct Differential {
    rust: AirohaRace,
    wasm: WasmCodec,
    /// `reset()` both after this many `decode` calls; 0 is never.
    reset_after: usize,
    /// `decode` calls so far.
    calls: usize,
}

impl Differential {
    fn new(reset_after: usize) -> Self {
        Self {
            rust: AirohaRace::new(),
            wasm: factory()
                .create_wasm()
                .expect("the committed RACE plugin instantiates"),
            reset_after,
            calls: 0,
        }
    }
}

impl Codec for Differential {
    fn describe(&self) -> CodecInfo {
        self.wasm.describe()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        let call = self.calls;
        self.calls += 1;
        let before = out.len();
        self.wasm.decode(chunk, at, raw_offset, out);
        let mut reference = Vec::new();
        self.rust.decode(chunk, at, raw_offset, &mut reference);

        // `out` may have been cleared or shortened, which `decode_chunks` reports.
        let added = out.get(before..).unwrap_or_default();
        if let Some(failed) = added.iter().find(|f| f.kind == PLUGIN_ERROR_KIND) {
            panic!("the plugin failed on chunk {call} ({chunk:02X?}): {failed:?}");
        }
        for (i, (want, got)) in reference.iter().zip(added).enumerate() {
            assert!(
                want == got,
                "chunk {call}, frame {i} differs\n Rust: {want:?}\n Wasm: {got:?}"
            );
        }
        assert!(
            reference.len() == added.len(),
            "chunk {call}: the Rust codec made {} frames, the plugin {}; first extra: {:?}",
            reference.len(),
            added.len(),
            reference.get(added.len()).or(added.get(reference.len()))
        );
        assert!(
            self.wasm.held_back() == self.rust.pending(),
            "after chunk {call} the plugin holds back {} bytes, the Rust codec {}",
            self.wasm.held_back(),
            self.rust.pending()
        );

        if self.calls == self.reset_after {
            self.reset();
        }
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        let rust = self.rust.encode(request);
        let wasm = self.wasm.encode(request);
        assert_encodes_alike(&rust, &wasm, request);
        assert_eq!(
            self.wasm.encode(request),
            wasm,
            "encode on the plugin depends on its state: {request:?}"
        );
        wasm
    }

    fn reset(&mut self) {
        self.rust.reset();
        self.wasm.reset();
        assert_eq!(
            self.rust.pending(),
            0,
            "reset left the Rust codec holding bytes"
        );
        assert_eq!(
            self.wasm.held_back(),
            0,
            "reset left the plugin holding bytes"
        );
    }
}

fn decode(input: &Input) {
    let t0 = Instant::now();
    let chunks: Vec<&[u8]> = input.chunks().collect();
    let reset_after = if input.flag(1) {
        (chunks.len() / 2).max(1)
    } else {
        0
    };
    let mut both = Differential::new(reset_after);
    let frames = codec::decode_chunks(&mut both, chunks.iter().copied(), t0);
    if reset_after == 0 {
        let held = both.wasm.held_back();
        assert!(held <= input.stream.len(), "more held back than received");
        codec::assert_tiled(&frames, (input.stream.len() - held) as u64);
    }
}

/// The deepest nesting of arrays and objects in `value`, a scalar being 1: the deepest
/// level the adapter's depth limit counts.
fn depth(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::Array(items) => 1 + items.iter().map(depth).max().unwrap_or(0),
        Value::Object(members) => 1 + members.values().map(depth).max().unwrap_or(0),
        _ => 1,
    }
}

/// `wasm` encodes `request` as `rust` does: the same bytes, or `BadField` errors naming
/// the same field, or equal errors; or the adapter refused a request nested past its
/// limit.
fn assert_encodes_alike(
    rust: &Result<Vec<u8>, CodecError>,
    wasm: &Result<Vec<u8>, CodecError>,
    request: &EncodeRequest,
) {
    let too_deep = request.fields.values().map(depth).max().unwrap_or(0) > HOST_DEPTH_LIMIT;
    match (rust, wasm) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{request:?}"),
        (Err(_), Err(CodecError::Internal(why))) if too_deep && why.contains("nests more") => {}
        (
            Err(CodecError::BadField { field: a, .. }),
            Err(CodecError::BadField { field: b, .. }),
        ) => assert_eq!(a, b, "{request:?}: {rust:?} vs {wasm:?}"),
        (Err(a), Err(b)) => assert_eq!(a, b, "{request:?}"),
        _ => panic!("{request:?}: Rust gave {rust:?}, the plugin gave {wasm:?}"),
    }
}

fn encode(json: &[u8]) {
    let mut both = Differential::new(0);
    let Some(default) = both.describe().default_command().map(|c| c.name.clone()) else {
        return;
    };
    let Some(request) = codec::encode_request(json, &default) else {
        return;
    };
    let Ok(bytes) = both.encode(&request) else {
        return;
    };

    assert!(
        bytes.len() <= HEADER_LEN + MAX_LEN,
        "{} bytes is more than a frame: {request:?}",
        bytes.len()
    );
    assert_eq!(bytes.first(), Some(&SYNC), "{request:?}");
    let t0 = Instant::now();
    let frames = codec::decode_chunks(&mut both, [bytes.as_slice()], t0);
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
        crate::replay_seeds("race_wasm", super::run);
    }

    /// A message from a failed `run`.
    fn failure(data: &[u8]) -> Option<String> {
        let payload = std::panic::catch_unwind(|| run(data)).err()?;
        Some(
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default(),
        )
    }

    #[test]
    fn nesting_past_the_hosts_limit_is_not_a_difference() {
        let nested = |levels: usize| {
            let mut data = b"\x01\x00{\"command\":\"race\",\"cmd_id\":1,\"payload\":".to_vec();
            data.extend(std::iter::repeat_n(b'[', levels));
            data.extend(std::iter::repeat_n(b']', levels));
            data.push(b'}');
            data
        };
        for levels in [1, HOST_DEPTH_LIMIT, HOST_DEPTH_LIMIT + 1, 100] {
            assert_eq!(failure(&nested(levels)), None, "{levels} levels");
        }
        // `depth` counts what the adapter counts: the payload array nested `levels` deep.
        for (levels, past) in [(HOST_DEPTH_LIMIT, false), (HOST_DEPTH_LIMIT + 1, true)] {
            let request = codec::encode_request(&nested(levels)[2..], "race").expect("a request");
            let deepest = request.fields.values().map(depth).max().expect("fields");
            assert_eq!(deepest > HOST_DEPTH_LIMIT, past, "{levels} levels");
        }
    }

    /// The harness fails when the two codecs differ, and says where: the Rust codec is
    /// fed a different stream from the plugin's.
    #[test]
    fn a_difference_is_reported_at_its_chunk() {
        let t0 = Instant::now();
        let mut both = Differential::new(0);
        let mut out = Vec::new();
        both.decode(b"abc\n", t0, 0, &mut out);
        both.rust.decode(b"\x05", t0, 4, &mut Vec::new());
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            both.decode(b"def\n", t0, 4, &mut out);
        }))
        .expect_err("the codecs differ");
        let message = payload.downcast_ref::<String>().expect("a message");
        assert!(message.contains("chunk 1"), "{message}");
    }
}
