//! Scrollback for the milestone 0 session view, in two halves.
//!
//! [`LineSplitter`] turns received chunks into text. It runs on the background executor
//! (the drain worker owns it), so raw bytes never reach the main thread. It splits on
//! `\n`, drops `\r` so CRLF and LFCR devices both read cleanly, and decodes UTF-8
//! lossily only once a line is complete, so a multi-byte character split across two
//! chunks still decodes.
//!
//! [`LineBuffer`] is what the view shows: a capped list of decoded lines plus the line
//! still in progress. It lives in the entity on the main thread.
//!
//! Both are placeholders for the page store and line index of milestone 1, kept free of
//! GPUI so they test as plain Rust.

use std::collections::VecDeque;

/// The milestone 0 scrollback cap.
pub const DEFAULT_MAX_LINES: usize = 10_000;

/// A line with no `\n` for this long is wrapped by force. Binary noise can arrive with no
/// newline at all, and one unbounded row would make every frame shape megabytes of text.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// Text produced from one batch of received chunks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RxText {
    /// Lines completed during the batch, oldest first.
    pub lines: Vec<String>,
    /// The whole line still waiting for its newline, as it should be displayed.
    pub partial: String,
}

/// Bytes to lines. Keeps only the bytes of the unfinished line between calls.
#[derive(Clone, Debug, Default)]
pub struct LineSplitter {
    /// Raw bytes of the current line, `\r` removed.
    partial: Vec<u8>,
}

impl LineSplitter {
    /// Split a batch of chunks, exactly as the transport delivered them.
    pub fn split<'a>(&mut self, chunks: impl IntoIterator<Item = &'a [u8]>) -> RxText {
        let mut lines = Vec::new();
        for chunk in chunks {
            self.push_chunk(chunk, &mut lines);
        }
        RxText {
            lines,
            partial: self.partial_text(),
        }
    }

    fn push_chunk(&mut self, bytes: &[u8], lines: &mut Vec<String>) {
        let mut rest = bytes;
        while let Some(newline) = rest.iter().position(|&b| b == b'\n') {
            self.extend(&rest[..newline], lines);
            lines.push(String::from_utf8_lossy(&self.partial).into_owned());
            self.partial.clear();
            rest = &rest[newline + 1..];
        }
        self.extend(rest, lines);
    }

    fn extend(&mut self, segment: &[u8], lines: &mut Vec<String>) {
        self.partial
            .extend(segment.iter().copied().filter(|&b| b != b'\r'));
        while self.partial.len() > MAX_LINE_BYTES {
            let cut = char_boundary_at_or_before(&self.partial, MAX_LINE_BYTES);
            lines.push(String::from_utf8_lossy(&self.partial[..cut]).into_owned());
            self.partial.drain(..cut);
        }
    }

    /// Display form of the unfinished line. A trailing, not yet complete UTF-8 sequence
    /// is held back: the next chunk most likely completes it, and showing U+FFFD for a
    /// frame would flicker.
    fn partial_text(&self) -> String {
        let shown = self.partial.len() - incomplete_utf8_tail(&self.partial);
        String::from_utf8_lossy(&self.partial[..shown]).into_owned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// Received from the device.
    Rx,
    /// Echo of what the user sent.
    Tx,
    /// Session notices such as "connected".
    Info,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row<'a> {
    pub kind: LineKind,
    pub text: &'a str,
}

#[derive(Clone, Debug)]
pub struct LineBuffer {
    lines: VecDeque<Line>,
    /// The received line in progress, minus any frozen prefix.
    partial: String,
    /// Start of the in-progress line that was already moved into `lines`, because a
    /// sent line or a clear closed it. The splitter still reports the line from its
    /// start, so this prefix is cut from its next reports.
    frozen: Option<String>,
    max_lines: usize,
    evicted: u64,
}

impl Default for LineBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LINES)
    }
}

