use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use super::*;
use crate::text::{Color, LineSource, SearchMatch, Searcher, StyleFlags, StyledLine};

fn t0(store: &Store) -> Instant {
    store.epoch().instant
}

fn texts(snap: &Snapshot) -> Vec<String> {
    let mut out = Vec::new();
    snap.lines(snap.first_line()..snap.end(), &mut out);
    out.into_iter().map(|l| l.text).collect()
}

fn raw_bytes(snap: &Snapshot) -> Vec<u8> {
    snap.raw(0..u64::MAX).flatten().copied().collect()
}

fn check_line(line: &StyledLine) {
    let total: usize = line.runs.iter().map(|r| r.len).sum();
    assert_eq!(total, line.text.len(), "runs cover {line:?}");
    assert!(line.runs.iter().all(|r| r.len > 0));
    assert!(!line.text.contains(['\n', '\r']));
}

#[test]
fn lines_text_runs_and_raw_ranges() {
    let mut store = Store::default();
    let at = t0(&store) + Duration::from_millis(5);
    let report = store.append(b"one\r\ntwo \x1b[31mred\x1b[0m\nthr", at);
    assert_eq!(report.new_lines, 3);
    assert!(report.incomplete);
    assert_eq!(report.changed, LineId(0)..LineId(3));
    let snap = store.snapshot();
    assert_eq!(snap.line_count(), 3);
    let l0 = snap.line(LineId(0)).unwrap();
    assert_eq!(l0.text, "one");
    assert_eq!(l0.raw, 0..5);
    assert!(l0.complete);
    assert_eq!(l0.received_at, at);
    assert_eq!(l0.direction, Direction::Rx);
    let l1 = snap.line(LineId(1)).unwrap();
    assert_eq!(l1.text, "two red");
    assert_eq!(l1.runs.len(), 2);
    assert_eq!(l1.runs[1].style.fg, Color::Ansi(1));
    assert_eq!(l1.raw, 5..22);
    let l2 = snap.line(LineId(2)).unwrap();
    assert_eq!(l2.text, "thr");
    assert!(!l2.complete);
    assert_eq!(l2.raw, 22..25);
    for l in [&l0, &l1, &l2] {
        check_line(l);
    }
    assert_eq!(snap.line(LineId(3)), None);

    // The line in progress continues and completes in the next chunk.
    let later = at + Duration::from_millis(10);
    let report = store.append(b"ee\n", later);
    assert_eq!(report.changed, LineId(2)..LineId(3));
    assert_eq!(report.new_lines, 0);
    assert!(!report.incomplete);
    let snap2 = store.snapshot();
    let l2 = snap2.line(LineId(2)).unwrap();
    assert_eq!(
        (l2.text.as_str(), l2.complete, l2.raw.clone()),
        ("three", true, 22..28)
    );
    assert_eq!(l2.received_at, at, "a line is stamped with its first byte");
    // The old snapshot is unchanged.
    assert_eq!(snap.line(LineId(2)).unwrap().text, "thr");
    assert_eq!(raw_bytes(&snap2), b"one\r\ntwo \x1b[31mred\x1b[0m\nthree\n");
}

#[test]
fn plain_lines_store_no_text() {
    let mut store = Store::default();
    let now = Instant::now();
    for i in 0..1000 {
        store.append(format!("line {i} ok\r\n").as_bytes(), now);
    }
    store.append(b"\r\rled\r\r\n\n\r\n", now);
    let stats = store.stats();
    assert_eq!(stats.text_pages, 0, "{stats:?}");
    let snap = store.snapshot();
    assert_eq!(snap.line(LineId(999)).unwrap().text, "line 999 ok");
    assert_eq!(snap.line(LineId(1000)).unwrap().text, "led");
    assert_eq!(snap.line(LineId(1001)).unwrap().text, "");
    assert_eq!(snap.line(LineId(1002)).unwrap().text, "");
    // Transformed lines do use text pages.
    store.append(b"a\tb\n", now);
    assert_eq!(store.stats().text_pages, 1);
    assert_eq!(
        store.snapshot().line(LineId(1003)).unwrap().text,
        "a       b"
    );
}

#[test]
fn cr_split_across_chunks_and_plain_tail() {
    let mut store = Store::default();
    let now = Instant::now();
    store.append(b"progress 10%\r", now);
    assert_eq!(
        store.snapshot().line(LineId(0)).unwrap().text,
        "progress 10%"
    );
    store.append(b"progress 99%", now);
    assert_eq!(
        store.snapshot().line(LineId(0)).unwrap().text,
        "progress 99%"
    );
    store.append(b"\r", now);
    store.append(b"\n", now);
    let snap = store.snapshot();
    assert_eq!(snap.line_count(), 1);
    let line = snap.line(LineId(0)).unwrap();
    assert_eq!(line.text, "progress 99%");
    assert!(line.complete);
    // A UTF-8 character split across chunks.
    store.append(b"caf\xc3", now);
    store.append(b"\xa9 \xe2\x82", now);
    store.append(b"\xac\n", now);
    assert_eq!(store.snapshot().line(LineId(1)).unwrap().text, "café €");
}

