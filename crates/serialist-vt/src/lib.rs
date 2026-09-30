//! Full terminal emulation for Serialist's VT mode: a screen that U-Boot menus, Linux
//! consoles and router CLIs can draw on with cursor addressing, shown through the same
//! [`LineSource`] contract as the scrollback store, so the existing terminal element
//! renders it.
//!
//! The emulator is [`alacritty_terminal`] 0.26 with no PTY: received bytes go through its
//! `vte::ansi::Processor` into its `Term` on the ingest thread. No GPUI here.
//!
//! - [`VtScreen`] owns the terminal: [`feed`](VtScreen::feed),
//!   [`resize`](VtScreen::resize), [`reset`](VtScreen::reset),
//!   [`snapshot`](VtScreen::snapshot), [`take_events`](VtScreen::take_events).
//! - [`VtSnapshot`] is an immutable view of the screen plus its scrollback. It
//!   implements [`LineSource`] (one [`StyledLine`] per grid row) and adds the
//!   cursor, the modes, the title and damage ([`VtSnapshot::changed_since`],
//!   [`VtSnapshot::generation`]).
//! - [`VtSink`] is the [`ChunkSink`](serialist_core::ChunkSink) that feeds a screen
//!   from a session, added with [`Ingest::spawn_with`](serialist_core::Ingest::spawn_with)
//!   next to the store; [`VtHandle`] is the UI's side of it.
//! - [`vt_key_bytes`] gives the cursor-key bytes that depend on the terminal's mode.
//!
//! # Line ids
//!
//! Every row a snapshot shows has a [`LineId`], so scrolling, search results and the
//! shaped-line cache work as they do on the store. The policy:
//!
//! - **Scrollback rows** get ids in the order they scroll off the top of the primary
//!   screen: the first row ever to scroll off is 0, the next 1, and so on. An id never
//!   changes and is never reused. The newest `scrollback_lines` rows are kept; older ones
//!   are evicted and [`LineSource::first_line`] moves past them, as on the store.
//! - **Screen rows follow**: row `r` of the screen is [`VtSnapshot::first_visible`] + `r`,
//!   where `first_visible` counts the rows scrolled off so far. When the screen scrolls,
//!   a row moves up one position and `first_visible` goes up one, so a row keeps its id
//!   while it scrolls up the screen and when it scrolls into the scrollback.
//! - But the id of a screen row belongs to the screen position, not to the text on it.
//!   Cursor addressing, erases, scroll regions and the alternate screen rewrite rows in
//!   place, and the same id then shows different text. Screen rows are therefore
//!   re-assigned in every snapshot and marked not `complete`; scrollback rows are
//!   `complete` and final.
//! - Clearing the scrollback (`CSI 3 J`, RIS, [`VtScreen::reset`]) retires its ids:
//!   `first_line` jumps to `first_visible`. A resize never renumbers the scrollback
//!   (it is not reflowed); rows a resize pushes off the top become scrollback with the
//!   next ids.
//!
//! **Selection across snapshots is best effort in VT mode.** A selection is a pair of
//! (line id, column) points, and on scrollback rows it stays exact. On screen rows it
//! keeps pointing at the same screen positions, so after the device redraws them (a menu
//! refresh, an editor repaint) it covers whatever is there now. A selection made while
//! the screen scrolls stays on its text, because the ids scroll with it.
//!
//! # Events
//!
//! [`VtEvent`] is what the terminal needs the app to do, drained with
//! [`VtScreen::take_events`] or [`VtHandle::take_events`]:
//!
//! | Event | Cause | The app |
//! | --- | --- | --- |
//! | [`VtEvent::Title`] | OSC 0 or 2 set a new title | shows it on the tab |
//! | [`VtEvent::ResetTitle`] | the title was cleared, or a full reset | shows its default |
//! | [`VtEvent::Bell`] | BEL | rings or flashes |
//! | [`VtEvent::Respond`] | a query: DA (`CSI c`, `CSI > c`), DSR (`CSI 5 n`, `CSI 6 n`), `CSI 18 t`, DECRQM | writes the bytes to the device |
//! | [`VtEvent::ColorRequest`] | OSC 4, 10, 11 or 12 with `?` | answers with its theme color |
//!
//! OSC 52 clipboard access is disabled: a device on a serial line does not get to read
//! or write the clipboard. Mouse reporting is tracked in [`VtModes`] but Serialist sends
//! no mouse reports yet.
//!
//! # Known limitations
//!
//! - **Wide characters.** Alacritty measures character widths, and a wide character
//!   takes two cells. The line text has it once (the spacer cell is skipped), but the
//!   terminal element draws one character per column, so the rest of that row is drawn
//!   one column left per wide character before it. Combining marks stay with their base
//!   character and take no column, which the element gets right.
//! - **Palette changes** (OSC 4, 10, 11) are accepted but not shown: colors reach the
//!   element as theme slots ([`Color::Ansi`](serialist_core::Color::Ansi) and friends).
//! - **Clearing the screen keeps it**: `CSI 2 J` on the primary screen scrolls what was
//!   on it into the scrollback before blanking (Alacritty's behavior, like VTE's), so
//!   the rows stay readable above. `clear` on Linux also sends `CSI 3 J`, which then
//!   drops them.
//! - **Growing the screen** adds blank rows at the bottom; rows are not pulled back out
//!   of the scrollback.
//! - **Synchronized updates** (`CSI ? 2026 h`) are honored: bytes are held until the
//!   device ends the update or 150 ms pass. Parsing starts the clock but nothing ticks
//!   it, so the screen checks on every chunk and [`VtSink`] on every idle tick.

mod convert;
mod events;
mod keys;
mod screen;
mod scrollback;
mod sink;
mod snapshot;

pub use events::{ColorRequest, MAX_EVENTS, VtEvent};
pub use keys::vt_key_bytes;
pub use screen::{
    DEFAULT_SCROLLBACK, MAX_COLUMNS, MAX_ROWS, MIN_COLUMNS, MIN_ROWS, STAGING_ROWS, VtScreen,
};
pub use sink::{VtHandle, VtSink};
pub use snapshot::{CursorShape, CursorState, VtModes, VtSnapshot};

// For the docs above.
#[allow(unused_imports)]
use serialist_core::{LineId, LineSource, StyledLine};