impl LineBuffer {
    /// `max_lines` counts the in-progress line too; it is clamped to at least 1.
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            partial: String::new(),
            frozen: None,
            max_lines: max_lines.max(1),
            evicted: 0,
        }
    }

    pub fn max_lines(&self) -> usize {
        self.max_lines
    }

    /// Rows to display: complete lines plus the in-progress received line, if any.
    pub fn len(&self) -> usize {
        self.lines.len() + usize::from(!self.partial.is_empty())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines dropped from the front to honour the cap since creation or the last `clear`.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    pub fn row(&self, ix: usize) -> Option<Row<'_>> {
        if let Some(line) = self.lines.get(ix) {
            return Some(Row {
                kind: line.kind,
                text: &line.text,
            });
        }
        (ix == self.lines.len() && !self.partial.is_empty()).then_some(Row {
            kind: LineKind::Rx,
            text: &self.partial,
        })
    }

    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        (0..self.len()).filter_map(|ix| self.row(ix))
    }

    /// Complete lines only, oldest first.
    pub fn complete_lines(&self) -> impl Iterator<Item = &Line> + '_ {
        self.lines.iter()
    }

    /// The received line still waiting for its newline, as displayed.
    pub fn partial_text(&self) -> &str {
        &self.partial
    }

    /// Take in what the splitter made of a batch. Returns whether anything changed.
    pub fn apply_rx(&mut self, rx: RxText) -> bool {
        let changed = !rx.lines.is_empty() || self.visible_part(&rx.partial) != self.partial;
        // Lines the cap would evict straight away are never stored.
        let skip = rx.lines.len().saturating_sub(self.max_lines);
        if skip > 0 {
            self.frozen = None;
            self.evicted += skip as u64;
        }
        for line in rx.lines.into_iter().skip(skip) {
            let text = match self.frozen.take() {
                Some(frozen) => match line.strip_prefix(frozen.as_str()) {
                    // The whole line was already shown; only its newline was missing.
                    Some("") => continue,
                    Some(rest) => rest.to_owned(),
                    None => line,
                },
                None => line,
            };
            self.lines.push_back(Line {
                kind: LineKind::Rx,
                text,
            });
        }
        self.partial = self.visible_part(&rx.partial).to_owned();
        self.enforce_cap();
        changed
    }

    /// Add a whole line that did not come from the device (TX echo, notices).
    ///
    /// An in-progress received line is closed first, so the new line never lands in the
    /// middle of device output; the rest of that line continues on a fresh row.
    pub fn push_line(&mut self, kind: LineKind, text: &str) {
        if let Some(partial) = self.freeze_partial() {
            self.lines.push_back(Line {
                kind: LineKind::Rx,
                text: partial,
            });
        }
        for piece in text.split('\n') {
            self.lines.push_back(Line {
                kind,
                text: piece.replace('\r', ""),
            });
        }
        self.enforce_cap();
    }

    pub fn clear(&mut self) {
        // Freeze so the rest of an interrupted line does not bring its start back.
        self.freeze_partial();
        self.lines.clear();
        self.evicted = 0;
    }

    /// Mark the displayed in-progress text as consumed and return it.
    fn freeze_partial(&mut self) -> Option<String> {
        if self.partial.is_empty() {
            return None;
        }
        let partial = std::mem::take(&mut self.partial);
        self.frozen
            .get_or_insert_with(String::new)
            .push_str(&partial);
        Some(partial)
    }

    fn visible_part<'a>(&self, partial: &'a str) -> &'a str {
        match &self.frozen {
            Some(frozen) => partial.strip_prefix(frozen.as_str()).unwrap_or(partial),
            None => partial,
        }
    }

    fn enforce_cap(&mut self) {
        while self.len() > self.max_lines {
            if self.lines.pop_front().is_some() {
                self.evicted += 1;
            } else {
                // Only reachable with a cap of 1 and an in-progress line; the partial row
                // itself always stays visible.
                break;
            }
        }
    }
}

