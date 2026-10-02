//! `vt_screen`: [`VtScreen`] (Alacritty's terminal emulator fed without a PTY) on
//! arbitrary bytes in arbitrary chunks, with a mid-stream resize.
//!
//! The input is an [`Input`]. Config bits 0..=2 pick the screen size `(columns, rows)`
//! from [`SIZES`] (config 0 is 80 by 24, a real console); bits 3..=4 pick the scrollback
//! capacity from [`SCROLLBACK`]; bit 5 resizes the screen halfway through the chunks, to
//! the size with columns and rows swapped (clamped by the screen).
//!
//! The same stream is fed twice, and the results must match:
//!
//! - the split run feeds [`Input::chunks`] one by one with `feed_at`, all at one fixed
//!   `Instant`, taking a snapshot every so often (at most [`MAX_SNAPSHOTS`] of them, and
//!   after the resize) the way the UI does, so the damage-driven reuse of rows is on the
//!   path;
//! - the whole run feeds the bytes before the resize as one chunk, resizes, and feeds the
//!   rest as one chunk, so both runs resize at the same byte offset.
//!
//! Both runs call `end_sync` before the comparison, and before the resize. `feed_at` reads
//! the clock to time out a synchronized update (`CSI ? 2026 h` with no end), so an update
//! left open must not decide the result. The split run takes no snapshot while an update
//! is open, so its checks never spend the 150 ms; only a machine that stalls that long
//! inside one chunk's parse could still time an update out, and that input replays
//! without the stall.
//!
//! The resize is the one thing besides the comparison that would see an open update:
//! vte 0.15.0 does not keep one independent of chunking when a begin that is not byte for
//! byte `ESC [ ? 2026 h` (say `ESC [ ? 2026 ; 1 h`) follows an end in the same chunk. That
//! chunk ignores the begin, while a chunk that starts with it opens an update and holds
//! the rest back until the update ends or times out. Ending the update first makes the
//! screens agree again (`regress_sync_begin_variant_at_resize`).
//!
//! What must be equal: every line from `first_line()` to `end()` (id, text, runs and
//! `complete`; the times and raw offsets belong to the chunk a row was captured in), the
//! cursor, the modes, the title, the size, and the events `take_events` returns, including
//! the answer each color request would give. This is the screen's promise that any split
//! of a stream gives the same screen. (It found vte 0.15.0 skipping the text after a
//! character cut by a chunk boundary: `seeds/vt_screen/regress_utf8_*`.)
//!
//! Beyond that, on the final snapshot and on every snapshot taken along the way:
//!
//! - Bounds: `bytes_fed` is the stream's length; the size is the requested one clamped to
//!   `MIN_COLUMNS..=MAX_COLUMNS` by `MIN_ROWS..=MAX_ROWS`; `line_count` is the
//!   scrollback kept plus the rows and at most `rows + scrollback_lines`; one drain holds
//!   at most [`MAX_EVENTS`] events.
//! - Ids: lines are consecutive from `first_line`; `first_visible` follows the scrollback;
//!   scrollback rows are `complete` and screen rows are not; the cursor is on a screen
//!   row and inside the columns.
//! - Lines: runs cover the text on character boundaries, none is empty, neighbours differ
//!   in style, the text has no control characters. The text has at most `columns`
//!   characters below U+0300 (a wide character is one character in two cells; a
//!   combining mark takes no cell, and every zero-width character is at or above U+0300),
//!   and at most `columns * (1 + MAX_ZERO_WIDTH)` characters in all, since a cell keeps
//!   at most [`MAX_ZERO_WIDTH`] zero-width characters (`regress_zero_width_rep`: a
//!   repeated `CSI 65535 b` on a combining mark, which grew one cell without bound).
//!   Scrollback rows are not reflowed by a resize, so for them `columns` is the most the
//!   screen has had.
//! - Events: no two bells, no two equal titles and no two title resets in a row; the
//!   title the snapshot shows is the one the events last set, when none were dropped.
//!
//! And between two snapshots of the split run, which are the stable-id promises:
//! the generation goes up by one when the snapshot's content changed, and equal
//! generations mean equal content (the reverse is not promised: erasing an empty
//! scrollback with `CSI 3 J` takes a new generation for the same content); ids never go
//! backwards; scrollback rows that stay retained read back unchanged; and every row
//! whose text or runs differ from the earlier snapshot's is inside
//! [`changed_since`](VtSnapshot::changed_since)'s range.
//!
//! Speed: the first row to scroll off a fresh Alacritty screen makes it allocate 1000
//! rows, so a run costs milliseconds before any check does.