#[test]
fn local_lines_break_the_partial_line_and_keep_order() {
    let mut store = Store::default();
    let now = Instant::now();
    let ids = store.append_local("Connected: virtual:echo", Direction::Notice);
    assert_eq!(ids, LineId(0)..LineId(1));
    store.append(b"prompt> ", now);
    let ids = store.append_local("AT\r\n", Direction::Tx);
    assert_eq!(ids, LineId(2)..LineId(3));
    store.append(b"OK\r\n", now);
    store.append_local("two\nlines\twith tab", Direction::Tx);
    let snap = store.snapshot();
    let mut lines = Vec::new();
    snap.lines(snap.first_line()..snap.end(), &mut lines);
    let summary: Vec<_> = lines
        .iter()
        .map(|l| (l.direction, l.text.as_str(), l.complete, l.raw.clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (Direction::Notice, "Connected: virtual:echo", true, 0..0),
            (Direction::Rx, "prompt> ", false, 0..8),
            (Direction::Tx, "AT", true, 8..8),
            (Direction::Rx, "OK", true, 8..12),
            (Direction::Tx, "two", true, 12..12),
            (Direction::Tx, "lines   with tab", true, 12..12),
        ]
    );
    for l in &lines {
        check_line(l);
    }
}

#[test]
fn eviction_keeps_ids_and_budget() {
    let config = StoreConfig {
        budget: 0,
        ..StoreConfig::default()
    };
    let mut store = Store::new(config);
    let budget = store.budget();
    let now = Instant::now();
    let mut sent = Vec::new();
    let early = store.snapshot();
    let mut before_eviction = None;
    let mut last_first = LineId(0);
    let mut i = 0u64;
    while store.stats().evicted_lines == 0 || i < 200_000 {
        let chunk = format!("{i:08} some log text for eviction\r\n");
        sent.extend_from_slice(chunk.as_bytes());
        let report = store.append(chunk.as_bytes(), now);
        if report.evicted_lines > 0 && before_eviction.is_none() {
            before_eviction = Some(early.clone());
        }
        if i == 1000 {
            before_eviction = Some(store.snapshot());
        }
        let stats = store.stats();
        assert!(
            stats.memory <= budget,
            "memory {} > budget {budget}",
            stats.memory
        );
        assert!(stats.first_line >= last_first);
        last_first = stats.first_line;
        i += 1;
    }
    let snap = store.snapshot();
    let stats = snap.stats();
    assert!(stats.first_line > LineId(0));
    assert_eq!(stats.end_line, LineId(i));
    assert_eq!(stats.raw_len, sent.len() as u64);
    assert_eq!(stats.raw_start % PAGE_SIZE as u64, 0);
    // Retained raw bytes are exactly the tail of what was sent.
    assert_eq!(raw_bytes(&snap), &sent[stats.raw_start as usize..]);
    // Every retained line reads back and its raw bytes are retained.
    let first = snap.line(snap.first_line()).unwrap();
    assert!(first.raw.start >= stats.raw_start);
    assert_eq!(
        first.text,
        format!("{:08} some log text for eviction", first.id.0)
    );
    assert!(snap.line(LineId(snap.first_line().0 - 1)).is_none());
    // A snapshot taken before the eviction still reads its lines.
    let old = before_eviction.unwrap();
    let old_line = old.line(LineId(500)).unwrap();
    assert_eq!(old_line.text, "00000500 some log text for eviction");
    assert!(old.first_line() < snap.first_line());
}

#[test]
fn local_lines_alone_are_evicted() {
    let mut store = Store::new(StoreConfig::with_budget(0));
    let budget = store.budget();
    let line = "x".repeat(200);
    for _ in 0..20_000 {
        store.append_local(&line, Direction::Tx);
        assert!(store.stats().memory <= budget);
    }
    let stats = store.stats();
    assert!(stats.first_line > LineId(0));
    let snap = store.snapshot();
    assert_eq!(snap.line(snap.first_line()).unwrap().text, line);
}

#[test]
fn long_lines_break_at_the_limit() {
    let mut store = Store::new(StoreConfig {
        max_line_bytes: 100,
        ..StoreConfig::default()
    });
    let now = Instant::now();
    store.append(&[b'a'; 250], now);
    let snap = store.snapshot();
    assert_eq!(snap.line_count(), 3);
    let l0 = snap.line(LineId(0)).unwrap();
    assert_eq!(
        (l0.text.len(), l0.complete, l0.raw.clone()),
        (100, false, 0..100)
    );
    assert_eq!(snap.line(LineId(2)).unwrap().raw, 200..250);
}

#[test]
fn lines_spanning_pages_read_back() {
    let mut store = Store::default();
    let now = Instant::now();
    let mut sent = Vec::new();
    for i in 0..3000 {
        let styled = if i % 7 == 0 { "\x1b[1m" } else { "" };
        let line = format!("{styled}{i} {}\r\n", "y".repeat(i % 97));
        sent.extend_from_slice(line.as_bytes());
    }
    for chunk in sent.chunks(4093) {
        store.append(chunk, now);
    }
    let snap = store.snapshot();
    assert!(snap.stats().pages > 2);
    for i in 0..3000u64 {
        let l = snap.line(LineId(i)).unwrap();
        check_line(&l);
        assert_eq!(l.text, format!("{i} {}", "y".repeat(i as usize % 97)));
    }
    assert_eq!(raw_bytes(&snap), sent);
}

