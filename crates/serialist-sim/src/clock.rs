//! Where a virtual link gets its time.
//!
//! Every time the link keeps (its release schedule, look-ahead window, latency, jitter,
//! read timeouts, device tick deadlines and unplug timing) comes from a [`Clock`], and
//! every blocking wait in the link goes through [`Clock::wait`]. [`SystemClock`] is real
//! time. [`ManualClock`] only moves when a test moves it, which makes timing assertions
//! exact and independent of how busy the machine is.
//!
//! # The wait protocol
//!
//! A thread that has to wait for "some condition, or until a deadline" does this:
//!
//! 1. Under the lock that guards the condition, check it, then read
//!    [`Wakeup::epoch`] from the [`Wakeup`] that is notified when it changes.
//! 2. Release the lock and call [`Clock::wait`] with that epoch and the deadline.
//! 3. Retake the lock and check again. Waits can end early, so this is always a loop.
//!
//! Whoever changes the condition does so under the same lock and calls
//! [`Wakeup::notify`] afterwards, which moves the epoch on. A wait that starts after the
//! change sees a newer epoch and returns at once, so no wake-up is ever lost.
//!
//! # Driving a manual clock from a test
//!
//! Time on a [`ManualClock`] stands still until the test calls [`ManualClock::advance`]
//! or [`ManualClock::set`], which wake every thread waiting on the clock; each re-checks
//! its own deadline against the new time and either proceeds or goes back to sleep.
//! Those threads then run in real time, so before asserting anything the test calls
//! [`ManualClock::settle`] with the number of threads it expects to be waiting (for a
//! link: its device thread, plus any thread blocked in the host's `read`). When `settle`
//! returns, every one of them has seen the new time and has nothing left to do until the
//! clock moves again or something notifies it.
//!
//! A read on a manual clock blocks until the clock passes its timeout, so the thread
//! that moves the clock must not be the one blocked in `read`. Either read with a zero
//! timeout (a non-blocking probe), or read on a helper thread and advance the clock from
//! the test thread.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};

use crate::link::later;

/// How long [`ManualClock::settle`] waits, in real time, before it fails the test.
const SETTLE_LIMIT: Duration = Duration::from_secs(10);

/// A source of time, and of timed waits against that time.
pub trait Clock: Send + Sync {
    /// The current time on this clock.
    fn now(&self) -> Instant;

    /// Block until `wakeup` is notified after `seen` was read from it, or until this
    /// clock reaches `deadline` (`None`: no deadline). See the module docs for the
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
/// See the module docs for how it is used with [`Clock::wait`].
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

/// A clock that only moves when told to. Compiled into the crate (not just its tests)
/// so integration tests and other crates' tests can drive links with it.
///
/// Waits on it never time out by themselves: [`ManualClock::advance`] and
/// [`ManualClock::set`] wake every waiting thread, and each one re-checks its deadline
/// against the new time. Share it as `Arc<ManualClock>`, which coerces to the
/// `Arc<dyn Clock>` that [`VirtualLink::connect_with_clock`](crate::VirtualLink::connect_with_clock)
/// and [`SimWorld::with_clock`](crate::SimWorld::with_clock) take.
pub struct ManualClock {
    state: Mutex<ManualState>,
    /// Notified whenever a thread starts or stops waiting, for `settle`.
    changed: Condvar,
}

struct ManualState {
    now: Instant,
    next_id: u64,
    waiters: BTreeMap<u64, Waiter>,
}

struct Waiter {
    wakeup: Arc<Wakeup>,
    seen: u64,
    deadline: Option<Instant>,
}

impl ManualState {
    /// Waiting threads that have not been woken and whose deadline is still ahead.
    fn asleep(&self) -> usize {
        self.waiters
            .values()
            .filter(|w| w.wakeup.epoch() == w.seen && w.deadline.is_none_or(|d| d > self.now))
            .count()
    }
}

impl ManualClock {
    /// A clock stopped at the current real time. Only the origin comes from real time;
    /// after this the clock moves only when told to.
    pub fn new() -> Self {
        Self::starting_at(SystemClock.now())
    }

    /// A clock stopped at `start`.
    pub fn starting_at(start: Instant) -> Self {
        Self {
            state: Mutex::new(ManualState {
                now: start,
                next_id: 0,
                waiters: BTreeMap::new(),
            }),
            changed: Condvar::new(),
        }
    }

    /// Move time forward by `by` and wake every waiting thread. Saturates far in the
    /// future rather than overflowing.
    pub fn advance(&self, by: Duration) {
        self.move_to(|now| later(now, by));
    }

    /// Move time to `to` and wake every waiting thread.
    ///
    /// # Panics
    ///
    /// If `to` is earlier than the current time: time on a link never goes backwards.
    pub fn set(&self, to: Instant) {
        self.move_to(|now| {
            assert!(
                to >= now,
                "ManualClock::set would move time backwards by {:?}",
                now - to
            );
            to
        });
    }

    fn move_to(&self, to: impl FnOnce(Instant) -> Instant) {
        let wakeups: Vec<Arc<Wakeup>> = {
            let mut st = self.state.lock();
            st.now = to(st.now);
            st.waiters.values().map(|w| Arc::clone(&w.wakeup)).collect()
        };
        for wakeup in wakeups {
            wakeup.notify();
        }
    }

    /// Threads waiting on this clock with nothing to do until it moves or something
    /// notifies them.
    pub fn sleepers(&self) -> usize {
        self.state.lock().asleep()
    }