use std::time::Instant;

use serialist_core::{Direction, LineId, LineSource, StyledLine};
use serialist_vt::{
    DEFAULT_SCROLLBACK, MAX_COLUMNS, MAX_EVENTS, MAX_ROWS, MAX_ZERO_WIDTH, MIN_COLUMNS, MIN_ROWS,
    VtEvent, VtScreen, VtSnapshot,
};

use crate::Input;

/// The `(columns, rows)` config bits 0..=2 choose from: a console, the smallest screens,
/// a wide and a tall one, and the largest in each direction.
pub const SIZES: [(usize, usize); 8] = [
    (80, 24),
    (2, 1),
    (10, 3),
    (132, 50),
    (1024, 2),
    (40, 1024),
    (MIN_COLUMNS, MAX_ROWS),
    (MAX_COLUMNS, MIN_ROWS),
];

/// The scrollback capacities config bits 3..=4 choose from.
pub const SCROLLBACK: [usize; 4] = [DEFAULT_SCROLLBACK, 0, 1, 100];

/// The most snapshots the split run takes between its first and its last. Every chunk
/// could be one (a stream fed a byte at a time has thousands), but a snapshot of a
/// 1024-row screen costs more than the byte it follows.
pub const MAX_SNAPSHOTS: usize = 32;

/// One thing done to a screen.
#[derive(Clone, Copy, Debug)]
enum Step<'a> {
    Feed(&'a [u8]),
    /// Resize to the swapped size.
    Resize,
}

/// What a run starts from.
#[derive(Clone, Copy, Debug)]
struct Setup {
    columns: usize,
    rows: usize,
    scrollback: usize,
    /// The one time every chunk arrives at.
    at: Instant,
}

/// The size of a screen, as a run has resized it.
#[derive(Clone, Copy, Debug)]
struct Dims {
    columns: usize,
    rows: usize,
    /// The most columns it has had. A resize does not reflow the scrollback, so rows that
    /// scrolled off before a narrowing keep their width.
    widest: usize,
}

impl Dims {
    /// The size the screen clamps `columns` by `rows` to.
    fn new(columns: usize, rows: usize) -> Self {
        let columns = columns.clamp(MIN_COLUMNS, MAX_COLUMNS);
        Self {
            columns,
            rows: rows.clamp(MIN_ROWS, MAX_ROWS),
            widest: columns,
        }
    }

    /// The size after the resize to columns and rows swapped.
    fn swapped(self) -> Self {
        let next = Self::new(self.rows, self.columns);
        Self {
            widest: self.widest.max(next.columns),
            ..next
        }
    }

