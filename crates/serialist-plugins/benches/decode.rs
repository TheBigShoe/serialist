//! Decode throughput of the RACE codecs on a 4 MiB capture: the Rust reference codec and
//! the bundled Lua plugin, both fed 4 KiB chunks as a USB-serial driver hands them over.
//! The capture comes from `corpus::generate_len`, so no hardware or link is involved.
//! The ignored timing test in tests/throughput.rs runs the same codecs on 32 MiB.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use serialist_core::{Codec, Frame};
use serialist_plugins::race::AirohaRace;
use serialist_plugins::{LuaLimits, bundled_race_lua, corpus};

const CAPTURE_BYTES: usize = 4 << 20;
const CHUNK: usize = 4096;

/// Decodes `capture` as one stream and returns how many frames came out.
fn decode_all<C: Codec>(codec: &mut C, capture: &[u8], at: Instant) -> usize {
    let mut out: Vec<Frame> = Vec::with_capacity(1024);
    let mut frames = 0;
    let mut offset = 0u64;
    for chunk in capture.chunks(CHUNK) {
        codec.decode(chunk, at, offset, &mut out);
        offset += chunk.len() as u64;
        frames += out.len();
        out.clear();
    }
    frames
}

fn decode(c: &mut Criterion) {
    let capture = corpus::generate_len(0x7417, CAPTURE_BYTES);
    let at = Instant::now();
    let mut group = c.benchmark_group("decode");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Bytes(capture.len() as u64));

    group.bench_function("race_rust", |b| {
        b.iter_batched(
            AirohaRace::new,
            |mut codec| black_box(decode_all(&mut codec, &capture, at)),
            BatchSize::LargeInput,
        );
    });

    // The plugin is compiled once; each iteration gets a fresh Lua state, built outside
    // the timed part.
    let factory = bundled_race_lua(LuaLimits::default()).expect("the bundled plugin loads");
    // A plugin that hit its instruction budget would turn the capture into error frames
    // and look fast: check once, outside the timing, that it decodes what the reference does.
    let lua_frames = decode_all(&mut factory.create_lua().expect("loads"), &capture, at);
    let rust_frames = decode_all(&mut AirohaRace::new(), &capture, at);
    assert!(rust_frames > 10_000, "the capture holds many frames");
    assert_eq!(
        lua_frames, rust_frames,
        "Lua and Rust decode the same frames"
    );
    group.bench_function("race_lua", |b| {
        b.iter_batched(
            || factory.create_lua().expect("the bundled plugin loads"),
            |mut codec| black_box(decode_all(&mut codec, &capture, at)),
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, decode);
criterion_main!(benches);
