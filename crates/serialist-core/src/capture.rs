//! Recorded captures: a raw byte file plus an optional timing sidecar next to it.
//!
//! # The two files
//!
//! - **The raw file** (any name, `boot.bin` say) holds every received byte exactly as
//!   the transport delivered it, and nothing else. It is what the Record toggle has
//!   always written, unchanged, so a recording made before sidecars existed is still a
//!   capture: it replays at a baud rate or as fast as possible.
//! - **The timing sidecar** is `<raw file name>.timing` in the same folder
//!   (`boot.bin.timing`, see [`timing_path`]). It says when each chunk arrived, so a
//!   replay can reproduce the original pacing and chunk boundaries.
//!
//! # Sidecar format, version 1
//!
//! UTF-8 text, one record per line, each line ended by `\n`, fields separated by one
//! space. The first line is the header, `serialist-timing 1` ([`TIMING_MAGIC`], then the
//! version). Every later line is a record, a comment (`#` first) or blank:
//!
//! ```text
//! serialist-timing 1
//! connect 0 virtual:race @ 115200 8N1
//! rx 1520 0 64
//! rx 11730 64 4096
//! disconnect 2004113
//! ```
//!
//! | Record | Fields | Meaning |
//! | --- | --- | --- |
//! | `rx <t> <offset> <len>` | integers | `len` (> 0) received bytes, stored at `offset` in the raw file, arrived at `t` |
//! | `connect <t> <description>` | integer, then the rest of the line | the link came up; the transport's description |
//! | `disconnect <t>` | integer | the link went down |
//!
//! - `t` is microseconds since the recording's origin, the moment recording started.
//!   Times never decrease from one record to the next.
//! - `rx` records are contiguous: the first starts at offset 0 and each starts where the
//!   previous one ended. One `rx` record is one chunk exactly as the session delivered it.
//! - `connect` and `disconnect` mark a recording that spans a reconnect. A replay uses
//!   them only as timing; it does not drop the link in the middle.
//! - A reader skips a record kind it does not know, so version 1 can grow new kinds (a
//!   `tx` direction, say) without breaking older readers. A different version number in
//!   the header is an error.
//!
//! **Append-only and crash-tolerant.** [`TimingWriter`] only appends through a buffer
//! and never seeks or syncs per record; the recorder flushes it on its own cadence (with
//! the raw file) and syncs once at the end. A crash can therefore leave the last line
//! cut short, and a cut line can still parse with a wrong value (`rx 100 0 50` cut to
//! `rx 100 0 5` reads as five bytes). Every line [`TimingWriter`] writes ends with `\n`,
//! so a complete sidecar ends with one: [`Timing::read`] drops a last line that has no
//! `\n`, whether or not it parses, and reports that in [`Timing::truncated`]. The header
//! is the exception to "drop and carry on": a first line without its `\n` is not a
//! sidecar ([`TimingError::NotTiming`], as for an empty file), since `serialist-timing 1`
//! is also what a cut `serialist-timing 10` looks like. The two files are flushed
//! independently, so either can be a little ahead of the other; [`Timing::schedule`]
//! reconciles them.

use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The first word of a sidecar's header line.
pub const TIMING_MAGIC: &str = "serialist-timing";
/// The sidecar version this build writes and reads.
pub const TIMING_VERSION: u32 = 1;
/// Appended to the raw file's name to name its sidecar.
pub const TIMING_SUFFIX: &str = ".timing";

/// Buffer size for a sidecar being written. Records are about 20 bytes, so this holds a
/// few thousand chunks between flushes.
const WRITE_BUFFER: usize = 64 * 1024;

/// The sidecar for the raw capture at `raw`: the same path with [`TIMING_SUFFIX`]
/// appended to the file name (`boot.bin` -> `boot.bin.timing`).
pub fn timing_path(raw: &Path) -> PathBuf {
    let mut name = raw.as_os_str().to_owned();
    name.push(TIMING_SUFFIX);
    PathBuf::from(name)
}

/// One line of a sidecar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TimingRecord {
    /// `len` received bytes at `offset` in the raw file, arrived `at` after the origin.
    Rx { at: Duration, offset: u64, len: u64 },
    /// The link came up; `description` is the transport's own name for it.
    Connect { at: Duration, description: String },
    /// The link went down.
    Disconnect { at: Duration },
}