fn search_store() -> Store {
    let mut store = Store::default();
    let now = Instant::now();
    let mut data = Vec::new();
    for i in 0..5000 {
        let line = match i % 50 {
            0 => format!("{i} ERROR disk full\r\n"),
            25 => format!("{i} \x1b[31merror\x1b[0m in colour\r\n"),
            _ => format!("{i} all good here\r\n"),
        };
        data.extend_from_slice(line.as_bytes());
    }
    for chunk in data.chunks(1000) {
        store.append(chunk, now);
    }
    store.append_local("tx error echo", Direction::Tx);
    store.append(b"tail error no lf", now);
    store
}

fn search(
    snap: &Snapshot,
    pattern: &str,
    from: u64,
    backward: bool,
    limit: usize,
) -> Vec<SearchMatch> {
    snap.search(
        pattern,
        LineId(from),
        backward,
        limit,
        &AtomicBool::new(false),
    )
    .unwrap()
}

#[test]
fn search_forward_backward_and_limits() {
    let store = search_store();
    let snap = store.snapshot();
    // Smart case: lowercase matches both spellings.
    let all = search(&snap, "error", 0, false, usize::MAX);
    assert_eq!(all.len(), 200 + 2);
    assert_eq!(all[0].line, LineId(0));
    assert_eq!(all[0].range, 2..7);
    assert_eq!(all[1].line, LineId(25));
    assert_eq!(all[1].range, 3..8, "range is in the decoded text");
    assert_eq!(all[200].line, LineId(5000), "local line");
    assert_eq!(all[201].line, LineId(5001), "line in progress");
    // An uppercase letter makes it case-sensitive.
    let upper = search(&snap, "ERROR", 0, false, usize::MAX);
    assert_eq!(upper.len(), 100);
    // Limit and start.
    let some = search(&snap, "error", 30, false, 3);
    let lines: Vec<_> = some.iter().map(|m| m.line.0).collect();
    assert_eq!(lines, vec![50, 75, 100]);
    // Backward includes `from` and goes newest first.
    let back = search(&snap, "error", 100, true, 3);
    let lines: Vec<_> = back.iter().map(|m| m.line.0).collect();
    assert_eq!(lines, vec![100, 75, 50]);
    let back_all = search(&snap, "error", u64::MAX, true, usize::MAX);
    let mut forward_lines: Vec<_> = all.iter().map(|m| m.line).collect();
    forward_lines.reverse();
    assert_eq!(
        back_all.iter().map(|m| m.line).collect::<Vec<_>>(),
        forward_lines
    );
    // Several matches in a line come in the search direction.
    let multi = search(&snap, "o", 1, false, 3);
    let got: Vec<_> = multi.iter().map(|m| (m.line.0, m.range.start)).collect();
    assert_eq!(got, vec![(1, 7), (1, 8), (2, 7)]);
    let multi_back = search(&snap, "o", 1, true, 3);
    let got: Vec<_> = multi_back
        .iter()
        .map(|m| (m.line.0, m.range.start))
        .collect();
    assert_eq!(got, vec![(1, 8), (1, 7), (0, 5)]);
    // Anchors behave per line in bulk and per-line search alike.
    let anchored = search(&snap, "^49 all good here$", 0, false, 10);
    assert_eq!(anchored.len(), 1);
    assert_eq!(anchored[0].line, LineId(49));
    let whole = search(&snap, r"\A4999 ", 0, false, 10);
    assert_eq!(
        whole.iter().map(|m| m.line.0).collect::<Vec<_>>(),
        vec![4999]
    );
    // No match across a line boundary.
    assert!(search(&snap, "here.49", 0, false, 10).is_empty());
    assert!(search(&snap, r"here\s+49", 0, false, 10).is_empty());
    assert_eq!(search(&snap, "x", 0, false, 0), vec![]);
}

#[test]
fn search_errors_and_cancel() {
    let store = search_store();
    let snap = store.snapshot();
    let err = snap
        .search("(unclosed", LineId(0), false, 10, &AtomicBool::new(false))
        .unwrap_err();
    assert!(err.contains("unclosed"), "{err}");
    let cancelled = snap
        .search(
            "error",
            LineId(0),
            false,
            usize::MAX,
            &AtomicBool::new(true),
        )
        .unwrap();
    assert!(cancelled.is_empty());
}

