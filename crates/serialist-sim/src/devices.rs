//! Built-in simulated devices other than the firehose, and a capturing `DeviceOutput`
//! for unit-testing devices without a link.

use crate::{DeviceOutput, SimDevice};

/// Longest line a line-buffering device holds before giving up on it.
const MAX_LINE: usize = 64 * 1024;

fn is_eol(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

/// A `DeviceOutput` that records everything, for driving a device directly in a test.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureOutput {
    pub sent: Vec<u8>,
    pub disconnected: bool,
}

impl CaptureOutput {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything sent since the last `take`.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.sent)
    }
}

impl DeviceOutput for CaptureOutput {
    fn send(&mut self, bytes: &[u8]) {
        self.sent.extend_from_slice(bytes);
    }

    fn disconnect(&mut self) {
        self.disconnected = true;
    }
}

/// Sends back whatever it receives, either immediately or a whole line at a time.
#[derive(Debug)]
pub struct EchoDevice {
    name: String,
    per_line: bool,
    line: Vec<u8>,
}

impl EchoDevice {
    /// Echo every chunk as it arrives. Named `echo`.
    pub fn new() -> Self {
        Self {
            name: "echo".into(),
            per_line: false,
            line: Vec::new(),
        }
    }

    /// Echo only complete lines (up to and including a CR or LF), like a device with a
    /// line-buffered console. Named `echo-lines`.
    pub fn lines() -> Self {
        Self {
            name: "echo-lines".into(),
            per_line: true,
            line: Vec::new(),
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

impl Default for EchoDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice for EchoDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        if !self.per_line {
            out.send(bytes);
            return;
        }
        match bytes.iter().rposition(|&b| is_eol(b)) {
            Some(i) => {
                self.line.extend_from_slice(&bytes[..=i]);
                out.send(&self.line);
                self.line.clear();
                self.line.extend_from_slice(&bytes[i + 1..]);
            }
            None => self.line.extend_from_slice(bytes),
        }
        if self.line.len() > MAX_LINE {
            out.send(&self.line);
            self.line.clear();
        }
    }
}

/// A Hayes-style command interpreter.
///
/// | Command   | Response                          |
/// |-----------|-----------------------------------|
/// | `AT`      | `OK`                              |
/// | `ATI`     | the identity line, then `OK`      |
/// | `AT+VER?` | `+VER: <version>`, then `OK`      |
/// | `ATE0/1`  | echo off/on, then `OK`            |
/// | other     | `ERROR`                           |
///
/// Commands end at CR or LF (a CRLF pair is one terminator, empty lines are ignored),
/// are case-insensitive, and tolerate surrounding spaces and backspace edits. Responses
/// use the verbose form, each line framed as `\r\n<text>\r\n`. Echo starts off.
#[derive(Debug)]
pub struct AtDevice {
    identity: String,
    version: String,
    echo: bool,
    line: Vec<u8>,
    overflow: bool,
}

impl AtDevice {
    pub const DEFAULT_IDENTITY: &'static str = "Serialist Virtual Modem";
    pub const DEFAULT_VERSION: &'static str = "1.0.0";

    pub fn new() -> Self {
        Self {
            identity: Self::DEFAULT_IDENTITY.into(),
            version: Self::DEFAULT_VERSION.into(),
            echo: false,
            line: Vec::new(),
            overflow: false,
        }
    }

