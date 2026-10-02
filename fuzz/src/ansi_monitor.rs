//! `ansi_monitor`: the monitor-mode ANSI parser ([`AnsiParser`]) on arbitrary bytes in
//! arbitrary chunks.
//!
//! The input is an [`Input`]. Config bit 0 turns on
//! [`show_control_chars`](AnsiParser::show_control_chars); bits 1..=3 pick
//! `max_line_bytes` from [`MAX_LINE`]: the default for 0, then small values so short
//! inputs reach the line limit.
//!
//! Beyond not panicking, it checks what the parser promises in its module docs:
//!
//! - Chunking does not matter: the lines, and the line in progress, are those of one
//!   `feed` of the whole stream.
//! - No byte is lost or counted twice: the lines' raw lengths add up to the stream.
//! - Every line is well formed: its runs cover the text on character boundaries, none
//!   is empty, neighbours differ in style, the text has no control characters, the raw
//!   length is within `max_line_bytes` and the text within the columns that allows.
//! - Memory is bounded: after every `feed`, [`AnsiParser::heap_bytes`] is within
//!   [`AnsiParser::worst_case_heap`], plus [`AnsiParser::worst_case_control_heap`] with
//!   glyphs shown. The store sizes its budget on those, so going past them is a bug.

use serialist_core::ansi::{
    AnsiParser, DEFAULT_MAX_LINE_BYTES, MAX_CONTROL_GLYPHS, MAX_LINE_BYTES_LIMIT, MIN_LINE_BYTES,
    MOTION_WIDTH, OwnedLine,
};

use crate::Input;

/// The `max_line_bytes` values config bits 1..=3 choose from.
pub const MAX_LINE: [usize; 8] = [
    DEFAULT_MAX_LINE_BYTES,
    MIN_LINE_BYTES,
    17,
    31,
    64,
    255,
    4096,
    MAX_LINE_BYTES_LIMIT,
];

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    let show = input.flag(0);
    let max_line = input.pick(1, &MAX_LINE[..]);

    let whole = parse(std::iter::once(input.stream), show, max_line);
    let split = parse(input.chunks(), show, max_line);
    assert_eq!(split, whole, "chunking changed the lines");

    let total: usize = whole.iter().map(|line| line.raw_len).sum();
    assert_eq!(
        total,
        input.stream.len(),
        "raw lengths do not tile the stream"
    );
    for line in &whole {
        check_line(line, show, max_line);
    }
}

/// Feed `chunks` to a fresh parser and return its lines, the one in progress last,
/// checking its heap after every feed.
fn parse<'a>(
    chunks: impl Iterator<Item = &'a [u8]>,
    show: bool,
    max_line: usize,
) -> Vec<OwnedLine> {
    let mut parser = AnsiParser::with_max_line_bytes(max_line).show_control_chars(show);
    assert_eq!(parser.max_line_bytes(), max_line);
    let mut limit = AnsiParser::worst_case_heap(max_line);
    if show {
        limit += AnsiParser::worst_case_control_heap(max_line);
    }
    let mut lines = Vec::new();
    for chunk in chunks {
        parser.feed(chunk, |line| lines.push(OwnedLine::from(line)));
        let heap = parser.heap_bytes();
        assert!(
            heap <= limit,
            "the parser holds {heap} bytes of heap, past its worst case of {limit}"
        );
    }
    if let Some(line) = parser.current() {
        lines.push(line.into());
    }
    lines
}

fn check_line(line: &OwnedLine, show: bool, max_line: usize) {
    assert!(
        (1..=max_line).contains(&line.raw_len),
        "raw length {} outside 1..={max_line}: {line:?}",
        line.raw_len
    );
    // One character per column, at most 4 bytes each; glyphs are 3 bytes (U+2400..).
    let glyphs = if show { 3 * MAX_CONTROL_GLYPHS } else { 0 };
    let most = 4 * (max_line + MOTION_WIDTH) + glyphs;
    assert!(line.text.len() <= most, "text past {most} bytes: {line:?}");
    assert!(
        !line.text.chars().any(char::is_control),
        "control character in the text: {line:?}"
    );

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
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_replay() {
        crate::replay_seeds("ansi_monitor", super::run);
    }
}
