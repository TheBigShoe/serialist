//! The bodies of Serialist's fuzz targets and the input format they share.
//!
//! Each target is a module of the same name with a `pub fn run(data: &[u8])` that panics
//! when a check fails. `fuzz_targets/<name>.rs` hands libFuzzer's input to it, and the
//! module's `seeds_replay` test runs it on every file in `seeds/<name>/` on stable, so a
//! crash reproducer committed there stays a regression test:
//!
//! ```text
//! just fuzz-check                  # fmt, clippy and the seed replay (stable)
//! just fuzz ansi_monitor 60        # fuzz for 60 s (nightly, ASan, cargo-fuzz)
//! just fuzz-stable ansi_monitor 60 # the same on stable, without ASan
//! ```
//!
//! [`codec`] is not a target: the checks every codec target shares (the decode contract,
//! tiling, building an encode request from fuzz bytes).
//!
//! Byte-stream targets read their input through [`Input`], so libFuzzer controls the
//! chunk boundaries as well as the bytes, and a seed stays readable: a config byte, the
//! chunk lengths, then the stream as it would arrive on the wire.

pub mod ansi_monitor;
pub mod codec;
pub mod lua_values;
pub mod race_rust;
pub mod text_lines;

/// The most chunk lengths an [`Input`] carries.
pub const MAX_CHUNK_LENS: usize = 32;

/// A fuzz input for a target that consumes a byte stream in chunks:
///
/// ```text
/// [config] [n] [len_1 ... len_n] [stream ...]
/// ```
///
/// - `config`: one byte of options, read by each target as its `run` documents.
/// - `n`: how many chunk lengths follow, taken modulo `MAX_CHUNK_LENS + 1`.
/// - `len_1 ... len_n`: chunk sizes in bytes (0..=255), used in order and repeated until
///   the stream runs out. A 0 is an empty chunk, which every consumer must accept. With
///   `n = 0` the whole stream is one chunk.
/// - `stream`: every byte after that.
///
/// Every input is valid. A missing header byte reads as 0, so an empty input is an empty
/// stream, and a seed of `\0\0` followed by text is that text in one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Input<'a> {
    pub config: u8,
    pub lens: &'a [u8],
    pub stream: &'a [u8],
}

impl<'a> Input<'a> {
    pub fn parse(data: &'a [u8]) -> Self {
        let config = data.first().copied().unwrap_or(0);
        let n = data
            .get(1)
            .map_or(0, |&n| usize::from(n) % (MAX_CHUNK_LENS + 1));
        let rest = data.get(2..).unwrap_or_default();
        let (lens, stream) = rest.split_at(n.min(rest.len()));
        Self {
            config,
            lens,
            stream,
        }
    }

    /// Bit `bit` (0 = least significant) of the config byte.
    pub fn flag(&self, bit: u32) -> bool {
        (self.field(bit) & 1) != 0
    }

    /// One of `choices`, picked by the config byte shifted right by `shift`, modulo the
    /// number of choices. With a power-of-two count that is a bit field from `shift` up.
    pub fn pick<T: Copy>(&self, shift: u32, choices: &[T]) -> T {
        choices[usize::from(self.field(shift)) % choices.len()]
    }

    fn field(&self, shift: u32) -> u8 {
        self.config.checked_shr(shift).unwrap_or(0)
    }

    /// The stream cut into chunks as `lens` says. Concatenated, they are `stream`.
    pub fn chunks(&self) -> Chunks<'a> {
        Chunks {
            rest: self.stream,
            lens: self.lens,
            next: 0,
            empties: 0,
        }
    }
}

/// The chunks of an [`Input`]; see [`Input::chunks`].
#[derive(Clone, Debug)]
pub struct Chunks<'a> {
    rest: &'a [u8],
    lens: &'a [u8],
    next: usize,
    /// Empty chunks in a row. A whole cycle of them would never end, so after one the
    /// rest of the stream goes in one chunk.
    empties: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        if self.lens.is_empty() || self.empties >= self.lens.len() {
            return Some(std::mem::take(&mut self.rest));
        }
        let len = usize::from(self.lens[self.next % self.lens.len()]);
        self.next += 1;
        self.empties = if len == 0 { self.empties + 1 } else { 0 };
        let (chunk, rest) = self.rest.split_at(len.min(self.rest.len()));
        self.rest = rest;
        Some(chunk)
    }
}

