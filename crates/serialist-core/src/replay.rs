//! The file-replay transport: `replay:<path>` ids play a recorded capture (see
//! [`crate::capture`]) back into a session as if a device were sending it.
//!
//! **Status: contract only.** The factory's public API is final, so the app and its
//! settings can be wired to it, but [`TransportFactory::open`] parses the id and then
//! fails with [`TransportError::Unsupported`]`(`[`REPLAY_NOT_BUILT`]`)`, never
//! `NotFound`, until the reader lands. What follows is the behaviour it must have.
//!
//! # Pacing
//!
//! The options in effect are the id's own (`?speed=…&end=…`), else the factory's
//! [`defaults`](ReplayTransportFactory::defaults) (the app's settings), else
//! [`ReplayOptions::default`] (1x, disconnect at the end).
//!
//! - **With a timing sidecar** the reader follows [`Timing::schedule`](crate::capture::Timing::schedule):
//!   chunk `i` is due `speed.scale(chunk.at)` after the open, measured on the factory's
//!   [`Clock`]. Each read returns at most one recorded chunk (split only when it is
//!   larger than the read buffer, the rest due at once), so chunk boundaries replay
//!   exactly. A reader that falls behind gets the overdue chunks back to back; the
//!   schedule is anchored to the open, so lateness never accumulates.
//! - **Without a sidecar** bytes go out at the session's baud rate times the speed
//!   factor (`SerialConfig::bytes_per_second`), in chunks of what that rate delivers in
//!   one read timeout (at least one byte). A `reconfigure` with a new baud changes the
//!   rate from the next read on.
//! - **`speed=max`** ignores time either way: every read returns the next chunk (with
//!   a sidecar) or a full buffer (without) at once.
//! - A read waits on the clock for at most its timeout and returns `Ok(0)` if nothing is
//!   due by then, so the session's stop flag is polled as usual and nothing spins.
//!
//! # End of the capture
//!
//! [`ReplayEnd::Disconnect`]: the read after the last byte fails with
//! [`TransportError::Disconnected`]. [`ReplayEnd::Hold`]: reads keep returning `Ok(0)`
//! after waiting their timeout, until the session closes.
//!
//! # Everything else
//!
//! - Writes are accepted and discarded (a capture has nobody to answer), so typing or
//!   a script's `port:write` against a replay is harmless.
//! - DTR, RTS and reconfigure are accepted (reconfigure only matters without a
//!   sidecar, above); break is `Unsupported`.
//! - Open fails with `NotFound` if the raw file does not exist, `Config` for a
//!   malformed id or a sidecar that does not parse, and `Io` otherwise. A missing
//!   sidecar is not an error.
//! - Reconnect opens the capture again and plays it from the start.
//! - The description is `replay:<file name> (<speed>[, no timing])`, for example
//!   `replay:boot.bin (1x)` or `replay:dump.bin (4x, no timing)`.

use std::sync::Arc;

use parking_lot::Mutex;

use crate::address::{ReplayAddress, ReplayEnd, ReplaySpeed};
use crate::clock::{Clock, SystemClock};
use crate::config::SerialConfig;
use crate::port::PortId;
use crate::transport::{Transport, TransportError, TransportFactory};

/// What `open` fails with until the replay reader is built.
pub const REPLAY_NOT_BUILT: &str = "file replay (not built yet)";

/// How a capture plays back. See the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReplayOptions {
    pub speed: ReplaySpeed,
    pub end: ReplayEnd,
}

/// Opens `replay:<path>` ids.
///
/// The defaults are shared by every open and can change at any time (the app updates
/// them when the settings change); a replay already open keeps the options it opened
/// with.
pub struct ReplayTransportFactory {
    clock: Arc<dyn Clock>,
    defaults: Mutex<ReplayOptions>,
}

impl ReplayTransportFactory {
    /// Paced in real time.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock))
    }

    /// Paced on `clock`; tests pass a `serialist_sim::ManualClock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            defaults: Mutex::new(ReplayOptions::default()),
        }
    }

    /// The clock replays are paced on.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// The options an id without its own get.
    pub fn defaults(&self) -> ReplayOptions {
        *self.defaults.lock()
    }

    pub fn set_defaults(&self, options: ReplayOptions) {
        *self.defaults.lock() = options;
    }

    /// The options a replay of `address` opens with: its own, else the defaults.
    pub fn options_for(&self, address: &ReplayAddress) -> ReplayOptions {
        let defaults = self.defaults();
        ReplayOptions {
            speed: address.speed.unwrap_or(defaults.speed),
            end: address.end.unwrap_or(defaults.end),
        }
    }
}

impl Default for ReplayTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ReplayTransportFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplayTransportFactory")
            .field("defaults", &self.defaults())
            .finish_non_exhaustive()
    }
}

impl TransportFactory for ReplayTransportFactory {
    fn open(&self, port: &PortId, _config: &SerialConfig) -> Result<Transport, TransportError> {
        let address = ReplayAddress::from_port_id(port)
            .map_err(|err| TransportError::Config(err.to_string()))?;
        let options = self.options_for(&address);
        tracing::debug!(%port, ?options, "replay requested, but the replay reader is not built");
        Err(TransportError::Unsupported(REPLAY_NOT_BUILT))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_come_from_the_id_then_the_defaults() {
        let factory = ReplayTransportFactory::new();
        let bare = ReplayAddress::new("/c/boot.bin");
        assert_eq!(factory.options_for(&bare), ReplayOptions::default());

        factory.set_defaults(ReplayOptions {
            speed: ReplaySpeed::Max,
            end: ReplayEnd::Hold,
        });
        assert_eq!(factory.options_for(&bare).speed, ReplaySpeed::Max);
        let own = bare.with_speed(ReplaySpeed::Times(2.0));
        assert_eq!(
            factory.options_for(&own),
            ReplayOptions {
                speed: ReplaySpeed::Times(2.0),
                end: ReplayEnd::Hold
            }
        );
    }

    #[test]
    fn the_stub_says_it_is_not_built_and_still_checks_the_id() {
        let factory = ReplayTransportFactory::new();
        let open = |id: &str| {
            factory
                .open(&PortId::new(id), &SerialConfig::default())
                .err()
                .expect("not built")
        };
        assert!(matches!(
            open("replay:/c/boot.bin"),
            TransportError::Unsupported(REPLAY_NOT_BUILT)
        ));
        assert!(matches!(
            open("replay:/c/boot.bin?speed=fast"),
            TransportError::Config(_)
        ));
    }
}
