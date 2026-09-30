//! [`VtSink`]: a [`ChunkSink`] that feeds a [`VtScreen`] on the ingest thread, and the
//! [`VtHandle`] the UI reads it through.
//!
//! ```no_run
//! use serialist_core::{ChunkSink, Ingest, Session, Store};
//! use serialist_vt::{VtScreen, VtSink};
//!
//! fn start(session: &Session) {
//!     let sink = VtSink::new(VtScreen::new(80, 24, 10_000), None);
//!     let screen = sink.handle(); // for the terminal element: screen.snapshot()
//!     let ingest = Ingest::spawn_with(
//!         session.events(),
//!         Store::default(),
//!         Box::new(move || -> Vec<Box<dyn ChunkSink>> { vec![Box::new(sink)] }),
//!         Box::new(|| {}),
//!     );
//! #   let _ = (screen, ingest);
//! }
//! ```
//!
//! The screen lives behind a mutex the sink and its handles share. The ingest thread
//! holds it while it parses one chunk and builds the snapshot; the UI holds it only to
//! resize, reset or drain events. After each chunk the sink publishes the new snapshot
//! by swapping one `Arc` under a second mutex, so [`VtHandle::snapshot`] never waits
//! for parsing.
//!
//! # Waking the UI
//!
//! The same doorbell protocol as the ingest thread's and the frame store's (see
//! [`serialist_core::ingest`]): after a chunk that changed the screen or queued an
//! event, the sink calls its waker only if the dirty flag was clear, and the UI calls
//! [`VtHandle::acknowledge`] *before* it takes the snapshot and drains the events. The
//! waker runs on the ingest thread and must not block.
//!
//! # Answering the device
//!
//! A device's queries (device attributes, cursor position) arrive as
//! [`VtEvent::Respond`]. Either the UI drains them and writes them to the session, or the
//! sink is given a responder ([`VtSink::with_responder`]) that writes them from the
//! ingest thread as soon as the chunk that asked is parsed; then they are not queued.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serialist_core::ChunkSink;

use crate::events::VtEvent;
use crate::screen::VtScreen;
use crate::snapshot::VtSnapshot;

struct Shared {
    screen: Mutex<VtScreen>,
    published: Mutex<Arc<VtSnapshot>>,
    dirty: AtomicBool,
}

impl Shared {
    /// Take the screen's snapshot and publish it if it is new. Returns whether it was.
    fn publish(&self, screen: &mut VtScreen) -> bool {
        let snapshot = screen.snapshot();
        let mut published = self.published.lock();
        if published.generation() == snapshot.generation() {
            return false;
        }
        *published = Arc::new(snapshot);
        true
    }

    /// Mark dirty; whether it was clean, which is when the waker should run.
    fn mark_dirty(&self) -> bool {
        !self.dirty.swap(true, Ordering::AcqRel)
    }
}

type Responder = Box<dyn FnMut(&[u8]) + Send>;

/// Feeds every received chunk to a shared [`VtScreen`]. See the module docs.
pub struct VtSink {
    shared: Arc<Shared>,
    waker: Option<Box<dyn Fn() + Send>>,
    responder: Option<Responder>,
}

impl std::fmt::Debug for VtSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtSink")
            .field("screen", &*self.shared.screen.lock())
            .field("responder", &self.responder.is_some())
            .finish_non_exhaustive()
    }
}

impl VtSink {
    /// Feed `screen`, calling `waker` (if any) when a new snapshot or event is ready and
    /// the reader has acknowledged the last wake.
    pub fn new(mut screen: VtScreen, waker: Option<Box<dyn Fn() + Send>>) -> Self {
        let first = Arc::new(screen.snapshot());
        Self {
            shared: Arc::new(Shared {
                screen: Mutex::new(screen),
                published: Mutex::new(first),
                dirty: AtomicBool::new(false),
            }),
            waker,
            responder: None,
        }
    }

    /// Write the device's answers ([`VtEvent::Respond`]) with `respond`, on the ingest
    /// thread, instead of queueing them. `respond` must not block for long; handing the
    /// bytes to the session's writer is the intended use.
    pub fn with_responder(mut self, respond: impl FnMut(&[u8]) + Send + 'static) -> Self {
        self.responder = Some(Box::new(respond));
        self
    }

    /// A handle for the UI. Any number can exist; they all see the same screen.
    pub fn handle(&self) -> VtHandle {
        VtHandle {
            shared: Arc::clone(&self.shared),
        }
    }

    /// Publish what `screen` shows now, answer the device, and wake the reader if there
    /// is anything new for it.
    fn settle(&mut self, screen: &mut VtScreen) {
        if let Some(respond) = &mut self.responder {
            for bytes in screen.take_responses() {
                respond(&bytes);
            }
        }
        let fresh = self.shared.publish(screen);
        let events = screen.has_events();
        if (fresh || events)
            && self.shared.mark_dirty()
            && let Some(waker) = &self.waker
        {
            waker();
        }
    }
}

impl ChunkSink for VtSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        let shared = Arc::clone(&self.shared);
        let mut screen = shared.screen.lock();
        screen.feed_at(bytes, at);
        self.settle(&mut screen);
    }

    fn on_disconnect(&mut self) {
        // Nothing more is coming, so a synchronized update left open will never end.
        let shared = Arc::clone(&self.shared);
        let mut screen = shared.screen.lock();
        if screen.end_sync() {
            self.settle(&mut screen);
        }
    }

    fn on_idle(&mut self, now: Instant) {
        let shared = Arc::clone(&self.shared);
        let mut screen = shared.screen.lock();
        if screen.flush_sync(now) {
            self.settle(&mut screen);
        }
    }
}

/// The UI's side of a [`VtSink`]: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct VtHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for VtHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtHandle")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl VtHandle {
    /// The screen as of the last chunk (or resize or reset). One uncontended lock and
    /// an `Arc` clone; never waits for parsing.
    pub fn snapshot(&self) -> Arc<VtSnapshot> {
        Arc::clone(&self.shared.published.lock())
    }

    /// The reader has looked, or is about to take its snapshot and drain its events:
    /// the next change may wake it again.
    pub fn acknowledge(&self) {
        self.shared.dirty.store(false, Ordering::Release);
    }

    /// Resize the screen, for instance to the element's size in cells, and publish the
    /// result. Does not wake: the caller takes the new snapshot itself.
    pub fn resize(&self, columns: usize, rows: usize) {
        let mut screen = self.shared.screen.lock();
        screen.resize(columns, rows);
        self.shared.publish(&mut screen);
    }

    /// Reset the screen and forget its scrollback (see [`VtScreen::reset`]), and publish
    /// the result.
    pub fn reset(&self) {
        let mut screen = self.shared.screen.lock();
        screen.reset();
        self.shared.publish(&mut screen);
    }

    /// Everything the terminal asked for since the last call, oldest first.
    pub fn take_events(&self) -> Vec<VtEvent> {
        self.shared.screen.lock().take_events()
    }

    /// The screen's size in columns and rows.
    pub fn size(&self) -> (usize, usize) {
        let screen = self.shared.screen.lock();
        (screen.columns(), screen.rows())
    }
}
