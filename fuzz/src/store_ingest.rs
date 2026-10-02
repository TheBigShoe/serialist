//! `store_ingest`: [`Store::append`] on arbitrary bytes in arbitrary chunks, and the
//! store's memory accounting.
//!
//! The input is an [`Input`]. Config bit 0 turns on [`StoreConfig::show_control_chars`];
//! bits 1..=2 pick `max_line_bytes` from [`MAX_LINE`]: the default for 0, then small
//! values so short inputs reach the line limit; bit 3 asks for the eviction mode (see
//! [`EVICT_ONE_IN`]); bits 4..=7 are unused. Config 0 is the realistic setting: the
//! default line limit, no control glyphs, no eviction.
//!
//! Each input gets one [`Epoch`] and every chunk is appended at the same instant, 1 ms
//! after it, as `store()` in crates/serialist-core/tests/chunking.rs does, so timestamps
//! compare equal across chunkings.
//!
//! # Chunking mode (bit 3 off, or not picked for eviction)
//!
//! One store is built from `input.chunks()` and one from the whole stream, both with the
//! default budget, so nothing is evicted. The checks beyond not panicking are the oracle
//! of `store_is_chunking_invariant` in that test, plus the state between chunks:
//!
//! - Chunking does not matter: every line (`Snapshot::lines` over the whole range) is
//!   equal in the two stores, timestamps included.
//! - The lines are what [`AnsiParser`] makes of the whole stream, configured as the store
//!   is: same text, runs and `complete`. The raw range of each line is the stretch of the
//!   stream it consumed, so the ranges tile `0..stream.len()`. Every line is received
//!   (`Direction::Rx`), stamped with the chunk time and well formed (runs cover the text,
//!   none is empty, neighbours differ, no control characters). `Snapshot::raw` over every
//!   offset concatenates to the stream.
//! - Every publication is right, not only the last: after each `append` the lines in
//!   [`AppendReport::changed`] (the line in progress that the chunk continued, the lines
//!   it ended, the new line in progress) read back as a second parser, fed the same
//!   chunks, has them, and the report's `new_lines`, `incomplete`, `committed_end` and
//!   `changed` agree with it. That is what the UI sees between two chunks. It stops after
//!   [`LIVE_WORK`] bytes of lines: re-reading a long line after every byte of it is
//!   quadratic.
//! - The store's counters agree with the reports after every append (see below).
//!
//! # Eviction mode (bit 3 on, and picked)
//!
//! The budget is 0, which the store raises to [`StoreConfig::min_budget`]: 1.6 to 1.9 MiB
//! for the small line limits, 3.1 MiB for the default one. (The task asked to append 1
//! MiB, which never reaches the budget.) The chunks go in again and again until the raw
//! bytes appended reach 1.5 times the budget, so the store evicts many pages. After every
//! `append`:
//!
//! - `memory <= budget`, whatever the input: that is `min_budget`'s promise.
//! - The accounting is consistent: `memory` is at least the retained raw bytes (raw
//!   pages are counted whole), `raw_len` is the bytes appended, `raw_start` (page
//!   aligned) is the sum of the reports' `evicted_bytes`, `first_line` the sum of their
//!   `evicted_lines`, `end_line` the sum of their `new_lines`, and the report's `changed`
//!   ends at `end_line` and its `committed_end` never moves back.
//!
//! At the end `first_line` has moved past 0 (the raw bytes outgrew the budget, so
//! something had to go), `Snapshot::raw` is exactly the tail of what was appended from
//! `raw_start`, and every retained line, `first_line..end`, reads back and equals the
//! line the parser made from the whole stream at that id, with the raw range the parser
//! says. The whole-stream comparison is skipped: a store that evicted has no lines below
//! `first_line` to compare.
//!
//! The input's own chunking is used for the first [`REAL_APPENDS`] appends, and the
//! per-publication check above for the first [`LIVE_APPENDS`] of them. After that the
//! stream, repeated to [`BULK_BYTES`], goes in as one chunk until the target is reached, so
//! an input of tiny chunks cannot run for minutes. An empty stream has nothing to repeat
//! and is skipped.
//!
//! # Why eviction is rare
//!
//! An eviction input ingests 2 to 5 MiB and then reads it all back: about half a second
//! of CPU under the fuzzer's instrumentation, against about a millisecond for a chunking
//! input. libFuzzer does not weigh an input by the time it takes, and mutating the stream
//! keeps the config byte, so with bit 3 alone choosing the mode every corpus entry that
//! has it on breeds more eviction inputs and the run spends nearly all of its time there
//! (the first, ungated 30 s run did 19 executions). So only one input in
//! [`EVICT_ONE_IN`] with bit 3 on, picked by a hash of all its bytes, takes the slow path;
//! the others run the chunking checks. Any mutation changes the hash, so no corpus entry
//! breeds eviction inputs. A seed that must run in eviction mode has two chunk lengths
//! tuned until its hash passes (the `evict_*` seeds), and `eviction_seeds_are_picked`
//! checks that they still are.

