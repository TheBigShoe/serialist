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
