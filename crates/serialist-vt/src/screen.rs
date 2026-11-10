//! [`VtScreen`]: Alacritty's `Term` fed from a byte stream, with the scrollback kept
//! outside it.
//!
//! # How rows reach the scrollback
//!
//! Alacritty moves a row into its history whenever the screen scrolls it off the top
//! (a line feed on the bottom row, `CSI S`, a delete-lines at the top, clearing the
//! screen with `CSI 2 J`, a resize that shrinks the screen). It does not say so, and once
//! its history is full a new row silently replaces the oldest, so its history size alone
//! cannot count rows. So the bytes are not handed to `Term` directly: the parser drives a
//! [`Tracker`], which forwards every `vte::ansi::Handler` call to the `Term` and then
//! looks at the history. Anything there scrolled off during that call: it is converted
//! to lines, appended to the [`Scrollback`] with the next ids, and the history is emptied.
//! One call scrolls at most one screen, and the history's capacity
//! ([`STAGING_ROWS`]) is far above that, so nothing is ever dropped unseen.
//!
//! Two calls clear the history instead of filling it: `CSI 3 J` (erase saved lines) and
//! RIS (`ESC c`). Alacritty would find its history already empty, so the tracker clears
//! the scrollback itself on those.
//!
//! Keeping Alacritty's history empty has one side effect the tracker undoes: clearing
//! an entirely blank screen (`CSI 2 J`) would scroll one blank row off it, because the
//! scan for the last non-empty cell stops at the top row instead of in the history.
//!
//! The alternate screen has no history (Alacritty gives it none), so nothing scrolls off
//! it; the scrollback shown above it is the primary screen's.
//!
//! # Characters cut by a chunk boundary
//!
//! vte 0.15.0 buffers a UTF-8 character cut off at the end of a chunk and finishes it
//! with the next, but when that next chunk holds more characters and then an invalid or
//! another cut-off one, it skips the bytes between (`advance_partial_utf8` returns the
//! length of everything valid in its four-byte buffer, not of the first character). So
//! [`VtScreen::feed_at`] holds a cut-off character back and parses it with the next chunk,
//! the way `serialist_core::AnsiParser` does: vte then never sees a character split across
//! two calls, and the screen does not depend on where the chunks end.
//!
//! # Zero-width characters
//!
//! A combining mark joins the cell before the cursor and takes no cell of its own, so a
//! row never scrolls off under a stream of them, and Alacritty keeps as many as arrive:
//! a cell grows for as long as the device sends, and `CSI n b` (repeat the last
//! character `n` times) makes eleven bytes add 65535. Every snapshot of the row copies
//! them too. So the tracker drops a zero-width character once its cell holds
//! [`MAX_ZERO_WIDTH`] of them.
//!
//! # Synchronized updates
//!
//! A device brackets a redraw with `CSI ? 2026 h` and `CSI ? 2026 l` so that it shows in
//! one piece. vte would do this by holding the bytes back, up to 2 MiB of them, and
//! parsing them all in one call when the update ends. The screen does it instead, and
//! holds back the view: vte is given a timeout that never holds anything
//! ([`NoHold`]), bytes are parsed as they arrive, and while an update is open
//! [`VtScreen::snapshot`] returns the snapshot before it. That snapshot is taken when the
//! update begins, so what arrived ahead of the begin in the same chunk shows at once, and
//! a device sending one update after another shows each when the next begins. For that
//! the parsing stops after an `h` that may end the begin ([`Stops`]), and if an
//! update did begin the snapshot is taken there, at most once per chunk: a device does
//! not get a snapshot for every eight bytes.
//!
//! An update ends at its `CSI ? 2026 l`, on a full reset, on a resize, or
//! [`SYNC_TIMEOUT`] after it began; a second begin inside it does not extend it. Answers
//! to the device's queries and its other events are not held: a device that asks inside
//! an update gets its answer when it asks.
//!
//! # Repeats
//!
//! `CSI n b` repeats the last character `n` times, up to 65535: eight bytes for about a
//! millisecond of parsing, or ten when each character scrolls a row off a narrow screen,
//! which is more than a link delivers them in. vte does the repeating, as that many
//! calls for one character, so the tracker cannot tell a repeat from text by what it is
//! asked to do. It can by when: a repeat runs while the `b` that ends its sequence is
//! parsed. So the screen looks for each `b` that may end one ([`Stops`]) and parses
//! it in a call of its own, in which the tracker takes one screenful of characters, rows
//! times columns, and drops the rest. Past a screenful a repeat only scrolls rows of the same
//! character by. (This is why the screen and not vte holds a synchronized update: vte
//! would parse a held repeat in one call with everything around it.) A screen of 65535
//! cells or more caps nothing.
//!
//! # Insert mode
//!
//! In insert mode (`CSI 4 h`) Alacritty makes room for every character by moving the cells
//! from the cursor to the end of the row one to the right, a cell at a time, so a
//! character costs as much as the row is wide. `CSI 65535 b` then makes eight bytes cost
//! 65535 times the row: 57 ms on a 1024-column screen, where a device at 115200 baud can
//! send over a thousand of them a second. When those cells are all equal, as on a blank
//! row or one the repeat has already filled, moving them changes nothing, so the tracker
//! has the character written without the move. It looks along the row once and then
//! remembers where it is, for as long as only characters arrive. What is left costs what
//! typing into a row of text in insert mode costs: one move of the row for each character
//! until the text has left it, which a repeat can ask for once a row.