    fn matches(self, screen: &VtScreen) -> bool {
        (screen.columns(), screen.rows()) == (self.columns, self.rows)
    }
}

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    let (columns, rows) = input.pick(0, &SIZES[..]);
    let setup = Setup {
        columns,
        rows,
        scrollback: input.pick(3, &SCROLLBACK[..]),
        at: Instant::now(),
    };

    let chunks: Vec<&[u8]> = input.chunks().collect();
    let mut split: Vec<Step> = chunks.iter().map(|&chunk| Step::Feed(chunk)).collect();
    let mut whole = vec![Step::Feed(input.stream)];
    if input.flag(5) {
        let half = chunks.len() / 2;
        let before: usize = chunks[..half].iter().map(|chunk| chunk.len()).sum();
        split.insert(half, Step::Resize);
        whole = vec![
            Step::Feed(&input.stream[..before]),
            Step::Resize,
            Step::Feed(&input.stream[before..]),
        ];
    }

    let split = drive(&setup, &split, input.stream.len(), true);
    let whole = drive(&setup, &whole, input.stream.len(), false);
    if let Some(diff) = split.seen.first_difference(&whole.seen) {
        panic!(
            "split against whole: {}",
            split.seen.explain(diff, &whole.seen)
        );
    }
    assert_eq!(split.events, whole.events, "events, split against whole");
}

/// A snapshot with its lines, read once and checked.
struct Seen {
    snapshot: VtSnapshot,
    /// `first_line()` to `end()`.
    lines: Vec<StyledLine>,
}

/// What differs between two [`Seen`]s, in the things that must not depend on chunking.
#[derive(Clone, Copy, Debug)]
enum Diff {
    FirstLine,
    FirstVisible,
    Size,
    LineCount,
    /// The line at this index.
    Line(usize),
    Cursor,
    Modes,
    Title,
}

impl Seen {
    fn first_line(&self) -> LineId {
        self.snapshot.first_line()
    }

    /// The line with id `id`, if the snapshot has it.
    fn line(&self, id: LineId) -> Option<&StyledLine> {
        let index = id.0.checked_sub(self.first_line().0)?;
        self.lines.get(usize::try_from(index).ok()?)
    }

    /// The first thing that differs between the two: the lines (id, text, runs and
    /// `complete`, not the times and raw offsets of the chunk they arrived in), the
    /// cursor, the modes, the title and the size.
    fn first_difference(&self, other: &Seen) -> Option<Diff> {
        let (a, b) = (&self.snapshot, &other.snapshot);
        if a.first_line() != b.first_line() {
            return Some(Diff::FirstLine);
        }
        if a.first_visible() != b.first_visible() {
            return Some(Diff::FirstVisible);
        }
        if (a.columns(), a.viewport_rows()) != (b.columns(), b.viewport_rows()) {
            return Some(Diff::Size);
        }
        if self.lines.len() != other.lines.len() {
            return Some(Diff::LineCount);
        }
        for (i, (x, y)) in self.lines.iter().zip(&other.lines).enumerate() {
            if (x.id, &x.text, &x.runs, x.complete) != (y.id, &y.text, &y.runs, y.complete) {
                return Some(Diff::Line(i));
            }
        }
        if a.cursor() != b.cursor() {
            return Some(Diff::Cursor);
        }
        if a.modes() != b.modes() {
            return Some(Diff::Modes);
        }
        if a.title() != b.title() {
            return Some(Diff::Title);
        }
        None
    }

    fn explain(&self, diff: Diff, other: &Seen) -> String {
        let (a, b) = (&self.snapshot, &other.snapshot);
        match diff {
            Diff::FirstLine => format!(
                "first_line {:?} against {:?}",
                a.first_line(),
                b.first_line()
            ),
            Diff::FirstVisible => format!(
                "first_visible {:?} against {:?}",
                a.first_visible(),
                b.first_visible()
            ),
            Diff::Size => format!(
                "size {}x{} against {}x{}",
                a.columns(),
                a.viewport_rows(),
                b.columns(),
                b.viewport_rows()
            ),
            Diff::LineCount => format!("{} lines against {}", self.lines.len(), other.lines.len()),
            Diff::Line(i) => format!("line {i}: {:?} against {:?}", self.lines[i], other.lines[i]),
            Diff::Cursor => format!("cursor {:?} against {:?}", a.cursor(), b.cursor()),
            Diff::Modes => format!("modes {:?} against {:?}", a.modes(), b.modes()),
            Diff::Title => format!("title {:?} against {:?}", a.title(), b.title()),
        }
    }
}

