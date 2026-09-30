//! The compose bar's history, kept across sessions.
//!
//! One [`PersistentHistory`] per workspace wraps the core [`History`] loaded from
//! `history.jsonl` in the config directory. Every compose bar is seeded from it when its
//! session opens, and every line sent is pushed to it. Saving is debounced: a push
//! schedules a write [`SAVE_DELAY`] later, and pushes in the meantime move it, so a
//! burst of sends costs one write. The write runs on the background executor from a
//! copy of the entries, atomically (the core writes a temp file and renames it). A
//! pending write is flushed when the workspace goes away.
//!
//! Without a loaded configuration (the bundled defaults only, as in most tests) the
//! history lives in memory and nothing is written.

use std::path::PathBuf;
use std::time::Duration;

use serialist_core::History;

use crate::prelude::*;

/// How long after the last push the history is written.
pub const SAVE_DELAY: Duration = Duration::from_millis(500);

pub struct PersistentHistory {
    history: History,
    /// The debounced write, replaced by each push.
    pending: Option<Task<()>>,
    /// Writes finished, for tests.
    saves: u64,
}

impl PersistentHistory {
    /// The history in the file at `path`, or an in-memory one for `None`. A missing file
    /// is an empty history; lines that do not parse are skipped and logged.
    pub fn load(path: Option<PathBuf>) -> Self {
        let history = match path {
            Some(path) => {
                let history = History::load(path);
                for warning in history.warnings() {
                    tracing::warn!("compose history: {warning}");
                }
                history
            }
            None => History::in_memory(),
        };
        Self {
            history,
            pending: None,
            saves: 0,
        }
    }

    /// The entries, oldest first, as a compose bar walks them.
    pub fn entries_oldest_first(&self) -> Vec<String> {
        self.history
            .iter_newest_first()
            .rev()
            .map(str::to_owned)
            .collect()
    }

    /// The newest entry.
    pub fn newest(&self) -> Option<&str> {
        self.history.get_from_newest(0)
    }

    pub fn len(&self) -> usize {
        self.history.len()
    }

    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }

    /// Writes finished so far.
    pub fn saves(&self) -> u64 {
        self.saves
    }

    /// Whether a write is scheduled.
    pub fn is_saving(&self) -> bool {
        self.pending.is_some()
    }

    /// Record a sent line and schedule a write. An empty line, or a repeat of the newest
    /// entry, changes nothing.
    pub fn push(&mut self, line: &str, cx: &mut Context<Self>) {
        if !self.history.push(line) || self.history.path().is_none() {
            return;
        }
        self.pending = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            let Ok(snapshot) = this.update(cx, |history, _| history.history.clone()) else {
                return;
            };
            let saved = cx.background_spawn(async move { snapshot.save() }).await;
            if let Err(error) = &saved {
                tracing::warn!(%error, "could not save the compose history");
            }
            this.update(cx, |history, _| {
                history.pending = None;
                history.saves += 1;
            })
            .ok();
        }));
        cx.notify();
    }

    /// Write now if a write is pending, off the main thread. For when the workspace
    /// closes.
    pub fn flush(&mut self, cx: &mut App) {
        if self.pending.take().is_some() {
            let snapshot = self.history.clone();
            cx.background_spawn(async move {
                if let Err(error) = snapshot.save() {
                    tracing::warn!(%error, "could not save the compose history");
                }
            })
            .detach();
        }
    }
}