impl TimingRecord {
    /// When the record happened, after the recording's origin.
    pub fn at(&self) -> Duration {
        match self {
            Self::Rx { at, .. } | Self::Connect { at, .. } | Self::Disconnect { at } => *at,
        }
    }
}

/// A sidecar that cannot be read.
#[derive(Debug, thiserror::Error)]
pub enum TimingError {
    #[error("could not read the timing sidecar: {0}")]
    Io(#[from] io::Error),
    #[error("not a timing sidecar: the first line is not `{TIMING_MAGIC} <version>`")]
    NotTiming,
    #[error(
        "timing sidecar version {0} is not supported (this build reads version {TIMING_VERSION})"
    )]
    Version(u32),
    #[error("timing sidecar line {line}: {message}")]
    Line { line: usize, message: String },
}

/// A parsed sidecar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timing {
    /// Every record, in file order.
    pub records: Vec<TimingRecord>,
    /// The last line had no `\n`, so it was dropped, parsed or not: the recording
    /// stopped mid-write.
    pub truncated: bool,
}

/// When one chunk of a capture is due on replay. See [`Timing::schedule`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScheduledChunk {
    /// After the replay starts, at 1x. The first chunk is always due at zero.
    pub at: Duration,
    /// Where the chunk's bytes start in the raw file.
    pub offset: u64,
    /// How many bytes, more than zero.
    pub len: u64,
}

impl Timing {
    /// Parse a sidecar. See the module docs for the format and what is tolerated.
    pub fn read(mut reader: impl BufRead) -> Result<Self, TimingError> {
        let mut line = Vec::new();
        // The next line and whether it ended with `\n`, or `None` at the end of input.
        let mut next_line = |line: &mut Vec<u8>| -> io::Result<Option<bool>> {
            line.clear();
            if reader.read_until(b'\n', line)? == 0 {
                return Ok(None);
            }
            let terminated = line.last() == Some(&b'\n');
            if terminated {
                line.pop();
            }
            Ok(Some(terminated))
        };

        // A header without its `\n` may be cut short, so it is not a header.
        let Some(true) = next_line(&mut line)? else {
            return Err(TimingError::NotTiming);
        };
        let header = String::from_utf8_lossy(&line);
        let version = header
            .trim_end_matches('\r')
            .strip_prefix(TIMING_MAGIC)
            .and_then(|rest| rest.strip_prefix(' '))
            .and_then(|version| version.trim().parse::<u32>().ok())
            .ok_or(TimingError::NotTiming)?;
        if version != TIMING_VERSION {
            return Err(TimingError::Version(version));
        }

        let mut timing = Self::default();
        let mut next_offset = 0u64;
        let mut last_at = Duration::ZERO;
        let mut number = 1;
        while let Some(terminated) = next_line(&mut line)? {
            // Only the last line of the input can lack its `\n`. It may be cut short
            // and still parse, with a wrong value, so it is dropped unread.
            if !terminated {
                timing.truncated = true;
                break;
            }
            number += 1;
            let text = match std::str::from_utf8(&line) {
                Ok(text) => text.trim_end_matches('\r'),
                Err(_) => {
                    return Err(TimingError::Line {
                        line: number,
                        message: "not UTF-8".into(),
                    });
                }
            };
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            let record = match parse_record(text) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(message) => {
                    return Err(TimingError::Line {
                        line: number,
                        message,
                    });
                }
            };
            let fail = |message: String| TimingError::Line {
                line: number,
                message,
            };
            if record.at() < last_at {
                return Err(fail(format!(
                    "time {} us is before the previous record's {} us",
                    record.at().as_micros(),
                    last_at.as_micros()
                )));
            }
            last_at = record.at();
            if let TimingRecord::Rx { offset, len, .. } = record {
                if offset != next_offset {
                    return Err(fail(format!(
                        "rx record starts at offset {offset}, expected {next_offset}"
                    )));
                }
                next_offset = offset.saturating_add(len);
            }
            timing.records.push(record);
        }
        Ok(timing)
    }

    /// Read and parse the sidecar at `path`. A missing file is
    /// `TimingError::Io` with `ErrorKind::NotFound`: a capture without a sidecar.
    pub fn read_file(path: &Path) -> Result<Self, TimingError> {
        Self::read(BufReader::new(File::open(path)?))
    }

    /// The `rx` records, in order.
    pub fn rx(&self) -> impl Iterator<Item = (Duration, u64, u64)> + '_ {
        self.records.iter().filter_map(|record| match record {
            TimingRecord::Rx { at, offset, len } => Some((*at, *offset, *len)),
            _ => None,
        })
    }

    /// When each chunk of a raw file of `raw_len` bytes is due on replay at 1x.
    ///
    /// - Times are relative to the first `rx` record, so leading silence before the
    ///   first byte is not replayed: the first chunk is due at zero.
    /// - Records past the end of the raw file (the sidecar was flushed further than the
    ///   raw file) are cut back to it.
    /// - Raw bytes after the last record (the raw file was flushed further) become one
    ///   more chunk, due with the last record; with no `rx` records at all the whole
    ///   file is one chunk due at zero.
    ///
    /// The chunks cover the raw file exactly, in order, with no gaps.
    pub fn schedule(&self, raw_len: u64) -> Vec<ScheduledChunk> {
        let mut chunks = Vec::new();
        let mut first = None;
        let mut end = 0u64;
        let mut last_at = Duration::ZERO;
        for (at, offset, len) in self.rx() {
            if offset >= raw_len {
                break;
            }
            let base = *first.get_or_insert(at);
            let len = len.min(raw_len - offset);
            last_at = at.saturating_sub(base);
            chunks.push(ScheduledChunk {
                at: last_at,
                offset,
                len,
            });
            end = offset + len;
        }
        if end < raw_len {
            chunks.push(ScheduledChunk {
                at: last_at,
                offset: end,
                len: raw_len - end,
            });
        }
        chunks
    }
}

