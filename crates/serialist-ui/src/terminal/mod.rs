//! The terminal: a custom GPUI element that draws a [`LineSource`], and the view that
//! owns its state (scroll, selection, search, wrap, timestamps, hex view, pause).
//!
//! Everything here reads lines through the `LineSource` and `Searcher` traits from
//! `serialist-core`, so the page store drops in for the in-memory doubles in
//! [`double`] without changes here.
//!
//! [`LineSource`]: serialist_core::LineSource

pub mod cache;
pub mod double;
pub mod element;
pub mod layout;
pub mod palette;
pub mod scroll;
pub mod search;
pub mod selection;
pub mod stats;
pub mod timestamps;
pub mod view;

#[cfg(test)]
mod tests;

pub use element::{CellMetrics, Highlights, TerminalElement, TerminalInputs};
pub use layout::{ScrollPosition, Span};
pub use palette::TerminalPalette;
pub use scroll::TerminalScrollHandle;
pub use search::SearchResults;
pub use selection::{Selection, SelectionMode, SelectionPoint};
pub use stats::{FrameSample, FrameStats, FrameSummary};
pub use timestamps::{Clock, TimestampMode};
pub use view::{DisplayMode, TerminalView};
