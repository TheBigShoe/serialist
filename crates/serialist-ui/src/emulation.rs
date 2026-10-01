//! VT mode's plumbing: how a session's ingest thread feeds a terminal screen next to its
//! store, and how the device's queries are answered.
//!
//! # The mode flow
//!
//! A session shows what it receives in one of two ways ([`Emulation`]): **monitor** mode
//! draws the store's lines, a log with colors and other escapes applied line by line;
//! **VT** mode draws a [`VtScreen`] the device draws on with cursor addressing (a U-Boot
//! menu, a Linux console, an editor). The session starts in the mode its device profile's
//! `emulation` names, else `terminal.emulation`; the toolbar's terminal button and
//! `terminal::ToggleEmulation` switch it.
//!
//! The store is not affected by the mode: it records every byte either way, so the raw
//! log, raw and text export, search and recording see the same stream in both. VT mode
//! adds a screen:
//!
//! - Every ingest thread runs a [`VtSlotSink`], built in its `spawn_with` closure with the
//!   session's doorbell, after the store and the view's other sinks. It feeds whatever
//!   [`VtSink`] the session's [`VtSlot`] holds; in monitor mode it holds none, and a chunk
//!   costs one uncontended lock.
//! - Switching to VT builds a fresh screen, sized to the terminal's cell grid, and a sink
//!   over it ([`VtSlot::sink`]), and installs it in the slot ([`VtSlot::install`]). It is
//!   fed from the next chunk on. **Bytes received before are not replayed**: a screen is
//!   the device's drawing from the moment it is shown, and replaying a log into a screen of
//!   another size would draw something the device never drew. Switching back takes the sink
//!   out ([`VtSlot::take`]) and shows the store's lines again, which never stopped growing.
//! - The main thread locks the slot only to install or take a sink, which waits at most
//!   for the chunk being parsed. The screen's own lock is the [`VtHandle`]'s business.
//!
//! # Waking
//!
//! The sink's waker is called on the ingest thread from inside the sink's own methods. It
//! cannot hold the doorbell itself: the view builds the sink, and a doorbell sender the
//! view kept would outlive the ingest thread and hide its end (a closed doorbell is how
//! the view learns the thread stopped). So the waker raises a flag in the slot, and the
//! slot sink rings the doorbell after the call that raised it. The view acknowledges the
//! screen ([`VtHandle::acknowledge`]) along with the store on every ring, then takes its
//! snapshot and drains its events.
//!
//! # Answering the device
//!
//! Queries (device attributes, cursor position, `CSI 18 t`) are answered on the ingest
//! thread as soon as the chunk that asked is parsed: the sink's responder writes the
//! answer through the session's [`SessionControl`] ([`VtSlot::set_writer`]; a reconnect
//! points it at the new session). A session with no control handle (a test double) gets
//! its answers queued as [`VtEvent::Respond`] instead, which the view writes on its next
//! wake. Color queries (OSC 4, 10, 11, 12) are answered by the view from the terminal
//! palette ([`palette_rgb`]): only it knows the theme.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serialist_core::ChunkSink;
pub use serialist_core::Emulation;
use serialist_vt::{VtEvent, VtHandle, VtScreen, VtSink};

use crate::session_handle::SessionControl;
use crate::terminal::TerminalPalette;

/// The writer a sink's answers go to.
type Writer = Arc<Mutex<Option<Arc<dyn SessionControl>>>>;

#[derive(Default)]
struct SlotInner {
    /// The sink the ingest thread feeds, with the id it was installed under.
    active: Mutex<Option<(u64, VtSink)>>,
    /// Raised by the active sink's waker; the slot sink rings the doorbell for it.
    rang: Arc<AtomicBool>,
    writer: Writer,
}

/// Where a session's ingest thread finds the screen to feed. Cheap to clone; clones share
/// the slot. See the module docs.
#[derive(Clone, Default)]
pub struct VtSlot {
    inner: Arc<SlotInner>,
}

impl std::fmt::Debug for VtSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VtSlot")
            .field("writer", &self.has_writer())
            .finish_non_exhaustive()
    }
}

impl VtSlot {
    /// A sink over `screen` whose wakes ring through this slot's ingest thread. With
    /// `answer`, the device's queries are answered from the ingest thread through the
    /// slot's writer; without, they are queued as events for the view.
    pub fn sink(&self, screen: VtScreen, answer: bool) -> VtSink {
        let rang = Arc::clone(&self.inner.rang);
        let sink = VtSink::new(
            screen,
            Some(Box::new(move || rang.store(true, Ordering::Release))),
        );
        if !answer {
            return sink;
        }
        let writer = Arc::clone(&self.inner.writer);
        sink.with_responder(move |bytes: &[u8]| {
            let writer = writer.lock().clone();
            match writer {
                Some(writer) => {
                    if writer.write(bytes.to_vec()).is_err() {
                        tracing::debug!("the session closed before a terminal answer went out");
                    }
                }
                None => tracing::debug!("no session to answer the device through"),
            }
        })
    }

