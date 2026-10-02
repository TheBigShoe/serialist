#![no_main]

// The JSONC settings, theme, keymap and command loaders on arbitrary text. The checks and
// the input format are in fuzz/src/config_jsonc.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::config_jsonc::run(data));
