//! The firehose: a device that streams self-checking traffic at a chosen rate, and the
//! verifier that proves a consumer saw every byte of it in order.
//!
//! Record formats. All content except `Binary` is a stream of text lines:
//!
//! ```text
//! #<seq: lowercase hex, at least 8 digits> <payload> *<CRC-32 of everything before " *": 8 hex digits><eol>
//! ```
//!
//! The payload never contains CR or LF; `Ansi` payloads carry SGR escape sequences.
//! `Binary` content is a stream of frames that may contain any byte value:
//!
//! ```text
//! A5 5A | seq: u32 LE | len: u16 LE | payload[len] | CRC-32 of all preceding frame bytes: u32 LE
//! ```
//!
//! Sequence numbers start at 0 and count records, so a verifier that sees `n` after
//! `n - 2` knows exactly one record went missing.

use std::io::Write as _;
use std::time::{Duration, Instant};

use crate::crc::crc32;
use crate::link::later;
use crate::{DeviceOutput, SimDevice};

/// What the firehose sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FirehoseContent {
    /// Short log-style lines of words and `key=value` pairs, CRLF terminated.
    Text,
    /// Log lines with SGR colour, bold, underline, 256-colour and true-colour sequences.
    Ansi,
    /// Lines of 512 to 4096 printable characters.
    LongLines,
    /// Text lines ending in a random choice of LF, CRLF, CR or LFCR.
    MixedEol,
    /// Binary frames containing every byte value, CR and LF included.
    Binary,
    /// Each line picks one of `Text`, `Ansi`, `LongLines` and `MixedEol` at random.
    Mixed,
}

/// Rate, size and content of a firehose.
#[derive(Clone, Debug)]
pub struct FirehoseConfig {
    pub content: FirehoseContent,
    /// Target output rate. `None` sends as fast as the link accepts, which on a paced
    /// link is exactly the baud rate.
    pub bytes_per_second: Option<u64>,
    /// Stop after this many bytes. `None` streams forever.
    pub total_bytes: Option<u64>,
    pub seed: u64,
    /// Largest single send, in bytes.
    pub max_batch: usize,
    /// How often a rate-limited firehose wakes up. Values under [`MIN_TICK`] are raised
    /// to it, so a zero tick cannot spin the device thread.
    pub tick: Duration,
    /// Unplug the link once `total_bytes` have been sent.
    pub disconnect_when_done: bool,
}

impl Default for FirehoseConfig {
    fn default() -> Self {
        Self {
            content: FirehoseContent::Text,
            bytes_per_second: None,
            total_bytes: None,
            seed: 0,
            max_batch: 16 * 1024,
            tick: Duration::from_millis(2),
            disconnect_when_done: false,
        }
    }
}

impl FirehoseConfig {
    pub fn new(content: FirehoseContent) -> Self {
        Self {
            content,
            ..Self::default()
        }
    }

    pub fn with_rate(mut self, bytes_per_second: u64) -> Self {
        self.bytes_per_second = Some(bytes_per_second);
        self
    }

    pub fn with_total(mut self, total_bytes: u64) -> Self {
        self.total_bytes = Some(total_bytes);
        self
    }

    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }
}

/// The shortest `tick` a rate-limited firehose uses.
pub const MIN_TICK: Duration = Duration::from_millis(1);

/// SplitMix64: tiny, fast and plenty for content that only has to be deterministic.
#[derive(Clone, Debug)]
struct Prng(u64);

