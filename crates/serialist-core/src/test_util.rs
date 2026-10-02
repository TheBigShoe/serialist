//! Helpers shared by the unit tests: a temp directory, and the wall-clock timing rule.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Whether a test holds a wall-clock threshold to account: not in a coverage build
/// (`cargo llvm-cov` passes `--cfg coverage`, and instrumented code runs several times
/// slower), and under `CI` only with `SERIALIST_TIMING_TESTS` set. On a loaded machine or a
/// shared runner a thread can wait seconds for a core, so a bound on how long something
/// takes is a target for a quiet machine. A test checks the rest of what it asserts on every
/// run and prints what it measured where this is false. The integration tests have the same
/// rule in `tests/timing/mod.rs`, which a unit test cannot reach.
pub(crate) fn wall_clock_timing_enabled() -> bool {
    !cfg!(coverage)
        && (std::env::var_os("CI").is_none()
            || std::env::var_os("SERIALIST_TIMING_TESTS").is_some())
}

/// A directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "serialist-test-{label}-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `relative` under the directory, creating parents.
    pub(crate) fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, text).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
