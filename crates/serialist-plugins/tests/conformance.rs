//! The Rust and Lua RACE codecs agree byte for byte: the same description, the same
//! frames for every way a capture is cut into chunks, and the same bytes (or the same
//! error) for every encode request. With the `wasm` feature, the WebAssembly RACE plugin
//! (`examples/plugins/airoha-race-wasm`, its committed build in `tests/fixtures`) is
//! held to the same checks in [`wasm`].

mod common;

use std::time::Instant;

use proptest::prelude::*;
use serde_json::{Map, Value as JsonValue, json};
use serialist_core::{Codec, CodecError, EncodeRequest, Frame};
use serialist_plugins::corpus;
use serialist_plugins::race::{AirohaRace, race_info};

use common::{decode_chunks, lua_race, timeless};

/// Panics at the first frame where the two decodes differ, showing both.
fn assert_same_frames(rust: &[Frame], lua: &[Frame], context: &str) {
    for (i, (r, l)) in rust.iter().zip(lua).enumerate() {
        assert_eq!(r, l, "frame {i} differs ({context})");
    }
    assert_eq!(
        rust.len(),
        lua.len(),
        "frame counts differ ({context}); first extra: {:?}",
        rust.get(lua.len()).or(lua.get(rust.len()))
    );
}

/// Every byte decoded is in exactly one frame: frames tile the stream from offset 0 up to
/// the bytes the codec still holds back.
fn assert_tiled(frames: &[Frame], decoded_up_to: u64) {
    let mut at = 0;
    for frame in frames {
        assert_eq!(frame.raw.start, at, "a gap or overlap before {frame:?}");
        at = frame.raw.end;
    }
    assert_eq!(at, decoded_up_to);
}

#[test]
fn both_describe_the_same_codec() {
    assert_eq!(lua_race().describe(), race_info());
    assert_eq!(AirohaRace::new().describe(), race_info());
}

#[test]
fn the_corpus_decodes_identically_at_every_chunk_size() {
    let t0 = Instant::now();
    for seed in 0..6u64 {
        let bytes = corpus::generate(seed, 90);
        let mut whole_codec = AirohaRace::new();
        let whole = decode_chunks(&mut whole_codec, &[&bytes], t0);
        assert_tiled(&whole, (bytes.len() - whole_codec.pending()) as u64);
        for max in [1, 2, 3, 7, 64, 700, 5000] {
            let sizes = corpus::chunk_sizes(seed * 31 + max as u64, bytes.len(), max);
            let chunks = corpus::split(&bytes, &sizes);
            let rust = decode_chunks(&mut AirohaRace::new(), &chunks, t0);
            let mut lua_codec = lua_race();
            let lua = decode_chunks(&mut lua_codec, &chunks, t0);
            let context = format!("seed {seed}, chunks up to {max}");
            assert_same_frames(&rust, &lua, &context);
            assert_eq!(lua_codec.held_back(), whole_codec.pending(), "{context}");
            assert_eq!(timeless(&rust, t0), timeless(&whole, t0), "{context}");
        }
    }
}

#[test]
fn the_corpus_holds_every_kind() {
    let bytes = corpus::generate(1, 90);
    let frames = decode_chunks(&mut AirohaRace::new(), &[&bytes], Instant::now());
    for kind in [
        "command",
        "response",
        "indication",
        "log",
        "malformed",
        "text",
    ] {
        assert!(
            frames.iter().any(|f| f.kind == kind),
            "no {kind} frame in the corpus"
        );
    }
}

