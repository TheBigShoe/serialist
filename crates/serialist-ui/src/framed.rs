//! Hiding framed bytes: the text view without the received lines whose bytes all belong
//! to decoded binary frames (`display.hide_framed_bytes`).
//!
//! The terminal keeps its scroll anchor, selection, marks and search results as line
//! ids, so a view that leaves lines out needs ids of its own that stay put as lines
//! arrive and are evicted. A *view id* counts the visible lines since the filter started;
//! [`FramedFilter`] maps them to store ids as runs of consecutive visible store lines,
//! `(view id, store id, length)`, kept in blocks of [`BLOCK`] runs. A full block is sealed
//! and shared (`Arc`) by every [`FilteredText`] made after, so handing the terminal a new
//! source on each wake copies one open block and the lines not yet decided, never the
//! whole map. Lookups either way are two binary searches.
//!
//! # Deciding a line
//!
//! A line is hidden when it was received (sent lines and notices always show), has
//! bytes, and every byte lies in a frame that [`hides_bytes`](crate::codecs::hides_bytes)
//! allows to hide: a decoded binary frame, never a text frame or a codec failure. A codec
//! emits frames in stream order, so every byte before the newest frame's end has been
//! seen by it: a complete line that ends there is decided for good. The lines after it
//! (bytes the codec still holds back, and the lines still arriving) are decided again on
//! every update, and their view ids may shift while they are; a codec that has gone
//! quiet cannot keep lines undecided for more than [`SETTLE_LINES`] lines.
//!
//! Turning the filter on mid-session decides only the last [`WINDOW`] retained lines;
//! older ones stay visible, as one run.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use serialist_core::{
    Direction, Epoch, Frame, FrameId, FrameSnapshot, LineId, LineSource, SearchMatch, Searcher,
    Snapshot, StyledLine,
};

use crate::scrollback::Floored;

/// Runs per block.
pub const BLOCK: usize = 256;

/// Lines further back from the newest than this are decided as they stand, frames or
/// not.
pub const SETTLE_LINES: u64 = 4096;

/// Lines decided when the filter starts on a session that already holds some.
pub const WINDOW: u64 = 100_000;

/// Consecutive visible store lines and the view ids they have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Run {
    view: u64,
    store: u64,
    len: u64,
}

impl Run {
    fn view_end(&self) -> u64 {
        self.view + self.len
    }

    fn store_end(&self) -> u64 {
        self.store + self.len
    }
}

/// The map from view ids to store ids for one session; see the module docs.
#[derive(Debug)]
pub struct FramedFilter {
    /// Sealed blocks, oldest first.
    sealed: Arc<[Arc<[Run]>]>,
    /// The block being filled with decided runs.
    open: Vec<Run>,
    /// Store lines below this are decided.
    decided: u64,
    /// View id of the next visible decided line.
    next_view: u64,
    /// The first frame that may cover a line not decided yet.
    cursor: u64,
}

impl FramedFilter {
    /// A filter over `snapshot`'s lines: the last [`WINDOW`] of them are decided on the
    /// first [`update`](Self::update), the ones before stay visible.
    pub fn new(snapshot: &Snapshot) -> Self {
        let first = snapshot.first_line().0;
        let start = first.max(snapshot.end().0.saturating_sub(WINDOW));
        let mut filter = Self {
            sealed: Arc::new([]),
            open: Vec::new(),
            decided: first,
            next_view: 0,
            cursor: 0,
        };
        for id in first..start {
            filter.push_visible(id);
        }
        filter
    }

    fn push_visible(&mut self, store: u64) {
        match self.open.last_mut() {
            Some(run) if run.store_end() == store && run.view_end() == self.next_view => {
                run.len += 1;
            }
            _ => {
                if self.open.len() == BLOCK {
                    let block: Arc<[Run]> = Arc::from(std::mem::take(&mut self.open));
                    self.sealed = self.sealed.iter().cloned().chain([block]).collect();
                }
                self.open.push(Run {
                    view: self.next_view,
                    store,
                    len: 1,
                });
            }
        }
        self.next_view += 1;
    }

    /// Drop sealed blocks whose lines the store has evicted.
    fn forget_evicted(&mut self, first: u64) {
        let gone = self
            .sealed
            .iter()
            .take_while(|block| block.last().is_some_and(|run| run.store_end() <= first))
            .count();
        if gone > 0 {
            self.sealed = self.sealed[gone..].iter().cloned().collect();
        }
    }

