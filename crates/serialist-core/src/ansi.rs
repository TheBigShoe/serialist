//! The monitor-mode ANSI parser: bytes in, styled lines out.
//!
//! A [`vte::Parser`] does the tokenising; [`AnsiParser`] gives the tokens serial-monitor
//! semantics on a single line:
//!
//! - LF ends a line. A CR moves the cursor to column 0 so later text overwrites (progress
//!   bars); a CR directly before the LF therefore changes nothing and CRLF is a plain
//!   line ending. BS moves back one column and TAB advances to the next 8-column stop;
//!   neither erases anything.
//! - SGR (`CSI … m`) sets bold, dim, italic, underline, inverse, hidden, strikethrough and
//!   reset, the 16 colors, bright colors, 256-color and truecolor, in both the `;` and the
//!   `:` forms. EL (`CSI K`) erases per its parameter; CUB, CUF and CHA (`CSI D`, `C`, `G`)
//!   move the cursor within the line. Every other CSI, and every ESC, OSC, DCS, SOS, PM and
//!   APC sequence, is consumed and ignored.
//! - Invalid UTF-8 becomes U+FFFD. vte reports stray bytes 0x80..=0x9F (and UTF-8 encoded
//!   C1 controls, which it cannot tell apart from them) as C1 controls; they also become
//!   U+FFFD, because a monitor must not silently hide bytes it received. DEL and the C0
//!   controls not listed above are dropped.
//! - Each character takes one column; wide and combining characters are not measured.
//! - Forward cursor motion (TAB, CUF, CHA) stops at column [`MOTION_WIDTH`] or the end
//!   of the text, whichever is further, like a terminal that wide. A width probe such as
//!   `ESC[999C` therefore pads at most 512 columns. Printing never wraps and is never
//!   dropped: a line holds at most `max_line_bytes + MOTION_WIDTH` columns, because every
//!   printed character consumes at least one raw byte of the line.
//! - A line keeps at most [`MAX_RUNS`] style runs; past that, new text takes the style
//!   of the last run. The text itself is never affected.
//!
//! # Showing control characters
//!
//! [`AnsiParser::show_control_chars`] (off by default) makes the bytes above that leave
//! no trace visible as placeholder glyphs in the line's text, each in a run of its own
//! whose style is `DIM | CONTROL` in the default colors (see [`CONTROL_STYLE`]). A C0
//! byte `b` shows as the Unicode Control Picture U+2400 + `b`; DEL is U+2421.
//!
//! | Input | Glyph |
//! | --- | --- |
//! | CR | `␍` |
//! | the LF that ends a line | `␊`, at the end of the line |
//! | each escape sequence the parser consumed (CSI, OSC, DCS, a bare ESC pair) | `␛` |
//! | BS | `␈` |
//! | any other C0 byte except TAB (NUL, BEL, VT, FF, ...) | U+2400 + the byte (`␀`, `␇`, `␋`, `␌`) |
//! | DEL | `␡` |
//!
//! An OSC or DCS string ended by ST shows two: one for the string, one for the `ESC \`.
//! A string that a line feed cuts off shows one, before the line's `␊`. SOS, PM and
//! APC strings and a CSI sequence with an invalid parameter are dropped by the tokenizer
//! without a callback, so they show nothing unless a line feed cuts them off. TAB keeps
//! expanding to spaces and shows no glyph; the bytes 0x80..=0x9F already show U+FFFD.
//!
//! The glyphs are visual only. They are not columns: the cursor, CR overwrites, BS, tabs
//! and erases behave exactly as with the flag off, and the text without its `CONTROL` runs
//! is the text the flag-off parser gives (the runs coalesce the same way). A glyph is
//! anchored to the column the cursor was in when its byte arrived, and sits before the
//! character in that column, so the placeholder for a CR goes at the end of the text it
//! is about to overwrite, and `abcdef\rXY\n` shows `XYcdef␍␊`. Glyphs at the
//! same column keep their order; a column past the end of the text puts the glyph at the
//! end. A line shows at most [`MAX_CONTROL_GLYPHS`] of them.
//!
//! A line with a glyph is never plain: the store keeps its decoded text, so a session
//! with the flag on costs about what an ANSI-heavy one does (see the store's memory
//! model). Searches see the glyphs as part of the text.
//!
//! Appending at the end of the line (almost all traffic) edits the text and runs
//! directly. The first edit that is not an append (an overwrite after CR or BS, an
//! erase) switches the line to one cell per column, so every later edit is O(1) and the
//! text is rebuilt once per chunk instead of shifted once per character.
//!
//! The parser is stateful across calls: an escape sequence or UTF-8 character split
//! between two [`AnsiParser::feed`] calls parses exactly as if it arrived in one. Two
//! rules keep a damaged stream from swallowing the log, and both depend only on the
//! bytes, never on how they were chunked:
//!
//! - An LF that vte swallows (inside an unterminated OSC or DCS string, say) still ends
//!   the line, and the escape parser is reset to its ground state.
//! - A line that reaches `max_line_bytes` raw bytes without an LF is ended there, marked
//!   incomplete, and the escape parser is reset. A sequence straddling that boundary is
//!   lost, which bounds both line length and the escape parser's buffers.

use std::fmt;

use vte::{Params, ParamsIter, Perform};

use crate::text::{Color, Style, StyleFlags, StyleRun};

/// Columns between tab stops.
pub const TAB_WIDTH: usize = 8;
/// A line with this many raw bytes and no LF is ended early.
pub const DEFAULT_MAX_LINE_BYTES: usize = 64 * 1024;
/// The largest `max_line_bytes` a parser accepts. The store relies on this bound.
pub const MAX_LINE_BYTES_LIMIT: usize = 1 << 20;
/// The smallest `max_line_bytes` a parser accepts.
pub const MIN_LINE_BYTES: usize = 16;
/// Forward cursor motion stops at this column or the end of the text, whichever is
/// further.
pub const MOTION_WIDTH: usize = 512;
/// Style runs kept per line; past this, new text takes the style of the last run.
pub const MAX_RUNS: usize = 2048;
/// Control-character glyphs a line shows when [`AnsiParser::show_control_chars`] is on;
/// past this, further control bytes show nothing.
pub const MAX_CONTROL_GLYPHS: usize = 4096;
/// The style of a control-character glyph run: dim, flagged `CONTROL`, default colors.
pub const CONTROL_STYLE: Style = Style {
    fg: Color::Default,
    bg: Color::Default,
    flags: StyleFlags(StyleFlags::DIM.0 | StyleFlags::CONTROL.0),
};
/// Distinct styles a line in cell mode can hold (11 bits of a packed cell); past this,
/// new styles reuse the last one.
const MAX_PALETTE: usize = 1 << 11;
/// Buffer capacities kept between lines; anything larger is released when a line ends.
const KEEP_TEXT: usize = 16 * 1024;
const KEEP_RUNS: usize = 256;
const KEEP_CELLS: usize = 4 * 1024;
const KEEP_PALETTE: usize = 64;
const KEEP_MARKS: usize = 256;
const KEEP_JOINED: usize = 64 * 1024;

/// A line as the parser produced it. Borrowed from the parser; copy what you keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParsedLine<'a> {
    pub text: &'a str,
    /// Coalesced runs covering `text` exactly. Empty when `text` is empty.
    pub runs: &'a [StyleRun],
    /// Raw bytes this line consumed so far, its LF included.
    pub raw_len: usize,
    /// `true` when an LF ended the line; `false` for the line in progress or a line
    /// ended early (by `max_line_bytes` or [`AnsiParser::break_line`]).
    pub complete: bool,
    /// A hint that nothing was transformed: every printed character went to the end of
    /// the line in the default style and no sequence or control other than CR and LF was
    /// seen. The store confirms it against the raw bytes before borrowing them as text.
    pub simple: bool,
}

/// An owned copy of a [`ParsedLine`], for tests and callers that keep lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedLine {
    pub text: String,
    pub runs: Vec<StyleRun>,
    pub raw_len: usize,
    pub complete: bool,
}

impl From<ParsedLine<'_>> for OwnedLine {
    fn from(line: ParsedLine<'_>) -> Self {
        Self {
            text: line.text.to_owned(),
            runs: line.runs.to_vec(),
            raw_len: line.raw_len,
            complete: line.complete,
        }
    }
}

/// The stateful monitor-mode parser. See the module docs for the semantics.
pub struct AnsiParser {
    vte: vte::Parser,
    line: LineState,
    max_line_bytes: usize,
    /// An incomplete UTF-8 sequence at the end of the last chunk, held back so vte
    /// never sees a character split across two `advance` calls (see [`AnsiParser::feed`]).
    held: [u8; 4],
    held_len: usize,
    /// Reused to join held bytes to the next chunk.
    joined: Vec<u8>,
}

impl Default for AnsiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for AnsiParser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnsiParser")
            .field("line", &self.line.text)
            .field("raw_len", &self.line.raw_len)
            .field("pen", &self.line.pen)
            .field("max_line_bytes", &self.max_line_bytes)
            .finish()
    }
}

impl AnsiParser {
    pub fn new() -> Self {
        Self::with_max_line_bytes(DEFAULT_MAX_LINE_BYTES)
    }

    /// A parser that ends a line early once it has `max_line_bytes` raw bytes, clamped
    /// to [`MIN_LINE_BYTES`]`..=`[`MAX_LINE_BYTES_LIMIT`].
    pub fn with_max_line_bytes(max_line_bytes: usize) -> Self {
        let max_line_bytes = max_line_bytes.clamp(MIN_LINE_BYTES, MAX_LINE_BYTES_LIMIT);
        Self {
            vte: vte::Parser::new(),
            line: LineState::new(),
            max_line_bytes,
            held: [0; 4],
            held_len: 0,
            joined: Vec::new(),
        }
    }

