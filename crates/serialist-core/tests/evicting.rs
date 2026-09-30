//! Property tests at the minimum budget, where eviction runs on almost every append.
//!
//! A store with an effectively unlimited budget, fed the same operations, is the
//! reference: every line the evicting store still holds must equal the reference's line
//! with the same id, and its retained raw bytes must equal the reference's.

mod common;

use std::time::Duration;

use proptest::prelude::*;
use serialist_core::{
    Direction, Epoch, LineId, LineSource, Snapshot, Store, StoreConfig, StyledLine,
};

/// Lines per index block, from the store's layout.
const BLOCK_LINES: u64 = 4096;

#[derive(Clone, Debug)]
enum Op {
    /// A received chunk: `bytes` repeated `repeat` times.
    Rx { bytes: Vec<u8>, repeat: usize },
    /// `lines` local lines of `len` bytes each.
    Local { lines: usize, len: usize, tx: bool },
    /// Local lines of `len` bytes up to the next index block boundary exactly.
    FillBlock { len: usize },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (common::stream(40), 1usize..3000).prop_map(|(bytes, repeat)| Op::Rx { bytes, repeat }),
        2 => (1usize..2500, 0usize..1200, any::<bool>())
            .prop_map(|(lines, len, tx)| Op::Local { lines, len, tx }),
        1 => (600usize..1200).prop_map(|len| Op::FillBlock { len }),
    ]
}

fn store(budget: usize, max_line_bytes: usize, epoch: Epoch) -> Store {
    Store::new(StoreConfig {
        budget,
        max_line_bytes,
        epoch: Some(epoch),
    })
}

fn apply(store: &mut Store, op: &Op, at: std::time::Instant) {
    match op {
        Op::Rx { bytes, repeat } => {
            let chunk = bytes.repeat(*repeat);
            store.append(&chunk, at);
        }
        Op::Local { lines, len, tx } => {
            let line = "l".repeat(*len);
            let text = vec![line.as_str(); *lines].join("\n");
            let dir = if *tx {
                Direction::Tx
            } else {
                Direction::Notice
            };
            store.append_local_at(&text, dir, at);
        }
        Op::FillBlock { len } => {
            // The line in progress, if any, is committed first and takes an id.
            let committed = store.end().0;
            let lines = (BLOCK_LINES - committed % BLOCK_LINES) as usize;
            let line = "f".repeat(*len);
            let text = vec![line.as_str(); lines].join("\n");
            store.append_local_at(&text, Direction::Notice, at);
        }
    }
}

fn lines(snap: &Snapshot, from: LineId) -> Vec<StyledLine> {
    let mut out = Vec::new();
    snap.lines(from..snap.end(), &mut out);
    out
}

fn raw(snap: &Snapshot, from: u64) -> Vec<u8> {
    snap.raw(from..u64::MAX).flatten().copied().collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    #[test]
    fn evicting_store_matches_the_reference(
        ops in prop::collection::vec(op(), 1..8),
        max_line in prop_oneof![Just(64 * 1024), Just(4096), 16usize..200],
    ) {
        let epoch = Epoch::now();
        let mut evicting = store(0, max_line, epoch);
        let mut reference = store(usize::MAX / 2, max_line, epoch);
        let budget = evicting.budget();
        let mut last_first = LineId(0);
        for (i, op) in ops.iter().enumerate() {
            let at = epoch.instant + Duration::from_millis(i as u64);
            apply(&mut evicting, op, at);
            apply(&mut reference, op, at);
            let stats = evicting.stats();
            prop_assert!(stats.memory <= budget, "{} over budget {budget} after {op:?}", stats.memory);
            prop_assert!(stats.first_line >= last_first);
            prop_assert_eq!(stats.end_line, reference.stats().end_line);
            last_first = stats.first_line;
        }
        let (snap, full) = (evicting.snapshot(), reference.snapshot());
        let kept = lines(&snap, snap.first_line());
        prop_assert_eq!(kept.len(), snap.line_count());
        prop_assert_eq!(&kept, &lines(&full, snap.first_line()));
        let raw_start = snap.raw_range().start;
        prop_assert_eq!(raw(&snap, raw_start), raw(&full, raw_start));
    }

    /// Chunking invariance while evicting: whole and chunked appends may evict at
    /// different moments, but every line both still hold is identical.
    #[test]
    fn chunking_is_invisible_while_evicting(
        unit in common::stream(100),
        repeat in 2000usize..6000,
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..24),
        max_line in prop_oneof![Just(64 * 1024), 16usize..200],
    ) {
        let bytes = unit.repeat(repeat);
        let mut cuts: Vec<usize> = cuts.iter().map(|c| c.index(bytes.len() + 1)).collect();
        cuts.sort_unstable();
        let epoch = Epoch::now();
        let at = epoch.instant + Duration::from_millis(1);
        let mut whole = store(0, max_line, epoch);
        whole.append(&bytes, at);
        let mut split = store(0, max_line, epoch);
        let mut from = 0;
        for &cut in &cuts {
            split.append(&bytes[from..cut], at);
            prop_assert!(split.stats().memory <= split.budget());
            from = cut;
        }
        split.append(&bytes[from..], at);
        let (a, b) = (whole.snapshot(), split.snapshot());
        prop_assert!(a.stats().memory <= whole.budget());
        prop_assert_eq!(a.end(), b.end());
        let first = a.first_line().max(b.first_line());
        prop_assert_eq!(lines(&a, first), lines(&b, first));
        let raw_start = a.raw_range().start.max(b.raw_range().start);
        prop_assert_eq!(raw(&a, raw_start), raw(&b, raw_start));
        prop_assert_eq!(raw(&a, raw_start), bytes[raw_start as usize..].to_vec());
    }
}

/// The reported sequence at the minimum budget, through every block boundary a few
/// times over: local lines only, then mixed with received data.
#[test]
fn block_boundaries_under_constant_eviction() {
    let epoch = Epoch::now();
    let mut store = store(0, 64 * 1024, epoch);
    let budget = store.budget();
    for round in 0..4u64 {
        let at = epoch.instant + Duration::from_millis(round);
        apply(&mut store, &Op::FillBlock { len: 1000 }, at);
        assert!(store.stats().memory <= budget);
        assert_eq!(store.end().0 % BLOCK_LINES, 0);
        apply(
            &mut store,
            &Op::Rx {
                bytes: b"rx line\r\n".to_vec(),
                repeat: 100,
            },
            at,
        );
        apply(&mut store, &Op::FillBlock { len: 700 }, at);
        assert!(store.stats().memory <= budget);
    }
    let snap = store.snapshot();
    assert!(snap.stats().evicted_lines > 0);
    let last = snap.line(LineId(snap.end().0 - 1)).expect("newest line");
    assert_eq!(last.text, "f".repeat(700));
}
