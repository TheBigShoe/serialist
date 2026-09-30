//! The compose bar: a single-line input that sends a line plus a line ending, with
//! shell-style history on Up and Down, and "save as command" for the text in it.
//!
//! The history a compose bar walks is seeded from the workspace's
//! [`PersistentHistory`](crate::history::PersistentHistory) when its session opens, so it
//! carries over from earlier runs.

use std::collections::VecDeque;

use crate::actions::{CycleLineEnding, HistoryNext, HistoryPrevious, SaveAsCommand, context};
use crate::config::Config;
use crate::prelude::*;

/// What Enter appends to the typed text: the settings type, so a `line_ending` setting
/// or a device profile's `eol` is used as is.
pub use serialist_core::LineEnding;

/// What the compose bar does with a [`LineEnding`] beyond what settings need.
pub trait LineEndingExt: Sized {
    /// The next ending in the order the cycle button walks through.
    fn next(self) -> Self;
    /// The bytes a submitted line puts on the wire.
    fn frame(self, text: &str) -> Vec<u8>;
}

impl LineEndingExt for LineEnding {
    fn next(self) -> Self {
        let ix = Self::ALL.iter().position(|e| *e == self).unwrap_or(0);
        Self::ALL[(ix + 1) % Self::ALL.len()]
    }

    fn frame(self, text: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(text.len() + 2);
        bytes.extend_from_slice(text.as_bytes());
        bytes.extend_from_slice(self.bytes());
        bytes
    }
}

/// Sent-line history with a cursor, like a shell's.
///
/// Walking up from a fresh line remembers what was typed so far (the draft), and
/// walking back down past the newest entry restores it.
#[derive(Clone, Debug)]
pub struct History {
    entries: VecDeque<String>,
    cursor: Option<usize>,
    draft: String,
    capacity: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(500)
    }
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            cursor: None,
            draft: String::new(),
            capacity: capacity.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &str> + '_ {
        self.entries.iter().map(String::as_str)
    }

    /// The newest entry.
    pub fn newest(&self) -> Option<&str> {
        self.entries.back().map(String::as_str)
    }

    /// Replace the entries with `entries`, oldest first, keeping the newest that fit.
    pub fn replace(&mut self, entries: impl IntoIterator<Item = String>) {
        self.entries.clear();
        self.cursor = None;
        self.draft.clear();
        for entry in entries {
            self.push(&entry);
        }
    }

    /// Record a sent line and reset the cursor. Blank lines and an immediate repeat of
    /// the newest entry are not recorded, as in most shells.
    pub fn push(&mut self, line: &str) {
        self.cursor = None;
        self.draft.clear();
        if line.trim().is_empty() || self.entries.back().is_some_and(|last| last == line) {
            return;
        }
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(line.to_owned());
    }

    /// Step to the previous (older) entry. `current` is the input's text, kept as the
    /// draft when leaving the fresh line. `None` means leave the input alone.
    pub fn older(&mut self, current: &str) -> Option<&str> {
        let ix = match self.cursor {
            None if self.entries.is_empty() => return None,
            None => {
                self.draft = current.to_owned();
                self.entries.len() - 1
            }
            Some(0) => return None,
            Some(ix) => ix - 1,
        };
        self.cursor = Some(ix);
        self.entries.get(ix).map(String::as_str)
    }

    /// Step to the next (newer) entry, or back to the draft past the newest one.
    pub fn newer(&mut self) -> Option<&str> {
        let ix = self.cursor?;
        if ix + 1 < self.entries.len() {
            self.cursor = Some(ix + 1);
            self.entries.get(ix + 1).map(String::as_str)
        } else {
            self.cursor = None;
            Some(&self.draft)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposeEvent {
    /// The user pressed Enter. `bytes` is the text framed with the line ending.
    Submit { text: String, bytes: Vec<u8> },
    /// Save `text` as a saved command: the input's text, or the newest history entry
    /// when the input is empty.
    SaveAsCommand { text: String },
}

pub struct ComposeBar {
    input: Entity<InputState>,
    history: History,
    line_ending: LineEnding,
    /// Echo each sent line into the scrollback, for devices that do not echo.
    local_echo: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposeEvent> for ComposeBar {}

impl ComposeBar {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Type a line and press Enter to send")
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.submit(window, cx);
            }
        });
        Self {
            input,
            history: History::default(),
            line_ending: LineEnding::default(),
            local_echo: true,
            _subscriptions: vec![subscription],
        }
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn local_echo(&self) -> bool {
        self.local_echo
    }

    pub fn set_local_echo(&mut self, echo: bool, cx: &mut Context<Self>) {
        if self.local_echo != echo {
            self.local_echo = echo;
            cx.notify();
        }
    }

    pub fn set_line_ending(&mut self, line_ending: LineEnding, cx: &mut Context<Self>) {
        self.line_ending = line_ending;
        cx.notify();
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// Start from `entries`, oldest first, as history (the persisted history of earlier
    /// sessions).
    pub fn set_history_entries(&mut self, entries: Vec<String>, cx: &mut Context<Self>) {
        self.history.replace(entries);
        cx.notify();
    }

    /// Ask for the text in the input (or the newest history entry) to become a saved
    /// command. Nothing happens when both are empty.
    pub fn save_as_command(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let text = if text.trim().is_empty() {
            self.history.newest().unwrap_or_default().to_owned()
        } else {
            text
        };
        if !text.trim().is_empty() {
            cx.emit(ComposeEvent::SaveAsCommand { text });
        }
    }

    pub fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Send the input's text: record it, clear the input and emit [`ComposeEvent::Submit`].
    pub fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        self.history.push(&text);
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        let bytes = self.line_ending.frame(&text);
        cx.emit(ComposeEvent::Submit { text, bytes });
        cx.notify();
    }

    fn cycle_line_ending(&mut self, _: &CycleLineEnding, _: &mut Window, cx: &mut Context<Self>) {
        self.set_line_ending(self.line_ending.next(), cx);
    }

    fn save_as_command_action(
        &mut self,
        _: &SaveAsCommand,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_as_command(cx);
    }

    fn history_previous(
        &mut self,
        _: &HistoryPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.text(cx);
        if let Some(entry) = self.history.older(&current).map(str::to_owned) {
            self.set_text(entry, window, cx);
        }
    }

    fn history_next(&mut self, _: &HistoryNext, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(entry) = self.history.newer().map(str::to_owned) {
            self.set_text(entry, window, cx);
        }
    }

    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        // `set_value` leaves the caret at the end of a single-line input, which is where
        // a recalled command should be edited from.
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
    }
}