#[test]
fn search_across_an_eviction_boundary() {
    let mut store = Store::new(StoreConfig::with_budget(0));
    let now = Instant::now();
    let mut i = 0u64;
    let mut old = None;
    while store.stats().first_line.0 < 20_000 {
        let line = if i.is_multiple_of(1000) {
            format!("{i:08} needle\r\n")
        } else {
            format!("{i:08} hay hay hay hay\r\n")
        };
        store.append(line.as_bytes(), now);
        if i == 10_000 {
            old = Some(store.snapshot());
        }
        i += 1;
    }
    let snap = store.snapshot();
    let first = snap.first_line().0;
    let hits = search(&snap, "needle", 0, false, usize::MAX);
    assert!(!hits.is_empty());
    assert!(
        hits.iter()
            .all(|m| m.line.0 >= first && m.line.0 % 1000 == 0)
    );
    assert_eq!(hits[0].line.0, first.div_ceil(1000) * 1000);
    let back = search(&snap, "needle", 0, true, 10);
    assert!(
        back.is_empty(),
        "from below first_line clamps to nothing backward"
    );
    // The old snapshot still finds matches the store has since evicted.
    let old = old.unwrap();
    let old_hits = search(&old, "needle", 0, false, 3);
    assert_eq!(old_hits[0].line, LineId(0));
}

#[test]
fn text_export_modes() {
    let mut store = Store::default();
    let base = t0(&store);
    store.append_local_at("hello", Direction::Notice, base);
    store.append(b"first\r\n", base + Duration::from_millis(1500));
    store.append_local_at("sent", Direction::Tx, base + Duration::from_millis(1600));
    store.append(
        b"\x1b[32msecond\x1b[0m\n",
        base + Duration::from_millis(2000),
    );
    let snap = store.snapshot();
    let all = snap.first_line()..snap.end();
    assert_eq!(
        snap.text(all.clone(), TextOptions::default()),
        "hello\nfirst\nsent\nsecond\n"
    );
    assert_eq!(
        snap.text(all.clone(), TextOptions::received_only()),
        "first\nsecond\n"
    );
    let rel = TextOptions::default().with_timestamps(Timestamps::Relative);
    assert_eq!(
        snap.text(all.clone(), rel),
        "[+0.000000] hello\n[+1.500000] first\n[+1.600000] sent\n[+2.000000] second\n"
    );
    let delta = TextOptions::received_only().with_timestamps(Timestamps::Delta);
    assert_eq!(
        snap.text(all.clone(), delta),
        "[+0.000000] first\n[+0.500000] second\n"
    );
    let abs = TextOptions::received_only().with_timestamps(Timestamps::Absolute);
    let wall = store.epoch().wall + Duration::from_millis(1500);
    let text = snap.text(all.clone(), abs);
    assert!(
        text.starts_with(&format!("[{}] first\n", format_utc(wall))),
        "{text}"
    );
    // Ranges clip.
    assert_eq!(
        snap.text(LineId(1)..LineId(2), TextOptions::default()),
        "first\n"
    );
    assert_eq!(snap.text(LineId(9)..LineId(20), TextOptions::default()), "");
}

#[test]
fn hex_view_rows() {
    let mut store = Store::default();
    let now = Instant::now();
    store.append(b"Hello\r\n\x00\x01\x02\x03\x04\x05\x06\x07\x08\xff", now);
    let snap = store.snapshot();
    let hex = snap.hex_view(16);
    assert_eq!(hex.first_line(), LineId(0));
    assert_eq!(hex.line_count(), 2);
    let row = hex.line(LineId(0)).unwrap();
    assert_eq!(
        row.text,
        "00000000  48 65 6c 6c 6f 0d 0a 00  01 02 03 04 05 06 07 08  |Hello...........|"
    );
    assert_eq!(row.runs.len(), 3);
    assert_eq!(row.runs[0].len, 10);
    assert_eq!(
        &row.text[10..10 + row.runs[1].len],
        "48 65 6c 6c 6f 0d 0a 00  01 02 03 04 05 06 07 08  "
    );
    assert!(row.text.ends_with("|Hello...........|"));
    assert_eq!(row.raw, 0..16);
    assert!(row.complete);
    check_line(&row);
    let last = hex.line(LineId(1)).unwrap();
    assert_eq!(
        last.text,
        format!("00000010  ff{}  |.|", " ".repeat(3 * 15 + 1))
    );
    assert!(!last.complete);
    assert_eq!(hex.line(LineId(2)), None);
    // Hex rows count against stream offsets, not lines.
    let narrow = snap.hex_view(4);
    assert_eq!(narrow.line_count(), 5);
    assert_eq!(
        narrow.line(LineId(1)).unwrap().text,
        "00000004  6f 0d 0a 00  |o...|"
    );
}

#[test]
fn styles_survive_the_store() {
    let mut store = Store::default();
    let now = Instant::now();
    store.append(
        b"\x1b[1;38;2;1;2;3;48;5;200mX\x1b[0m\x1b[4;91mY\x1b[0mZ\n",
        now,
    );
    let line = store.snapshot().line(LineId(0)).unwrap();
    assert_eq!(line.text, "XYZ");
    assert_eq!(line.runs[0].style.fg, Color::Rgb(1, 2, 3));
    assert_eq!(line.runs[0].style.bg, Color::Indexed(200));
    assert!(line.runs[0].style.flags.contains(StyleFlags::BOLD));
    assert_eq!(line.runs[1].style.fg, Color::Ansi(9));
    assert!(line.runs[1].style.flags.contains(StyleFlags::UNDERLINE));
    assert_eq!(line.runs[2].style, Style::default());
}

