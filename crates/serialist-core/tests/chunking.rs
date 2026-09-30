//! The key property of the parser and store: splitting one byte stream at arbitrary
//! chunk boundaries yields identical lines and runs, and `raw()` gives back the input.

use std::time::Duration;

mod common;

use proptest::prelude::*;
use serialist_core::ansi::OwnedLine;
use serialist_core::{
    AnsiParser, Epoch, LineId, LineSource, Snapshot, Store, StoreConfig, StyledLine,
};

/// A stream and sorted cut points inside it.
fn stream_and_cuts() -> impl Strategy<Value = (Vec<u8>, Vec<usize>)> {
    common::stream(160).prop_flat_map(|bytes| {
        let len = bytes.len();
        let cuts = prop::collection::vec(0..=len, 0..24).prop_map(|mut c| {
            c.sort_unstable();
            c
        });
        (Just(bytes), cuts)
    })
}

fn chunks<'a>(bytes: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    for &cut in cuts {
        out.push(&bytes[at..cut]);
        at = cut;
    }
    out.push(&bytes[at..]);
    out
}

fn parse(bytes: &[u8], cuts: &[usize], max_line: usize) -> Vec<OwnedLine> {
    let mut parser = AnsiParser::with_max_line_bytes(max_line);
    let mut lines = Vec::new();
    for chunk in chunks(bytes, cuts) {
        parser.feed(chunk, |l| lines.push(l.into()));
    }
    if let Some(l) = parser.current() {
        lines.push(l.into());
    }
    lines
}

/// Every chunk arrives 1 ms after the previous one, on a shared epoch, so timestamps
/// are comparable across stores.
fn store(bytes: &[u8], cuts: &[usize], max_line: usize, epoch: Epoch) -> Snapshot {
    let mut store = Store::new(StoreConfig {
        max_line_bytes: max_line,
        epoch: Some(epoch),
        ..StoreConfig::default()
    });
    for chunk in chunks(bytes, cuts) {
        store.append(chunk, epoch.instant + Duration::from_millis(1));
    }
    store.snapshot()
}

fn all_lines(snap: &Snapshot) -> Vec<StyledLine> {
    let mut out = Vec::new();
    snap.lines(LineId(0)..snap.end(), &mut out);
    out
}

fn check_well_formed(line: &StyledLine) {
    let total: usize = line.runs.iter().map(|r| r.len).sum();
    assert_eq!(total, line.text.len(), "runs cover the text: {line:?}");
    assert!(
        line.runs.iter().all(|r| r.len > 0),
        "no empty runs: {line:?}"
    );
    for pair in line.runs.windows(2) {
        assert_ne!(pair[0].style, pair[1].style, "runs coalesced: {line:?}");
    }
    assert!(
        !line.text.chars().any(char::is_control),
        "no controls: {line:?}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn parser_is_chunking_invariant(
        (bytes, cuts) in stream_and_cuts(),
        max_line in prop_oneof![Just(64 * 1024), 16usize..120],
    ) {
        let whole = parse(&bytes, &[], max_line);
        let split = parse(&bytes, &cuts, max_line);
        prop_assert_eq!(&split, &whole);
        let total: usize = whole.iter().map(|l| l.raw_len).sum();
        prop_assert_eq!(total, bytes.len());
    }

    #[test]
    fn store_is_chunking_invariant(
        (bytes, cuts) in stream_and_cuts(),
        max_line in prop_oneof![Just(64 * 1024), 16usize..120],
    ) {
        let epoch = Epoch::now();
        let whole = store(&bytes, &[], max_line, epoch);
        let split = store(&bytes, &cuts, max_line, epoch);
        let whole_lines = all_lines(&whole);
        prop_assert_eq!(&all_lines(&split), &whole_lines);

        // Line text in the store (borrowed from raw pages when plain) is what the
        // parser produced, and raw ranges tile the stream.
        let parsed = parse(&bytes, &[], max_line);
        prop_assert_eq!(whole_lines.len(), parsed.len());
        let mut offset = 0u64;
        for (line, p) in whole_lines.iter().zip(&parsed) {
            check_well_formed(line);
            prop_assert_eq!(&line.text, &p.text);
            prop_assert_eq!(&line.runs, &p.runs);
            prop_assert_eq!(line.complete, p.complete);
            prop_assert_eq!(line.raw.clone(), offset..offset + p.raw_len as u64);
            offset += p.raw_len as u64;
        }

        let raw: Vec<u8> = split.raw(0..u64::MAX).flatten().copied().collect();
        prop_assert_eq!(raw, bytes);
    }
}

#[test]
fn every_single_cut_of_a_dense_sample() {
    let sample: &[u8] = b"boot \x1b[1;32mOK\x1b[0m\r\n10%\r50%\r\x1b[K100% \xe2\x9c\x93\r\n\
        tab\there\x08X\x1b[3Dz\x1b]0;t\x07 \xff\xfe end\x1b[38;2;1;2;3mc\x1b[m\n\
        \x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ caf\xc3\xa9 \xf0\x9f\x98\x80\n";
    let epoch = Epoch::now();
    let whole = all_lines(&store(sample, &[], 64 * 1024, epoch));
    for cut in 0..=sample.len() {
        for second in [
            cut,
            (cut + 1).min(sample.len()),
            (cut + 7).min(sample.len()),
        ] {
            let split = all_lines(&store(sample, &[cut, second], 64 * 1024, epoch));
            assert_eq!(split, whole, "cuts at {cut} and {second}");
        }
    }
}
