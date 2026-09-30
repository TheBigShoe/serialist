//! The session's scrollback as the terminal sees it: a store [`Snapshot`] (text) and
//! its [`HexView`] (hex rows), each behind a floor that Clear raises.
//!
//! The store has no way to forget lines on request, and should not: its raw pages are
//! also the record raw export reads. Clear is therefore a view concern like pause: the
//! floor hides every line (or hex row) below it, and the store evicts them when its
//! budget says so. A floor of zero hides nothing.
//!
//! [`Scrollback`] bundles both sources for one snapshot and is what the session view
//! hands the terminal after every wake.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serialist_core::{
    Epoch, HexView, LineId, LineSource, SearchMatch, Searcher, Snapshot, StyledLine,
};

/// Bytes per hex row.
pub const HEX_BYTES_PER_ROW: usize = 16;

/// A source with everything below `floor` hidden.
#[derive(Clone, Debug)]
pub struct Floored<S> {
    pub inner: S,
    pub floor: LineId,
}

impl<S: LineSource> Floored<S> {
    pub fn new(inner: S, floor: LineId) -> Self {
        Self { inner, floor }
    }
}

impl<S: LineSource> LineSource for Floored<S> {
    fn first_line(&self) -> LineId {
        self.inner
            .first_line()
            .max(self.floor)
            .min(self.inner.end())
    }

    fn line_count(&self) -> usize {
        (self.end().0 - self.first_line().0) as usize
    }

    fn end(&self) -> LineId {
        self.inner.end()
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        if id < self.floor {
            return None;
        }
        self.inner.line(id)
    }

    fn lines(&self, range: std::ops::Range<LineId>, out: &mut Vec<StyledLine>) {
        self.inner
            .lines(range.start.max(self.floor)..range.end, out);
    }

    fn epoch(&self) -> Epoch {
        self.inner.epoch()
    }
}

/// The store's own search (bulk regex over pages), bounded to the lines from the floor
/// up with [`Snapshot::search_in`]: nothing below the floor is returned or scanned, so a
/// backward search from the newest line costs the lines it shows, however much the store
/// still holds below the floor. A `from` below the floor starts at it.
impl Searcher for Floored<Snapshot> {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        self.inner.search_in(
            pattern,
            self.floor..self.end(),
            from.max(self.floor),
            backward,
            limit,
            cancel,
        )
    }
}

/// The hex rows' own search, [`HexView::search_in`] bounded to the rows from the floor
/// up, with the store's smart-case rule. A pattern can match the hex column (`0d 0a`) or
/// the ASCII column (`OK`), within one row. A `from` outside the rows shown is moved to
/// the nearest one, so it always searches from a row; a match that is empty (a pattern
/// like `x*`) is dropped, as it highlights nothing.
impl Searcher for Floored<HexView> {
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        let (first, end) = (self.first_line(), self.end());
        let from = if first < end {
            from.max(first).min(LineId(end.0 - 1))
        } else {
            from
        };
        let mut found = self
            .inner
            .search_in(pattern, first..end, from, backward, limit, cancel)?;
        found.retain(|m| !m.range.is_empty());
        Ok(found)
    }
}

/// Where Clear put the floors: a text line id, and the stream offset the hex rows are
/// hidden below. The hex floor is kept in bytes so it survives a change of row width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floors {
    pub text: LineId,
    pub raw: u64,
}

impl Default for Floors {
    /// Nothing hidden.
    fn default() -> Self {
        Self {
            text: LineId::ZERO,
            raw: 0,
        }
    }
}

impl Floors {
    /// Floors that hide everything `snapshot` holds: its lines and its bytes.
    pub fn above(snapshot: &Snapshot) -> Self {
        Self {
            text: snapshot.end(),
            raw: snapshot.raw_range().end,
        }
    }

    /// The first hex row shown at `bytes_per_row`: every row that starts before the
    /// clear is hidden, except a row the clear cut through, so no byte after the clear
    /// is ever hidden.
    pub fn hex_row(&self, bytes_per_row: usize) -> LineId {
        LineId(self.raw / bytes_per_row.max(1) as u64)
    }
}