impl Prng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    fn range(&mut self, lo: u64, hi_inclusive: u64) -> u64 {
        lo + self.below(hi_inclusive - lo + 1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

const WORDS: &[&str] = &[
    "boot", "sensor", "temp", "ok", "link", "up", "down", "rx", "tx", "dma", "irq", "uart", "gpio",
    "flash", "write", "read", "retry", "timeout", "ready", "idle", "wifi", "ble", "adc", "sample",
    "value", "status", "event", "queue", "heap", "task", "watchdog", "clock",
];

const LEVELS: &[&str] = &[
    "\x1b[32mINFO\x1b[0m",
    "\x1b[33mWARN\x1b[0m",
    "\x1b[31mERROR\x1b[0m",
    "\x1b[36mDEBUG\x1b[0m",
    "\x1b[1;35mTRACE\x1b[0m",
];

const EOLS: &[&[u8]] = &[b"\n", b"\r\n", b"\r", b"\n\r"];

const SYNC: [u8; 2] = [0xA5, 0x5A];
const FRAME_HEADER: usize = 8;
const FRAME_TRAILER: usize = 4;

fn push_hex(out: &mut Vec<u8>, value: u64, min_digits: usize) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let digits = (((64 - value.leading_zeros()) as usize).div_ceil(4)).max(min_digits);
    for i in (0..digits).rev() {
        out.push(DIGITS[((value >> (i * 4)) & 0xF) as usize]);
    }
}

fn parse_hex(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 16 {
        return None;
    }
    let mut v = 0u64;
    for &d in digits {
        let n = match d {
            b'0'..=b'9' => d - b'0',
            b'a'..=b'f' => d - b'a' + 10,
            _ => return None,
        };
        v = (v << 4) | u64::from(n);
    }
    Some(v)
}

/// A deterministic, endless byte stream of firehose records. Useful on its own for
/// feeding a store or parser benchmark without a link.
#[derive(Clone, Debug)]
pub struct FirehoseGenerator {
    content: FirehoseContent,
    rng: Prng,
    seq: u64,
    record: Vec<u8>,
    pos: usize,
}

impl FirehoseGenerator {
    pub fn new(content: FirehoseContent, seed: u64) -> Self {
        Self {
            content,
            rng: Prng(seed),
            seq: 0,
            record: Vec::new(),
            pos: 0,
        }
    }

    /// Append exactly `n` more bytes of the stream to `out`. Records split across calls
    /// continue where they left off.
    pub fn fill(&mut self, out: &mut Vec<u8>, n: usize) {
        out.reserve(n);
        let mut need = n;
        while need > 0 {
            if self.pos == self.record.len() {
                self.next_record();
            }
            let avail = &self.record[self.pos..];
            let k = avail.len().min(need);
            out.extend_from_slice(&avail[..k]);
            self.pos += k;
            need -= k;
        }
    }

    /// Records begun so far (the last one may be partly sent).
    pub fn records_started(&self) -> u64 {
        self.seq
    }

    fn next_record(&mut self) {
        self.record.clear();
        self.pos = 0;
        let kind = match self.content {
            FirehoseContent::Mixed => *self.rng.pick(&[
                FirehoseContent::Text,
                FirehoseContent::Ansi,
                FirehoseContent::LongLines,
                FirehoseContent::MixedEol,
            ]),
            other => other,
        };
        if kind == FirehoseContent::Binary {
            self.binary_record();
        } else {
            self.text_record(kind);
        }
        self.seq += 1;
    }

    fn text_record(&mut self, kind: FirehoseContent) {
        self.record.push(b'#');
        push_hex(&mut self.record, self.seq, 8);
        self.record.push(b' ');
        match kind {
            FirehoseContent::Ansi => self.ansi_payload(),
            FirehoseContent::LongLines => self.long_payload(),
            _ => self.words(3, 14),
        }
        let crc = crc32(&self.record);
        self.record.extend_from_slice(b" *");
        push_hex(&mut self.record, u64::from(crc), 8);
        let eol: &[u8] = if kind == FirehoseContent::MixedEol {
            self.rng.pick(EOLS)
        } else {
            b"\r\n"
        };
        self.record.extend_from_slice(eol);
    }

    fn word(&mut self) {
        let word = *self.rng.pick(WORDS);
        self.record.extend_from_slice(word.as_bytes());
        if self.rng.below(5) == 0 {
            let value = self.rng.below(100_000);
            let _ = write!(self.record, "={value}");
        }
    }

    fn words(&mut self, min: u64, max: u64) {
        let count = self.rng.range(min, max);
        for i in 0..count {
            if i > 0 {
                self.record.push(b' ');
            }
            self.word();
        }
    }

