//! The `inline` settings object: how inline interactive mode talks to the device.
//!
//! Inline mode sends every keystroke to the port as it is typed. These keys tune the
//! few parts of that which vary between devices:
//!
//! ```jsonc
//! "inline": {
//!   "backspace": "del",         // "del" (0x7f) or "bs" (0x08)
//!   "escape_chord": "ctrl-]",   // leaves inline mode; press twice to send it
//!   "paste_chunk_bytes": 64,    // paste is written in chunks this size...
//!   "paste_chunk_delay_ms": 10  // ...this far apart, for bootloaders that drop bytes
//! }
//! ```
//!
//! Every value is checked when the settings load, and a bad one is an error naming the
//! file, line and column, as for every other key. The escape chord is checked for
//! keystroke syntax only (this crate has no window system to ask); the UI turns the
//! string into a keystroke and falls back to the default if it cannot.

use serde::{Deserialize, Serialize};

use super::de::{Raw, scalar};
use super::defaults as d;

/// What the Backspace key sends in inline mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub enum BackspaceKey {
    /// 0x7f, DEL: what most terminals and Linux devices expect.
    #[default]
    #[serde(rename = "del")]
    Del,
    /// 0x08, BS: what some bootloaders and older devices expect.
    #[serde(rename = "bs")]
    Bs,
}

impl BackspaceKey {
    /// The byte the key sends.
    pub fn byte(self) -> u8 {
        match self {
            BackspaceKey::Del => 0x7f,
            BackspaceKey::Bs => 0x08,
        }
    }

    /// The byte Ctrl-Backspace sends: the other one.
    pub fn alternate_byte(self) -> u8 {
        match self {
            BackspaceKey::Del => 0x08,
            BackspaceKey::Bs => 0x7f,
        }
    }

    /// The name settings files use: `del` or `bs`.
    pub fn name(self) -> &'static str {
        match self {
            BackspaceKey::Del => "del",
            BackspaceKey::Bs => "bs",
        }
    }
}

const BACKSPACE: &str = "\"del\" (0x7f) or \"bs\" (0x08)";

/// `del` and `bs`, plus the spellings by byte (`"0x7f"`, `"0x08"`, `127`, `8`) and by
/// key name (`delete`, `backspace`).
fn backspace(raw: Raw<'_>) -> Result<BackspaceKey, String> {
    let bad = |got: String| {
        format!("inline.backspace must be \"del\" (0x7f) or \"bs\" (0x08), got {got}")
    };
    match raw {
        Raw::Str(text) => match text.trim().to_ascii_lowercase().as_str() {
            "del" | "delete" | "0x7f" | "127" => Ok(BackspaceKey::Del),
            "bs" | "backspace" | "0x08" | "0x8" | "8" => Ok(BackspaceKey::Bs),
            _ => Err(bad(format!("{text:?}"))),
        },
        Raw::Bool(v) => Err(bad(v.to_string())),
        Raw::Int(v) => number(raw, v.to_string()).map_err(bad),
        Raw::Uint(v) => number(raw, v.to_string()).map_err(bad),
        Raw::Float(v) => number(raw, v.to_string()).map_err(bad),
    }
}

fn number(raw: Raw<'_>, shown: String) -> Result<BackspaceKey, String> {
    match raw.unsigned() {
        Some(0x7f) => Ok(BackspaceKey::Del),
        Some(0x08) => Ok(BackspaceKey::Bs),
        _ => Err(shown),
    }
}

impl<'de> Deserialize<'de> for BackspaceKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        scalar(deserializer, BACKSPACE, backspace)
    }
}

/// The modifier names a keystroke may start with, as GPUI spells them.
const MODIFIERS: [&str; 8] = [
    "ctrl",
    "alt",
    "shift",
    "fn",
    "secondary",
    "cmd",
    "super",
    "win",
];

/// Whether `name` is a modifier that makes a chord out of a typed key: everything but
/// `shift` and `fn`, which alone still type the character.
fn is_chording(name: &str) -> bool {
    MODIFIERS.contains(&name) && !matches!(name, "shift" | "fn")
}

