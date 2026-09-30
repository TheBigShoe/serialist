//! Writing the scrollback or the raw capture to a file.
//!
//! Every export goes to a temporary file next to the target and is renamed into place
//! only once fully written and synced, so a failed export never leaves a partial file
//! and never clobbers an existing one. The functions take a path; the save dialog is
//! only ever their caller.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ExportFormat {
    /// The displayed lines, each ending in `\n`.
    Text,
    /// The raw capture, byte for byte.
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

/// What to export, captured on the main thread (cheap: the displayed lines, or shared
/// raw chunks) and written on a background thread.
#[derive(Clone, Debug)]
pub enum ExportJob {
    Text(Vec<String>),
    Raw {
        chunks: Vec<Arc<[u8]>>,
        /// Older bytes the raw ring had already dropped, for the summary.
        evicted: u64,
    },
}

impl ExportJob {
    /// Write the file and describe the outcome for the status line.
    pub fn run(&self, path: &Path) -> Result<String, String> {
        let name = crate::session_model::file_name(path);
        let result = match self {
            ExportJob::Text(lines) => export_text(path, lines)
                .map(|_| format!("Exported {} lines to {name}", lines.len())),
            ExportJob::Raw { chunks, evicted } => export_raw(path, chunks).map(|bytes| {
                let size = crate::session_model::format_bytes(bytes);
                match evicted {
                    0 => format!("Exported {size} raw to {name}"),
                    evicted => format!(
                        "Exported {size} raw to {name}; {} older were already dropped",
                        crate::session_model::format_bytes(*evicted)
                    ),
                }
            }),
        };
        result.map_err(|error| format!("Export to {name} failed: {error}"))
    }
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

/// Lines as text, each followed by `\n`. Returns the bytes written.
pub fn export_text(path: &Path, lines: &[String]) -> io::Result<u64> {
    write_atomically(path, |out| {
        let mut bytes = 0;
        for line in lines {
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
            bytes += line.len() as u64 + 1;
        }
        Ok(bytes)
    })
}

/// Raw chunks, byte for byte. Returns the bytes written.
pub fn export_raw(path: &Path, chunks: &[Arc<[u8]>]) -> io::Result<u64> {
    write_atomically(path, |out| {
        let mut bytes = 0;
        for chunk in chunks {
            out.write_all(chunk)?;
            bytes += chunk.len() as u64;
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
    fn text_and_raw_exports_write_exactly_their_content() {
        let dir = TestDir::new("export-content");
        let text = dir.path().join("lines.txt");
        let lines = vec!["one".to_owned(), String::new(), "three".to_owned()];
        assert_eq!(export_text(&text, &lines).unwrap(), 11);
        assert_eq!(fs::read(&text).unwrap(), b"one\n\nthree\n");

        let raw = dir.path().join("dump.bin");
        let chunks: Vec<Arc<[u8]>> = vec![Arc::from(&b"\x00\xff"[..]), Arc::from(&b"\r\n"[..])];
        assert_eq!(export_raw(&raw, &chunks).unwrap(), 4);
        assert_eq!(fs::read(&raw).unwrap(), b"\x00\xff\r\n");
        assert_eq!(
            entries(dir.path()),
            ["dump.bin", "lines.txt"],
            "no temp files left"
        );
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
    fn job_summaries_name_the_file_and_the_evicted_bytes() {
        let dir = TestDir::new("export-summary");
        let text = ExportJob::Text(vec!["a".into(), "b".into()]);
        assert_eq!(
            text.run(&dir.path().join("x.txt")),
            Ok("Exported 2 lines to x.txt".to_owned())
        );
        let raw = ExportJob::Raw {
            chunks: vec![Arc::from(&b"abc"[..])],
            evicted: 5 * 1024 * 1024,
        };
        assert_eq!(
            raw.run(&dir.path().join("x.bin")),
            Ok("Exported 3 B raw to x.bin; 5.0 MiB older were already dropped".to_owned())
        );
        let failed = text.run(&dir.path().join("missing").join("y.txt"));
        assert!(failed.unwrap_err().starts_with("Export to y.txt failed: "));
    }

    #[test]
    fn exporting_into_a_missing_directory_fails_cleanly() {
        let dir = TestDir::new("export-missing-dir");
        let target = dir.path().join("nope").join("out.txt");
        assert!(export_text(&target, &["x".to_owned()]).is_err());
        assert!(entries(dir.path()).is_empty());
    }
}
