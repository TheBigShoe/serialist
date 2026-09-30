//! A U-Boot style boot menu: a full-screen device for the VT screen.
//!
//! Like U-Boot's `bootmenu`, it hides the cursor, clears the screen once, and from then
//! on redraws in place with cursor addressing: cursor home, then every line rewritten at
//! its row with an erase to the end of the line. Nothing it draws ever scrolls. A monitor
//! view shows the escape sequences' leftovers as a smear; a VT screen shows a menu.
//!
//! ```text
//! row 1   *** Serialist Boot Menu ***
//! row 3        Boot from eMMC                  <- the selected item, in reverse video
//! row 4        Boot from network (TFTP)
//! row 5        U-Boot console
//! row 7     Press UP/DOWN to move, ENTER to select
//! row 8     Redraw 12
//! ```
//!
//! The up and down arrows move the highlight (stopping at the ends), in either cursor
//! key form (`ESC [ A` or `ESC O A`), split across chunks or not. Enter (CR or LF)
//! selects the highlighted item, which the status row then names. Every key that
//! changes something redraws at once, and a timer redraws every
//! [`redraw interval`](MenuDevice::with_redraw_interval) regardless, with the redraw
//! count on the status row, the way a bootloader refreshes its countdown.

use std::time::{Duration, Instant};

use crate::{DeviceOutput, SimDevice};

/// Where an escape sequence from the host has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keys {
    Idle,
    /// After ESC.
    Escape,
    /// After `ESC [` or `ESC O`: parameters until a final byte.
    Sequence,
}

/// The simulated boot menu. See the module docs.
#[derive(Debug)]
pub struct MenuDevice {
    selected: usize,
    chosen: Option<usize>,
    redraws: u64,
    interval: Option<Duration>,
    next: Option<Instant>,
    keys: Keys,
}

impl MenuDevice {
    /// The menu's items, top to bottom.
    pub const ITEMS: [&'static str; 3] = [
        "Boot from eMMC",
        "Boot from network (TFTP)",
        "U-Boot console",
    ];
    pub const TITLE: &'static str = "*** Serialist Boot Menu ***";
    pub const HELP: &'static str = "Press UP/DOWN to move, ENTER to select";
    /// Screen row (1-based) of the title.
    pub const TITLE_ROW: usize = 1;
    /// Screen row (1-based) of the first item; the others follow.
    pub const FIRST_ITEM_ROW: usize = 3;
    pub const HELP_ROW: usize = 7;
    /// Screen row (1-based) of the redraw count and the selection.
    pub const STATUS_ROW: usize = 8;
    /// Columns of indent before an item.
    pub const ITEM_INDENT: usize = 5;
    pub const DEFAULT_REDRAW_INTERVAL: Duration = Duration::from_millis(500);

    pub fn new() -> Self {
        Self {
            selected: 0,
            chosen: None,
            redraws: 0,
            interval: Some(Self::DEFAULT_REDRAW_INTERVAL),
            next: None,
            keys: Keys::Idle,
        }
    }

    /// How often the timer redraws; `None` redraws only on connect and on keys.
    pub fn with_redraw_interval(mut self, interval: Option<Duration>) -> Self {
        self.interval = interval.filter(|d| !d.is_zero());
        self
    }

    /// The highlighted item's index in [`ITEMS`](Self::ITEMS).
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The item Enter last selected, if any.
    pub fn chosen(&self) -> Option<usize> {
        self.chosen
    }

    /// Frames drawn so far.
    pub fn redraws(&self) -> u64 {
        self.redraws
    }

    /// The status row's text (without its indent) after `redraws` frames.
    pub fn status(redraws: u64, chosen: Option<usize>) -> String {
        match chosen {
            Some(item) => format!("Redraw {redraws}, selected: {}", Self::ITEMS[item]),
            None => format!("Redraw {redraws}"),
        }
    }

    /// The bytes of the next frame: the whole menu, redrawn in place.
    pub fn frame(&self) -> Vec<u8> {
        let mut out = String::from("\x1b[H");
        let line = |out: &mut String, row: usize, text: &str| {
            out.push_str(&format!("\x1b[{row};1H{text}\x1b[K"));
        };
        line(&mut out, Self::TITLE_ROW, &format!("  {}", Self::TITLE));
        for (i, item) in Self::ITEMS.iter().enumerate() {
            let indent = " ".repeat(Self::ITEM_INDENT);
            let text = if i == self.selected {
                format!("{indent}\x1b[7m{item}\x1b[0m")
            } else {
                format!("{indent}{item}")
            };
            line(&mut out, Self::FIRST_ITEM_ROW + i, &text);
        }
        line(&mut out, Self::HELP_ROW, &format!("  {}", Self::HELP));
        let status = Self::status(self.redraws + 1, self.chosen);
        line(&mut out, Self::STATUS_ROW, &format!("  {status}"));
        out.into_bytes()
    }

    fn redraw(&mut self, out: &mut dyn DeviceOutput) {
        out.send(&self.frame());
        self.redraws += 1;
    }

