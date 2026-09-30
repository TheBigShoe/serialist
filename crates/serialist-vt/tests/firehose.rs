//! Throughput: 32 MiB of ANSI-colored log lines through `feed`, in 4 KiB chunks (what a
//! USB-serial driver and the virtual link hand over).
//!
//! The bound is loose, since this runs on shared CI machines: a few seconds in a release
//! build. A debug build of this crate is several times slower and gets a bound to match.
//! For the numbers:
//!
//! ```text
//! cargo test --release -p serialist-vt --test firehose -- --nocapture
//! ```

use std::time::{Duration, Instant};

use serialist_core::LineSource;
use serialist_sim::{FirehoseContent, FirehoseGenerator};
use serialist_vt::VtScreen;

const TOTAL: usize = 32 << 20;
const CHUNK: usize = 4096;

fn firehose() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(TOTAL);
    FirehoseGenerator::new(FirehoseContent::Ansi, 0x5eed).fill(&mut bytes, TOTAL);
    bytes
}

fn bound() -> Duration {
    if cfg!(debug_assertions) {
        Duration::from_secs(90)
    } else {
        Duration::from_secs(5)
    }
}

fn report(what: &str, elapsed: Duration) {
    let mib = TOTAL as f64 / (1024.0 * 1024.0);
    let secs = elapsed.as_secs_f64();
    eprintln!(
        "{what}: {mib:.0} MiB in {secs:.3} s = {:.1} MiB/s",
        mib / secs
    );
}

#[test]
fn a_32_mib_firehose_feeds_within_the_bound() {
    let bytes = firehose();
    let mut screen = VtScreen::new(120, 40, 10_000);
    let started = Instant::now();
    for chunk in bytes.chunks(CHUNK) {
        screen.feed(chunk);
    }
    let snapshot = screen.snapshot();
    let elapsed = started.elapsed();
    report("feed", elapsed);
    assert_eq!(snapshot.scrollback_lines(), 10_000);
    assert!(snapshot.first_line().0 > 100_000, "{snapshot:?}");
    // Records start with `#` and a sequence number; long ones wrap onto more rows.
    let mut lines = Vec::new();
    snapshot.lines(snapshot.first_line()..snapshot.first_visible(), &mut lines);
    let records = lines.iter().filter(|l| l.text.starts_with('#')).count();
    assert!(records > 5_000, "{records} records in {} rows", lines.len());
    assert!(elapsed < bound(), "{elapsed:?}");
}

/// What the sink does: a snapshot after every chunk.
#[test]
fn a_32_mib_firehose_with_a_snapshot_per_chunk_stays_within_the_bound() {
    let bytes = firehose();
    let mut screen = VtScreen::new(120, 40, 10_000);
    let started = Instant::now();
    let mut generations = 0;
    for chunk in bytes.chunks(CHUNK) {
        screen.feed(chunk);
        generations = screen.snapshot().generation();
    }
    let elapsed = started.elapsed();
    report("feed + snapshot per chunk", elapsed);
    assert_eq!(generations as usize, TOTAL / CHUNK);
    assert!(elapsed < bound(), "{elapsed:?}");
}
