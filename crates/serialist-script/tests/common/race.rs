//! A small RACE codec for the tests, written from the frame layout `serialist-sim`'s
//! device speaks (`[0x05][type][len u16 LE][cmd_id u16 LE][payload]`), so the script
//! crate's tests need no plugin. `encode` builds command frames (command `race`:
//! `cmd_id`, optional `payload`) and panics on the command `boom`; `decode` turns
//! responses, indications and logs into frames of those kinds with `cmd_id` and
//! `payload` fields, skipping every other byte.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use serialist_core::{
    Codec, CodecError, CodecFactory, CodecInfo, CommandInfo, EncodeRequest, FieldInfo, FieldType,
    Frame,
};

pub const NAME: &str = "race";

pub fn info() -> CodecInfo {
    CodecInfo {
        name: NAME.into(),
        version: "0".into(),
        description: "RACE for tests".into(),
        kinds: Vec::new(),
        commands: vec![
            CommandInfo::new("race", "a command frame")
                .field(FieldInfo::new("cmd_id", FieldType::UInt, "command id"))
                .field(FieldInfo::new("payload", FieldType::Bytes, "payload").optional()),
            CommandInfo::new("boom", "panics"),
        ],
    }
}

/// Makes [`TestRace`] codecs and counts them.
#[derive(Default)]
pub struct Factory {
    made: AtomicUsize,
}

impl Factory {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn made(&self) -> usize {
        self.made.load(Ordering::SeqCst)
    }
}

impl CodecFactory for Factory {
    fn info(&self) -> CodecInfo {
        info()
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        self.made.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(TestRace::default()))
    }
}

#[derive(Default)]
pub struct TestRace {
    held: Vec<u8>,
}

impl Codec for TestRace {
    fn describe(&self) -> CodecInfo {
        info()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        let start = raw_offset - self.held.len() as u64;
        self.held.extend_from_slice(chunk);
        let mut i = 0;
        loop {
            while i < self.held.len() && self.held[i] != 0x05 {
                i += 1;
            }
            if self.held.len() < i + 6 {
                break;
            }
            let kind = match self.held[i + 1] {
                0x5B => "response",
                0x5C => "indication",
                0x5D => "log",
                _ => {
                    i += 1;
                    continue;
                }
            };
            let len = u16::from_le_bytes([self.held[i + 2], self.held[i + 3]]) as usize;
            if !(2..=4096).contains(&len) {
                i += 1;
                continue;
            }
            if self.held.len() < i + 4 + len {
                break;
            }
            let cmd_id = u16::from_le_bytes([self.held[i + 4], self.held[i + 5]]);
            let payload = self.held[i + 6..i + 4 + len].to_vec();
            let raw = start + i as u64..start + (i + 4 + len) as u64;
            out.push(
                Frame::new(kind, raw, at)
                    .with_field("cmd_id", u64::from(cmd_id))
                    .with_field("payload", payload)
                    .with_summary(format!("{kind} {cmd_id:#06X}")),
            );
            i += 4 + len;
        }
        self.held.drain(..i.min(self.held.len()));
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        match request.command.as_str() {
            "race" => {}
            "boom" => panic!("the test codec blew up"),
            other => return Err(CodecError::UnknownCommand(other.to_owned())),
        }
        request.check_fields(&["cmd_id", "payload"])?;
        let cmd_id = request
            .uint("cmd_id", 0xFFFF)?
            .ok_or_else(|| CodecError::MissingField("cmd_id".into()))?;
        let payload = request.bytes("payload")?.unwrap_or_default();
        let len = (payload.len() + 2) as u16;
        let mut frame = vec![0x05, 0x5A];
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&(cmd_id as u16).to_le_bytes());
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    fn reset(&mut self) {
        self.held.clear();
    }
}