/// Checks that `text` is a single keystroke in GPUI's syntax (`ctrl-]`, `alt-shift-x`,
/// `ctrl--`, `f12`) that is safe to use as the escape chord, and returns it trimmed.
///
/// A key that types something (a character, `space`, `enter`, `tab`, `backspace`) needs
/// ctrl, alt or the platform key with it, or every press of that key would leave inline
/// mode.
pub fn validate_chord(text: &str) -> Result<String, String> {
    let chord = text.trim();
    let bad = |why: &str| format!("inline.escape_chord {text:?} is not a keystroke: {why}");
    if chord.is_empty() {
        return Err(bad("it is empty"));
    }
    if chord.chars().any(char::is_whitespace) {
        return Err(bad("expected one keystroke, not a sequence"));
    }
    let mut rest = chord;
    let mut chording = false;
    // Modifiers come first, each followed by `-`. What is left is the key, which may be
    // `-` itself.
    let key = loop {
        if rest == "-" {
            break rest;
        }
        let Some((head, tail)) = rest.split_once('-') else {
            break rest;
        };
        let head = head.to_ascii_lowercase();
        if !MODIFIERS.contains(&head.as_str()) {
            return Err(bad(&format!(
                "unknown modifier `{head}` (use ctrl, alt, shift, cmd, super, win or fn)"
            )));
        }
        chording |= is_chording(&head);
        if tail.is_empty() {
            return Err(bad("no key after the last `-`"));
        }
        rest = tail;
    };
    let single = key.chars().count() == 1;
    // A key name such as `f12`, `escape` or `pageup`.
    if !single && !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(bad(&format!("`{key}` is not a key name")));
    }
    let typed = single || matches!(key, "space" | "enter" | "tab" | "backspace");
    if typed && !chording {
        return Err(bad(&format!(
            "`{key}` types a character, so the chord needs ctrl, alt or cmd with it"
        )));
    }
    Ok(chord.to_owned())
}

const CHORD: &str = "a keystroke such as \"ctrl-]\"";

fn chord(raw: Raw<'_>) -> Result<String, String> {
    match raw {
        Raw::Str(text) => validate_chord(text),
        _ => Err(format!("invalid value, expected {CHORD}")),
    }
}

fn chunk_bytes(raw: Raw<'_>) -> Result<usize, String> {
    match raw.unsigned() {
        Some(v) if (1..=MAX_PASTE_CHUNK_BYTES as u64).contains(&v) => Ok(v as usize),
        Some(v) => Err(format!(
            "inline.paste_chunk_bytes must be from 1 to {MAX_PASTE_CHUNK_BYTES}, got {v}"
        )),
        None => Err("invalid value, expected inline.paste_chunk_bytes as a whole number".into()),
    }
}

fn chunk_delay(raw: Raw<'_>) -> Result<u64, String> {
    match raw.unsigned() {
        Some(v) if v <= MAX_PASTE_CHUNK_DELAY_MS => Ok(v),
        Some(v) => Err(format!(
            "inline.paste_chunk_delay_ms must be from 0 to {MAX_PASTE_CHUNK_DELAY_MS}, got {v}"
        )),
        None => Err("invalid value, expected inline.paste_chunk_delay_ms as a whole number".into()),
    }
}

fn escape_chord<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    scalar(d, CHORD, chord)
}

fn paste_chunk_bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    scalar(d, "a byte count from 1", chunk_bytes)
}

fn paste_chunk_delay_ms<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    scalar(d, "a delay in milliseconds", chunk_delay)
}

/// The largest `paste_chunk_bytes`: a megabyte in one write is no chunking at all.
pub const MAX_PASTE_CHUNK_BYTES: usize = 1 << 20;

/// The longest `paste_chunk_delay_ms`: ten seconds.
pub const MAX_PASTE_CHUNK_DELAY_MS: u64 = 10_000;

/// The `inline` object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineSettings {
    /// What Backspace sends: `del` (0x7f, the default) or `bs` (0x08). Ctrl-Backspace
    /// sends the other one.
    #[serde(default = "d::inline_backspace")]
    pub backspace: BackspaceKey,
    /// The keystroke that leaves inline mode instead of being sent; pressing it twice
    /// quickly sends it to the device. In GPUI's keystroke syntax, default `ctrl-]`.
    #[serde(default = "d::inline_escape_chord", deserialize_with = "escape_chord")]
    pub escape_chord: String,
    /// A paste is written to the port in chunks of this many bytes, at least 1.
    /// Default 64.
    #[serde(
        default = "d::inline_paste_chunk_bytes",
        deserialize_with = "paste_chunk_bytes"
    )]
    pub paste_chunk_bytes: usize,
    /// Milliseconds between paste chunks, for bootloaders that drop bytes; 0 sends them
    /// back to back. Default 10.
    #[serde(
        default = "d::inline_paste_chunk_delay_ms",
        deserialize_with = "paste_chunk_delay_ms"
    )]
    pub paste_chunk_delay_ms: u64,
}

impl Default for InlineSettings {
    /// The bundled defaults.
    fn default() -> Self {
        d::inline()
    }
}

impl InlineSettings {
    /// The pause between paste chunks.
    pub fn paste_chunk_delay(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.paste_chunk_delay_ms)
    }
}