fn byte_soup() -> impl Strategy<Value = Vec<u8>> {
    let byte = prop_oneof![
        4 => any::<u8>(),
        2 => Just(0x05u8),
        1 => Just(0x5Au8),
        1 => Just(0x5Du8),
        1 => Just(b'\n'),
        1 => Just(b'\r'),
        2 => 0u8..8,
    ];
    prop::collection::vec(byte, 0..1500)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn rust_and_lua_agree_on_any_capture_however_it_is_cut(
        seed in any::<u64>(),
        segments in 1usize..40,
        sizes in prop::collection::vec(1usize..400, 1..16),
    ) {
        let t0 = Instant::now();
        let bytes = corpus::generate(seed, segments);
        let chunks = corpus::split(&bytes, &sizes);
        let mut rust_codec = AirohaRace::new();
        let rust = decode_chunks(&mut rust_codec, &chunks, t0);
        let mut lua_codec = lua_race();
        let lua = decode_chunks(&mut lua_codec, &chunks, t0);
        prop_assert_eq!(&rust, &lua);
        prop_assert_eq!(lua_codec.held_back(), rust_codec.pending());
        let whole = decode_chunks(&mut AirohaRace::new(), &[&bytes], t0);
        prop_assert_eq!(timeless(&rust, t0), timeless(&whole, t0));
        assert_tiled(&rust, (bytes.len() - rust_codec.pending()) as u64);
    }

    #[test]
    fn rust_and_lua_agree_on_arbitrary_bytes(
        bytes in byte_soup(),
        sizes in prop::collection::vec(1usize..64, 1..8),
    ) {
        let t0 = Instant::now();
        let chunks = corpus::split(&bytes, &sizes);
        let mut rust_codec = AirohaRace::new();
        let rust = decode_chunks(&mut rust_codec, &chunks, t0);
        let lua = decode_chunks(&mut lua_race(), &chunks, t0);
        prop_assert_eq!(&rust, &lua);
        assert_tiled(&rust, (bytes.len() - rust_codec.pending()) as u64);
    }
}

/// Same bytes, or errors of the same kind about the same field. (An error's `reason` is
/// prose and may be worded differently.)
fn assert_same_encoding(request: &EncodeRequest) {
    assert_encodes_like_rust(&mut lua_race(), "Lua", request);
}

/// `codec` (called `name`) encodes `request` as the Rust codec does.
fn assert_encodes_like_rust(codec: &mut dyn Codec, name: &str, request: &EncodeRequest) {
    let rust = AirohaRace::new().encode(request);
    let other = codec.encode(request);
    match (&rust, &other) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{request:?}"),
        (
            Err(CodecError::BadField { field: a, .. }),
            Err(CodecError::BadField { field: b, .. }),
        ) => assert_eq!(a, b, "{request:?}: {rust:?} vs {other:?}"),
        (Err(a), Err(b)) => assert_eq!(a, b, "{request:?}"),
        _ => panic!("{request:?}: Rust gave {rust:?}, {name} gave {other:?}"),
    }
}

fn request(command: &str, fields: JsonValue) -> EncodeRequest {
    let JsonValue::Object(fields) = fields else {
        panic!("fields must be an object");
    };
    EncodeRequest {
        command: command.to_owned(),
        fields,
    }
}

#[test]
fn both_encode_the_documented_forms_alike() {
    let cases = documented_requests();
    for case in &cases {
        assert_same_encoding(case);
    }
    // And the forms really do what they say.
    assert_eq!(
        lua_race().encode(&cases[4]),
        Ok(vec![0x05, 0x5C, 0x05, 0x00, 0x15, 0x0F, 1, 2, 255])
    );
}

