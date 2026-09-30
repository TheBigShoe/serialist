//! In-memory [`LineSource`] and [`Searcher`] doubles.
//!
//! The terminal element is built and tested against these until the page store lands:
//! [`MemoryLines`] holds real lines and can evict, [`SyntheticLines`] generates any
//! number of lines on demand (a million costs nothing), and [`HexLines`] is a hex dump
//! of raw bytes standing in for the store's hex view. Each counts the lines it hands
//! out, which is how tests check that a frame fetches only what is on screen.
//!
//! Their searcher is a real regex search over line text, run line by line with the
//! cancel flag polled, so the search bar sees the same behaviour the store will give.

use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serialist_core::{
    Color, Direction, Epoch, LineId, LineSource, SearchMatch, Searcher, Style, StyleFlags,
    StyleRun, StyledLine,
};

/// Lines handed out, for tests that bound per-frame fetches.
#[derive(Debug, Default)]
pub struct FetchCounter(AtomicU64);

impl FetchCounter {
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn reset(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }

    fn add(&self, n: usize) {
        self.0.fetch_add(n as u64, Ordering::Relaxed);
    }
}

/// A run of `len` bytes in the default style.
pub fn plain_runs(len: usize) -> Vec<StyleRun> {
    if len == 0 {
        return Vec::new();
    }
    vec![StyleRun {
        len,
        style: Style::default(),
    }]
}

/// A run in the given foreground color and flags.
pub fn run(len: usize, fg: Color, flags: StyleFlags) -> StyleRun {
    StyleRun {
        len,
        style: Style {
            fg,
            bg: Color::Default,
            flags,
        },
    }
}

struct MemoryInner {
    first: LineId,
    lines: VecDeque<StyledLine>,
    raw_end: u64,
    capacity: Option<usize>,
}

/// Lines kept in memory, appended and evicted by hand or by a capacity.
pub struct MemoryLines {
    inner: RwLock<MemoryInner>,
    epoch: Epoch,
    fetched: FetchCounter,
}

impl Default for MemoryLines {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryLines {
    pub fn new() -> Self {
        Self::with_epoch(Epoch::now())
    }

    pub fn with_epoch(epoch: Epoch) -> Self {
        Self {
            inner: RwLock::new(MemoryInner {
                first: LineId::ZERO,
                lines: VecDeque::new(),
                raw_end: 0,
                capacity: None,
            }),
            epoch,
            fetched: FetchCounter::default(),
        }
    }

    /// Keep at most `capacity` lines, evicting the oldest.
    pub fn with_capacity(self, capacity: usize) -> Self {
        self.inner.write().capacity = Some(capacity.max(1));
        self
    }

    pub fn fetched(&self) -> &FetchCounter {
        &self.fetched
    }

    /// Append a received line in the default style, arriving now.
    pub fn push(&self, text: &str) -> LineId {
        self.push_styled(text, plain_runs(text.len()), Direction::Rx, Instant::now())
    }

    /// Append a line with its runs, direction and arrival time.
    pub fn push_styled(
        &self,
        text: &str,
        runs: Vec<StyleRun>,
        direction: Direction,
        received_at: Instant,
    ) -> LineId {
        let mut inner = self.inner.write();
        let id = inner.first.offset(inner.lines.len());
        let raw = match direction {
            Direction::Rx => {
                let start = inner.raw_end;
                inner.raw_end += text.len() as u64 + 1;
                start..inner.raw_end
            }
            Direction::Tx | Direction::Notice => inner.raw_end..inner.raw_end,
        };
        inner.lines.push_back(StyledLine {
            id,
            text: text.to_owned(),
            runs,
            direction,
            received_at,
            raw,
            complete: true,
        });
        if let Some(capacity) = inner.capacity {
            let excess = inner.lines.len().saturating_sub(capacity);
            drop(inner);
            self.evict(excess);
        }
        id
    }

    /// Drop the `n` oldest lines, as the store does when it reaches its cap.
    pub fn evict(&self, n: usize) {
        let mut inner = self.inner.write();
        let n = n.min(inner.lines.len());
        inner.lines.drain(..n);
        inner.first = inner.first.offset(n);
    }
}

impl LineSource for MemoryLines {
    fn first_line(&self) -> LineId {
        self.inner.read().first
    }

