//! Mouse reports: the bytes a terminal sends a device that asked for the mouse.
//!
//! A device turns reporting on with one of three modes, the last one set winning:
//!
//! | Mode | Sequence | Reported |
//! | --- | --- | --- |
//! | [`MouseMode::Click`] | `CSI ? 1000 h` | presses, releases and the wheel |
//! | [`MouseMode::Drag`] | `CSI ? 1002 h` | those, and moves while a button is down |
//! | [`MouseMode::Motion`] | `CSI ? 1003 h` | those, and every move |
//!
//! and picks how a report is written with an encoding: [`MouseEncoding::Sgr`]
//! (`CSI ? 1006 h`), `CSI < b ; x ; y M` for a press and `m` for a release, with no limit
//! on the coordinates; [`MouseEncoding::Utf8`] (`CSI ? 1005 h`); or, when it asked for
//! neither, xterm's original form [`MouseEncoding::X10`], `CSI M` and three bytes, each
//! 32 more than its value, which cannot say a column or row past 223 (such a report is
//! not sent) and says a release without saying which button.
//!
//! The first byte's value is the button (0 left, 1 middle, 2 right; 64 and 65 the wheel
//! up and down; 3 a release in X10 and UTF-8, or a move with no button), plus 32 for a
//! move, 8 with Alt and 16 with Control. Shift is not reported: with Shift held the
//! terminal keeps the mouse for its own selection, as terminals do. Coordinates count
//! from 1. A wheel step is a press with no release.

/// Which mouse events the device asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseMode {
    /// `CSI ? 1000 h`: presses, releases and the wheel.
    Click,
    /// `CSI ? 1002 h`: also moves with a button down.
    Drag,
    /// `CSI ? 1003 h`: also every move.
    Motion,
}

/// How reports are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MouseEncoding {
    /// `CSI M` and three bytes of 32 plus the value: the default.
    #[default]
    X10,
    /// `CSI ? 1005 h`: as X10, with values past 95 written as UTF-8 characters.
    Utf8,
    /// `CSI ? 1006 h`: `CSI < b ; x ; y M` or `m`.
    Sgr,
}

/// The device's mouse reporting, when it asked for any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MouseReporting {
    pub mode: MouseMode,
    pub encoding: MouseEncoding,
}

/// A mouse button, as reports name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

impl MouseButton {
    fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    }
}

/// What the mouse did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    /// The pointer moved to another cell, with this button down, or none.
    Move(Option<MouseButton>),
    /// One step of the wheel, up (towards older text) or down.
    Wheel {
        up: bool,
    },
}

/// A mouse event on a screen cell (0-based), with the modifiers a report carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    pub action: MouseAction,
    pub column: usize,
    pub row: usize,
    pub alt: bool,
    pub control: bool,
}

impl MouseReporting {
    /// Whether the device asked to hear about `action` in this mode.
    pub fn reports(&self, action: MouseAction) -> bool {
        match action {
            MouseAction::Press(_) | MouseAction::Release(_) | MouseAction::Wheel { .. } => true,
            MouseAction::Move(Some(_)) => self.mode != MouseMode::Click,
            MouseAction::Move(None) => self.mode == MouseMode::Motion,
        }
    }

