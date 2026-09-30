//! Keys whose bytes depend on the terminal's modes.
//!
//! The UI's inline encoder turns keystrokes into xterm's normal-mode bytes. In VT mode a
//! device can switch the cursor keys to application mode (DECCKM, `CSI ? 1 h`, which
//! full-screen programs and some bootloader menus do), and then an unmodified arrow sends
//! `ESC O A` instead of `ESC [ A`. [`vt_key_bytes`] gives those bytes; the encoder asks it
//! first for an unmodified key and falls back to its own table when it returns `None`.
//!
//! With a modifier held, xterm sends the same `CSI 1 ; m A` form in both modes, so
//! modified keys never need this.

/// The bytes an unmodified cursor key sends, by the UI's key name (`"up"`, `"down"`,
/// `"right"`, `"left"`, `"home"`, `"end"`), in application cursor mode (`true`,
/// [`VtModes::app_cursor_keys`](crate::VtModes::app_cursor_keys)) or normal mode.
/// `None` for every other key: its bytes do not depend on the cursor key mode.
pub fn vt_key_bytes(key: &str, app_cursor_mode: bool) -> Option<&'static [u8]> {
    let (normal, application): (&'static [u8], &'static [u8]) = match key {
        "up" => (b"\x1b[A", b"\x1bOA"),
        "down" => (b"\x1b[B", b"\x1bOB"),
        "right" => (b"\x1b[C", b"\x1bOC"),
        "left" => (b"\x1b[D", b"\x1bOD"),
        "home" => (b"\x1b[H", b"\x1bOH"),
        "end" => (b"\x1b[F", b"\x1bOF"),
        _ => return None,
    };
    Some(if app_cursor_mode { application } else { normal })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrows_follow_the_cursor_key_mode() {
        assert_eq!(vt_key_bytes("up", false), Some(&b"\x1b[A"[..]));
        assert_eq!(vt_key_bytes("up", true), Some(&b"\x1bOA"[..]));
        assert_eq!(vt_key_bytes("down", true), Some(&b"\x1bOB"[..]));
        assert_eq!(vt_key_bytes("right", false), Some(&b"\x1b[C"[..]));
        assert_eq!(vt_key_bytes("left", true), Some(&b"\x1bOD"[..]));
        assert_eq!(vt_key_bytes("home", true), Some(&b"\x1bOH"[..]));
        assert_eq!(vt_key_bytes("end", false), Some(&b"\x1b[F"[..]));
    }

    #[test]
    fn other_keys_are_left_to_the_encoder() {
        for key in ["a", "enter", "pageup", "f1", "delete", "tab"] {
            assert_eq!(vt_key_bytes(key, true), None, "{key}");
        }
    }
}