    /// Send the device's answers through `writer`: the session's control handle, or
    /// `None` once it is gone.
    pub fn set_writer(&self, writer: Option<Arc<dyn SessionControl>>) {
        *self.inner.writer.lock() = writer;
    }

    pub fn has_writer(&self) -> bool {
        self.inner.writer.lock().is_some()
    }

    /// Feed `sink` from the next chunk on, under `id`, in place of any other. Returns the
    /// sink it replaced.
    pub fn install(&self, id: u64, sink: VtSink) -> Option<VtSink> {
        self.inner
            .active
            .lock()
            .replace((id, sink))
            .map(|(_, sink)| sink)
    }

    /// Stop feeding the sink installed under `id`, and hand it back, if it is still the
    /// one fed.
    pub fn take(&self, id: u64) -> Option<VtSink> {
        let mut active = self.inner.active.lock();
        match &*active {
            Some((current, _)) if *current == id => active.take().map(|(_, sink)| sink),
            _ => None,
        }
    }

    /// Whether a screen is being fed.
    pub fn is_active(&self) -> bool {
        self.inner.active.lock().is_some()
    }

    /// The ingest thread's side: a sink that feeds the slot's screen and calls `ring`
    /// when that screen has something new for the view.
    pub fn ingest_sink(&self, ring: impl Fn() + Send + 'static) -> VtSlotSink {
        VtSlotSink {
            slot: self.clone(),
            ring: Box::new(ring),
        }
    }
}

/// The [`ChunkSink`] on a session's ingest thread that feeds the [`VtSlot`]'s screen. See
/// the module docs.
pub struct VtSlotSink {
    slot: VtSlot,
    ring: Box<dyn Fn() + Send>,
}

impl VtSlotSink {
    fn feed(&mut self, call: impl FnOnce(&mut VtSink)) {
        if let Some((_, sink)) = self.slot.inner.active.lock().as_mut() {
            call(sink);
        }
        if self.slot.inner.rang.swap(false, Ordering::AcqRel) {
            (self.ring)();
        }
    }
}

impl ChunkSink for VtSlotSink {
    fn on_chunk(&mut self, bytes: &[u8], at: Instant) {
        self.feed(|sink| sink.on_chunk(bytes, at));
    }

    fn on_disconnect(&mut self) {
        self.feed(VtSink::on_disconnect);
    }

    fn on_idle(&mut self, now: Instant) {
        self.feed(|sink| sink.on_idle(now));
    }
}

/// The screen a view shows in VT mode.
pub struct VtState {
    /// The id it was installed in the slot under.
    pub id: u64,
    pub handle: VtHandle,
    /// The snapshot on display (held while paused).
    pub snapshot: Arc<serialist_vt::VtSnapshot>,
    /// The title the device set last, for the tab.
    pub title: Option<String>,
}

/// What a batch of screen events asks of the view, besides the answers it writes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EventEffects {
    /// Bytes to write to the device, in order: answers the ingest thread did not write,
    /// and color answers.
    pub writes: Vec<Vec<u8>>,
    /// The title changed: `Some(Some(title))`, or reset: `Some(None)`.
    pub title: Option<Option<String>>,
    /// The device rang the bell.
    pub bell: bool,
}

/// Sort `events` into what the view does about them, answering color queries from
/// `palette`.
pub fn effects_of(events: Vec<VtEvent>, palette: &TerminalPalette) -> EventEffects {
    let mut effects = EventEffects::default();
    for event in events {
        match event {
            VtEvent::Title(title) => effects.title = Some(Some(title)),
            VtEvent::ResetTitle => effects.title = Some(None),
            VtEvent::Bell => effects.bell = true,
            VtEvent::Respond(bytes) => effects.writes.push(bytes),
            VtEvent::ColorRequest(request) => match palette_rgb(palette, request.index) {
                Some((r, g, b)) => effects.writes.push(request.answer(r, g, b)),
                None => tracing::debug!(index = request.index, "color query not answered"),
            },
        }
    }
    effects
}

