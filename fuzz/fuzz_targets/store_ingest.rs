#![no_main]

// The page store on arbitrary bytes in arbitrary chunks, and its memory accounting. The
// checks and the input format are in fuzz/src/store_ingest.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::store_ingest::run(data));