    fn line_count(&self) -> usize {
        self.inner.read().lines.len()
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        let inner = self.inner.read();
        let index = id.0.checked_sub(inner.first.0)? as usize;
        let line = inner.lines.get(index).cloned();
        self.fetched.add(usize::from(line.is_some()));
        line
    }

    fn lines(&self, range: Range<LineId>, out: &mut Vec<StyledLine>) {
        let inner = self.inner.read();
        let end = inner.first.offset(inner.lines.len());
        let start = range.start.max(inner.first);
        let stop = range.end.min(end);
        if start >= stop {
            return;
        }
        let from = (start.0 - inner.first.0) as usize;
        let to = (stop.0 - inner.first.0) as usize;
        out.extend(inner.lines.range(from..to).cloned());
        self.fetched.add(to - from);
    }

    fn epoch(&self) -> Epoch {
        self.epoch
    }
}

impl Searcher for MemoryLines {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        let (first, end) = {
            let inner = self.inner.read();
            (inner.first, inner.first.offset(inner.lines.len()))
        };
        search_lines(pattern, first..end, from, backward, limit, cancel, |id| {
            let inner = self.inner.read();
            let index = id.0.checked_sub(inner.first.0)? as usize;
            inner.lines.get(index).map(|line| line.text.clone())
        })
    }
}

/// Regex search over `text_of` for each line in `span`, starting at `from`.
fn search_lines(
    pattern: &str,
    span: Range<LineId>,
    from: LineId,
    backward: bool,
    limit: usize,
    cancel: &AtomicBool,
    text_of: impl Fn(LineId) -> Option<String>,
) -> Result<Vec<SearchMatch>, String> {
    let regex = regex::Regex::new(pattern).map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    if span.start >= span.end || limit == 0 {
        return Ok(out);
    }
    let from = from.max(span.start).min(LineId(span.end.0 - 1));
    let visit = |id: LineId, out: &mut Vec<SearchMatch>| -> bool {
        if let Some(text) = text_of(id) {
            let mut found: Vec<SearchMatch> = regex
                .find_iter(&text)
                .filter(|m| !m.range().is_empty())
                .map(|m| SearchMatch {
                    line: id,
                    range: m.range(),
                })
                .collect();
            if backward {
                found.reverse();
            }
            for m in found {
                out.push(m);
                if out.len() >= limit {
                    return false;
                }
            }
        }
        true
    };
    // Poll the flag every 1024 lines, starting before the first.
    let cancelled = |visited: usize| visited.is_multiple_of(1024) && cancel.load(Ordering::Relaxed);
    let mut id = from;
    let mut visited = 0;
    if backward {
        while !cancelled(visited) && visit(id, &mut out) && id > span.start {
            id = LineId(id.0 - 1);
            visited += 1;
        }
    } else {
        while id < span.end && !cancelled(visited) && visit(id, &mut out) {
            id = id.next();
            visited += 1;
        }
    }
    Ok(out)
}

/// Any number of generated lines, O(1) per line and no memory per line. Line `n` is
/// always the same text, so a test can predict what is on screen. Arrival times step
/// by a millisecond per line from the epoch.
pub struct SyntheticLines {
    first: AtomicU64,
    end: AtomicU64,
    epoch: Epoch,
    fetched: FetchCounter,
}

impl SyntheticLines {
    pub fn new(count: u64) -> Self {
        Self {
            first: AtomicU64::new(0),
            end: AtomicU64::new(count),
            epoch: Epoch::now(),
            fetched: FetchCounter::default(),
        }
    }

    pub fn fetched(&self) -> &FetchCounter {
        &self.fetched
    }

    /// Append `n` more lines.
    pub fn grow(&self, n: u64) {
        self.end.fetch_add(n, Ordering::Relaxed);
    }

    /// Evict the `n` oldest lines.
    pub fn evict(&self, n: u64) {
        let end = self.end.load(Ordering::Relaxed);
        let first = self.first.load(Ordering::Relaxed);
        self.first.store((first + n).min(end), Ordering::Relaxed);
    }