#[test]
fn empty_and_reader_views() {
    let store = Store::default();
    let reader = store.reader();
    let snap = reader.snapshot();
    assert_eq!(snap.line_count(), 0);
    assert_eq!(snap.line(LineId(0)), None);
    assert_eq!(raw_bytes(&snap), b"");
    assert_eq!(snap.hex_view(16).line_count(), 0);
    assert_eq!(snap.text(LineId(0)..LineId(10), TextOptions::default()), "");
    assert!(search(&snap, "x", 0, false, 10).is_empty());
    assert!(search(&snap, "x", 0, true, 10).is_empty());
    assert_eq!(reader.stats().end_line, LineId(0));
}

#[test]
fn texts_helper_matches_lines() {
    let mut store = Store::default();
    store.append(b"a\nb\nc", Instant::now());
    assert_eq!(texts(&store.snapshot()), vec!["a", "b", "c"]);
}

/// The reported crash: at the minimum budget, one local append of exactly a block of
/// large lines filled the open block while eviction removed every line, and the full,
/// unsealed block was drained (subtract overflow in debug, a panic in release).
#[test]
fn a_block_filled_while_everything_is_evicted() {
    let mut store = Store::with_budget(0);
    let line = "x".repeat(1000);
    let text = vec![line.as_str(); BLOCK_LINES].join("\n");
    store.append_local(&text, Direction::Tx);
    let stats = store.stats();
    assert!(stats.memory <= stats.budget, "{stats:?}");
    assert_eq!(stats.end_line, LineId(B));
    assert!(
        stats.first_line > LineId(0),
        "the budget forces eviction: {stats:?}"
    );
    // The store keeps working across the next blocks.
    store.append(b"after\n", Instant::now());
    store.append_local(&text, Direction::Tx);
    store.append_local(&text, Direction::Notice);
    let snap = store.snapshot();
    assert_eq!(snap.end(), LineId(3 * B + 1));
    assert_eq!(snap.line(LineId(3 * B)).unwrap().text, line);
    assert!(snap.stats().memory <= snap.stats().budget);
}

/// A megabyte-long line that changes style on every character must not leave the
/// parser's buffers (or the store's) pinned over budget once it ends.
#[test]
fn one_pathological_line_does_not_pin_memory() {
    let mut store = Store::new(StoreConfig {
        budget: 0,
        max_line_bytes: MAX_LINE_BYTES_LIMIT,
        ..StoreConfig::default()
    });
    let budget = store.budget();
    let now = Instant::now();
    let mut line = Vec::new();
    let mut i = 0;
    while line.len() < MAX_LINE_BYTES_LIMIT - 64 {
        line.extend_from_slice(format!("\x1b[3{}m\u{e9}", 1 + i % 7).as_bytes());
        i += 1;
    }
    for chunk in line.chunks(4096) {
        store.append(chunk, now);
        let stats = store.stats();
        assert!(
            stats.memory <= budget,
            "{} > {budget} mid-line",
            stats.memory
        );
    }
    store.append(b"\r\n", now);
    assert!(store.stats().memory <= budget);
    for i in 0..2000 {
        store.append(format!("normal {i}\r\n").as_bytes(), now);
    }
    let snap = store.snapshot();
    let stats = snap.stats();
    assert!(
        stats.memory < budget / 2,
        "{} held for budget {budget}",
        stats.memory
    );
    assert!(
        snap.line_count() > 2000,
        "later lines must be kept: {stats:?}"
    );
    assert_eq!(
        snap.line(LineId(snap.end().0 - 1)).unwrap().text,
        "normal 1999"
    );
}

/// A local line larger than the whole budget is let go, and its encoding buffer with it.
#[test]
fn a_local_line_larger_than_the_budget_is_let_go() {
    let mut store = Store::with_budget(0);
    let budget = store.budget();
    store.append_local(&"y".repeat(2 * budget), Direction::Notice);
    assert!(store.stats().memory <= budget);
    let now = Instant::now();
    for i in 0..2000 {
        store.append(format!("normal {i}\r\n").as_bytes(), now);
    }
    let snap = store.snapshot();
    let stats = snap.stats();
    assert!(
        stats.memory < budget / 2,
        "{} held for budget {budget}",
        stats.memory
    );
    assert!(snap.line_count() >= 2000, "{stats:?}");
}

#[test]
fn min_budget_scales_with_the_line_limit() {
    let at = |max_line_bytes| {
        StoreConfig {
            max_line_bytes,
            ..StoreConfig::default()
        }
        .min_budget()
    };
    let default = at(DEFAULT_MAX_LINE_BYTES);
    assert!(default < 4 * 1024 * 1024, "{default}");
    assert!(at(MAX_LINE_BYTES_LIMIT) > 16 * MAX_LINE_BYTES_LIMIT);
    assert_eq!(
        Store::with_budget(0).budget(),
        StoreConfig::default().min_budget()
    );
}

