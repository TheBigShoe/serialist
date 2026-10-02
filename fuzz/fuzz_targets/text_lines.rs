#![no_main]

// The one-frame-per-line reference codec, decoding and encoding, on arbitrary bytes in
// arbitrary chunks. The checks and the input format are in fuzz/src/text_lines.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::text_lines::run(data));