    fn ansi_payload(&mut self) {
        let level = *self.rng.pick(LEVELS);
        self.record.extend_from_slice(level.as_bytes());
        let count = self.rng.range(3, 12);
        for _ in 0..count {
            self.record.push(b' ');
            match self.rng.below(6) {
                0 => {
                    self.record.extend_from_slice(b"\x1b[1m");
                    self.word();
                    self.record.extend_from_slice(b"\x1b[22m");
                }
                1 => {
                    let color = self.rng.below(256);
                    let _ = write!(self.record, "\x1b[38;5;{color}m");
                    self.word();
                    self.record.extend_from_slice(b"\x1b[39m");
                }
                2 => {
                    self.record.extend_from_slice(b"\x1b[4m");
                    self.word();
                    self.record.extend_from_slice(b"\x1b[24m");
                }
                3 => {
                    let rgb = self.rng.next_u64();
                    let (r, g, b) = (rgb & 0xFF, (rgb >> 8) & 0xFF, (rgb >> 16) & 0xFF);
                    let _ = write!(self.record, "\x1b[48;2;{r};{g};{b}m");
                    self.word();
                    self.record.extend_from_slice(b"\x1b[49m");
                }
                _ => self.word(),
            }
        }
    }

    fn long_payload(&mut self) {
        let len = self.rng.range(512, 4096) as usize;
        let end = self.record.len() + len;
        while self.record.len() < end {
            let r = self.rng.next_u64().to_le_bytes();
            for b in r {
                if self.record.len() == end {
                    break;
                }
                // Printable ASCII, 0x20..=0x7E.
                self.record.push(0x20 + b % 95);
            }
        }
    }

    fn binary_record(&mut self) {
        let len = self.rng.range(1, 256) as usize;
        self.record.extend_from_slice(&SYNC);
        self.record
            .extend_from_slice(&(self.seq as u32).to_le_bytes());
        self.record.extend_from_slice(&(len as u16).to_le_bytes());
        let end = self.record.len() + len;
        while self.record.len() < end {
            let r = self.rng.next_u64().to_le_bytes();
            let k = (end - self.record.len()).min(8);
            self.record.extend_from_slice(&r[..k]);
        }
        let crc = crc32(&self.record);
        self.record.extend_from_slice(&crc.to_le_bytes());
    }
}

/// A device that streams firehose records from `on_tick` and ignores what it receives.
#[derive(Debug)]
pub struct FirehoseDevice {
    name: String,
    cfg: FirehoseConfig,
    generator: FirehoseGenerator,
    started: Option<Instant>,
    emitted: u64,
    batch: Vec<u8>,
    done: bool,
}

impl FirehoseDevice {
    /// Named `firehose`.
    pub fn new(cfg: FirehoseConfig) -> Self {
        Self {
            name: "firehose".into(),
            generator: FirehoseGenerator::new(cfg.content, cfg.seed),
            cfg,
            started: None,
            emitted: 0,
            batch: Vec::new(),
            done: false,
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Bytes sent so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }
}

impl SimDevice for FirehoseDevice {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_receive(&mut self, _bytes: &[u8], _out: &mut dyn DeviceOutput) {}

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        if self.done {
            return None;
        }
        let start = *self.started.get_or_insert(now);
        let max_batch = self.cfg.max_batch.max(1) as u64;
        let (want, behind) = match self.cfg.bytes_per_second {
            None => (max_batch, false),
            Some(rate) => {
                let elapsed = now.saturating_duration_since(start).as_secs_f64();
                let due = (elapsed * rate as f64) as u64;
                let owed = due.saturating_sub(self.emitted);
                (owed.min(max_batch), owed > max_batch)
            }
        };
        let cap_left = self
            .cfg
            .total_bytes
            .map_or(u64::MAX, |total| total.saturating_sub(self.emitted));
        let n = want.min(cap_left);
        if n > 0 {
            self.batch.clear();
            self.generator.fill(&mut self.batch, n as usize);
            out.send(&self.batch);
            self.emitted += n;
        }
        if self
            .cfg
            .total_bytes
            .is_some_and(|total| self.emitted >= total)
        {
            self.done = true;
            if self.cfg.disconnect_when_done {
                out.disconnect();
            }
            return None;
        }
        if self.cfg.bytes_per_second.is_none() || behind {
            Some(now)
        } else {
            Some(later(now, self.cfg.tick.max(MIN_TICK)))
        }
    }
}

/// A place in the stream where records went missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqGap {
    pub expected: u64,
    pub found: u64,
}