/// `count` lines `"{i:06} alpha"`, with `" needle"` added to every 1000th.
fn numbered_store(count: u64) -> Store {
    let mut store = Store::default();
    let now = Instant::now();
    let mut data = Vec::new();
    for i in 0..count {
        let mark = if i % 1000 == 0 { " needle" } else { "" };
        data.extend_from_slice(format!("{i:06} alpha{mark}\r\n").as_bytes());
        if data.len() >= 4096 {
            store.append(&data, now);
            data.clear();
        }
    }
    store.append(&data, now);
    store
}

fn search_in(
    snap: &Snapshot,
    pattern: &str,
    range: Range<u64>,
    from: u64,
    backward: bool,
    limit: usize,
) -> Vec<SearchMatch> {
    snap.search_in(
        pattern,
        LineId(range.start)..LineId(range.end),
        LineId(from),
        backward,
        limit,
        &AtomicBool::new(false),
    )
    .unwrap()
}

fn hit_lines(hits: &[SearchMatch]) -> Vec<u64> {
    hits.iter().map(|m| m.line.0).collect()
}

#[test]
fn bounded_search_never_returns_a_match_outside_its_range() {
    let store = numbered_store(20_000);
    let snap = store.snapshot();
    let needles: Vec<u64> = (6..=14).map(|k| k * 1000).collect();
    let mut reversed = needles.clone();
    reversed.reverse();
    // Lines 5500..15000: needles at 6000 through 14000; 5000 and 15000 are outside.
    let range = 5_500..15_000;
    let go =
        |from, backward, limit| search_in(&snap, "needle", range.clone(), from, backward, limit);
    assert_eq!(hit_lines(&go(0, false, usize::MAX)), needles);
    assert_eq!(hit_lines(&go(u64::MAX, true, usize::MAX)), reversed);
    // `from` is inclusive and is clamped into the range.
    assert_eq!(hit_lines(&go(10_000, true, usize::MAX)), reversed[4..]);
    assert_eq!(hit_lines(&go(10_000, false, usize::MAX)), needles[4..]);
    assert_eq!(hit_lines(&go(14_999, true, 2)), [14_000, 13_000]);
    assert_eq!(hit_lines(&go(0, false, 2)), [6_000, 7_000]);
    // Nothing when `from` lies on the wrong side of the range.
    assert!(go(5_499, true, 10).is_empty());
    assert!(go(15_000, false, 10).is_empty());
    assert!(go(20_000, false, 10).is_empty());
    assert!(go(0, false, 0).is_empty());
    // A pattern that matches every line yields exactly the range's lines, both ways,
    // so the bulk scan is cut at both ends.
    let all = r"^\d{6} alpha";
    let forward = search_in(&snap, all, range.clone(), 0, false, usize::MAX);
    assert_eq!(hit_lines(&forward), (5_500..15_000).collect::<Vec<_>>());
    let backward = search_in(&snap, all, range.clone(), u64::MAX, true, usize::MAX);
    assert_eq!(
        hit_lines(&backward),
        (5_500..15_000).rev().collect::<Vec<_>>()
    );
    // Small, empty and inverted ranges, and ranges past what is retained.
    assert_eq!(
        hit_lines(&search_in(&snap, "needle", 6_000..6_001, 0, false, 9)),
        [6_000]
    );
    assert_eq!(
        hit_lines(&search_in(&snap, "needle", 6_000..6_001, 9_999, true, 9)),
        [6_000]
    );
    assert!(search_in(&snap, "needle", 6_001..6_002, 0, false, 9).is_empty());
    assert!(search_in(&snap, "needle", 9_000..9_000, 0, false, 9).is_empty());
    let (later, earlier) = (9_000, 1_000);
    assert!(search_in(&snap, "needle", later..earlier, 0, true, 9).is_empty());
    assert_eq!(
        hit_lines(&search_in(&snap, "needle", 19_000..u64::MAX, 0, false, 9)),
        [19_000]
    );
    assert!(search_in(&snap, "needle", 30_000..40_000, 0, false, 9).is_empty());
    // The full range is what `search` does.
    let full = snap
        .search(
            "needle",
            LineId(0),
            false,
            usize::MAX,
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        full,
        search_in(&snap, "needle", 0..u64::MAX, 0, false, usize::MAX)
    );
    // A bad pattern is an error whatever the range, and a set cancel flag stops the scan.
    let cancelled = AtomicBool::new(true);
    let bad = snap.search_in("(", LineId(9)..LineId(9), LineId(0), false, 1, &cancelled);
    assert!(bad.is_err());
    let none = snap.search_in(
        "needle",
        LineId(0)..LineId(20_000),
        LineId(0),
        false,
        9,
        &cancelled,
    );
    assert!(none.unwrap().is_empty());
}