/// One snapshot as the terminal's text and hex sources and searchers.
#[derive(Clone)]
pub struct Scrollback {
    pub text: Arc<Floored<Snapshot>>,
    pub hex: Arc<Floored<HexView>>,
}

impl Scrollback {
    /// Hex rows of [`HEX_BYTES_PER_ROW`] bytes.
    pub fn new(snapshot: &Snapshot, floors: Floors) -> Self {
        Self::with_hex_row(snapshot, floors, HEX_BYTES_PER_ROW)
    }

    /// Hex rows of `bytes_per_row` bytes (clamped to 1..=256 by the store).
    pub fn with_hex_row(snapshot: &Snapshot, floors: Floors, bytes_per_row: usize) -> Self {
        let hex = snapshot.hex_view(bytes_per_row);
        let hex_floor = floors.hex_row(hex.bytes_per_row());
        Self {
            text: Arc::new(Floored::new(snapshot.clone(), floors.text)),
            hex: Arc::new(Floored::new(hex, hex_floor)),
        }
    }

    pub fn hex_bytes_per_row(&self) -> usize {
        self.hex.inner.bytes_per_row()
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.text.inner
    }

    pub fn text_source(&self) -> Arc<dyn LineSource> {
        self.text.clone()
    }

    pub fn text_searcher(&self) -> Arc<dyn Searcher> {
        self.text.clone()
    }

    pub fn hex_source(&self) -> Arc<dyn LineSource> {
        self.hex.clone()
    }