    /// Apply one key; whether the menu changed.
    fn key(&mut self, byte: u8) -> bool {
        let (keys, changed) = match (self.keys, byte) {
            (_, 0x1b) => (Keys::Escape, false),
            (Keys::Idle, b'\r' | b'\n') => {
                let changed = self.chosen != Some(self.selected);
                self.chosen = Some(self.selected);
                (Keys::Idle, changed)
            }
            (Keys::Idle, _) => (Keys::Idle, false),
            (Keys::Escape, b'[' | b'O') => (Keys::Sequence, false),
            (Keys::Escape, _) => (Keys::Idle, false),
            // Parameters and intermediates (`ESC [ 1 ; 5 A` is ctrl-Up: still up).
            (Keys::Sequence, 0x20..=0x3f) => (Keys::Sequence, false),
            (Keys::Sequence, b'A') => (Keys::Idle, self.move_by(-1)),
            (Keys::Sequence, b'B') => (Keys::Idle, self.move_by(1)),
            (Keys::Sequence, _) => (Keys::Idle, false),
        };
        self.keys = keys;
        changed
    }

    fn move_by(&mut self, delta: isize) -> bool {
        let next = self
            .selected
            .saturating_add_signed(delta)
            .min(Self::ITEMS.len() - 1);
        let changed = next != self.selected;
        self.selected = next;
        changed
    }
}

impl Default for MenuDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice for MenuDevice {
    fn name(&self) -> &str {
        "menu"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        // Hide the cursor and clear the screen once, as U-Boot's bootmenu does.
        out.send(b"\x1b[?25l\x1b[2J");
        self.redraw(out);
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        let mut changed = false;
        for &byte in bytes {
            changed |= self.key(byte);
        }
        if changed {
            self.redraw(out);
        }
    }

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let interval = self.interval?;
        let due = *self.next.get_or_insert(now + interval);
        if now >= due {
            self.redraw(out);
            // Keep the rate, but do not burst to catch up after a stall.
            let next = due + interval;
            self.next = Some(if next <= now { now + interval } else { next });
        }
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureOutput;

    fn quiet() -> MenuDevice {
        MenuDevice::new().with_redraw_interval(None)
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn connect_clears_once_and_draws_the_menu_in_place() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        dev.on_connect(&mut out);
        let sent = text(&out.take());
        assert!(
            sent.starts_with("\x1b[?25l\x1b[2J\x1b[H\x1b[1;1H  *** Serialist Boot Menu ***\x1b[K")
        );
        assert!(sent.contains("\x1b[3;1H     \x1b[7mBoot from eMMC\x1b[0m\x1b[K"));
        assert!(sent.contains("\x1b[4;1H     Boot from network (TFTP)\x1b[K"));
        assert!(sent.contains("\x1b[5;1H     U-Boot console\x1b[K"));
        assert!(sent.ends_with("\x1b[8;1H  Redraw 1\x1b[K"));
        assert!(!sent.contains('\n'), "nothing scrolls: no line feeds");
        assert_eq!(dev.redraws(), 1);
    }

    #[test]
    fn arrows_move_the_highlight_in_either_form_and_split() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        dev.on_receive(b"\x1b[B", &mut out);
        assert_eq!(dev.selected(), 1);
        assert!(text(&out.take()).contains("\x1b[4;1H     \x1b[7mBoot from network (TFTP)"));
        // Application cursor keys, one byte per chunk.
        for byte in b"\x1bOB" {
            dev.on_receive(std::slice::from_ref(byte), &mut out);
        }
        assert_eq!(dev.selected(), 2);
        // At the bottom: no change, no redraw.
        dev.on_receive(b"\x1b[B", &mut out);
        let _ = out.take();
        dev.on_receive(b"\x1b[B", &mut out);
        assert!(out.take().is_empty());
        // Up with a modifier parameter is still up.
        dev.on_receive(b"\x1b[1;5A\x1b[A\x1b[A\x1b[A", &mut out);
        assert_eq!(dev.selected(), 0);
        // Other keys and sequences are ignored.
        dev.on_receive(b"x\x1b[C\x1b[2~", &mut out);
        assert_eq!(dev.selected(), 0);
    }

    #[test]
    fn enter_selects_the_highlighted_item() {
        let mut dev = quiet();
        let mut out = CaptureOutput::new();
        dev.on_receive(b"\x1b[B\r", &mut out);
        assert_eq!(dev.chosen(), Some(1));
        assert!(
            text(&out.take()).ends_with("  Redraw 1, selected: Boot from network (TFTP)\x1b[K")
        );
    }

    #[test]
    fn the_timer_redraws_at_its_interval() {
        let interval = Duration::from_millis(100);
        let mut dev = MenuDevice::new().with_redraw_interval(Some(interval));
        let mut out = CaptureOutput::new();
        dev.on_connect(&mut out);
        let _ = out.take();
        let t0 = Instant::now();
        assert_eq!(dev.on_tick(t0, &mut out), Some(t0 + interval));
        assert!(out.take().is_empty());
        assert_eq!(
            dev.on_tick(t0 + interval, &mut out),
            Some(t0 + 2 * interval)
        );
        assert!(text(&out.take()).ends_with("  Redraw 2\x1b[K"));
        let late = t0 + 40 * interval;
        assert_eq!(dev.on_tick(late, &mut out), Some(late + interval));
        assert_eq!(dev.redraws(), 3, "a stall gives one redraw, not a burst");
        assert_eq!(quiet().on_tick(t0, &mut out), None);
    }
}