/// The RGB of color number `index` in Alacritty's numbering, from `palette`: 0 to 255
/// the palette's colors, 256 the default foreground, 257 the background, 258 the cursor.
pub fn palette_rgb(palette: &TerminalPalette, index: usize) -> Option<(u8, u8, u8)> {
    let color = match index {
        0..=255 => palette.indexed(index as u8),
        256 => palette.foreground,
        257 => palette.background,
        258 => palette.cursor,
        _ => return None,
    };
    let rgb = color.to_rgb();
    let byte = |channel: f32| (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some((byte(rgb.r), byte(rgb.g), byte(rgb.b)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A control handle that records what it was asked to write.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<Vec<u8>>>);

    impl SessionControl for Recorder {
        fn write(&self, bytes: Vec<u8>) -> Result<(), serialist_core::SessionClosed> {
            self.0.lock().push(bytes);
            Ok(())
        }

        fn set_control(
            &self,
            _: serialist_core::ControlLine,
            _: bool,
        ) -> Result<(), serialist_core::SessionClosed> {
            Ok(())
        }

        fn reconfigure(
            &self,
            _: serialist_core::SerialConfig,
        ) -> Result<(), serialist_core::SessionClosed> {
            Ok(())
        }

        fn serial_config(&self) -> Option<serialist_core::SerialConfig> {
            None
        }
    }

    #[test]
    fn the_slot_feeds_its_screen_rings_once_per_acknowledge_and_answers_the_device() {
        let slot = VtSlot::default();
        let recorder = Arc::new(Recorder::default());
        slot.set_writer(Some(recorder.clone()));
        let rings = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&rings);
        let mut ingest = slot.ingest_sink(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let now = Instant::now();

        // Nothing installed: chunks go nowhere and ring nothing.
        ingest.on_chunk(b"lost", now);
        assert_eq!(rings.load(Ordering::SeqCst), 0);
        assert!(!slot.is_active());

        let sink = slot.sink(VtScreen::new(20, 4, 100), true);
        let screen = sink.handle();
        assert!(slot.install(1, sink).is_none());
        ingest.on_chunk(b"hi\x1b[c", now);
        let snapshot = screen.snapshot();
        assert_eq!(
            snapshot.screen_text()[0],
            "hi",
            "fed from the next chunk only"
        );
        assert_eq!(rings.load(Ordering::SeqCst), 1);
        assert_eq!(
            *recorder.0.lock(),
            [b"\x1b[?6c".to_vec()],
            "answered at once"
        );
        assert!(screen.take_events().is_empty());

        // Unacknowledged: no second ring. Acknowledged: the next change rings.
        ingest.on_chunk(b"!", now);
        assert_eq!(rings.load(Ordering::SeqCst), 1);
        screen.acknowledge();
        ingest.on_chunk(b"?", now);
        assert_eq!(rings.load(Ordering::SeqCst), 2);

        // A stale id takes nothing; the right one stops the feed.
        assert!(slot.take(7).is_none());
        assert!(slot.take(1).is_some());
        screen.acknowledge();
        ingest.on_chunk(b"more", now);
        assert_eq!(screen.snapshot().screen_text()[0], "hi!?");
        assert_eq!(rings.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn without_a_writer_answers_are_queued_for_the_view() {
        let slot = VtSlot::default();
        let mut ingest = slot.ingest_sink(|| {});
        let sink = slot.sink(VtScreen::new(20, 4, 100), false);
        let screen = sink.handle();
        slot.install(1, sink);
        ingest.on_chunk(b"\x1b[5n\x1b]2;box\x07\x07", Instant::now());
        let palette = TerminalPalette::default();
        let effects = effects_of(screen.take_events(), &palette);
        assert_eq!(effects.writes, [b"\x1b[0n".to_vec()]);
        assert_eq!(effects.title, Some(Some("box".to_owned())));
        assert!(effects.bell);
    }

    #[test]
    fn color_queries_are_answered_from_the_palette() {
        let slot = VtSlot::default();
        let mut ingest = slot.ingest_sink(|| {});
        let sink = slot.sink(VtScreen::new(20, 4, 100), false);
        let screen = sink.handle();
        slot.install(1, sink);
        // The background, then palette color 1, each ended with BEL.
        ingest.on_chunk(b"\x1b]11;?\x07\x1b]4;1;?\x07", Instant::now());
        let palette = TerminalPalette::default();
        let effects = effects_of(screen.take_events(), &palette);
        let (r, g, b) = palette_rgb(&palette, 257).unwrap();
        let background = String::from_utf8(effects.writes[0].clone()).unwrap();
        assert!(background.starts_with("\x1b]11;rgb:"), "{background:?}");
        assert!(
            background.contains(&format!("{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")),
            "{background:?} for {r} {g} {b}"
        );
        assert_eq!(effects.writes.len(), 2);
        assert!(String::from_utf8_lossy(&effects.writes[1]).starts_with("\x1b]4;1;rgb:"));
        assert_eq!(palette_rgb(&palette, 300), None);
    }
}
