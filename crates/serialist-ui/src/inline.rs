//! Inline interactive mode: every keystroke goes to the port as it is typed, the way
//! picocom or a real terminal behaves.
//!
//! A session is in one of two [`Mode`]s. In [`Mode::Command`] the compose bar sends a
//! line at a time. In [`Mode::Inline`] the compose bar is hidden, the terminal has the
//! focus under the `TerminalInline` key context, and every key event is encoded by
//! [`KeyEncoder`] and written to the session at once, one write per key.
//!
//! # Encoding
//!
//! [`encode_key`] turns a GPUI [`Keystroke`] into the bytes an xterm-style terminal
//! sends for it (normal cursor mode; in monitor mode no terminal modes are tracked):
//!
//! | Key | Bytes |
//! | --- | --- |
//! | printable characters | their UTF-8 |
//! | Enter | the session's line ending (CR when it is `none`) |
//! | Backspace | `inline.backspace`: 0x7f (default) or 0x08; ctrl-Backspace sends the other |
//! | Tab, shift-Tab | 0x09, `ESC [ Z` |
//! | Escape | 0x1b |
//! | Up, Down, Right, Left, Home, End | `ESC [ A` `B` `C` `D` `H` `F` |
//! | Insert, Delete, PageUp, PageDown | `ESC [ 2 ~`, `3 ~`, `5 ~`, `6 ~` |
//! | F1 to F4 | `ESC O P` to `ESC O S` |
//! | F5 to F12 | `ESC [ 15 ~`, `17`, `18`, `19`, `20`, `21`, `23`, `24 ~` |
//! | ctrl-a to ctrl-z | 0x01 to 0x1a (ctrl-c included) |
//! | ctrl-@ (ctrl-space, ctrl-2) | 0x00 |
//! | ctrl-\[ ctrl-\\ ctrl-\] ctrl-^ ctrl-_ | 0x1b 0x1c 0x1d 0x1e 0x1f (and ctrl-3 to ctrl-7) |
//! | ctrl-? (ctrl-8) | 0x7f |
//!
//! Shift and ctrl on the cursor, editing and function keys add xterm's modifier
//! parameter (`ESC [ 1 ; 5 A` is ctrl-Up). Alt sends ESC before whatever the key sends
//! without it. Keys with the platform modifier (cmd, super) send nothing: they are the
//! app's shortcuts.
//!
//! In VT mode the screen tracks the device's modes, and the encoder is told the cursor
//! key mode ([`KeyEncoder::with_cursor_keys`]): an unmodified arrow, Home or End then
//! sends what [`serialist_vt::vt_key_bytes`] gives, `ESC O A` in application cursor
//! mode (DECCKM). A paste into a screen in bracketed paste mode is wrapped in
//! `ESC [ 200 ~` and `ESC [ 201 ~` ([`bracketed`]).
//!
//! # Leaving
//!
//! The escape chord (`inline.escape_chord`, default `ctrl-]`) leaves inline mode instead
//! of being sent. Pressing it twice within [`DOUBLE_PRESS`] sends it: the first press
//! leaves, the second comes back into inline mode and writes the chord's byte, so a
//! device that needs ctrl-] still gets it. The mode toggle (`terminal::ToggleInline`)
//! leaves too.
//!
//! # Local echo
//!
//! With local echo on, what is typed appears in the scrollback as it is typed: each
//! key's [`Echo`] goes to the session's ingest thread, which types it into a `Tx` line
//! with [`IngestHandle::append_local_inline`](serialist_core::IngestHandle::append_local_inline).
//! Text grows the line, Backspace takes the last character back
//! ([`truncate_local_line`](serialist_core::IngestHandle::truncate_local_line)), Enter
//! ends the line, and control keys echo as nothing. A received line in progress (a
//! prompt without its newline) is not interrupted: the typed line follows it, and a
//! line the device prints between two keystrokes splits what was typed in two. The
//! store's module docs give the exact rules under "Typing in line". Leaving inline mode
//! ends the line being typed.

use std::time::{Duration, Instant};

use serialist_core::LineEnding;

use crate::prelude::*;

/// ESC, which starts every escape sequence and prefixes alt chords.
pub const ESC: u8 = 0x1b;