    /// The text of line `id`: a sequence number, a level, a sensor reading and a
    /// payload whose length varies from 20 to 160 characters, so wrapping is exercised.
    pub fn text_of(id: LineId) -> String {
        let n = id.0;
        let level = ["INFO", "WARN", "DBG ", "ERR "][(n % 4) as usize];
        let payload_len = 20 + (n * 7919 % 141) as usize;
        let payload: String = (0..payload_len)
            .map(|i| (b'a' + ((n as usize + i * 13) % 26) as u8) as char)
            .collect();
        format!(
            "{n:08} {level} sensor={:03} t={:>3}.{} {payload}",
            n % 997,
            n % 120,
            n % 10
        )
    }

    fn styled(&self, id: LineId) -> StyledLine {
        let text = Self::text_of(id);
        let level_color = match id.0 % 4 {
            0 => Color::Ansi(2),
            1 => Color::Ansi(3),
            2 => Color::Indexed(244),
            _ => Color::Ansi(1),
        };
        let runs = vec![
            run(9, Color::Default, StyleFlags::DIM),
            run(4, level_color, StyleFlags::BOLD),
            run(12, Color::Rgb(0x61, 0xaf, 0xef), StyleFlags::NONE),
            run(text.len() - 25, Color::Default, StyleFlags::NONE),
        ];
        StyledLine {
            id,
            runs,
            direction: Direction::Rx,
            received_at: self.epoch.instant + Duration::from_millis(id.0),
            raw: 0..0,
            complete: true,
            text,
        }
    }
}

impl LineSource for SyntheticLines {
    fn first_line(&self) -> LineId {
        LineId(self.first.load(Ordering::Relaxed))
    }

    fn line_count(&self) -> usize {
        (self.end.load(Ordering::Relaxed) - self.first.load(Ordering::Relaxed)) as usize
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        if id < self.first_line() || id >= self.end() {
            return None;
        }
        self.fetched.add(1);
        Some(self.styled(id))
    }

    fn epoch(&self) -> Epoch {
        self.epoch
    }
}

impl Searcher for SyntheticLines {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        search_lines(
            pattern,
            self.first_line()..self.end(),
            from,
            backward,
            limit,
            cancel,
            |id| Some(Self::text_of(id)),
        )
    }
}

/// A hex dump of raw bytes, sixteen per line, `00000010  41 54 0d 0a …  |AT..|`: what
/// the store's hex view will provide, here so the toggle can be built and tested.
pub struct HexLines {
    bytes: RwLock<Vec<u8>>,
    epoch: Epoch,
    fetched: FetchCounter,
}

impl HexLines {
    pub const BYTES_PER_LINE: usize = 16;

    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: RwLock::new(bytes),
            epoch: Epoch::now(),
            fetched: FetchCounter::default(),
        }
    }

    pub fn fetched(&self) -> &FetchCounter {
        &self.fetched
    }

    pub fn extend(&self, more: &[u8]) {
        self.bytes.write().extend_from_slice(more);
    }

    /// The dump line for `chunk` at byte `offset`.
    pub fn format(offset: usize, chunk: &[u8]) -> String {
        let mut hex = String::with_capacity(3 * Self::BYTES_PER_LINE + 1);
        for i in 0..Self::BYTES_PER_LINE {
            if i == 8 {
                hex.push(' ');
            }
            match chunk.get(i) {
                Some(byte) => hex.push_str(&format!("{byte:02x} ")),
                None => hex.push_str("   "),
            }
        }
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        format!("{offset:08x}  {hex} |{ascii}|")
    }
}

impl LineSource for HexLines {
    fn first_line(&self) -> LineId {
        LineId::ZERO
    }

    fn line_count(&self) -> usize {
        self.bytes.read().len().div_ceil(Self::BYTES_PER_LINE)
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        let bytes = self.bytes.read();
        let start = (id.0 as usize).checked_mul(Self::BYTES_PER_LINE)?;
        if start >= bytes.len() {
            return None;
        }
        let end = (start + Self::BYTES_PER_LINE).min(bytes.len());
        let text = Self::format(start, &bytes[start..end]);
        self.fetched.add(1);
        let runs = vec![
            run(10, Color::Default, StyleFlags::DIM),
            run(text.len() - 10, Color::Default, StyleFlags::NONE),
        ];
        Some(StyledLine {
            id,
            text,
            runs,
            direction: Direction::Rx,
            received_at: self.epoch.instant,
            raw: start as u64..end as u64,
            complete: end - start == Self::BYTES_PER_LINE,
        })
    }

