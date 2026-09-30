//! What the terminal asks of the app: [`VtEvent`]s, collected by the [`Listener`] that
//! Alacritty's `Term` reports to.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::vte::ansi::Rgb;
use parking_lot::Mutex;

/// Events kept for the app at most. Past that the oldest go, with a warning: an app that
/// never drains events must not grow without bound under a device that keeps asking.
pub const MAX_EVENTS: usize = 1024;

/// Something the terminal wants the app to do, in the order the device caused it.
/// Drained with [`VtScreen::take_events`](crate::VtScreen::take_events) or
/// [`VtHandle::take_events`](crate::VtHandle::take_events).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VtEvent {
    /// The device set the window title (OSC 0 or 2), to something new.
    Title(String),
    /// The device reset the title to the app's default (an empty OSC 0 or 2, a title
    /// stack pop to nothing, or a full reset).
    ResetTitle,
    /// BEL. Consecutive bells with nothing between them in the queue are one event.
    Bell,
    /// Bytes to write back to the device: the answer to a query such as primary or
    /// secondary device attributes (`CSI c`, `CSI > c`), device status or the cursor
    /// position (`CSI 5 n`, `CSI 6 n`), the text area size in characters (`CSI 18 t`),
    /// or a mode report (`CSI ? Ps $ p`). A device that asks usually waits for the answer,
    /// so write these to the session promptly.
    Respond(Vec<u8>),
    /// The device asked for the RGB value of a color (OSC 4, 10, 11 or 12 with `?`).
    /// Only the app knows the theme, so it answers with [`ColorRequest::answer`].
    ColorRequest(ColorRequest),
}

/// A color query from the device. See [`VtEvent::ColorRequest`].
#[derive(Clone)]
pub struct ColorRequest {
    /// Which color: 0 to 255 is the palette, 256 the default foreground, 257 the default
    /// background, 258 the cursor (Alacritty's numbering).
    pub index: usize,
    format: Arc<dyn Fn(Rgb) -> String + Send + Sync>,
}

impl ColorRequest {
    /// The bytes that answer the query with `r`, `g`, `b`, in the form and with the
    /// terminator the device used.
    pub fn answer(&self, r: u8, g: u8, b: u8) -> Vec<u8> {
        (self.format)(Rgb { r, g, b }).into_bytes()
    }
}

impl fmt::Debug for ColorRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ColorRequest")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

/// Two requests are equal when they ask for the same color.
impl PartialEq for ColorRequest {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for ColorRequest {}

#[derive(Default)]
pub(crate) struct EventState {
    pub events: VecDeque<VtEvent>,
    pub title: Option<String>,
    dropped: u64,
}

impl EventState {
    fn push(&mut self, event: VtEvent) {
        if self.events.len() == MAX_EVENTS {
            self.events.pop_front();
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                tracing::warn!(
                    dropped = self.dropped,
                    "terminal events are not being drained; dropping the oldest"
                );
            }
        }
        self.events.push_back(event);
    }

    /// A reset that Alacritty does not report: its full reset clears the title silently.
    pub fn reset_title(&mut self) {
        if self.title.take().is_some() {
            self.push(VtEvent::ResetTitle);
        }
    }
}

/// The `EventListener` handed to Alacritty's `Term`. Shared with the screen, which reads
/// the title and drains the queue; both only ever run under the screen's `&mut`.
#[derive(Clone, Default)]
pub(crate) struct Listener(pub Arc<Mutex<EventState>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut state = self.0.lock();
        match event {
            Event::Title(title) => {
                if state.title.as_deref() != Some(title.as_str()) {
                    state.title = Some(title.clone());
                    state.push(VtEvent::Title(title));
                }
            }
            Event::ResetTitle => state.reset_title(),
            Event::Bell => {
                if state.events.back() != Some(&VtEvent::Bell) {
                    state.push(VtEvent::Bell);
                }
            }
            Event::PtyWrite(text) => state.push(VtEvent::Respond(text.into_bytes())),
            Event::ColorRequest(index, format) => {
                state.push(VtEvent::ColorRequest(ColorRequest { index, format }));
            }
            // The rest concern a GUI terminal with a PTY: mouse cursor shape, cursor
            // blinking (the snapshot carries it), wake-ups, the clipboard (OSC 52 is
            // disabled), pixel sizes, and child processes.
            other => tracing::trace!(event = ?other, "terminal event ignored"),
        }
    }
}