    pub fn with_identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = identity.into();
        self
    }

    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    pub fn with_echo(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    fn execute(&mut self, out: &mut dyn DeviceOutput) {
        let overflow = std::mem::take(&mut self.overflow);
        let command = self.line.trim_ascii().to_ascii_uppercase();
        self.line.clear();
        if command.is_empty() && !overflow {
            return;
        }
        let response = match command.as_slice() {
            _ if overflow => "\r\nERROR\r\n".to_owned(),
            b"AT" => "\r\nOK\r\n".to_owned(),
            b"ATI" => format!("\r\n{}\r\n\r\nOK\r\n", self.identity),
            b"AT+VER?" => format!("\r\n+VER: {}\r\n\r\nOK\r\n", self.version),
            b"ATE0" => {
                self.echo = false;
                "\r\nOK\r\n".to_owned()
            }
            b"ATE1" => {
                self.echo = true;
                "\r\nOK\r\n".to_owned()
            }
            _ => "\r\nERROR\r\n".to_owned(),
        };
        out.send(response.as_bytes());
    }
}

impl Default for AtDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice for AtDevice {
    fn name(&self) -> &str {
        "at"
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        for &b in bytes {
            if self.echo {
                out.send(&[b]);
            }
            match b {
                b'\r' | b'\n' => self.execute(out),
                0x08 | 0x7F => {
                    self.line.pop();
                }
                _ if self.line.len() < 1024 => self.line.push(b),
                _ => self.overflow = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(device: &mut dyn SimDevice, input: &[u8]) -> Vec<u8> {
        let mut out = CaptureOutput::new();
        device.on_receive(input, &mut out);
        out.take()
    }

    #[test]
    fn echo_is_immediate() {
        let mut dev = EchoDevice::new();
        assert_eq!(feed(&mut dev, b"ab"), b"ab");
        assert_eq!(feed(&mut dev, b"c\r\n"), b"c\r\n");
        assert_eq!(dev.name(), "echo");
    }

    #[test]
    fn echo_lines_waits_for_terminator() {
        let mut dev = EchoDevice::lines();
        assert_eq!(feed(&mut dev, b"hel"), b"");
        assert_eq!(feed(&mut dev, b"lo\r\nwor"), b"hello\r\n");
        assert_eq!(feed(&mut dev, b"ld\n"), b"world\n");
        assert_eq!(feed(&mut dev, b"x\ry"), b"x\r");
        assert_eq!(dev.name(), "echo-lines");
    }

    #[test]
    fn at_basic_commands() {
        let mut dev = AtDevice::new();
        assert_eq!(feed(&mut dev, b"AT\r"), b"\r\nOK\r\n");
        assert_eq!(
            feed(&mut dev, b"ATI\r\n"),
            b"\r\nSerialist Virtual Modem\r\n\r\nOK\r\n"
        );
        assert_eq!(
            feed(&mut dev, b"AT+VER?\n"),
            b"\r\n+VER: 1.0.0\r\n\r\nOK\r\n"
        );
        assert_eq!(feed(&mut dev, b"AT+FOO\r"), b"\r\nERROR\r\n");
        assert_eq!(feed(&mut dev, b"hello\r"), b"\r\nERROR\r\n");
    }

    #[test]
    fn at_is_line_ending_and_case_tolerant() {
        let mut dev = AtDevice::new().with_version("9.9");
        // CRLF, LFCR, blank lines and split chunks all give one response per command.
        assert_eq!(feed(&mut dev, b"\r\n\r\nat\r\n"), b"\r\nOK\r\n");
        assert_eq!(
            feed(&mut dev, b"  at+ver?  \n\r"),
            b"\r\n+VER: 9.9\r\n\r\nOK\r\n"
        );
        assert_eq!(feed(&mut dev, b"A"), b"");
        assert_eq!(feed(&mut dev, b"T"), b"");
        assert_eq!(feed(&mut dev, b"\r"), b"\r\nOK\r\n");
        // Backspace edits the line.
        assert_eq!(feed(&mut dev, b"ATX\x08\r"), b"\r\nOK\r\n");
    }

    #[test]
    fn at_echo_toggles() {
        let mut dev = AtDevice::new().with_identity("Widget");
        assert_eq!(feed(&mut dev, b"ATE1\r"), b"\r\nOK\r\n");
        assert_eq!(feed(&mut dev, b"ATI\r"), b"ATI\r\r\nWidget\r\n\r\nOK\r\n");
        assert_eq!(feed(&mut dev, b"ATE0\r"), b"ATE0\r\r\nOK\r\n");
        assert_eq!(feed(&mut dev, b"AT\r"), b"\r\nOK\r\n");
    }

    #[test]
    fn at_rejects_overlong_lines() {
        let mut dev = AtDevice::new();
        let mut input = vec![b'A'; 5000];
        input.push(b'\r');
        assert_eq!(feed(&mut dev, &input), b"\r\nERROR\r\n");
        assert_eq!(feed(&mut dev, b"AT\r"), b"\r\nOK\r\n");
    }
}