    fn epoch(&self) -> Epoch {
        self.epoch
    }
}

/// Convenience for tests and the demo: shared handles to one double as the element's
/// source and searcher.
pub fn shared<T: LineSource + Searcher + 'static>(
    double: T,
) -> (Arc<T>, Arc<dyn LineSource>, Arc<dyn Searcher>) {
    let double = Arc::new(double);
    (double.clone(), double.clone(), double)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_lines_append_evict_and_count_fetches() {
        let lines = MemoryLines::new().with_capacity(3);
        for text in ["a", "b", "c", "d"] {
            lines.push(text);
        }
        assert_eq!(lines.first_line(), LineId(1));
        assert_eq!(lines.end(), LineId(4));
        assert_eq!(lines.line(LineId(0)), None, "evicted");
        let mut out = Vec::new();
        lines.fetched().reset();
        lines.lines(LineId(0)..LineId(10), &mut out);
        let texts: Vec<_> = out.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["b", "c", "d"]);
        assert_eq!(lines.fetched().get(), 3);
        assert_eq!(out[0].id, LineId(1));
    }

    #[test]
    fn synthetic_lines_are_stable_and_styled_exactly() {
        let lines = SyntheticLines::new(1_000_000);
        let a = lines.line(LineId(123_456)).unwrap();
        let b = lines.line(LineId(123_456)).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.runs.iter().map(|r| r.len).sum::<usize>(), a.text.len());
        assert!(a.text.starts_with("00123456 "));
        assert_eq!(lines.line(LineId(1_000_000)), None);
        lines.evict(10);
        assert_eq!(lines.first_line(), LineId(10));
        assert_eq!(lines.line(LineId(9)), None);
        for id in 0..500 {
            let line = lines.styled(LineId(id));
            assert_eq!(
                line.runs.iter().map(|r| r.len).sum::<usize>(),
                line.text.len()
            );
        }
    }

    #[test]
    fn hex_lines_dump_sixteen_bytes_a_line() {
        let hex = HexLines::new(b"AT\r\nOK\r\n0123456789".to_vec());
        assert_eq!(hex.line_count(), 2);
        let first = hex.line(LineId(0)).unwrap();
        assert_eq!(
            first.text,
            "00000000  41 54 0d 0a 4f 4b 0d 0a  30 31 32 33 34 35 36 37  |AT..OK..01234567|"
        );
        assert!(first.complete);
        let second = hex.line(LineId(1)).unwrap();
        assert!(second.text.starts_with("00000010  38 39 "));
        assert!(second.text.ends_with("|89|"));
        assert!(!second.complete);
        assert_eq!(
            first.runs.iter().map(|r| r.len).sum::<usize>(),
            first.text.len()
        );
    }

    #[test]
    fn search_forward_backward_limit_and_cancel() {
        let lines = MemoryLines::new();
        for text in ["ok", "error 1", "ok", "error 2 error 3", "ok"] {
            lines.push(text);
        }
        let never = AtomicBool::new(false);
        let found = lines.search("error", LineId(0), false, 10, &never).unwrap();
        let at: Vec<_> = found.iter().map(|m| (m.line.0, m.range.start)).collect();
        assert_eq!(at, [(1, 0), (3, 0), (3, 8)]);
        let back = lines.search("error", LineId(4), true, 2, &never).unwrap();
        let at: Vec<_> = back.iter().map(|m| (m.line.0, m.range.start)).collect();
        assert_eq!(at, [(3, 8), (3, 0)]);
        assert!(lines.search("(", LineId(0), false, 10, &never).is_err());
        let cancelled = AtomicBool::new(true);
        assert!(
            lines
                .search("error", LineId(0), false, 10, &cancelled)
                .unwrap()
                .is_empty()
        );
    }
}