use std::time::{Duration, Instant, SystemTime};

use serialist_core::ansi::{AnsiParser, DEFAULT_MAX_LINE_BYTES, ParsedLine};
use serialist_core::store::PAGE_SIZE;
use serialist_core::{
    AppendReport, Direction, Epoch, LineId, LineSource, Snapshot, Store, StoreConfig, StyleRun,
    StyledLine,
};

use crate::Input;

/// The `max_line_bytes` values config bits 1..=2 choose from.
pub const MAX_LINE: [usize; 4] = [DEFAULT_MAX_LINE_BYTES, 16, 64, 4096];

/// One input in this many with config bit 3 on runs the eviction mode; see the module docs.
/// Set it to 1 and bit 3 alone picks the mode.
pub const EVICT_ONE_IN: u64 = 256;

/// In eviction mode, the appends that use the input's own chunking before the stream goes
/// in as big chunks.
pub const REAL_APPENDS: usize = 1024;

/// In eviction mode, the appends checked one by one against the second parser.
pub const LIVE_APPENDS: usize = 64;

/// The most raw bytes of lines the per-publication check looks at before it stops. Each
/// append re-reads the line in progress, and the parser re-syncs it, so feeding one long
/// line in small chunks costs the square of its length.
pub const LIVE_WORK: usize = 256 * 1024;

/// In eviction mode, the size of the repeated stream appended once `REAL_APPENDS` is used.
pub const BULK_BYTES: usize = 32 * 1024;

pub fn run(data: &[u8]) {
    let input = Input::parse(data);
    let setup = Setup::new(&input);
    if takes_eviction_mode(data) {
        eviction(&input, &setup);
    } else {
        chunking(&input, &setup);
    }
}

/// Whether `data` runs the eviction mode: config bit 3 is on and one hash in
/// [`EVICT_ONE_IN`] says so (FNV-1a, 64 bit, of every byte of the input).
pub fn takes_eviction_mode(data: &[u8]) -> bool {
    let hash = data.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    Input::parse(data).flag(3) && hash.is_multiple_of(EVICT_ONE_IN)
}

/// What every store of one input shares.
struct Setup {
    show: bool,
    max_line: usize,
    epoch: Epoch,
    /// When every chunk arrives.
    at: Instant,
}

impl Setup {
    fn new(input: &Input) -> Self {
        let epoch = Epoch {
            instant: Instant::now(),
            wall: SystemTime::UNIX_EPOCH,
        };
        Self {
            show: input.flag(0),
            max_line: input.pick(1, &MAX_LINE[..]),
            at: epoch.instant + Duration::from_millis(1),
            epoch,
        }
    }

    /// The reference: the parser the store is documented to be built on.
    fn parser(&self) -> AnsiParser {
        AnsiParser::with_max_line_bytes(self.max_line).show_control_chars(self.show)
    }

