//! The tabs open at quit, kept in `state.json` in the config directory so the next start
//! can reopen them (the `restore_session` setting).
//!
//! State, not settings: the app writes the file and nobody is expected to edit it, so
//! it is plain JSON, rewritten whole (to a temporary file, then renamed over the old
//! one) each time the workspace closes. It is only read and written for a configuration
//! loaded from its directory, as the compose history is.
//!
//! ```json
//! {
//!   "version": 1,
//!   "active": 1,
//!   "tabs": [
//!     { "port": "virtual:at",
//!       "serial": { "baud": 115200, "data_bits": "eight", "parity": "none",
//!                   "stop_bits": "one", "flow_control": "none" },
//!       "codec": null, "mode": "command" },
//!     { "port": "/dev/cu.usbserial-1420",
//!       "serial": { "baud": 921600, ... }, "codec": "airoha-race", "mode": "inline" }
//!   ],
//!   "docks": { "left_width": 280.0, "right_width": 400.0, "devices": true, "commands": true }
//! }
//! ```
//!
//! `docks` is the dock widths and which left panels are open (see
//! [`docks`](crate::docks)); a file without it leaves the docks at their defaults.
//!
//! A file that does not parse, or of another version, is ignored (and logged): the app
//! starts with no tabs, as it would without one.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, SerialConfig};

use crate::docks::SavedDocks;
use crate::inline::Mode;

/// The version this build writes and reads.
pub const STATE_VERSION: u32 = 1;

/// The file name in the config directory.
pub const STATE_FILE: &str = "state.json";

/// Where the state file lives for `paths`.
pub fn state_path(paths: &ConfigPaths) -> PathBuf {
    paths.dir.join(STATE_FILE)
}

/// A tab's input mode, as the file spells it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedMode {
    #[default]
    Command,
    Inline,
}

impl From<Mode> for SavedMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Command => SavedMode::Command,
            Mode::Inline => SavedMode::Inline,
        }
    }
}

impl From<SavedMode> for Mode {
    fn from(mode: SavedMode) -> Self {
        match mode {
            SavedMode::Command => Mode::Command,
            SavedMode::Inline => Mode::Inline,
        }
    }
}

/// One tab: its port and what its session was set to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    pub port: PortId,
    /// The line settings it was opened with: baud, framing, flow control.
    pub serial: SerialConfig,
    /// The codec decoding it, if any.
    #[serde(default)]
    pub codec: Option<String>,
    #[serde(default)]
    pub mode: SavedMode,
}

/// The tabs, in order, which was active, and the docks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub version: u32,
    /// Index into `tabs`.
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    /// The dock widths and the left panels open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docks: Option<SavedDocks>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            active: 0,
            tabs: Vec::new(),
            docks: None,
        }
    }
}

impl SessionState {
    /// The state saved at `path`, or `None` for a missing file, one that does not
    /// parse, or one of another version.
    pub fn load(path: &Path) -> Option<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "could not read the session state");
                return None;
            }
        };
        match serde_json::from_str::<SessionState>(&text) {
            Ok(state) if state.version == STATE_VERSION => Some(state),
            Ok(state) => {
                tracing::warn!(
                    path = %path.display(),
                    version = state.version,
                    "ignoring session state of another version"
                );
                None
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "ignoring a session state that does not parse");
                None
            }
        }
    }

    /// Write the state to `path`: a temporary file next to it, renamed over it. The
    /// directory must exist; the state is not worth creating a config directory for.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text + "\n")?;
        std::fs::rename(&temp, path)
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::Parity;

    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn state_round_trips_through_the_file() {
        let dir = TestDir::new("session-state");
        let path = state_path(&ConfigPaths::new(dir.path()));
        assert_eq!(path, dir.join("state.json"));
        assert_eq!(SessionState::load(&path), None, "no file, no state");

        let state = SessionState {
            version: STATE_VERSION,
            active: 1,
            tabs: vec![
                SavedTab {
                    port: PortId::new("virtual:at"),
                    serial: SerialConfig::default(),
                    codec: None,
                    mode: SavedMode::Command,
                },
                SavedTab {
                    port: PortId::new("/dev/cu.usbserial-1420"),
                    serial: SerialConfig {
                        baud: 921_600,
                        parity: Parity::Even,
                        ..SerialConfig::default()
                    },
                    codec: Some("airoha-race".into()),
                    mode: SavedMode::Inline,
                },
            ],
            docks: Some(SavedDocks {
                left_width: 312.,
                right_width: 450.,
                devices: true,
                commands: false,
            }),
        };
        state.save(&path).unwrap();
        assert_eq!(SessionState::load(&path), Some(state.clone()));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""port": "virtual:at""#), "{text}");
        assert!(text.contains(r#""mode": "inline""#), "{text}");
        assert!(text.contains(r#""left_width": 312.0"#), "{text}");
        assert!(!dir.join("state.json.tmp").exists());
    }

    #[test]
    fn a_bad_or_foreign_file_is_ignored() {
        let dir = TestDir::new("session-state-bad");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(SessionState::load(&path), None);
        std::fs::write(&path, r#"{ "version": 99, "tabs": [] }"#).unwrap();
        assert_eq!(SessionState::load(&path), None);
        // Missing optional keys take their defaults.
        std::fs::write(
            &path,
            r#"{ "version": 1, "tabs": [ { "port": "virtual:echo", "serial":
                { "baud": 9600, "data_bits": "eight", "parity": "none",
                  "stop_bits": "one", "flow_control": "none" } } ] }"#,
        )
        .unwrap();
        let state = SessionState::load(&path).unwrap();
        assert_eq!(state.active, 0);
        assert_eq!(state.tabs[0].mode, SavedMode::Command);
        assert_eq!(state.tabs[0].codec, None);
        assert_eq!(state.tabs[0].serial.baud, 9600);
        assert_eq!(state.docks, None);
    }

    #[test]
    fn saving_needs_the_directory() {
        let dir = TestDir::new("session-state-gone");
        let path = dir.join("missing").join("state.json");
        assert!(SessionState::default().save(&path).is_err());
        assert!(!dir.join("missing").exists(), "no directory is created");
    }
}