    /// Decide what arrived since the last update and return the terminal's source for
    /// `text` (the floored snapshot the view shows). `hides` says which frames may hide
    /// their bytes.
    pub fn update(
        &mut self,
        text: &Arc<Floored<Snapshot>>,
        frames: &FrameSnapshot,
        hides: &dyn Fn(&Frame) -> bool,
    ) -> FilteredText {
        let snapshot = &text.inner;
        let first = snapshot.first_line().0;
        let end = snapshot.end().0;
        let committed = snapshot.committed_end().0;
        self.forget_evicted(first);
        self.decided = self.decided.max(first);
        self.cursor = self.cursor.max(frames.first().0);
        // Frames come out in stream order: the newest one's end is as far as the codec
        // has decided.
        let frontier = frames.last().map_or(0, |frame| frame.raw.end);

        let mut cover = Cover {
            frames,
            next: self.cursor,
        };
        while self.decided < committed {
            let id = self.decided;
            let Some(line) = snapshot.line(LineId(id)) else {
                self.decided += 1;
                continue;
            };
            let settled = line.direction != Direction::Rx
                || line.raw.end <= frontier
                || id + SETTLE_LINES <= end;
            if !settled {
                break;
            }
            if !cover.hides(&line, hides) {
                self.push_visible(id);
            }
            self.decided += 1;
        }
        self.cursor = cover.next;

        // The lines not decided yet, decided for now.
        let mut open = self.open.clone();
        let mut next_view = self.next_view;
        for id in self.decided..end {
            let Some(line) = snapshot.line(LineId(id)) else {
                continue;
            };
            if cover.hides(&line, hides) {
                continue;
            }
            match open.last_mut() {
                Some(run) if run.store_end() == id && run.view_end() == next_view => run.len += 1,
                _ => open.push(Run {
                    view: next_view,
                    store: id,
                    len: 1,
                }),
            }
            next_view += 1;
        }
        FilteredText::new(text.clone(), self.sealed.clone(), Arc::from(open))
    }
}

/// Walks the frames in stream order to tell whether a line's bytes are all covered.
struct Cover<'a> {
    frames: &'a FrameSnapshot,
    /// The first frame that may cover the next line.
    next: u64,
}

impl Cover<'_> {
    /// Whether `line` is hidden. Lines must come in stream order.
    fn hides(&mut self, line: &StyledLine, hides: &dyn Fn(&Frame) -> bool) -> bool {
        if line.direction != Direction::Rx || line.raw.is_empty() {
            return false;
        }
        self.covered(line.raw.clone(), hides)
    }

    fn covered(&mut self, raw: Range<u64>, hides: &dyn Fn(&Frame) -> bool) -> bool {
        // Frames that end before the line starts cover nothing from here on.
        while let Some(frame) = self.frames.get(FrameId(self.next)) {
            if frame.raw.end > raw.start {
                break;
            }
            self.next += 1;
        }
        let mut pos = raw.start;
        let mut id = self.next.max(self.frames.first().0);
        while let Some(frame) = self.frames.get(FrameId(id)) {
            id += 1;
            if frame.raw.is_empty() {
                continue;
            }
            if frame.raw.start > pos || !hides(frame) {
                return false;
            }
            pos = pos.max(frame.raw.end);
            if pos >= raw.end {
                return true;
            }
        }
        false
    }
}

/// The terminal's text source while framed bytes are hidden: the floored snapshot with
/// the hidden lines left out, addressed by view ids.
#[derive(Clone)]
pub struct FilteredText {
    inner: Arc<Floored<Snapshot>>,
    sealed: Arc<[Arc<[Run]>]>,
    open: Arc<[Run]>,
    first: u64,
    end: u64,
}

impl std::fmt::Debug for FilteredText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilteredText")
            .field("lines", &(self.first..self.end))
            .field("blocks", &self.sealed.len())
            .finish_non_exhaustive()
    }
}

impl FilteredText {
    fn new(inner: Arc<Floored<Snapshot>>, sealed: Arc<[Arc<[Run]>]>, open: Arc<[Run]>) -> Self {
        let mut text = Self {
            inner,
            sealed,
            open,
            first: 0,
            end: 0,
        };
        let last_view = (0..text.blocks())
            .rev()
            .find_map(|i| text.block(i).last())
            .map_or(0, Run::view_end);
        text.first = text
            .view_at_or_after(text.inner.first_line())
            .0
            .min(last_view);
        text.end = last_view.max(text.first);
        text
    }