    fn feed(&self, budget: usize, live: usize) -> Feed<'_> {
        let store = Store::new(StoreConfig {
            budget,
            max_line_bytes: self.max_line,
            epoch: Some(self.epoch),
            show_control_chars: self.show,
        });
        Feed {
            setup: self,
            store,
            oracle: (live > 0).then(|| Oracle {
                parser: self.parser(),
                ended: 0,
                offset: 0,
                in_progress: false,
                work: 0,
            }),
            live,
            bytes: 0,
            new_lines: 0,
            evicted_lines: 0,
            evicted_bytes: 0,
            committed: LineId(0),
        }
    }
}

fn chunking(input: &Input, setup: &Setup) {
    let whole = feed_all(setup, std::iter::once(input.stream));
    let split = feed_all(setup, input.chunks());
    let (whole, split) = (whole.store.snapshot(), split.store.snapshot());

    let whole_lines = lines(&whole);
    let split_lines = lines(&split);
    assert_eq!(split_lines, whole_lines, "chunking changed the lines");
    assert_eq!(split.line_count(), split_lines.len());
    assert_eq!(split.first_line(), LineId(0), "nothing was evicted");

    check_retained(&split, input.stream, setup);
}

/// A store fed `chunks`, every publication checked.
fn feed_all<'a>(setup: &Setup, chunks: impl Iterator<Item = &'a [u8]>) -> Feed<'_> {
    let mut feed = setup.feed(0, usize::MAX);
    for chunk in chunks {
        feed.append(chunk);
    }
    feed
}

fn eviction(input: &Input, setup: &Setup) {
    if input.stream.is_empty() {
        return;
    }
    let mut feed = setup.feed(0, LIVE_APPENDS);
    let budget = feed.store.budget();
    let target = budget + budget / 2;

    let mut sent = Vec::with_capacity(target + BULK_BYTES + input.stream.len());
    let mut appends = 0;
    while sent.len() < target && appends < REAL_APPENDS {
        for chunk in input.chunks() {
            feed.append(chunk);
            sent.extend_from_slice(chunk);
            appends += 1;
            if sent.len() >= target || appends >= REAL_APPENDS {
                break;
            }
        }
    }
    if sent.len() < target {
        let mut bulk = Vec::with_capacity(BULK_BYTES + input.stream.len());
        while bulk.len() < BULK_BYTES {
            bulk.extend_from_slice(input.stream);
        }
        while sent.len() < target {
            feed.append(&bulk);
            sent.extend_from_slice(&bulk);
        }
    }

    let snapshot = feed.store.snapshot();
    let stats = snapshot.stats();
    assert_eq!(stats.raw_len, sent.len() as u64);
    assert!(
        stats.raw_len > budget as u64 && stats.first_line > LineId(0),
        "{} bytes went into a budget of {budget} and nothing was evicted: {stats:?}",
        stats.raw_len
    );
    check_retained(&snapshot, &sent, setup);
}

/// A store being fed, with the checks that run after every append.
struct Feed<'a> {
    setup: &'a Setup,
    store: Store,
    /// Checks each publication; dropped after `live` appends.
    oracle: Option<Oracle>,
    live: usize,
    /// What the reports add up to, against the store's own counters.
    bytes: u64,
    new_lines: u64,
    evicted_lines: u64,
    evicted_bytes: u64,
    committed: LineId,
}

