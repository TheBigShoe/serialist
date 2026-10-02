#![no_main]

// The WebAssembly Airoha RACE plugin against the Rust codec, decoding and encoding, on
// arbitrary bytes in arbitrary chunks. The checks and the input format are in
// fuzz/src/race_wasm.rs. Needs `--features wasm`.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::race_wasm::run(data));
