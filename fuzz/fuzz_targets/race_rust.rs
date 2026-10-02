#![no_main]

// The Rust Airoha RACE codec, decoding and encoding, on arbitrary bytes in arbitrary
// chunks. The checks and the input format are in fuzz/src/race_rust.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::race_rust::run(data));
