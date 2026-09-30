//! Capture corpora for codec conformance: deterministic RACE traffic with everything a
//! decoder must survive, and deterministic ways to cut it into chunks.
//!
//! A corpus is a sequence of [`Segment`]s. The first ones cover every kind once, in the
//! order of [`Segment::ALL`], so any corpus of at least that many segments exercises
//! every path; the rest are drawn at random. Output depends only on the seed (the
//! generator is a self-contained SplitMix64), so a seed names a corpus forever.

use crate::race::{MAX_LEN, MAX_PAYLOAD, RaceType, SYNC, encode_frame};

/// SplitMix64: small, fast and stable, so a seed always means the same bytes.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below((hi - lo + 1) as u64) as usize
    }

    pub fn byte(&mut self) -> u8 {
        self.next_u64() as u8
    }

    /// True `percent` times in a hundred.
    pub fn percent(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// One piece of a corpus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    /// A well-formed frame; payloads are random bytes, so they hold sync bytes and line
    /// feeds too.
    Frame(RaceType),
    /// A printable line ending in CRLF or LF.
    TextLine,
    /// Printable text with no line ending, running straight into what follows.
    PartialText,
    /// A sync byte and a known type with a length outside 2..=4096.
    Malformed,
    /// A sync byte followed by something that is not a type (sometimes another sync byte
    /// or a line feed).
    LoneSync,
    /// A few random bytes.
    Junk,
    /// Printable text longer than a text frame, then a line feed.
    LongText,
    /// A frame header whose body is cut short, so what follows is swallowed as payload.
    Truncated,
    /// A line with tabs, backslashes, bare CRs, DEL and high bytes.
    Escapes,
}

impl Segment {
    pub const ALL: [Segment; 12] = [
        Segment::Frame(RaceType::Command),
        Segment::Frame(RaceType::Response),
        Segment::Frame(RaceType::Indication),
        Segment::Frame(RaceType::Log),
        Segment::TextLine,
        Segment::PartialText,
        Segment::Malformed,
        Segment::LoneSync,
        Segment::Junk,
        Segment::LongText,
        Segment::Truncated,
        Segment::Escapes,
    ];

    fn random(rng: &mut Rng) -> Segment {
        match rng.below(100) {
            0..40 => Segment::Frame(RaceType::ALL[rng.below(4) as usize]),
            40..62 => Segment::TextLine,
            62..67 => Segment::PartialText,
            67..75 => Segment::Malformed,
            75..81 => Segment::LoneSync,
            81..87 => Segment::Junk,
            87..89 => Segment::LongText,
            89..93 => Segment::Truncated,
            _ => Segment::Escapes,
        }
    }

    fn write(self, rng: &mut Rng, out: &mut Vec<u8>) {
        match self {
            Segment::Frame(ty) => {
                let payload = payload(rng);
                let frame = encode_frame(ty, cmd_id(rng), &payload).expect("a payload that fits");
                out.extend_from_slice(&frame);
            }
            Segment::TextLine => {
                words(rng, out);
                out.extend_from_slice(if rng.percent(70) { b"\r\n" } else { b"\n" });
            }
            Segment::PartialText => words(rng, out),
            Segment::Malformed => {
                let len: u16 = match rng.below(5) {
                    0 => 0,
                    1 => 1,
                    2 => MAX_LEN as u16 + 1,
                    3 => u16::MAX,
                    _ => (MAX_LEN as u16 + 1).saturating_add(rng.below(60_000) as u16),
                };
                out.push(SYNC);
                out.push(RaceType::ALL[rng.below(4) as usize].byte());
                out.extend_from_slice(&len.to_le_bytes());
            }
            Segment::LoneSync => {
                out.push(SYNC);
                out.push(match rng.below(4) {
                    0 => SYNC,
                    1 => b'\n',
                    _ => loop {
                        let b = rng.byte();
                        if RaceType::from_byte(b).is_none() {
                            break b;
                        }
                    },
                });
            }
            Segment::Junk => {
                for _ in 0..rng.range(1, 16) {
                    out.push(rng.byte());
                }
            }
            Segment::LongText => {
                for _ in 0..rng.range(1000, 3000) {
                    out.push(b' ' + rng.below(95) as u8);
                }
                out.push(b'\n');
            }
            Segment::Truncated => {
                let len = rng.range(10, 60);
                let frame = encode_frame(
                    RaceType::ALL[rng.below(4) as usize],
                    cmd_id(rng),
                    &vec![0xA5; len],
                )
                .expect("a small payload");
                out.extend_from_slice(&frame[..rng.range(4, frame.len() - 1)]);
            }
            Segment::Escapes => {
                const PIECES: [&[u8]; 8] = [
                    b"\t",
                    b"\\",
                    b"\r",
                    b"\x7F",
                    b"\x80",
                    b"\xFF",
                    "é".as_bytes(),
                    b"\x00",
                ];
                for _ in 0..rng.range(2, 8) {
                    if rng.percent(50) {
                        out.extend_from_slice(PIECES[rng.below(8) as usize]);
                    } else {
                        words(rng, out);
                    }
                }
                out.extend_from_slice(b"\r\n");
            }
        }
    }
}

