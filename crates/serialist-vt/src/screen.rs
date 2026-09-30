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

use std::sync::Arc;
use std::time::Instant;

use alacritty_terminal::grid::{Dimensions, Grid, GridCell, Row};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::Cell;
use alacritty_terminal::term::{Config, Osc52, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape as VtCursorShape, CursorStyle, Handler, Hyperlink,
    KeyboardModes, KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode,
    Processor, Rgb, ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
    cursor_icon::CursorIcon,
};
use serialist_core::{Epoch, LineId, StyledLine};

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
    processor: Processor,
    scrollback: Scrollback,
    /// The snapshot handed out last: rows are reused from it when unchanged.
    last: Option<VtSnapshot>,
    /// Something may have changed since `last`.
    dirty: bool,
    generation: u64,
    epoch: Epoch,
    /// Bytes fed so far: the stream offset of the next byte.
    offset: u64,
    /// When the last chunk arrived; the time given to rows it changed.
    last_at: Instant,
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
            generation: 0,
            epoch,
            offset: 0,
            last_at: epoch.instant,
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
    /// chunk changes get `at` as their `received_at`.
    pub fn feed_at(&mut self, bytes: &[u8], at: Instant) {
        if bytes.is_empty() {
            return;
        }
        self.last_at = at;
        self.flush_sync(Instant::now());
        self.with_tracker(|processor, tracker| processor.advance(tracker, bytes));
        self.offset += bytes.len() as u64;
        self.dirty = true;
    }

    /// End a synchronized update (`CSI ? 2026 h`) the device began more than 150 ms
    /// before `now` and never ended, applying what it buffered. Feeding does this on its
    /// own; call it when no bytes arrive (the sink does, on idle) so a device that stops
    /// mid-update does not freeze the screen. Returns whether an update was ended.
    pub fn flush_sync(&mut self, now: Instant) -> bool {
        let expired = self
            .processor
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|deadline| now >= deadline);
        if expired {
            self.with_tracker(|processor, tracker| processor.stop_sync(tracker));
            self.dirty = true;
        }
        expired
    }

    /// Run `f` with the parser and a tracker over this screen, stamping rows with the
    /// time of the last chunk.
    fn with_tracker(&mut self, f: impl FnOnce(&mut Processor, &mut Tracker<'_>)) {
        let mut tracker = Tracker {
            term: &mut self.term,
            listener: &self.listener,
            rows: Absorb {
                scrollback: &mut self.scrollback,
                prev: self.last.as_ref(),
                at: self.last_at,
                offset: self.offset,
            },
        };
        f(&mut self.processor, &mut tracker);
    }

    /// Whether a synchronized update is holding bytes back.
    pub fn sync_pending(&self) -> bool {
        self.processor.sync_timeout().sync_timeout().is_some()
    }

    /// End a synchronized update now, timed out or not, applying what it buffered (the
    /// link is gone, say). Returns whether one was open.
    pub fn end_sync(&mut self) -> bool {
        if !self.sync_pending() {
            return false;
        }
        self.with_tracker(|processor, tracker| processor.stop_sync(tracker));
        self.dirty = true;
        true
    }

    /// Resize the screen to `columns` by `rows` (clamped as in [`new`](Self::new)).
    ///
    /// Alacritty keeps the cursor row on screen: shrinking pushes rows off the top into
    /// the scrollback, and a narrower primary screen rewraps its rows. Growing adds blank
    /// rows at the bottom; rows are not pulled back out of the scrollback, whose lines
    /// and ids never change.
    pub fn resize(&mut self, columns: usize, rows: usize) {
        let size = clamp_size(columns, rows);
        if size.columns == self.columns() && size.rows == self.rows() {
            return;
        }
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
    /// half-received escape sequence. Line ids keep counting up.
    pub fn reset(&mut self) {
        self.processor = Processor::new();
        self.with_tracker(|_, tracker| tracker.reset_state());
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

    /// The screen and scrollback now. Cheap when nothing changed (the same snapshot
    /// again), and otherwise proportional to the rows that changed: rows Alacritty did
    /// not damage are shared with the previous snapshot.
    pub fn snapshot(&mut self) -> VtSnapshot {
        if !self.dirty
            && let Some(last) = &self.last
        {
            return last.clone();
        }
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
}

impl Tracker<'_> {
    #[inline(always)]
    fn after(&mut self) {
        if self.term.grid().history_size() != 0 {
            self.rows.absorb(self.term);
        }
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
        input(c: char);
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
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
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
        self.rows.scrollback.clear();
        // Alacritty clears its title on a full reset without telling its listener.
        self.listener.0.lock().reset_title();
    }
}
