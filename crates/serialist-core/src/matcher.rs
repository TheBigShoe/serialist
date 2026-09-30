//! Line matchers: wait for a pattern in what the device sends.
//!
//! A saved command's `expect`, and later a script's `port:expect`, both come down to
//! "tell me when a line matching this regex arrives, or that it did not within a
//! time". [`IngestHandle::matchers`](crate::IngestHandle::matchers) returns a
//! [`MatcherHandle`], which is `Clone + Send + Sync`; [`MatcherHandle::expect`] registers
//! an expectation and returns an [`Expectation`] to wait on from any thread.
//!
//! ```no_run
//! # use std::time::Duration;
//! # fn demo(matchers: serialist_core::MatcherHandle, session: &serialist_core::Session) {
//! // Register first, then send, so the reply cannot arrive before anyone is listening.
//! let expectation = matchers.expect("^OK|^ERROR", Duration::from_secs(1)).unwrap();
//! session.write(b"AT\r\n".to_vec()).unwrap();
//! match expectation.wait() {
//!     serialist_core::ExpectResult::Matched { text, .. } => println!("device said {text}"),
//!     serialist_core::ExpectResult::TimedOut { elapsed } => println!("nothing in {elapsed:?}"),
//!     serialist_core::ExpectResult::Closed => println!("the session ended"),
//! }
//! # }
//! ```
//!
//! # What counts
//!
//! - **Received lines only.** The ingest thread hands the matchers each line the parser
//!   finishes from received bytes, after it is stored and published. Sent-command echoes
//!   and notices (`Tx` and `Notice` lines) are never matched, so `expect("AT")` is not
//!   satisfied by your own echo. A line still waiting for its LF is not matched until it
//!   ends; a line the parser ends early at its length cap is. A received line that a
//!   local line interrupts is committed unmatched.
//! - **Only lines appended after registration.** An expectation starts at the line the
//!   ingest thread will store next; everything already stored, including a line that had
//!   begun but not ended, is never matched. This is why the order is register, then send.
//!   The boundary is exact: a line is handed to the matchers either before an
//!   expectation registers, and then it does not count, or after, and then it does.
//! - **Smart case**, as in scrollback search: a pattern with no uppercase letters
//!   matches either case; see [`crate::store::smart_case_insensitive`]. The pattern is
//!   searched anywhere in the line's text (anchor it with `^` and `$` to match the whole
//!   line), and a line can satisfy any number of expectations at once.
//! - **Captures**: [`ExpectResult::Matched::captures`] is the regex's capture list, group
//!   0 (the whole match) first, `None` for a group that did not take part.
//!
//! # Timing
//!
//! The ingest thread does all the resolving, so nothing needs a timer of its own. It
//! checks deadlines after every chunk and local line, and on its idle tick
//! ([`IDLE_INTERVAL`](crate::ingest::IDLE_INTERVAL), 250 ms) when nothing arrives, so a
//! timeout resolves within about one idle interval after its deadline, never before. A
//! chunk received after the deadline does not count even if its line would have matched.
//! `elapsed` on a match is measured to when the chunk was received, not to when the
//! ingest thread got to it.
//!
//! # Ending
//!
//! When the session disconnects (an unplug, or `close`) or the ingest thread ends, every
//! pending expectation resolves [`ExpectResult::Closed`], and so does any registered
//! later: no more lines can arrive from that session. After a reconnect (a new session
//! and a new ingest thread) take the new [`IngestHandle::matchers`](crate::IngestHandle::matchers).
//! Dropping every clone of an [`Expectation`] abandons it; the ingest thread forgets it
//! at its next check.
//!
//! Future work: `MatcherHandle::expect_frame`, a predicate over decoded frames, arrives
//! with the codecs of milestone 5. Matching a prompt with no line ending (`login: `)
//! needs an option to test the line in progress; it is not here yet.

use std::fmt;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use regex::{Regex, RegexBuilder};

use crate::store::{AppendReport, Store, smart_case_insensitive};
use crate::text::{Direction, LineId, LineSource};

/// Compiles `pattern` the way matchers and saved commands' `expect` do: smart case, so a
/// pattern with no uppercase letter ignores case.
pub fn compile_pattern(pattern: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(smart_case_insensitive(pattern))
        .build()
}

