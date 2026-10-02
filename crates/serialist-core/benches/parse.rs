//! ANSI parser benchmarks: `AnsiParser` alone, with no store behind it, over 4 MiB of
//! each kind of firehose content, plus the CR-overwrite cell-mode path. Input comes from
//! the simulator's firehose generator, so no hardware or link is involved.

use std::hint::black_box;
use std::time::Duration;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use serialist_core::AnsiParser;
use serialist_sim::{FirehoseContent, FirehoseGenerator};

const PARSE_BYTES: usize = 4 * 1024 * 1024;
/// What a USB-serial driver and the virtual link hand over.
const CHUNK: usize = 4096;

fn firehose(content: FirehoseContent) -> Vec<u8> {
    let mut data = Vec::with_capacity(PARSE_BYTES);
    FirehoseGenerator::new(content, 1).fill(&mut data, PARSE_BYTES);
    data
}

/// Feeds `data` to a fresh parser in `CHUNK`-byte pieces and returns what the lines
/// held, so the work cannot be optimized away.
fn parse_all(data: &[u8], mut parser: AnsiParser) -> (usize, usize) {
    let (mut lines, mut text) = (0usize, 0usize);
    for chunk in data.chunks(CHUNK) {
        parser.feed(chunk, |line| {
            lines += 1;
            text += line.text.len() + line.runs.len();
        });
    }
    (lines, text)
}

fn parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Bytes(PARSE_BYTES as u64));
    for (name, content) in [
        ("text", FirehoseContent::Text),
        ("ansi", FirehoseContent::Ansi),
        ("long_lines", FirehoseContent::LongLines),
        ("mixed", FirehoseContent::Mixed),
        ("binary", FirehoseContent::Binary),
    ] {
        let data = firehose(content);
        group.bench_function(name, |b| {
            b.iter(|| black_box(parse_all(&data, AnsiParser::new())));
        });
    }
    // Control bytes shown as placeholder glyphs: every complete line carries decoded text.
    let data = firehose(FirehoseContent::Ansi);
    group.bench_function("ansi_glyphs", |b| {
        b.iter(|| black_box(parse_all(&data, AnsiParser::new().show_control_chars(true))));
    });
    group.finish();
}

/// CR overwrite of a long line of multi-byte characters: the cell-mode path. Moved here
/// from store.rs as it was: same group, id and sampling, so a baseline still matches it.
fn overwrite(c: &mut Criterion) {
    let n = 15 * 1024;
    let mut input = "\u{e9}".repeat(n).into_bytes();
    input.push(b'\r');
    input.extend_from_slice("\u{e8}".repeat(n).as_bytes());
    input.push(b'\n');
    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("overwrite_non_ascii", |b| {
        b.iter(|| {
            let mut parser = AnsiParser::new();
            parser.feed(&input, |line| {
                black_box(line.text.len());
            });
        });
    });
    group.finish();
}

criterion_group!(benches, parse, overwrite);
criterion_main!(benches);