/// Every documented form of a request, and the errors in a fixed order.
fn documented_requests() -> Vec<EncodeRequest> {
    let long = "AB".repeat(4094);
    let too_long = "AB".repeat(4095);
    vec![
        request("race_version", json!({})),
        request("race", json!({ "cmd_id": "0x0F15" })),
        request("race", json!({ "cmd_id": 3861, "type": "command" })),
        request(
            "race",
            json!({ "cmd_id": "0f15", "type": 0x5B, "payload": "00 01" }),
        ),
        request(
            "race",
            json!({ "cmd_id": "0X0F15", "type": "0x5C", "payload": [1, 2, 255] }),
        ),
        request(
            "race",
            json!({ "cmd_id": 0, "type": "log", "payload": "0x01,0x02\n0x03" }),
        ),
        request(
            "race",
            json!({ "cmd_id": 65535, "type": "5d", "payload": [] }),
        ),
        request("race", json!({ "cmd_id": "000000000000000000000F15" })),
        request("race", json!({ "cmd_id": 1, "payload": long })),
        // Errors.
        request("race", json!({})),
        request("nope", json!({})),
        request("race_version", json!({ "cmd_id": 1 })),
        request("race", json!({ "cmd_id": 1, "zz": 1, "aa": 2 })),
        request("race", json!({ "cmd_id": 65536 })),
        request("race", json!({ "cmd_id": -1 })),
        request("race", json!({ "cmd_id": 1.5 })),
        request("race", json!({ "cmd_id": "0x" })),
        request("race", json!({ "cmd_id": "" })),
        request("race", json!({ "cmd_id": "+1" })),
        request("race", json!({ "cmd_id": null })),
        request("race", json!({ "cmd_id": u64::MAX })),
        request("race", json!({ "cmd_id": "FFFFFFFFFFFFFFFFFFFF" })),
        request("race", json!({ "cmd_id": 1, "type": "Command" })),
        request("race", json!({ "cmd_id": 1, "type": 0x5E })),
        request("race", json!({ "cmd_id": 1, "type": [] })),
        request("race", json!({ "cmd_id": 1, "payload": "123" })),
        request("race", json!({ "cmd_id": 1, "payload": "zz" })),
        request("race", json!({ "cmd_id": 1, "payload": [256] })),
        request("race", json!({ "cmd_id": 1, "payload": [null] })),
        request("race", json!({ "cmd_id": 1, "payload": {} })),
        request("race", json!({ "cmd_id": 1, "payload": "01\u{a0}02" })),
        request("race", json!({ "cmd_id": 1, "payload": too_long })),
        request(
            "race",
            json!({ "type": "x", "cmd_id": "y", "payload": "z" }),
        ),
    ]
}

fn field_value(field: &'static str) -> impl Strategy<Value = JsonValue> {
    let pool: Vec<JsonValue> = match field {
        "type" => vec![
            json!("command"),
            json!("response"),
            json!("indication"),
            json!("log"),
            json!("0x5B"),
            json!("5c"),
            json!("0x5E"),
            json!("Command"),
            json!(0x5A),
            json!(0x5D),
            json!(-1),
            json!(1.5),
            json!(null),
            json!(true),
            json!([]),
            json!({}),
        ],
        "cmd_id" => vec![
            json!(0),
            json!(3861),
            json!(65535),
            json!(65536),
            json!("0x0F15"),
            json!("0f15"),
            json!("0x"),
            json!(""),
            json!("g1"),
            json!("0x10000"),
            json!(-5),
            json!(2.0),
            json!(null),
            json!([1]),
            json!("00000000000000000000001"),
        ],
        _ => vec![
            json!(""),
            json!("01 02"),
            json!("0x01,0x02"),
            json!("1"),
            json!("zz"),
            json!([]),
            json!([1, 2, 255]),
            json!([256]),
            json!([-1]),
            json!([1.5]),
            json!([null]),
            json!(5),
            json!(null),
            json!({}),
        ],
    };
    prop::sample::select(pool)
}

fn encode_request() -> impl Strategy<Value = EncodeRequest> {
    (
        prop::sample::select(vec!["race", "race", "race", "race_version", "other"]),
        prop::option::of(field_value("type")),
        prop::option::weighted(0.8, field_value("cmd_id")),
        prop::option::of(field_value("payload")),
        prop::option::weighted(0.1, prop::sample::select(vec!["aaa", "zzz", "cmd"])),
        prop::collection::vec(any::<u8>(), 0..40),
    )
        .prop_map(|(command, ty, cmd_id, payload, extra, raw_payload)| {
            let mut fields = Map::new();
            if let Some(ty) = ty {
                fields.insert("type".into(), ty);
            }
            if let Some(cmd_id) = cmd_id {
                fields.insert("cmd_id".into(), cmd_id);
            }
            match payload {
                // Sometimes a real random payload, as a list or as hex.
                Some(JsonValue::Null) if !raw_payload.is_empty() => {
                    fields.insert("payload".into(), json!(raw_payload));
                }
                Some(JsonValue::String(s)) if s.is_empty() && !raw_payload.is_empty() => {
                    let hex = serialist_core::codec::encode_hex(&raw_payload, " ");
                    fields.insert("payload".into(), json!(hex));
                }
                Some(payload) => {
                    fields.insert("payload".into(), payload);
                }
                None => {}
            }
            if let Some(extra) = extra {
                fields.insert(extra.into(), json!(1));
            }
            EncodeRequest {
                command: command.into(),
                fields,
            }
        })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn rust_and_lua_encode_any_request_alike(request in encode_request()) {
        assert_same_encoding(&request);
    }
}

