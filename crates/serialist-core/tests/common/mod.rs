//! Strategies shared by the property tests.
#![allow(dead_code)]

use proptest::prelude::*;

/// Stream fragments, biased towards printable ASCII with CR/LF and escapes mixed in.
pub fn token() -> impl Strategy<Value = Vec<u8>> {
    let fixed: Vec<&'static [u8]> = vec![
        b"\r\n",
        b"\n",
        b"\r",
        b"\t",
        b"\x08",
        b"\x1b[K",
        b"\x1b[1K",
        b"\x1b[2K",
        b"\x1b[3D",
        b"\x1b[2C",
        b"\x1b[5G",
        b"\x1b[?25l",
        b"\x1b[2J",
        b"\x1b]0;title\x07",
        b"\x1b]0;never closed",
        b"\x1b(B",
        b"\x1b",
        b"\x1b[",
        b"\x1bP1;2qsixel\x1b\\",
        b"\x1b[0m",
        b"\x1b[m",
        b"\x1b[1;4m",
        b"\x1b[38;5;208m",
        b"\x1b[48:2:1:2:3m",
        b"\x1b[38;2;10;20;30m",
        b"\x1b[91m",
        b"\x1b[22;24m",
        "é".as_bytes(),
        "€".as_bytes(),
        "😀".as_bytes(),
        b"\x7f",
        b"\x00",
        b"\x9b",
    ];
    prop_oneof![
        8 => "[a-zA-Z0-9 ,.:=#*]{1,16}".prop_map(String::into_bytes),
        2 => Just(b"\r\n".to_vec()),
        1 => Just(b"\n".to_vec()),
        4 => prop::sample::select(fixed).prop_map(<[u8]>::to_vec),
        1 => prop::collection::vec(any::<u8>(), 1..4),
    ]
}

/// A byte stream of up to `max_tokens` fragments.
pub fn stream(max_tokens: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(token(), 0..max_tokens).prop_map(|tokens| tokens.concat())
}
