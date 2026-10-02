#![no_main]

// The Lua-to-frame conversion: a fixed plugin returns the values its input bytes build.
// The checks and the input format are in fuzz/src/lua_values.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::lua_values::run(data));
