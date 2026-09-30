//! The scrollback: rows that have scrolled off the top of the primary screen, as
//! immutable lines with stable ids, cheap to snapshot.
//!
//! Alacritty keeps its own history as grid rows, but a snapshot must not share them with
//! the thread that keeps mutating the grid, and copying 10 000 rows per snapshot is too
//! slow. So every row that scrolls off is converted once, as it goes, and kept here. The
//! screen empties Alacritty's history right after (see [`crate::screen`]).
//!
//! Rows live in blocks of up to [`BLOCK`] lines. A full block is sealed into an
//! `Arc<[..]>` and never changes; the block being filled is an `Arc<Vec<..>>` that a
//! snapshot shares until the next push copies it (at most `BLOCK` pointer copies). A
//! snapshot therefore costs one `Arc` clone for the block directory, rebuilt only when a
//! block is sealed or evicted, and one for the open block.

use std::collections::VecDeque;
use std::sync::Arc;

use serialist_core::StyledLine;

/// Lines per block: the unit of sealing and eviction.
pub(crate) const BLOCK: usize = 256;

type Block = Arc<[Arc<StyledLine>]>;

/// The owner's side. See the module docs.
pub(crate) struct Scrollback {
    capacity: usize,
    /// Sealed blocks, oldest first, each with the id of its first line. Each holds
    /// consecutive ids; a [`skip`](Self::skip) leaves a gap of ids no block holds.
    sealed: VecDeque<(u64, Block)>,
    /// Id of the open block's first line.
    open_start: u64,
    open: Arc<Vec<Arc<StyledLine>>>,
    /// Oldest retained id.
    first: u64,
    /// One past the newest id: how many rows have ever scrolled off.
    end: u64,
    dir: Arc<[(u64, Block)]>,
    dir_dirty: bool,
}

impl Scrollback {
    /// Keep the newest `capacity` rows. Zero keeps none, but still counts ids.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            sealed: VecDeque::new(),
            open_start: 0,
            open: Arc::new(Vec::new()),
            first: 0,
            end: 0,
            dir: Arc::new([]),
            dir_dirty: false,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// The id the next row will get.
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Add the next row, whose id must be [`end`](Self::end).
    pub fn push(&mut self, line: Arc<StyledLine>) {
        debug_assert_eq!(line.id.0, self.end);
        if self.capacity == 0 {
            self.skip(1);
            return;
        }
        Arc::make_mut(&mut self.open).push(line);
        self.end += 1;
        if self.open.len() == BLOCK {
            let full = std::mem::take(Arc::make_mut(&mut self.open));
            self.sealed.push_back((self.open_start, full.into()));
            self.open_start = self.end;
            self.dir_dirty = true;
        }
        self.evict();
    }

    /// Count `n` rows that scrolled off without keeping them, because they would be
    /// evicted at once. Their ids are used up. The open block is sealed short first, so
    /// blocks stay contiguous in the ids they hold and the skipped ids fall in a gap.
    pub fn skip(&mut self, n: u64) {
        if n == 0 {
            return;
        }
        if !self.open.is_empty() {
            let partial = std::mem::take(Arc::make_mut(&mut self.open));
            self.sealed.push_back((self.open_start, partial.into()));
            self.dir_dirty = true;
        }
        self.end += n;
        self.open_start = self.end;
        self.evict();
    }

    /// Forget every row. Ids are not reused: the next row still gets [`end`](Self::end).
    pub fn clear(&mut self) {
        self.sealed.clear();
        self.open = Arc::new(Vec::new());
        self.open_start = self.end;
        self.first = self.end;
        self.dir_dirty = true;
    }

    fn evict(&mut self) {
        self.first = self
            .first
            .max(self.end.saturating_sub(self.capacity as u64));
        while let Some((start, block)) = self.sealed.front() {
            if start + block.len() as u64 > self.first {
                break;
            }
            self.sealed.pop_front();
            self.dir_dirty = true;
        }
        // The open block always holds the newest row, which is retained.
    }

    /// An immutable view of what is retained now.
    pub fn view(&mut self) -> ScrollbackView {
        if self.dir_dirty {
            self.dir = self.sealed.iter().cloned().collect();
            self.dir_dirty = false;
        }
        ScrollbackView {
            dir: Arc::clone(&self.dir),
            open_start: self.open_start,
            open: Arc::clone(&self.open),
            first: self.first,
            end: self.end,
        }
    }
}

