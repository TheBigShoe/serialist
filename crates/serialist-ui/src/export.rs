//! Writing the scrollback or the raw stream to a file.
//!
//! An export is taken on the main thread as a store [`Snapshot`] plus a range (an `Arc`
//! clone, no copying) and written on a background thread: text through
//! [`Snapshot::write_text_counted`], hex rows through the store's [`write_lines`] (the
//! same stamps, the same counts), raw bytes straight from the store's pages with
//! [`Snapshot::raw`]. What a text export wrote, lines and bytes, comes back as the
//! store's [`TextExportReport`]. Both text jobs carry a full [`TextOptions`], so what
//! stamps a line (the mode, and for absolute stamps the `strftime` format from
//! `display.timestamp_format`) is decided when the job is taken, in the session view.
//!
//! Every export goes to a temporary file next to the target and is renamed into place
//! only once fully written and synced, so a failed export never leaves a partial file
//! and never clobbers an existing one. The functions take a path; the save dialog is
//! only ever their caller.
//!
//! # Decoded frames
//!
//! While a codec decodes the session, `.csv` and `.json` export the decoded frames: every
//! frame the session's frame store retains, from a [`FrameSnapshot`] taken with the job,
//! with the raw bytes of each read from the store [`Snapshot`] by its stream offsets.
//!
//! - **CSV**: a header row, then one row per frame: `time`, `direction`, `kind`,
//!   `summary`, one column per field name of the kinds present (in the order the fields
//!   first appear; a frame without that field leaves it empty), and `raw`, the frame's
//!   bytes as hex. Cells with commas, quotes or line breaks are quoted.
//! - **JSON**: an array of objects with `id`, `time`, `direction`, `kind`, `severity`,
//!   `summary`, `fields` (numbers as numbers, bytes as hex), `raw_start`, `raw_end` and
//!   `raw` (hex, or `null` once the store has evicted the bytes).
//!
//! Times are stamped as the Decoded panel stamps them ([`FrameTime`]).

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value as JsonValue, json};
use serialist_core::codec::encode_hex;
use serialist_core::store::write_lines;
use serialist_core::{
    Frame, FrameSnapshot, HexView, LineId, LineSource, Snapshot, TextExportReport, TextOptions,
};

use crate::codecs::{FrameTime, direction_label, value_json};
use crate::status::{file_name, format_bytes};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    /// The displayed lines, each ending in `\n`.
    Text,
    /// The raw bytes as received, byte for byte.
    Raw,
    /// The decoded frames as CSV.
    Csv,
    /// The decoded frames as a JSON array.
    Json,
}

impl ExportFormat {
    /// The format a file name asks for: `.bin` and `.raw` are raw, `.txt`, `.log` and
    /// `.text` are text, `.csv` and `.json` are decoded frames, anything else is up to
    /// the caller.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "bin" | "raw" => Some(ExportFormat::Raw),
            "txt" | "log" | "text" => Some(ExportFormat::Text),
            "csv" => Some(ExportFormat::Csv),
            "json" => Some(ExportFormat::Json),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Text => "txt",
            ExportFormat::Raw => "bin",
            ExportFormat::Csv => "csv",
            ExportFormat::Json => "json",
        }
    }

    /// Whether this format writes decoded frames, which needs a codec.
    pub fn is_decoded(self) -> bool {
        matches!(self, ExportFormat::Csv | ExportFormat::Json)
    }
}

/// Which file a frames export writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FramesFormat {
    Csv,
    Json,
}

/// What to export, taken on the main thread and written on a background thread.
#[derive(Clone, Debug)]
pub enum ExportJob {
    /// Lines `lines` of the snapshot as displayed: escapes applied, one per line.
    Text {
        snapshot: Snapshot,
        lines: Range<LineId>,
        options: TextOptions,
    },
    /// Hex rows `rows` as displayed, stamped like text lines: `options` are the same as
    /// for [`ExportJob::Text`], so absolute stamps use the configured format.
    HexText {
        hex: HexView,
        rows: Range<LineId>,
        options: TextOptions,
    },
    /// Stream bytes `range`, clipped to what the snapshot retains.
    Raw {
        snapshot: Snapshot,
        range: Range<u64>,
    },
    /// Lines `lines` of any source as displayed, such as the text view with framed
    /// bytes hidden, whose ids are its own.
    Lines {
        source: SharedSource,
        lines: Range<LineId>,
        options: TextOptions,
    },
    /// Every retained decoded frame, with its bytes from `raw`.
    Frames {
        frames: FrameSnapshot,
        raw: Snapshot,
        time: FrameTime,
        format: FramesFormat,
    },
}