/// How an [`Expectation`] ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExpectResult {
    /// A line matched.
    Matched {
        /// The line's id in the scrollback, for highlighting it.
        line: LineId,
        /// The whole line's text.
        text: String,
        /// The regex's captures, whole match first.
        captures: Vec<Option<String>>,
        /// From registering the expectation to the chunk holding the line arriving.
        elapsed: Duration,
    },
    /// The deadline passed with no match. `elapsed` is when the ingest thread noticed,
    /// which is up to one idle interval past the timeout.
    TimedOut { elapsed: Duration },
    /// It can no longer resolve: the session disconnected or its ingest thread ended,
    /// or [`Expectation::cancel`] was called.
    Closed,
}

/// The place a result lands and a waiter sleeps.
#[derive(Default)]
struct Slot {
    result: Mutex<Option<ExpectResult>>,
    done: Condvar,
}

impl Slot {
    /// Sets the result unless there is one already.
    fn resolve(&self, result: ExpectResult) {
        let mut slot = self.result.lock();
        if slot.is_none() {
            *slot = Some(result);
            self.done.notify_all();
        }
    }
}

struct Entry {
    id: u64,
    regex: Regex,
    /// The first line that counts.
    start: LineId,
    registered: Instant,
    /// `None` if the timeout is too long to represent: never.
    deadline: Option<Instant>,
    slot: Weak<Slot>,
}

struct Registry {
    closed: bool,
    /// One past the last line the ingest thread has handed over (or stored, for local
    /// lines): where a new expectation starts.
    end: LineId,
    next_id: u64,
    entries: Vec<Entry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            closed: false,
            end: LineId::ZERO,
            next_id: 0,
            entries: Vec::new(),
        }
    }
}

impl Registry {
    /// Resolves every entry whose deadline has passed at `now`, and forgets the ones
    /// nobody waits on.
    fn expire(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            let Some(slot) = entry.slot.upgrade() else {
                return false;
            };
            if entry.deadline.is_some_and(|deadline| now >= deadline) {
                slot.resolve(ExpectResult::TimedOut {
                    elapsed: now.saturating_duration_since(entry.registered),
                });
                return false;
            }
            true
        });
    }
}

#[derive(Default)]
struct Core {
    registry: Mutex<Registry>,
}

/// Registers expectations with a session's ingest thread. Cheap to clone and send;
/// every clone shares one registry.
#[derive(Clone, Default)]
pub struct MatcherHandle {
    core: Arc<Core>,
}

impl fmt::Debug for MatcherHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MatcherHandle")
            .field("pending", &self.pending())
            .finish()
    }
}

impl MatcherHandle {
    /// Waits for a received line matching `pattern` (smart-case regex) for up to
    /// `timeout` from now. Only lines that start after this call count; see the
    /// [module docs](self). Any number of expectations can be pending, each seeing every
    /// line. An invalid pattern is the regex crate's error.
    ///
    /// On a session that has already ended the expectation is returned already resolved
    /// as [`ExpectResult::Closed`].
    pub fn expect(&self, pattern: &str, timeout: Duration) -> Result<Expectation, regex::Error> {
        let regex = compile_pattern(pattern)?;
        let slot = Arc::new(Slot::default());
        let id = {
            let mut registry = self.core.registry.lock();
            let id = registry.next_id;
            registry.next_id += 1;
            if registry.closed {
                slot.resolve(ExpectResult::Closed);
            } else {
                let registered = Instant::now();
                let start = registry.end;
                registry.entries.push(Entry {
                    id,
                    regex,
                    start,
                    registered,
                    deadline: registered.checked_add(timeout),
                    slot: Arc::downgrade(&slot),
                });
            }
            id
        };
        Ok(Expectation {
            id,
            pattern: pattern.into(),
            timeout,
            slot,
            handle: self.clone(),
        })
    }

    /// How many expectations are waiting to resolve.
    pub fn pending(&self) -> usize {
        self.core
            .registry
            .lock()
            .entries
            .iter()
            .filter(|entry| entry.slot.strong_count() > 0)
            .count()
    }

    // ---- The ingest thread's side ----