use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions, Grid, GridCell, Row};
use alacritty_terminal::index::{Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape as VtCursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedMode,
    NamedPrivateMode, PrivateMode, Processor, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset,
    TabulationClearMode, Timeout, cursor_icon::CursorIcon,
};
use serialist_core::ansi::incomplete_utf8_suffix;
use serialist_core::{Epoch, LineId, StyledLine};
use unicode_width::UnicodeWidthChar;

use crate::convert::{Stamp, build_line, same_content};
use crate::events::{Listener, VtEvent};
use crate::scrollback::Scrollback;
use crate::snapshot::{CursorShape, CursorState, VtModes, VtSnapshot};

/// Rows Alacritty's own history may hold between two looks. The tracker empties it after
/// every handler call, so this only has to exceed what one call (or one resize) can
/// scroll off; rows are allocated as they arrive, not up front.
pub const STAGING_ROWS: usize = 1 << 16;

/// Smallest screen: Alacritty needs two columns and one line.
pub const MIN_COLUMNS: usize = 2;
pub const MIN_ROWS: usize = 1;
/// Largest screen accepted; larger requests are clamped.
pub const MAX_COLUMNS: usize = 1024;
pub const MAX_ROWS: usize = 1024;

/// Scrollback rows kept by [`VtScreen::default`].
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Zero-width characters (combining marks, variation selectors, joiners) a cell keeps
/// after its character; further ones are dropped. Thirty is the limit of the Unicode
/// Stream-Safe Text Format (UAX #15), which no conforming text exceeds, so nothing of
/// such text is lost. See the module docs for why there is a limit at all.
pub const MAX_ZERO_WIDTH: usize = 30;

/// The largest buffer kept for joining a held-back character to the next chunk.
const KEEP_JOINED: usize = 64 * 1024;

/// How long a synchronized update (`CSI ? 2026 h`) holds the view back when the device
/// does not end it: what vte and Alacritty allow.
pub const SYNC_TIMEOUT: Duration = Duration::from_millis(150);

const SYNC_UPDATE: PrivateMode = PrivateMode::Named(NamedPrivateMode::SyncUpdate);

/// The timeout vte keeps for a synchronized update, as one that is never pending: vte
/// then holds no bytes back and parses every chunk as it comes. It still tells the
/// handler when an update begins and ends, which is all the screen needs (see the module
/// docs).
#[derive(Default)]
struct NoHold;

impl Timeout for NoHold {
    fn set_timeout(&mut self, _duration: Duration) {}

    fn clear_timeout(&mut self) {}

    fn pending_timeout(&self) -> bool {
        false
    }
}

