//! The simplest codec: one `line` frame per received line. A reference for tests: the app
//! does not register it (its decoders are plugins the user installs).

use std::sync::Arc;
use std::time::Instant;

use serialist_core::codec::{
    Codec, CodecError, CodecFactory, CodecInfo, CommandInfo, EncodeRequest, FieldInfo, FieldType,
    FnCodecFactory, Frame, FrameKindInfo,
};

/// The name the codec describes itself by.
pub const NAME: &str = "text-lines";
/// A line with no line feed after this many bytes is cut into a frame anyway.
pub const MAX_LINE: usize = 4096;

/// Splits the stream at line feeds. Each line becomes a `line` frame whose `text` is the
/// line without its ending (LF or CRLF), invalid UTF-8 replaced. A line longer than
/// [`MAX_LINE`] is cut every [`MAX_LINE`] bytes. `encode` has one command, `line`, which
/// sends `text` and a line ending (`eol`: `crlf`, the default, `lf`, `cr` or `none`).
#[derive(Debug, Default)]
pub struct TextLines {
    line: Vec<u8>,
    start: u64,
}

impl TextLines {
    pub fn new() -> Self {
        Self::default()
    }

    /// A factory for the codec, for tests that want it behind the [`CodecFactory`]
    /// seam. The app never registers it.
    pub fn factory() -> Arc<dyn CodecFactory> {
        Arc::new(FnCodecFactory::new(Self::info(), || {
            Ok(Box::new(TextLines::new()) as Box<dyn Codec>)
        }))
    }

    pub fn info() -> CodecInfo {
        CodecInfo {
            name: NAME.into(),
            version: "1.0.0".into(),
            description: "One frame per line of text".into(),
            kinds: vec![
                FrameKindInfo::new("line", "A received line").field(FieldInfo::new(
                    "text",
                    FieldType::Str,
                    "The line without its ending",
                )),
            ],
            commands: vec![
                CommandInfo::new("line", "Send a line of text")
                    .field(FieldInfo::new("text", FieldType::Str, "The text to send"))
                    .field(
                        FieldInfo::new("eol", FieldType::Str, "crlf (the default), lf, cr or none")
                            .optional(),
                    ),
            ],
        }
    }

    fn flush(&mut self, at: Instant, out: &mut Vec<Frame>) {
        if self.line.is_empty() {
            return;
        }
        let raw = self.start..self.start + self.line.len() as u64;
        let mut body = &self.line[..];
        if let Some(rest) = body.strip_suffix(b"\n") {
            body = rest.strip_suffix(b"\r").unwrap_or(rest);
        }
        let text = String::from_utf8_lossy(body).into_owned();
        out.push(
            Frame::new("line", raw, at)
                .with_summary(text.clone())
                .with_field("text", text),
        );
        self.line.clear();
    }
}

impl Codec for TextLines {
    fn describe(&self) -> CodecInfo {
        Self::info()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        let mut rest = chunk;
        let mut offset = raw_offset;
        while !rest.is_empty() {
            if self.line.is_empty() {
                self.start = offset;
            }
            let room = MAX_LINE - self.line.len();
            let (take, ended) = match memchr::memchr(b'\n', &rest[..rest.len().min(room)]) {
                Some(i) => (i + 1, true),
                None => (rest.len().min(room), false),
            };
            self.line.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            offset += take as u64;
            if ended || self.line.len() == MAX_LINE {
                self.flush(at, out);
            }
        }
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        if request.command != "line" {
            return Err(CodecError::UnknownCommand(request.command.clone()));
        }
        request.check_fields(&["eol", "text"])?;
        let text = request
            .str("text")?
            .ok_or_else(|| CodecError::MissingField("text".into()))?;
        let eol: &[u8] = match request.str("eol")?.unwrap_or("crlf") {
            "crlf" => b"\r\n",
            "lf" => b"\n",
            "cr" => b"\r",
            "none" => b"",
            _ => return Err(CodecError::bad_field("eol", "must be crlf, lf, cr or none")),
        };
        let mut bytes = text.as_bytes().to_vec();
        bytes.extend_from_slice(eol);
        Ok(bytes)
    }

    fn reset(&mut self) {
        self.line.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_frame_per_line_whatever_the_chunking() {
        let stream = b"one\r\ntwo\nthr\xffee\n";
        for split in 0..=stream.len() {
            let mut codec = TextLines::new();
            let mut out = Vec::new();
            let at = Instant::now();
            codec.decode(&stream[..split], at, 0, &mut out);
            codec.decode(&stream[split..], at, split as u64, &mut out);
            let got: Vec<_> = out
                .iter()
                .map(|f| (f.summary.as_str(), f.raw.clone()))
                .collect();
            assert_eq!(
                got,
                [("one", 0..5), ("two", 5..9), ("thr\u{fffd}ee", 9..16)],
                "{split}"
            );
        }
    }

    #[test]
    fn long_lines_are_cut() {
        let mut codec = TextLines::new();
        let mut out = Vec::new();
        codec.decode(&vec![b'a'; MAX_LINE + 1], Instant::now(), 0, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].raw, 0..MAX_LINE as u64);
    }

    #[test]
    fn encodes_a_line_with_its_ending() {
        let mut codec = TextLines::new();
        let line = |eol: Option<&str>| {
            let mut r = EncodeRequest::new("line").with("text", "AT");
            if let Some(eol) = eol {
                r = r.with("eol", eol);
            }
            TextLines::new().encode(&r)
        };
        assert_eq!(line(None), Ok(b"AT\r\n".to_vec()));
        assert_eq!(line(Some("none")), Ok(b"AT".to_vec()));
        assert!(line(Some("crlf!")).is_err());
        assert_eq!(
            codec.encode(&EncodeRequest::new("line")),
            Err(CodecError::MissingField("text".into()))
        );
    }
}
