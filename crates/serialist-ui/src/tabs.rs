//! What a session tab is called and what its dot says, as plain data.
//!
//! The workspace holds a tab per open port (see [`workspace`](crate::workspace)); these
//! are the pieces of a tab that tests read and the tab bar draws: its id, which outlives
//! the session views that come and go in it, and its label.

use std::fmt;

use crate::status::format_bytes;

/// A tab's identity for as long as it is open. A tab keeps it across reconnects (a new
/// session view in the same tab) and moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TabId(pub u64);

impl fmt::Display for TabId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tab {}", self.0)
    }
}

/// What a tab's dot says about its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TabState {
    /// No port picked yet (a new tab).
    Empty,
    /// The port is being opened.
    Connecting,
    /// A port is picked (a restored tab whose device is not plugged in, or an open
    /// that failed) and nothing is open.
    NotConnected,
    /// Open and following the stream.
    Connected,
    /// Open, and the view is paused.
    Paused,
    /// Open, and received bytes are being recorded (whether paused or not).
    Recording,
    /// Closed by the user; the scrollback stays.
    Disconnected,
    /// The link went away (unplugged, or an error).
    Lost,
}

impl TabState {
    /// A word for the dot's tooltip and for tests.
    pub fn label(self) -> &'static str {
        match self {
            TabState::Empty => "no port",
            TabState::Connecting => "opening",
            TabState::NotConnected => "not connected",
            TabState::Connected => "connected",
            TabState::Paused => "paused",
            TabState::Recording => "recording",
            TabState::Disconnected => "disconnected",
            TabState::Lost => "connection lost",
        }
    }

    /// Whether a port is open behind the tab.
    pub fn is_open(self) -> bool {
        matches!(
            self,
            TabState::Connected | TabState::Paused | TabState::Recording
        )
    }
}

/// What a session view says about itself for its tab: the dot, and the bytes that
/// arrived while its tab was in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TabStatus {
    pub state: TabState,
    /// RX bytes since the tab was last the active one; 0 while it is.
    pub unseen_bytes: u64,
}

/// A tab as the tab bar draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabLabel {
    pub id: TabId,
    /// The device's display name (a profile's `name`, the transport's name) or the
    /// port id; `New tab` before a port is picked.
    pub title: String,
    pub state: TabState,
    /// `+1.2 KiB` received while in the background, if anything was.
    pub unseen: Option<String>,
    pub active: bool,
}

impl TabLabel {
    pub fn new(id: TabId, title: String, status: TabStatus, active: bool) -> Self {
        Self {
            id,
            title,
            state: status.state,
            unseen: (!active && status.unseen_bytes > 0)
                .then(|| format!("+{}", format_bytes(status.unseen_bytes))),
            active,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_background_tab_counts_what_it_has_not_shown() {
        let status = TabStatus {
            state: TabState::Connected,
            unseen_bytes: 1536,
        };
        let background = TabLabel::new(TabId(1), "AT modem".into(), status, false);
        assert_eq!(background.unseen.as_deref(), Some("+1.5 KiB"));
        let active = TabLabel::new(TabId(1), "AT modem".into(), status, true);
        assert_eq!(active.unseen, None, "the active tab shows everything");
        let quiet = TabLabel::new(
            TabId(2),
            "Echo".into(),
            TabStatus {
                state: TabState::Paused,
                unseen_bytes: 0,
            },
            false,
        );
        assert_eq!(quiet.unseen, None);
        assert_eq!(quiet.state.label(), "paused");
        assert!(quiet.state.is_open());
        assert!(!TabState::Lost.is_open());
    }
}
