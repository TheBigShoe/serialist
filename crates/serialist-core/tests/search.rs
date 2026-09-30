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
    Direction, HexView, LineId, LineSource, SearchMatch, Searcher, Snapshot, Store, StoreConfig,
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

/// A searcher that can also search inside a line range, as snapshots and hex views can.
trait Bounded: Searcher {
    fn search_in(
        &self,
        pattern: &str,
        range: Range<LineId>,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String>;
}

impl Bounded for Snapshot {
    fn search_in(
        &self,
        pattern: &str,
        range: Range<LineId>,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        Snapshot::search_in(self, pattern, range, from, backward, limit, cancel)
    }
}

impl Bounded for HexView {
    fn search_in(
        &self,
        pattern: &str,
        range: Range<LineId>,
        from: LineId,
        backward: bool,
        limit: usize,
        cancel: &AtomicBool,
    ) -> Result<Vec<SearchMatch>, String> {
        HexView::search_in(self, pattern, range, from, backward, limit, cancel)
    }
}

/// Every match of `pattern` in every retained line of `source`, in order.
fn reference_all(source: &dyn LineSource, pattern: &str) -> Vec<SearchMatch> {
    let re = RegexBuilder::new(pattern)
        .case_insensitive(smart_case_insensitive(pattern))
        .build()
        .expect("valid pattern");
    let mut lines = Vec::new();
    source.lines(source.first_line()..source.end(), &mut lines);
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
    snap: &impl Bounded,
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
    snap: &impl Bounded,
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

// --- Hex rows ---------------------------------------------------------------------

/// Patterns for hex rows: the offset, hex and ASCII columns, both cases, and shapes
/// that cross the gap between the two groups of eight.
const HEX_PATTERNS: &[&str] = &[
    "^0000",
    r"^0000[0-9a-f]0\b",
    "0d 0a",
    "0D 0A",
    "(?i)0D 0A",
    "4f 4b",
    "4F",
    "de ad be ef",
    "  ",
    "20  4f",
    r"\|id=",
    r"\|.*\|$",
    "ok",
    "OK",
    r"\.\.",
    r"[0-9a-f]{2} 00",
    "x*",
    r"\A0",
    r"\|\z",
];

/// About 2.7 kB: 12-byte CRLF lines, with binary junk now and then so that rows are not
/// aligned to lines.
fn hex_stream() -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in 0..220 {
        bytes.extend_from_slice(format!("id={i:04} OK\r\n").as_bytes());
        if i % 20 == 5 {
            bytes.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00]);
        }
    }
    bytes
}

fn hex_store(bytes: &[u8]) -> Store {
    let mut store = Store::default();
    let now = Instant::now();
    for chunk in bytes.chunks(1000) {
        store.append(chunk, now);
    }
    store
}