/// A line source an export job holds.
#[derive(Clone)]
pub struct SharedSource(pub Arc<dyn LineSource>);

impl std::fmt::Debug for SharedSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedSource")
            .field("lines", &(self.0.first_line()..self.0.end()))
            .finish()
    }
}

impl ExportJob {
    /// Write the file and describe the outcome for the status line.
    pub fn run(&self, path: &Path) -> Result<String, String> {
        let name = file_name(path);
        let result = match self {
            ExportJob::Text {
                snapshot,
                lines,
                options,
            } => write_report(path, |mut out| {
                snapshot.write_text_counted(lines.clone(), options.clone(), &mut out)
            })
            .map(|report| format!("Exported {} lines to {name}", report.lines)),
            ExportJob::HexText { hex, rows, options } => write_report(path, |out| {
                write_lines(hex, rows.clone(), options.clone(), out)
            })
            .map(|report| format!("Exported {} hex rows to {name}", report.lines)),
            ExportJob::Lines {
                source,
                lines,
                options,
            } => write_report(path, |out| {
                write_lines(source.0.as_ref(), lines.clone(), options.clone(), out)
            })
            .map(|report| format!("Exported {} lines to {name}", report.lines)),
            ExportJob::Frames {
                frames,
                raw,
                time,
                format,
            } => {
                let mut count = 0;
                write_atomically(path, |out| {
                    let (written, frames) = match format {
                        FramesFormat::Csv => write_frames_csv(frames, raw, time, out)?,
                        FramesFormat::Json => write_frames_json(frames, raw, time, out)?,
                    };
                    count = frames;
                    Ok(written)
                })
                .map(|_| format!("Exported {count} frames to {name}"))
            }
            ExportJob::Raw { snapshot, range } => {
                let evicted = evicted_bytes(snapshot, range);
                export_raw(path, snapshot, range.clone()).map(|bytes| {
                    let size = format_bytes(bytes);
                    match evicted {
                        0 => format!("Exported {size} raw to {name}"),
                        evicted => format!(
                            "Exported {size} raw to {name}; {} older were already dropped",
                            format_bytes(evicted)
                        ),
                    }
                })
            }
        };
        result.map_err(|error| format!("Export to {name} failed: {error}"))
    }
}

/// Bytes of `range` the store had already evicted from `snapshot`.
fn evicted_bytes(snapshot: &Snapshot, range: &Range<u64>) -> u64 {
    let end = range.end.max(range.start);
    snapshot.raw_range().start.clamp(range.start, end) - range.start
}

/// [`write_atomically`] for a text export: whatever `write` reports it wrote.
fn write_report(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> io::Result<TextExportReport>,
) -> io::Result<TextExportReport> {
    let mut report = TextExportReport::default();
    write_atomically(path, |out| {
        report = write(out)?;
        Ok(report.bytes)
    })?;
    Ok(report)
}

/// Counts the bytes that pass through, for the frame writers' reports.
struct Counting<'a> {
    out: &'a mut dyn Write,
    bytes: u64,
}

impl Write for Counting<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.out.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The bytes of `frame` as spaced hex, or `None` if the store no longer has all of them.
pub fn frame_raw_hex(raw: &Snapshot, frame: &Frame) -> Option<String> {
    let kept = raw.raw_range();
    if frame.raw.start < kept.start || frame.raw.end > kept.end {
        return None;
    }
    let mut bytes = Vec::with_capacity(frame.raw_len() as usize);
    for slice in raw.raw(frame.raw.clone()) {
        bytes.extend_from_slice(slice);
    }
    Some(encode_hex(&bytes, " "))
}

/// Append `text` to a CSV row, quoted when it must be.
fn csv_cell(row: &mut String, text: &str) {
    if !row.is_empty() {
        row.push(',');
    }
    if text.contains([',', '"', '\n', '\r']) {
        row.push('"');
        row.push_str(&text.replace('"', "\"\""));
        row.push('"');
    } else {
        row.push_str(text);
    }
}