/// Two presses of the escape chord closer than this send the chord to the device.
pub const DOUBLE_PRESS: Duration = Duration::from_millis(500);

/// Pastes longer than this many chunks show their progress in the status line.
pub const PASTE_PROGRESS_CHUNKS: usize = 4;

/// How a session takes keyboard input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Mode {
    /// The compose bar sends a line at a time; saved commands send from the panel.
    #[default]
    Command,
    /// Every keystroke goes to the port as it is typed.
    Inline,
}

impl Mode {
    /// What the status line shows.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Command => "COMMAND",
            Mode::Inline => "INLINE",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            Mode::Command => Mode::Inline,
            Mode::Inline => Mode::Command,
        }
    }
}

// --- Settings ----------------------------------------------------------------------

/// The `inline.*` settings as the UI uses them: [`serialist_core::InlineSettings`] (which
/// parses and validates the `inline` object) with the escape chord turned into a
/// keystroke and the delay into a `Duration`.
///
/// ```jsonc
/// "inline": {
///   "backspace": "del",         // or "bs" (0x08)
///   "escape_chord": "ctrl-]",   // leaves inline mode; press twice to send it
///   "paste_chunk_bytes": 64,    // paste is written in chunks this size...
///   "paste_chunk_delay_ms": 10  // ...this far apart, for bootloaders that drop bytes
/// }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineConfig {
    /// What Backspace sends: 0x7f (DEL) or 0x08 (BS).
    pub backspace: u8,
    /// Leaves inline mode instead of being sent.
    pub escape_chord: Keystroke,
    /// Bytes per paste chunk; at least 1.
    pub paste_chunk_bytes: usize,
    /// Pause between paste chunks.
    pub paste_chunk_delay: Duration,
}

/// The chord the settings default to, for when a configured one cannot be parsed.
const DEFAULT_CHORD: &str = "ctrl-]";

impl Default for InlineConfig {
    fn default() -> Self {
        Self::from_settings(&serialist_core::InlineSettings::default())
    }
}

impl InlineConfig {
    /// The values `settings` holds. The core settings already checked the chord's
    /// syntax; a chord GPUI still cannot parse falls back to `ctrl-]`.
    pub fn from_settings(settings: &serialist_core::InlineSettings) -> Self {
        let escape_chord = Keystroke::parse(&settings.escape_chord).unwrap_or_else(|_| {
            tracing::warn!(
                chord = %settings.escape_chord,
                "inline.escape_chord is not a keystroke; using {DEFAULT_CHORD}"
            );
            Keystroke::parse(DEFAULT_CHORD).expect("the default chord parses")
        });
        Self {
            backspace: settings.backspace.byte(),
            escape_chord,
            paste_chunk_bytes: settings.paste_chunk_bytes.max(1),
            paste_chunk_delay: settings.paste_chunk_delay(),
        }
    }
}
// --- Encoding ----------------------------------------------------------------------

/// What a key does to the local echo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Echo {
    /// Printable text, echoed as typed.
    Text(String),
    /// Enter: the pending line is complete.
    Enter,
    /// Backspace: take back the last character.
    Backspace,
    /// A control key or escape sequence: echoed as nothing.
    Nothing,
}

/// The bytes one key sends and what it does to the echo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedKey {
    pub bytes: Vec<u8>,
    pub echo: Echo,
}

/// Encodes keystrokes for one session: its line ending on Enter and its Backspace byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyEncoder {
    /// What Enter sends.
    pub enter: Vec<u8>,
    /// What Backspace sends.
    pub backspace: u8,
    /// In VT mode, the screen's cursor key mode: `true` in application mode (DECCKM).
    /// `None` in monitor mode, where the cursor keys always send the normal form.
    pub cursor_keys: Option<bool>,
}

impl Default for KeyEncoder {
    /// Enter sends CR and Backspace sends DEL, as most terminals do.
    fn default() -> Self {
        Self {
            enter: b"\r".to_vec(),
            backspace: 0x7f,
            cursor_keys: None,
        }
    }
}

/// The bytes a terminal sends for `keystroke`, with Enter as CR and Backspace as DEL,
/// or `None` for a key that sends nothing (a platform-modifier shortcut, a lone
/// modifier, a key with no terminal meaning). See the module docs for the table.
pub fn encode_key(keystroke: &Keystroke) -> Option<Vec<u8>> {
    KeyEncoder::default()
        .encode(keystroke)
        .map(|encoded| encoded.bytes)
}