struct Size {
    columns: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

fn clamp_size(columns: usize, rows: usize) -> Size {
    Size {
        columns: columns.clamp(MIN_COLUMNS, MAX_COLUMNS),
        rows: rows.clamp(MIN_ROWS, MAX_ROWS),
    }
}

/// A terminal screen fed from a serial stream: Alacritty's emulator with a scrollback of
/// stable line ids. Owned by one thread at a time (the ingest thread, through a
/// [`VtSink`](crate::VtSink)); readers take [`VtSnapshot`]s.
pub struct VtScreen {
    term: Term<Listener>,
    listener: Listener,
    processor: Processor<NoHold>,
    scrollback: Scrollback,
    /// The snapshot handed out last: rows are reused from it when unchanged.
    last: Option<VtSnapshot>,
    /// Something may have changed since `last`.
    dirty: bool,
    /// When the open synchronized update times out; `None` when none is open.
    sync: Option<Instant>,
    generation: u64,
    epoch: Epoch,
    /// Bytes fed so far: the stream offset of the next byte.
    offset: u64,
    /// When the last chunk arrived; the time given to rows it changed.
    last_at: Instant,
    /// A UTF-8 character cut off at the end of the last chunk, held back until the next
    /// one (see the module docs). Counted in `offset`, not yet parsed.
    held: [u8; 3],
    held_len: usize,
    /// Reused to join held bytes to the next chunk.
    joined: Vec<u8>,
}

impl Default for VtScreen {
    /// 80 by 24 with [`DEFAULT_SCROLLBACK`] rows of scrollback.
    fn default() -> Self {
        Self::new(80, 24, DEFAULT_SCROLLBACK)
    }
}

impl std::fmt::Debug for VtScreen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtScreen")
            .field("columns", &self.columns())
            .field("rows", &self.rows())
            .field("scrollback_capacity", &self.scrollback.capacity())
            .field("offset", &self.offset)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl VtScreen {
    /// A blank `columns` by `rows` screen keeping the newest `scrollback_lines` rows that
    /// scroll off it. Sizes are clamped to [`MIN_COLUMNS`]..=[`MAX_COLUMNS`] and
    /// [`MIN_ROWS`]..=[`MAX_ROWS`].
    pub fn new(columns: usize, rows: usize, scrollback_lines: usize) -> Self {
        let size = clamp_size(columns, rows);
        let config = Config {
            scrolling_history: STAGING_ROWS,
            // A device on a serial line does not get to write the clipboard.
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let listener = Listener::default();
        let term = Term::new(config, &size, listener.clone());
        let epoch = Epoch::now();
        Self {
            term,
            listener,
            processor: Processor::new(),
            scrollback: Scrollback::new(scrollback_lines),
            last: None,
            dirty: true,
            sync: None,
            generation: 0,
            epoch,
            offset: 0,
            last_at: epoch.instant,
            held: [0; 3],
            held_len: 0,
            joined: Vec::new(),
        }
    }

    /// Use `epoch` for mapping line times to wall-clock time, such as the session
    /// store's, so timestamps agree between the two views.
    pub fn with_epoch(mut self, epoch: Epoch) -> Self {
        self.epoch = epoch;
        self.last_at = epoch.instant;
        self.dirty = true;
        self
    }

    pub fn columns(&self) -> usize {
        self.term.columns()
    }

    pub fn rows(&self) -> usize {
        self.term.screen_lines()
    }

    /// Bytes fed since the screen was made.
    pub fn bytes_fed(&self) -> u64 {
        self.offset
    }

    /// Feed received bytes, arriving now. Chunk boundaries do not matter: any split of
    /// a stream gives the same screen.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.feed_at(bytes, Instant::now());
    }

    /// Feed received bytes that arrived at `at` (the session's chunk time). Rows the
    /// chunk changes get `at` as their `received_at`, and a synchronized update it begins
    /// times out [`SYNC_TIMEOUT`] after `at`.
    pub fn feed_at(&mut self, bytes: &[u8], at: Instant) {
        if bytes.is_empty() {
            return;
        }
        self.last_at = at;
        self.flush_sync(at);
        if self.held_len == 0 {
            self.advance_whole_characters(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.joined);
            joined.clear();
            joined.extend_from_slice(&self.held[..self.held_len]);
            joined.extend_from_slice(bytes);
            self.held_len = 0;
            self.advance_whole_characters(&joined);
            if joined.capacity() <= KEEP_JOINED {
                self.joined = joined;
            }
        }
        self.offset += bytes.len() as u64;
        self.dirty = true;
    }

    /// Parse `bytes` but for a UTF-8 character cut off at their end, which waits in
    /// `held` for the next chunk. Nothing is held when this is called.
    fn advance_whole_characters(&mut self, bytes: &[u8]) {
        let keep = incomplete_utf8_suffix(bytes);
        let (whole, tail) = bytes.split_at(bytes.len() - keep);
        if !whole.is_empty() {
            self.parse(whole);
        }
        self.held[..keep].copy_from_slice(tail);
        self.held_len = keep;
    }

    /// Parse `bytes`, which end on a character boundary, in pieces (see the module docs
    /// and [`Stops`]):
    ///
    /// - A `b` that may end a repeat is parsed alone, with a screenful of characters
    ///   allowed.
    /// - After an `h` that may begin a synchronized update the parsing stops; if one did
    ///   begin, for a snapshot of the screen as it is, the one shown while the update is
    ///   open. The first time in these bytes only.
    fn parse(&mut self, bytes: &[u8]) {
        let mut start = 0;
        let mut captured = false;
        for stop in Stops::new(bytes) {
            if bytes[stop] == b'b' {
                self.advance(&bytes[start..stop], None);
                let screenful = self.rows() * self.columns();
                self.advance(&bytes[stop..=stop], Some(screenful));
                start = stop + 1;
            } else if !captured {
                if self.advance(&bytes[start..=stop], None) {
                    self.refresh();
                    captured = true;
                }
                start = stop + 1;
            }
        }
        self.advance(&bytes[start..], None);
    }

    /// Hand `bytes` to the parser, which may write `characters` characters at most, if
    /// that is given. Returns whether the last thing they did was begin a synchronized
    /// update.
    fn advance(&mut self, bytes: &[u8], characters: Option<usize>) -> bool {
        if bytes.is_empty() {
            return false;
        }
        self.with_tracker(characters, |processor, tracker| {
            processor.advance(tracker, bytes)
        })
    }

    /// End a synchronized update (`CSI ? 2026 h`) that the device began [`SYNC_TIMEOUT`]
    /// or more before `now` and never ended, so the screen shows what has arrived since.
    /// Feeding does this on its own; call it when no bytes arrive (the sink does, on
    /// idle) so a device that stops mid-update does not freeze the screen. Returns
    /// whether an update was ended.
    pub fn flush_sync(&mut self, now: Instant) -> bool {
        let expired = self.sync.is_some_and(|deadline| now >= deadline);
        if expired {
            self.end_sync();
        }
        expired
    }

    /// Run `f` with the parser and a tracker over this screen, stamping rows with the
    /// time of the last chunk; the tracker takes `characters` characters, if that is
    /// given, and drops any more. Returns whether the last mode `f` had the tracker set
    /// began a synchronized update.
    fn with_tracker(
        &mut self,
        characters: Option<usize>,
        f: impl FnOnce(&mut Processor<NoHold>, &mut Tracker<'_>),
    ) -> bool {
        let mut tracker = Tracker {
            term: &mut self.term,
            listener: &self.listener,
            rows: Absorb {
                scrollback: &mut self.scrollback,
                prev: self.last.as_ref(),
                at: self.last_at,
                offset: self.offset,
            },
            equal_from: None,
            sync: &mut self.sync,
            begun: false,
            characters,
        };
        f(&mut self.processor, &mut tracker);
        tracker.begun
    }

    /// Whether a synchronized update is open, holding the view back.
    pub fn sync_pending(&self) -> bool {
        self.sync.is_some()
    }

    /// End a synchronized update now, timed out or not, so the screen shows what has
    /// arrived (the link is gone, say). Returns whether one was open.
    pub fn end_sync(&mut self) -> bool {
        let open = self.sync.take().is_some();
        if open {
            self.dirty = true;
        }
        open
    }

    /// Resize the screen to `columns` by `rows` (clamped as in [`new`](Self::new)).
    ///
    /// Alacritty keeps the cursor row on screen: shrinking pushes rows off the top into
    /// the scrollback, and a narrower primary screen rewraps its rows. Growing adds blank
    /// rows at the bottom; rows are not pulled back out of the scrollback, whose lines
    /// and ids never change. A synchronized update left open ends: the snapshot it
    /// holds is of the old size.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        let size = clamp_size(columns, rows);
        if size.columns == self.columns() && size.rows == self.rows() {
            return;
        }
        self.sync = None;
        self.term.resize(size);
        // Rows the resize pushed off the primary screen. On the alternate screen they
        // wait in the primary's history until it is back.
        let mut rows = Absorb {
            scrollback: &mut self.scrollback,
            prev: self.last.as_ref(),
            at: self.last_at,
            offset: self.offset,
        };
        rows.absorb(&mut self.term);
        self.dirty = true;
    }

    /// Reset everything as RIS (`ESC c`) does, and forget the scrollback and any
    /// half-received escape sequence or character. Line ids keep counting up.
    pub fn reset(&mut self) {
        self.processor = Processor::new();
        self.held_len = 0;
        self.with_tracker(None, |_, tracker| tracker.reset_state());
        self.dirty = true;
    }

    /// Whether events are waiting.
    pub fn has_events(&self) -> bool {
        !self.listener.0.lock().events.is_empty()
    }

    /// Everything the terminal asked for since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<VtEvent> {
        self.listener.0.lock().events.drain(..).collect()
    }