/// The CSV form of a frames export; see the module docs. Returns the bytes and frames
/// written.
fn write_frames_csv(
    frames: &FrameSnapshot,
    raw: &Snapshot,
    time: &FrameTime,
    out: &mut dyn Write,
) -> io::Result<(u64, usize)> {
    let mut out = Counting { out, bytes: 0 };
    // The field columns: every field name the frames carry, in order of first sight.
    let mut fields: Vec<&str> = Vec::new();
    for (_, frame) in frames.frames() {
        for (name, _) in &frame.fields {
            if !fields.contains(&name.as_str()) {
                fields.push(name.as_str());
            }
        }
    }
    let mut row = String::new();
    for header in ["time", "direction", "kind", "summary"]
        .into_iter()
        .chain(fields.iter().copied())
        .chain(["raw"])
    {
        csv_cell(&mut row, header);
    }
    writeln!(out, "{row}")?;
    let mut previous = None;
    let mut count = 0;
    for (_, frame) in frames.frames() {
        row.clear();
        csv_cell(&mut row, &time.stamp(frame.at, previous));
        csv_cell(&mut row, direction_label(frame));
        csv_cell(&mut row, &frame.kind);
        csv_cell(&mut row, &frame.summary);
        for name in &fields {
            let value = frame
                .field(name)
                .map(ToString::to_string)
                .unwrap_or_default();
            csv_cell(&mut row, &value);
        }
        csv_cell(&mut row, &frame_raw_hex(raw, frame).unwrap_or_default());
        writeln!(out, "{row}")?;
        previous = Some(frame.at);
        count += 1;
    }
    Ok((out.bytes, count))
}

/// The JSON form of a frames export; see the module docs. Returns the bytes and frames
/// written.
fn write_frames_json(
    frames: &FrameSnapshot,
    raw: &Snapshot,
    time: &FrameTime,
    out: &mut dyn Write,
) -> io::Result<(u64, usize)> {
    let mut out = Counting { out, bytes: 0 };
    out.write_all(b"[")?;
    let mut previous = None;
    let mut count = 0;
    for (id, frame) in frames.frames() {
        let fields: Map<String, JsonValue> = frame
            .fields
            .iter()
            .map(|(name, value)| (name.to_string(), value_json(value)))
            .collect();
        let object = json!({
            "id": id.0,
            "time": time.stamp(frame.at, previous),
            "direction": direction_label(frame),
            "kind": frame.kind.as_str(),
            "severity": frame.severity.name(),
            "summary": frame.summary,
            "fields": fields,
            "raw_start": frame.raw.start,
            "raw_end": frame.raw.end,
            "raw": frame_raw_hex(raw, frame),
        });
        out.write_all(if count == 0 { b"\n  " } else { b",\n  " })?;
        serde_json::to_writer(&mut out, &object).map_err(io::Error::other)?;
        previous = Some(frame.at);
        count += 1;
    }
    out.write_all(if count == 0 { b"]\n" } else { b"\n]\n" })?;
    Ok((out.bytes, count))
}