/// The WebAssembly RACE plugin against the Rust codec: the same checks as the Lua one.
#[cfg(feature = "wasm")]
mod wasm {
    use super::*;
    use common::wasm_race;

    #[test]
    fn it_describes_the_same_codec() {
        assert_eq!(wasm_race().describe(), race_info());
    }

    #[test]
    fn the_corpus_decodes_identically_at_every_chunk_size() {
        let t0 = Instant::now();
        for seed in 0..6u64 {
            let bytes = corpus::generate(seed, 90);
            let mut whole_codec = AirohaRace::new();
            let whole = decode_chunks(&mut whole_codec, &[&bytes], t0);
            for max in [1, 2, 3, 7, 64, 700, 5000] {
                let sizes = corpus::chunk_sizes(seed * 31 + max as u64, bytes.len(), max);
                let chunks = corpus::split(&bytes, &sizes);
                let rust = decode_chunks(&mut AirohaRace::new(), &chunks, t0);
                let mut wasm_codec = wasm_race();
                let wasm = decode_chunks(&mut wasm_codec, &chunks, t0);
                let context = format!("seed {seed}, chunks up to {max}");
                assert_same_frames(&rust, &wasm, &context);
                assert_eq!(wasm_codec.held_back(), whole_codec.pending(), "{context}");
                assert_eq!(timeless(&rust, t0), timeless(&whole, t0), "{context}");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        #[test]
        fn rust_and_wasm_agree_on_any_capture_however_it_is_cut(
            seed in any::<u64>(),
            segments in 1usize..40,
            sizes in prop::collection::vec(1usize..400, 1..16),
        ) {
            let t0 = Instant::now();
            let bytes = corpus::generate(seed, segments);
            let chunks = corpus::split(&bytes, &sizes);
            let mut rust_codec = AirohaRace::new();
            let rust = decode_chunks(&mut rust_codec, &chunks, t0);
            let mut wasm_codec = wasm_race();
            let wasm = decode_chunks(&mut wasm_codec, &chunks, t0);
            prop_assert_eq!(&rust, &wasm);
            prop_assert_eq!(wasm_codec.held_back(), rust_codec.pending());
            assert_tiled(&wasm, (bytes.len() - wasm_codec.held_back()) as u64);
        }

        #[test]
        fn rust_and_wasm_agree_on_arbitrary_bytes(
            bytes in byte_soup(),
            sizes in prop::collection::vec(1usize..64, 1..8),
        ) {
            let t0 = Instant::now();
            let chunks = corpus::split(&bytes, &sizes);
            let rust = decode_chunks(&mut AirohaRace::new(), &chunks, t0);
            let wasm = decode_chunks(&mut wasm_race(), &chunks, t0);
            prop_assert_eq!(&rust, &wasm);
        }
    }

    #[test]
    fn it_encodes_the_documented_forms_alike() {
        let mut codec = wasm_race();
        let cases = documented_requests();
        for case in &cases {
            assert_encodes_like_rust(&mut codec, "WebAssembly", case);
        }
        assert_eq!(
            codec.encode(&cases[4]),
            Ok(vec![0x05, 0x5C, 0x05, 0x00, 0x15, 0x0F, 1, 2, 255])
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        #[test]
        fn rust_and_wasm_encode_any_request_alike(request in encode_request()) {
            assert_encodes_like_rust(&mut wasm_race(), "WebAssembly", &request);
        }
    }
}
