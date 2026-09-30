//! Decode throughput of the Rust and Lua RACE codecs on a 32 MiB capture.
//!
//! Ignored by default, since timings only mean something in a release build:
//!
//! ```text
//! cargo test --release -p serialist-plugins --test throughput -- --ignored --nocapture
//! ```

mod common;

use std::time::Instant;

use serialist_core::Codec;
use serialist_plugins::corpus;
use serialist_plugins::race::AirohaRace;

use common::lua_race;

#[test]
#[ignore = "a timing run: use --release --ignored --nocapture"]
fn race_decode_throughput_on_32_mib() {
    let bytes = corpus::generate_len(0x7417, 32 << 20);
    let mib = bytes.len() as f64 / (1024.0 * 1024.0);
    // 4 KiB chunks, what a USB-serial driver and the virtual link hand over.
    let chunks = corpus::split(&bytes, &[4096]);
    let codecs: [(&str, Box<dyn Codec>); 2] = [
        ("rust", Box::new(AirohaRace::new())),
        ("lua", Box::new(lua_race())),
    ];
    for (name, mut codec) in codecs {
        let at = Instant::now();
        let mut out = Vec::with_capacity(1024);
        let mut frames = 0usize;
        let mut offset = 0u64;
        let started = Instant::now();
        for chunk in &chunks {
            codec.decode(chunk, at, offset, &mut out);
            offset += chunk.len() as u64;
            frames += out.len();
            out.clear();
        }
        let secs = started.elapsed().as_secs_f64();
        eprintln!(
            "{name}: {mib:.1} MiB in {secs:.3} s = {:.1} MiB/s, {frames} frames, {:.0} ns/frame",
            mib / secs,
            secs * 1e9 / frames as f64
        );
        assert!(frames > 100_000);
    }
}
