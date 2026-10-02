//! Where time-driven code gets its time: the [`Clock`] trait, real time
//! ([`SystemClock`]) and the [`Wakeup`] a waiting thread is woken through.
//!
//! The trait lives in the core so that core transports with a schedule (the file-replay
//! transport paces a recorded capture) take an injectable clock, exactly as the
//! simulator's virtual link does. `serialist-sim` re-exports these three items and adds
//! `ManualClock`, a clock a test moves by hand, so timing assertions are exact on both.

use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Condvar, Mutex};

/// A source of time, and of timed waits against that time.
///
/// Every time a paced component keeps (a virtual link's release schedule, look-ahead
/// window, latency, jitter, read timeouts and device deadlines; a replayed capture's
/// chunk schedule) comes from its clock, and every blocking wait goes through
/// [`Clock::wait`]. [`SystemClock`] is real time. `serialist_sim::ManualClock` only moves
/// when a test moves it, which makes timing assertions exact and independent of how busy
/// the machine is.
///
/// # The wait protocol
///
/// A thread that has to wait for "some condition, or until a deadline" does this:
///
/// 1. Under the lock that guards the condition, check it, then read
///    [`Wakeup::epoch`] from the [`Wakeup`] that is notified when it changes.
/// 2. Release the lock and call [`Clock::wait`] with that epoch and the deadline.
/// 3. Retake the lock and check again. Waits can end early, so this is always a loop.
///
/// Whoever changes the condition does so under the same lock and calls
/// [`Wakeup::notify`] afterwards, which moves the epoch on. A wait that starts after the
/// change sees a newer epoch and returns at once, so no wake-up is ever lost. A thread
/// that waits for time alone uses a private `Wakeup` nobody notifies.
pub trait Clock: Send + Sync {
    /// The current time on this clock.
    fn now(&self) -> Instant;

    /// Block until `wakeup` is notified after `seen` was read from it, or until this
    /// clock reaches `deadline` (`None`: no deadline). See the trait docs for the
    /// protocol. May return early, so callers re-check their condition and
    /// [`Clock::now`] in a loop.
    fn wait(&self, wakeup: &Arc<Wakeup>, seen: u64, deadline: Option<Instant>);
}

/// Real time: [`Instant::now`] and condition-variable timeouts.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wait(&self, wakeup: &Arc<Wakeup>, seen: u64, deadline: Option<Instant>) {
        wakeup.block(seen, deadline);
    }
}

/// What a waiting thread is woken through: an epoch counter plus a condition variable.
/// The [`Clock`] docs describe how it is used with [`Clock::wait`].
#[derive(Debug, Default)]
pub struct Wakeup {
    epoch: Mutex<u64>,
    cv: Condvar,
}

impl Wakeup {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current epoch. Read it while holding the lock that guards the condition being
    /// waited for.
    pub fn epoch(&self) -> u64 {
        *self.epoch.lock()
    }

    /// Move the epoch on and wake every thread waiting on this `Wakeup`.
    pub fn notify(&self) {
        let mut epoch = self.epoch.lock();
        *epoch = epoch.wrapping_add(1);
        self.cv.notify_all();
    }

    /// Block until the epoch moves past `seen`, or until the real instant `until`.
    /// Returns whether the epoch moved. This is the blocking step a [`Clock`]
    /// implementation builds its `wait` on.
    pub fn block(&self, seen: u64, until: Option<Instant>) -> bool {
        let mut epoch = self.epoch.lock();
        while *epoch == seen {
            match until {
                Some(t) => {
                    if self.cv.wait_until(&mut epoch, t).timed_out() {
                        return *epoch != seen;
                    }
                }
                None => self.cv.wait(&mut epoch),
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn system_clock_waits_time_out_in_real_time() {
        let wakeup = Arc::new(Wakeup::new());
        let started = SystemClock.now();
        SystemClock.wait(&wakeup, wakeup.epoch(), Some(started + MS));
        assert!(SystemClock.now() >= started + MS);
        let seen = wakeup.epoch();
        wakeup.notify();
        SystemClock.wait(&wakeup, seen, None);
    }

    #[test]
    fn a_notify_wakes_a_blocked_thread() {
        let wakeup = Arc::new(Wakeup::new());
        let seen = wakeup.epoch();
        let waiter = {
            let wakeup = Arc::clone(&wakeup);
            std::thread::spawn(move || wakeup.block(seen, None))
        };
        wakeup.notify();
        assert!(waiter.join().unwrap(), "the epoch moved");
        assert!(
            !wakeup.block(wakeup.epoch(), Some(Instant::now() + MS)),
            "a timeout reports that nothing moved"
        );
    }
}
