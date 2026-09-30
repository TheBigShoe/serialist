//! The compose bar's history: what was sent, newest first, kept across sessions.
//!
//! The file is one JSON string per line (`"AT+VER?"`), oldest first, so any text
//! survives, newlines and control characters included, and appending or trimming with
//! ordinary tools stays possible. [`History::load`] never fails: a missing file is an
//! empty history, and a line that is not a JSON string is skipped and reported in
//! [`History::warnings`]. [`History::save`] writes the whole file atomically through a
//! temp file and a rename.
//!
//! Pushing an entry equal to the newest one changes nothing, and an empty entry is
//! ignored. Only the newest [`MAX_ENTRIES`] are kept.

use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::commands::write_atomic;

/// The most entries kept; older ones are dropped as new ones arrive.
pub const MAX_ENTRIES: usize = 1000;

/// A bounded list of sent texts.
#[derive(Clone, Debug)]
pub struct History {
    /// Oldest first.
    entries: VecDeque<String>,
    path: Option<PathBuf>,
    warnings: Vec<String>,
}

impl History {
    /// An empty history that [`save`](Self::save) will not write anywhere, for a session
    /// without a config directory and for tests.
    pub fn in_memory() -> Self {
        Self {
            entries: VecDeque::new(),
            path: None,
            warnings: Vec::new(),
        }
    }

    /// Reads the history file at `path`, keeping the newest [`MAX_ENTRIES`] lines. A
    /// missing or unreadable file gives an empty history (an unreadable one is also
    /// logged and reported), and so does each line that is not a JSON string: it is
    /// skipped with a warning. Later saves go to `path`.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut history = Self {
            entries: VecDeque::new(),
            path: Some(path.clone()),
            warnings: Vec::new(),
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return history,
            Err(err) => {
                history.warn(format!(
                    "{}: cannot read the history: {err}",
                    path.display()
                ));
                return history;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<String>(line) {
                Ok(entry) => history.entries.push_back(entry),
                Err(err) => history.warn(format!(
                    "{}:{}: skipped a line that is not a JSON string: {err}",
                    path.display(),
                    index + 1
                )),
            }
        }
        while history.entries.len() > MAX_ENTRIES {
            history.entries.pop_front();
        }
        history
    }

    fn warn(&mut self, message: String) {
        tracing::warn!("{message}");
        self.warnings.push(message);
    }

    /// The file this history saves to.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Problems found while loading, such as lines that were skipped.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Adds `entry` as the newest. Returns whether it was added: an empty entry, and one
    /// equal to the newest, are not.
    pub fn push(&mut self, entry: impl Into<String>) -> bool {
        let entry = entry.into();
        if entry.is_empty() || self.entries.back() == Some(&entry) {
            return false;
        }
        self.entries.push_back(entry);
        while self.entries.len() > MAX_ENTRIES {
            self.entries.pop_front();
        }
        true
    }

    /// The entries, newest first.
    pub fn iter_newest_first(&self) -> impl DoubleEndedIterator<Item = &str> + '_ {
        self.entries.iter().rev().map(String::as_str)
    }

    /// The entries that start with `prefix`, newest first, each text once. An empty
    /// prefix lists them all.
    pub fn prefix_matches(&self, prefix: &str) -> Vec<&str> {
        let mut found: Vec<&str> = Vec::new();
        for entry in self.iter_newest_first() {
            if entry.starts_with(prefix) && !found.contains(&entry) {
                found.push(entry);
            }
        }
        found
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The `n`th newest entry, 0 being the newest.
    pub fn get_from_newest(&self, n: usize) -> Option<&str> {
        self.entries
            .len()
            .checked_sub(n + 1)
            .and_then(|i| self.entries.get(i))
            .map(String::as_str)
    }

    /// Forgets every entry. [`save`](Self::save) afterwards empties the file.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Writes the history to its file, creating the directory if needed. Atomic: a
    /// crash leaves the old file or the new one. A history without a path does nothing.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut text = String::new();
        for entry in &self.entries {
            // A string cannot fail to serialize, and JSON escapes every newline.
            text.push_str(&serde_json::to_string(entry).unwrap_or_else(|_| "\"\"".to_owned()));
            text.push('\n');
        }
        write_atomic(path, text.as_bytes())
    }
}

impl Default for History {
    fn default() -> Self {
        Self::in_memory()
    }
}
