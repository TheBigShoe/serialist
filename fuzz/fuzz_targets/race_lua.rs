#![no_main]

// The bundled Lua RACE plugin against the Rust reference codec, decoding in arbitrary
// chunks (with a reset halfway) and encoding arbitrary requests. The checks and the input
// format are in fuzz/src/race_lua.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::race_lua::run(data));