    /// Take only the [`VtEvent::Respond`] events, in order, leaving the others queued.
    pub(crate) fn take_responses(&mut self) -> Vec<Vec<u8>> {
        let mut state = self.listener.0.lock();
        let mut responses = Vec::new();
        state.events.retain_mut(|event| match event {
            VtEvent::Respond(bytes) => {
                responses.push(std::mem::take(bytes));
                false
            }
            _ => true,
        });
        responses
    }

    /// The screen and scrollback now, or as they were when the synchronized update that
    /// is open began. Cheap when nothing changed (the same snapshot again), and otherwise
    /// proportional to the rows that changed: rows Alacritty did not damage are shared
    /// with the previous snapshot.
    pub fn snapshot(&mut self) -> VtSnapshot {
        if let Some(last) = &self.last
            && (self.sync.is_some() || !self.dirty)
        {
            return last.clone();
        }
        self.refresh()
    }

    /// The screen and scrollback now, open update or not, as the snapshot to hand out
    /// from here on.
    fn refresh(&mut self) -> VtSnapshot {
        self.dirty = false;
        let snapshot = self.build();
        if let Some(last) = &self.last
            && unchanged(last, &snapshot)
        {
            return last.clone();
        }
        let mut snapshot = snapshot;
        self.generation += 1;
        snapshot.generation = self.generation;
        self.last = Some(snapshot.clone());
        snapshot
    }

    fn build(&mut self) -> VtSnapshot {
        let rows = self.rows();
        let columns = self.columns();
        let scrollback = self.scrollback.view();
        let base = scrollback.end();

        // Rows Alacritty changed since the last snapshot; every row if it lost track.
        let damaged: Option<Vec<bool>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(lines) => {
                let mut damaged = vec![false; rows];
                for line in lines {
                    if let Some(row) = damaged.get_mut(line.line) {
                        *row = true;
                    }
                }
                Some(damaged)
            }
        };
        self.term.reset_damage();

        let prev = self.last.as_ref();
        // The previous snapshot's rows are at the same positions with the same ids.
        let same_grid = prev.is_some_and(|prev| {
            prev.first_visible().0 == base && prev.columns == columns && prev.visible.len() == rows
        });
        let grid = self.term.grid();
        let mut visible: Vec<Arc<StyledLine>> = Vec::with_capacity(rows);
        for row in 0..rows {
            let id = LineId(base + row as u64);
            if same_grid
                && damaged.as_ref().is_some_and(|d| !d[row])
                && let Some(prev) = prev
            {
                visible.push(Arc::clone(&prev.visible[row]));
                continue;
            }
            visible.push(capture(
                &grid[Line(row as i32)],
                id,
                prev,
                self.last_at,
                self.offset,
                false,
            ));
        }

        let mode = *self.term.mode();
        let style = self.term.cursor_style();
        let point = grid.cursor.point;
        let cursor = CursorState {
            line: LineId(base + point.line.0.max(0) as u64),
            column: point.column.0,
            shape: match style.shape {
                VtCursorShape::Underline => CursorShape::Underline,
                VtCursorShape::Beam => CursorShape::Beam,
                VtCursorShape::HollowBlock => CursorShape::HollowBlock,
                VtCursorShape::Block | VtCursorShape::Hidden => CursorShape::Block,
            },
            visible: mode.contains(TermMode::SHOW_CURSOR) && style.shape != VtCursorShape::Hidden,
            blinking: style.blinking,
        };
        let modes = VtModes {
            alternate_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            app_cursor_keys: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            mouse_reporting: mode.intersects(TermMode::MOUSE_MODE),
        };
        let title = self.listener.0.lock().title.clone();
        let title = match (title, prev.and_then(|p| p.title.as_ref())) {
            (Some(now), Some(before)) if **before == *now => Some(Arc::clone(before)),
            (now, _) => now.map(Arc::from),
        };
        VtSnapshot {
            scrollback,
            visible: visible.into(),
            columns,
            cursor,
            modes,
            title,
            generation: self.generation,
            epoch: self.epoch,
        }
    }
}

const ESC: u8 = 0x1b;

/// How many bytes [`Stops`] looks through at once.
const BLOCK: usize = 32;