/// The reason for `search_in`: a backward search from a clear floor must not scan the
/// hidden lines below it. Counted in lines visited, not measured in time.
#[test]
fn a_bounded_search_scans_no_line_outside_its_range() {
    use super::search::{reset_scanned, scanned};

    let store = numbered_store(200_000);
    let snap = store.snapshot();
    let cancel = AtomicBool::new(false);
    // "needle" occurs only at multiples of 1000, all below the floor.
    let floor = 199_500;
    reset_scanned();
    let hits = search_in(&snap, "needle", floor..200_000, u64::MAX, true, usize::MAX);
    assert!(hits.is_empty());
    assert_eq!(scanned(), 200_000 - floor, "only the lines above the floor");
    // Unbounded, the same search walks down past the floor to line 199_000.
    reset_scanned();
    let hits = snap
        .search("needle", LineId(u64::MAX), true, 1, &cancel)
        .unwrap();
    assert_eq!(hit_lines(&hits), [199_000]);
    assert!(scanned() >= 1_000, "{}", scanned());
    // A forward search stops at the end of its range, and starts at its start.
    reset_scanned();
    let hits = search_in(&snap, "needle", 100_000..100_500, 0, false, usize::MAX);
    assert_eq!(hit_lines(&hits), [100_000]);
    assert_eq!(scanned(), 500);
    reset_scanned();
    let hits = search_in(&snap, "needle", 100_001..100_500, 0, false, usize::MAX);
    assert!(hits.is_empty());
    assert_eq!(scanned(), 499);
    // A limit stops a backward search after the window that held the match, which is
    // still inside the range.
    reset_scanned();
    let hits = search_in(&snap, "needle", 150_000..200_000, u64::MAX, true, 1);
    assert_eq!(hit_lines(&hits), [199_000]);
    assert!(scanned() < 50_000, "{}", scanned());
}

/// Eviction moves the store's first line up through the range: the retained part is what
/// is searched.
#[test]
fn a_bounded_search_clips_to_the_retained_lines() {
    let mut store = Store::new(StoreConfig {
        budget: 0,
        max_line_bytes: 4096,
        ..StoreConfig::default()
    });
    let now = Instant::now();
    let mut i = 0u64;
    while store.stats().first_line.0 < 3_000 {
        store.append(format!("{i:08} needle\r\n").as_bytes(), now);
        i += 1;
    }
    let snap = store.snapshot();
    let (first, end) = (snap.first_line().0, snap.end().0);
    let hits = search_in(&snap, "needle", 0..end, 0, false, usize::MAX);
    assert_eq!(hits.len() as u64, end - first);
    assert_eq!(hits[0].line.0, first);
    let hits = search_in(&snap, "needle", 0..first + 10, u64::MAX, true, usize::MAX);
    assert_eq!(
        hit_lines(&hits),
        (first..first + 10).rev().collect::<Vec<_>>()
    );
    // From below the retained lines, backward finds nothing, as `search` does.
    assert!(search_in(&snap, "needle", 0..end, first - 1, true, 5).is_empty());
}

/// Hex rows are rendered one at a time, so the bound saves real work: a backward search
/// from a floor renders only the rows above it.
#[test]
fn a_bounded_hex_search_renders_no_row_outside_its_range() {
    use super::search::{reset_scanned, scanned};

    let store = numbered_store(5_000);
    let snap = store.snapshot();
    let hex = snap.hex_view(16);
    let rows = hex.end().0;
    assert!(rows > 4_000, "{rows} rows");
    let cancel = AtomicBool::new(false);
    let floor = rows - 100;

    reset_scanned();
    let none = hex
        .search_in(
            "zzz",
            LineId(floor)..LineId(rows),
            LineId(u64::MAX),
            true,
            usize::MAX,
            &cancel,
        )
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(scanned(), 100, "only the rows above the floor");

    reset_scanned();
    let none = hex
        .search("zzz", LineId(u64::MAX), true, 1, &cancel)
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(scanned(), rows, "the unbounded search walks every row");

    // Forward, both ends of the range are respected.
    reset_scanned();
    let hits = hex
        .search_in(
            "^[0-9a-f]{8} ",
            LineId(10)..LineId(20),
            LineId(0),
            false,
            usize::MAX,
            &cancel,
        )
        .unwrap();
    assert_eq!(scanned(), 10);
    assert_eq!(hit_lines(&hits), (10..20).collect::<Vec<_>>());
    // A range past the retained rows clips to nothing.
    let none = hex
        .search_in(
            "^[0-9a-f]{8} ",
            LineId(rows)..LineId(rows + 50),
            LineId(0),
            false,
            usize::MAX,
            &cancel,
        )
        .unwrap();
    assert!(none.is_empty());
}

/// A store with one line per 16 bytes, so that each 16-byte hex row is one line.
fn timed_rows() -> Store {
    let mut store = Store::default();
    let base = t0(&store);
    store.append(b"0123456789abcde\n", base + Duration::from_millis(1000));
    store.append(b"fghijklmnopqrst\n", base + Duration::from_millis(3000));
    store.append(b"uvwxyz01234567\r\n", base + Duration::from_millis(3500));
    store
}

fn to_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).expect("exports are UTF-8")
}