/// Largest index `<= at` that does not split a UTF-8 sequence, so a forced wrap never
/// turns one valid character into two replacement characters.
fn char_boundary_at_or_before(bytes: &[u8], at: usize) -> usize {
    let at = at.min(bytes.len());
    let mut cut = at;
    while cut > 0 && at - cut < 4 && cut < bytes.len() && is_continuation(bytes[cut]) {
        cut -= 1;
    }
    if cut == 0 || at - cut >= 4 { at } else { cut }
}

/// How many trailing bytes form the start of a multi-byte sequence that is still missing
/// its continuation bytes.
fn incomplete_utf8_tail(bytes: &[u8]) -> usize {
    let len = bytes.len();
    for back in 1..=len.min(3) {
        let byte = bytes[len - back];
        if is_continuation(byte) {
            continue;
        }
        let needed = match byte {
            0xF0..=0xFF => 4,
            0xE0..=0xEF => 3,
            0xC0..=0xDF => 2,
            _ => 1,
        };
        return if needed > back { back } else { 0 };
    }
    0
}

fn is_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A splitter and a buffer wired the way the session view wires them.
    #[derive(Default)]
    struct Pipeline {
        splitter: LineSplitter,
        buffer: LineBuffer,
    }

    impl Pipeline {
        fn with_cap(max_lines: usize) -> Self {
            Self {
                splitter: LineSplitter::default(),
                buffer: LineBuffer::new(max_lines),
            }
        }

        fn rx(&mut self, chunks: &[&[u8]]) -> bool {
            let rx = self.splitter.split(chunks.iter().copied());
            self.buffer.apply_rx(rx)
        }
    }

    /// Complete lines each with their newline, then the in-progress line: the inverse of
    /// splitting, which is what the chunking property compares against.
    fn concatenated(buffer: &LineBuffer) -> String {
        let mut out = String::new();
        for line in buffer.complete_lines() {
            out.push_str(&line.text);
            out.push('\n');
        }
        out.push_str(buffer.partial_text());
        out
    }

    fn texts(buffer: &LineBuffer) -> Vec<&str> {
        buffer.rows().map(|row| row.text).collect()
    }

    #[test]
    fn splits_on_newline_and_keeps_the_partial_line() {
        let mut p = Pipeline::default();
        assert!(p.rx(&[b"hello\nwor"]));
        assert_eq!(texts(&p.buffer), ["hello", "wor"]);
        assert!(p.rx(&[b"ld\n"]));
        assert_eq!(texts(&p.buffer), ["hello", "world"]);
        assert!(!p.rx(&[]), "an empty batch changes nothing");
    }

    #[test]
    fn carriage_returns_are_tolerated() {
        let mut p = Pipeline::default();
        p.rx(&[b"crlf\r\nlfcr\n\rmid\rdle\r", b"\n\r\n"]);
        assert_eq!(texts(&p.buffer), ["crlf", "lfcr", "middle", ""]);
    }

    #[test]
    fn crlf_split_across_batches_yields_one_line_break() {
        let mut p = Pipeline::default();
        p.rx(&[b"OK\r"]);
        assert_eq!(texts(&p.buffer), ["OK"]);
        p.rx(&[b"\nnext"]);
        assert_eq!(texts(&p.buffer), ["OK", "next"]);
    }

    #[test]
    fn invalid_utf8_is_shown_lossily() {
        let mut p = Pipeline::default();
        p.rx(&[b"ok \xff\xfe bytes\n"]);
        assert_eq!(texts(&p.buffer), ["ok \u{FFFD}\u{FFFD} bytes"]);
    }

    #[test]
    fn multibyte_character_split_across_chunks_decodes_once_complete() {
        let snowman = "\u{2603}".as_bytes();
        let mut p = Pipeline::default();
        p.rx(&[&[b'a', snowman[0]]]);
        assert_eq!(p.buffer.partial_text(), "a", "incomplete tail is held back");
        p.rx(&[&snowman[1..]]);
        assert_eq!(p.buffer.partial_text(), "a\u{2603}");
        p.rx(&[b"\n"]);
        assert_eq!(texts(&p.buffer), ["a\u{2603}"]);
    }

    #[test]
    fn caps_the_number_of_rows_and_counts_evictions() {
        let mut p = Pipeline::with_cap(3);
        p.rx(&[b"1\n2\n3\n4\n5"]);
        assert_eq!(texts(&p.buffer), ["3", "4", "5"]);
        assert_eq!(p.buffer.evicted(), 2);
        p.rx(&[b"\n"]);
        assert_eq!(texts(&p.buffer), ["3", "4", "5"]);
        p.buffer.push_line(LineKind::Tx, "sent");
        assert_eq!(texts(&p.buffer), ["4", "5", "sent"]);
        assert_eq!(p.buffer.evicted(), 3);
    }

    #[test]
    fn default_cap_is_ten_thousand_lines() {
        let mut p = Pipeline::default();
        let input: String = (0..10_050).map(|i| format!("{i}\n")).collect();
        p.rx(&[input.as_bytes()]);
        assert_eq!(p.buffer.len(), DEFAULT_MAX_LINES);
        assert_eq!(p.buffer.row(0).unwrap().text, "50");
        assert_eq!(p.buffer.row(9_999).unwrap().text, "10049");
        assert_eq!(p.buffer.evicted(), 50);
    }

    #[test]
    fn a_sent_line_closes_the_partial_line_and_the_rest_continues_below() {
        let mut p = Pipeline::default();
        p.rx(&[b"prompt> "]);
        p.buffer.push_line(LineKind::Tx, "AT");
        p.rx(&[b"AT"]);
        p.buffer.push_line(LineKind::Tx, "ATI");
        p.rx(&[b"\r\nOK\r\n"]);
        let rows: Vec<_> = p.buffer.rows().map(|r| (r.kind, r.text)).collect();
        assert_eq!(
            rows,
            [
                (LineKind::Rx, "prompt> "),
                (LineKind::Tx, "AT"),
                (LineKind::Rx, "AT"),
                (LineKind::Tx, "ATI"),
                (LineKind::Rx, "OK"),
            ]
        );
    }

    #[test]
    fn a_frozen_line_whose_newline_arrives_later_adds_no_blank_row() {
        let mut p = Pipeline::default();
        p.rx(&[b"> "]);
        p.buffer.push_line(LineKind::Info, "note");
        p.rx(&[b"\nnext\n"]);
        assert_eq!(texts(&p.buffer), ["> ", "note", "next"]);
    }

    #[test]
    fn clear_forgets_the_start_of_an_interrupted_line() {
        let mut p = Pipeline::default();
        p.rx(&[b"a\nb"]);
        p.buffer.clear();
        assert!(p.buffer.is_empty());
        assert_eq!(p.buffer.evicted(), 0);
        p.rx(&[b"c\nd\n"]);
        assert_eq!(texts(&p.buffer), ["c", "d"]);
    }

    #[test]
    fn long_lines_wrap_on_a_character_boundary() {
        let mut p = Pipeline::default();
        // 'é' is two bytes, so MAX_LINE_BYTES lands in the middle of one when the line
        // starts with a single ASCII byte.
        let mut line = String::from("x");
        while line.len() < MAX_LINE_BYTES + 10 {
            line.push('é');
        }
        p.rx(&[line.as_bytes()]);
        let first = p.buffer.row(0).unwrap().text;
        assert!(first.len() <= MAX_LINE_BYTES);
        assert!(!first.contains('\u{FFFD}'));
        assert_eq!(concatenated(&p.buffer).replace('\n', ""), line);
    }

    /// Split `bytes` at the given positions (taken modulo the length, so any byte
    /// offset, including the middle of a UTF-8 sequence, can be a chunk boundary), and
    /// group the chunks into batches of `per_batch`.
    fn batches(bytes: &[u8], cuts: &[usize], per_batch: usize) -> Vec<Vec<Vec<u8>>> {
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let mut chunks = Vec::new();
        let mut start = 0;
        for cut in cuts {
            chunks.push(bytes[start..cut].to_vec());
            start = cut;
        }
        chunks.push(bytes[start..].to_vec());
        chunks
            .chunks(per_batch.max(1))
            .map(|batch| batch.to_vec())
            .collect()
    }

    fn feed(p: &mut Pipeline, batches: &[Vec<Vec<u8>>]) {
        for batch in batches {
            let rx = p.splitter.split(batch.iter().map(Vec::as_slice));
            p.buffer.apply_rx(rx);
        }
    }

    proptest! {
        #[test]
        fn chunking_never_changes_the_text(
            text in "[a-z\u{e9}\u{2603}\u{1F980} \r\n]{0,400}",
            cuts in proptest::collection::vec(any::<usize>(), 0..20),
            per_batch in 1usize..5,
        ) {
            let mut p = Pipeline::with_cap(usize::MAX);
            feed(&mut p, &batches(text.as_bytes(), &cuts, per_batch));
            prop_assert_eq!(concatenated(&p.buffer), text.replace('\r', ""));
        }

        #[test]
        fn arbitrary_bytes_decode_like_one_lossy_pass(
            mut bytes in proptest::collection::vec(any::<u8>(), 0..400),
            cuts in proptest::collection::vec(any::<usize>(), 0..20),
            per_batch in 1usize..5,
        ) {
            // A trailing newline completes every line, so the comparison does not depend
            // on how the in-progress line holds back an unfinished sequence.
            bytes.push(b'\n');
            let mut p = Pipeline::with_cap(usize::MAX);
            feed(&mut p, &batches(&bytes, &cuts, per_batch));
            let without_cr: Vec<u8> = bytes.iter().copied().filter(|&b| b != b'\r').collect();
            prop_assert_eq!(concatenated(&p.buffer), String::from_utf8_lossy(&without_cr));
        }

        #[test]
        fn the_cap_keeps_the_newest_rows(
            text in "[ab\n]{0,300}",
            cap in 1usize..20,
            cuts in proptest::collection::vec(any::<usize>(), 0..10),
            per_batch in 1usize..5,
        ) {
            let batches = batches(text.as_bytes(), &cuts, per_batch);
            let mut capped = Pipeline::with_cap(cap);
            let mut full = Pipeline::with_cap(usize::MAX);
            feed(&mut capped, &batches);
            feed(&mut full, &batches);
            let expected: Vec<&str> = texts(&full.buffer);
            let keep = expected.len().min(cap);
            prop_assert!(capped.buffer.len() <= cap);
            prop_assert_eq!(texts(&capped.buffer), expected[expected.len() - keep..].to_vec());
        }

        #[test]
        fn sent_lines_never_duplicate_or_lose_received_text(
            text in "[ab\r\n]{0,200}",
            cuts in proptest::collection::vec(any::<usize>(), 0..10),
            sends in proptest::collection::vec(any::<bool>(), 0..10),
        ) {
            // Interleave a sent line after some batches. A send may break a received line
            // in two, but the received characters, ignoring where rows break, must still
            // be the input's, each exactly once.
            let batches = batches(text.as_bytes(), &cuts, 1);
            let mut p = Pipeline::with_cap(usize::MAX);
            for (ix, batch) in batches.iter().enumerate() {
                let rx = p.splitter.split(batch.iter().map(Vec::as_slice));
                p.buffer.apply_rx(rx);
                if sends.get(ix).copied().unwrap_or(false) {
                    p.buffer.push_line(LineKind::Tx, "sent");
                }
            }
            let expected: String = text.chars().filter(|c| *c != '\r' && *c != '\n').collect();
            let mut shown: String = p
                .buffer
                .complete_lines()
                .filter(|l| l.kind == LineKind::Rx)
                .map(|l| l.text.as_str())
                .collect();
            shown.push_str(p.buffer.partial_text());
            prop_assert_eq!(shown, expected);
        }
    }
}