/// The bytes of a chunk that may be the `b` ending a repeat (`CSI n b`) or the `h` ending
/// the begin of a synchronized update (`CSI ? 2026 h`), by their offsets in order, going
/// by the bytes before each in the chunk. It errs only towards yes: every such `b` and
/// `h` is found, wherever the chunk begins and ends, and so are a few that are text or
/// end some other sequence, which costs a call and changes nothing.
///
/// vte acts on a sequence when its final byte comes, and it reads that byte as the final
/// one of a CSI sequence only in a state that ESC and then `[` put it in and that any
/// byte from `@` to `~` takes it out of. From that:
///
/// - The byte before the final one ([`may_precede`]). A repeat has no intermediate bytes
///   (blank to `/`) and no private marker (`<` to `?`), and the begin of an update has
///   its `?` before a parameter. So that byte is the `[`, part of a parameter (a digit,
///   `:` or `;`), or one vte passes over inside a sequence: a control character, DEL, or
///   a byte above ASCII. Most of what a device sends has no such pair in it, and is
///   passed over a block at a time in a loop that is all comparisons.
/// - The bytes before that ([`may_end_csi`]): none from `@` to `~` back to a `[`, and
///   from there none from blank to `~` back to an ESC. Where the chunk begins first, the
///   answer is yes.
/// - So a sequence whose ESC came in an earlier chunk ends by the second byte from `@`
///   to `~` of this one. From there to this chunk's first ESC nothing is looked at, which
///   for text with no ESC in it is nearly all of it.
struct Stops<'a> {
    bytes: &'a [u8],
    /// The next byte to look at.
    at: usize,
    /// From `skip.start` nothing can be a stop before `skip.end`.
    skip: Range<usize>,
    /// Up to here the bytes are in a block that holds a pair, and are gone through one by
    /// one.
    slow: usize,
}

impl<'a> Stops<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        let finals = |&(_, byte): &(usize, &u8)| (b'@'..=b'~').contains(byte);
        let second = bytes.iter().enumerate().filter(finals).nth(1);
        let head = second.map_or(bytes.len(), |(at, _)| at + 1);
        Self {
            bytes,
            at: 0,
            skip: head..find_escape(bytes).unwrap_or(bytes.len()),
            slow: 0,
        }
    }
}

impl Iterator for Stops<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        let bytes = self.bytes;
        while self.at < bytes.len() {
            if self.skip.contains(&self.at) {
                self.at = self.skip.end;
                continue;
            }
            let at = self.at;
            if at >= self.slow {
                if let Some(before) = at.checked_sub(1)
                    && let Some(before) = bytes[before..].first_chunk::<BLOCK>()
                    && let Some(block) = bytes[at..].first_chunk::<BLOCK>()
                {
                    let mut any = 0;
                    for i in 0..BLOCK {
                        any |= may_stop(before[i], block[i]);
                    }
                    if any == 0 {
                        self.at += BLOCK;
                        continue;
                    }
                }
                self.slow = at + BLOCK;
            }
            self.at += 1;
            let alone = match at.checked_sub(1) {
                Some(before) => may_stop(bytes[before], bytes[at]) != 0,
                None => matches!(bytes[at], b'b' | b'h'),
            };
            if alone && may_end_csi(bytes, at) {
                return Some(at);
            }
        }
        None
    }
}

/// The offset of the first ESC in `bytes`. Blocks with none are passed over in a loop
/// that is all comparisons.
fn find_escape(bytes: &[u8]) -> Option<usize> {
    let (blocks, _) = bytes.as_chunks::<BLOCK>();
    let mut offset = 0;
    for block in blocks {
        if block.iter().fold(false, |any, &byte| any | (byte == ESC)) {
            break;
        }
        offset += BLOCK;
    }
    bytes[offset..]
        .iter()
        .position(|&byte| byte == ESC)
        .map(|at| offset + at)
}

/// 1 when `byte` is a `b` or an `h` that, after `before`, may be a stop (see [`Stops`]),
/// and 0 when not: a number, so that a block of them is one sum.
#[inline(always)]
fn may_stop(before: u8, byte: u8) -> u8 {
    (u8::from(byte == b'b') | u8::from(byte == b'h')) & may_precede(before)
}

/// 1 when `byte` may come right before the final byte of a repeat or of the begin of a
/// synchronized update: `[`, `0` to `;`, a control character, DEL, or above ASCII.
#[inline(always)]
fn may_precede(byte: u8) -> u8 {
    u8::from(byte == b'[')
        | u8::from((b'0'..=b';').contains(&byte))
        | u8::from(!(0x20..0x7f).contains(&byte))
}

/// Whether the byte at `at` may be the final byte of a CSI sequence, going by the bytes
/// before it: see [`Stops`].
fn may_end_csi(bytes: &[u8], at: usize) -> bool {
    let mut before = bytes[..at].iter().rev();
    // Back to the `[`, and no other byte that would have ended the sequence.
    loop {
        match before.next() {
            None => return true,
            Some(b'[') => break,
            Some(b'@'..=b'~') => return false,
            Some(_) => {}
        }
    }
    // From there back to the ESC, over what vte lets stand between the two.
    loop {
        match before.next() {
            None | Some(&ESC) => return true,
            Some(b' '..=b'~') => return false,
            Some(_) => {}
        }
    }
}

/// `row` as the line with id `id`, sharing the previous snapshot's line when that one
/// had the same id and draws the same (so it keeps its arrival time), and otherwise new,
/// stamped `at`.
fn capture(
    row: &Row<Cell>,
    id: LineId,
    prev: Option<&VtSnapshot>,
    at: Instant,
    offset: u64,
    complete: bool,
) -> Arc<StyledLine> {
    let line = build_line(
        row,
        id,
        Stamp {
            received_at: at,
            raw: offset..offset,
            complete,
        },
    );
    let before = prev.and_then(|prev| {
        let first = prev.first_visible().0;
        let row = id.0.checked_sub(first)?;
        prev.visible.get(row as usize)
    });
    match before {
        Some(before) if same_content(before, &line) => {
            if before.complete == complete {
                Arc::clone(before)
            } else {
                Arc::new(StyledLine {
                    received_at: before.received_at,
                    raw: before.raw.clone(),
                    ..line
                })
            }
        }
        _ => Arc::new(line),
    }
}

