//! Store benchmarks: append throughput by content, snapshot cost, line lookup and
//! search over a million lines, and the line index under a window read, eviction and
//! tiny lines. Input comes from the simulator's firehose generator, so no hardware or
//! link is involved. The parser alone is in parse.rs.

use std::hint::black_box;
use std::io::Write as _;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use serialist_core::{LineId, LineSource, Searcher, Store, StoreConfig};
use serialist_sim::{FirehoseContent, FirehoseGenerator};

const APPEND_BYTES: usize = 4 * 1024 * 1024;
const CHUNK: usize = 4096;
const LINES: u64 = 1_000_000;

fn firehose(content: FirehoseContent) -> Vec<u8> {
    let mut data = Vec::with_capacity(APPEND_BYTES);
    FirehoseGenerator::new(content, 1).fill(&mut data, APPEND_BYTES);
    data
}

fn append(c: &mut Criterion) {
    let mut group = c.benchmark_group("append");
    group.throughput(Throughput::Bytes(APPEND_BYTES as u64));
    group.sample_size(20);
    for (name, content) in [
        ("plain", FirehoseContent::Text),
        ("ansi", FirehoseContent::Ansi),
        ("long_lines", FirehoseContent::LongLines),
    ] {
        let data = firehose(content);
        group.bench_function(name, |b| {
            b.iter_batched(
                || Store::new(StoreConfig::default()),
                |mut store| {
                    let now = Instant::now();
                    for chunk in data.chunks(CHUNK) {
                        store.append(chunk, now);
                    }
                    store
                },
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

/// A store of a million short lines; the last one is the only `needle`.
fn million_lines() -> Store {
    let mut store = Store::new(StoreConfig::default());
    let mut buf = Vec::with_capacity(8192);
    let now = Instant::now();
    for i in 0..LINES {
        let kind = if i + 1 == LINES {
            "needle"
        } else {
            "status ok"
        };
        write!(buf, "{i:08} sensor temp={} {kind}\r\n", i % 90).expect("write to vec");
        if buf.len() >= CHUNK || i + 1 == LINES {
            store.append(&buf, now);
            buf.clear();
        }
    }
    store
}

fn reads(c: &mut Criterion) {
    let store = million_lines();
    let reader = store.reader();
    let snap = reader.snapshot();

    c.bench_function("snapshot", |b| b.iter(|| black_box(reader.snapshot())));

    let mut next = 0x2545_F491_4F6C_DD1Du64;
    c.bench_function("line_lookup_1m", |b| {
        b.iter(|| {
            next ^= next << 13;
            next ^= next >> 7;
            next ^= next << 17;
            black_box(snap.line(LineId(next % LINES)))
        });
    });

    let mut group = c.benchmark_group("search_1m");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    let cancel = AtomicBool::new(false);
    group.bench_function("forward_full_scan", |b| {
        b.iter(|| {
            snap.search("needle", LineId(0), false, 1, &cancel)
                .expect("pattern")
        });
    });
    group.bench_function("backward_full_scan", |b| {
        b.iter(|| {
            snap.search("^00000000 ", LineId(LINES), true, 1, &cancel)
                .expect("pattern")
        });
    });
    // The same backward search as above, from a Clear floor 10 000 lines below the end:
    // the hidden 990 000 lines are not scanned.
    let floor = LineId(LINES - 10_000)..LineId(LINES);
    group.bench_function("backward_above_floor", |b| {
        b.iter(|| {
            snap.search_in("^00000000 ", floor.clone(), LineId(LINES), true, 1, &cancel)
                .expect("pattern")
        });
    });
    group.bench_function("regex_full_scan", |b| {
        b.iter(|| {
            snap.search(r"temp=9\d needle", LineId(0), false, 1, &cancel)
                .expect("pattern")
        });
    });
    group.finish();
}

/// One xorshift64 step, for ids that jump around the store the way a scrollbar drag does.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// A store held at its minimum budget after four times what it can keep, so the oldest
/// lines were evicted and `first_line` is well past zero.
fn evicted_store() -> Store {
    let mut store = Store::new(StoreConfig::with_budget(0));
    let data = firehose(FirehoseContent::Text);
    let now = Instant::now();
    for _ in 0..4 {
        for chunk in data.chunks(CHUNK) {
            store.append(chunk, now);
        }
    }
    let stats = store.stats();
    assert!(
        stats.first_line.0 > 0,
        "the bench needs a store that evicted"
    );
    assert!(
        stats.memory <= stats.budget,
        "the store stays within budget"
    );
    store
}

/// What the line index costs on the paths the UI and the appender use: a frame's worth of
/// lines, a lookup once the front of the index has been evicted, and an append where
/// every line is two bytes, so index work dominates.
fn line_index(c: &mut Criterion) {
    let mut group = c.benchmark_group("line_index");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));

    // What a frame does: read the 60 consecutive lines on screen, wherever the view is.
    const WINDOW: u64 = 60;
    let store = million_lines();
    let snap = store.snapshot();
    let mut next = 0x9E37_79B9_7F4A_7C15u64;
    let mut out = Vec::with_capacity(WINDOW as usize);
    group.throughput(Throughput::Elements(WINDOW));
    group.bench_function("visible_window_1m", |b| {
        b.iter(|| {
            let first = xorshift(&mut next) % (LINES - WINDOW);
            out.clear();
            snap.lines(LineId(first)..LineId(first + WINDOW), &mut out);
            black_box(out.len())
        });
    });

    // Lookups in a store whose index front was evicted: ids start at `first_line`, not 0.
    let evicted = evicted_store();
    let snap = evicted.snapshot();
    let (first, end) = (snap.first_line().0, snap.end().0);
    group.throughput(Throughput::Elements(1));
    group.bench_function("lookup_after_eviction", |b| {
        b.iter(|| {
            let id = first + xorshift(&mut next) % (end - first);
            black_box(snap.line(LineId(id)).expect("retained line"))
        });
    });

    // 4 MiB of one-character lines: two million index entries.
    let tiny = "x\n".repeat(APPEND_BYTES / 2).into_bytes();
    group.sample_size(10);
    group.throughput(Throughput::Bytes(tiny.len() as u64));
    group.bench_function("append_tiny_lines", |b| {
        b.iter_batched(
            || Store::new(StoreConfig::default()),
            |mut store| {
                let now = Instant::now();
                for chunk in tiny.chunks(CHUNK) {
                    store.append(chunk, now);
                }
                store
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, append, reads, line_index);
criterion_main!(benches);
