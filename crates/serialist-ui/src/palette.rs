//! The command palette: every action the app registers, an entry per bundled example
//! plugin not yet installed ("Install example plugin: Airoha RACE"), every saved command
//! and every script in one list, narrowed as you type with the Commands panel's fuzzy
//! matcher
//! ([`fuzzy_match`]); Enter (or a click) runs the selected entry.
//!
//! The workspace opens it in a dialog (`command_palette::Toggle`, `cmd-shift-p` on macOS
//! and `ctrl-shift-p` elsewhere) and runs what it confirms: an action is dispatched from
//! the focus it would have from its key binding (the element focused before the palette
//! opened, else the terminal, the compose bar, a panel or the workspace, whichever has
//! it), a saved command is sent as the Commands panel sends it, a script runs on the
//! active tab's session. Each entry shows the keystrokes bound to it there.
//!
//! The list is built once when the palette opens, so it names the actions and commands
//! of that moment; filtering only reorders it.

use std::path::Path;

use serialist_core::CommandRef;
use serialist_core::commands::fuzzy_match;

use crate::actions::command_palette::{SelectNext, SelectPrevious};
use crate::actions::context;
use crate::actions::plugins::InstallExamplePlugin;
use crate::chrome;
use crate::config::Config;
use crate::plugin_files;
use crate::prelude::*;

/// Namespaces whose actions the palette lists: the app's own, not gpui-kit's.
const NAMESPACES: [&str; 9] = [
    "serialist",
    "serial",
    "terminal",
    "compose",
    "devices",
    "commands",
    "tabs",
    "scripts",
    "plugins",
];

/// Rows the list shows before it scrolls.
const VISIBLE_ROWS: usize = 12;

/// What an entry runs.
pub enum PaletteTarget {
    Action(Box<dyn Action>),
    /// A saved command, sent as the Commands panel sends it.
    Command(CommandRef),
    /// A script under the scripts folder, run on the active tab's session.
    Script(String),
}

impl Clone for PaletteTarget {
    fn clone(&self) -> Self {
        match self {
            PaletteTarget::Action(action) => PaletteTarget::Action(action.boxed_clone()),
            PaletteTarget::Command(reference) => PaletteTarget::Command(reference.clone()),
            PaletteTarget::Script(path) => PaletteTarget::Script(path.clone()),
        }
    }
}

impl std::fmt::Debug for PaletteTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PaletteTarget::Action(action) => write!(f, "Action({})", action.name()),
            PaletteTarget::Command(reference) => write!(f, "Command({reference})"),
            PaletteTarget::Script(path) => write!(f, "Script({path})"),
        }
    }
}

/// One line of the palette.
#[derive(Clone, Debug)]
pub struct PaletteEntry {
    /// `Terminal: Toggle hex view`, `Send: ATI`, `Run script: version_probe.lua`.
    pub label: String,
    /// The action's documentation, or where a command lives; matched too.
    pub detail: Option<String>,
    /// The keystrokes bound to it, as the platform writes them.
    pub binding: Option<String>,
    pub target: PaletteTarget,
}

/// What the palette tells the workspace.
#[derive(Clone, Debug)]
pub enum PaletteEvent {
    /// Run this; the palette is done.
    Confirmed(PaletteTarget),
}

