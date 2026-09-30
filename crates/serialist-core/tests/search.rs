//! Search must agree exactly with a brute-force reference: every line read back through
//! `line()` and searched on its own with the same smart-case regex.

mod common;

use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use proptest::prelude::*;
use regex::bytes::RegexBuilder;
use serialist_core::store::smart_case_insensitive;
use serialist_core::{
    Direction, LineId, LineSource, SearchMatch, Searcher, Snapshot, Store, StoreConfig,
};
use serialist_sim::{FirehoseContent, FirehoseGenerator};

const PATTERNS: &[&str] = &[
    "a",
    "o",
    "^a",
    "a$",
    "^$",
    r"\bok\b",
    "a.b",
    r"\d+",
    "é",
    "(?i)A",
    "x*",
    "[^a]",
    r"\s",
    "=$",
    "^#",
    r"\A.",
    r"z\z",
    ".",
    "A",
    "(?m)^[0-9]",
    "ok|zz",
    r"\p{L}{3}",
    r" \*[0-9a-f]{8}$",
    "(?-m)^#",
    "error|watchdog",
];

/// Every match of `pattern` in every retained line, in order.
fn reference_all(snap: &Snapshot, pattern: &str) -> Vec<SearchMatch> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(smart_case_insensitive(pattern))
        .build()
        .expect("valid pattern");
    let mut lines = Vec::new();
    snap.lines(snap.first_line()..snap.end(), &mut lines);
    let mut all = Vec::new();
    for line in &lines {
        for m in re.find_iter(line.text.as_bytes()) {
            all.push(SearchMatch {
                line: line.id,
                range: m.range(),
            });
        }
    }
    all
}

/// What a search from `from` should return, given every match in order.
fn expected(all: &[SearchMatch], from: u64, backward: bool, limit: usize) -> Vec<SearchMatch> {
    if backward {
        all.iter()
            .rev()
            .filter(|m| m.line.0 <= from)
            .take(limit)
            .cloned()
            .collect()
    } else {
        all.iter()
            .filter(|m| m.line.0 >= from)
            .take(limit)
            .cloned()
            .collect()
    }
}

fn check(
    snap: &Snapshot,
    all: &[SearchMatch],
    pattern: &str,
    from: u64,
    backward: bool,
    limit: usize,
) {
    let got = snap
        .search(
            pattern,
            LineId(from),
            backward,
            limit,
            &AtomicBool::new(false),
        )
        .expect("valid pattern");
    assert_eq!(
        got,
        expected(all, from, backward, limit),
        "pattern {pattern:?} from {from} backward {backward} limit {limit}"
    );
}

/// What a search inside `range` from `from` should return: the same matches, with
/// everything outside the range gone.
fn expected_in(
    all: &[SearchMatch],
    range: &Range<u64>,
    from: u64,
    backward: bool,
    limit: usize,
) -> Vec<SearchMatch> {
    let inside: Vec<_> = all
        .iter()
        .filter(|m| range.contains(&m.line.0))
        .cloned()
        .collect();
    expected(&inside, from, backward, limit)
}