fn cmd_id(rng: &mut Rng) -> u16 {
    match rng.below(5) {
        0 => 0x0F15,
        1 => 0x0F40,
        2 => 0x0001,
        3 => 0x1C00,
        _ => rng.next_u64() as u16,
    }
}

fn payload(rng: &mut Rng) -> Vec<u8> {
    let len = match rng.below(100) {
        0..10 => 0,
        10..75 => rng.range(1, 32),
        75..97 => rng.range(33, 300),
        _ => rng.range(301, MAX_PAYLOAD),
    };
    (0..len).map(|_| rng.byte()).collect()
}

/// A few printable words, no line ending.
fn words(rng: &mut Rng, out: &mut Vec<u8>) {
    const WORDS: [&[u8]; 12] = [
        b"boot",
        b"ok",
        b"bt:",
        b"link",
        b"rssi=-52",
        b"heap",
        b"[I]",
        b"a2dp",
        b"volume=7",
        b"ready>",
        b"0x0F15",
        b"err=0",
    ];
    for i in 0..rng.range(1, 8) {
        if i > 0 {
            out.push(b' ');
        }
        out.extend_from_slice(WORDS[rng.below(WORDS.len() as u64) as usize]);
    }
}

/// A corpus of `segments` segments: every kind once, then random ones.
pub fn generate(seed: u64, segments: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::new();
    for i in 0..segments {
        let segment = Segment::ALL
            .get(i)
            .copied()
            .unwrap_or_else(|| Segment::random(&mut rng));
        segment.write(&mut rng, &mut out);
    }
    out
}

/// A corpus of at least `len` bytes, for throughput runs.
pub fn generate_len(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(len + 8192);
    let mut i = 0;
    while out.len() < len {
        let segment = Segment::ALL
            .get(i)
            .copied()
            .unwrap_or_else(|| Segment::random(&mut rng));
        segment.write(&mut rng, &mut out);
        i += 1;
    }
    out
}

/// Chunk sizes from 1 to `max_chunk` covering `total` bytes, the last one cut to fit.
pub fn chunk_sizes(seed: u64, total: usize, max_chunk: usize) -> Vec<usize> {
    let mut rng = Rng::new(seed);
    let mut sizes = Vec::new();
    let mut left = total;
    while left > 0 {
        let size = rng.range(1, max_chunk.max(1)).min(left);
        sizes.push(size);
        left -= size;
    }
    sizes
}

/// `bytes` cut into consecutive chunks of `sizes` (cycled), covering all of it.
pub fn split<'a>(bytes: &'a [u8], sizes: &[usize]) -> Vec<&'a [u8]> {
    let mut chunks = Vec::new();
    let mut rest = bytes;
    let mut i = 0;
    while !rest.is_empty() {
        let size = sizes
            .get(i % sizes.len().max(1))
            .copied()
            .unwrap_or(rest.len())
            .clamp(1, rest.len());
        let (chunk, tail) = rest.split_at(size);
        chunks.push(chunk);
        rest = tail;
        i += 1;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_always_means_the_same_bytes() {
        assert_eq!(generate(7, 50), generate(7, 50));
        assert_ne!(generate(7, 50), generate(8, 50));
        // Pinned, so a change to the generator is noticed (fixtures would move too).
        let mut rng = Rng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
    }

    #[test]
    fn chunks_cover_the_bytes_exactly() {
        let bytes = generate(3, 40);
        for max in [1, 5, 64, 100_000] {
            let sizes = chunk_sizes(9, bytes.len(), max);
            assert_eq!(sizes.iter().sum::<usize>(), bytes.len());
            assert!(sizes.iter().all(|&s| (1..=max).contains(&s)));
            assert_eq!(split(&bytes, &sizes).concat(), bytes);
        }
        assert_eq!(split(&bytes, &[7]).concat(), bytes);
        assert!(generate_len(1, 10_000).len() >= 10_000);
    }
}