/// What a snapshot holds of the scrollback.
#[derive(Clone)]
pub(crate) struct ScrollbackView {
    dir: Arc<[(u64, Block)]>,
    open_start: u64,
    open: Arc<Vec<Arc<StyledLine>>>,
    first: u64,
    end: u64,
}

impl ScrollbackView {
    pub fn first(&self) -> u64 {
        self.first
    }

    pub fn end(&self) -> u64 {
        self.end
    }

    pub fn len(&self) -> usize {
        (self.end - self.first) as usize
    }

    pub fn get(&self, id: u64) -> Option<&Arc<StyledLine>> {
        if id < self.first || id >= self.end {
            return None;
        }
        if id >= self.open_start {
            return self.open.get((id - self.open_start) as usize);
        }
        let ix = self.dir.partition_point(|(start, _)| *start <= id);
        let (start, block) = self.dir.get(ix.checked_sub(1)?)?;
        block.get((id - start) as usize)
    }

    /// Whether both views hold the same rows (same ids, same lines).
    pub fn same_as(&self, other: &Self) -> bool {
        self.first == other.first
            && self.end == other.end
            && Arc::ptr_eq(&self.dir, &other.dir)
            && Arc::ptr_eq(&self.open, &other.open)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serialist_core::{Direction, LineId};

    use super::*;

    fn line(id: u64) -> Arc<StyledLine> {
        Arc::new(StyledLine {
            id: LineId(id),
            text: format!("row {id}"),
            runs: Vec::new(),
            direction: Direction::Rx,
            received_at: Instant::now(),
            raw: 0..0,
            complete: true,
        })
    }

    fn fill(sb: &mut Scrollback, n: u64) {
        for _ in 0..n {
            let id = sb.end();
            sb.push(line(id));
        }
    }

    fn check(view: &ScrollbackView) {
        for id in view.first()..view.end() {
            assert_eq!(view.get(id).map(|l| l.id), Some(LineId(id)), "id {id}");
        }
        assert!(view.get(view.end()).is_none());
        if view.first() > 0 {
            assert!(view.get(view.first() - 1).is_none());
        }
    }

    #[test]
    fn keeps_the_newest_rows_across_blocks() {
        let mut sb = Scrollback::new(1000);
        fill(&mut sb, 3 * BLOCK as u64 + 17);
        let view = sb.view();
        assert_eq!((view.first(), view.end()), (0, 3 * BLOCK as u64 + 17));
        check(&view);
        fill(&mut sb, 2000);
        let view = sb.view();
        assert_eq!(view.len(), 1000);
        assert_eq!(view.end(), 3 * BLOCK as u64 + 17 + 2000);
        check(&view);
        assert!(sb.sealed.len() <= 1000 / BLOCK + 1);
    }

    #[test]
    fn a_view_does_not_change_after_more_pushes() {
        let mut sb = Scrollback::new(10_000);
        fill(&mut sb, 10);
        let before = sb.view();
        fill(&mut sb, 600);
        check(&before);
        assert_eq!(before.end(), 10);
        assert_eq!(before.get(9).unwrap().text, "row 9");
    }

    #[test]
    fn clear_retires_ids() {
        let mut sb = Scrollback::new(100);
        fill(&mut sb, 300);
        sb.clear();
        let view = sb.view();
        assert_eq!((view.first(), view.end()), (300, 300));
        fill(&mut sb, 5);
        let view = sb.view();
        assert_eq!((view.first(), view.end()), (300, 305));
        check(&view);
    }

    #[test]
    fn zero_capacity_counts_but_keeps_nothing() {
        let mut sb = Scrollback::new(0);
        fill(&mut sb, 700);
        let view = sb.view();
        assert_eq!((view.first(), view.end(), view.len()), (700, 700, 0));
    }

    #[test]
    fn skipped_rows_use_up_ids() {
        let mut sb = Scrollback::new(4);
        fill(&mut sb, 3);
        sb.skip(10);
        fill(&mut sb, 2);
        let view = sb.view();
        assert_eq!((view.first(), view.end()), (11, 15));
        assert!(view.get(12).is_none(), "skipped rows are not retained");
        assert_eq!(view.get(13).unwrap().id, LineId(13));
        assert_eq!(view.get(14).unwrap().id, LineId(14));
    }
}