/// `search_in` over `range` must equal the unbounded reference cut to the range.
fn check_in(
    snap: &Snapshot,
    all: &[SearchMatch],
    pattern: &str,
    range: Range<u64>,
    from: u64,
    backward: bool,
    limit: usize,
) {
    let got = snap
        .search_in(
            pattern,
            LineId(range.start)..LineId(range.end),
            LineId(from),
            backward,
            limit,
            &AtomicBool::new(false),
        )
        .expect("valid pattern");
    assert_eq!(
        got,
        expected_in(all, &range, from, backward, limit),
        "pattern {pattern:?} in {range:?} from {from} backward {backward} limit {limit}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn search_matches_the_reference(
        bytes in common::stream(300),
        cuts in prop::collection::vec((0usize..4000, any::<bool>()), 0..12),
        max_line in prop_oneof![Just(64 * 1024), 16usize..120],
        pattern in prop::sample::select(PATTERNS),
        from_frac in 0.0f64..1.2,
        backward in any::<bool>(),
        limit in prop_oneof![Just(1usize), Just(3), Just(usize::MAX)],
        lo_frac in 0.0f64..1.1,
        hi_frac in 0.0f64..1.2,
    ) {
        let mut store = Store::new(StoreConfig {
            max_line_bytes: max_line,
            ..StoreConfig::default()
        });
        let now = Instant::now();
        let mut cuts: Vec<_> = cuts.into_iter().map(|(c, l)| (c.min(bytes.len()), l)).collect();
        cuts.sort_unstable();
        let mut at = 0;
        for (cut, local) in cuts {
            store.append(&bytes[at..cut], now);
            at = cut;
            if local {
                store.append_local("local ok a=1 é", Direction::Tx);
            }
        }
        store.append(&bytes[at..], now);
        let snap = store.snapshot();
        let from = (snap.end().0 as f64 * from_frac) as u64;
        let all = reference_all(&snap, pattern);
        check(&snap, &all, pattern, from, backward, limit);
        // The same search inside a random range, which may be empty or inverted.
        let end = snap.end().0 as f64;
        let range = (end * lo_frac) as u64..(end * hi_frac) as u64;
        check_in(&snap, &all, pattern, range, from, backward, limit);
    }
}

/// Large streams: many raw pages, many text pages, lines spanning pages, eviction.
#[test]
fn large_streams_match_the_reference() {
    std::thread::scope(|scope| {
        for (content, seed, chunks, budget) in [
            (FirehoseContent::Mixed, 1, 800, 0),
            (FirehoseContent::Ansi, 2, 400, 0),
            (FirehoseContent::MixedEol, 3, 400, 64 * 1024 * 1024),
            (FirehoseContent::Binary, 4, 400, 64 * 1024 * 1024),
        ] {
            scope.spawn(move || large_stream(content, seed, chunks, budget));
        }
    });
}

fn large_stream(content: FirehoseContent, seed: u64, chunks: usize, budget: usize) {
    // A 4 KiB line limit keeps the minimum budget near 1.7 MiB, so the budget-0 streams
    // evict; longer lines are broken, which the reference sees too.
    let mut store = Store::new(StoreConfig {
        budget,
        max_line_bytes: 4096,
        ..StoreConfig::default()
    });
    let mut generator = FirehoseGenerator::new(content, seed);
    let mut chunk = Vec::new();
    let now = Instant::now();
    for i in 0..chunks {
        chunk.clear();
        generator.fill(&mut chunk, 1 + (i * 7919) % 5000);
        store.append(&chunk, now);
        if i % 37 == 0 {
            store.append_local("local watchdog ok", Direction::Notice);
        }
    }
    let snap = store.snapshot();
    let stats = snap.stats();
    assert!(
        stats.pages > 4 && stats.text_pages > 1,
        "{content:?}: {stats:?}"
    );
    assert_eq!(
        budget == 0,
        stats.evicted_lines > 0,
        "{content:?}: {stats:?}"
    );
    let (first, end) = (snap.first_line().0, snap.end().0);
    for pattern in PATTERNS {
        let all = reference_all(&snap, pattern);
        for from in [first, first + (end - first) / 3, end.saturating_sub(1)] {
            for backward in [false, true] {
                for limit in [1, 50] {
                    check(&snap, &all, pattern, from, backward, limit);
                }
            }
        }
        check(&snap, &all, pattern, first, false, usize::MAX);
        check(&snap, &all, pattern, end, true, usize::MAX);
        // Bounded to a middle third, and to the newest tenth (a clear floor).
        let third = first + (end - first) / 3..first + 2 * (end - first) / 3;
        let floor = end - (end - first) / 10..end;
        for range in [third, floor] {
            for from in [range.start, (range.start + range.end) / 2, end] {
                for backward in [false, true] {
                    for limit in [1, 50, usize::MAX] {
                        check_in(&snap, &all, pattern, range.clone(), from, backward, limit);
                    }
                }
            }
        }
    }
}