    pub fn hex_searcher(&self) -> Arc<dyn Searcher> {
        self.hex.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use regex::RegexBuilder;
    use serialist_core::store::smart_case_insensitive;
    use serialist_core::{Direction, Store};

    use super::*;

    fn store_with(lines: &[&str]) -> Store {
        let mut store = Store::default();
        for line in lines {
            store.append(format!("{line}\r\n").as_bytes(), Instant::now());
        }
        store
    }

    fn texts(source: &dyn LineSource) -> Vec<String> {
        let mut out = Vec::new();
        source.lines(source.first_line()..source.end(), &mut out);
        out.into_iter().map(|line| line.text).collect()
    }

    fn search(searcher: &dyn Searcher, pattern: &str, backward: bool) -> Vec<(u64, usize)> {
        let from = if backward {
            LineId(u64::MAX)
        } else {
            LineId(0)
        };
        searcher
            .search(pattern, from, backward, 100, &AtomicBool::new(false))
            .unwrap()
            .into_iter()
            .map(|m| (m.line.0, m.range.start))
            .collect()
    }

    #[test]
    fn a_floor_hides_the_lines_below_it_and_their_matches() {
        let mut store = store_with(&["error one", "fine", "error two"]);
        let floors = Floors::above(&store.snapshot());
        assert_eq!(floors.text, LineId(3));
        store.append(b"error three\r\n", Instant::now());
        store.append_local("sent error", Direction::Tx);

        let scrollback = Scrollback::new(&store.snapshot(), floors);
        let text = scrollback.text_source();
        assert_eq!(text.first_line(), LineId(3));
        assert_eq!(text.line_count(), 2);
        assert_eq!(texts(text.as_ref()), ["error three", "sent error"]);
        assert_eq!(text.line(LineId(0)), None);

        let searcher = scrollback.text_searcher();
        assert_eq!(search(searcher.as_ref(), "error", true), [(4, 5), (3, 0)]);
        assert_eq!(search(searcher.as_ref(), "error", false), [(3, 0), (4, 5)]);

        let unfloored = Scrollback::new(&store.snapshot(), Floors::default());
        assert_eq!(unfloored.text_source().line_count(), 5);
    }

    #[test]
    fn hex_rows_follow_the_raw_bytes_and_search_row_by_row() {
        let mut store = store_with(&["AT", "OK"]);
        store.append(&[0u8; 20], Instant::now());
        let scrollback = Scrollback::new(&store.snapshot(), Floors::default());
        let hex = scrollback.hex_source();
        // 8 bytes of text and 20 zeros: two rows.
        assert_eq!(hex.line_count(), 2);
        let row = hex.line(LineId(0)).unwrap();
        assert!(
            row.text
                .starts_with("00000000  41 54 0d 0a 4f 4b 0d 0a  00 00")
        );
        assert!(row.text.ends_with("|AT..OK..........|"));

        let searcher = scrollback.hex_searcher();
        assert_eq!(search(searcher.as_ref(), "ok", false), [(0, 65)]);
        assert_eq!(search(searcher.as_ref(), "0d 0a", true), [(0, 28), (0, 16)]);
        assert!(
            searcher
                .search("(", LineId(0), false, 10, &AtomicBool::new(false))
                .is_err()
        );

        // A clear hides the rows before the last byte, but not a row it cut through.
        let floors = Floors::above(&store.snapshot());
        assert_eq!(floors.hex_row(HEX_BYTES_PER_ROW), LineId(1));
        store.append(b"more", Instant::now());
        let cleared = Scrollback::new(&store.snapshot(), floors);
        assert_eq!(cleared.hex_source().first_line(), LineId(1));
        assert_eq!(search(cleared.hex_searcher().as_ref(), "41 54", false), []);

        // The same clear at eight bytes a row: 28 bytes cleared, so rows 0 to 2 go and
        // row 3, which the clear cut through, stays.
        let narrow = Scrollback::with_hex_row(&store.snapshot(), floors, 8);
        assert_eq!(narrow.hex_bytes_per_row(), 8);
        assert_eq!(narrow.hex_source().first_line(), LineId(3));
        assert_eq!(narrow.hex_source().line_count(), 1);
    }

    /// What the hex search did before the store had `HexView::search_in`: every row from
    /// `from` to the end (or back to the floor) rendered and searched by hand.
    fn hex_search_row_by_row(
        hex: &Floored<HexView>,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
    ) -> Result<Vec<SearchMatch>, String> {
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(smart_case_insensitive(pattern))
            .build()
            .map_err(|error| error.to_string())?;
        let (first, end) = (hex.first_line(), hex.end());
        let mut out = Vec::new();
        if first >= end || limit == 0 {
            return Ok(out);
        }
        let mut id = from.max(first).min(LineId(end.0 - 1));
        loop {
            if let Some(row) = hex.inner.line(id) {
                let mut found: Vec<SearchMatch> = regex
                    .find_iter(&row.text)
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
                        return Ok(out);
                    }
                }
            }
            if backward {
                if id <= first {
                    break;
                }
                id = LineId(id.0 - 1);
            } else {
                id = id.next();
                if id >= end {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// The text search as it was: the store's unbounded search from the floor, with what
    /// it found below the floor dropped.
    fn text_search_then_filter(
        text: &Floored<Snapshot>,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
    ) -> Result<Vec<SearchMatch>, String> {
        let cancel = AtomicBool::new(false);
        let from = from.max(text.floor);
        let mut found = text.inner.search(pattern, from, backward, limit, &cancel)?;
        found.retain(|m| m.line >= text.floor);
        Ok(found)
    }

    /// A store with text lines, a Clear after some raw bytes, and more lines after it.
    fn cleared_store() -> (Store, Floors) {
        let mut store = store_with(&[
            "Error: first",
            "ok",
            "error again",
            "AT+CMD=1",
            "OK",
            "error error",
        ]);
        store.append(&[0x0d, 0x0a, 0x00, 0x41, 0x54], Instant::now());
        let floors = Floors::above(&store.snapshot());
        for line in ["error after", "fine", "OK error", "the end"] {
            store.append(format!("{line}\r\n").as_bytes(), Instant::now());
        }
        store.append_local("sent error", Direction::Tx);
        (store, floors)
    }

    #[test]
    fn a_floored_search_finds_what_the_scan_and_filter_did() {
        let (store, floors) = cleared_store();
        let snapshot = store.snapshot();
        let scrollback = Scrollback::new(&snapshot, floors);
        let cancel = AtomicBool::new(false);
        let patterns = [
            "error", "Error", "ok", "OK", "e.*r", "x*", "^", "$", "(", "AT",
        ];
        let froms = [
            0,
            floors.text.0 - 1,
            floors.text.0,
            floors.text.0 + 2,
            snapshot.end().0,
            u64::MAX,
        ];
        for pattern in patterns {
            for backward in [false, true] {
                for limit in [0, 1, 3, 100] {
                    for from in froms {
                        let from = LineId(from);
                        let expected = text_search_then_filter(
                            &scrollback.text,
                            pattern,
                            from,
                            backward,
                            limit,
                        );
                        let got = scrollback
                            .text_searcher()
                            .search(pattern, from, backward, limit, &cancel);
                        assert_eq!(
                            got, expected,
                            "text {pattern:?} from {from:?} backward={backward} limit={limit}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_floored_hex_search_finds_what_the_row_by_row_search_did() {
        let (store, floors) = cleared_store();
        let snapshot = store.snapshot();
        let cancel = AtomicBool::new(false);
        let patterns = [
            "error",
            "0d 0a",
            "OK",
            "ok",
            "41 54",
            "x*",
            "^",
            "|",
            "(",
            "[0-9a-f]{2}",
        ];
        for bytes_per_row in [8, 16, 32] {
            let scrollback = Scrollback::with_hex_row(&snapshot, floors, bytes_per_row);
            let (first, end) = (scrollback.hex.first_line(), scrollback.hex.end());
            let froms = [
                0,
                first.0.saturating_sub(1),
                first.0,
                first.0 + 1,
                end.0 - 1,
                end.0,
                u64::MAX,
            ];
            for pattern in patterns {
                for backward in [false, true] {
                    for limit in [0, 1, 3, 1000] {
                        for from in froms {
                            let from = LineId(from);
                            let expected = hex_search_row_by_row(
                                &scrollback.hex,
                                pattern,
                                from,
                                backward,
                                limit,
                            );
                            let got = scrollback
                                .hex_searcher()
                                .search(pattern, from, backward, limit, &cancel);
                            assert_eq!(
                                got, expected,
                                "hex/{bytes_per_row} {pattern:?} from {from:?} \
                                 backward={backward} limit={limit}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_search_of_nothing_shown_still_rejects_a_bad_pattern() {
        // Everything is cleared, so no rows are shown; the pattern is still checked.
        // Sixteen bytes, so the clear lands on a row boundary and hides the only row.
        let store = store_with(&["AT", "OK", "AT+CMD"]);
        let snapshot = store.snapshot();
        let scrollback = Scrollback::new(&snapshot, Floors::above(&snapshot));
        assert_eq!(scrollback.hex.line_count(), 0);
        assert_eq!(scrollback.text.line_count(), 0);
        let cancel = AtomicBool::new(false);
        let hex = scrollback.hex_searcher();
        assert_eq!(hex.search("41", LineId(0), false, 10, &cancel), Ok(vec![]));
        assert!(hex.search("(", LineId(0), false, 10, &cancel).is_err());
        let text = scrollback.text_searcher();
        assert_eq!(text.search("AT", LineId(0), true, 10, &cancel), Ok(vec![]));
        assert!(text.search("(", LineId(0), true, 10, &cancel).is_err());
    }

    #[test]
    fn a_cancelled_backward_search_returns_what_it_has() {
        let (store, floors) = cleared_store();
        let scrollback = Scrollback::new(&store.snapshot(), floors);
        let cancelled = AtomicBool::new(true);
        let found = scrollback
            .text_searcher()
            .search("error", LineId(u64::MAX), true, 100, &cancelled)
            .unwrap();
        assert!(found.is_empty(), "a backward search checks the flag first");
        let found = scrollback
            .hex_searcher()
            .search("error", LineId(u64::MAX), true, 100, &cancelled)
            .unwrap();
        assert!(found.is_empty(), "so does a hex one");
    }
}
