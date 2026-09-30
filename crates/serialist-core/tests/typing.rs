//! Typing in line against a received stream. Text typed while bytes arrive is echoed as
//! local lines that must not depend on how the received bytes happen to be cut into
//! chunks, must not disturb the received lines, and must never change once they are in
//! the index. The rules are in the store's module docs under "Typing in line".

use std::time::Duration;

mod common;

use proptest::prelude::*;
use serialist_core::ansi::OwnedLine;
use serialist_core::{
    AnsiParser, Direction, Epoch, LineId, LineSource, Snapshot, Store, StoreConfig, StyledLine,
};

/// Something done to the store between two received bytes.
#[derive(Clone, Debug)]
enum Op {
    Type(String),
    Truncate(usize),
    /// A whole local line, which ends the received line in progress.
    Local(String),
}

fn op(with_local: bool) -> impl Strategy<Value = Op> {
    let typed = "[a-z ]{0,4}\n?[a-z\t]{0,3}".prop_map(Op::Type);
    let truncate = (1usize..5).prop_map(Op::Truncate);
    let local = "[A-Za-z]{1,6}".prop_map(Op::Local);
    if with_local {
        prop_oneof![6 => typed, 3 => truncate, 1 => local].boxed()
    } else {
        prop_oneof![6 => typed, 3 => truncate].boxed()
    }
}

/// A stream, operations at offsets in it, and extra cut points.
#[derive(Clone, Debug)]
struct Scenario {
    bytes: Vec<u8>,
    ops: Vec<(usize, Op)>,
    cuts: Vec<usize>,
}

fn scenario(with_local: bool) -> impl Strategy<Value = Scenario> {
    common::stream(60).prop_flat_map(move |bytes| {
        let len = bytes.len();
        let ops = prop::collection::vec((0..=len, op(with_local)), 0..14).prop_map(|mut ops| {
            // Operations at one offset keep their order; the rest go by offset.
            ops.sort_by_key(|(offset, _)| *offset);
            ops
        });
        let cuts = prop::collection::vec(0..=len, 0..16);
        (Just(bytes), ops, cuts).prop_map(|(bytes, ops, cuts)| Scenario { bytes, ops, cuts })
    })
}

/// A snapshot with where in the scenario it was taken: bytes fed and operations done.
type Tagged = ((usize, usize), Snapshot);

/// Everything happens on one epoch, chunks 1 ms in and typing 2 ms in, so the lines of
/// runs cut differently compare equal, arrival times included. Every operation is a cut
/// point; `extra_cuts` adds the scenario's own.
fn run(scenario: &Scenario, extra_cuts: bool, max_line: usize, epoch: Epoch) -> Vec<Tagged> {
    let mut store = Store::new(StoreConfig {
        max_line_bytes: max_line,
        epoch: Some(epoch),
        ..StoreConfig::default()
    });
    let mut points: Vec<usize> = scenario.ops.iter().map(|(offset, _)| *offset).collect();
    if extra_cuts {
        points.extend(&scenario.cuts);
    }
    points.push(scenario.bytes.len());
    points.sort_unstable();
    points.dedup();
    let chunk_at = epoch.instant + Duration::from_millis(1);
    let typed_at = epoch.instant + Duration::from_millis(2);
    let mut snapshots = vec![((0, 0), store.snapshot())];
    let mut done = 0;
    let mut next_op = 0;
    for point in points {
        if point > done {
            store.append(&scenario.bytes[done..point], chunk_at);
            done = point;
            snapshots.push(((done, next_op), store.snapshot()));
        }
        while let Some((offset, op)) = scenario.ops.get(next_op)
            && *offset == point
        {
            match op {
                Op::Type(text) => {
                    store.append_local_inline_at(text, Direction::Tx, typed_at);
                }
                Op::Truncate(chars) => {
                    store.truncate_local_line(*chars);
                }
                Op::Local(text) => {
                    store.append_local_at(text, Direction::Notice, typed_at);
                }
            }
            next_op += 1;
            snapshots.push(((done, next_op), store.snapshot()));
        }
    }
    snapshots
}

fn last(snapshots: &[Tagged]) -> &Snapshot {
    &snapshots.last().expect("at least the empty store").1
}