/// What a [`FirehoseVerifier`] has seen so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FirehoseReport {
    /// Bytes fed in.
    pub bytes: u64,
    /// Intact records accepted in sequence order.
    pub records: u64,
    /// Records skipped over by sequence number, summed over all gaps.
    pub missing_records: u64,
    /// Fragments that failed to parse or failed their checksum.
    pub corrupt_records: u64,
    /// Intact records whose sequence number was behind the expected one.
    pub out_of_order: u64,
    /// The first gaps found, at most 64.
    pub gaps: Vec<SeqGap>,
    /// The sequence number expected next.
    pub next_seq: u64,
}

const MAX_GAPS_KEPT: usize = 64;
/// A text line longer than any the generator makes is garbage.
const MAX_PARTIAL: usize = 64 * 1024;

impl FirehoseReport {
    /// No gaps, no corruption, no reordering.
    pub fn is_clean(&self) -> bool {
        self.missing_records == 0 && self.corrupt_records == 0 && self.out_of_order == 0
    }

    fn accept(&mut self, seq: u64) {
        if seq < self.next_seq {
            self.out_of_order += 1;
            return;
        }
        if seq > self.next_seq {
            self.missing_records += seq - self.next_seq;
            if self.gaps.len() < MAX_GAPS_KEPT {
                self.gaps.push(SeqGap {
                    expected: self.next_seq,
                    found: seq,
                });
            }
        }
        self.records += 1;
        self.next_seq = seq + 1;
    }

    /// Binary frames carry the low 32 bits of the sequence number.
    fn accept_u32(&mut self, seq: u32) {
        let delta = seq.wrapping_sub(self.next_seq as u32);
        if delta < 1 << 31 {
            self.accept(self.next_seq + u64::from(delta));
        } else {
            self.out_of_order += 1;
        }
    }
}

/// Parse one text line (without its terminator) and return its sequence number if the
/// line is intact.
fn parse_text_record(line: &[u8]) -> Option<u64> {
    // "#" + 8 digits + " " + payload + " *" + 8 digits
    if line.len() < 1 + 8 + 1 + 2 + 8 || line[0] != b'#' {
        return None;
    }
    let (body, tail) = line.split_at(line.len() - 10);
    if &tail[..2] != b" *" {
        return None;
    }
    let crc = parse_hex(&tail[2..])?;
    if u64::from(crc32(body)) != crc {
        return None;
    }
    let space = memchr::memchr(b' ', body)?;
    parse_hex(&body[1..space])
}

/// Consumes a firehose byte stream in chunks of any size and reports anything missing,
/// damaged or out of order.
#[derive(Clone, Debug)]
pub struct FirehoseVerifier {
    binary: bool,
    partial: Vec<u8>,
    report: FirehoseReport,
}

