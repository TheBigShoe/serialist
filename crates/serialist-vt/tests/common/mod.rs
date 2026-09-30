//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::time::Instant;

use serialist_core::{Color, LineId, LineSource, Style, StyleFlags, StyleRun};
use serialist_vt::{CursorState, VtEvent, VtModes, VtScreen, VtSnapshot};

/// Everything a snapshot shows, minus times and raw offsets, which depend on when each
/// row was captured.
#[derive(Debug, PartialEq, Eq)]
pub struct Print {
    pub first: LineId,
    pub first_visible: LineId,
    pub lines: Vec<(LineId, String, Vec<StyleRun>, bool)>,
    pub cursor: Option<CursorState>,
    pub modes: VtModes,
    pub title: Option<String>,
    pub events: Vec<VtEvent>,
}

pub fn print(snapshot: &VtSnapshot, events: Vec<VtEvent>) -> Print {
    let mut lines = Vec::new();
    snapshot.lines(snapshot.first_line()..snapshot.end(), &mut lines);
    Print {
        first: snapshot.first_line(),
        first_visible: snapshot.first_visible(),
        lines: lines
            .into_iter()
            .map(|l| (l.id, l.text, l.runs, l.complete))
            .collect(),
        cursor: snapshot.cursor(),
        modes: snapshot.modes(),
        title: snapshot.title().map(str::to_owned),
        events,
    }
}

/// A fixed arrival time, so rows captured at different points compare equal.
pub fn t0() -> Instant {
    use std::sync::OnceLock;
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// Feed `bytes` in one go and take the snapshot.
pub fn one_shot(columns: usize, rows: usize, bytes: &[u8]) -> Print {
    let mut screen = VtScreen::new(columns, rows, 1000);
    screen.feed_at(bytes, t0());
    let snapshot = screen.snapshot();
    print(&snapshot, screen.take_events())
}

/// Feed `bytes` cut at `cuts` (sorted offsets), taking a snapshot after every piece so
/// the damage-driven reuse of rows is exercised, and return the last snapshot.
pub fn in_pieces(columns: usize, rows: usize, bytes: &[u8], cuts: &[usize]) -> Print {
    let mut screen = VtScreen::new(columns, rows, 1000);
    let mut start = 0;
    for &cut in cuts.iter().chain(std::iter::once(&bytes.len())) {
        let cut = cut.clamp(start, bytes.len());
        screen.feed_at(&bytes[start..cut], t0());
        let _ = screen.snapshot();
        start = cut;
    }
    let snapshot = screen.snapshot();
    print(&snapshot, screen.take_events())
}

pub fn screen_text(snapshot: &VtSnapshot) -> Vec<String> {
    snapshot
        .screen_text()
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub fn scrollback_text(snapshot: &VtSnapshot) -> Vec<String> {
    let mut lines = Vec::new();
    snapshot.lines(snapshot.first_line()..snapshot.first_visible(), &mut lines);
    lines.into_iter().map(|l| l.text).collect()
}

pub fn run(len: usize, fg: Color, bg: Color, flags: StyleFlags) -> StyleRun {
    StyleRun {
        len,
        style: Style { fg, bg, flags },
    }
}

pub fn plain(len: usize) -> StyleRun {
    run(len, Color::Default, Color::Default, StyleFlags::NONE)
}

pub fn flags(list: &[StyleFlags]) -> StyleFlags {
    let mut f = StyleFlags::NONE;
    for &flag in list {
        f.insert(flag);
    }
    f
}