#[test]
fn counted_text_export_reports_lines_and_bytes() {
    let mut store = Store::default();
    let base = t0(&store);
    store.append_local_at("hello", Direction::Notice, base);
    store.append(b"first\r\n", base + Duration::from_millis(1500));
    store.append_local_at("sent", Direction::Tx, base + Duration::from_millis(1600));
    store.append(
        b"\x1b[32msecond \xc3\xa9\x1b[0m\n",
        base + Duration::from_secs(2),
    );
    let snap = store.snapshot();
    let all = snap.first_line()..snap.end();
    for options in [
        TextOptions::default(),
        TextOptions::received_only(),
        TextOptions::default().with_timestamps(Timestamps::Absolute),
        TextOptions::default().with_timestamps(Timestamps::Relative),
        TextOptions::received_only().with_timestamps(Timestamps::Delta),
    ] {
        let mut out = Vec::new();
        let report = snap
            .write_text_counted(all.clone(), options, &mut out)
            .unwrap();
        let text = to_string(out);
        assert_eq!(
            report.lines,
            text.matches('\n').count(),
            "{options:?}: {text}"
        );
        assert_eq!(report.bytes, text.len() as u64, "{options:?}");
        assert_eq!(text, snap.text(all.clone(), options), "{options:?}");
        // The plain writer is the counting one without the counts.
        let mut plain = Vec::new();
        snap.write_text(all.clone(), options, &mut plain).unwrap();
        assert_eq!(to_string(plain), text);
    }
    let mut out = Vec::new();
    let report = snap
        .write_text_counted(all.clone(), TextOptions::received_only(), &mut out)
        .unwrap();
    assert_eq!(
        report,
        TextExportReport {
            lines: 2,
            bytes: "first\nsecond \u{e9}\n".len() as u64
        }
    );
    // A range with nothing in it writes nothing.
    let mut out = Vec::new();
    let report = snap
        .write_text_counted(LineId(40)..LineId(50), TextOptions::default(), &mut out)
        .unwrap();
    assert_eq!(report, TextExportReport::default());
    assert!(out.is_empty());
}

#[test]
fn write_lines_stamps_hex_rows_like_text_lines() {
    let store = timed_rows();
    let snap = store.snapshot();
    let hex = snap.hex_view(16);
    assert_eq!(hex.line_count(), 3);
    let rows: Vec<String> = (0..3).map(|i| hex.line(LineId(i)).unwrap().text).collect();
    let all = hex.first_line()..hex.end();
    let export = |timestamps| {
        let mut out = Vec::new();
        let options = TextOptions::default().with_timestamps(timestamps);
        let report = write_lines(&hex, all.clone(), options, &mut out).unwrap();
        (report, to_string(out))
    };

    let (report, plain) = export(Timestamps::None);
    assert_eq!(plain, format!("{}\n{}\n{}\n", rows[0], rows[1], rows[2]));
    assert_eq!(report.lines, 3);
    assert_eq!(report.bytes, plain.len() as u64);

    let (_, relative) = export(Timestamps::Relative);
    assert_eq!(
        relative,
        format!(
            "[+1.000000] {}\n[+3.000000] {}\n[+3.500000] {}\n",
            rows[0], rows[1], rows[2]
        )
    );
    let (_, delta) = export(Timestamps::Delta);
    assert_eq!(
        delta,
        format!(
            "[+0.000000] {}\n[+2.000000] {}\n[+0.500000] {}\n",
            rows[0], rows[1], rows[2]
        )
    );
    let (report, absolute) = export(Timestamps::Absolute);
    let wall = |ms| format_utc(store.epoch().wall + Duration::from_millis(ms));
    assert_eq!(
        absolute,
        format!(
            "[{}] {}\n[{}] {}\n[{}] {}\n",
            wall(1000),
            rows[0],
            wall(3000),
            rows[1],
            wall(3500),
            rows[2]
        )
    );
    assert_eq!(report.bytes, absolute.len() as u64);

    // The row range clips, and the store's own lines go through the same function.
    let mut out = Vec::new();
    let options = TextOptions::default().with_timestamps(Timestamps::Delta);
    write_lines(&hex, LineId(1)..LineId(99), options, &mut out).unwrap();
    assert_eq!(
        to_string(out),
        format!("[+0.000000] {}\n[+0.500000] {}\n", rows[1], rows[2])
    );
    let mut from_snapshot = Vec::new();
    let mut from_write_text = Vec::new();
    let all = snap.first_line()..snap.end();
    write_lines(&snap, all.clone(), options, &mut from_snapshot).unwrap();
    snap.write_text(all, options, &mut from_write_text).unwrap();
    assert_eq!(from_snapshot, from_write_text);
}

/// The writer may be a trait object, which is what a caller with a boxed sink has.
#[test]
fn write_lines_takes_a_dyn_writer_and_reports_io_errors() {
    struct Full(usize);

    impl std::io::Write for Full {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.0 < buf.len() {
                return Err(std::io::Error::other("disk full"));
            }
            self.0 -= buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let store = timed_rows();
    let snap = store.snapshot();
    let mut sink: Box<dyn std::io::Write> = Box::new(Vec::new());
    let report = write_lines(
        &snap,
        snap.first_line()..snap.end(),
        TextOptions::default(),
        &mut *sink,
    )
    .unwrap();
    assert_eq!(report.lines, 3);

    let mut full = Full(20);
    let error = write_lines(
        &snap,
        snap.first_line()..snap.end(),
        TextOptions::default(),
        &mut full,
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "disk full");
}
