//! Hot reload: watches the config directory and reports which kind of file changed.
//!
//! The watcher observes the config *directory*, not the files in it, so a settings file
//! created after the app started is noticed, and so is one an editor replaces by writing
//! a temp file and renaming it. Changes are debounced for 100 ms and coalesced: however
//! many files change in one window, each [`ConfigEvent`] is sent once per batch.
//!
//! Only files produce events. A directory event, including the creation of the config or
//! `themes/` directory that [`ConfigWatcher::spawn`] performs itself, is dropped: some
//! backends (FSEvents on macOS) deliver directory events late and folded into later
//! batches, and a reload for one would be spurious. A batch may still be split across
//! debounce windows, so one save can produce more than one event of a kind; treat an
//! event as "reload this", never as "exactly one change happened".
//!
//! The event says what to reload, not what changed; the receiver re-reads the file with
//! [`load_settings`](crate::load_settings), [`load_keymap`](crate::load_keymap) or
//! [`ThemeRegistry::load`](crate::ThemeRegistry::load).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{Receiver, SendTimeoutError, Sender, select, unbounded};
use notify::event::{CreateKind, RemoveKind};
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
    /// A `*.json` file under the `themes/` folder, at any depth.
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

    /// The kind of config file `path` names, if it names one. Directories never do,
    /// and neither does the `themes/` folder itself.
    fn classify(&self, path: &Path) -> Option<ConfigEvent> {
        let kind = if self.settings.iter().any(|settings| settings == path) {
            ConfigEvent::Settings
        } else if path == self.keymap {
            ConfigEvent::Keymap
        } else if path != self.themes
            && path.starts_with(&self.themes)
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            ConfigEvent::Themes
        } else {
            return None;
        };
        // A directory that happens to carry a config file's name is not one. A file
        // that was just deleted is not a directory either, so removals still count.
        (!path.is_dir()).then_some(kind)
    }

    /// The distinct events for a batch, in a fixed order.
    fn events(&self, batch: &[DebouncedEvent]) -> Vec<ConfigEvent> {
        let mut found = Vec::new();
        for event in batch {
            match event.kind {
                EventKind::Access(_)
                | EventKind::Create(CreateKind::Folder)
                | EventKind::Remove(RemoveKind::Folder) => continue,
                _ => {}
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
///
/// Two threads run behind it: the debouncer's, which only queues batches, and a
/// forwarding thread that classifies them and sends [`ConfigEvent`]s. The forwarding
/// thread owns the caller's sender.
pub struct ConfigWatcher {
    debouncer: Option<Debouncer<RecommendedWatcher, RecommendedCache>>,
    /// Dropping this wakes the forwarding thread and tells it to stop.
    stop: Option<Sender<()>>,
    forwarder: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl ConfigWatcher {
    /// Starts watching and sends a [`ConfigEvent`] on `tx` for each change.
    ///
    /// Watches the config directory non-recursively and `themes/` recursively, creating
    /// either if it is missing so files added later are noticed, plus the directory of
    /// the project settings file when `paths` has one. Creating those directories
    /// produces no event. If the OS watcher cannot be started the problem is logged and
    /// the returned watcher is inert ([`is_active`](Self::is_active) is false); the app
    /// still runs, just without hot reload.
    ///
    /// Dropping the watcher is synchronous: it stops the OS watcher, wakes and joins the
    /// forwarding thread, and so drops `tx`. No event is sent after `drop` returns, and
    /// the receiver sees the channel disconnect once it has read what was already sent.
    pub fn spawn(paths: &ConfigPaths, tx: Sender<ConfigEvent>) -> ConfigWatcher {
        match start(paths, tx) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!(%err, dir = %paths.dir.display(), "config hot reload is off");
                ConfigWatcher {
                    debouncer: None,
                    stop: None,
                    forwarder: None,
                    alive: Arc::new(AtomicBool::new(false)),
                }
            }
        }
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
            // Waits for the debouncer's thread, at most one tick (a quarter of the
            // debounce). After this nothing new reaches the forwarder.
            debouncer.stop();
        }
        // Wake the forwarder out of its wait, then wait for it. It owns the caller's
        // sender, so once it has gone no event can be sent.
        drop(self.stop.take());
        if let Some(forwarder) = self.forwarder.take()
            && forwarder.join().is_err()
        {
            tracing::warn!("the config watcher's forwarding thread panicked");
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

type Batch = Vec<DebouncedEvent>;

/// The forwarding thread: turns raw batches into events until told to stop.
fn forward(
    targets: &Targets,
    batches: &Receiver<Batch>,
    stop: &Receiver<()>,
    tx: &Sender<ConfigEvent>,
    alive: &AtomicBool,
) {
    loop {
        select! {
            // Nothing is ever sent on `stop`; it fires when the watcher drops its end.
            recv(stop) -> _ => return,
            recv(batches) -> batch => {
                let Ok(batch) = batch else { return };
                for event in targets.events(&batch) {
                    // Checked per event so nothing is sent once the watcher is dropped.
                    if !alive.load(Ordering::SeqCst) {
                        return;
                    }
                    match tx.send_timeout(event, SEND_TIMEOUT) {
                        Ok(()) => {}
                        Err(SendTimeoutError::Timeout(_)) => {
                            tracing::debug!(?event, "config event not delivered: channel full");
                        }
                        // Nobody is listening any more.
                        Err(SendTimeoutError::Disconnected(_)) => return,
                    }
                }
            }
        }
    }
}

fn start(paths: &ConfigPaths, tx: Sender<ConfigEvent>) -> Result<ConfigWatcher, notify::Error> {
    // The directories have to exist to be watched.
    for dir in [&paths.dir, &paths.themes] {
        if let Err(err) = std::fs::create_dir_all(dir) {
            tracing::warn!(%err, dir = %dir.display(), "cannot create the config directory");
        }
    }
    let targets = Targets::new(paths);

    let (batch_tx, batch_rx) = unbounded::<Batch>();
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            // The forwarder may already be gone, which is fine.
            Ok(batch) => drop(batch_tx.send(batch)),
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

    let alive = Arc::new(AtomicBool::new(true));
    let (stop_tx, stop_rx) = unbounded::<()>();
    let forwarder = {
        let alive = Arc::clone(&alive);
        std::thread::Builder::new()
            .name("serialist-config-watch".to_string())
            .spawn(move || forward(&targets, &batch_rx, &stop_rx, &tx, &alive))
            .map_err(notify::Error::io)?
    };
    Ok(ConfigWatcher {
        debouncer: Some(debouncer),
        stop: Some(stop_tx),
        forwarder: Some(forwarder),
        alive,
    })
}

#[cfg(test)]
mod tests;