/// A run's end state: the last snapshot and the events drained after it.
struct Observed {
    seen: Seen,
    /// Each event with, for a color request, the bytes that answer it with a fixed color.
    events: Vec<(VtEvent, Vec<u8>)>,
}

/// Run `steps` on a fresh screen. With `watch`, take snapshots along the way and check
/// each against the one before.
fn drive(setup: &Setup, steps: &[Step], stream_len: usize, watch: bool) -> Observed {
    let mut screen = VtScreen::new(setup.columns, setup.rows, setup.scrollback);
    let mut dims = Dims::new(setup.columns, setup.rows);
    assert!(dims.matches(&screen), "the new screen's size, {dims:?}");

    let stride = (steps.len() / MAX_SNAPSHOTS).max(1);
    let mut previous = None;
    if watch {
        previous = Some(look(&mut screen, dims, setup, None));
    }
    for (i, step) in steps.iter().enumerate() {
        match *step {
            Step::Feed(bytes) => screen.feed_at(bytes, setup.at),
            Step::Resize => {
                screen.end_sync();
                screen.resize(dims.rows, dims.columns);
                dims = dims.swapped();
                assert!(dims.matches(&screen), "the resized size, {dims:?}");
            }
        }
        // Not while an update is open: see the module docs.
        if watch && !screen.sync_pending() && (i % stride == 0 || matches!(step, Step::Resize)) {
            previous = Some(look(&mut screen, dims, setup, previous.as_ref()));
        }
    }

    // `feed_at` times an update out by the clock; an end that does not is not the clock's.
    screen.end_sync();
    assert!(!screen.sync_pending(), "end_sync left an update open");
    assert!(!screen.end_sync(), "end_sync found a second update to end");

    assert_eq!(screen.bytes_fed(), stream_len as u64, "bytes fed");
    let seen = look(&mut screen, dims, setup, previous.as_ref());
    check_clipping(&seen.snapshot, &seen.lines);
    let again = screen.snapshot();
    assert_eq!(
        again.generation(),
        seen.snapshot.generation(),
        "a snapshot with nothing new fed has a new generation"
    );

    let queued = screen.has_events();
    let events = screen.take_events();
    assert_eq!(queued, !events.is_empty(), "has_events and take_events");
    assert!(
        !screen.has_events() && screen.take_events().is_empty(),
        "events left after a drain"
    );
    check_events(&events, seen.snapshot.title());
    let events = events
        .into_iter()
        .map(|event| {
            let answer = match &event {
                VtEvent::ColorRequest(request) => request.answer(0x12, 0x34, 0x56),
                _ => Vec::new(),
            };
            (event, answer)
        })
        .collect();
    Observed { seen, events }
}

/// Take a snapshot and check it, and how it follows `previous`.
fn look(screen: &mut VtScreen, dims: Dims, setup: &Setup, previous: Option<&Seen>) -> Seen {
    let snapshot = screen.snapshot();
    let seen = read(snapshot, dims, setup, screen.bytes_fed());
    if let Some(previous) = previous {
        check_step(previous, &seen);
    }
    seen
}

