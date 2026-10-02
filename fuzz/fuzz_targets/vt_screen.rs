#![no_main]

// The VT screen (Alacritty's emulator, no PTY) on arbitrary bytes in arbitrary chunks. The
// checks and the input format are in fuzz/src/vt_screen.rs.

libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::vt_screen::run(data));