impl KeyEncoder {
    /// Enter sends `line_ending` (CR when it is `none`: a key must send something),
    /// Backspace sends `backspace`.
    pub fn new(line_ending: LineEnding, backspace: u8) -> Self {
        let enter = match line_ending.bytes() {
            [] => b"\r".to_vec(),
            bytes => bytes.to_vec(),
        };
        Self {
            enter,
            backspace,
            cursor_keys: None,
        }
    }

    /// Encode unmodified cursor keys (arrows, Home, End) for a VT screen whose cursor key
    /// mode is `application` (see [`serialist_vt::vt_key_bytes`]); `None` for monitor
    /// mode.
    pub fn with_cursor_keys(mut self, application: Option<bool>) -> Self {
        self.cursor_keys = application;
        self
    }

    /// What `keystroke` sends, or `None` if it sends nothing.
    pub fn encode(&self, keystroke: &Keystroke) -> Option<EncodedKey> {
        let modifiers = keystroke.modifiers;
        if modifiers.platform {
            return None;
        }
        let (mut bytes, echo) = self.encode_without_alt(keystroke)?;
        if modifiers.alt {
            bytes.insert(0, ESC);
            return Some(EncodedKey {
                bytes,
                echo: Echo::Nothing,
            });
        }
        Some(EncodedKey { bytes, echo })
    }

    fn encode_without_alt(&self, keystroke: &Keystroke) -> Option<(Vec<u8>, Echo)> {
        let modifiers = keystroke.modifiers;
        // xterm's modifier parameter: 1 + shift + 4 * ctrl (alt is the ESC prefix).
        let parameter = 1 + u8::from(modifiers.shift) + 4 * u8::from(modifiers.control);
        let control = |bytes: Vec<u8>| Some((bytes, Echo::Nothing));
        // A VT screen's cursor key mode decides the unmodified cursor keys.
        if let Some(application) = self.cursor_keys
            && parameter == 1
            && !modifiers.alt
            && let Some(bytes) = serialist_vt::vt_key_bytes(&keystroke.key, application)
        {
            return control(bytes.to_vec());
        }
        match keystroke.key.as_str() {
            "enter" => Some((self.enter.clone(), Echo::Enter)),
            "tab" if modifiers.shift => control(b"\x1b[Z".to_vec()),
            "tab" => control(b"\t".to_vec()),
            "escape" => control(vec![ESC]),
            "backspace" if modifiers.control => {
                control(vec![if self.backspace == 0x7f { 0x08 } else { 0x7f }])
            }
            "backspace" => Some((vec![self.backspace], Echo::Backspace)),
            "space" if modifiers.control => control(vec![0x00]),
            "space" => Some((b" ".to_vec(), Echo::Text(" ".to_owned()))),
            "up" => control(csi_final(b'A', parameter)),
            "down" => control(csi_final(b'B', parameter)),
            "right" => control(csi_final(b'C', parameter)),
            "left" => control(csi_final(b'D', parameter)),
            "home" => control(csi_final(b'H', parameter)),
            "end" => control(csi_final(b'F', parameter)),
            "insert" => control(csi_tilde(2, parameter)),
            "delete" => control(csi_tilde(3, parameter)),
            "pageup" => control(csi_tilde(5, parameter)),
            "pagedown" => control(csi_tilde(6, parameter)),
            "f1" => control(ss3_final(b'P', parameter)),
            "f2" => control(ss3_final(b'Q', parameter)),
            "f3" => control(ss3_final(b'R', parameter)),
            "f4" => control(ss3_final(b'S', parameter)),
            "f5" => control(csi_tilde(15, parameter)),
            "f6" => control(csi_tilde(17, parameter)),
            "f7" => control(csi_tilde(18, parameter)),
            "f8" => control(csi_tilde(19, parameter)),
            "f9" => control(csi_tilde(20, parameter)),
            "f10" => control(csi_tilde(21, parameter)),
            "f11" => control(csi_tilde(23, parameter)),
            "f12" => control(csi_tilde(24, parameter)),
            _ if modifiers.control => control(vec![ctrl_byte(keystroke)?]),
            _ => {
                let text = printable_text(keystroke)?;
                Some((text.as_bytes().to_vec(), Echo::Text(text)))
            }
        }
    }
}

