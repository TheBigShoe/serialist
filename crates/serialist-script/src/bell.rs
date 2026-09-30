//! The line bell: how a waiting script learns that new lines were stored.
//!
//! The ingest thread rings it through [`BellSink`] after each received chunk is stored
//! and published, and closes it when the session disconnects. A waiting script
//! subscribes, takes a snapshot, looks, and sleeps until the next ring. Subscribing
//! before the snapshot is what makes it lossless: a chunk stored before the snapshot is
//! in it, and one stored after rings a bell the script is already listening to. Rings
//! coalesce, so a script that is busy while ten chunks land wakes once and reads all ten.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use serialist_core::ChunkSink;
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, Default)]
struct Ring {
    closed: bool,
}

/// Rung after each received chunk is stored; closed when no more can arrive. Cheap to
/// clone and safe to ring from any thread; ringing never blocks.
#[derive(Clone)]
pub struct LineBell {
    tx: Arc<watch::Sender<Ring>>,
}

impl Default for LineBell {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for LineBell {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LineBell")
            .field("closed", &self.is_closed())
            .field("listeners", &self.tx.receiver_count())
            .finish()
    }
}

impl LineBell {
    pub fn new() -> Self {
        let (tx, _rx) = watch::channel(Ring::default());
        Self { tx: Arc::new(tx) }
    }

    /// New lines were stored: wake every waiting script.
    pub fn ring(&self) {
        self.tx.send_modify(|_| {});
    }

    /// No more lines will arrive: wake every waiting script for the last time. A wait
    /// then ends with "closed" once it has read what is stored.
    pub fn close(&self) {
        self.tx.send_modify(|ring| ring.closed = true);
    }

    pub fn is_closed(&self) -> bool {
        self.tx.borrow().closed
    }

    /// A [`ChunkSink`] for the session's ingest thread that rings on every chunk and
    /// closes the bell on disconnect.
    pub fn sink(&self) -> BellSink {
        BellSink(self.clone())
    }

    pub(crate) fn listen(&self) -> Listener {
        Listener {
            rx: self.tx.subscribe(),
            gone: false,
        }
    }
}

/// Rings a [`LineBell`] from the ingest thread. See [`LineBell::sink`].
#[derive(Debug)]
pub struct BellSink(LineBell);

impl ChunkSink for BellSink {
    fn on_chunk(&mut self, _bytes: &[u8], _at: Instant) {
        self.0.ring();
    }

    fn on_disconnect(&mut self) {
        self.0.close();
    }
}

/// One waiter's subscription, created before the waiter looks at the store.
pub(crate) struct Listener {
    rx: watch::Receiver<Ring>,
    /// Every bell handle was dropped: nothing can ring any more.
    gone: bool,
}

impl Listener {
    pub(crate) fn is_closed(&self) -> bool {
        self.gone || self.rx.borrow().closed
    }

    /// Returns after the next ring, at once if one came since the last call, or when
    /// the bell is dropped.
    pub(crate) async fn rung(&mut self) {
        if self.gone {
            return;
        }
        if self.rx.changed().await.is_err() {
            self.gone = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_before_the_wait_is_not_lost() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let bell = LineBell::new();
        let mut listener = bell.listen();
        bell.ring();
        runtime.block_on(listener.rung());
        assert!(!listener.is_closed());
        let mut sink = bell.sink();
        sink.on_disconnect();
        runtime.block_on(listener.rung());
        assert!(listener.is_closed());
        assert!(bell.is_closed());
    }

    #[test]
    fn a_dropped_bell_reads_as_closed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let bell = LineBell::new();
        let mut listener = bell.listen();
        drop(bell);
        runtime.block_on(listener.rung());
        assert!(listener.is_closed());
    }
}
