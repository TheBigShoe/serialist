//! Store benchmarks: append throughput by content, snapshot cost, line lookup and
//! search over a million lines. Input comes from the simulator's firehose generator,
//! so no hardware or link is involved.

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
    group.bench_function("regex_full_scan", |b| {
        b.iter(|| {
            snap.search(r"temp=9\d needle", LineId(0), false, 1, &cancel)
                .expect("pattern")
        });
    });
    group.finish();
}

criterion_group!(benches, append, reads);
criterion_main!(benches);