/// Write `path` through a temporary sibling file. `write` returns the bytes it wrote.
pub fn write_atomically(
    path: &Path,
    write: impl FnOnce(&mut dyn Write) -> io::Result<u64>,
) -> io::Result<u64> {
    let (temp_path, file) = create_temp_sibling(path)?;
    let written = (|| {
        let mut out = BufWriter::new(file);
        let bytes = write(&mut out)?;
        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp_path, path)?;
        Ok(bytes)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    written
}

/// Stream bytes `range` of `snapshot`, clipped to what it retains, byte for byte.
/// Returns the bytes written.
pub fn export_raw(path: &Path, snapshot: &Snapshot, range: Range<u64>) -> io::Result<u64> {
    write_atomically(path, |out| {
        let mut bytes = 0;
        for slice in snapshot.raw(range) {
            out.write_all(slice)?;
            bytes += slice.len() as u64;
        }
        Ok(bytes)
    })
}

/// `.<name>.<pid>-<n>.partial` in the target's directory, so the final rename stays on
/// one filesystem and is atomic.
fn create_temp_sibling(path: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "export path has no file name")
    })?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".{}-{n}.partial", std::process::id()));
        let temp_path = dir.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use serialist_core::{Direction, Epoch, LineSource, Store, StoreConfig, Timestamps};

    use super::*;
    use crate::test_support::{TestDir, parse_csv};

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A store holding two received lines 1.5 s and 2 s after `epoch`, a sent line,
    /// and a received line still waiting for its LF.
    fn sample(epoch: Epoch) -> Store {
        let mut store = Store::new(StoreConfig {
            epoch: Some(epoch),
            ..StoreConfig::default()
        });
        let at = |ms: u64| epoch.instant + Duration::from_millis(ms);
        store.append(b"\x1b[31mone\x1b[0m\r\n", at(1500));
        store.append(b"\x00\xfftwo\r\n", at(2000));
        store.append_local_at("AT", Direction::Tx, at(2250));
        store.append(b"par", at(3000));
        store
    }

    #[test]
    fn format_follows_the_extension() {
        let format = |name: &str| ExportFormat::from_path(Path::new(name));
        assert_eq!(format("log.txt"), Some(ExportFormat::Text));
        assert_eq!(format("boot.LOG"), Some(ExportFormat::Text));
        assert_eq!(format("capture.bin"), Some(ExportFormat::Raw));
        assert_eq!(format("capture.raw"), Some(ExportFormat::Raw));
        assert_eq!(format("capture.dat"), None);
        assert_eq!(format("noextension"), None);
        assert_eq!(format("frames.CSV"), Some(ExportFormat::Csv));
        assert_eq!(format("frames.json"), Some(ExportFormat::Json));
        assert!(ExportFormat::Csv.is_decoded() && ExportFormat::Json.is_decoded());
        assert!(!ExportFormat::Text.is_decoded());
    }

    #[test]
    fn decoded_frames_export_as_csv_and_json() {
        use serialist_core::{Codec, FrameStore};
        use serialist_plugins::AirohaRace;
        use serialist_plugins::race::{RaceType, encode_frame};

        use crate::terminal::{Clock, TimestampMode};

        let dir = TestDir::new("export-frames");
        let epoch = Epoch::now();
        let mut store = Store::new(StoreConfig {
            epoch: Some(epoch),
            ..StoreConfig::default()
        });
        let mut frames = FrameStore::default();
        let mut race = AirohaRace::new();
        let mut stream = b"hello, \"world\"\r\n".to_vec();
        stream.extend(encode_frame(RaceType::Log, 0x0F40, b"boot").unwrap());
        stream.extend(encode_frame(RaceType::Response, 0x0F15, b"\x00V1").unwrap());
        let at = epoch.instant + Duration::from_millis(1500);
        store.append(&stream, at);
        let mut out = Vec::new();
        race.decode(&stream, at, 0, &mut out);
        frames.extend(out);
        let time = FrameTime {
            clock: Clock::fixed(epoch, 0),
            mode: TimestampMode::Relative,
            format: None,
        };
        let job = |format| ExportJob::Frames {
            frames: frames.snapshot(),
            raw: store.snapshot(),
            time: time.clone(),
            format,
        };

        let csv = dir.join("frames.csv");
        assert_eq!(
            job(FramesFormat::Csv).run(&csv),
            Ok("Exported 3 frames to frames.csv".into())
        );
        let rows = parse_csv(&fs::read_to_string(&csv).unwrap());
        assert_eq!(rows.len(), 4, "a header and three frames");
        assert_eq!(
            rows[0][..5],
            ["time", "direction", "kind", "summary", "text"]
        );
        let column = |name: &str| rows[0].iter().position(|h| h == name).unwrap();
        assert_eq!(rows[1][column("kind")], "text");
        assert_eq!(
            rows[1][column("summary")],
            "hello, \"world\"",
            "quoted and back"
        );
        assert_eq!(rows[1][column("time")], "+00:00:01.500");
        assert_eq!(rows[2][column("kind")], "log");
        assert_eq!(rows[2][column("cmd_id")], "3904");
        assert_eq!(rows[2][column("text")], "", "a log frame has no text");
        assert_eq!(rows[3][column("cmd_id")], "3861");
        assert_eq!(rows[3][column("raw")], "05 5B 05 00 15 0F 00 56 31");
        assert!(rows.iter().all(|row| row.len() == rows[0].len()));

        let json = dir.join("frames.json");
        assert_eq!(
            job(FramesFormat::Json).run(&json),
            Ok("Exported 3 frames to frames.json".into())
        );
        let parsed: JsonValue = serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
        let array = parsed.as_array().unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array[2]["kind"], "response");
        assert_eq!(array[2]["fields"]["cmd_id"], 0x0F15);
        assert_eq!(array[2]["fields"]["payload"], "005631");
        assert_eq!(array[2]["raw"], "05 5B 05 00 15 0F 00 56 31");
        assert_eq!(array[1]["severity"], "info");
        assert_eq!(array[0]["raw_start"], 0);
        assert_eq!(entries(dir.path()), ["frames.csv", "frames.json"]);
    }

    #[test]
    fn text_export_writes_lines_as_displayed_with_the_chosen_stamps() {
        let dir = TestDir::new("export-text");
        let snapshot = sample(Epoch::now()).snapshot();
        let path = dir.join("lines.txt");
        let job = |lines: Range<LineId>, timestamps| ExportJob::Text {
            snapshot: snapshot.clone(),
            lines,
            options: TextOptions::default().with_timestamps(timestamps),
        };

        let all = job(LineId(0)..LineId(4), Timestamps::None);
        assert_eq!(all.run(&path), Ok("Exported 4 lines to lines.txt".into()));
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "one", "escapes applied");
        assert!(lines[1].ends_with("two") && lines[1].contains('\u{fffd}'));
        assert_eq!(lines[2..], ["AT", "par"]);

        let stamped = job(LineId(2)..LineId(4), Timestamps::Relative);
        assert_eq!(
            stamped.run(&path),
            Ok("Exported 2 lines to lines.txt".into())
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[+2.250000] AT\n[+3.000000] par\n"
        );
        let delta = job(LineId(2)..LineId(4), Timestamps::Delta);
        delta.run(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[+0.000000] AT\n[+0.750000] par\n"
        );
        assert_eq!(entries(dir.path()), ["lines.txt"], "no temp files left");
    }

    #[test]
    fn hex_rows_export_as_displayed() {
        let dir = TestDir::new("export-hex");
        let snapshot = sample(Epoch::now()).snapshot();
        let hex = snapshot.hex_view(16);
        let path = dir.join("dump.txt");
        let job = ExportJob::HexText {
            hex: hex.clone(),
            rows: LineId(0)..LineId(2),
            options: TextOptions::default(),
        };
        assert_eq!(job.run(&path), Ok("Exported 2 hex rows to dump.txt".into()));
        let expected = format!(
            "{}\n{}\n",
            hex.line(LineId(0)).unwrap().text,
            hex.line(LineId(1)).unwrap().text
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        assert!(expected.starts_with("00000000  1b 5b 33 31 6d 6f 6e 65"));
    }

    #[test]
    fn hex_rows_are_stamped_the_way_text_lines_are() {
        let dir = TestDir::new("export-hex-stamped");
        let snapshot = sample(Epoch::now()).snapshot();
        let hex = snapshot.hex_view(16);
        let (first, second) = (
            hex.line(LineId(0)).unwrap().text,
            hex.line(LineId(1)).unwrap().text,
        );
        let path = dir.join("dump.txt");
        let job = |timestamps| ExportJob::HexText {
            hex: hex.clone(),
            rows: LineId(0)..LineId(2),
            options: TextOptions::default().with_timestamps(timestamps),
        };
        // Row 0 starts in the line that arrived at 1.5 s, row 1 in the one at 2 s.
        job(Timestamps::Relative).run(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("[+1.500000] {first}\n[+2.000000] {second}\n")
        );
        job(Timestamps::Delta).run(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("[+0.000000] {first}\n[+0.500000] {second}\n")
        );
    }

    #[test]
    fn absolute_stamps_use_the_format_the_job_carries() {
        let dir = TestDir::new("export-format");
        // A known wall clock, so the seconds can be checked in any time zone (offsets
        // are whole minutes).
        let epoch = Epoch {
            instant: Instant::now(),
            wall: std::time::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        };
        let snapshot = sample(epoch).snapshot();
        let options = TextOptions::default()
            .with_timestamps(Timestamps::Absolute)
            .with_timestamp_format("%S%.3f");
        let path = dir.join("stamped.txt");

        let text = ExportJob::Text {
            snapshot: snapshot.clone(),
            lines: LineId(0)..LineId(4),
            options: options.clone(),
        };
        text.run(&path).unwrap();
        let second = snapshot.line(LineId(1)).unwrap().text;
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!("[41.500] one\n[42.000] {second}\n[42.250] AT\n[43.000] par\n")
        );

        // The same options stamp hex rows.
        let hex = snapshot.hex_view(16);
        let rows = ExportJob::HexText {
            hex: hex.clone(),
            rows: LineId(0)..LineId(2),
            options,
        };
        rows.run(&path).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            format!(
                "[41.500] {}\n[42.000] {}\n",
                hex.line(LineId(0)).unwrap().text,
                hex.line(LineId(1)).unwrap().text
            )
        );

        // Without a format the default applies: the time of day.
        let default = ExportJob::Text {
            snapshot,
            lines: LineId(0)..LineId(1),
            options: TextOptions::default().with_timestamps(Timestamps::Absolute),
        };
        default.run(&path).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let stamp = text.split(']').next().unwrap();
        assert_eq!(stamp.len(), "[hh:mm:ss.mmm".len(), "{text}");
        assert!(stamp.ends_with(":41.500"), "{text}");
    }

    #[test]
    fn a_text_export_reports_the_lines_and_bytes_it_wrote() {
        let dir = TestDir::new("export-report");
        let snapshot = sample(Epoch::now()).snapshot();
        let path = dir.join("lines.txt");
        let options = TextOptions::default().with_timestamps(Timestamps::Relative);
        let report = write_report(&path, |mut out| {
            snapshot.write_text_counted(LineId(0)..LineId(4), options.clone(), &mut out)
        })
        .unwrap();
        assert_eq!(report.lines, 4);
        assert_eq!(report.bytes, fs::metadata(&path).unwrap().len());
        // A line the options drop is not counted: the sent line is not received.
        let received = TextOptions::received_only();
        let report = write_report(&path, |mut out| {
            snapshot.write_text_counted(LineId(0)..LineId(4), received.clone(), &mut out)
        })
        .unwrap();
        assert_eq!(report.lines, 3);
        assert_eq!(report.bytes, fs::metadata(&path).unwrap().len());
        // A failure is the error, with no report.
        let missing = dir.path().join("nope").join("out.txt");
        assert!(write_report(&missing, |_| Ok(TextExportReport::default())).is_err());
    }

    #[test]
    fn raw_export_is_the_stream_byte_for_byte_and_counts_what_was_evicted() {
        let dir = TestDir::new("export-raw");
        let snapshot = sample(Epoch::now()).snapshot();
        let path = dir.join("dump.bin");
        let raw = ExportJob::Raw {
            snapshot: snapshot.clone(),
            range: 0..u64::MAX,
        };
        assert_eq!(raw.run(&path), Ok("Exported 24 B raw to dump.bin".into()));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"\x1b[31mone\x1b[0m\r\n\x00\xfftwo\r\npar"
        );

        // A store over its budget drops its oldest pages.
        let mut store = Store::with_budget(0);
        let total = store.budget() * 2;
        let pattern = b"0123456789abcde\n";
        let stream: Vec<u8> = (0..total).map(|i| pattern[i % pattern.len()]).collect();
        for chunk in stream.chunks(4096) {
            store.append(chunk, Instant::now());
        }
        let snapshot = store.snapshot();
        let kept = snapshot.raw_range();
        assert!(kept.start > 0, "something was evicted");
        let job = ExportJob::Raw {
            snapshot,
            range: 0..kept.end,
        };
        let path = dir.join("tail.bin");
        assert_eq!(
            job.run(&path),
            Ok(format!(
                "Exported {} raw to tail.bin; {} older were already dropped",
                format_bytes(kept.end - kept.start),
                format_bytes(kept.start)
            ))
        );
        assert!(fs::read(&path).unwrap() == stream[kept.start as usize..]);
    }

    #[test]
    fn a_failed_export_leaves_no_partial_file_and_keeps_the_old_one() {
        let dir = TestDir::new("export-failure");
        let target = dir.path().join("keep.txt");
        fs::write(&target, "previous export\n").unwrap();

        let result = write_atomically(&target, |out| {
            out.write_all(b"half of the new")?;
            Err(io::Error::other("disk full"))
        });
        assert_eq!(result.unwrap_err().to_string(), "disk full");
        assert_eq!(fs::read_to_string(&target).unwrap(), "previous export\n");
        assert_eq!(entries(dir.path()), ["keep.txt"]);
    }

    #[test]
    fn exporting_into_a_missing_directory_fails_cleanly() {
        let dir = TestDir::new("export-missing-dir");
        let snapshot = sample(Epoch::now()).snapshot();
        let job = ExportJob::Text {
            snapshot,
            lines: LineId(0)..LineId(4),
            options: TextOptions::default(),
        };
        let failed = job.run(&dir.path().join("nope").join("out.txt"));
        assert!(
            failed
                .unwrap_err()
                .starts_with("Export to out.txt failed: ")
        );
        assert!(entries(dir.path()).is_empty());
    }
}