/// `ESC [ <final>`, or `ESC [ 1 ; <parameter> <final>` with modifiers.
fn csi_final(final_byte: u8, parameter: u8) -> Vec<u8> {
    if parameter == 1 {
        vec![ESC, b'[', final_byte]
    } else {
        format!("\x1b[1;{parameter}{}", final_byte as char).into_bytes()
    }
}

/// `ESC O <final>` for F1 to F4, which take the CSI form with modifiers.
fn ss3_final(final_byte: u8, parameter: u8) -> Vec<u8> {
    if parameter == 1 {
        vec![ESC, b'O', final_byte]
    } else {
        csi_final(final_byte, parameter)
    }
}

/// `ESC [ <number> ~`, or `ESC [ <number> ; <parameter> ~` with modifiers.
fn csi_tilde(number: u8, parameter: u8) -> Vec<u8> {
    if parameter == 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{parameter}~").into_bytes()
    }
}

/// The C0 byte a ctrl chord sends, as xterm maps it, or `None` for a chord with no
/// control code (ctrl-1, ctrl-.).
fn ctrl_byte(keystroke: &Keystroke) -> Option<u8> {
    let mut chars = keystroke.key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' | '{' => 0x1b,
        '\\' | '4' | '|' => 0x1c,
        ']' | '5' | '}' => 0x1d,
        '^' | '6' | '~' | '`' => 0x1e,
        '_' | '-' | '7' | '/' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// The text an unmodified (or shifted) key types: what the platform says it typed,
/// else the key itself, upper-cased with shift for letters.
fn printable_text(keystroke: &Keystroke) -> Option<String> {
    if !keystroke.modifiers.alt
        && let Some(typed) = keystroke.key_char.as_deref()
        && !typed.is_empty()
        && !typed.chars().any(char::is_control)
    {
        return Some(typed.to_owned());
    }
    let mut chars = keystroke.key.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        return None;
    };
    if c.is_control() {
        return None;
    }
    let c = if keystroke.modifiers.shift {
        c.to_ascii_uppercase()
    } else {
        c
    };
    Some(c.to_string())
}

/// Whether `pressed` is the chord `chord`: the same key with the same ctrl, alt, shift
/// and platform modifiers.
pub fn is_chord(chord: &Keystroke, pressed: &Keystroke) -> bool {
    let (a, b) = (chord.modifiers, pressed.modifiers);
    chord.key.eq_ignore_ascii_case(&pressed.key)
        && a.control == b.control
        && a.alt == b.alt
        && a.shift == b.shift
        && a.platform == b.platform
}

/// `bytes` as a bracketed paste: between `ESC [ 200 ~` and `ESC [ 201 ~`, which a
/// terminal in bracketed paste mode (`CSI ? 2004 h`) sends so the program can tell a
/// paste from typing.
pub fn bracketed(bytes: &[u8]) -> Vec<u8> {
    [b"\x1b[200~".as_slice(), bytes, b"\x1b[201~"].concat()
}

// --- The escape chord --------------------------------------------------------------

/// Tracks the escape chord across the two modes, so a quick second press sends it.
///
/// A press counts as the second one only after the first one's key came up, so a held
/// chord (auto-repeat) leaves once and does not bounce between the modes.
#[derive(Clone, Debug, Default)]
pub struct EscapeChord {
    left_at: Option<Instant>,
    held: bool,
}

impl EscapeChord {
    /// The chord was pressed in inline mode at `now`: inline mode ends.
    pub fn left(&mut self, now: Instant) {
        self.left_at = Some(now);
        self.held = true;
    }

    /// The chord's key came up, or another key went down.
    pub fn released(&mut self) {
        self.held = false;
    }

    /// The key that left inline mode is still down.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// The chord was pressed in command mode at `now`. Returns true if this is the
    /// second press of a double press, which goes back to inline mode and sends it.
    pub fn pressed_again(&mut self, now: Instant) -> bool {
        if self.held {
            return false;
        }
        match self.left_at.take() {
            Some(left) => now.saturating_duration_since(left) <= DOUBLE_PRESS,
            None => false,
        }
    }

    /// Forget a first press, as when the mode changes some other way.
    pub fn reset(&mut self) {
        self.left_at = None;
        self.held = false;
    }
}

// --- Local echo --------------------------------------------------------------------

/// The text that echoes a paste of `text` in the scrollback: `text` with every line break
/// (CRLF, LF or a lone CR, as [`paste_bytes`] sends them) as one `\n`, which ends the
/// echoed line. The store drops other control characters and expands tabs.
pub fn paste_echo(text: &str) -> String {
    let mut echo = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                echo.push('\n');
            }
            c => echo.push(c),
        }
    }
    echo
}
// --- Paste -------------------------------------------------------------------------