impl Feed<'_> {
    fn append(&mut self, chunk: &[u8]) {
        let report = self.store.append(chunk, self.setup.at);
        self.bytes += chunk.len() as u64;
        self.new_lines += report.new_lines as u64;
        self.evicted_lines += report.evicted_lines as u64;
        self.evicted_bytes += report.evicted_bytes;

        let stats = self.store.stats();
        let at = self.bytes;
        assert_eq!(stats.budget, self.store.budget());
        assert!(
            stats.memory <= stats.budget,
            "after {at} bytes the store holds {} bytes of a budget of {}: {stats:?}",
            stats.memory,
            stats.budget
        );
        assert!(
            stats.memory as u64 >= stats.retained_bytes(),
            "after {at} bytes the store counts {} bytes for {} retained: {stats:?}",
            stats.memory,
            stats.retained_bytes()
        );
        assert_eq!(stats.raw_len, at, "{stats:?}");
        assert_eq!(stats.raw_start % PAGE_SIZE as u64, 0, "{stats:?}");
        assert_eq!(stats.raw_start, self.evicted_bytes, "{stats:?}");
        assert_eq!(stats.first_line.0, self.evicted_lines, "{stats:?}");
        assert_eq!(stats.evicted_lines, self.evicted_lines, "{stats:?}");
        assert_eq!(stats.end_line.0, self.new_lines, "{stats:?}");
        assert_eq!(stats.end_line, self.store.end());
        assert_eq!(report.changed.end, stats.end_line, "{report:?}");
        assert!(
            report.committed_end >= self.committed && report.committed_end <= stats.end_line,
            "committed_end went from {:?} to {:?}: {report:?}",
            self.committed,
            report.committed_end
        );
        self.committed = report.committed_end;

        if let Some(oracle) = &mut self.oracle {
            oracle.check(chunk, &report, &self.store.snapshot(), self.setup);
            self.live -= 1;
            if self.live == 0 || oracle.work > LIVE_WORK {
                self.oracle = None;
            }
        }
    }
}

/// A second parser fed the same chunks, to check what each append published.
struct Oracle {
    parser: AnsiParser,
    /// Lines that have ended, and where in the stream the next one starts.
    ended: u64,
    offset: u64,
    /// The parser had a line in progress before the chunk.
    in_progress: bool,
    /// Raw bytes of lines compared so far; see [`LIVE_WORK`].
    work: usize,
}

impl Oracle {
    fn check(&mut self, chunk: &[u8], report: &AppendReport, snapshot: &Snapshot, setup: &Setup) {
        let first = snapshot.first_line().0;
        let ended_before = self.ended;
        let end_before = ended_before + u64::from(self.in_progress);

        let mut work = 0;
        let (mut id, mut offset) = (self.ended, self.offset);
        self.parser.feed(chunk, |line| {
            if id >= first {
                check_line(snapshot, id, offset, line.into(), setup);
            }
            work += line.raw_len;
            id += 1;
            offset += line.raw_len as u64;
        });
        (self.ended, self.offset) = (id, offset);
        self.in_progress = false;
        if let Some(line) = self.parser.current() {
            if id >= first {
                check_line(snapshot, id, offset, line.into(), setup);
            }
            work += line.raw_len;
            self.in_progress = true;
        }
        self.work += work;

        let end_after = self.ended + u64::from(self.in_progress);
        assert_eq!(
            report.new_lines as u64,
            end_after - end_before,
            "{report:?}"
        );
        assert_eq!(report.incomplete, self.in_progress, "{report:?}");
        assert_eq!(report.committed_end, LineId(self.ended), "{report:?}");
        // An empty chunk changes nothing; any other re-publishes the line it continued.
        let from = if chunk.is_empty() {
            end_before
        } else {
            ended_before.max(first)
        };
        assert_eq!(
            report.changed,
            LineId(from)..LineId(end_after),
            "{report:?}"
        );
    }
}

/// What a line should hold, from the reference parser.
struct Want<'a> {
    text: &'a str,
    runs: &'a [StyleRun],
    raw_len: usize,
    complete: bool,
}

impl<'a> From<ParsedLine<'a>> for Want<'a> {
    fn from(line: ParsedLine<'a>) -> Self {
        Self {
            text: line.text,
            runs: line.runs,
            raw_len: line.raw_len,
            complete: line.complete,
        }
    }
}

/// Line `id` of `snapshot` is `want`, which starts at stream offset `raw_start`.
fn check_line(snapshot: &Snapshot, id: u64, raw_start: u64, want: Want<'_>, setup: &Setup) {
    let got = snapshot
        .line(LineId(id))
        .unwrap_or_else(|| panic!("line {id} is retained but does not read back"));
    assert_eq!(got.id, LineId(id));
    assert_eq!(got.text, want.text, "text of line {id}: {got:?}");
    assert_eq!(got.runs, want.runs, "runs of line {id}: {got:?}");
    assert_eq!(got.complete, want.complete, "line {id}: {got:?}");
    assert_eq!(
        got.raw,
        raw_start..raw_start + want.raw_len as u64,
        "raw range of line {id}: {got:?}"
    );
    assert_eq!(got.direction, Direction::Rx, "line {id}: {got:?}");
    assert_eq!(got.received_at, setup.at, "line {id}: {got:?}");
    check_well_formed(&got);
}

