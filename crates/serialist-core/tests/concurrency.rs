//! One thread appends a firehose while another snapshots and reads continuously.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serialist_core::{
    Direction, LineId, LineSource, Searcher, Snapshot, Store, StoreConfig, StyledLine,
};
use serialist_sim::{FirehoseContent, FirehoseGenerator};

/// SplitMix64, so the reader's choices are cheap and reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

fn check_line(snap: &Snapshot, line: &StyledLine, id: LineId) {
    assert_eq!(line.id, id);
    let total: usize = line.runs.iter().map(|r| r.len).sum();
    assert_eq!(total, line.text.len(), "runs cover {line:?}");
    assert!(line.runs.iter().all(|r| r.len > 0), "{line:?}");
    assert!(
        !line.text.chars().any(char::is_control),
        "control in {line:?}"
    );
    let raw = snap.raw_range();
    match line.direction {
        Direction::Rx => {
            assert!(!line.raw.is_empty(), "{line:?}");
            assert!(
                line.raw.start >= raw.start && line.raw.end <= raw.end,
                "{line:?}"
            );
            // A received line whose bytes are plain text is exactly those bytes.
            let bytes: Vec<u8> = snap.raw(line.raw.clone()).flatten().copied().collect();
            assert_eq!(bytes.len() as u64, line.raw.end - line.raw.start);
            let body = bytes.strip_suffix(b"\r\n").unwrap_or(&bytes);
            if line.complete && body.iter().all(|b| (0x20..0x7f).contains(b)) {
                assert_eq!(line.text.as_bytes(), body, "{line:?}");
            }
        }
        Direction::Tx | Direction::Notice => {
            assert!(line.raw.is_empty());
            assert!(line.text.starts_with("local "), "{line:?}");
        }
    }
}

#[test]
fn appends_and_reads_run_concurrently() {
    // The smallest budget allowed, so eviction runs all the time.
    let mut store = Store::new(StoreConfig::with_budget(0));
    let reader = store.reader();
    let done = Arc::new(AtomicBool::new(false));
    let run_for = Duration::from_secs(1);

    let writer_done = Arc::clone(&done);
    let writer = thread::spawn(move || {
        let mut generator = FirehoseGenerator::new(FirehoseContent::Mixed, 7);
        let mut chunk = Vec::with_capacity(8192);
        let mut sizes = Rng(1);
        let start = Instant::now();
        let mut appended = 0u64;
        let mut n = 0u64;
        while start.elapsed() < run_for {
            chunk.clear();
            generator.fill(&mut chunk, 1 + (sizes.next() % 6000) as usize);
            store.append(&chunk, Instant::now());
            appended += chunk.len() as u64;
            n += 1;
            if n.is_multiple_of(97) {
                store.append_local(&format!("local {n}"), Direction::Tx);
            }
        }
        writer_done.store(true, Ordering::Release);
        (store, appended)
    });

    let mut rng = Rng(2);
    let mut last_end = LineId(0);
    let mut last_first = LineId(0);
    let mut reads = 0u64;
    let mut snapshots = 0u64;
    while !done.load(Ordering::Acquire) {
        let snap = reader.snapshot();
        snapshots += 1;
        let (first, end) = (snap.first_line(), snap.end());
        assert!(end >= last_end, "end went back: {end:?} < {last_end:?}");
        assert!(first >= last_first, "first went back");
        assert_eq!(snap.line_count() as u64, end.0 - first.0);
        (last_end, last_first) = (end, first);
        if end == first {
            continue;
        }
        let span = end.0 - first.0;
        for _ in 0..32 {
            let id = LineId(first.0 + rng.next() % span);
            let line = snap.line(id).expect("a retained line reads back");
            check_line(&snap, &line, id);
            reads += 1;
        }
        // The newest line, often the one in progress.
        let newest = LineId(end.0 - 1);
        check_line(&snap, &snap.line(newest).expect("newest line"), newest);
        assert!(snap.line(end).is_none());
        if snapshots.is_multiple_of(64) {
            let hits = snap
                .search("ERROR|watchdog", first, false, 5, &AtomicBool::new(false))
                .expect("valid pattern");
            assert!(hits.iter().all(|m| m.line >= first && m.line < end));
            let hex = snap.hex_view(16);
            if let Some(row) = hex.line(LineId(hex.end().0 - 1)) {
                assert_eq!(row.runs.len(), 3);
            }
        }
    }

    let (store, appended) = writer.join().expect("writer thread");
    let stats = store.stats();
    assert_eq!(stats.raw_len, appended);
    assert!(stats.memory <= stats.budget, "{stats:?}");
    assert!(
        stats.evicted_lines > 0,
        "the budget should have forced eviction: {stats:?}"
    );
    assert!(reads > 1000, "only {reads} reads in {snapshots} snapshots");
}
