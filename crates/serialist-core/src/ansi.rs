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
use std::ops::Range;

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
            line: LineState::new(max_line_bytes),
            max_line_bytes,
        }
    }

    pub fn max_line_bytes(&self) -> usize {
        self.max_line_bytes
    }

    /// The style new text is drawn in.
    pub fn pen(&self) -> Style {
        self.line.pen
    }

    /// Parse `bytes`, calling `on_line` for every line that ends in them, in order.
    pub fn feed(&mut self, bytes: &[u8], mut on_line: impl FnMut(ParsedLine<'_>)) {
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
                    }
                    self.line.raw_len += i + 1;
                    on_line(self.line.view(true));
                    self.line.reset();
                    rest = &rest[i + 1..];
                }
                None => {
                    self.vte.advance(&mut self.line, window);
                    self.line.raw_len += window.len();
                    if self.line.raw_len >= self.max_line_bytes {
                        self.vte = vte::Parser::new();
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
    pub fn break_line(&mut self, on_line: impl FnOnce(ParsedLine<'_>)) {
        if self.line.raw_len > 0 {
            on_line(self.line.view(false));
            self.line.reset();
        }
    }

    /// Bytes of heap this parser holds, for memory accounting.
    pub fn heap_bytes(&self) -> usize {
        self.line.text.capacity()
            + (self.line.runs.capacity() + self.line.scratch.capacity())
                * std::mem::size_of::<StyleRun>()
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
struct LineState {
    text: String,
    runs: Vec<StyleRun>,
    /// Reused by run splicing so an overwrite does not allocate.
    scratch: Vec<StyleRun>,
    /// Characters in `text`, which is also its width in columns.
    cols: usize,
    /// `text` is all ASCII, so a column is a byte offset.
    ascii: bool,
    cursor: usize,
    pen: Style,
    simple: bool,
    raw_len: usize,
    lf_executed: bool,
    max_cols: usize,
}

impl LineState {
    fn new(max_cols: usize) -> Self {
        Self {
            text: String::new(),
            runs: Vec::new(),
            scratch: Vec::new(),
            cols: 0,
            ascii: true,
            cursor: 0,
            pen: Style::default(),
            simple: true,
            raw_len: 0,
            lf_executed: false,
            max_cols,
        }
    }

    fn view(&self, complete: bool) -> ParsedLine<'_> {
        ParsedLine {
            text: &self.text,
            runs: &self.runs,
            raw_len: self.raw_len,
            complete,
            simple: self.simple,
        }
    }

    /// Start a new line. The pen carries over; everything else starts fresh.
    fn reset(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.cols = 0;
        self.ascii = true;
        self.cursor = 0;
        self.simple = true;
        self.raw_len = 0;
        if self.text.capacity() > 4 * DEFAULT_MAX_LINE_BYTES {
            self.text.shrink_to(DEFAULT_MAX_LINE_BYTES);
        }
    }

    fn clear_text(&mut self) {
        self.text.clear();
        self.runs.clear();
        self.cols = 0;
        self.ascii = true;
    }

    fn byte_of_col(&self, col: usize) -> usize {
        if col >= self.cols {
            self.text.len()
        } else if self.ascii {
            col
        } else {
            self.text
                .char_indices()
                .nth(col)
                .map_or(self.text.len(), |(i, _)| i)
        }
    }

    fn style_at(&self, byte: usize) -> Style {
        let mut pos = 0;
        for run in &self.runs {
            pos += run.len;
            if byte < pos {
                return run.style;
            }
        }
        Style::default()
    }

    fn append(&mut self, c: char, style: Style) {
        self.text.push(c);
        if !c.is_ascii() {
            self.ascii = false;
        }
        push_run(&mut self.runs, c.len_utf8(), style);
        self.cols += 1;
    }

    /// Draw `c` at the cursor and advance it.
    fn put(&mut self, c: char) {
        if self.cursor >= self.max_cols {
            return;
        }
        let style = self.pen;
        if self.cursor >= self.cols {
            for _ in self.cols..self.cursor {
                self.append(' ', Style::default());
            }
            self.append(c, style);
        } else {
            let start = self.byte_of_col(self.cursor);
            let old_len = self.text[start..].chars().next().map_or(0, char::len_utf8);
            let mut buf = [0u8; 4];
            let new = c.encode_utf8(&mut buf);
            let same_style = self.style_at(start) == style;
            self.text.replace_range(start..start + old_len, new);
            if !(same_style && old_len == new.len()) {
                self.splice(start..start + old_len, new.len(), style);
            }
            if !c.is_ascii() {
                self.ascii = false;
            }
        }
        self.cursor += 1;
    }

    /// Replace the runs over `range` (old byte offsets) with one run of `new_len` bytes.
    fn splice(&mut self, range: Range<usize>, new_len: usize, style: Style) {
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let mut pos = 0;
        for run in &self.runs {
            let (start, end) = (pos, pos + run.len);
            pos = end;
            let before_end = end.min(range.start);
            if start < before_end {
                push_run(&mut out, before_end - start, run.style);
            }
        }
        if new_len > 0 {
            push_run(&mut out, new_len, style);
        }
        pos = 0;
        for run in &self.runs {
            let (start, end) = (pos, pos + run.len);
            pos = end;
            let after_start = start.max(range.end);
            if after_start < end {
                push_run(&mut out, end - after_start, run.style);
            }
        }
        self.scratch = std::mem::replace(&mut self.runs, out);
    }

    fn truncate_at_col(&mut self, col: usize) {
        if col >= self.cols {
            return;
        }
        if col == 0 {
            self.clear_text();
            return;
        }
        let byte = self.byte_of_col(col);
        self.text.truncate(byte);
        let mut pos = 0;
        let mut keep = 0;
        for run in &mut self.runs {
            if pos + run.len >= byte {
                run.len = byte - pos;
                keep += 1;
                break;
            }
            pos += run.len;
            keep += 1;
        }
        self.runs.truncate(keep);
        if self.runs.last().is_some_and(|r| r.len == 0) {
            self.runs.pop();
        }
        self.cols = col;
    }

    fn erase_in_line(&mut self, mode: u16) {
        match mode {
            0 => self.truncate_at_col(self.cursor),
            1 => {
                let n = (self.cursor + 1).min(self.cols);
                if n >= self.cols {
                    self.clear_text();
                } else if n > 0 {
                    let end = self.byte_of_col(n);
                    self.text.replace_range(..end, &" ".repeat(n));
                    self.splice(0..end, n, Style::default());
                }
            }
            2 => self.clear_text(),
            _ => {}
        }
    }

    fn move_to(&mut self, col: usize) {
        self.cursor = col.min(self.max_cols);
    }

    fn sgr(&mut self, params: &Params) {
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

/// Append a run, merging it into the last one when the style matches.
pub(crate) fn push_run(runs: &mut Vec<StyleRun>, len: usize, style: Style) {
    if len == 0 {
        return;
    }
    match runs.last_mut() {
        Some(last) if last.style == style => last.len += len,
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
            return;
        }
        if c == char::REPLACEMENT_CHARACTER
            || self.cursor != self.cols
            || self.pen != Style::default()
        {
            self.simple = false;
        }
        self.put(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.lf_executed = true,
            b'\r' => self.cursor = 0,
            0x08 => {
                self.simple = false;
                self.cursor = self.cursor.saturating_sub(1);
            }
            b'\t' => {
                self.simple = false;
                self.move_to((self.cursor / TAB_WIDTH + 1) * TAB_WIDTH);
            }
            0x80..=0x9F => {
                self.simple = false;
                self.put(char::REPLACEMENT_CHARACTER);
            }
            _ => self.simple = false,
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.simple = false;
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
    }

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {
        self.simple = false;
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.simple = false;
    }

    fn put(&mut self, _byte: u8) {
        self.simple = false;
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
}
