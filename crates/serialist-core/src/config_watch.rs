//! Hot reload: watches the config directory and reports which kind of file changed.
//!
//! The watcher observes the config *directory*, not the files in it, so a settings file
//! created after the app started is noticed, and so is one an editor replaces by writing
//! a temp file and renaming it. Changes are debounced for 100 ms and coalesced: however
//! many files change in one window, each [`ConfigEvent`] is sent once.
//!
//! The event says what to reload, not what changed; the receiver re-reads the file with
//! [`load_settings`](crate::load_settings), [`load_keymap`](crate::load_keymap) or
//! [`ThemeRegistry::load`](crate::ThemeRegistry::load).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossbeam_channel::Sender;
use notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, new_debouncer,
};

use crate::settings::ConfigPaths;

/// How long changes are gathered before they are reported.
pub const DEBOUNCE: Duration = Duration::from_millis(100);

/// How long a send waits on a full channel before giving up on that event.
const SEND_TIMEOUT: Duration = Duration::from_millis(100);

/// Which kind of configuration changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ConfigEvent {
    /// `settings.json` in the config directory, or the project settings file.
    Settings,
    /// `keymap.json`.
    Keymap,
    /// A `*.json` file in the `themes/` folder.
    Themes,
}

/// The files the watcher cares about, in the form the OS reports them.
#[derive(Clone, Debug)]
struct Targets {
    settings: Vec<PathBuf>,
    keymap: PathBuf,
    themes: PathBuf,
}

impl Targets {
    fn new(paths: &ConfigPaths) -> Self {
        let mut settings = vec![canonical(&paths.settings)];
        settings.extend(paths.project_settings.iter().map(|path| canonical(path)));
        Self {
            settings,
            keymap: canonical(&paths.keymap),
            themes: canonical(&paths.themes),
        }
    }

    fn classify(&self, path: &Path) -> Option<ConfigEvent> {
        if self.settings.iter().any(|settings| settings == path) {
            Some(ConfigEvent::Settings)
        } else if path == self.keymap {
            Some(ConfigEvent::Keymap)
        } else if path == self.themes
            // The folder itself, or a theme file in it.
            || (path.starts_with(&self.themes)
                && path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json")))
        {
            Some(ConfigEvent::Themes)
        } else {
            None
        }
    }

    /// The distinct events for a batch, in a fixed order.
    fn events(&self, batch: &[DebouncedEvent]) -> Vec<ConfigEvent> {
        let mut found = Vec::new();
        for event in batch {
            if matches!(event.kind, EventKind::Access(_)) {
                continue;
            }
            for path in &event.paths {
                if let Some(kind) = self.classify(path)
                    && !found.contains(&kind)
                {
                    found.push(kind);
                }
            }
        }
        found.sort_unstable();
        found
    }
}

/// `path` as the OS will report it: symlinks resolved, and on macOS `/var` spelled
/// `/private/var`. A path that does not exist yet is resolved through its parent.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map(|parent| parent.join(name))
            .unwrap_or_else(|_| path.to_path_buf()),
        _ => path.to_path_buf(),
    }
}

/// Watches the config directory until dropped.
pub struct ConfigWatcher {
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    alive: Arc<AtomicBool>,
}

impl ConfigWatcher {
    /// Starts watching and sends a [`ConfigEvent`] on `tx` for each change.
    ///
    /// Watches the config directory non-recursively and `themes/` recursively, creating
    /// either if it is missing so files added later are noticed, plus the directory of
    /// the project settings file when `paths` has one. If the OS watcher cannot be
    /// started the problem is logged and the returned watcher is inert
    /// ([`is_active`](Self::is_active) is false); the app still runs, just without hot
    /// reload.
    ///
    /// Dropping the watcher stops it: no event is sent after `drop` returns.
    pub fn spawn(paths: &ConfigPaths, tx: Sender<ConfigEvent>) -> ConfigWatcher {
        let alive = Arc::new(AtomicBool::new(true));
        let debouncer = match start(paths, tx, &alive) {
            Ok(debouncer) => Some(debouncer),
            Err(err) => {
                tracing::warn!(%err, dir = %paths.dir.display(), "config hot reload is off");
                None
            }
        };
        ConfigWatcher { debouncer, alive }
    }

    /// Whether the OS watcher is running.
    pub fn is_active(&self) -> bool {
        self.debouncer.is_some()
    }
}

impl Drop for ConfigWatcher {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(debouncer) = self.debouncer.take() {
            // Waits for the event thread, at most one tick (a quarter of the debounce).
            debouncer.stop();
        }
    }
}

impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("ConfigWatcher")
            .field("active", &self.is_active())
            .finish()
    }
}

fn start(
    paths: &ConfigPaths,
    tx: Sender<ConfigEvent>,
    alive: &Arc<AtomicBool>,
) -> Result<Debouncer<RecommendedWatcher, RecommendedCache>, notify::Error> {
    // The directories have to exist to be watched.
    for dir in [&paths.dir, &paths.themes] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            tracing::warn!(%err, dir = %dir.display(), "cannot create the config directory");
        }
    }
    let targets = Targets::new(paths);
    let handler_alive = Arc::clone(alive);
    let handler_targets = targets.clone();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            Ok(batch) => {
                for event in handler_targets.events(&batch) {
                    // Checked per event so nothing is sent once the watcher is dropped.
                    if !handler_alive.load(Ordering::SeqCst) {
                        return;
                    }
                    if tx.send_timeout(event, SEND_TIMEOUT).is_err() {
                        tracing::debug!(?event, "config event not delivered");
                    }
                }
            }
            Err(errors) => {
                for err in errors {
                    tracing::warn!(%err, "config watcher error");
                }
            }
        }
    })?;

    let config_dir = canonical(&paths.dir);
    debouncer.watch(&config_dir, RecursiveMode::NonRecursive)?;
    let themes_dir = canonical(&paths.themes);
    if themes_dir != config_dir
        && let Err(err) = debouncer.watch(&themes_dir, RecursiveMode::Recursive)
    {
        tracing::warn!(%err, dir = %themes_dir.display(), "cannot watch the themes folder");
    }
    // A project settings file usually lives outside the config directory.
    if let Some(project) = &paths.project_settings
        && let Some(parent) = project.parent().map(canonical)
        && parent != config_dir
        && let Err(err) = debouncer.watch(&parent, RecursiveMode::NonRecursive)
    {
        tracing::warn!(%err, dir = %parent.display(), "cannot watch the project settings");
    }
    Ok(debouncer)
}

#[cfg(test)]
mod tests;