/// Whether two snapshots of one screen show the same thing.
fn unchanged(a: &VtSnapshot, b: &VtSnapshot) -> bool {
    a.scrollback.same_as(&b.scrollback)
        && a.columns == b.columns
        && a.cursor == b.cursor
        && a.modes == b.modes
        && a.title == b.title
        && a.visible.len() == b.visible.len()
        && a.visible
            .iter()
            .zip(b.visible.iter())
            .all(|(x, y)| Arc::ptr_eq(x, y))
}

/// Whether every cell on screen is empty by Alacritty's own test, the one its
/// clear-screen scan uses.
fn is_blank(grid: &Grid<Cell>) -> bool {
    (0..grid.screen_lines()).all(|row| grid[Line(row as i32)][..].iter().all(GridCell::is_empty))
}

/// Moves rows from Alacritty's history into the scrollback.
struct Absorb<'a> {
    scrollback: &'a mut Scrollback,
    prev: Option<&'a VtSnapshot>,
    at: Instant,
    offset: u64,
}

impl Absorb<'_> {
    /// Take whatever scrolled off the active screen into the scrollback and empty the
    /// history. The alternate screen has no history, so this only ever takes rows from
    /// the primary one.
    #[cold]
    fn absorb(&mut self, term: &mut Term<Listener>) {
        let grid = term.grid();
        let n = grid.history_size();
        if n == 0 {
            return;
        }
        // Rows beyond the capacity would be evicted at once: count them, skip the work.
        let keep = n.min(self.scrollback.capacity());
        self.scrollback.skip((n - keep) as u64);
        // Line(-n) is the oldest row in the history, Line(-1) the newest.
        for back in (1..=keep).rev() {
            let id = LineId(self.scrollback.end());
            let line = capture(
                &grid[Line(-(back as i32))],
                id,
                self.prev,
                self.at,
                self.offset,
                true,
            );
            self.scrollback.push(line);
        }
        term.grid_mut().clear_history();
    }
}

/// The `Handler` the parser drives: every call goes to the `Term`, then rows that
/// scrolled off go to the scrollback. See the module docs.
///
/// Every method of `vte::ansi::Handler` must be forwarded: the trait gives each a
/// default that does nothing, so a missing one would silently drop a sequence. The list
/// matches vte 0.15; check it against `Handler` when bumping `alacritty_terminal`.
struct Tracker<'a> {
    term: &'a mut Term<Listener>,
    listener: &'a Listener,
    rows: Absorb<'a>,
    /// Where the cursor is, when the cells from it to the end of its row are known to be
    /// all equal and no part of a wide character: set by an insert-mode character
    /// written without moving them, and forgotten on any other call. See the module docs.
    equal_from: Option<Point>,
    /// The screen's open synchronized update: when it times out.
    sync: &'a mut Option<Instant>,
    /// The last private mode set began a synchronized update. The bytes of a call end
    /// with the sequence that sets it (see [`VtScreen::parse`]), so nothing came after.
    begun: bool,
    /// How many more characters to take in this call; the rest are dropped. A screenful
    /// when the call parses the end of what may be a repeat, and `None`, for no limit,
    /// otherwise. See the module docs.
    characters: Option<usize>,
}

impl Tracker<'_> {
    /// After every call but a character: whatever it did, the row may have changed.
    #[inline(always)]
    fn after(&mut self) {
        self.equal_from = None;
        self.absorb_scrolled();
    }

    #[inline(always)]
    fn absorb_scrolled(&mut self) {
        if self.term.grid().history_size() != 0 {
            self.rows.absorb(self.term);
        }
    }

    /// Whether Alacritty's `input` would, for a character `width` cells wide, move the
    /// cells from the cursor to the end of the row and leave the row as it was: insert
    /// mode is on, the character goes on this row with room after it (otherwise `input`
    /// wraps first or moves nothing), and those cells are all equal. Cells of a wide
    /// character do not count as equal: writing over one changes its neighbour.
    fn insert_moves_nothing(&self, width: usize) -> bool {
        if !self.term.mode().contains(TermMode::INSERT) {
            return false;
        }
        let grid = self.term.grid();
        let point = grid.cursor.point;
        if grid.cursor.input_needs_wrap || point.column.0 + width >= grid.columns() {
            return false;
        }
        if self.equal_from == Some(point) {
            return true;
        }
        let rest = &grid[point.line][point.column..];
        let wide = Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        !rest[0].flags.intersects(wide) && rest.windows(2).all(|pair| pair[0] == pair[1])
    }

    /// Whether the cell a zero-width character would join already holds
    /// [`MAX_ZERO_WIDTH`] of them. The cell is the one Alacritty's `input` picks: the one
    /// before the cursor, or the cursor's own when the last character filled the row and
    /// the next will wrap, and the first cell of a wide character.
    fn zero_width_full(&self) -> bool {
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let line = cursor.point.line;
        let mut column = cursor.point.column;
        if !cursor.input_needs_wrap {
            column.0 = column.0.saturating_sub(1);
        }
        if grid[line][column].flags.contains(Flags::WIDE_CHAR_SPACER) {
            column.0 = column.0.saturating_sub(1);
        }
        grid[line][column]
            .zerowidth()
            .is_some_and(|marks| marks.len() >= MAX_ZERO_WIDTH)
    }
}

macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            #[inline]
            fn $name(&mut self, $($arg: $ty),*) {
                Handler::$name(&mut *self.term, $($arg),*);
                self.after();
            }
        )*
    };
}