impl FirehoseVerifier {
    /// A verifier for streams of `content`, starting at sequence number 0.
    pub fn new(content: FirehoseContent) -> Self {
        Self {
            binary: content == FirehoseContent::Binary,
            partial: Vec::new(),
            report: FirehoseReport::default(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.report.bytes += bytes.len() as u64;
        if self.binary {
            self.feed_binary(bytes);
        } else {
            self.feed_text(bytes);
        }
    }

    pub fn report(&self) -> &FirehoseReport {
        &self.report
    }

    pub fn into_report(self) -> FirehoseReport {
        self.report
    }

    /// Bytes of an unfinished record held back until the rest arrives.
    pub fn pending_bytes(&self) -> usize {
        self.partial.len()
    }

    fn text_line(report: &mut FirehoseReport, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        match parse_text_record(line) {
            Some(seq) => report.accept(seq),
            None => report.corrupt_records += 1,
        }
    }

    fn feed_text(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some(i) = memchr::memchr2(b'\r', b'\n', rest) {
            if self.partial.is_empty() {
                Self::text_line(&mut self.report, &rest[..i]);
            } else {
                self.partial.extend_from_slice(&rest[..i]);
                Self::text_line(&mut self.report, &self.partial);
                self.partial.clear();
            }
            rest = &rest[i + 1..];
        }
        self.partial.extend_from_slice(rest);
        if self.partial.len() > MAX_PARTIAL {
            self.report.corrupt_records += 1;
            self.partial.clear();
        }
    }

    fn feed_binary(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        let Self {
            partial, report, ..
        } = self;
        let mut pos = 0;
        loop {
            let avail = &partial[pos..];
            if avail.len() < SYNC.len() {
                break;
            }
            if avail[..2] != SYNC {
                // Garbage: skip to the next possible frame start.
                report.corrupt_records += 1;
                match memchr::memchr(SYNC[0], &avail[1..]) {
                    Some(k) => pos += 1 + k,
                    None => {
                        pos = partial.len();
                        break;
                    }
                }
                continue;
            }
            if avail.len() < FRAME_HEADER {
                break;
            }
            let len = usize::from(u16::from_le_bytes([avail[6], avail[7]]));
            let total = FRAME_HEADER + len + FRAME_TRAILER;
            if avail.len() < total {
                break;
            }
            let body = &avail[..FRAME_HEADER + len];
            let crc = u32::from_le_bytes([
                avail[total - 4],
                avail[total - 3],
                avail[total - 2],
                avail[total - 1],
            ]);
            if crc32(body) == crc {
                report.accept_u32(u32::from_le_bytes([avail[2], avail[3], avail[4], avail[5]]));
                pos += total;
            } else {
                report.corrupt_records += 1;
                pos += 1;
                match memchr::memchr(SYNC[0], &partial[pos..]) {
                    Some(k) => pos += k,
                    None => {
                        pos = partial.len();
                        break;
                    }
                }
            }
        }
        partial.drain(..pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CaptureOutput;
    use crate::{Clock, SystemClock};

    const ALL: [FirehoseContent; 6] = [
        FirehoseContent::Text,
        FirehoseContent::Ansi,
        FirehoseContent::LongLines,
        FirehoseContent::MixedEol,
        FirehoseContent::Binary,
        FirehoseContent::Mixed,
    ];

    fn stream(content: FirehoseContent, seed: u64, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        FirehoseGenerator::new(content, seed).fill(&mut out, n);
        out
    }

    #[test]
    fn hex_round_trip() {
        let mut v = Vec::new();
        push_hex(&mut v, 0x1f, 8);
        assert_eq!(v, b"0000001f");
        v.clear();
        push_hex(&mut v, 0x1_2345_6789, 8);
        assert_eq!(v, b"123456789");
        assert_eq!(parse_hex(b"123456789"), Some(0x1_2345_6789));
        assert_eq!(parse_hex(b"12G4"), None);
    }

    #[test]
    fn generator_is_deterministic_and_split_invariant() {
        for content in ALL {
            let whole = stream(content, 5, 50_000);
            assert_eq!(whole.len(), 50_000);
            assert_eq!(whole, stream(content, 5, 50_000), "{content:?}");
            assert_ne!(whole, stream(content, 6, 50_000), "{content:?}");
            let mut generator = FirehoseGenerator::new(content, 5);
            let mut pieces = Vec::new();
            for n in [1, 7, 300, 4096, 13, 45_583] {
                generator.fill(&mut pieces, n);
            }
            assert_eq!(pieces, whole, "{content:?}");
        }
    }

    #[test]
    fn text_payloads_are_single_lines() {
        let bytes = stream(FirehoseContent::Mixed, 1, 200_000);
        let segments: Vec<&[u8]> = bytes.split(|&b| b == b'\r' || b == b'\n').collect();
        // Every complete segment is exactly one intact record; the last may be cut short.
        let (_, complete) = segments.split_last().unwrap();
        let mut expected_seq = 0;
        for line in complete.iter().filter(|l| !l.is_empty()) {
            assert_eq!(parse_text_record(line), Some(expected_seq));
            expected_seq += 1;
        }
        assert!(expected_seq > 50);
        let ansi = stream(FirehoseContent::Ansi, 1, 10_000);
        assert!(ansi.windows(2).any(|w| w == b"\x1b["));
        let long = stream(FirehoseContent::LongLines, 1, 100_000);
        let longest = long.split(|&b| b == b'\n').map(<[u8]>::len).max().unwrap();
        assert!(longest >= 512);
    }

    #[test]
    fn verifier_accepts_every_content_in_any_chunking() {
        for content in ALL {
            let bytes = stream(content, 11, 300_000);
            let mut verifier = FirehoseVerifier::new(content);
            let mut rng = Prng(3);
            let mut rest = bytes.as_slice();
            while !rest.is_empty() {
                let n = (rng.range(1, 5000) as usize).min(rest.len());
                verifier.feed(&rest[..n]);
                rest = &rest[n..];
            }
            let report = verifier.report();
            assert!(report.is_clean(), "{content:?}: {report:?}");
            assert!(report.records > 50, "{content:?}: {report:?}");
            assert_eq!(report.bytes, 300_000);
        }
    }

    #[test]
    fn verifier_detects_drops_corruption_and_reordering() {
        for content in [FirehoseContent::Text, FirehoseContent::Binary] {
            let bytes = stream(content, 2, 100_000);

            // A dropped run of bytes in the middle.
            let mut dropped = bytes.clone();
            dropped.drain(40_000..40_500);
            let mut v = FirehoseVerifier::new(content);
            v.feed(&dropped);
            assert!(
                v.report().missing_records > 0,
                "{content:?}: {:?}",
                v.report()
            );
            assert!(!v.report().gaps.is_empty());

            // One flipped byte.
            let mut flipped = bytes.clone();
            flipped[50_000] ^= 0x40;
            let mut v = FirehoseVerifier::new(content);
            v.feed(&flipped);
            assert!(!v.report().is_clean(), "{content:?}: {:?}", v.report());
            assert!(v.report().corrupt_records > 0);
        }

        // Two whole lines swapped.
        let text = stream(FirehoseContent::Text, 4, 20_000);
        let mut lines: Vec<&[u8]> = text.split_inclusive(|&b| b == b'\n').collect();
        lines.swap(10, 11);
        let mut v = FirehoseVerifier::new(FirehoseContent::Text);
        v.feed(&lines.concat());
        assert_eq!(v.report().out_of_order, 1, "{:?}", v.report());
    }

    #[test]
    fn device_respects_rate_and_cap() {
        let cfg = FirehoseConfig::new(FirehoseContent::Text)
            .with_rate(100_000)
            .with_total(25_000);
        let mut dev = FirehoseDevice::new(FirehoseConfig {
            disconnect_when_done: true,
            ..cfg
        });
        let mut out = CaptureOutput::new();
        let t0 = SystemClock.now();
        let next = dev.on_tick(t0, &mut out);
        assert_eq!(out.sent.len(), 0);
        assert_eq!(next, Some(t0 + Duration::from_millis(2)));
        dev.on_tick(t0 + Duration::from_millis(100), &mut out);
        assert_eq!(out.sent.len(), 10_000);
        assert!(!out.disconnected);
        let next = dev.on_tick(t0 + Duration::from_secs(1), &mut out);
        assert_eq!(next, None);
        assert_eq!(out.sent.len(), 25_000);
        assert!(out.disconnected);
        assert_eq!(out.sent, stream(FirehoseContent::Text, 0, 25_000));
    }

    #[test]
    fn tick_is_clamped_and_saturates() {
        let t0 = SystemClock.now();
        let mut zero = FirehoseDevice::new(FirehoseConfig {
            tick: Duration::ZERO,
            ..FirehoseConfig::new(FirehoseContent::Text).with_rate(1_000)
        });
        let mut out = CaptureOutput::new();
        assert_eq!(zero.on_tick(t0, &mut out), Some(t0 + MIN_TICK));

        let mut huge = FirehoseDevice::new(FirehoseConfig {
            tick: Duration::MAX,
            ..FirehoseConfig::new(FirehoseContent::Text).with_rate(1_000)
        });
        assert!(huge.on_tick(t0, &mut out).is_some_and(|t| t > t0));
    }

    #[test]
    fn unlimited_device_sends_full_batches() {
        let mut dev = FirehoseDevice::new(FirehoseConfig::new(FirehoseContent::Binary));
        let mut out = CaptureOutput::new();
        let t0 = SystemClock.now();
        assert_eq!(dev.on_tick(t0, &mut out), Some(t0));
        assert_eq!(out.sent.len(), 16 * 1024);
        assert_eq!(dev.emitted(), 16 * 1024);
    }
}