/// Run `run` on every file in `seeds/<target>/`, naming the file that panicked.
#[cfg(test)]
pub(crate) fn replay_seeds(target: &str, run: fn(&[u8])) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("seeds")
        .join(target);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "{} has no seeds", dir.display());
    for path in paths {
        let data = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if std::panic::catch_unwind(|| run(&data)).is_err() {
            panic!("{target} failed on {}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use super::*;

    fn chunks(data: &[u8]) -> Vec<&[u8]> {
        Input::parse(data).chunks().collect()
    }

    #[test]
    fn the_header_is_optional() {
        for data in [&b""[..], b"\x07", b"\x07\x05"] {
            let input = Input::parse(data);
            assert!(input.stream.is_empty() && input.lens.is_empty());
            assert_eq!(input.chunks().count(), 0);
        }
        assert_eq!(Input::parse(b"\x07").config, 7);
    }

    #[test]
    fn no_lengths_is_one_chunk() {
        assert_eq!(chunks(b"\x00\x00hello"), [b"hello"]);
        // n is taken modulo 33: 33 lengths is none.
        assert_eq!(chunks(b"\x00\x21hello"), [b"hello"]);
    }

    #[test]
    fn lengths_cycle_until_the_stream_ends() {
        let got = chunks(b"\x00\x02\x01\x03abcdefgh");
        assert_eq!(got, [&b"a"[..], b"bcd", b"e", b"fgh"]);
        // More lengths than the input holds: the stream is what is left.
        let input = Input::parse(b"\x00\x05\x01\x02");
        assert_eq!(input.lens, [1, 2]);
        assert!(input.stream.is_empty());
    }

    #[test]
    fn zero_lengths_are_empty_chunks_and_always_end() {
        assert_eq!(
            chunks(b"\x00\x02\x00\x02abcde"),
            [&b""[..], b"ab", b"", b"cd", b"", b"e"]
        );
        assert_eq!(chunks(b"\x00\x02\x00\x00abc"), [&b""[..], b"", b"abc"]);
    }

    #[test]
    fn chunks_concatenate_to_the_stream() {
        let data: Vec<u8> = (0..=255u8).cycle().take(2000).collect();
        for start in 0..64 {
            let input = Input::parse(&data[start..]);
            assert_eq!(input.chunks().collect::<Vec<_>>().concat(), input.stream);
        }
    }

    #[test]
    fn config_fields() {
        let input = Input::parse(&[0b1011_0101]);
        assert!(input.flag(0) && !input.flag(1) && input.flag(2));
        assert_eq!(input.pick(1, &[10, 20, 30, 40]), 30);
        assert_eq!(input.pick(4, &['a', 'b', 'c']), 'c');
    }

    /// Every `fuzz_targets/*.rs` has a `[[bin]]` in Cargo.toml and seeds to replay.
    #[test]
    fn every_target_is_registered_and_seeded() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest = std::fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
        let targets: BTreeSet<String> = std::fs::read_dir(root.join("fuzz_targets"))
            .expect("fuzz_targets/")
            .map(|entry| entry.expect("a directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
            .map(|path| path.file_stem().expect("a name").to_string_lossy().into())
            .collect();
        assert!(!targets.is_empty());
        for target in &targets {
            assert!(
                manifest.contains(&format!("path = \"fuzz_targets/{target}.rs\"")),
                "{target} has no [[bin]] in fuzz/Cargo.toml"
            );
            let seeds = root.join("seeds").join(target);
            let count = std::fs::read_dir(&seeds).map_or(0, |dir| dir.count());
            assert!(count > 0, "{} has no seeds", seeds.display());
        }
    }
}