/// Read every line of `snapshot`, a screen of size `dims` that has been fed `fed` bytes,
/// checking what one snapshot must hold.
fn read(snapshot: VtSnapshot, dims: Dims, setup: &Setup, fed: u64) -> Seen {
    let Dims { columns, rows, .. } = dims;
    assert_eq!(snapshot.columns(), columns, "snapshot columns");
    assert_eq!(snapshot.viewport_rows(), rows, "snapshot rows");
    assert!(
        snapshot.scrollback_lines() <= setup.scrollback,
        "{} scrollback rows kept, capacity {}",
        snapshot.scrollback_lines(),
        setup.scrollback
    );
    assert_eq!(
        snapshot.line_count(),
        snapshot.scrollback_lines() + rows,
        "line count is scrollback plus rows"
    );
    assert!(snapshot.line_count() <= rows + setup.scrollback);
    let first = snapshot.first_line();
    let first_visible = snapshot.first_visible();
    assert_eq!(first_visible, first.offset(snapshot.scrollback_lines()));
    assert_eq!(snapshot.end(), first_visible.offset(rows));

    let mut lines = Vec::with_capacity(snapshot.line_count());
    snapshot.lines(first..snapshot.end(), &mut lines);
    assert_eq!(
        lines.len(),
        snapshot.line_count(),
        "lines() holds every line"
    );
    for (i, line) in lines.iter().enumerate() {
        let id = first.offset(i);
        assert_eq!(line.id, id, "line ids are consecutive");
        assert_eq!(
            line.complete,
            id < first_visible,
            "scrollback rows are complete and screen rows are not: {line:?}"
        );
        assert_eq!(line.direction, Direction::Rx);
        assert!(
            line.received_at == setup.at || line.received_at == snapshot.epoch().instant,
            "a row stamped with neither the chunk's time nor the epoch's: {line:?}"
        );
        assert!(
            line.raw.start == line.raw.end && line.raw.end <= fed,
            "raw offsets {:?} with {fed} bytes fed",
            line.raw
        );
        let wide = if id < first_visible {
            dims.widest
        } else {
            columns
        };
        check_line(line, wide);
    }

    // `line` and `visible_line` against `lines`: the ends, and every screen row.
    for id in [first, first_visible, snapshot.end()] {
        let index = usize::try_from(id.0 - first.0).expect("an index");
        assert_eq!(
            snapshot.line(id).as_ref(),
            lines.get(index),
            "line({id:?}) against lines()"
        );
    }
    if let Some(last) = lines.last() {
        assert_eq!(snapshot.line(last.id).as_ref(), Some(last), "the last line");
    }
    if first.0 > 0 {
        assert!(snapshot.line(LineId(first.0 - 1)).is_none(), "evicted row");
    }
    assert!(snapshot.visible_line(rows).is_none());
    let screen_text = snapshot.screen_text();
    assert_eq!(screen_text.len(), rows);
    for (row, text) in screen_text.iter().enumerate() {
        let shown = snapshot.visible_line(row).expect("a screen row");
        assert_eq!(shown.id, first_visible.offset(row));
        assert_eq!(shown.text, *text);
        assert_eq!(*text, lines[snapshot.scrollback_lines() + row].text);
    }

    let cursor = snapshot.cursor().expect("a terminal screen has a cursor");
    assert!(
        cursor.line >= first_visible && cursor.line < snapshot.end(),
        "cursor {cursor:?} off the screen rows {first_visible:?}..{:?}",
        snapshot.end()
    );
    assert!(
        cursor.column < columns,
        "cursor {cursor:?} past {columns} columns"
    );
    Seen { snapshot, lines }
}

/// `lines()` with a range wider than the snapshot gives the retained lines.
fn check_clipping(snapshot: &VtSnapshot, lines: &[StyledLine]) {
    let mut clipped = Vec::new();
    snapshot.lines(LineId(0)..LineId(u64::MAX), &mut clipped);
    assert_eq!(
        clipped, lines,
        "lines() clips its range to what is retained"
    );
}

