//! A line framer that misbehaves on request: the WebAssembly adapter's test plugin, like
//! the Lua adapter's `LINES`. One `line` frame per LF-terminated line; the rest is held
//! back. Some lines make it break the contract or trap:
//!
//! | Line | What `decode` does |
//! |---|---|
//! | `loop` | spins forever (the time limit) |
//! | `panic` | panics (the SDK's panic handler logs it, then traps) |
//! | `grow` | allocates until memory runs out (the memory limit) |
//! | `deep` | recurses until the stack runs out |
//! | `badtype` | gives field `n` as a string although it is declared `uint` |
//! | `missing` | leaves out the required field `n` |
//! | `outside` | reports a frame beyond the input |
//! | `warn` | marks the frame a warning |
//! | `chatty` | writes 100 log lines |
//! | `list` | adds a nested list field |
//!
//! An input containing `#` makes `decode` hold back more bytes than it was given.

#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::hint::black_box;

use serialist_plugin_sdk::{
    CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Json, Plugin,
    Request, Severity, Value, log,
};

pub struct Lines {
    /// `decode` calls since the plugin started or was reset.
    calls: i64,
}

serialist_plugin_sdk::export_plugin!(Lines);

fn deep(n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        black_box(deep(black_box(n - 1))) + 1
    }
}

impl Plugin for Lines {
    fn new() -> Self {
        Lines { calls: 0 }
    }

    fn describe(&self) -> CodecInfo {
        CodecInfo::new(
            "lines",
            "0.1.0",
            "One frame per line; some lines make it misbehave",
        )
        .with_kind(
            FrameKindInfo::new("line", "One line")
                .with_field(FieldInfo::new("text", FieldType::Str, "The line"))
                .with_field(FieldInfo::new("n", FieldType::Uint, "Its length"))
                .with_field(FieldInfo::new("body", FieldType::Bytes, "Unused").optional()),
        )
        .with_command(CommandInfo::new("say", "A line").with_field(FieldInfo::new(
            "text",
            FieldType::Str,
            "The line",
        )))
    }

    fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> usize {
        self.calls += 1;
        let mut start = 0;
        while let Some(i) = input[start..].iter().position(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(&input[start..start + i]).into_owned();
            let mut frame = Frame::new("line", start, i + 1).with_summary(text.clone());
            match text.as_str() {
                "loop" => loop {
                    black_box(&frame);
                },
                "panic" => panic!("boom at offset {start}"),
                "grow" => {
                    let mut hog: Vec<Vec<u8>> = Vec::new();
                    loop {
                        hog.push(black_box(vec![1u8; 1 << 20]));
                    }
                }
                "deep" => {
                    black_box(deep(u64::MAX));
                }
                "outside" => frame.offset = input.len() + 5,
                "warn" => frame = frame.with_severity(Severity::Warning),
                "chatty" => {
                    for k in 0..100 {
                        log::info(&format!("chatty line {k}"));
                    }
                }
                _ => {}
            }
            frame.push_field("text", text.as_str());
            match text.as_str() {
                "badtype" => frame.push_field("n", "seven"),
                "missing" => {}
                _ => frame.push_field("n", text.len()),
            }
            frame.push_field("calls", self.calls);
            if text == "list" {
                frame.push_field(
                    "nested",
                    vec![
                        Value::from(1i64),
                        vec![Value::from("a")].into(),
                        true.into(),
                    ],
                );
            }
            frames.push(frame);
            start += i + 1;
        }
        if input.contains(&b'#') {
            return input.len() + 1;
        }
        input.len() - start
    }

    fn encode(&mut self, request: &Request<'_>) -> Result<Vec<u8>, CodecError> {
        match request.command() {
            "say" => {
                let text = request
                    .str("text")?
                    .ok_or_else(|| CodecError::missing_field("text"))?;
                Ok(format!("{text}\n").into_bytes())
            }
            "types" => {
                let f = |name| request.field(name);
                let array = f("b").and_then(|v| v.as_array());
                let object = f("c").and_then(|v| v.as_object());
                let ok = f("a") == Some(Json::Null)
                    && array.is_some_and(|b| b.len() == 2 && b.get(1) == Some(Json::Int(2)))
                    && object.is_some_and(|c| c.get("d") == Some(Json::Int(1)))
                    && f("e") == Some(Json::Float(1.5))
                    && f("f") == Some(Json::Int(2))
                    && f("g")
                        .and_then(|v| v.as_array())
                        .is_some_and(|g| g.is_empty())
                    && f("h") == Some(Json::UInt(u64::MAX))
                    && f("i") == Some(Json::Int(-3))
                    && f("j") == Some(Json::Str("s"))
                    && f("k") == Some(Json::Bool(true));
                Ok(if ok {
                    b"typed".to_vec()
                } else {
                    b"untyped".to_vec()
                })
            }
            "fail" => Err(CodecError::internal("plain message")),
            "panic" => panic!("raised here"),
            other => Err(CodecError::unknown_command(other)),
        }
    }
}
