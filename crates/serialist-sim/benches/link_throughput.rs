//! Throughput of an unpaced virtual link, and of the firehose generator and verifier
//! that tests put on either end of it.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serialist_sim::{
    DeviceOutput, FirehoseContent, FirehoseGenerator, FirehoseVerifier, LinkConfig, SimDevice,
    VirtualLink,
};

const TOTAL: usize = 8 * 1024 * 1024;

/// Sends one pre-built block over and over until `TOTAL` bytes are out, so the bench
/// measures the link rather than content generation.
struct BlastDevice {
    block: Arc<[u8]>,
    remaining: usize,
}

impl SimDevice for BlastDevice {
    fn name(&self) -> &str {
        "blast"
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {}

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let n = self.remaining.min(self.block.len());
        out.send(&self.block[..n]);
        self.remaining -= n;
        (self.remaining > 0).then_some(now)
    }
}

fn link(c: &mut Criterion) {
    let block: Arc<[u8]> = (0..64 * 1024).map(|i| i as u8).collect();
    let mut group = c.benchmark_group("virtual_link_unpaced");
    group.throughput(Throughput::Bytes(TOTAL as u64));
    group.sample_size(20);
    for max_chunk in [4096usize, 65_536] {
        group.bench_with_input(
            BenchmarkId::from_parameter(max_chunk),
            &max_chunk,
            |b, &max_chunk| {
                let mut buf = vec![0u8; 64 * 1024];
                b.iter(|| {
                    let device = BlastDevice {
                        block: Arc::clone(&block),
                        remaining: TOTAL,
                    };
                    let cfg = LinkConfig {
                        max_chunk,
                        ..LinkConfig::unpaced()
                    };
                    let (mut t, _handle) = VirtualLink::connect(Box::new(device), cfg);
                    let mut got = 0;
                    while got < TOTAL {
                        got += t
                            .reader
                            .read(&mut buf, Duration::from_millis(20))
                            .expect("link stays up");
                    }
                    black_box(got)
                });
            },
        );
    }
    group.finish();
}

fn firehose(c: &mut Criterion) {
    let mut group = c.benchmark_group("firehose");
    group.throughput(Throughput::Bytes(TOTAL as u64));
    group.sample_size(20);
    for content in [FirehoseContent::Mixed, FirehoseContent::Binary] {
        let mut bytes = Vec::with_capacity(TOTAL);
        FirehoseGenerator::new(content, 1).fill(&mut bytes, TOTAL);
        group.bench_function(BenchmarkId::new("generate", format!("{content:?}")), |b| {
            let mut out = Vec::with_capacity(TOTAL);
            b.iter(|| {
                out.clear();
                FirehoseGenerator::new(content, 1).fill(&mut out, TOTAL);
                black_box(out.len())
            });
        });
        group.bench_function(BenchmarkId::new("verify", format!("{content:?}")), |b| {
            b.iter(|| {
                let mut verifier = FirehoseVerifier::new(content);
                for chunk in bytes.chunks(4096) {
                    verifier.feed(chunk);
                }
                assert!(verifier.report().is_clean());
                black_box(verifier.report().records)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, link, firehose);
criterion_main!(benches);