    /// Block, in real time, until at least `threads` threads are waiting on this clock
    /// with nothing to do until it moves again or something notifies them. Call it after
    /// every [`ManualClock::advance`], and after anything else that wakes the link's
    /// threads, before asserting on the link's state.
    ///
    /// Pass every thread that can act on the link: its device thread, and whichever
    /// thread is blocked in the host's `read`. A thread that is running is invisible to
    /// the clock, so counting too few lets `settle` return before that thread is done.
    ///
    /// # Panics
    ///
    /// After 10 s of real time, so a test that miscounts fails instead of hanging.
    pub fn settle(&self, threads: usize) {
        let limit = later(SystemClock.now(), SETTLE_LIMIT);
        let mut st = self.state.lock();
        loop {
            let asleep = st.asleep();
            if asleep >= threads {
                return;
            }
            if self.changed.wait_until(&mut st, limit).timed_out() && st.asleep() < threads {
                panic!(
                    "ManualClock::settle({threads}): only {} of {} waiting threads asleep \
                     after {SETTLE_LIMIT:?}",
                    st.asleep(),
                    st.waiters.len()
                );
            }
        }
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.state.lock().now
    }

    fn wait(&self, wakeup: &Arc<Wakeup>, seen: u64, deadline: Option<Instant>) {
        let id = {
            let mut st = self.state.lock();
            if deadline.is_some_and(|d| d <= st.now) {
                return;
            }
            let id = st.next_id;
            st.next_id += 1;
            st.waiters.insert(
                id,
                Waiter {
                    wakeup: Arc::clone(wakeup),
                    seen,
                    deadline,
                },
            );
            id
        };
        self.changed.notify_all();
        // No real-time limit: moving the clock notifies every registered waiter, and a
        // move between registering and blocking has already moved the epoch past `seen`.
        wakeup.block(seen, None);
        self.state.lock().waiters.remove(&id);
        self.changed.notify_all();
    }
}

impl fmt::Debug for ManualClock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state.lock();
        f.debug_struct("ManualClock")
            .field("now", &st.now)
            .field("waiting", &st.waiters.len())
            .field("asleep", &st.asleep())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// A thread waiting on `clock` for `flag` or `deadline`, looping as every waiter must.
    /// Its result is the clock time it finished at.
    struct Waiting {
        wakeup: Arc<Wakeup>,
        flag: Arc<Mutex<bool>>,
        handle: thread::JoinHandle<Instant>,
    }

    impl Waiting {
        fn start(clock: &Arc<ManualClock>, deadline: Option<Instant>) -> Self {
            let wakeup = Arc::new(Wakeup::new());
            let flag = Arc::new(Mutex::new(false));
            let (clock, w, f) = (Arc::clone(clock), Arc::clone(&wakeup), Arc::clone(&flag));
            let handle = thread::spawn(move || {
                loop {
                    let seen = {
                        let set = f.lock();
                        if *set || deadline.is_some_and(|d| clock.now() >= d) {
                            return clock.now();
                        }
                        w.epoch()
                    };
                    clock.wait(&w, seen, deadline);
                }
            });
            Self {
                wakeup,
                flag,
                handle,
            }
        }

        fn raise(&self) {
            *self.flag.lock() = true;
            self.wakeup.notify();
        }
    }

    #[test]
    fn manual_time_moves_only_when_told() {
        let clock = ManualClock::new();
        let t0 = clock.now();
        assert_eq!(clock.now(), t0);
        clock.advance(5 * MS);
        assert_eq!(clock.now(), t0 + 5 * MS);
        clock.set(t0 + 7 * MS);
        assert_eq!(clock.now() - t0, 7 * MS);
        clock.advance(Duration::MAX);
        assert!(clock.now() > t0);
    }

    #[test]
    #[should_panic(expected = "backwards")]
    fn manual_time_never_goes_backwards() {
        let clock = ManualClock::new();
        let t0 = clock.now();
        clock.advance(MS);
        clock.set(t0);
    }

    #[test]
    fn a_wait_ends_when_the_clock_reaches_its_deadline_and_not_before() {
        let clock = Arc::new(ManualClock::new());
        let t0 = clock.now();
        let waiting = Waiting::start(&clock, Some(t0 + 10 * MS));
        clock.settle(1);
        clock.advance(10 * MS - Duration::from_nanos(1));
        // Woken by the move, it re-checked its deadline and went back to sleep.
        clock.settle(1);
        assert!(!waiting.handle.is_finished());
        clock.advance(Duration::from_nanos(1));
        assert_eq!(waiting.handle.join().unwrap(), t0 + 10 * MS);
        assert_eq!(clock.sleepers(), 0);
    }

    #[test]
    fn a_notify_ends_a_wait_without_the_clock_moving() {
        let clock = Arc::new(ManualClock::new());
        let t0 = clock.now();
        let waiting = Waiting::start(&clock, None);
        clock.settle(1);
        waiting.raise();
        assert_eq!(waiting.handle.join().unwrap(), t0);
    }

    #[test]
    fn a_notify_before_the_wait_starts_is_not_lost() {
        let clock = ManualClock::new();
        let wakeup = Arc::new(Wakeup::new());
        let seen = wakeup.epoch();
        wakeup.notify();
        // Returns at once: the epoch has already moved past `seen`.
        clock.wait(&wakeup, seen, None);
        // So does a wait whose deadline has already passed.
        clock.wait(&wakeup, wakeup.epoch(), Some(clock.now()));
    }

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
}
