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

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serialist_core::store::write_lines;
use serialist_core::{HexView, LineId, Snapshot, TextExportReport, TextOptions};

use crate::status::{file_name, format_bytes};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    /// The displayed lines, each ending in `\n`.
    Text,
    /// The raw bytes as received, byte for byte.
    Raw,
}

impl ExportFormat {
    /// The format a file name asks for: `.bin` and `.raw` are raw, `.txt`, `.log` and
    /// `.text` are text, anything else is up to the caller.
    pub fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        match extension.as_str() {
            "bin" | "raw" => Some(ExportFormat::Raw),
            "txt" | "log" | "text" => Some(ExportFormat::Text),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Text => "txt",
            ExportFormat::Raw => "bin",
        }
    }
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
    use crate::test_support::TestDir;

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