    fn blocks(&self) -> usize {
        self.sealed.len() + 1
    }

    fn block(&self, i: usize) -> &[Run] {
        if i < self.sealed.len() {
            &self.sealed[i]
        } else {
            &self.open
        }
    }

    /// The last run whose `key` is at most `x`.
    fn floor(&self, key: impl Fn(&Run) -> u64, x: u64) -> Option<Run> {
        let (mut lo, mut hi) = (0, self.blocks());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match self.block(mid).first() {
                Some(run) if key(run) <= x => lo = mid + 1,
                _ => hi = mid,
            }
        }
        let block = self.block(lo.checked_sub(1)?);
        let at = block.partition_point(|run| key(run) <= x);
        block.get(at.checked_sub(1)?).copied()
    }

    fn first_run(&self) -> Option<Run> {
        (0..self.blocks()).find_map(|i| self.block(i).first().copied())
    }

    /// The view id of store line `store`, if it is shown.
    pub fn view_of(&self, store: LineId) -> Option<LineId> {
        let run = self.floor(|run| run.store, store.0)?;
        (store.0 < run.store_end()).then(|| LineId(run.view + (store.0 - run.store)))
    }

    /// The view id of the first shown line at or after store line `store`.
    pub fn view_at_or_after(&self, store: LineId) -> LineId {
        match self.floor(|run| run.store, store.0) {
            Some(run) if store.0 < run.store_end() => LineId(run.view + (store.0 - run.store)),
            Some(run) => LineId(run.view_end()),
            None => LineId(self.first_run().map_or(0, |run| run.view)),
        }
    }

    /// The store id of view line `view`.
    pub fn store_of(&self, view: LineId) -> Option<LineId> {
        let run = self.floor(|run| run.view, view.0)?;
        (view.0 < run.view_end()).then(|| LineId(run.store + (view.0 - run.view)))
    }

    /// The floored snapshot under it.
    pub fn inner(&self) -> &Arc<Floored<Snapshot>> {
        &self.inner
    }
}

impl LineSource for FilteredText {
    fn first_line(&self) -> LineId {
        LineId(self.first)
    }

    fn line_count(&self) -> usize {
        (self.end - self.first) as usize
    }

    fn end(&self) -> LineId {
        LineId(self.end)
    }

    fn line(&self, id: LineId) -> Option<StyledLine> {
        if id.0 < self.first || id.0 >= self.end {
            return None;
        }
        let mut line = self.inner.line(self.store_of(id)?)?;
        line.id = id;
        Some(line)
    }

    fn epoch(&self) -> Epoch {
        self.inner.epoch()
    }
}