fn check_well_formed(line: &StyledLine) {
    let total: usize = line.runs.iter().map(|run| run.len).sum();
    assert_eq!(
        total,
        line.text.len(),
        "runs do not cover the text: {line:?}"
    );
    assert!(
        line.runs.iter().all(|run| run.len > 0),
        "empty run: {line:?}"
    );
    for pair in line.runs.windows(2) {
        assert_ne!(pair[0].style, pair[1].style, "runs not coalesced: {line:?}");
    }
    assert!(
        !line.text.chars().any(char::is_control),
        "control character in the text: {line:?}"
    );
}

/// Every retained line, in order.
fn lines(snapshot: &Snapshot) -> Vec<StyledLine> {
    let mut out = Vec::new();
    snapshot.lines(LineId(0)..snapshot.end(), &mut out);
    out
}

/// The snapshot, taken after `sent` was appended in all, holds exactly the tail of it
/// from its first retained line: the raw bytes, and every line against a parse of `sent`.
fn check_retained(snapshot: &Snapshot, sent: &[u8], setup: &Setup) {
    let stats = snapshot.stats();
    assert_eq!(stats.raw_len, sent.len() as u64);
    assert_eq!(snapshot.raw_range(), stats.raw_start..stats.raw_len);
    assert_eq!(snapshot.first_line(), stats.first_line);
    assert_eq!(snapshot.end(), stats.end_line);
    assert_eq!(snapshot.line_count(), stats.lines());

    let raw: Vec<u8> = snapshot.raw(0..u64::MAX).flatten().copied().collect();
    assert!(
        raw == sent[stats.raw_start as usize..],
        "the retained raw bytes are not the tail of the stream from {}",
        stats.raw_start
    );

    let first = snapshot.first_line().0;
    let mut parser = setup.parser();
    let (mut id, mut offset) = (0u64, 0u64);
    parser.feed(sent, |line| {
        if id >= first {
            check_line(snapshot, id, offset, line.into(), setup);
        }
        id += 1;
        offset += line.raw_len as u64;
    });
    if let Some(line) = parser.current() {
        if id >= first {
            check_line(snapshot, id, offset, line.into(), setup);
        }
        id += 1;
        offset += line.raw_len as u64;
    }
    assert_eq!(id, snapshot.end().0, "line count");
    assert_eq!(
        offset,
        sent.len() as u64,
        "raw lengths do not tile the stream"
    );

    assert!(snapshot.line(snapshot.end()).is_none());
    if first > 0 {
        assert!(snapshot.line(LineId(first - 1)).is_none());
    }
    if let Some(line) = snapshot.line(snapshot.first_line()) {
        assert!(
            line.raw.start >= stats.raw_start,
            "the first line starts below the retained bytes: {stats:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn seeds_replay() {
        crate::replay_seeds("store_ingest", super::run);
    }

    /// The `evict_*` seeds are there to run the eviction mode: editing one must not
    /// silently move it to the chunking mode.
    #[test]
    fn eviction_seeds_are_picked() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("seeds/store_ingest");
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).expect("seeds/store_ingest") {
            let path = entry.expect("a directory entry").path();
            let name = path
                .file_name()
                .expect("a name")
                .to_string_lossy()
                .into_owned();
            let data = std::fs::read(&path).expect("a seed");
            let picked = super::takes_eviction_mode(&data);
            assert_eq!(picked, name.starts_with("evict_"), "{name}");
            count += usize::from(picked);
        }
        assert!(count >= 3, "only {count} seeds run the eviction mode");
    }
}