    /// The bytes of the report of `event`, or `None` when this mode does not report it
    /// or the encoding cannot say where it was.
    pub fn report(&self, event: MouseEvent) -> Option<Vec<u8>> {
        if !self.reports(event.action) {
            return None;
        }
        let sgr = self.encoding == MouseEncoding::Sgr;
        let mut code = match event.action {
            MouseAction::Press(button) => button.code(),
            // SGR says which button came up; the others only that one did.
            MouseAction::Release(button) if sgr => button.code(),
            MouseAction::Release(_) => 3,
            MouseAction::Move(button) => 32 + button.map_or(3, MouseButton::code),
            MouseAction::Wheel { up } => 64 + u32::from(!up),
        };
        if event.alt {
            code += 8;
        }
        if event.control {
            code += 16;
        }
        let (x, y) = (event.column + 1, event.row + 1);
        match self.encoding {
            MouseEncoding::Sgr => {
                let last = if matches!(event.action, MouseAction::Release(_)) {
                    'm'
                } else {
                    'M'
                };
                Some(format!("\x1b[<{code};{x};{y}{last}").into_bytes())
            }
            MouseEncoding::X10 => {
                let byte = |value: usize| u8::try_from(32 + value).ok();
                let (x, y) = (byte(x)?, byte(y)?);
                Some(vec![0x1b, b'[', b'M', 32 + code as u8, x, y])
            }
            MouseEncoding::Utf8 => {
                let mut out = b"\x1b[M".to_vec();
                for value in [code as usize, x, y] {
                    // Two bytes of UTF-8 go up to U+07FF.
                    let c = char::from_u32(u32::try_from(32 + value).ok()?)
                        .filter(|c| (*c as u32) < 0x800)?;
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
                Some(out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(action: MouseAction, column: usize, row: usize) -> MouseEvent {
        MouseEvent {
            action,
            column,
            row,
            alt: false,
            control: false,
        }
    }

    fn reporting(mode: MouseMode, encoding: MouseEncoding) -> MouseReporting {
        MouseReporting { mode, encoding }
    }

    const LEFT: MouseButton = MouseButton::Left;

    #[test]
    fn sgr_reports_say_the_button_and_its_release() {
        let sgr = reporting(MouseMode::Click, MouseEncoding::Sgr);
        let report = |action, column, row| {
            sgr.report(at(action, column, row))
                .map(|bytes| String::from_utf8(bytes).unwrap())
        };
        assert_eq!(
            report(MouseAction::Press(LEFT), 4, 2).unwrap(),
            "\x1b[<0;5;3M"
        );
        assert_eq!(
            report(MouseAction::Release(LEFT), 4, 2).unwrap(),
            "\x1b[<0;5;3m"
        );
        assert_eq!(
            report(MouseAction::Press(MouseButton::Right), 0, 0).unwrap(),
            "\x1b[<2;1;1M"
        );
        assert_eq!(
            report(MouseAction::Release(MouseButton::Middle), 0, 0).unwrap(),
            "\x1b[<1;1;1m"
        );
        assert_eq!(
            report(MouseAction::Wheel { up: true }, 9, 9).unwrap(),
            "\x1b[<64;10;10M"
        );
        assert_eq!(
            report(MouseAction::Wheel { up: false }, 9, 9).unwrap(),
            "\x1b[<65;10;10M"
        );
        // No limit on the coordinates.
        assert_eq!(
            report(MouseAction::Press(LEFT), 999, 300).unwrap(),
            "\x1b[<0;1000;301M"
        );
        // Click mode reports no moves.
        assert_eq!(report(MouseAction::Move(Some(LEFT)), 1, 1), None);
        assert_eq!(report(MouseAction::Move(None), 1, 1), None);
    }

    #[test]
    fn x10_reports_are_three_bytes_past_32() {
        let x10 = reporting(MouseMode::Click, MouseEncoding::X10);
        let report = |action, column, row| x10.report(at(action, column, row));
        assert_eq!(
            report(MouseAction::Press(LEFT), 4, 2).unwrap(),
            b"\x1b[M\x20\x25\x23"
        );
        // A release says which button only as "a button": 3.
        assert_eq!(
            report(MouseAction::Release(MouseButton::Right), 4, 2).unwrap(),
            b"\x1b[M\x23\x25\x23"
        );
        assert_eq!(
            report(MouseAction::Wheel { up: true }, 0, 0).unwrap(),
            b"\x1b[M\x60\x21\x21"
        );
        assert_eq!(
            report(MouseAction::Wheel { up: false }, 0, 0).unwrap(),
            b"\x1b[M\x61\x21\x21"
        );
        // 223 is the last column the form can say.
        assert_eq!(report(MouseAction::Press(LEFT), 222, 0).unwrap()[4], 255);
        assert_eq!(report(MouseAction::Press(LEFT), 223, 0), None);
        assert_eq!(report(MouseAction::Press(LEFT), 0, 223), None);
    }

    #[test]
    fn utf8_reports_write_large_coordinates_as_characters() {
        let utf8 = reporting(MouseMode::Click, MouseEncoding::Utf8);
        let report = |column| utf8.report(at(MouseAction::Press(LEFT), column, 0));
        assert_eq!(report(4).unwrap(), b"\x1b[M\x20\x25\x21");
        // Column 300 is 32 + 300 = U+014C, two bytes.
        assert_eq!(report(299).unwrap(), "\x1b[M\u{20}\u{14c}\u{21}".as_bytes());
        assert_eq!(report(2015), None, "past what two bytes say");
    }

    #[test]
    fn modes_choose_which_moves_are_reported() {
        let mv = |button| at(MouseAction::Move(button), 3, 1);
        for encoding in [MouseEncoding::X10, MouseEncoding::Sgr] {
            let click = reporting(MouseMode::Click, encoding);
            let drag = reporting(MouseMode::Drag, encoding);
            let motion = reporting(MouseMode::Motion, encoding);
            assert_eq!(click.report(mv(Some(LEFT))), None);
            assert!(drag.report(mv(Some(LEFT))).is_some());
            assert_eq!(drag.report(mv(None)), None);
            assert!(motion.report(mv(Some(LEFT))).is_some());
            assert!(motion.report(mv(None)).is_some());
        }
        let sgr = reporting(MouseMode::Motion, MouseEncoding::Sgr);
        let text = |event| String::from_utf8(sgr.report(event).unwrap()).unwrap();
        assert_eq!(text(mv(Some(LEFT))), "\x1b[<32;4;2M");
        assert_eq!(text(mv(Some(MouseButton::Right))), "\x1b[<34;4;2M");
        assert_eq!(text(mv(None)), "\x1b[<35;4;2M");
        let x10 = reporting(MouseMode::Drag, MouseEncoding::X10);
        assert_eq!(x10.report(mv(Some(LEFT))).unwrap(), b"\x1b[M\x40\x24\x22");
    }

    #[test]
    fn alt_and_control_are_reported() {
        let sgr = reporting(MouseMode::Click, MouseEncoding::Sgr);
        let event = MouseEvent {
            alt: true,
            control: true,
            ..at(MouseAction::Press(LEFT), 0, 0)
        };
        assert_eq!(sgr.report(event).unwrap(), b"\x1b[<24;1;1M");
        let event = MouseEvent {
            control: true,
            ..at(MouseAction::Wheel { up: true }, 0, 0)
        };
        assert_eq!(sgr.report(event).unwrap(), b"\x1b[<80;1;1M");
    }
}