impl Render for ComposeBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = Config::toolbar_background(cx);
        let theme = cx.theme();
        h_flex()
            .key_context(context::COMPOSE_BAR)
            .on_action(cx.listener(Self::cycle_line_ending))
            .on_action(cx.listener(Self::save_as_command_action))
            .on_action(cx.listener(Self::history_previous))
            .on_action(cx.listener(Self::history_next))
            .w_full()
            .gap_2()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .when_some(toolbar, |bar, background| bar.bg(background))
            .child(
                div()
                    .flex_1()
                    .font_family(theme.mono_font_family.clone())
                    .child(Input::new(&self.input).id("compose-input").small()),
            )
            .child(
                Button::new("local-echo")
                    .label(if self.local_echo { "Echo" } else { "No echo" })
                    .tooltip("Show sent lines in the scrollback; click to toggle")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_local_echo(!this.local_echo, cx);
                    })),
            )
            .child(
                Button::new("line-ending")
                    .label(self.line_ending.label())
                    .tooltip("Line ending sent after each line; click to cycle")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_line_ending(this.line_ending.next(), cx);
                    })),
            )
            .child(
                Button::new("save-as-command")
                    .label("Save…")
                    .tooltip("Save this line (or the last one sent) as a command")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|this, _, _, cx| this.save_as_command(cx))),
            )
            .child(
                Button::new("send")
                    .label("Send")
                    .small()
                    .primary()
                    .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ending_cycles_through_all_four_and_defaults_to_crlf() {
        assert_eq!(LineEnding::default(), LineEnding::Crlf);
        let mut ending = LineEnding::Crlf;
        let mut seen = Vec::new();
        for _ in 0..4 {
            ending = ending.next();
            seen.push(ending);
        }
        assert_eq!(
            seen,
            [
                LineEnding::None,
                LineEnding::Cr,
                LineEnding::Lf,
                LineEnding::Crlf
            ]
        );
    }

    #[test]
    fn line_ending_frames_bytes() {
        assert_eq!(LineEnding::None.frame("AT"), b"AT");
        assert_eq!(LineEnding::Cr.frame("AT"), b"AT\r");
        assert_eq!(LineEnding::Lf.frame("AT"), b"AT\n");
        assert_eq!(LineEnding::Crlf.frame("AT"), b"AT\r\n");
        assert_eq!(LineEnding::Crlf.frame(""), b"\r\n");
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut history = History::default();
        history.push("one");
        history.push("two");
        history.push("three");

        assert_eq!(history.older("dra"), Some("three"));
        assert_eq!(history.older("three"), Some("two"));
        assert_eq!(history.older("two"), Some("one"));
        assert_eq!(history.older("one"), None, "stops at the oldest");
        assert_eq!(history.newer(), Some("two"));
        assert_eq!(history.newer(), Some("three"));
        assert_eq!(history.newer(), Some("dra"), "past the newest is the draft");
        assert_eq!(history.newer(), None, "already on the fresh line");
    }

    #[test]
    fn history_is_empty_safe() {
        let mut history = History::default();
        assert_eq!(history.older("x"), None);
        assert_eq!(history.newer(), None);
    }

    #[test]
    fn history_skips_blanks_and_immediate_repeats() {
        let mut history = History::default();
        history.push("AT");
        history.push("AT");
        history.push("   ");
        history.push("");
        history.push("ATI");
        history.push("AT");
        assert_eq!(history.entries().collect::<Vec<_>>(), ["AT", "ATI", "AT"]);
    }

    #[test]
    fn pushing_resets_the_cursor() {
        let mut history = History::default();
        history.push("a");
        history.push("b");
        assert_eq!(history.older(""), Some("b"));
        assert_eq!(history.older("b"), Some("a"));
        history.push("c");
        assert_eq!(history.older(""), Some("c"));
    }

    #[test]
    fn history_can_be_seeded_from_disk() {
        let mut history = History::new(3);
        history.push("typed");
        history.replace(["a", "b", "b", "c", "d"].map(str::to_owned));
        assert_eq!(history.entries().collect::<Vec<_>>(), ["b", "c", "d"]);
        assert_eq!(history.newest(), Some("d"));
        assert_eq!(history.older(""), Some("d"));
    }

    #[test]
    fn history_drops_the_oldest_past_capacity() {
        let mut history = History::new(2);
        history.push("a");
        history.push("b");
        history.push("c");
        assert_eq!(history.entries().collect::<Vec<_>>(), ["b", "c"]);
    }
}
