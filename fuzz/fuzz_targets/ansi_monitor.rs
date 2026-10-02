#![no_main]

// The monitor-mode ANSI parser on arbitrary bytes in arbitrary chunks. The checks and
// the input format are in fuzz/src/ansi_monitor.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::ansi_monitor::run(data));