/// Parses one record line. `Ok(None)` is a record kind this version does not know.
fn parse_record(text: &str) -> Result<Option<TimingRecord>, String> {
    let (kind, rest) = text.split_once(' ').unwrap_or((text, ""));
    let micros = |field: Option<&str>, name: &str| -> Result<Duration, String> {
        field
            .and_then(|f| f.parse::<u64>().ok())
            .map(Duration::from_micros)
            .ok_or_else(|| format!("{kind}: missing or bad {name}"))
    };
    let number = |field: Option<&str>, name: &str| -> Result<u64, String> {
        field
            .and_then(|f| f.parse::<u64>().ok())
            .ok_or_else(|| format!("{kind}: missing or bad {name}"))
    };
    match kind {
        "rx" => {
            let mut fields = rest.split(' ');
            let at = micros(fields.next(), "time")?;
            let offset = number(fields.next(), "offset")?;
            let len = number(fields.next(), "length")?;
            if fields.next().is_some() {
                return Err("rx: too many fields".into());
            }
            if len == 0 {
                return Err("rx: a chunk has at least one byte".into());
            }
            Ok(Some(TimingRecord::Rx { at, offset, len }))
        }
        "connect" => {
            let (time, description) = rest.split_once(' ').unwrap_or((rest, ""));
            let at = micros(Some(time), "time")?;
            Ok(Some(TimingRecord::Connect {
                at,
                description: description.to_owned(),
            }))
        }
        "disconnect" => {
            let mut fields = rest.split(' ');
            let at = micros(fields.next(), "time")?;
            if fields.next().is_some() {
                return Err("disconnect: too many fields".into());
            }
            Ok(Some(TimingRecord::Disconnect { at }))
        }
        _ if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') => {
            Ok(None)
        }
        _ => Err(format!("{kind:?} is not a record")),
    }
}

/// Appends a sidecar as a recording runs. See the module docs for the format.
///
/// Cheap per chunk: one formatted line into a buffer, no flush and no sync. Call
/// [`flush`](Self::flush) on the recorder's own cadence and [`finish`](Self::finish)
/// (or flush and sync the inner writer) once at the end. Times are taken against the
/// `origin` given at creation and never go backwards in the file, whatever the instants
/// passed in.
pub struct TimingWriter<W: Write> {
    out: W,
    origin: Instant,
    /// Where the next `rx` chunk starts in the raw file.
    offset: u64,
    /// The last time written, so times never decrease.
    last: Duration,
}

impl TimingWriter<BufWriter<File>> {
    /// Create (or truncate) the sidecar at `path`, buffered, and write its header.
    /// `origin` is the moment the recording started.
    pub fn create(path: &Path, origin: Instant) -> io::Result<Self> {
        Self::new(
            BufWriter::with_capacity(WRITE_BUFFER, File::create(path)?),
            origin,
        )
    }