/// The bytes a paste of `text` sends: its UTF-8 with every line break (CRLF, LF or CR)
/// replaced by what Enter sends, as a terminal pastes.
pub fn paste_bytes(text: &str, enter: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut buffer = [0; 4];
    while let Some(c) = chars.next() {
        match c {
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                bytes.extend_from_slice(enter);
            }
            c => bytes.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes()),
        }
    }
    bytes
}

/// A paste in progress, for the status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PasteProgress {
    pub sent: usize,
    pub total: usize,
    pub chunks: usize,
}

impl PasteProgress {
    /// `Pasting 128 B of 1.0 KiB` for a paste of more than a few chunks.
    pub fn label(&self) -> Option<String> {
        (self.chunks > PASTE_PROGRESS_CHUNKS).then(|| {
            format!(
                "Pasting {} of {}",
                crate::status::format_bytes(self.sent as u64),
                crate::status::format_bytes(self.total as u64)
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(source: &str) -> Keystroke {
        Keystroke::parse(source).unwrap_or_else(|error| panic!("{source}: {error}"))
    }

    /// As the platform delivers a typed character: the key and the text it typed.
    fn typed(source: &str, text: &str) -> Keystroke {
        Keystroke {
            key_char: Some(text.to_owned()),
            ..key(source)
        }
    }

    fn bytes(source: &str) -> Option<Vec<u8>> {
        encode_key(&key(source))
    }

    #[test]
    fn printable_characters_send_their_utf8() {
        assert_eq!(encode_key(&typed("a", "a")), Some(b"a".to_vec()));
        assert_eq!(encode_key(&typed("shift-a", "A")), Some(b"A".to_vec()));
        assert_eq!(encode_key(&typed("1", "1")), Some(b"1".to_vec()));
        assert_eq!(encode_key(&typed("shift-1", "!")), Some(b"!".to_vec()));
        assert_eq!(encode_key(&typed("e", "é")), Some("é".as_bytes().to_vec()));
        // Without key_char, as `Keystroke::parse` leaves it.
        assert_eq!(bytes("a"), Some(b"a".to_vec()));
        assert_eq!(bytes("shift-a"), Some(b"A".to_vec()));
        assert_eq!(bytes("A"), Some(b"A".to_vec()));
        assert_eq!(bytes("/"), Some(b"/".to_vec()));
        assert_eq!(bytes("space"), Some(b" ".to_vec()));
    }

    #[test]
    fn enter_backspace_tab_and_escape() {
        assert_eq!(bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(encode_key(&typed("enter", "\n")), Some(b"\r".to_vec()));
        assert_eq!(bytes("shift-enter"), Some(b"\r".to_vec()));
        assert_eq!(bytes("backspace"), Some(vec![0x7f]));
        assert_eq!(bytes("ctrl-backspace"), Some(vec![0x08]));
        assert_eq!(bytes("tab"), Some(b"\t".to_vec()));
        assert_eq!(encode_key(&typed("tab", "\t")), Some(b"\t".to_vec()));
        assert_eq!(bytes("shift-tab"), Some(b"\x1b[Z".to_vec()));
        assert_eq!(bytes("escape"), Some(vec![0x1b]));
    }

    #[test]
    fn the_session_line_ending_and_backspace_setting_apply() {
        let encoder = KeyEncoder::new(LineEnding::Crlf, 0x08);
        let enter = encoder.encode(&key("enter")).unwrap();
        assert_eq!(enter.bytes, b"\r\n");
        assert_eq!(enter.echo, Echo::Enter);
        assert_eq!(KeyEncoder::new(LineEnding::Lf, 0x7f).enter, b"\n");
        assert_eq!(KeyEncoder::new(LineEnding::Cr, 0x7f).enter, b"\r");
        assert_eq!(
            KeyEncoder::new(LineEnding::None, 0x7f).enter,
            b"\r",
            "Enter always sends something"
        );
        let backspace = encoder.encode(&key("backspace")).unwrap();
        assert_eq!(backspace.bytes, [0x08]);
        assert_eq!(backspace.echo, Echo::Backspace);
        assert_eq!(
            encoder.encode(&key("ctrl-backspace")).unwrap().bytes,
            [0x7f]
        );
    }

    #[test]
    fn cursor_and_editing_keys_send_ansi_sequences() {
        let table: &[(&str, &[u8])] = &[
            ("up", b"\x1b[A"),
            ("down", b"\x1b[B"),
            ("right", b"\x1b[C"),
            ("left", b"\x1b[D"),
            ("home", b"\x1b[H"),
            ("end", b"\x1b[F"),
            ("insert", b"\x1b[2~"),
            ("delete", b"\x1b[3~"),
            ("pageup", b"\x1b[5~"),
            ("pagedown", b"\x1b[6~"),
            ("shift-up", b"\x1b[1;2A"),
            ("ctrl-left", b"\x1b[1;5D"),
            ("ctrl-shift-right", b"\x1b[1;6C"),
            ("shift-home", b"\x1b[1;2H"),
            ("ctrl-end", b"\x1b[1;5F"),
            ("shift-pageup", b"\x1b[5;2~"),
            ("ctrl-delete", b"\x1b[3;5~"),
            // The function key on a laptop keyboard changes nothing.
            ("fn-up", b"\x1b[A"),
        ];
        for (source, expected) in table {
            assert_eq!(bytes(source).as_deref(), Some(*expected), "{source}");
        }
    }

    #[test]
    fn a_vt_screen_in_application_cursor_mode_changes_only_the_plain_cursor_keys() {
        let application = KeyEncoder::default().with_cursor_keys(Some(true));
        let normal = KeyEncoder::default().with_cursor_keys(Some(false));
        let sent = |encoder: &KeyEncoder, source: &str| encoder.encode(&key(source)).unwrap().bytes;
        let table: &[(&str, &[u8], &[u8])] = &[
            ("up", b"\x1bOA", b"\x1b[A"),
            ("down", b"\x1bOB", b"\x1b[B"),
            ("right", b"\x1bOC", b"\x1b[C"),
            ("left", b"\x1bOD", b"\x1b[D"),
            ("home", b"\x1bOH", b"\x1b[H"),
            ("end", b"\x1bOF", b"\x1b[F"),
            // Modified keys take the CSI form in both modes; other keys do not change.
            ("shift-up", b"\x1b[1;2A", b"\x1b[1;2A"),
            ("ctrl-left", b"\x1b[1;5D", b"\x1b[1;5D"),
            ("alt-up", b"\x1b\x1b[A", b"\x1b\x1b[A"),
            ("pageup", b"\x1b[5~", b"\x1b[5~"),
            ("f1", b"\x1bOP", b"\x1bOP"),
            ("a", b"a", b"a"),
        ];
        for (source, in_application, in_normal) in table {
            assert_eq!(sent(&application, source), *in_application, "{source}");
            assert_eq!(sent(&normal, source), *in_normal, "{source}");
        }
        assert_eq!(bracketed(b"ls\r"), b"\x1b[200~ls\r\x1b[201~");
    }

    #[test]
    fn function_keys_send_xterm_sequences() {
        let table: &[(&str, &[u8])] = &[
            ("f1", b"\x1bOP"),
            ("f2", b"\x1bOQ"),
            ("f3", b"\x1bOR"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f6", b"\x1b[17~"),
            ("f7", b"\x1b[18~"),
            ("f8", b"\x1b[19~"),
            ("f9", b"\x1b[20~"),
            ("f10", b"\x1b[21~"),
            ("f11", b"\x1b[23~"),
            ("f12", b"\x1b[24~"),
            ("shift-f1", b"\x1b[1;2P"),
            ("ctrl-f4", b"\x1b[1;5S"),
            ("shift-f5", b"\x1b[15;2~"),
            ("ctrl-f12", b"\x1b[24;5~"),
        ];
        for (source, expected) in table {
            assert_eq!(bytes(source).as_deref(), Some(*expected), "{source}");
        }
        assert_eq!(bytes("f13"), None, "no sequence past F12");
    }

    #[test]
    fn ctrl_letters_send_c0_codes() {
        for (i, letter) in ('a'..='z').enumerate() {
            let expected = vec![i as u8 + 1];
            assert_eq!(bytes(&format!("ctrl-{letter}")), Some(expected.clone()));
            assert_eq!(
                bytes(&format!("ctrl-shift-{letter}")),
                Some(expected),
                "shift makes no difference to a ctrl letter"
            );
        }
        assert_eq!(
            bytes("ctrl-c"),
            Some(vec![0x03]),
            "ctrl-c goes to the device"
        );
        assert_eq!(bytes("ctrl-z"), Some(vec![0x1a]));
    }

    #[test]
    fn ctrl_punctuation_sends_the_rest_of_c0() {
        let table: &[(&str, u8)] = &[
            ("ctrl-@", 0x00),
            ("ctrl-space", 0x00),
            ("ctrl-2", 0x00),
            ("ctrl-[", 0x1b),
            ("ctrl-3", 0x1b),
            ("ctrl-\\", 0x1c),
            ("ctrl-4", 0x1c),
            ("ctrl-]", 0x1d),
            ("ctrl-5", 0x1d),
            ("ctrl-^", 0x1e),
            ("ctrl-6", 0x1e),
            ("ctrl-shift-6", 0x1e),
            ("ctrl-_", 0x1f),
            ("ctrl-shift--", 0x1f),
            ("ctrl-7", 0x1f),
            ("ctrl-/", 0x1f),
            ("ctrl-?", 0x7f),
            ("ctrl-8", 0x7f),
        ];
        for (source, expected) in table {
            assert_eq!(bytes(source), Some(vec![*expected]), "{source}");
        }
        assert_eq!(bytes("ctrl-1"), None, "no control code");
        assert_eq!(bytes("ctrl-."), None);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(bytes("alt-a"), Some(b"\x1ba".to_vec()));
        // macOS reports the option-composed character; alt still sends the key.
        assert_eq!(encode_key(&typed("alt-a", "å")), Some(b"\x1ba".to_vec()));
        assert_eq!(bytes("alt-shift-a"), Some(b"\x1bA".to_vec()));
        assert_eq!(bytes("alt-ctrl-c"), Some(vec![0x1b, 0x03]));
        assert_eq!(bytes("alt-up"), Some(b"\x1b\x1b[A".to_vec()));
        assert_eq!(bytes("alt-backspace"), Some(vec![0x1b, 0x7f]));
        assert_eq!(bytes("alt-enter"), Some(b"\x1b\r".to_vec()));
        assert_eq!(bytes("alt-escape"), Some(vec![0x1b, 0x1b]));
    }

    #[test]
    fn platform_shortcuts_and_lone_modifiers_send_nothing() {
        for source in [
            "cmd-v",
            "cmd-c",
            "cmd-shift-a",
            "cmd-enter",
            "cmd-up",
            "shift",
            "ctrl",
        ] {
            assert_eq!(bytes(source), None, "{source}");
        }
    }

    #[test]
    fn keys_say_what_they_do_to_the_echo() {
        let encoder = KeyEncoder::default();
        let echo = |source: &str| encoder.encode(&typed(source, source)).unwrap().echo;
        assert_eq!(echo("a"), Echo::Text("a".into()));
        assert_eq!(
            encoder.encode(&key("space")).unwrap().echo,
            Echo::Text(" ".into())
        );
        assert_eq!(
            encoder.encode(&typed("shift-a", "A")).unwrap().echo,
            Echo::Text("A".into())
        );
        assert_eq!(
            encoder.encode(&key("backspace")).unwrap().echo,
            Echo::Backspace
        );
        assert_eq!(encoder.encode(&key("enter")).unwrap().echo, Echo::Enter);
        for control in ["left", "ctrl-c", "escape", "tab", "f5", "ctrl-backspace"] {
            assert_eq!(
                encoder.encode(&key(control)).unwrap().echo,
                Echo::Nothing,
                "{control}"
            );
        }
        assert_eq!(
            encoder.encode(&key("alt-b")).unwrap().echo,
            Echo::Nothing,
            "an alt chord is a control sequence"
        );
    }

    #[test]
    fn a_pastes_echo_ends_a_line_at_every_break() {
        assert_eq!(
            paste_echo("one\r\ntwo\nthree\rfour"),
            "one\ntwo\nthree\nfour"
        );
        assert_eq!(paste_echo("tail\n"), "tail\n");
        assert_eq!(paste_echo("\r\n\r\n"), "\n\n");
        assert_eq!(paste_echo("x\n\ry"), "x\n\ny", "LF then CR is two breaks");
        assert_eq!(paste_echo("caf\u{e9}\t!"), "caf\u{e9}\t!");
        assert_eq!(paste_echo(""), "");
    }

    #[test]
    fn paste_bytes_turn_line_breaks_into_enter() {
        assert_eq!(paste_bytes("a\r\nb\nc\rd", b"\r\n"), b"a\r\nb\r\nc\r\nd");
        assert_eq!(paste_bytes("x\n", b"\r"), b"x\r");
        assert_eq!(paste_bytes("héllo", b"\r"), "héllo".as_bytes());
    }

    #[test]
    fn the_escape_chord_matches_exactly() {
        let chord = key("ctrl-]");
        assert!(is_chord(&chord, &key("ctrl-]")));
        assert!(!is_chord(&chord, &key("ctrl-shift-]")));
        assert!(!is_chord(&chord, &key("]")));
        assert!(!is_chord(&chord, &key("ctrl-[")));
    }

    #[test]
    fn a_quick_second_press_sends_the_chord() {
        let start = Instant::now();
        let mut chord = EscapeChord::default();
        assert!(!chord.pressed_again(start), "no first press yet");
        chord.left(start);
        assert!(
            !chord.pressed_again(start + Duration::from_millis(50)),
            "auto-repeat of the key that left"
        );
        chord.released();
        assert!(chord.pressed_again(start + Duration::from_millis(200)));
        assert!(
            !chord.pressed_again(start + Duration::from_millis(300)),
            "a double press is used up"
        );
        chord.left(start);
        chord.released();
        assert!(!chord.pressed_again(start + DOUBLE_PRESS + Duration::from_millis(1)));
    }

    #[test]
    fn the_config_follows_the_core_settings() {
        let defaults = InlineConfig::default();
        assert_eq!(defaults.backspace, 0x7f);
        assert_eq!(defaults.escape_chord, key("ctrl-]"));
        assert_eq!(defaults.paste_chunk_bytes, 64);
        assert_eq!(defaults.paste_chunk_delay, Duration::from_millis(10));
        assert_eq!(
            defaults,
            InlineConfig::from_settings(&serialist_core::Settings::default().inline)
        );

        let settings = serialist_core::Settings::from_jsonc(
            r#"{ "inline": { "backspace": "bs", "escape_chord": "ctrl-a",
                             "paste_chunk_bytes": 16, "paste_chunk_delay_ms": 25 } }"#,
        )
        .unwrap();
        let config = InlineConfig::from_settings(&settings.inline);
        assert_eq!(config.backspace, 0x08);
        assert_eq!(config.escape_chord, key("ctrl-a"));
        assert_eq!(config.paste_chunk_bytes, 16);
        assert_eq!(config.paste_chunk_delay, Duration::from_millis(25));

        // A chord GPUI cannot parse (the core would have refused it) falls back to the
        // default.
        let odd = serialist_core::InlineSettings {
            escape_chord: "ctrl-a-b".to_owned(),
            ..serialist_core::InlineSettings::default()
        };
        assert_eq!(
            InlineConfig::from_settings(&odd).escape_chord,
            key("ctrl-]")
        );
    }

    #[test]
    fn large_pastes_show_progress() {
        let small = PasteProgress {
            sent: 0,
            total: 100,
            chunks: 2,
        };
        assert_eq!(small.label(), None);
        let large = PasteProgress {
            sent: 128,
            total: 2048,
            chunks: 32,
        };
        assert_eq!(large.label().as_deref(), Some("Pasting 128 B of 2.0 KiB"));
    }
}