impl Handler for Tracker<'_> {
    forward! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: VtCursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(count: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        linefeed();
        bell();
        substitute();
        newline();
        set_horizontal_tabstop();
        scroll_up(rows: usize);
        scroll_down(rows: usize);
        insert_blank_lines(rows: usize);
        delete_lines(rows: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, base64: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(hyperlink: Option<Hyperlink>);
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(to_pop: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }

    /// A character, unless this call has had its fill of them (a long repeat). A
    /// zero-width one joins a cell unless that cell is full, in which case it is dropped.
    /// In insert mode, one that would move a run of equal cells is written without the
    /// move. See the module docs for all three.
    fn input(&mut self, c: char) {
        if let Some(left) = &mut self.characters {
            if *left == 0 {
                return;
            }
            *left -= 1;
        }
        let width = c.width();
        if width == Some(0) && self.zero_width_full() {
            return;
        }
        if width.is_some_and(|width| width > 0 && self.insert_moves_nothing(width)) {
            // Insert mode is all that makes `input` move the cells. Turning it off marks
            // the whole screen damaged, which a screen in insert mode is anyway.
            let insert = Mode::Named(NamedMode::Insert);
            Handler::unset_mode(&mut *self.term, insert);
            Handler::input(&mut *self.term, c);
            Handler::set_mode(&mut *self.term, insert);
            // The cells after the character were not touched. On the last column the
            // cursor stays on the character instead, waiting to wrap.
            let cursor = &self.term.grid().cursor;
            self.equal_from = (!cursor.input_needs_wrap).then_some(cursor.point);
        } else {
            Handler::input(&mut *self.term, c);
            self.equal_from = None;
        }
        self.absorb_scrolled();
    }

    /// `CSI ? Pm h`. Mode 2026 begins a synchronized update, unless one is open: a
    /// second begin does not extend the first.
    fn set_private_mode(&mut self, mode: PrivateMode) {
        Handler::set_private_mode(&mut *self.term, mode);
        self.after();
        self.begun = mode == SYNC_UPDATE && self.sync.is_none();
        if self.begun {
            *self.sync = Some(self.rows.at + SYNC_TIMEOUT);
        }
    }

    /// `CSI ? Pm l`. Mode 2026 ends the synchronized update.
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        Handler::unset_private_mode(&mut *self.term, mode);
        self.after();
        if mode == SYNC_UPDATE {
            *self.sync = None;
        }
    }

    /// `CSI ? Ps $ p`. Alacritty reports mode 2026 as reset, since with vte holding the
    /// bytes it never saw the question inside an update. Here it can.
    fn report_private_mode(&mut self, mode: PrivateMode) {
        if mode == SYNC_UPDATE && self.sync.is_some() {
            self.listener.0.lock().respond(b"\x1b[?2026;1$y");
        } else {
            Handler::report_private_mode(&mut *self.term, mode);
        }
        self.after();
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let primary = !self.term.mode().contains(TermMode::ALT_SCREEN);
        // Clearing the whole primary screen scrolls it up to its last non-empty cell,
        // found by scanning back from the bottom until a non-empty cell or the history.
        // With the history empty, as the tracker keeps it, a blank screen stops the scan
        // at row 0 and scrolls one blank row off. Alacritty with history would scroll
        // none, so that row is dropped rather than kept.
        let blank = primary && matches!(mode, ClearMode::All) && is_blank(self.term.grid());
        // Erase saved lines. The history Alacritty would clear has already moved here.
        // Like Alacritty, only on the primary screen.
        let saved = primary && matches!(mode, ClearMode::Saved);
        Handler::clear_screen(&mut *self.term, mode);
        if blank {
            self.term.grid_mut().clear_history();
        }
        self.after();
        if saved {
            self.rows.scrollback.clear();
        }
    }

    fn reset_state(&mut self) {
        Handler::reset_state(&mut *self.term);
        self.after();
        *self.sync = None;
        self.rows.scrollback.clear();
        // Alacritty clears its title on a full reset without telling its listener.
        self.listener.0.lock().reset_title();
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use proptest::prelude::*;

    use super::*;

    /// One thing a device sends.
    #[derive(Clone, Debug)]
    enum Sent {
        Text(String),
        /// `CSI n b`: repeat the last character.
        Repeat(u16),
        InsertMode(bool),
        Wrap(bool),
        Goto(u8, u8),
        Control(u8),
        Color(u8),
        /// `CSI n` and this final byte: erase in line, and insert, delete or erase cells.
        Edit(u8, u8),
        ReverseIndex,
    }

    impl Sent {
        fn write(&self, out: &mut Vec<u8>, screenful: usize) {
            match self {
                Sent::Text(text) => out.extend_from_slice(text.as_bytes()),
                // No more than the screen caps it at, which Alacritty does not.
                Sent::Repeat(n) => {
                    let n = usize::from(*n).min(screenful);
                    out.extend_from_slice(format!("\x1b[{n}b").as_bytes())
                }
                Sent::InsertMode(on) => {
                    out.extend_from_slice(if *on { b"\x1b[4h" } else { b"\x1b[4l" })
                }
                Sent::Wrap(on) => {
                    out.extend_from_slice(if *on { b"\x1b[?7h" } else { b"\x1b[?7l" })
                }
                Sent::Goto(row, column) => {
                    out.extend_from_slice(format!("\x1b[{row};{column}H").as_bytes())
                }
                Sent::Control(byte) => out.push(*byte),
                Sent::Color(n) => out.extend_from_slice(format!("\x1b[{n}m").as_bytes()),
                Sent::Edit(n, last) => {
                    out.extend_from_slice(format!("\x1b[{n}{}", char::from(*last)).as_bytes())
                }
                Sent::ReverseIndex => out.extend_from_slice(b"\x1bM"),
            }
        }
    }

    fn sent() -> impl Strategy<Value = Sent> {
        prop_oneof![
            // Narrow and wide characters, and blanks. No combining marks: the screen
            // caps them and Alacritty does not.
            4 => "[ab \u{65e5}]{1,6}".prop_map(Sent::Text),
            4 => (0u16..40).prop_map(Sent::Repeat),
            3 => any::<bool>().prop_map(Sent::InsertMode),
            1 => any::<bool>().prop_map(Sent::Wrap),
            3 => (1u8..6, 1u8..12).prop_map(|(row, column)| Sent::Goto(row, column)),
            2 => prop_oneof![Just(b'\r'), Just(b'\n'), Just(0x08u8), Just(b'\t')]
                .prop_map(Sent::Control),
            2 => prop_oneof![Just(0u8), Just(7), Just(31), Just(44)].prop_map(Sent::Color),
            2 => (0u8..4, prop_oneof![Just(b'K'), Just(b'@'), Just(b'P'), Just(b'X')])
                .prop_map(|(n, last)| Sent::Edit(n, last)),
            1 => Just(Sent::ReverseIndex),
        ]
    }

    /// Notes where vte acts on a repeat or on the begin of a synchronized update, by
    /// stopping the parser there.
    #[derive(Default)]
    struct Finder {
        found: bool,
    }

    impl alacritty_terminal::vte::Perform for Finder {
        fn csi_dispatch(
            &mut self,
            params: &alacritty_terminal::vte::Params,
            intermediates: &[u8],
            ignore: bool,
            action: char,
        ) {
            // As vte's `ansi` module reads them.
            self.found |= !ignore
                && match (action, intermediates) {
                    ('b', []) => true,
                    ('h', [b'?']) => params.iter().any(|param| param[0] == 2026),
                    _ => false,
                };
        }

        fn terminated(&self) -> bool {
            self.found
        }
    }

    /// The offsets in `stream` of the final bytes of the repeats and update begins vte
    /// finds in it.
    fn stops_vte_acts_on(stream: &[u8]) -> Vec<usize> {
        let mut parser = alacritty_terminal::vte::Parser::new();
        let mut stops = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let mut finder = Finder::default();
            at += parser.advance_until_terminated(&mut finder, &stream[at..]);
            if finder.found {
                stops.push(at - 1);
            }
        }
        stops
    }

    /// Bytes that sequences are made of, with the ones that matter here made likely.
    fn sequence_soup() -> impl Strategy<Value = Vec<u8>> {
        let byte = prop_oneof![
            6 => Just(ESC),
            6 => Just(b'['),
            6 => Just(b'b'),
            3 => Just(b'h'),
            2 => Just(b'?'),
            8 => prop_oneof![Just(b'2'), Just(b'0'), Just(b'6'), Just(b'9'), Just(b';'), Just(b':')],
            // Intermediates, private markers, controls (CAN and SUB among them), DEL,
            // bytes above ASCII, other final bytes, and what begins a string.
            8 => prop::sample::select(
                &b" !/<>=\n\r\t\x18\x1a\x7f\x80\x9b\xc3\xa9\xf0\x9fmlHa@~]PX^_\\\x07"[..]
            ),
        ];
        proptest::collection::vec(byte, 0..120)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4096))]

        /// However a stream is cut into chunks, `Stops` finds every repeat and every begin
        /// of a synchronized update that vte acts on.
        #[test]
        fn stops_are_where_vte_acts(
            stream in sequence_soup(),
            chunk in 1usize..90,
            lead in 0usize..90,
        ) {
            let wanted = stops_vte_acts_on(&stream);
            let mut found = Vec::new();
            // A first chunk of another length, so that cuts fall everywhere.
            let (first, rest) = stream.split_at(lead.min(stream.len()));
            let mut offset = 0;
            for piece in std::iter::once(first).chain(rest.chunks(chunk)) {
                found.extend(Stops::new(piece).map(|stop| offset + stop));
                offset += piece.len();
            }
            for stop in &wanted {
                prop_assert!(found.contains(stop), "missed {} in {:x?}", stop, stream);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Insert mode without Alacritty's move of equal cells leaves every cell, the
        /// cursor and the modes as Alacritty alone leaves them.
        #[test]
        fn insert_mode_matches_alacritty(
            columns in 2usize..10,
            rows in 1usize..5,
            stream in proptest::collection::vec(sent(), 1..40),
            cut in any::<prop::sample::Index>(),
        ) {
            let mut bytes = Vec::new();
            for sent in &stream {
                sent.write(&mut bytes, columns * rows);
            }

            // The screen in two chunks: what it remembers about a row does not outlive one.
            let cut = cut.index(bytes.len() + 1);
            let mut screen = VtScreen::new(columns, rows, 100);
            screen.feed(&bytes[..cut]);
            screen.feed(&bytes[cut..]);

            let mut alacritty = Term::new(Config::default(), &Size { columns, rows }, VoidListener);
            Processor::<NoHold>::new()
                .advance(&mut alacritty, &bytes);

            let (ours, theirs) = (screen.term.grid(), alacritty.grid());
            for row in 0..rows {
                let line = Line(row as i32);
                prop_assert_eq!(&ours[line][..], &theirs[line][..], "row {}", row);
            }
            prop_assert_eq!(ours.cursor.point, theirs.cursor.point);
            prop_assert_eq!(ours.cursor.input_needs_wrap, theirs.cursor.input_needs_wrap);
            prop_assert_eq!(screen.term.mode(), alacritty.mode());
        }
    }
}
