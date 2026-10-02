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
//!   "version": 2,
//!   "active": 1,
//!   "tabs": [
//!     { "port": "virtual:at",
//!       "serial": { "baud": 115200, "data_bits": "eight", "parity": "none",
//!                   "stop_bits": "one", "flow_control": "none" },
//!       "codec": null, "mode": "command", "emulation": "monitor" },
//!     { "port": "/dev/cu.usbserial-1420",
//!       "serial": { "baud": 921600, ... }, "codec": "airoha-race", "mode": "inline",
//!       "emulation": "vt" }
//!   ],
//!   "docks": { "left_width": 280.0, "right_width": 400.0, "devices": true, "commands": true }
//! }
//! ```
//!
//! `docks` is the dock widths and which left panels are open (see
//! [`docks`](crate::docks)); a file without it leaves the docks at their defaults.
//!
//! `emulation` is how the tab showed the device: `"monitor"` or `"vt"`. Version 2 added it;
//! a version 1 file still loads, and its tabs take the emulation the settings name, as
//! they did before it was kept. A tab with no `emulation` does the same.
//!
//! A file that does not parse, or of a version this build does not know (none, or newer
//! than [`STATE_VERSION`]), is ignored (and logged): the app starts with no tabs, as it
//! would without one.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serialist_core::settings::ConfigPaths;
use serialist_core::{Emulation, PortId, SerialConfig};

use crate::docks::SavedDocks;
use crate::inline::Mode;

/// The version this build writes. It also reads version 1, which had no `emulation`.
pub const STATE_VERSION: u32 = 2;

/// The oldest version this build still reads.
const OLDEST_VERSION: u32 = 1;

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
    /// Monitor or VT, as the tab showed it. `None` (a version 1 file, or a tab that
    /// never opened) leaves it to the settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulation: Option<Emulation>,
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
    /// parse, or one of a version this build does not read.
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
            Ok(state) if (OLDEST_VERSION..=STATE_VERSION).contains(&state.version) => Some(state),
            Ok(state) => {
                tracing::warn!(
                    path = %path.display(),
                    version = state.version,
                    "ignoring session state of a version this build does not read"
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
                    emulation: Some(Emulation::Monitor),
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
                    emulation: Some(Emulation::Vt),
                },
                SavedTab {
                    // The top of the standard list, an FT232H at full speed.
                    port: PortId::new("/dev/cu.usbserial-FT232H"),
                    serial: SerialConfig {
                        baud: 12_000_000,
                        ..SerialConfig::default()
                    },
                    codec: None,
                    mode: SavedMode::Command,
                    emulation: Some(Emulation::Monitor),
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
        assert!(text.contains(r#""version": 2"#), "{text}");
        assert!(text.contains(r#""emulation": "vt""#), "{text}");
        assert!(text.contains(r#""emulation": "monitor""#), "{text}");
        assert!(text.contains(r#""left_width": 312.0"#), "{text}");
        assert!(text.contains(r#""baud": 12000000"#), "{text}");
        assert!(!dir.join("state.json.tmp").exists());
    }

    #[test]
    fn a_bad_or_foreign_file_is_ignored() {
        let dir = TestDir::new("session-state-bad");
        let path = dir.join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(SessionState::load(&path), None);
        for version in [0, STATE_VERSION + 1, 99] {
            std::fs::write(&path, format!(r#"{{ "version": {version}, "tabs": [] }}"#)).unwrap();
            assert_eq!(SessionState::load(&path), None, "version {version}");
        }
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
        assert_eq!(state.tabs[0].emulation, None);
        assert_eq!(state.tabs[0].serial.baud, 9600);
        assert_eq!(state.docks, None);
    }

    #[test]
    fn a_version_1_file_still_loads_without_an_emulation() {
        let dir = TestDir::new("session-state-v1");
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "active": 1,
  "tabs": [
    { "port": "virtual:at",
      "serial": { "baud": 115200, "data_bits": "eight", "parity": "none",
                  "stop_bits": "one", "flow_control": "none" },
      "codec": null, "mode": "inline" },
    { "port": "virtual:echo",
      "serial": { "baud": 9600, "data_bits": "eight", "parity": "none",
                  "stop_bits": "one", "flow_control": "none" },
      "codec": "airoha-race", "mode": "command" }
  ],
  "docks": { "left_width": 280.0, "right_width": 400.0, "devices": true, "commands": true }
}"#,
        )
        .unwrap();
        let state = SessionState::load(&path).expect("version 1 is read");
        assert_eq!(state.version, 1);
        assert_eq!(state.active, 1);
        assert_eq!(state.tabs.len(), 2);
        assert_eq!(state.tabs[0].mode, SavedMode::Inline);
        assert_eq!(state.tabs[1].codec.as_deref(), Some("airoha-race"));
        assert!(state.tabs.iter().all(|tab| tab.emulation.is_none()));
        assert!(state.docks.is_some());
    }

    #[test]
    fn saving_needs_the_directory() {
        let dir = TestDir::new("session-state-gone");
        let path = dir.join("missing").join("state.json");
        assert!(SessionState::default().save(&path).is_err());
        assert!(!dir.join("missing").exists(), "no directory is created");
    }
}
