//! VT screen benchmarks: `VtScreen` (alacritty_terminal fed without a PTY) over 4 MiB of
//! firehose output in 4 KiB chunks, and the same with a snapshot after every chunk, which
//! is what the UI asks for. Input comes from the simulator's firehose generator, so no
//! hardware or link is involved.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use serialist_core::LineSource;
use serialist_sim::{FirehoseContent, FirehoseGenerator};
use serialist_vt::{DEFAULT_SCROLLBACK, VtScreen};

const FEED_BYTES: usize = 4 * 1024 * 1024;
/// What a USB-serial driver and the virtual link hand over.
const CHUNK: usize = 4096;

fn firehose(content: FirehoseContent) -> Vec<u8> {
    let mut data = Vec::with_capacity(FEED_BYTES);
    FirehoseGenerator::new(content, 1).fill(&mut data, FEED_BYTES);
    data
}

fn screen() -> VtScreen {
    VtScreen::new(80, 24, DEFAULT_SCROLLBACK)
}

fn vt_feed(c: &mut Criterion) {
    let at = Instant::now();
    let mut group = c.benchmark_group("vt_feed");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Bytes(FEED_BYTES as u64));

    let ansi = firehose(FirehoseContent::Ansi);
    let text = firehose(FirehoseContent::Text);
    for (name, data) in [("ansi", &ansi), ("text", &text)] {
        group.bench_function(name, |b| {
            b.iter_batched(
                screen,
                |mut screen| {
                    for chunk in data.chunks(CHUNK) {
                        screen.feed_at(chunk, at);
                    }
                    screen
                },
                BatchSize::LargeInput,
            );
        });
    }

    group.bench_function("ansi_snapshot_per_chunk", |b| {
        b.iter_batched(
            screen,
            |mut screen| {
                let mut lines = 0;
                for chunk in ansi.chunks(CHUNK) {
                    screen.feed_at(chunk, at);
                    lines += black_box(screen.snapshot()).line_count();
                }
                (screen, lines)
            },
            BatchSize::LargeInput,
        );
    });
    group.finish();
}

criterion_group!(benches, vt_feed);
criterion_main!(benches);