/// `terminal::ToggleHexView` as the palette writes it: `Terminal: Toggle hex view`.
pub fn humanize(name: &str) -> String {
    let (namespace, action) = name.rsplit_once("::").unwrap_or(("", name));
    let mut words = Vec::new();
    let mut word = String::new();
    for c in action.chars() {
        let starts_number = c.is_ascii_digit()
            && !word
                .chars()
                .last()
                .is_some_and(|last| last.is_ascii_digit());
        if (c.is_uppercase() || starts_number) && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        word.push(c);
    }
    words.push(word);
    let action = words
        .iter()
        .enumerate()
        .map(|(ix, word)| {
            // An initialism Rust's naming spells in title case.
            match word.as_str() {
                "Ui" => return "UI".to_owned(),
                "Tcp" => return "TCP".to_owned(),
                _ => {}
            }
            let all_caps = word.len() > 1 && word.chars().all(|c| !c.is_lowercase());
            if ix == 0 || all_caps || word.chars().all(|c| c.is_ascii_digit()) {
                word.clone()
            } else {
                word.to_lowercase()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut namespace = namespace.replace('_', " ");
    if let Some(first) = namespace.get(..1) {
        namespace = first.to_uppercase() + &namespace[1..];
    }
    if namespace.is_empty() {
        action
    } else {
        format!("{namespace}: {action}")
    }
}

/// Where `action` would run from: the first of `handles` it is available to.
pub fn target_for<'a>(
    action: &dyn Action,
    handles: &'a [FocusHandle],
    window: &Window,
) -> Option<&'a FocusHandle> {
    handles
        .iter()
        .find(|handle| window.is_action_available_in(action, handle))
}

/// Every entry the palette lists now: the app's actions (with the keystrokes bound to
/// them where they would run, `handles` in order of preference), then an "Install
/// example plugin" entry per bundled example that is not installed, then the saved
/// commands, then the scripts.
pub fn entries(handles: &[FocusHandle], window: &Window, cx: &App) -> Vec<PaletteEntry> {
    let mut entries = Vec::new();
    let docs = cx.action_documentation();
    let mut names: Vec<&'static str> = cx
        .all_action_names()
        .iter()
        .copied()
        .filter(|name| {
            name.split_once("::")
                .is_some_and(|(namespace, _)| NAMESPACES.contains(&namespace))
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        // Actions that need arguments (a command to send, a script to run) are listed
        // as those commands and scripts below.
        let Ok(action) = cx.build_action(name, None) else {
            continue;
        };
        let binding = target_for(action.as_ref(), handles, window)
            .and_then(|handle| chrome::binding_text_in(action.as_ref(), handle, window))
            .or_else(|| {
                window
                    .highest_precedence_binding_for_action(action.as_ref())
                    .as_ref()
                    .and_then(chrome::keystroke_text)
            });
        let detail = docs
            .get(name)
            .and_then(|doc| doc.lines().next())
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty());
        entries.push(PaletteEntry {
            label: humanize(name),
            detail,
            binding,
            target: PaletteTarget::Action(action),
        });
    }
    for example in plugin_files::examples_to_install(cx) {
        entries.push(PaletteEntry {
            label: format!("Install example plugin: {}", example.title),
            detail: Some(example.description.to_owned()),
            binding: None,
            target: PaletteTarget::Action(Box::new(InstallExamplePlugin {
                name: example.name.to_owned(),
            })),
        });
    }
    if let Some(config) = cx.try_global::<Config>() {
        for collection in config.commands().collections() {
            for group in &collection.groups {
                for command in &group.commands {
                    let binding = command
                        .keybinding
                        .as_deref()
                        .map(str::trim)
                        .filter(|keys| !keys.is_empty())
                        .map(|keys| {
                            keys.split_whitespace()
                                .map(|key| {
                                    Keystroke::parse(key)
                                        .map_or_else(|_| key.to_owned(), |key| Kbd::format(&key))
                                })
                                .collect::<Vec<_>>()
                                .join(" ")
                        });
                    entries.push(PaletteEntry {
                        label: format!("Send: {}", command.name),
                        detail: Some(format!("{} \u{203a} {}", collection.name, group.name)),
                        binding,
                        target: PaletteTarget::Command(collection.command_ref(group, command)),
                    });
                }
            }
        }
        for script in config.scripts().iter() {
            entries.push(PaletteEntry {
                label: format!("Run script: {}", script.relative),
                detail: Path::new(&script.relative)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().replace('_', " ")),
                binding: None,
                target: PaletteTarget::Script(script.relative.clone()),
            });
        }
    }
    entries
}

/// How well `query` matches `entry`: every word of it must match the label or the
/// detail, as in the Commands panel's filter. `None` for no match.
pub fn score(query: &str, entry: &PaletteEntry) -> Option<i32> {
    let mut total = 0;
    for word in query.split_whitespace() {
        let label = fuzzy_match(word, &entry.label).map(|hit| hit.score * 2);
        let detail = entry
            .detail
            .as_deref()
            .and_then(|detail| fuzzy_match(word, detail))
            .map(|hit| hit.score);
        total += label.max(detail)?;
    }
    Some(total)
}