    /// A chunk that arrived at `at` was appended to `store` with this `report`: hand the
    /// lines it finished to the pending expectations, then expire the overdue ones.
    pub(crate) fn on_append(&self, store: &Store, report: &AppendReport, at: Instant) {
        let mut registry = self.core.registry.lock();
        // The line in progress is the newest one and is not handed over until it ends.
        let ended = if report.incomplete {
            LineId(report.changed.end.0.saturating_sub(1))
        } else {
            report.changed.end
        };
        if !registry.entries.is_empty() && report.changed.start < ended {
            let snapshot = store.snapshot();
            let mut lines = Vec::new();
            snapshot.lines(report.changed.start..ended, &mut lines);
            for line in lines.iter().filter(|line| line.direction == Direction::Rx) {
                registry.entries.retain(|entry| {
                    if line.id < entry.start {
                        return true;
                    }
                    let Some(slot) = entry.slot.upgrade() else {
                        return false;
                    };
                    if entry.deadline.is_some_and(|deadline| at > deadline) {
                        // Too late to count. Reported as a timeout at the next expiry.
                        return true;
                    }
                    if !entry.regex.is_match(&line.text) {
                        return true;
                    }
                    let captures = entry
                        .regex
                        .captures(&line.text)
                        .map(|caps| {
                            caps.iter()
                                .map(|group| group.map(|m| m.as_str().to_owned()))
                                .collect()
                        })
                        .unwrap_or_default();
                    slot.resolve(ExpectResult::Matched {
                        line: line.id,
                        text: line.text.clone(),
                        captures,
                        elapsed: at.saturating_duration_since(entry.registered),
                    });
                    false
                });
            }
        }
        registry.end = registry.end.max(report.changed.end);
        registry.expire(Instant::now());
    }

    /// Local lines (an echo, a notice) were stored: expectations registered from now on
    /// start after them. `end` is the store's end.
    pub(crate) fn advance(&self, end: LineId, now: Instant) {
        let mut registry = self.core.registry.lock();
        registry.end = registry.end.max(end);
        registry.expire(now);
    }

    /// Nothing arrived for a while: expire what is overdue.
    pub(crate) fn idle(&self, now: Instant) {
        self.core.registry.lock().expire(now);
    }

    /// No more lines will come: resolve everything [`ExpectResult::Closed`], now and for
    /// expectations registered later.
    pub(crate) fn close(&self) {
        let mut registry = self.core.registry.lock();
        registry.closed = true;
        for entry in registry.entries.drain(..) {
            if let Some(slot) = entry.slot.upgrade() {
                slot.resolve(ExpectResult::Closed);
            }
        }
    }
}

/// A registered wait for a line. Clone it to wait in one place and cancel from another;
/// every clone sees the same result, and asking again returns the same result again.
#[derive(Clone)]
pub struct Expectation {
    id: u64,
    pattern: Arc<str>,
    timeout: Duration,
    slot: Arc<Slot>,
    handle: MatcherHandle,
}

impl fmt::Debug for Expectation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expectation")
            .field("pattern", &self.pattern)
            .field("timeout", &self.timeout)
            .field("result", &self.try_wait())
            .finish()
    }
}

impl Expectation {
    /// The pattern as given.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The timeout as given.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Blocks until the expectation resolves: at most about the timeout plus one idle
    /// interval, or sooner on a match, a disconnect or [`cancel`](Self::cancel). Do not
    /// call it on a UI thread.
    pub fn wait(&self) -> ExpectResult {
        let mut result = self.slot.result.lock();
        loop {
            if let Some(result) = &*result {
                return result.clone();
            }
            self.slot.done.wait(&mut result);
        }
    }

    /// [`wait`](Self::wait) for at most `limit`: `None` if it has not resolved by then.
    /// The expectation stays registered.
    pub fn wait_timeout(&self, limit: Duration) -> Option<ExpectResult> {
        let deadline = Instant::now().checked_add(limit);
        let mut result = self.slot.result.lock();
        loop {
            if let Some(result) = &*result {
                return Some(result.clone());
            }
            match deadline {
                Some(deadline) => {
                    if self.slot.done.wait_until(&mut result, deadline).timed_out() {
                        return result.clone();
                    }
                }
                None => self.slot.done.wait(&mut result),
            }
        }
    }

    /// The result if there is one yet. Never blocks.
    pub fn try_wait(&self) -> Option<ExpectResult> {
        self.slot.result.lock().clone()
    }

    /// Gives up. A thread blocked in [`wait`](Self::wait) returns
    /// [`ExpectResult::Closed`]. Does nothing if the expectation has already resolved.
    pub fn cancel(&self) {
        self.handle
            .core
            .registry
            .lock()
            .entries
            .retain(|entry| entry.id != self.id);
        self.slot.resolve(ExpectResult::Closed);
    }
}

#[cfg(test)]
mod tests;