    /// Flush, then sync the file to disk. Once, when the recording ends.
    pub fn finish(mut self) -> io::Result<()> {
        self.out.flush()?;
        self.out.get_ref().sync_all()
    }
}

impl<W: Write> TimingWriter<W> {
    /// Write the header to `out`. `origin` is the moment the recording started.
    pub fn new(mut out: W, origin: Instant) -> io::Result<Self> {
        writeln!(out, "{TIMING_MAGIC} {TIMING_VERSION}")?;
        Ok(Self {
            out,
            origin,
            offset: 0,
            last: Duration::ZERO,
        })
    }

    /// A received chunk of `len` bytes that arrived `at` (the session's `received_at`),
    /// appended to the raw file right after the previous one. Zero bytes write nothing.
    pub fn rx(&mut self, at: Instant, len: usize) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let t = self.micros(at);
        writeln!(self.out, "rx {t} {} {len}", self.offset)?;
        self.offset += len as u64;
        Ok(())
    }

    /// The link came up at `at`. Line breaks in `description` become spaces.
    pub fn connect(&mut self, at: Instant, description: &str) -> io::Result<()> {
        let t = self.micros(at);
        let description = description.replace(['\r', '\n'], " ");
        writeln!(self.out, "connect {t} {description}")
    }

    /// The link went down at `at`.
    pub fn disconnect(&mut self, at: Instant) -> io::Result<()> {
        let t = self.micros(at);
        writeln!(self.out, "disconnect {t}")
    }

    /// Raw-file bytes accounted for so far: where the next chunk will start.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }

    pub fn get_ref(&self) -> &W {
        &self.out
    }

    pub fn into_inner(self) -> W {
        self.out
    }

    /// `at` as microseconds after the origin, never before the last record.
    fn micros(&mut self, at: Instant) -> u128 {
        self.last = self.last.max(at.saturating_duration_since(self.origin));
        self.last.as_micros()
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    fn read(text: &str) -> Result<Timing, TimingError> {
        Timing::read(text.as_bytes())
    }

    fn rx(at_us: u64, offset: u64, len: u64) -> TimingRecord {
        TimingRecord::Rx {
            at: Duration::from_micros(at_us),
            offset,
            len,
        }
    }

    #[test]
    fn the_sidecar_sits_next_to_the_raw_file() {
        assert_eq!(
            timing_path(Path::new("/captures/boot.bin")),
            PathBuf::from("/captures/boot.bin.timing")
        );
        assert_eq!(
            timing_path(Path::new("no-extension")),
            PathBuf::from("no-extension.timing")
        );
    }

    #[test]
    fn written_records_read_back() {
        let origin = Instant::now();
        let mut writer = TimingWriter::new(Vec::new(), origin).unwrap();
        writer.connect(origin, "virtual:race @ 115200 8N1").unwrap();
        writer.rx(origin + 2 * MS, 5).unwrap();
        writer.rx(origin + 2 * MS, 0).unwrap();
        writer.rx(origin + 7 * MS, 3).unwrap();
        writer.disconnect(origin + 9 * MS).unwrap();
        writer
            .connect(origin + 20 * MS, "line one\r\nline two")
            .unwrap();
        writer.rx(origin + 21 * MS, 1).unwrap();
        assert_eq!(writer.offset(), 9);
        let bytes = writer.into_inner();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert_eq!(
            text,
            "serialist-timing 1\n\
             connect 0 virtual:race @ 115200 8N1\n\
             rx 2000 0 5\n\
             rx 7000 5 3\n\
             disconnect 9000\n\
             connect 20000 line one  line two\n\
             rx 21000 8 1\n"
        );

        let timing = Timing::read(bytes.as_slice()).unwrap();
        assert!(!timing.truncated);
        assert_eq!(
            timing.records,
            [
                TimingRecord::Connect {
                    at: Duration::ZERO,
                    description: "virtual:race @ 115200 8N1".into()
                },
                rx(2000, 0, 5),
                rx(7000, 5, 3),
                TimingRecord::Disconnect { at: 9 * MS },
                TimingRecord::Connect {
                    at: 20 * MS,
                    description: "line one  line two".into()
                },
                rx(21_000, 8, 1),
            ]
        );
    }

    #[test]
    fn times_never_go_backwards_in_the_file() {
        let origin = Instant::now() + 10 * MS;
        let mut writer = TimingWriter::new(Vec::new(), origin).unwrap();
        // Before the origin counts as the origin; an instant earlier than the last one
        // counts as the last one.
        writer.rx(origin - 5 * MS, 1).unwrap();
        writer.rx(origin + 4 * MS, 1).unwrap();
        writer.rx(origin + 3 * MS, 1).unwrap();
        let timing = Timing::read(writer.into_inner().as_slice()).unwrap();
        assert_eq!(
            timing.records,
            [rx(0, 0, 1), rx(4000, 1, 1), rx(4000, 2, 1)]
        );
    }

    #[test]
    fn comments_blank_lines_crlf_and_unknown_kinds_are_skipped() {
        let timing = read(
            "serialist-timing 1\r\n\
             # recorded by hand\n\
             \n\
             rx 10 0 4\r\n\
             tx 11 0 2\n\
             future_kind with any fields\n\
             rx 12 4 4\n",
        )
        .unwrap();
        assert_eq!(timing.records, [rx(10, 0, 4), rx(12, 4, 4)]);
    }

    #[test]
    fn a_cut_short_last_line_is_tolerated() {
        let timing = read("serialist-timing 1\nrx 10 0 4\nrx 12 4").unwrap();
        assert!(timing.truncated);
        assert_eq!(timing.records, [rx(10, 0, 4)]);

        let complete = read("serialist-timing 1\nrx 10 0 4\nrx 12 4 1\n").unwrap();
        assert!(!complete.truncated);
        assert_eq!(complete.records.len(), 2);

        let not_utf8 = Timing::read(&b"serialist-timing 1\nrx 10 0 4\nrx 1\xff"[..]).unwrap();
        assert!(not_utf8.truncated);
        assert_eq!(not_utf8.records, [rx(10, 0, 4)]);

        let error = read("serialist-timing 1\nrx 12 4\nrx 13 0 1\n").unwrap_err();
        assert!(
            matches!(error, TimingError::Line { line: 2, .. }),
            "a bad line in the middle is an error: {error}"
        );
    }

    #[test]
    fn an_unterminated_last_line_is_dropped_even_when_it_parses() {
        // `rx 100 0 50` cut after its `5` is a valid record for five bytes.
        let timing = read("serialist-timing 1\nrx 100 0 5").unwrap();
        assert!(timing.truncated);
        assert_eq!(timing.records, []);
        assert_eq!(timing.rx().count(), 0, "the record is absent");
        assert_eq!(
            timing.schedule(50),
            [ScheduledChunk {
                at: Duration::ZERO,
                offset: 0,
                len: 50
            }],
            "the raw file is still covered, as one chunk"
        );

        // After a complete record, the raw bytes the lost line described land in the
        // tail chunk, due with the last record that survived.
        let timing = read("serialist-timing 1\nrx 100 0 20\nrx 900 20 5").unwrap();
        assert!(timing.truncated);
        assert_eq!(timing.records, [rx(100, 0, 20)]);
        assert_eq!(
            timing.schedule(50),
            [
                ScheduledChunk {
                    at: Duration::ZERO,
                    offset: 0,
                    len: 20
                },
                ScheduledChunk {
                    at: Duration::ZERO,
                    offset: 20,
                    len: 30
                }
            ]
        );

        // Other kinds of last line are dropped the same way, and the same line with its
        // `\n` is read.
        for last in ["connect 5 tcp:h:1", "disconnect 5", "# note", ""] {
            let cut = read(&format!("serialist-timing 1\nrx 1 0 4\n{last}")).unwrap();
            assert_eq!(cut.truncated, !last.is_empty(), "{last:?}");
            assert_eq!(cut.records, [rx(1, 0, 4)], "{last:?}");
        }
        assert_eq!(
            read("serialist-timing 1\nrx 1 0 4\ndisconnect 5\n")
                .unwrap()
                .records
                .len(),
            2
        );
    }

    #[test]
    fn bad_sidecars_are_errors() {
        assert!(matches!(read(""), Err(TimingError::NotTiming)));
        assert!(
            matches!(read("serialist-timing 1"), Err(TimingError::NotTiming)),
            "a header without its newline may be a cut `serialist-timing 10`"
        );
        assert!(matches!(read("rx 1 0 1\n"), Err(TimingError::NotTiming)));
        assert!(matches!(
            read("serialist-timing x\n"),
            Err(TimingError::NotTiming)
        ));
        assert!(matches!(
            read("serialist-timing 2\nrx 1 0 1\n"),
            Err(TimingError::Version(2))
        ));
        let line = |text: &str| match read(text) {
            Err(TimingError::Line { line, message }) => (line, message),
            other => panic!("{text:?}: {other:?}"),
        };
        assert_eq!(line("serialist-timing 1\nrx 5 0 0\n").0, 2);
        assert_eq!(line("serialist-timing 1\nrx 5 0 1 9\n").0, 2);
        assert_eq!(line("serialist-timing 1\nRX 5 0 1\n").0, 2);
        assert_eq!(
            line("serialist-timing 1\nrx 5 0 1\nrx 6 2 1\n"),
            (3, "rx record starts at offset 2, expected 1".into())
        );
        assert_eq!(
            line("serialist-timing 1\nrx 5 0 1\ndisconnect 4\n"),
            (3, "time 4 us is before the previous record's 5 us".into())
        );
        assert_eq!(line("serialist-timing 1\nrx 5 1 1\n").0, 2, "starts at 0");
    }

    #[test]
    fn the_schedule_covers_the_raw_file_exactly() {
        let timing = read("serialist-timing 1\nrx 1000 0 4\nrx 1500 4 2\nrx 9000 6 4\n").unwrap();
        let chunk = |at_us: u64, offset: u64, len: u64| ScheduledChunk {
            at: Duration::from_micros(at_us),
            offset,
            len,
        };
        assert_eq!(
            timing.schedule(10),
            [chunk(0, 0, 4), chunk(500, 4, 2), chunk(8000, 6, 4)],
            "relative to the first chunk"
        );
        assert_eq!(
            timing.schedule(7),
            [chunk(0, 0, 4), chunk(500, 4, 2), chunk(8000, 6, 1)],
            "records past the raw file are cut back to it"
        );
        assert_eq!(
            timing.schedule(5),
            [chunk(0, 0, 4), chunk(500, 4, 1)],
            "and dropped once wholly past it"
        );
        assert_eq!(
            timing.schedule(13),
            [
                chunk(0, 0, 4),
                chunk(500, 4, 2),
                chunk(8000, 6, 4),
                chunk(8000, 10, 3)
            ],
            "bytes after the last record follow it at once"
        );
        assert_eq!(timing.schedule(0), []);
        assert_eq!(
            Timing::default().schedule(3),
            [chunk(0, 0, 3)],
            "no records: one chunk"
        );
    }

    proptest! {
        /// Any run of chunks and link events survives a write and a read, and its
        /// schedule rebuilds the chunk boundaries exactly.
        #[test]
        fn round_trip(events in prop::collection::vec((0u64..5_000, 0usize..3, 1usize..5000), 0..64)) {
            let origin = Instant::now();
            let mut writer = TimingWriter::new(Vec::new(), origin).unwrap();
            let mut t = Duration::ZERO;
            let mut chunks = Vec::new();
            let mut raw_len = 0u64;
            for (gap_us, kind, len) in events {
                t += Duration::from_micros(gap_us);
                match kind {
                    0 => writer.connect(origin + t, "tcp:host:1 (10.0.0.1:1)").unwrap(),
                    1 => writer.disconnect(origin + t).unwrap(),
                    _ => {
                        writer.rx(origin + t, len).unwrap();
                        chunks.push((t, raw_len, len as u64));
                        raw_len += len as u64;
                    }
                }
            }
            let timing = Timing::read(writer.into_inner().as_slice()).unwrap();
            let rx: Vec<_> = timing.rx().collect();
            prop_assert_eq!(&rx, &chunks);
            let schedule = timing.schedule(raw_len);
            prop_assert_eq!(schedule.len(), chunks.len());
            let base = chunks.first().map_or(Duration::ZERO, |c| c.0);
            for (scheduled, (at, offset, len)) in schedule.iter().zip(&chunks) {
                prop_assert_eq!(scheduled.at, *at - base);
                prop_assert_eq!((scheduled.offset, scheduled.len), (*offset, *len));
            }
        }
    }
}