pub struct CommandPalette {
    input: Entity<InputState>,
    entries: Vec<PaletteEntry>,
    /// Indices into `entries`, best match first.
    matches: Vec<usize>,
    /// Index into `matches`.
    selected: usize,
    /// Confirmed once already: Enter reaches both the field and the dialog.
    confirmed: bool,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for CommandPalette {}

impl CommandPalette {
    pub fn new(entries: Vec<PaletteEntry>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Run an action, send a command or a script")
        });
        let changes = cx.subscribe_in(&input, window, |this, input, event, _, cx| match event {
            InputEvent::Change => {
                let query = input.read(cx).value().to_string();
                this.filter(&query, cx);
            }
            InputEvent::PressEnter { .. } => this.confirm(cx),
            _ => {}
        });
        let mut palette = Self {
            input,
            entries,
            matches: Vec::new(),
            selected: 0,
            confirmed: false,
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![changes],
        };
        palette.filter("", cx);
        palette
    }

    pub fn input(&self) -> &Entity<InputState> {
        &self.input
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// The entries that match the query, best first.
    pub fn matches(&self) -> impl Iterator<Item = &PaletteEntry> + '_ {
        self.matches.iter().map(|ix| &self.entries[*ix])
    }

    /// The labels of [`Self::matches`].
    pub fn labels(&self) -> Vec<String> {
        self.matches().map(|entry| entry.label.clone()).collect()
    }

    pub fn selected(&self) -> Option<&PaletteEntry> {
        self.matches.get(self.selected).map(|ix| &self.entries[*ix])
    }

    /// Show the entries matching `query`, best first, and select the first.
    pub fn filter(&mut self, query: &str, cx: &mut Context<Self>) {
        let mut scored: Vec<(i32, usize)> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(ix, entry)| score(query, entry).map(|score| (score, ix)))
            .collect();
        // Stable: equal scores keep the list's order (actions, commands, scripts).
        scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        self.matches = scored.into_iter().map(|(_, ix)| ix).collect();
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    /// Type `query` into the field, as a user would.
    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(query.to_owned(), window, cx)
        });
        self.filter(query, cx);
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
            self.scroll
                .scroll_to_item(self.selected, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    pub fn select_previous(&mut self, cx: &mut Context<Self>) {
        self.selected = self.selected.saturating_sub(1);
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Run the selected entry.
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if self.confirmed {
            return;
        }
        if let Some(entry) = self.selected() {
            let target = entry.target.clone();
            self.confirmed = true;
            cx.emit(PaletteEvent::Confirmed(target));
        }
    }

    /// Run the entry at `ix` of the matches, as a click on it does.
    pub fn confirm_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.matches.len() {
            self.selected = ix;
            self.confirm(cx);
        }
    }

    fn render_row(&self, ix: usize, entry: &PaletteEntry, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let selected = ix == self.selected;
        h_flex()
            .id(("palette-row", ix))
            .h(chrome::ROW_HEIGHT)
            .w_full()
            .px_2()
            .gap_2()
            .items_center()
            .rounded(px(4.))
            .when(selected, |row| row.bg(theme.list_active))
            .when(!selected, |row| {
                row.hover(|style| style.bg(theme.list_hover))
            })
            .child(
                div()
                    .flex_none()
                    .max_w(px(300.))
                    .truncate()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(SharedString::from(entry.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .children(entry.detail.clone().map(SharedString::from)),
            )
            .children(
                entry
                    .binding
                    .clone()
                    .map(|binding| chrome::quiet_chip(binding, cx)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.confirm_index(ix, cx)))
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = self.matches.len();
        let list = if rows == 0 {
            div()
                .h(chrome::ROW_HEIGHT)
                .px_2()
                .flex()
                .items_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Nothing matches.")
                .into_any_element()
        } else {
            uniform_list(
                "palette-list",
                rows,
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    range
                        .map(|ix| {
                            let entry = this.entries[this.matches[ix]].clone();
                            this.render_row(ix, &entry, cx)
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.scroll)
            .h(chrome::ROW_HEIGHT * rows.min(VISIBLE_ROWS) as f32)
            .into_any_element()
        };
        v_flex()
            .id("command-palette")
            .key_context(context::COMMAND_PALETTE)
            .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.select_next(cx)))
            .on_action(cx.listener(|this, _: &SelectPrevious, _, cx| this.select_previous(cx)))
            .w_full()
            .gap_2()
            .child(
                Input::new(&self.input)
                    .id("palette-input")
                    .prefix(Icon::new(IconName::Search).text_color(theme.muted_foreground)),
            )
            .child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, detail: Option<&str>) -> PaletteEntry {
        PaletteEntry {
            label: label.to_owned(),
            detail: detail.map(str::to_owned),
            binding: None,
            target: PaletteTarget::Script(label.to_owned()),
        }
    }

    #[test]
    fn action_names_read_as_words() {
        assert_eq!(
            humanize("terminal::ToggleHexView"),
            "Terminal: Toggle hex view"
        );
        assert_eq!(humanize("serial::Connect"), "Serial: Connect");
        assert_eq!(humanize("serial::ConnectTcp"), "Serial: Connect TCP");
        assert_eq!(humanize("serial::OpenCapture"), "Serial: Open capture");
        assert_eq!(humanize("tabs::ActivateTab1"), "Tabs: Activate tab 1");
        assert_eq!(
            humanize("command_palette::Toggle"),
            "Command palette: Toggle"
        );
        assert_eq!(
            humanize("serialist::OpenSettingsUi"),
            "Serialist: Open settings UI"
        );
    }

    #[test]
    fn every_word_must_match_and_the_label_counts_double() {
        let clear = entry("Terminal: Clear", None);
        let search = entry("Terminal: Search", Some("Open the search bar"));
        assert!(score("clear", &clear).is_some());
        assert!(score("term clr", &clear).is_some());
        assert!(score("clear bar", &clear).is_none(), "bar matches nothing");
        assert!(score("bar", &search).is_some(), "the detail matches too");
        assert!(
            score("search", &search) > score("bar", &search),
            "a label match outranks a detail match"
        );
        assert_eq!(score("", &clear), Some(0), "no query matches everything");
    }
}