fn all_lines(snapshot: &Snapshot) -> Vec<StyledLine> {
    let mut out = Vec::new();
    snapshot.lines(LineId(0)..snapshot.end(), &mut out);
    out
}

fn parse(bytes: &[u8], max_line: usize) -> Vec<OwnedLine> {
    let mut parser = AnsiParser::with_max_line_bytes(max_line);
    let mut lines = Vec::new();
    parser.feed(bytes, |l| lines.push(l.into()));
    if let Some(l) = parser.current() {
        lines.push(l.into());
    }
    lines
}

fn max_line() -> impl Strategy<Value = usize> {
    prop_oneof![Just(64 * 1024), 16usize..120]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Cutting the received bytes into more chunks changes nothing about the lines.
    #[test]
    fn typed_lines_are_chunking_invariant(
        scenario in scenario(true),
        max_line in max_line(),
    ) {
        let epoch = Epoch::now();
        let whole = run(&scenario, false, max_line, epoch);
        let split = run(&scenario, true, max_line, epoch);
        prop_assert_eq!(all_lines(last(&whole)), all_lines(last(&split)));
        // Not just at the end: the same lines wherever both runs stopped, which includes
        // after every operation.
        for (tag, snapshot) in &whole {
            let other = split.iter().find(|(other_tag, _)| other_tag == tag);
            let Some((_, other)) = other else {
                continue;
            };
            prop_assert_eq!(all_lines(snapshot), all_lines(other), "at {:?}", tag);
            prop_assert_eq!(snapshot.committed_end(), other.committed_end(), "at {:?}", tag);
        }
    }

    /// A line that is in the index never changes afterwards, and ids never move.
    #[test]
    fn a_committed_line_never_changes(
        scenario in scenario(true),
        max_line in max_line(),
    ) {
        let epoch = Epoch::now();
        let snapshots = run(&scenario, true, max_line, epoch);
        let last = last(&snapshots);
        for (tag, snapshot) in &snapshots {
            prop_assert!(snapshot.committed_end() <= snapshot.end(), "at {:?}", tag);
            let mut committed = Vec::new();
            snapshot.lines(snapshot.first_line()..snapshot.committed_end(), &mut committed);
            for line in committed {
                prop_assert_eq!(last.line(line.id), Some(line), "at {:?}", tag);
            }
        }
    }

    /// Typing never disturbs the received lines: with no whole local lines (which end
    /// the line in progress), they are exactly what the parser makes of the stream.
    #[test]
    fn typing_leaves_the_received_lines_alone(
        scenario in scenario(false),
        max_line in max_line(),
    ) {
        let epoch = Epoch::now();
        let snapshots = run(&scenario, true, max_line, epoch);
        let received: Vec<(String, Vec<_>, bool, std::ops::Range<u64>)> =
            all_lines(last(&snapshots))
                .into_iter()
                .filter(|line| line.direction == Direction::Rx)
                .map(|line| (line.text, line.runs, line.complete, line.raw))
                .collect();
        let mut offset = 0u64;
        let parsed: Vec<_> = parse(&scenario.bytes, max_line)
            .into_iter()
            .map(|line| {
                let raw = offset..offset + line.raw_len as u64;
                offset = raw.end;
                (line.text, line.runs, line.complete, raw)
            })
            .collect();
        prop_assert_eq!(received, parsed);
    }

    /// Typed lines have no raw bytes of their own, and the stream is all there is.
    #[test]
    fn typed_lines_add_no_raw_bytes(
        scenario in scenario(true),
        max_line in max_line(),
    ) {
        let epoch = Epoch::now();
        let snapshots = run(&scenario, true, max_line, epoch);
        let last = last(&snapshots);
        let raw: Vec<u8> = last.raw(0..u64::MAX).flatten().copied().collect();
        prop_assert_eq!(raw, scenario.bytes.clone());
        for line in all_lines(last) {
            if line.direction != Direction::Rx {
                prop_assert!(line.raw.is_empty(), "{:?}", line);
                prop_assert!(!line.text.contains(['\n', '\r', '\t']), "{:?}", line);
            }
        }
    }
}