#[test]
fn hex_view_search_forward_backward_limit_and_cancel() {
    let store = hex_store(&hex_stream());
    let snap = store.snapshot();
    let hex = snap.hex_view(16);
    let rows = hex.line_count() as u64;
    assert!(rows > 150, "{rows} rows");
    let cancel = AtomicBool::new(false);
    let search = |pattern: &str, from: u64, backward: bool, limit: usize| {
        hex.search(pattern, LineId(from), backward, limit, &cancel)
            .expect("valid pattern")
    };
    assert_eq!(
        hex.line(LineId(0)).unwrap().text,
        "00000000  69 64 3d 30 30 30 30 20  4f 4b 0d 0a 69 64 3d 30  |id=0000 OK..id=0|"
    );

    // Each column, with ranges in the row's text.
    let offset = search("^00000030", 0, false, 10);
    assert_eq!(
        offset,
        [SearchMatch {
            line: LineId(3),
            range: 0..8
        }]
    );
    let hex_column = search("0d 0a", 0, false, 1);
    assert_eq!(
        hex_column,
        [SearchMatch {
            line: LineId(0),
            range: 41..46
        }]
    );
    let ascii = search(r"\|id=0000", 0, false, 1);
    assert_eq!(
        ascii,
        [SearchMatch {
            line: LineId(0),
            range: 60..68
        }]
    );

    // Smart case: lowercase patterns are insensitive, an uppercase letter is not.
    assert!(
        !search("ok", 0, false, 1).is_empty(),
        "matches the ASCII OK"
    );
    assert!(!search("OK", 0, false, 1).is_empty());
    assert!(!search("4f 4b", 0, false, 1).is_empty());
    assert!(
        search("4F 4B", 0, false, 1).is_empty(),
        "hex digits are lowercase"
    );
    assert!(!search("(?i)4F 4B", 0, false, 1).is_empty());

    // Forward goes up from `from`, inclusive; backward goes down from it, inclusive.
    let forward = search("0d 0a", 40, false, usize::MAX);
    assert_eq!(forward.first().unwrap().line, LineId(40));
    assert!(forward.windows(2).all(|w| w[0].line <= w[1].line));
    let backward = search("0d 0a", 40, true, usize::MAX);
    assert_eq!(backward.first().unwrap().line, LineId(40));
    assert!(backward.windows(2).all(|w| w[0].line >= w[1].line));
    assert_eq!(backward.last().unwrap().line, LineId(0));
    // Two matches in a row come in the search direction.
    let all_forward = search("0d 0a", 0, false, usize::MAX);
    let two = all_forward
        .windows(2)
        .find(|w| w[0].line == w[1].line)
        .expect("some row holds two line ends");
    let (line, first, second) = (two[0].line, two[0].range.start, two[1].range.start);
    assert!(first < second);
    let back_in_row: Vec<_> = search("0d 0a", line.0, true, usize::MAX)
        .into_iter()
        .filter(|m| m.line == line)
        .map(|m| m.range.start)
        .collect();
    assert_eq!(back_in_row[..2], [second, first]);

    // Limits count matches, not rows.
    assert_eq!(search("0d 0a", 0, false, 3).len(), 3);
    assert_eq!(search("0d 0a", rows, true, 5).len(), 5);
    assert_eq!(search("0d 0a", 0, false, 0), []);
    assert_eq!(search("0d 0a", 0, false, 7), all_forward[..7]);
    let mut all_backward = search("0d 0a", u64::MAX, true, usize::MAX);
    assert_eq!(all_backward.len(), all_forward.len());
    // Backward lists a row's matches last first, so compare the two as sets.
    all_backward.sort_by_key(|m| (m.line, m.range.start));
    assert_eq!(all_backward, all_forward);

    // A pattern that is not a regex is an error, and a set flag stops the scan.
    assert!(hex.search("(", LineId(0), false, 1, &cancel).is_err());
    let cancelled = AtomicBool::new(true);
    for backward in [false, true] {
        let none = hex.search("0d 0a", LineId(rows), backward, usize::MAX, &cancelled);
        assert_eq!(none, Ok(Vec::new()), "backward: {backward}");
    }

    // An empty view has nothing to search.
    let empty = Store::default().snapshot().hex_view(16);
    for backward in [false, true] {
        let none = empty.search("0", LineId(0), backward, 5, &cancel);
        assert_eq!(none, Ok(Vec::new()));
    }
}

/// Hex search agrees with searching every row's text with the same regex, for each
/// direction and range.
fn hex_matches_the_reference(bytes: &[u8], per_row: usize, cases: &[(&str, f64, bool, usize)]) {
    let store = hex_store(bytes);
    let hex = store.snapshot().hex_view(per_row);
    let rows = hex.end().0 as f64;
    for &(pattern, from_frac, backward, limit) in cases {
        let all = reference_all(&hex, pattern);
        let from = (rows * from_frac) as u64;
        check(&hex, &all, pattern, from, backward, limit);
        let range = (rows * 0.25) as u64..(rows * 0.75) as u64;
        check_in(&hex, &all, pattern, range, from, backward, limit);
    }
}

#[test]
fn hex_search_matches_the_reference_on_a_fixed_stream() {
    let bytes = hex_stream();
    let mut cases = Vec::new();
    for pattern in HEX_PATTERNS {
        for from_frac in [0.0, 0.4, 1.0, 1.2] {
            for backward in [false, true] {
                for limit in [1, 4, usize::MAX] {
                    cases.push((*pattern, from_frac, backward, limit));
                }
            }
        }
    }
    for per_row in [4, 8, 16, 32] {
        hex_matches_the_reference(&bytes, per_row, &cases);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn hex_search_matches_the_reference(
        bytes in common::stream(200),
        per_row in prop::sample::select(vec![1usize, 5, 8, 16, 64]),
        pattern in prop::sample::select(HEX_PATTERNS),
        from_frac in 0.0f64..1.2,
        backward in any::<bool>(),
        limit in prop_oneof![Just(1usize), Just(3), Just(usize::MAX)],
    ) {
        hex_matches_the_reference(&bytes, per_row, &[(pattern, from_frac, backward, limit)]);
    }
}