    /// Show control characters as dim placeholder glyphs (default off). See the module
    /// docs for the glyphs and what they do not change. Set it before the first `feed`:
    /// the lines already parsed, and the one in progress, are not redone.
    pub fn show_control_chars(mut self, show: bool) -> Self {
        self.line.show_control = show;
        self
    }

    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    /// The style new text is drawn in.
    pub fn pen(&self) -> Style {
        self.line.pen
    }

    /// Parse `bytes`, calling `on_line` for every line that ends in them, in order.
    ///
    /// A UTF-8 sequence cut off at the end of `bytes` is held back and parsed with the
    /// next call. vte 0.15.0 buffers such a sequence itself, but when the next call
    /// completes it and the following bytes hold another character and then an
    /// incomplete one, `advance_partial_utf8` skips a byte; holding the sequence back
    /// keeps the parse independent of chunking. Held bytes count towards the line in
    /// progress.
    pub fn feed(&mut self, bytes: &[u8], mut on_line: impl FnMut(ParsedLine<'_>)) {
        if bytes.is_empty() {
            return;
        }
        if self.held_len == 0 {
            let keep = incomplete_utf8_suffix(bytes);
            self.feed_whole(&bytes[..bytes.len() - keep], &mut on_line);
            self.hold(&bytes[bytes.len() - keep..], &mut on_line);
        } else {
            let mut joined = std::mem::take(&mut self.joined);
            joined.clear();
            joined.extend_from_slice(&self.held[..self.held_len]);
            joined.extend_from_slice(bytes);
            // Already counted when they were held.
            self.line.raw_len -= self.held_len;
            self.held_len = 0;
            let keep = incomplete_utf8_suffix(&joined);
            self.feed_whole(&joined[..joined.len() - keep], &mut on_line);
            self.hold(&joined[joined.len() - keep..], &mut on_line);
            if joined.capacity() <= KEEP_JOINED {
                self.joined = joined;
            }
        }
        // `current()` borrows the line; bring its text up to date with its cells.
        self.line.sync();
    }

    /// Hold back `tail`, unless that would reach the line limit: then the limit breaks
    /// the line inside it exactly as it would in one chunk, so feed it now.
    fn hold(&mut self, tail: &[u8], on_line: &mut impl FnMut(ParsedLine<'_>)) {
        if tail.is_empty() {
            return;
        }
        if self.line.raw_len + tail.len() >= self.max_line_bytes {
            self.feed_whole(tail, on_line);
            return;
        }
        self.held[..tail.len()].copy_from_slice(tail);
        self.held_len = tail.len();
        self.line.raw_len += tail.len();
    }

    fn feed_whole(&mut self, bytes: &[u8], on_line: &mut impl FnMut(ParsedLine<'_>)) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let room = self.max_line_bytes - self.line.raw_len;
            let window = &rest[..rest.len().min(room)];
            match memchr::memchr(b'\n', window) {
                Some(i) => {
                    if i > 0 {
                        self.vte.advance(&mut self.line, &window[..i]);
                    }
                    self.line.lf_executed = false;
                    self.vte.advance(&mut self.line, b"\n");
                    if !self.line.lf_executed {
                        // Swallowed by a string sequence: the LF still ends the line.
                        self.vte = vte::Parser::new();
                        self.line.mark(ESC_GLYPH);
                    }
                    self.line.mark_end(LF_GLYPH);
                    self.line.raw_len += i + 1;
                    self.line.sync();
                    on_line(self.line.view(true));
                    self.line.reset();
                    rest = &rest[i + 1..];
                }
                None => {
                    self.vte.advance(&mut self.line, window);
                    self.line.raw_len += window.len();
                    if self.line.raw_len >= self.max_line_bytes {
                        self.vte = vte::Parser::new();
                        self.line.sync();
                        on_line(self.line.view(false));
                        self.line.reset();
                    }
                    rest = &rest[window.len()..];
                }
            }
        }
    }

    /// The line in progress, if any raw byte has arrived for it.
    pub fn current(&self) -> Option<ParsedLine<'_>> {
        (self.line.raw_len > 0).then(|| self.line.view(false))
    }

    /// End the line in progress (marked incomplete) so that what follows starts a new
    /// line. The escape state and pen carry over. Used when a local line is inserted.
    /// A UTF-8 sequence held back at the end of the last chunk is dropped: its bytes stay
    /// in the ended line's raw range, and a continuation arriving later shows as U+FFFD.
    pub fn break_line(&mut self, on_line: impl FnOnce(ParsedLine<'_>)) {
        self.held_len = 0;
        if self.line.raw_len > 0 {
            self.line.sync();
            on_line(self.line.view(false));
            self.line.reset();
        }
    }

    /// Bytes of heap this parser holds, for memory accounting.
    pub fn heap_bytes(&self) -> usize {
        let line = &self.line;
        self.joined.capacity()
            + line.text.capacity()
            + line.runs.capacity() * size_of::<StyleRun>()
            + line.cells.capacity() * size_of::<u32>()
            + line.palette.capacity() * size_of::<Style>()
            + line.marks.capacity() * size_of::<Mark>()
            + line.shown_text.capacity()
            + line.shown_runs.capacity() * size_of::<StyleRun>()
    }

    /// The most heap a parser with this `max_line_bytes` holds after a `feed`, whatever
    /// the input: text, runs and cells for the widest possible line, all at twice their
    /// length for vector growth. The store's minimum budget is built on it.
    pub fn worst_case_heap(max_line_bytes: usize) -> usize {
        let cols = max_line_bytes.clamp(MIN_LINE_BYTES, MAX_LINE_BYTES_LIMIT) + MOTION_WIDTH;
        2 * (4 * cols
            + MAX_RUNS * size_of::<StyleRun>()
            + cols * size_of::<u32>()
            + MAX_PALETTE * size_of::<Style>())
            + KEEP_JOINED
    }

    /// The most heap the control-glyph buffers add to [`AnsiParser::worst_case_heap`]
    /// after a `feed` with this `max_line_bytes`, when [`AnsiParser::show_control_chars`]
    /// is on: the glyph anchors, and the text and runs with the glyphs merged in, at
    /// twice their length for vector growth.
    pub fn worst_case_control_heap(max_line_bytes: usize) -> usize {
        let cols = max_line_bytes.clamp(MIN_LINE_BYTES, MAX_LINE_BYTES_LIMIT) + MOTION_WIDTH;
        2 * (MAX_CONTROL_GLYPHS * size_of::<Mark>()
            + 4 * cols
            + 3 * MAX_CONTROL_GLYPHS
            + MAX_RUNS * size_of::<StyleRun>())
    }

    /// Parse a whole buffer and return every line, the one in progress last. Convenient
    /// for tests and one-off conversions.
    pub fn parse_all(bytes: &[u8]) -> Vec<OwnedLine> {
        let mut parser = Self::new();
        let mut lines = Vec::new();
        parser.feed(bytes, |line| lines.push(line.into()));
        if let Some(line) = parser.current() {
            lines.push(line.into());
        }
        lines
    }
}

/// The line being built plus the cursor and pen: the `vte::Perform` side of the parser.
///
/// In append mode `text` and `runs` are the line. In cell mode (entered by the first
/// edit that is not an append) `cells` are, one per column, and `sync` rebuilds `text`
/// and `runs` from them.
struct LineState {
    text: String,
    runs: Vec<StyleRun>,
    /// Cell mode: a character (low 21 bits) and a `palette` index (high 11) per column.
    cells: Vec<u32>,
    palette: Vec<Style>,
    /// The pen's `palette` index, found on first use after the pen or palette changes.
    pen_index: Option<u32>,
    cell_mode: bool,
    /// `text` and `runs` lag behind `cells`.
    stale: bool,
    /// Width of the line in columns: characters in `text`, or cells.
    cols: usize,
    cursor: usize,
    pen: Style,
    simple: bool,
    raw_len: usize,
    lf_executed: bool,
    /// Control glyphs are on (see [`AnsiParser::show_control_chars`]).
    show_control: bool,
    /// The glyphs of this line, anchored to columns; sorted (stably) when composed.
    marks: Vec<Mark>,
    /// With marks: `text` and `runs` with the glyphs merged in, which is what a view
    /// of the line borrows.
    shown_text: String,
    shown_runs: Vec<StyleRun>,
    /// `text`, `runs` or `marks` changed since `shown_*` was composed.
    shown_stale: bool,
}

const CR_GLYPH: char = '\u{240D}';
const LF_GLYPH: char = '\u{240A}';
const ESC_GLYPH: char = '\u{241B}';
const BS_GLYPH: char = '\u{2408}';
const DEL_GLYPH: char = '\u{2421}';

/// A control glyph anchored before column `col` (see the module docs).
#[derive(Clone, Copy, Debug)]
struct Mark {
    col: u32,
    glyph: char,
}

/// The Control Picture standing for the C0 byte `byte`.
fn control_picture(byte: u8) -> char {
    debug_assert!(byte < 0x20);
    char::from_u32(0x2400 + u32::from(byte)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

fn pack(c: char, index: u32) -> u32 {
    c as u32 | (index << 21)
}

fn unpack(cell: u32) -> (char, usize) {
    let c = char::from_u32(cell & 0x1F_FFFF).unwrap_or(char::REPLACEMENT_CHARACTER);
    (c, (cell >> 21) as usize)
}

/// A blank cell: a space in the default style (palette index 0).
const BLANK: u32 = b' ' as u32;

/// `style`'s index in `palette`, added if new; a full palette reuses its last entry.
fn palette_index(palette: &mut Vec<Style>, style: Style) -> u32 {
    if let Some(i) = palette.iter().position(|s| *s == style) {
        return i as u32;
    }
    if palette.len() < MAX_PALETTE {
        palette.push(style);
    }
    (palette.len() - 1) as u32
}

impl LineState {
    fn new() -> Self {
        Self {
            text: String::new(),
            runs: Vec::new(),
            cells: Vec::new(),
            palette: Vec::new(),
            pen_index: None,
            cell_mode: false,
            stale: false,
            cols: 0,
            cursor: 0,
            pen: Style::default(),
            simple: true,
            raw_len: 0,
            lf_executed: false,
            show_control: false,
            marks: Vec::new(),
            shown_text: String::new(),
            shown_runs: Vec::new(),
            shown_stale: false,
        }
    }

    fn view(&self, complete: bool) -> ParsedLine<'_> {
        debug_assert!(!self.stale, "view of an unsynced line");
        debug_assert!(
            !self.shown_stale || self.marks.is_empty(),
            "view of an unsynced line"
        );
        let (text, runs) = if self.marks.is_empty() {
            (&self.text, &self.runs)
        } else {
            (&self.shown_text, &self.shown_runs)
        };
        ParsedLine {
            text,
            runs,
            raw_len: self.raw_len,
            complete,
            simple: self.simple,
        }
    }

    /// Start a new line. The pen carries over; everything else starts fresh, and buffers
    /// a long line grew are released so one pathological line cannot pin memory.
    fn reset(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.cells.clear();
        self.palette.clear();
        self.pen_index = None;
        self.cell_mode = false;
        self.stale = false;
        self.cols = 0;
        self.cursor = 0;
        self.simple = true;
        self.raw_len = 0;
        self.marks.clear();
        self.shown_text.clear();
        self.shown_runs.clear();
        self.shown_stale = false;
        if self.marks.capacity() > KEEP_MARKS {
            self.marks.shrink_to(KEEP_MARKS);
        }
        if self.shown_text.capacity() > KEEP_TEXT {
            self.shown_text.shrink_to(KEEP_TEXT);
        }
        if self.shown_runs.capacity() > KEEP_RUNS {
            self.shown_runs.shrink_to(KEEP_RUNS);
        }
        if self.text.capacity() > KEEP_TEXT {
            self.text.shrink_to(KEEP_TEXT);
        }
        if self.runs.capacity() > KEEP_RUNS {
            self.runs.shrink_to(KEEP_RUNS);
        }
        if self.cells.capacity() > KEEP_CELLS {
            self.cells.shrink_to(KEEP_CELLS);
        }
        if self.palette.capacity() > KEEP_PALETTE {
            self.palette.shrink_to(KEEP_PALETTE);
        }
    }

    /// Erase the whole line; the cursor stays where it is.
    fn clear_text(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.cells.clear();
        self.cols = 0;
        self.stale = false;
        self.shown_stale = true;
    }

    /// Switch to one cell per column, built from the current text and runs.
    fn enter_cells(&mut self) {
        if self.cell_mode {
            return;
        }
        let Self {
            text,
            runs,
            cells,
            palette,
            ..
        } = self;
        cells.clear();
        palette.clear();
        palette.push(Style::default());
        let mut chars = text.chars();
        for run in runs.iter() {
            let index = palette_index(palette, run.style);
            let mut used = 0;
            while used < run.len {
                let Some(c) = chars.next() else {
                    break;
                };
                cells.push(pack(c, index));
                used += c.len_utf8();
            }
        }
        self.pen_index = None;
        self.cell_mode = true;
        self.stale = false;
    }

    /// Rebuild `text` and `runs` from the cells, if they changed, and the text and runs
    /// with the control glyphs in them, if either changed.
    fn sync(&mut self) {
        if self.stale {
            let Self {
                text,
                runs,
                cells,
                palette,
                ..
            } = self;
            text.clear();
            runs.clear();
            for &cell in cells.iter() {
                let (c, index) = unpack(cell);
                text.push(c);
                push_run(runs, c.len_utf8(), palette[index]);
            }
            self.stale = false;
            self.shown_stale = true;
        }
        if self.shown_stale {
            if !self.marks.is_empty() {
                self.compose();
            }
            self.shown_stale = false;
        }
    }

    /// Show `glyph` before the column the cursor is in (see the module docs), if control
    /// characters are shown. The line is no longer plain.
    fn mark(&mut self, glyph: char) {
        self.mark_at(self.cursor, glyph);
    }

    /// Show `glyph` at the end of the line, wherever the text ends up.
    fn mark_end(&mut self, glyph: char) {
        self.mark_at(usize::MAX, glyph);
    }

    fn mark_at(&mut self, col: usize, glyph: char) {
        if !self.show_control {
            return;
        }
        self.simple = false;
        if self.marks.len() < MAX_CONTROL_GLYPHS {
            let col = u32::try_from(col).unwrap_or(u32::MAX);
            self.marks.push(Mark { col, glyph });
            self.shown_stale = true;
        }
    }

    /// Merge the marks into `text` and `runs`, giving `shown_text` and `shown_runs`. A
    /// mark goes before the character in its column, after the marks anchored earlier
    /// in the same column, and at the end if the text is shorter than its column.
    fn compose(&mut self) {
        // Marks arrive in time order, columns in any order; equal columns keep theirs.
        if !self.marks.is_sorted_by_key(|mark| mark.col) {
            self.marks.sort_by_key(|mark| mark.col);
        }
        let Self {
            text,
            runs,
            marks,
            shown_text,
            shown_runs,
            ..
        } = self;
        shown_text.clear();
        shown_runs.clear();
        let mut put_text = |part: &str, style: Style| {
            if !part.is_empty() {
                shown_text.push_str(part);
                push_run(shown_runs, part.len(), style);
            }
        };
        let mut next = 0;
        let mut col = 0;
        let mut at = 0;
        for run in runs.iter() {
            let end = (at + run.len).min(text.len());
            let mut from = at;
            for (offset, _) in text[at..end].char_indices() {
                if marks.get(next).is_some_and(|mark| mark.col as usize <= col) {
                    put_text(&text[from..at + offset], run.style);
                    from = at + offset;
                    while let Some(mark) = marks.get(next).filter(|m| m.col as usize <= col) {
                        put_text(mark.glyph.encode_utf8(&mut [0; 4]), CONTROL_STYLE);
                        next += 1;
                    }
                }
                col += 1;
            }
            put_text(&text[from..end], run.style);
            at = end;
        }
        for mark in &marks[next..] {
            put_text(mark.glyph.encode_utf8(&mut [0; 4]), CONTROL_STYLE);
        }
    }

    fn append(&mut self, c: char, style: Style) {
        self.text.push(c);
        push_run(&mut self.runs, c.len_utf8(), style);
        self.cols += 1;
        self.shown_stale = true;
    }

    /// Draw `c` at the cursor and advance it.
    fn draw(&mut self, c: char) {
        // Forward motion stops at MOTION_WIDTH or the text's end, and every column past
        // MOTION_WIDTH was drawn by a character that consumed a raw byte of this line
        // (an erase keeps the cursor where such characters left it). So the cursor stays
        // within max_line_bytes + MOTION_WIDTH columns and nothing is ever dropped.
        if !self.cell_mode && self.cursor < self.cols {
            self.enter_cells();
        }
        if self.cell_mode {
            let index = match self.pen_index {
                Some(index) => index,
                None => {
                    let index = palette_index(&mut self.palette, self.pen);
                    self.pen_index = Some(index);
                    index
                }
            };
            if self.cursor < self.cells.len() {
                self.cells[self.cursor] = pack(c, index);
            } else {
                self.cells.resize(self.cursor, BLANK);
                self.cells.push(pack(c, index));
            }
            self.cols = self.cells.len();
            self.stale = true;
        } else {
            for _ in self.cols..self.cursor {
                self.append(' ', Style::default());
            }
            self.append(c, self.pen);
        }
        self.cursor += 1;
    }

    fn erase_in_line(&mut self, mode: u16) {
        match mode {
            0 if self.cursor == 0 => self.clear_text(),
            0 if self.cursor < self.cols => {
                self.enter_cells();
                self.cells.truncate(self.cursor);
                self.cols = self.cursor;
                self.stale = true;
            }
            1 => {
                let n = (self.cursor + 1).min(self.cols);
                if n >= self.cols {
                    self.clear_text();
                } else if n > 0 {
                    self.enter_cells();
                    self.cells[..n].fill(BLANK);
                    self.stale = true;
                }
            }
            2 => self.clear_text(),
            _ => {}
        }
    }

    /// Move the cursor to `col`. Backward motion is free; forward motion stops at
    /// [`MOTION_WIDTH`] or the end of the text, whichever is further, and never moves
    /// the cursor back.
    fn move_to(&mut self, col: usize) {
        self.cursor = if col <= self.cursor {
            col
        } else {
            col.min(self.cols.max(MOTION_WIDTH)).max(self.cursor)
        };
    }

    fn sgr(&mut self, params: &Params) {
        self.pen_index = None;
        let mut it = params.iter();
        while let Some(param) = it.next() {
            let pen = &mut self.pen;
            match param[0] {
                0 => *pen = Style::default(),
                1 => pen.flags.insert(StyleFlags::BOLD),
                2 => pen.flags.insert(StyleFlags::DIM),
                3 => pen.flags.insert(StyleFlags::ITALIC),
                4 if param.get(1) == Some(&0) => pen.flags.remove(StyleFlags::UNDERLINE),
                4 | 21 => pen.flags.insert(StyleFlags::UNDERLINE),
                7 => pen.flags.insert(StyleFlags::INVERSE),
                8 => pen.flags.insert(StyleFlags::HIDDEN),
                9 => pen.flags.insert(StyleFlags::STRIKETHROUGH),
                22 => {
                    pen.flags.remove(StyleFlags::BOLD);
                    pen.flags.remove(StyleFlags::DIM);
                }
                23 => pen.flags.remove(StyleFlags::ITALIC),
                24 => pen.flags.remove(StyleFlags::UNDERLINE),
                27 => pen.flags.remove(StyleFlags::INVERSE),
                28 => pen.flags.remove(StyleFlags::HIDDEN),
                29 => pen.flags.remove(StyleFlags::STRIKETHROUGH),
                n @ 30..=37 => pen.fg = Color::Ansi((n - 30) as u8),
                38 => {
                    if let Some(color) = extended_color(param, &mut it) {
                        pen.fg = color;
                    }
                }
                39 => pen.fg = Color::Default,
                n @ 40..=47 => pen.bg = Color::Ansi((n - 40) as u8),
                48 => {
                    if let Some(color) = extended_color(param, &mut it) {
                        pen.bg = color;
                    }
                }
                49 => pen.bg = Color::Default,
                // Underline color: parsed so its arguments are not read as SGR codes.
                58 => {
                    let _ = extended_color(param, &mut it);
                }
                n @ 90..=97 => pen.fg = Color::Ansi((n - 90 + 8) as u8),
                n @ 100..=107 => pen.bg = Color::Ansi((n - 100 + 8) as u8),
                _ => {}
            }
        }
    }
}

/// The color after a `38`, `48` or `58`: `5;n` / `:5:n` for 256 colors, `2;r;g;b`,
/// `:2:r:g:b` or `:2:cs:r:g:b` for truecolor. Out-of-range values yield `None`.
fn extended_color(param: &[u16], it: &mut ParamsIter<'_>) -> Option<Color> {
    let byte = |v: u16| u8::try_from(v).ok();
    if param.len() > 1 {
        return match param[1] {
            5 => param.get(2).copied().and_then(byte).map(indexed),
            2 => {
                let rgb = &param[2..];
                let rgb = if rgb.len() >= 4 { &rgb[1..4] } else { rgb };
                match rgb {
                    [r, g, b, ..] => Some(Color::Rgb(byte(*r)?, byte(*g)?, byte(*b)?)),
                    _ => None,
                }
            }
            _ => None,
        };
    }
    match it.next()?[0] {
        5 => byte(it.next()?[0]).map(indexed),
        2 => {
            let r = it.next()?[0];
            let g = it.next()?[0];
            let b = it.next()?[0];
            Some(Color::Rgb(byte(r)?, byte(g)?, byte(b)?))
        }
        _ => None,
    }
}

fn indexed(n: u8) -> Color {
    if n < 16 {
        Color::Ansi(n)
    } else {
        Color::Indexed(n)
    }
}

/// Length of an incomplete UTF-8 sequence at the end of `bytes` (0 if there is none):
/// a lead byte followed by fewer continuation bytes than it announces, so at most 3.
///
/// Public for `serialist-vt`, which feeds vte through Alacritty and holds the same suffix
/// back for the same reason as [`AnsiParser::feed`] does.
pub fn incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    let n = bytes.len();
    for back in 1..=n.min(3) {
        let b = bytes[n - back];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    0
}

/// Append a run, merging it into the last one when the style matches or the line
/// already has [`MAX_RUNS`] runs.
fn push_run(runs: &mut Vec<StyleRun>, len: usize, style: Style) {
    if len == 0 {
        return;
    }
    let full = runs.len() >= MAX_RUNS;
    match runs.last_mut() {
        Some(last) if full || last.style == style => last.len += len,
        _ => runs.push(StyleRun { len, style }),
    }
}

fn first_param(params: &Params, default: u16) -> u16 {
    params.iter().next().map_or(default, |p| p[0])
}

impl Perform for LineState {
    fn print(&mut self, c: char) {
        if c == '\u{7f}' {
            self.simple = false;
            self.mark(DEL_GLYPH);
            return;
        }
        if c == char::REPLACEMENT_CHARACTER
            || self.cursor != self.cols
            || self.pen != Style::default()
        {
            self.simple = false;
        }
        self.draw(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.lf_executed = true,
            b'\r' => {
                self.mark(CR_GLYPH);
                self.cursor = 0;
            }
            0x08 => {
                self.simple = false;
                self.mark(BS_GLYPH);
                self.cursor = self.cursor.saturating_sub(1);
            }
            b'\t' => {
                self.simple = false;
                self.move_to((self.cursor / TAB_WIDTH + 1) * TAB_WIDTH);
            }
            0x80..=0x9F => {
                self.simple = false;
                self.draw(char::REPLACEMENT_CHARACTER);
            }
            _ => {
                self.simple = false;
                if byte < 0x20 {
                    self.mark(control_picture(byte));
                }
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.simple = false;
        self.mark(ESC_GLYPH);
        if ignore || !intermediates.is_empty() {
            return;
        }
        match action {
            'm' => self.sgr(params),
            'K' => self.erase_in_line(first_param(params, 0)),
            'C' => {
                let n = first_param(params, 1).max(1) as usize;
                self.move_to(self.cursor.saturating_add(n));
            }
            'D' => {
                let n = first_param(params, 1).max(1) as usize;
                self.cursor = self.cursor.saturating_sub(n);
            }
            'G' => {
                let n = first_param(params, 1).max(1) as usize;
                self.move_to(n - 1);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {
        self.simple = false;
        self.mark(ESC_GLYPH);
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.simple = false;
        self.mark(ESC_GLYPH);
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.simple = false;
    }

    fn put(&mut self, _byte: u8) {
        self.simple = false;
    }

    fn unhook(&mut self) {
        self.simple = false;
        self.mark(ESC_GLYPH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Style = Style {
        fg: Color::Default,
        bg: Color::Default,
        flags: StyleFlags::NONE,
    };

    fn fg(c: Color) -> Style {
        Style { fg: c, ..D }
    }

    fn bg(c: Color) -> Style {
        Style { bg: c, ..D }
    }

    fn flags(f: StyleFlags) -> Style {
        Style { flags: f, ..D }
    }

    fn run(len: usize, style: Style) -> StyleRun {
        StyleRun { len, style }
    }

    /// Parse and return the first line's text and runs.
    fn first(bytes: &[u8]) -> (String, Vec<StyleRun>) {
        let lines = AnsiParser::parse_all(bytes);
        let line = lines.into_iter().next().expect("a line");
        (line.text, line.runs)
    }

    fn check_well_formed(line: &OwnedLine) {
        let total: usize = line.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.text.len(), "runs cover text: {line:?}");
        assert!(
            line.runs.iter().all(|r| r.len > 0),
            "no empty runs: {line:?}"
        );
        for pair in line.runs.windows(2) {
            assert_ne!(pair[0].style, pair[1].style, "coalesced: {line:?}");
        }
        let mut pos = 0;
        for r in &line.runs {
            pos += r.len;
            assert!(line.text.is_char_boundary(pos), "run on char boundary");
        }
        assert!(
            !line.text.chars().any(|c| c.is_control()),
            "no controls in {:?}",
            line.text
        );
    }

    /// The fixture table: input bytes, expected first-line text, expected runs.
    #[test]
    fn fixtures() {
        let red = fg(Color::Ansi(1));
        let cases: Vec<(&str, &[u8], &str, Vec<StyleRun>)> = vec![
            ("plain", b"hello\n", "hello", vec![run(5, D)]),
            ("crlf", b"hello\r\n", "hello", vec![run(5, D)]),
            ("empty", b"\n", "", vec![]),
            (
                "reset",
                b"\x1b[31mA\x1b[0mB\n",
                "AB",
                vec![run(1, red), run(1, D)],
            ),
            (
                "reset empty",
                b"\x1b[31mA\x1b[mB\n",
                "AB",
                vec![run(1, red), run(1, D)],
            ),
            (
                "bold",
                b"\x1b[1mB\n",
                "B",
                vec![run(1, flags(StyleFlags::BOLD))],
            ),
            (
                "dim",
                b"\x1b[2mB\n",
                "B",
                vec![run(1, flags(StyleFlags::DIM))],
            ),
            (
                "italic",
                b"\x1b[3mB\n",
                "B",
                vec![run(1, flags(StyleFlags::ITALIC))],
            ),
            (
                "underline",
                b"\x1b[4mB\n",
                "B",
                vec![run(1, flags(StyleFlags::UNDERLINE))],
            ),
            (
                "underline off by subparam",
                b"\x1b[4mA\x1b[4:0mB\n",
                "AB",
                vec![run(1, flags(StyleFlags::UNDERLINE)), run(1, D)],
            ),
            (
                "inverse",
                b"\x1b[7mB\n",
                "B",
                vec![run(1, flags(StyleFlags::INVERSE))],
            ),
            (
                "hidden",
                b"\x1b[8mB\n",
                "B",
                vec![run(1, flags(StyleFlags::HIDDEN))],
            ),
            (
                "strike",
                b"\x1b[9mB\n",
                "B",
                vec![run(1, flags(StyleFlags::STRIKETHROUGH))],
            ),
            (
                "flags off",
                b"\x1b[1;2;3;4;7;8;9mA\x1b[22;23;24;27;28;29mB\n",
                "AB",
                vec![
                    run(
                        1,
                        flags(StyleFlags(
                            StyleFlags::BOLD.0
                                | StyleFlags::DIM.0
                                | StyleFlags::ITALIC.0
                                | StyleFlags::UNDERLINE.0
                                | StyleFlags::INVERSE.0
                                | StyleFlags::HIDDEN.0
                                | StyleFlags::STRIKETHROUGH.0,
                        )),
                    ),
                    run(1, D),
                ],
            ),
            (
                "fg 16",
                b"\x1b[37mW\n",
                "W",
                vec![run(1, fg(Color::Ansi(7)))],
            ),
            (
                "bg 16",
                b"\x1b[42mG\n",
                "G",
                vec![run(1, bg(Color::Ansi(2)))],
            ),
            (
                "bright fg",
                b"\x1b[91mR\n",
                "R",
                vec![run(1, fg(Color::Ansi(9)))],
            ),
            (
                "bright bg",
                b"\x1b[107mW\n",
                "W",
                vec![run(1, bg(Color::Ansi(15)))],
            ),
            (
                "default fg bg",
                b"\x1b[31;42mA\x1b[39;49mB\n",
                "AB",
                vec![
                    run(
                        1,
                        Style {
                            fg: Color::Ansi(1),
                            bg: Color::Ansi(2),
                            ..D
                        },
                    ),
                    run(1, D),
                ],
            ),
            (
                "256 fg",
                b"\x1b[38;5;208mO\n",
                "O",
                vec![run(1, fg(Color::Indexed(208)))],
            ),
            (
                "256 low is ansi",
                b"\x1b[38;5;3mO\n",
                "O",
                vec![run(1, fg(Color::Ansi(3)))],
            ),
            (
                "256 bg colon",
                b"\x1b[48:5:17mO\n",
                "O",
                vec![run(1, bg(Color::Indexed(17)))],
            ),
            (
                "truecolor fg",
                b"\x1b[38;2;1;2;3mT\n",
                "T",
                vec![run(1, fg(Color::Rgb(1, 2, 3)))],
            ),
            (
                "truecolor bg colon",
                b"\x1b[48:2:10:20:30mT\n",
                "T",
                vec![run(1, bg(Color::Rgb(10, 20, 30)))],
            ),
            (
                "truecolor colorspace",
                b"\x1b[38:2:0:9:8:7mT\n",
                "T",
                vec![run(1, fg(Color::Rgb(9, 8, 7)))],
            ),
            (
                "truecolor then bold",
                b"\x1b[38;2;1;2;3;1mT\n",
                "T",
                vec![run(
                    1,
                    Style {
                        fg: Color::Rgb(1, 2, 3),
                        flags: StyleFlags::BOLD,
                        ..D
                    },
                )],
            ),
            (
                "underline color ignored",
                b"\x1b[58;5;9;1mU\n",
                "U",
                vec![run(1, flags(StyleFlags::BOLD))],
            ),
            (
                "out of range 256",
                b"\x1b[38;5;300mX\n",
                "X",
                vec![run(1, D)],
            ),
            ("cr overwrite", b"abcdef\rXY\n", "XYcdef", vec![run(6, D)]),
            (
                "cr progress",
                b"10%\r20%\r100%\r\n",
                "100%",
                vec![run(4, D)],
            ),
            (
                "cr overwrite styled",
                b"abc\r\x1b[31mX\n",
                "Xbc",
                vec![run(1, red), run(2, D)],
            ),
            ("backspace", b"abc\x08X\n", "abX", vec![run(3, D)]),
            ("backspace at 0", b"\x08\x08a\n", "a", vec![run(1, D)]),
            (
                "bs does not erase",
                b"abc\x08\x08\n",
                "abc",
                vec![run(3, D)],
            ),
            ("tab", b"a\tb\n", "a       b", vec![run(9, D)]),
            (
                "tab stop",
                b"12345678\tb\n",
                "12345678        b",
                vec![run(17, D)],
            ),
            ("tab trailing", b"a\t\n", "a", vec![run(1, D)]),
            (
                "tab overwrite",
                b"abcdefghijk\r\tX\n",
                "abcdefghXjk",
                vec![run(11, D)],
            ),
            ("el0", b"abcdef\r\x1b[Kxy\n", "xy", vec![run(2, D)]),
            ("el0 mid", b"abcdef\x1b[3D\x1b[0K\n", "abc", vec![run(3, D)]),
            ("el1", b"abcdef\x1b[3D\x1b[1K\n", "    ef", vec![run(6, D)]),
            ("el2", b"abcdef\x1b[2Kxy\n", "      xy", vec![run(8, D)]),
            (
                "el styled",
                b"\x1b[31mabc\x1b[0m\x08\x1b[K\n",
                "ab",
                vec![run(2, red)],
            ),
            ("cub", b"abc\x1b[2DX\n", "aXc", vec![run(3, D)]),
            ("cub default", b"abc\x1b[DX\n", "abX", vec![run(3, D)]),
            ("cuf pads", b"a\x1b[3Cb\n", "a   b", vec![run(5, D)]),
            ("cha", b"abcdef\x1b[3GX\n", "abXdef", vec![run(6, D)]),
            (
                "other csi ignored",
                b"a\x1b[?25l\x1b[2J\x1b[1;1Hb\n",
                "ab",
                vec![run(2, D)],
            ),
            ("osc bel", b"a\x1b]0;title\x07b\n", "ab", vec![run(2, D)]),
            ("osc st", b"a\x1b]0;title\x1b\\b\n", "ab", vec![run(2, D)]),
            ("dcs", b"a\x1bPq#0;2;0;0;0\x1b\\b\n", "ab", vec![run(2, D)]),
            (
                "esc ignored",
                b"a\x1b7\x1b8\x1b(Bb\n",
                "ab",
                vec![run(2, D)],
            ),
            ("bel dropped", b"a\x07b\n", "ab", vec![run(2, D)]),
            ("del dropped", b"a\x7fb\n", "ab", vec![run(2, D)]),
            ("nul dropped", b"a\x00b\n", "ab", vec![run(2, D)]),
            ("invalid utf8", b"a\xffb\n", "a\u{fffd}b", vec![run(5, D)]),
            ("c1 byte", b"a\x9bb\n", "a\u{fffd}b", vec![run(5, D)]),
            (
                "truncated utf8",
                b"a\xe2\x82\n",
                "a\u{fffd}",
                vec![run(4, D)],
            ),
            ("valid utf8", "é€😀\n".as_bytes(), "é€😀", vec![run(9, D)]),
            (
                "utf8 overwrite",
                "é€😀\rab\n".as_bytes(),
                "ab😀",
                vec![run(6, D)],
            ),
            ("lf inside csi", b"a\x1b[3\n1mb\n", "a", vec![run(1, D)]),
            (
                "swallowed lf resets",
                b"a\x1b]0;never terminated\nb\n",
                "a",
                vec![run(1, D)],
            ),
        ];
        for (name, input, text, runs) in cases {
            let lines = AnsiParser::parse_all(input);
            for line in &lines {
                check_well_formed(line);
            }
            let (got_text, got_runs) = first(input);
            assert_eq!(got_text, text, "{name}: text");
            assert_eq!(got_runs, runs, "{name}: runs");
        }
    }

    #[test]
    fn line_after_lf_inside_csi_gets_the_sequence() {
        let lines = AnsiParser::parse_all(b"a\x1b[3\n1mb\n");
        assert_eq!(lines[1].text, "b");
        assert_eq!(lines[1].runs, vec![run(1, fg(Color::Ansi(1)))]);
    }

    #[test]
    fn swallowed_lf_starts_a_clean_line() {
        let lines = AnsiParser::parse_all(b"a\x1b]0;x\nb\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "b");
        assert!(lines[0].complete && lines[1].complete);
    }

    #[test]
    fn pen_carries_across_lines() {
        let lines = AnsiParser::parse_all(b"\x1b[32mone\ntwo\x1b[0m\nthree\n");
        assert_eq!(lines[1].runs, vec![run(3, fg(Color::Ansi(2)))]);
        assert_eq!(lines[2].runs, vec![run(5, D)]);
    }

    #[test]
    fn split_escape_parses_like_whole() {
        let input: &[u8] = b"x\x1b[38;2;10;20;30mcolour\x1b[0m \xe2\x82\xac\r\nnext\x1b]0;t\x07!\n";
        let whole = AnsiParser::parse_all(input);
        for cut in 0..input.len() {
            let mut parser = AnsiParser::new();
            let mut lines = Vec::new();
            parser.feed(&input[..cut], |l| lines.push(OwnedLine::from(l)));
            parser.feed(&input[cut..], |l| lines.push(OwnedLine::from(l)));
            if let Some(l) = parser.current() {
                lines.push(l.into());
            }
            assert_eq!(lines, whole, "cut at {cut}");
        }
    }

    #[test]
    fn utf8_split_before_more_text() {
        // vte 0.15.0 alone drops the space here when the chunks split after 0xC3.
        let chunks: [&[u8]; 3] = [b"caf\xc3", b"\xa9 \xe2\x82", b"\xac\n"];
        let mut parser = AnsiParser::new();
        let mut lines: Vec<OwnedLine> = Vec::new();
        for c in chunks {
            parser.feed(c, |l| lines.push(l.into()));
        }
        assert_eq!(lines[0].text, "café €");
        assert_eq!(lines[0].raw_len, 10);
        assert_eq!(incomplete_utf8_suffix(b"ab\xf0\x9f\x98"), 3);
        assert_eq!(incomplete_utf8_suffix(b"ab\xf0\x9f\x98\x80"), 0);
        assert_eq!(incomplete_utf8_suffix(b"\xff"), 0);
        assert_eq!(incomplete_utf8_suffix(b"\x80\x80\x80"), 0);
    }

    #[test]
    fn long_line_without_lf() {
        let mut input = vec![b'x'; 4096];
        input.extend_from_slice(b"\x1b[1mY");
        let lines = AnsiParser::parse_all(&input);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(!line.complete);
        assert_eq!(line.text.len(), 4097);
        assert_eq!(
            line.runs,
            vec![run(4096, D), run(1, flags(StyleFlags::BOLD))]
        );
        assert_eq!(line.raw_len, input.len());
    }

    #[test]
    fn max_line_bytes_breaks_lines() {
        let mut parser = AnsiParser::with_max_line_bytes(16);
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(&[b'a'; 40], |l| lines.push(l.into()));
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.raw_len == 16 && !l.complete));
        assert_eq!(parser.current().map(|l| l.raw_len), Some(8));
        // An LF exactly at the limit still completes the line.
        let mut parser = AnsiParser::with_max_line_bytes(16);
        let mut input = vec![b'b'; 15];
        input.push(b'\n');
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(&input, |l| lines.push(l.into()));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].complete);
    }

    #[test]
    fn simple_flag() {
        let simple = |b: &[u8]| {
            let mut p = AnsiParser::new();
            let mut s = None;
            p.feed(b, |l| s = Some(l.simple));
            s.expect("a line")
        };
        assert!(simple(b"hello world\r\n"));
        assert!(simple(b"\rhello\n"));
        assert!(!simple(b"a\x1b[0mb\n"));
        assert!(!simple(b"ab\rc\n"));
        assert!(!simple(b"a\tb\n"));
        assert!(!simple(b"a\xffb\n"));
    }

    #[test]
    fn break_line_keeps_pen_and_state() {
        let mut parser = AnsiParser::new();
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(b"\x1b[31mhalf\x1b[", |l| lines.push(l.into()));
        parser.break_line(|l| lines.push(l.into()));
        parser.feed(b"1mrest\n", |l| lines.push(l.into()));
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].complete);
        assert_eq!(lines[0].text, "half");
        assert_eq!(
            lines[1].runs,
            vec![run(
                4,
                Style {
                    fg: Color::Ansi(1),
                    flags: StyleFlags::BOLD,
                    ..D
                }
            )]
        );
    }

    #[test]
    fn motion_stops_at_the_motion_width() {
        // A width probe with nothing answering it: the prompt lands at column 512.
        let probe = AnsiParser::parse_all(b"\x1b[999Cuart:~$ \n");
        assert_eq!(probe[0].text.len(), MOTION_WIDTH + 8);
        assert!(probe[0].text.ends_with("uart:~$ "));
        let cha = AnsiParser::parse_all(b"\x1b[65535Gx\n");
        assert_eq!(cha[0].text, format!("{}x", " ".repeat(MOTION_WIDTH)));
        // Past the motion width, printed text keeps going and motion stops at its end.
        let long = format!("{}\t\x1b[9CY\n", "x".repeat(600));
        let lines = AnsiParser::parse_all(long.as_bytes());
        assert_eq!(lines[0].text, format!("{}Y", "x".repeat(600)));
        // Motion back into the text is unaffected.
        let back = AnsiParser::parse_all(b"abcdef\x1b[2GZ\x1b[999CQ\n");
        assert_eq!(
            back[0].text,
            format!("aZcdef{}Q", " ".repeat(MOTION_WIDTH - 6))
        );
    }

    #[test]
    fn motion_cannot_amplify_input() {
        // 10 KiB of absolute moves to column 65535, each followed by a character.
        let input: Vec<u8> = b"\x1b[65535Gx\n"
            .iter()
            .copied()
            .cycle()
            .take(10 * 1024)
            .collect();
        let lines = AnsiParser::parse_all(&input);
        let text: usize = lines.iter().map(|l| l.text.len()).sum();
        assert_eq!(lines.len(), 1024);
        assert!(
            text <= lines.len() * (MOTION_WIDTH + 1),
            "{text} text bytes"
        );
    }

    #[test]
    fn nothing_is_dropped_at_the_line_limit() {
        let mut parser = AnsiParser::with_max_line_bytes(16);
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(b"abcdefghijklm\tZ\n", |l| lines.push(l.into()));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "abcdefghijklm   Z");
        // A tab or move far past the raw limit still keeps every character.
        let mut parser = AnsiParser::with_max_line_bytes(16);
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(b"\t\t\t\t\t\t\t\t\t\tabcdef", |l| lines.push(l.into()));
        parser.feed(b"\x1b[999Cgh", |l| lines.push(l.into()));
        if let Some(l) = parser.current() {
            lines.push(l.into());
        }
        let all: String = lines.iter().map(|l| l.text.trim()).collect();
        assert_eq!(all, "abcdefgh");
    }

    #[test]
    fn runs_are_capped_per_line() {
        let mut input = Vec::new();
        for i in 0..(MAX_RUNS + 500) {
            input.extend_from_slice(format!("\x1b[3{}mx", 1 + i % 2).as_bytes());
        }
        input.push(b'\n');
        let lines = AnsiParser::parse_all(&input);
        let line = &lines[0];
        assert_eq!(line.text.len(), MAX_RUNS + 500, "text is never capped");
        assert_eq!(line.runs.len(), MAX_RUNS);
        assert_eq!(
            line.runs.iter().map(|r| r.len).sum::<usize>(),
            line.text.len()
        );
        // The same holds once the line is in cell mode.
        let mut input = input[..input.len() - 1].to_vec();
        input.extend_from_slice(b"\rX\n");
        let lines = AnsiParser::parse_all(&input);
        let line = &lines[0];
        assert!(line.runs.len() <= MAX_RUNS);
        assert_eq!(
            line.runs.iter().map(|r| r.len).sum::<usize>(),
            line.text.len()
        );
        assert!(line.text.starts_with('X'));
    }

    #[test]
    fn buffers_shrink_after_a_long_line() {
        let mut parser = AnsiParser::with_max_line_bytes(MAX_LINE_BYTES_LIMIT);
        let small = parser.heap_bytes();
        let mut input = Vec::new();
        for i in 0..100_000 {
            input.extend_from_slice(format!("\x1b[3{}mé", 1 + i % 7).as_bytes());
        }
        input.extend_from_slice(b"\r\x1b[0mX\n");
        parser.feed(&input, |_| {});
        assert!(parser.heap_bytes() <= AnsiParser::worst_case_heap(MAX_LINE_BYTES_LIMIT));
        parser.feed(b"short\n", |_| {});
        let after = parser.heap_bytes();
        assert!(
            after <= small + KEEP_TEXT + KEEP_RUNS * 24 + KEEP_CELLS * 4 + KEEP_PALETTE * 9,
            "{after} bytes held after the long line"
        );
    }

    /// Overwriting a long line of multi-byte characters is linear, not quadratic.
    #[test]
    fn long_overwrites_are_linear() {
        let n = 15 * 1024;
        let cases = [("é", "è"), ("a", "é"), ("é", "a")];
        for (under, over) in cases {
            let mut input = under.repeat(n).into_bytes();
            input.push(b'\r');
            input.extend_from_slice(over.repeat(n).as_bytes());
            input.push(b'\n');
            let start = std::time::Instant::now();
            let lines = AnsiParser::parse_all(&input);
            let elapsed = start.elapsed();
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].text, over.repeat(n));
            // Quadratic editing took over 400 ms in release; linear takes a few ms. A
            // coverage build gets no limit: instrumented code is several times slower.
            let limit = if cfg!(coverage) {
                u128::MAX
            } else if cfg!(debug_assertions) {
                500
            } else {
                100
            };
            assert!(
                elapsed.as_millis() < limit,
                "{under:?} overwritten by {over:?}: {elapsed:?}"
            );
        }
    }

    /// A naive one-cell-per-column model of the documented line semantics.
    #[derive(Default)]
    struct Model {
        cells: Vec<(char, Style)>,
        cursor: usize,
        pen: Style,
    }

    impl Model {
        fn move_to(&mut self, col: usize) {
            self.cursor = if col <= self.cursor {
                col
            } else {
                col.min(self.cells.len().max(MOTION_WIDTH)).max(self.cursor)
            };
        }

        fn apply(&mut self, op: &Op) {
            match *op {
                Op::Char(c) => {
                    if self.cursor < self.cells.len() {
                        self.cells[self.cursor] = (c, self.pen);
                    } else {
                        self.cells.resize(self.cursor, (' ', D));
                        self.cells.push((c, self.pen));
                    }
                    self.cursor += 1;
                }
                Op::Cr => self.cursor = 0,
                Op::Bs => self.cursor = self.cursor.saturating_sub(1),
                Op::Tab => self.move_to((self.cursor / TAB_WIDTH + 1) * TAB_WIDTH),
                Op::Cuf(n) => self.move_to(self.cursor + n.max(1) as usize),
                Op::Cub(n) => self.cursor = self.cursor.saturating_sub(n.max(1) as usize),
                Op::Cha(n) => self.move_to(n.max(1) as usize - 1),
                Op::El(0) => {
                    if self.cursor < self.cells.len() {
                        self.cells.truncate(self.cursor);
                    }
                }
                Op::El(1) => {
                    let n = (self.cursor + 1).min(self.cells.len());
                    if n >= self.cells.len() {
                        self.cells.clear();
                    } else {
                        self.cells[..n].fill((' ', D));
                    }
                }
                Op::El(_) => self.cells.clear(),
                Op::Fg(0) => self.pen = D,
                Op::Fg(k) => self.pen = fg(Color::Ansi(k)),
            }
        }

        fn line(&self) -> (String, Vec<StyleRun>) {
            let mut text = String::new();
            let mut runs = Vec::new();
            for &(c, style) in &self.cells {
                text.push(c);
                push_run(&mut runs, c.len_utf8(), style);
            }
            (text, runs)
        }
    }

    #[derive(Clone, Debug)]
    enum Op {
        Char(char),
        Cr,
        Bs,
        Tab,
        Cuf(u16),
        Cub(u16),
        Cha(u16),
        El(u8),
        Fg(u8),
    }

    impl Op {
        fn encode(&self, out: &mut Vec<u8>) {
            match *self {
                Op::Char(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
                Op::Cr => out.push(b'\r'),
                Op::Bs => out.push(0x08),
                Op::Tab => out.push(b'\t'),
                Op::Cuf(n) => out.extend_from_slice(format!("\x1b[{n}C").as_bytes()),
                Op::Cub(n) => out.extend_from_slice(format!("\x1b[{n}D").as_bytes()),
                Op::Cha(n) => out.extend_from_slice(format!("\x1b[{n}G").as_bytes()),
                Op::El(k) => out.extend_from_slice(format!("\x1b[{k}K").as_bytes()),
                Op::Fg(0) => out.extend_from_slice(b"\x1b[0m"),
                Op::Fg(k) => out.extend_from_slice(format!("\x1b[3{k}m").as_bytes()),
            }
        }
    }

    fn op() -> impl proptest::strategy::Strategy<Value = Op> {
        use proptest::prelude::*;
        prop_oneof![
            8 => prop::sample::select(vec!['a', 'b', 'Z', ' ', 'é', '€', '😀', 'ñ'])
                .prop_map(Op::Char),
            2 => Just(Op::Cr),
            1 => Just(Op::Bs),
            1 => Just(Op::Tab),
            1 => (0u16..700).prop_map(Op::Cuf),
            1 => (0u16..30).prop_map(Op::Cub),
            1 => (0u16..700).prop_map(Op::Cha),
            1 => (0u8..3).prop_map(Op::El),
            1 => (0u8..4).prop_map(Op::Fg),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Append mode, cell mode and the switch between them all match the model, and
        /// splitting the bytes anywhere changes nothing.
        #[test]
        fn line_edits_match_the_model(
            ops in proptest::collection::vec(op(), 0..120),
            cut in 0usize..2000,
        ) {
            let mut model = Model::default();
            let mut bytes = Vec::new();
            for op in &ops {
                model.apply(op);
                op.encode(&mut bytes);
            }
            bytes.push(b'\n');
            let (text, runs) = model.line();
            let cut = cut.min(bytes.len());
            let mut parser = AnsiParser::new();
            let mut lines: Vec<OwnedLine> = Vec::new();
            parser.feed(&bytes[..cut], |l| lines.push(l.into()));
            if let Some(l) = parser.current() {
                // The line in progress is readable mid-way, and well formed.
                let total: usize = l.runs.iter().map(|r| r.len).sum();
                proptest::prop_assert_eq!(total, l.text.len());
            }
            parser.feed(&bytes[cut..], |l| lines.push(l.into()));
            proptest::prop_assert_eq!(lines.len(), 1);
            proptest::prop_assert_eq!(&lines[0].text, &text);
            proptest::prop_assert_eq!(&lines[0].runs, &runs);
        }
    }
}

/// The control-glyph mode: what each control byte shows as, and that showing it changes
/// nothing else.
#[cfg(test)]
mod control_tests {
    use super::*;

    const D: Style = Style {
        fg: Color::Default,
        bg: Color::Default,
        flags: StyleFlags::NONE,
    };

    fn run(len: usize, style: Style) -> StyleRun {
        StyleRun { len, style }
    }

    /// A glyph run of `glyphs` glyphs: each is three bytes of UTF-8.
    fn glyphs(count: usize) -> StyleRun {
        run(3 * count, CONTROL_STYLE)
    }

    fn parse(bytes: &[u8], show: bool) -> Vec<OwnedLine> {
        let mut parser = AnsiParser::new().show_control_chars(show);
        let mut lines = Vec::new();
        parser.feed(bytes, |line| lines.push(line.into()));
        if let Some(line) = parser.current() {
            lines.push(line.into());
        }
        lines
    }

    fn well_formed(line: &OwnedLine) {
        let total: usize = line.runs.iter().map(|r| r.len).sum();
        assert_eq!(total, line.text.len(), "runs cover text: {line:?}");
        assert!(line.runs.iter().all(|r| r.len > 0), "no empty runs");
        for pair in line.runs.windows(2) {
            assert_ne!(pair[0].style, pair[1].style, "coalesced: {line:?}");
        }
        let mut pos = 0;
        for r in &line.runs {
            pos += r.len;
            assert!(line.text.is_char_boundary(pos), "run on char boundary");
        }
        assert!(
            !line.text.chars().any(|c| c.is_control()),
            "no real controls in {:?}",
            line.text
        );
    }

    /// The text of the runs that are not control glyphs, with runs merged again.
    fn without_glyphs(line: &OwnedLine) -> (String, Vec<StyleRun>) {
        let (mut text, mut runs) = (String::new(), Vec::new());
        let mut at = 0;
        for r in &line.runs {
            if !r.style.flags.contains(StyleFlags::CONTROL) {
                text.push_str(&line.text[at..at + r.len]);
                push_run(&mut runs, r.len, r.style);
            }
            at += r.len;
        }
        (text, runs)
    }

    #[test]
    fn the_glyph_for_each_control_byte() {
        let red = Style {
            fg: Color::Ansi(1),
            ..D
        };
        let cases: Vec<(&str, &[u8], &str, Vec<StyleRun>)> = vec![
            (
                "lf ends the line",
                b"abc\n",
                "abc\u{240a}",
                vec![run(3, D), glyphs(1)],
            ),
            ("empty line", b"\n", "\u{240a}", vec![glyphs(1)]),
            (
                "crlf",
                b"abc\r\n",
                "abc\u{240d}\u{240a}",
                vec![run(3, D), glyphs(2)],
            ),
            (
                "cr goes at the end of what it overwrites",
                b"abcdef\rXY\n",
                "XYcdef\u{240d}\u{240a}",
                vec![run(6, D), glyphs(2)],
            ),
            (
                "cr progress",
                b"10%\r20%\r100%\r\n",
                "100\u{240d}\u{240d}%\u{240d}\u{240a}",
                vec![run(3, D), glyphs(2), run(1, D), glyphs(2)],
            ),
            (
                "bs",
                b"abc\x08X\n",
                "abX\u{2408}\u{240a}",
                vec![run(3, D), glyphs(2)],
            ),
            (
                "bs at the start",
                b"\x08\x08a\n",
                "\u{2408}\u{2408}a\u{240a}",
                vec![glyphs(2), run(1, D), glyphs(1)],
            ),
            (
                "sgr",
                b"\x1b[31mred\x1b[0m!\n",
                "\u{241b}red\u{241b}!\u{240a}",
                vec![glyphs(1), run(3, red), glyphs(1), run(1, D), glyphs(1)],
            ),
            (
                "osc ended by bel",
                b"a\x1b]0;title\x07b\n",
                "a\u{241b}b\u{240a}",
                vec![run(1, D), glyphs(1), run(1, D), glyphs(1)],
            ),
            (
                "osc ended by st is two escapes",
                b"a\x1b]0;title\x1b\\b\n",
                "a\u{241b}\u{241b}b\u{240a}",
                vec![run(1, D), glyphs(2), run(1, D), glyphs(1)],
            ),
            (
                "dcs ended by st is two escapes",
                b"a\x1bPq#0\x1b\\b\n",
                "a\u{241b}\u{241b}b\u{240a}",
                vec![run(1, D), glyphs(2), run(1, D), glyphs(1)],
            ),
            (
                "esc pair",
                b"a\x1b7b\n",
                "a\u{241b}b\u{240a}",
                vec![run(1, D), glyphs(1), run(1, D), glyphs(1)],
            ),
            (
                "unsupported csi",
                b"a\x1b[?25lb\n",
                "a\u{241b}b\u{240a}",
                vec![run(1, D), glyphs(1), run(1, D), glyphs(1)],
            ),
            (
                "other c0 bytes are their control pictures",
                b"a\x00b\x07c\x0bd\x0ce\x1af\n",
                "a\u{2400}b\u{2407}c\u{240b}d\u{240c}e\u{241a}f\u{240a}",
                vec![
                    run(1, D),
                    glyphs(1),
                    run(1, D),
                    glyphs(1),
                    run(1, D),
                    glyphs(1),
                    run(1, D),
                    glyphs(1),
                    run(1, D),
                    glyphs(1),
                    run(1, D),
                    glyphs(1),
                ],
            ),
            (
                "del",
                b"a\x7fb\n",
                "a\u{2421}b\u{240a}",
                vec![run(1, D), glyphs(1), run(1, D), glyphs(1)],
            ),
            (
                "tab is spaces and has no glyph",
                b"a\tb\n",
                "a       b\u{240a}",
                vec![run(9, D), glyphs(1)],
            ),
            (
                "invalid utf-8 is still the replacement character",
                b"a\xffb\x9bc\n",
                "a\u{fffd}b\u{fffd}c\u{240a}",
                vec![run(9, D), glyphs(1)],
            ),
            (
                "el0 truncates, glyphs go to the end",
                b"abcdef\x1b[3D\x1b[0K\n",
                "abc\u{241b}\u{241b}\u{240a}",
                vec![run(3, D), glyphs(3)],
            ),
            (
                "cuf pads after the glyph anchored before it",
                b"a\x1b[3Cb\n",
                "a\u{241b}   b\u{240a}",
                vec![run(1, D), glyphs(1), run(4, D), glyphs(1)],
            ),
            (
                "a glyph stays where the cursor was, not where the sequence moves it",
                b"abc\x1b[2DX\n",
                "aXc\u{241b}\u{240a}",
                vec![run(3, D), glyphs(2)],
            ),
            (
                "a line feed cuts off an unterminated string",
                b"a\x1b]0;never\nb\n",
                "a\u{241b}\u{240a}",
                vec![run(1, D), glyphs(2)],
            ),
        ];
        for (name, input, text, runs) in cases {
            let lines = parse(input, true);
            for line in &lines {
                well_formed(line);
            }
            assert_eq!(lines[0].text, text, "{name}: text");
            assert_eq!(lines[0].runs, runs, "{name}: runs");
            assert!(lines[0].complete, "{name}: complete");
        }
    }

    #[test]
    fn a_string_cut_off_by_a_line_feed_starts_the_next_line_clean() {
        let lines = parse(b"a\x1b]0;never\nb\n", true);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].text, "b\u{240a}");
        assert_eq!(lines[1].runs, vec![run(1, D), glyphs(1)]);
    }

    #[test]
    fn a_line_in_progress_and_a_broken_line_have_no_line_feed_glyph() {
        let lines = parse(b"abc\r", true);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].complete);
        assert_eq!(lines[0].text, "abc\u{240d}");
        // Ended early by the length limit: no LF, so no glyph for one.
        let mut parser = AnsiParser::with_max_line_bytes(16).show_control_chars(true);
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(&[b'a'; 40], |l| lines.push(l.into()));
        assert!(
            lines
                .iter()
                .all(|l| !l.complete && !l.text.contains('\u{240a}'))
        );
        // Ended by a local line: the glyphs so far stay, and nothing is added.
        let mut parser = AnsiParser::new().show_control_chars(true);
        let mut lines: Vec<OwnedLine> = Vec::new();
        parser.feed(b"half\r\x1b[1m", |l| lines.push(l.into()));
        parser.break_line(|l| lines.push(l.into()));
        parser.feed(b"rest\n", |l| lines.push(l.into()));
        // The escape sequence came after the CR, so its glyph is anchored at column 0.
        assert_eq!(lines[0].text, "\u{241b}half\u{240d}");
        assert!(!lines[0].complete);
        assert_eq!(lines[1].text, "rest\u{240a}");
        assert_eq!(lines[1].runs[0].style.flags, StyleFlags::BOLD);
    }

    #[test]
    fn the_flag_is_off_by_default_and_changes_nothing_when_off() {
        let input = b"a\rb\x08\x1b[31mc\x00\x7f\x1b]0;t\x07\n";
        let default = {
            let mut parser = AnsiParser::new();
            let mut lines = Vec::new();
            parser.feed(input, |l| lines.push(OwnedLine::from(l)));
            lines
        };
        assert_eq!(default, parse(input, false));
        assert!(
            default[0]
                .runs
                .iter()
                .all(|r| !r.style.flags.contains(StyleFlags::CONTROL))
        );
        assert_ne!(default, parse(input, true));
    }

    #[test]
    fn glyph_runs_are_dim_control_and_default_colored() {
        assert_eq!(CONTROL_STYLE.fg, Color::Default);
        assert_eq!(CONTROL_STYLE.bg, Color::Default);
        assert!(CONTROL_STYLE.flags.contains(StyleFlags::DIM));
        assert!(CONTROL_STYLE.flags.contains(StyleFlags::CONTROL));
        assert_eq!(
            CONTROL_STYLE.flags,
            StyleFlags(StyleFlags::DIM.0 | StyleFlags::CONTROL.0)
        );
        // The pen's color does not leak into the glyphs, and they do not touch the pen.
        let lines = parse(b"\x1b[31;1mx\ry\n", true);
        let glyph = lines[0].runs.iter().find(|r| r.style == CONTROL_STYLE);
        assert!(glyph.is_some(), "{:?}", lines[0]);
        let mut parser = AnsiParser::new().show_control_chars(true);
        parser.feed(b"\x1b[31;1m\r", |_| {});
        assert_eq!(
            parser.pen(),
            Style {
                fg: Color::Ansi(1),
                bg: Color::Default,
                flags: StyleFlags::BOLD
            }
        );
    }

    #[test]
    fn a_line_with_glyphs_is_never_plain() {
        let simple = |b: &[u8], show: bool| {
            let mut p = AnsiParser::new().show_control_chars(show);
            let mut s = None;
            p.feed(b, |l| s = Some(l.simple));
            s.expect("a line")
        };
        assert!(simple(b"hello\r\n", false));
        assert!(!simple(b"hello\r\n", true), "the LF has a glyph");
        assert!(!simple(b"\rhello\n", true));
    }

    #[test]
    fn a_line_shows_at_most_the_glyph_limit() {
        let mut input = b"x".to_vec();
        input.extend(std::iter::repeat_n(b'\r', MAX_CONTROL_GLYPHS + 100));
        input.push(b'\n');
        let lines = parse(&input, true);
        assert_eq!(lines.len(), 1);
        well_formed(&lines[0]);
        let shown = lines[0].text.chars().filter(|&c| c == '\u{240d}').count();
        assert!(shown == MAX_CONTROL_GLYPHS, "{shown} glyphs");
        assert!(!lines[0].text.contains('\u{240a}'), "past the cap, nothing");
        assert!(lines[0].text.replace('\u{240d}', "") == "x");
    }

    #[test]
    fn heap_stays_within_the_stated_worst_case() {
        let mut parser =
            AnsiParser::with_max_line_bytes(MAX_LINE_BYTES_LIMIT).show_control_chars(true);
        let small = parser.heap_bytes();
        let mut input = Vec::new();
        while input.len() < MAX_LINE_BYTES_LIMIT - 64 {
            input.extend_from_slice(b"\x1b[31m\xc3\xa9\r\x08\x1b[0m\xc3\xa9\x00");
        }
        parser.feed(&input, |_| {});
        let held = parser.heap_bytes();
        let limit = AnsiParser::worst_case_heap(MAX_LINE_BYTES_LIMIT)
            + AnsiParser::worst_case_control_heap(MAX_LINE_BYTES_LIMIT);
        assert!(held <= limit, "{held} > {limit}");
        parser.feed(b"\n", |_| {});
        parser.feed(b"short\n", |_| {});
        let after = parser.heap_bytes();
        let kept = 2 * KEEP_TEXT
            + 2 * KEEP_RUNS * size_of::<StyleRun>()
            + KEEP_CELLS * size_of::<u32>()
            + KEEP_PALETTE * size_of::<Style>()
            + KEEP_MARKS * size_of::<Mark>();
        assert!(
            after <= small + kept,
            "{after} bytes held after the long line"
        );
    }

    /// Pieces of a stream that between them exercise every kind of control byte.
    const PIECES: &[&[u8]] = &[
        b"a",
        b"b",
        b" ",
        "\u{e9}".as_bytes(),
        "\u{20ac}".as_bytes(),
        b"\r",
        b"\n",
        b"\r\n",
        b"\x08",
        b"\t",
        b"\x00",
        b"\x07",
        b"\x7f",
        b"\x18",
        b"\x1a",
        b"\x1b[31m",
        b"\x1b[0m",
        b"\x1b[1;4m",
        b"\x1b[K",
        b"\x1b[2K",
        b"\x1b[3D",
        b"\x1b[5C",
        b"\x1b[2G",
        b"\x1b[?25l",
        b"\x1b[3:",
        b"\x1b]0;title\x07",
        b"\x1b]0;title\x1b\\",
        b"\x1b]0;never ends",
        b"\x1bPq#0\x1b\\",
        b"\x1b7",
        b"\x1b(B",
        b"\xff",
        b"\x9b",
    ];

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Splitting the stream anywhere gives the same lines with the flag on, as it
        /// does with it off.
        #[test]
        fn chunk_boundaries_do_not_matter_with_glyphs_on(
            pieces in proptest::collection::vec(proptest::sample::select(PIECES), 0..80),
            cuts in proptest::collection::vec(0usize..400, 1..4),
        ) {
            let bytes: Vec<u8> = pieces.concat();
            let whole = parse(&bytes, true);
            let mut cuts: Vec<usize> = cuts.into_iter().map(|c| c.min(bytes.len())).collect();
            cuts.sort_unstable();
            let mut parser = AnsiParser::new().show_control_chars(true);
            let mut lines: Vec<OwnedLine> = Vec::new();
            let mut from = 0;
            for cut in cuts.into_iter().chain([bytes.len()]) {
                parser.feed(&bytes[from..cut], |l| lines.push(l.into()));
                if let Some(l) = parser.current() {
                    let total: usize = l.runs.iter().map(|r| r.len).sum();
                    proptest::prop_assert_eq!(total, l.text.len());
                }
                from = cut;
            }
            if let Some(l) = parser.current() {
                lines.push(l.into());
            }
            proptest::prop_assert_eq!(lines, whole);
        }

        /// The glyphs are visual only: without them the line is what the flag-off parser
        /// makes of the same bytes, and every glyph run is a dim control run of glyphs.
        #[test]
        fn glyphs_change_nothing_else(
            pieces in proptest::collection::vec(proptest::sample::select(PIECES), 0..80),
        ) {
            let bytes: Vec<u8> = pieces.concat();
            let plain = parse(&bytes, false);
            let shown = parse(&bytes, true);
            proptest::prop_assert_eq!(plain.len(), shown.len());
            for (plain, shown) in plain.iter().zip(&shown) {
                well_formed(shown);
                proptest::prop_assert_eq!(plain.raw_len, shown.raw_len);
                proptest::prop_assert_eq!(plain.complete, shown.complete);
                let (text, runs) = without_glyphs(shown);
                proptest::prop_assert_eq!(&text, &plain.text);
                proptest::prop_assert_eq!(&runs, &plain.runs);
                let mut at = 0;
                for r in &shown.runs {
                    if r.style.flags.contains(StyleFlags::CONTROL) {
                        proptest::prop_assert_eq!(r.style, CONTROL_STYLE);
                        let all_pictures = shown.text[at..at + r.len]
                            .chars()
                            .all(|c| ('\u{2400}'..='\u{2421}').contains(&c));
                        proptest::prop_assert!(all_pictures);
                    }
                    at += r.len;
                }
            }
        }
    }
}