fn check_line(line: &StyledLine, columns: usize) {
    assert_eq!(line.runs.is_empty(), line.text.is_empty(), "{line:?}");
    let mut end = 0;
    for run in &line.runs {
        assert!(run.len > 0, "empty run: {line:?}");
        end += run.len;
        assert!(
            line.text.is_char_boundary(end),
            "run ends inside a character: {line:?}"
        );
    }
    assert_eq!(end, line.text.len(), "runs do not cover the text: {line:?}");
    for pair in line.runs.windows(2) {
        assert_ne!(pair[0].style, pair[1].style, "runs not coalesced: {line:?}");
    }
    assert!(
        !line.text.chars().any(char::is_control),
        "control character in the text: {line:?}"
    );
    // One character per cell, and a wide character fills two cells but is one character.
    // Combining marks take no cell, and every zero-width character is at or above U+0300.
    let cells = line.text.chars().filter(|&c| c < '\u{300}').count();
    assert!(
        cells <= columns,
        "{cells} cells of text in {columns} columns: {line:?}"
    );
    // A cell keeps at most MAX_ZERO_WIDTH zero-width characters after its own.
    let chars = line.text.chars().count();
    let most = columns * (1 + MAX_ZERO_WIDTH);
    assert!(
        chars <= most,
        "{chars} characters in {columns} columns, more than {most}: {line:?}"
    );
}

/// What `current` must hold given `previous`, an earlier snapshot of the same screen.
fn check_step(previous: &Seen, current: &Seen) {
    let (before, now) = (&previous.snapshot, &current.snapshot);
    assert!(
        now.first_line() >= before.first_line(),
        "first_line went back from {:?} to {:?}",
        before.first_line(),
        now.first_line()
    );
    assert!(
        now.first_visible() >= before.first_visible(),
        "first_visible went back from {:?} to {:?}",
        before.first_visible(),
        now.first_visible()
    );

    // A snapshot whose content differs has the next generation, and equal generations mean
    // equal content. The reverse is not promised: the scrollback is compared by identity,
    // so erasing an empty one (`CSI 3 J`) takes a new generation for the same content.
    let unchanged = previous.first_difference(current).is_none();
    let (then, next) = (before.generation(), now.generation());
    assert!(
        next == then || next == then + 1,
        "generation {then} then {next}"
    );
    assert!(
        unchanged || next == then + 1,
        "generation {then} then {next}, with the content changed"
    );

    let changed = now.changed_since(before);
    assert!(changed.start <= changed.end, "inverted range {changed:?}");
    let mut id = now.first_line().max(before.first_line());
    while id < now.end() {
        let line = current.line(id).expect("a retained line");
        let earlier = previous.line(id);
        if id < before.first_visible() {
            let earlier = earlier.expect("a scrollback row of the earlier snapshot");
            assert!(
                (&earlier.text, &earlier.runs) == (&line.text, &line.runs),
                "scrollback row {id:?} changed: {earlier:?} then {line:?}"
            );
        } else if earlier.is_none_or(|e| (&e.text, &e.runs) != (&line.text, &line.runs)) {
            assert!(
                changed.contains(&id),
                "row {id:?} changed ({line:?}) outside changed_since's {changed:?}"
            );
        }
        id = id.next();
    }
}

fn check_events(events: &[VtEvent], title: Option<&str>) {
    assert!(
        events.len() <= MAX_EVENTS,
        "{} events in one drain",
        events.len()
    );
    for pair in events.windows(2) {
        match (&pair[0], &pair[1]) {
            (VtEvent::Bell, VtEvent::Bell) => panic!("two bells in a row"),
            (VtEvent::ResetTitle, VtEvent::ResetTitle) => panic!("two title resets in a row"),
            (VtEvent::Title(a), VtEvent::Title(b)) if a == b => {
                panic!("the title {a:?} set twice in a row")
            }
            _ => {}
        }
    }
    if events.len() < MAX_EVENTS {
        // Nothing was dropped, so the events say what the title is.
        let last = events.iter().rev().find_map(|event| match event {
            VtEvent::Title(title) => Some(Some(title.as_str())),
            VtEvent::ResetTitle => Some(None),
            _ => None,
        });
        assert_eq!(
            title,
            last.flatten(),
            "the title against the events that set it"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_replay() {
        crate::replay_seeds("vt_screen", super::run);
    }
}
