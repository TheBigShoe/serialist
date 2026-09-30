//! Moving events from a core channel into a GPUI entity without waking the UI per event.
//!
//! Core crates hand over work on crossbeam channels, which have no async receive. The
//! wait therefore happens on the background executor with a bounded timeout, never on
//! the main thread, and the entity is updated once per batch. After each batch the
//! loop sleeps for about a frame, so a busy port costs at most one entity update and
//! one repaint per frame no matter how small its chunks are.

use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::prelude::*;

/// Longest a background worker blocks waiting for the first event of a batch. Bounded so
/// the loop notices a dropped entity, and so idle sessions can refresh counters.
pub(crate) const IDLE_WAIT: Duration = Duration::from_millis(50);

/// Pause between batches: roughly one frame at 120 Hz.
pub(crate) const FRAME: Duration = Duration::from_millis(8);

/// Upper bound on one batch, so a producer that outruns the UI cannot keep a single
/// update running forever. Anything beyond it is picked up a frame later.
pub(crate) const MAX_BATCH: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) struct Batch<E> {
    pub events: Vec<E>,
    /// Every sender is gone; no more events will arrive.
    pub closed: bool,
}

/// Wait up to `wait` for one event, then take everything else already queued.
pub(crate) fn recv_batch<E>(rx: &Receiver<E>, wait: Duration, max: usize) -> Batch<E> {
    let mut events = Vec::new();
    match rx.recv_timeout(wait) {
        Ok(first) => events.push(first),
        Err(RecvTimeoutError::Timeout) => {
            return Batch {
                events,
                closed: false,
            };
        }
        Err(RecvTimeoutError::Disconnected) => {
            return Batch {
                events,
                closed: true,
            };
        }
    }
    events.extend(rx.try_iter().take(max.saturating_sub(1)));
    // An empty, disconnected channel is only reported on the next call, so no batch
    // that carries events is ever lost to an early exit.
    Batch {
        events,
        closed: false,
    }
}

/// Drain `rx` into the entity behind `cx` until the channel closes or the entity is
/// dropped.
///
/// `prepare` runs on the background executor with `state`, which lives there between
/// batches; it is where byte-level work belongs, so raw bytes never reach the main
/// thread. `apply` runs on the main thread once per wake with what `prepare` returned,
/// including after an idle timeout with no events; it decides whether anything changed
/// and calls `cx.notify()` itself.
pub(crate) fn drain_into<T, E, S, P>(
    rx: Receiver<E>,
    state: S,
    prepare: fn(&mut S, Vec<E>) -> P,
    cx: &mut Context<T>,
    mut apply: impl FnMut(&mut T, P, &mut Context<T>) + 'static,
) -> Task<()>
where
    T: 'static,
    E: Send + 'static,
    S: Send + 'static,
    P: Send + 'static,
{
    cx.spawn(async move |this, cx| {
        let mut state = state;
        loop {
            let worker_rx = rx.clone();
            let mut worker_state = state;
            let (prepared, closed, returned_state) = cx
                .background_spawn(async move {
                    let batch = recv_batch(&worker_rx, IDLE_WAIT, MAX_BATCH);
                    let prepared = prepare(&mut worker_state, batch.events);
                    (prepared, batch.closed, worker_state)
                })
                .await;
            state = returned_state;
            if this
                .update(cx, |entity, cx| apply(entity, prepared, cx))
                .is_err()
                || closed
            {
                break;
            }
            cx.background_executor().timer(FRAME).await;
        }
    })
}

/// A `prepare` for events that need no background work.
pub(crate) fn pass_through<E>(_: &mut (), events: Vec<E>) -> Vec<E> {
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_takes_everything_queued() {
        let (tx, rx) = crossbeam_channel::unbounded();
        for i in 0..5 {
            tx.send(i).unwrap();
        }
        let batch = recv_batch(&rx, Duration::from_millis(1), MAX_BATCH);
        assert_eq!(batch.events, [0, 1, 2, 3, 4]);
        assert!(!batch.closed);
    }

    #[test]
    fn batch_is_bounded() {
        let (tx, rx) = crossbeam_channel::unbounded();
        for i in 0..10 {
            tx.send(i).unwrap();
        }
        assert_eq!(recv_batch(&rx, Duration::ZERO, 4).events, [0, 1, 2, 3]);
        assert_eq!(recv_batch(&rx, Duration::ZERO, 4).events, [4, 5, 6, 7]);
    }

    #[test]
    fn idle_timeout_yields_an_empty_open_batch() {
        let (_tx, rx) = crossbeam_channel::unbounded::<u8>();
        let batch = recv_batch(&rx, Duration::from_millis(1), MAX_BATCH);
        assert!(batch.events.is_empty());
        assert!(!batch.closed);
    }

    #[test]
    fn closing_is_reported_after_the_last_events() {
        let (tx, rx) = crossbeam_channel::unbounded();
        tx.send(1).unwrap();
        drop(tx);
        let batch = recv_batch(&rx, Duration::from_millis(1), MAX_BATCH);
        assert_eq!(batch.events, [1]);
        assert!(!batch.closed);
        let batch = recv_batch(&rx, Duration::from_millis(1), MAX_BATCH);
        assert!(batch.events.is_empty());
        assert!(batch.closed);
    }
}