impl Searcher for FilteredText {
    /// The store's search from the store line under `from`, with the matches on hidden
    /// lines dropped and the rest given their view ids.
    fn search(
        &self,
        pattern: &str,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        let from = if from.0 >= self.end {
            self.inner.end()
        } else if from.0 < self.first {
            self.inner.first_line()
        } else {
            self.store_of(from).unwrap_or(from)
        };
        let found = self.inner.search(pattern, from, backward, limit, cancel)?;
        Ok(found
            .into_iter()
            .filter_map(|found| {
                Some(SearchMatch {
                    line: self.view_of(found.line)?,
                    range: found.range,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serialist_core::{Codec, FrameStore, Store};
    use serialist_plugins::AirohaRace;
    use serialist_plugins::race::{RaceType, encode_frame, race_info};

    use super::*;
    use crate::codecs::hides_bytes;

    /// A store and its frames, decoded by the RACE codec chunk by chunk.
    struct Rig {
        store: Store,
        frames: FrameStore,
        race: AirohaRace,
        offset: u64,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                store: Store::default(),
                frames: FrameStore::default(),
                race: AirohaRace::new(),
                offset: 0,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let at = Instant::now();
            self.store.append(bytes, at);
            let mut out = Vec::new();
            self.race.decode(bytes, at, self.offset, &mut out);
            self.frames.extend(out);
            self.offset += bytes.len() as u64;
        }

        fn log(&mut self, text: &str) {
            self.feed(&encode_frame(RaceType::Log, 0x0F40, text.as_bytes()).unwrap());
        }

        fn source(&self, filter: &mut FramedFilter) -> FilteredText {
            let info = race_info();
            let text = Arc::new(Floored::new(self.store.snapshot(), LineId::ZERO));
            filter.update(&text, &self.frames.snapshot(), &|frame| {
                hides_bytes(frame, Some(&info))
            })
        }
    }

    fn texts(source: &FilteredText) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        source.lines(source.first_line()..source.end(), &mut out);
        out.into_iter().map(|line| (line.id.0, line.text)).collect()
    }

    #[test]
    fn lines_of_binary_frames_go_and_text_and_local_lines_stay() {
        let mut rig = Rig::new();
        rig.feed(b"banner\r\n");
        rig.log("one");
        rig.log("two");
        // A local line ends the line of frames, as an inline summary does.
        rig.store.append_local("\u{25B8} log", Direction::Notice);
        rig.log("three");
        rig.feed(b"heartbeat\r\n");
        rig.store.append_local("sent", Direction::Tx);
        let mut filter = FramedFilter::new(&rig.store.snapshot());
        let source = rig.source(&mut filter);
        // Store lines: 0 banner, 1 frames one+two, 2 notice, 3 frame three + heartbeat
        // (mixed: shown), 4 sent.
        let shown = texts(&source);
        assert_eq!(shown.len(), 4, "{shown:?}");
        assert_eq!(shown[0], (0, "banner".into()));
        assert_eq!(shown[1], (1, "\u{25B8} log".into()));
        assert!(shown[2].1.ends_with("heartbeat"), "{shown:?}");
        assert_eq!(shown[3], (3, "sent".into()));
        assert_eq!(
            source.view_of(LineId(1)),
            None,
            "the frames' line is hidden"
        );
        assert_eq!(source.view_of(LineId(2)), Some(LineId(1)));
        assert_eq!(source.store_of(LineId(1)), Some(LineId(2)));
        assert_eq!(source.view_at_or_after(LineId(1)), LineId(1));

        // More arrives: the ids of what was shown stay.
        rig.log("four");
        rig.feed(b"tail line\r\n");
        let source = rig.source(&mut filter);
        let shown = texts(&source);
        assert_eq!(shown[..4], texts(&rig.source(&mut filter))[..4]);
        assert!(shown.last().unwrap().1.ends_with("tail line"));
    }

    #[test]
    fn a_line_still_arriving_is_decided_again_later() {
        let mut rig = Rig::new();
        rig.feed(b"text\r\n");
        let frame = encode_frame(RaceType::Log, 0x0F40, b"partial").unwrap();
        // Half a frame: the codec holds it back, so the line shows for now.
        rig.feed(&frame[..5]);
        let mut filter = FramedFilter::new(&rig.store.snapshot());
        assert_eq!(texts(&rig.source(&mut filter)).len(), 2);
        // The rest: now the bytes are a frame's, and the open line hides.
        rig.feed(&frame[5..]);
        let shown = texts(&rig.source(&mut filter));
        assert_eq!(shown, [(0, "text".into())]);
    }

    #[test]
    fn search_skips_hidden_lines_and_answers_in_view_ids() {
        let mut rig = Rig::new();
        rig.feed(b"needle one\r\n");
        rig.log("needle hidden");
        rig.store.append_local("x", Direction::Notice);
        rig.feed(b"needle two\r\n");
        let mut filter = FramedFilter::new(&rig.store.snapshot());
        let source = rig.source(&mut filter);
        let found = source
            .search(
                "needle",
                LineId(u64::MAX),
                true,
                10,
                &AtomicBool::new(false),
            )
            .unwrap();
        let lines: Vec<u64> = found.iter().map(|m| m.line.0).collect();
        assert_eq!(lines, [2, 0]);
        assert_eq!(source.line(LineId(2)).unwrap().text, "needle two");
    }

    #[test]
    fn many_runs_seal_blocks_and_lookups_still_find_every_line() {
        let mut rig = Rig::new();
        for i in 0..(BLOCK * 3) {
            rig.feed(format!("line {i}\r\n").as_bytes());
            rig.log("x");
            rig.store.append_local("n", Direction::Notice);
        }
        let mut filter = FramedFilter::new(&rig.store.snapshot());
        let source = rig.source(&mut filter);
        assert!(filter.sealed.len() >= 2, "{}", filter.sealed.len());
        // Per round: a text line, a hidden frame line, a notice.
        assert_eq!(source.line_count(), BLOCK * 3 * 2);
        for view in (0..source.line_count() as u64).step_by(37) {
            let store = source.store_of(LineId(view)).unwrap();
            assert_eq!(source.view_of(store), Some(LineId(view)));
        }
        let last = source.line(LineId(source.end().0 - 1)).unwrap();
        assert_eq!(last.text, "n");
    }
}
