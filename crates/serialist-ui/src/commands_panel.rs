//! The Commands panel: saved-command collections in the left dock, below Devices.
//!
//! The panel lists the [`CommandStore`] of the installed [`Config`]: each collection and
//! each group is a collapsible section, and each command a 28 px row with its name, a
//! chip for its keybinding and its description as a tooltip; hovering a row brings up
//! its send and edit buttons. The filter field at the top narrows the list with
//! [`CommandStore::filter`], best match first. Enter or a click sends a command
//! ([`CommandsPanelEvent::Send`], which the workspace turns into a send on the session,
//! asking for parameters first); a right click or the row's edit button opens the
//! [`CommandEditor`] in a dialog. The header's + opens a menu with New command and New
//! collection.
//!
//! Edits never change the store in memory: the editor writes the collection's file
//! through [`CommandStore::save`] (atomically), the config watcher reports the file
//! ([`ConfigEvent::Commands`](serialist_core::ConfigEvent::Commands)), and the reload
//! brings the change back here, as a hand edit of the file would.
//!
//! The bundled examples are read-only, tagged "examples". Their section's copy button
//! writes them to a collection of the user's own, and editing one of their commands
//! saves a copy of it into a user collection.

use std::collections::HashSet;

use serialist_core::commands::DEFAULT_EXPECT_TIMEOUT_MS;
use serialist_core::{Command, CommandRef, CommandStore, Expect, LineEnding, Payload};

use crate::actions::commands::{
    EditSelected, NewCollection, NewCommand, SelectNext, SelectPrevious, SendSelected,
};
use crate::actions::context;
use crate::chrome;
use crate::config::Config;
use crate::dialog_footer::DialogButtons;
use crate::prelude::*;
use crate::status::Notice;

/// The collection a new command goes to when the user has none.
pub const DEFAULT_COLLECTION: &str = "My commands";
/// The group a new command goes to unless the form says otherwise.
pub const DEFAULT_GROUP: &str = "Commands";
/// The group "save as command" puts compose lines in.
pub const SAVED_GROUP: &str = "Saved";

/// A collapsible section of the list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Collection(String),
    Group { collection: String, group: String },
}

/// One row of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    Collection {
        name: String,
        read_only: bool,
        collapsed: bool,
        commands: usize,
    },
    Group {
        collection: String,
        name: String,
        collapsed: bool,
    },
    Command {
        reference: CommandRef,
        keybinding: Option<String>,
        description: String,
        read_only: bool,
        /// `collection › group`, shown when filtering flattens the sections.
        location: Option<String>,
    },
}

impl Row {
    pub fn command(&self) -> Option<&CommandRef> {
        match self {
            Row::Command { reference, .. } => Some(reference),
            _ => None,
        }
    }
}

/// The rows for `store`: every collection, group and command, skipping what collapsed
/// sections hide, or, for a non-blank `query`, the matching commands best first with
/// their location.
pub fn build_rows(store: &CommandStore, query: &str, collapsed: &HashSet<Section>) -> Vec<Row> {
    let command_row =
        |reference: CommandRef, command: &Command, read_only, location| Row::Command {
            keybinding: command
                .keybinding
                .as_deref()
                .map(str::trim)
                .filter(|keys| !keys.is_empty())
                .map(str::to_owned),
            description: command.description.clone(),
            read_only,
            location,
            reference,
        };
    let read_only = |reference: &CommandRef| {
        store
            .collection(&reference.collection)
            .is_some_and(|collection| collection.is_read_only())
    };
    if !query.trim().is_empty() {
        return store
            .filter(query)
            .into_iter()
            .filter_map(|(reference, _)| {
                let command = store.get(&reference)?;
                let location = format!("{} \u{203a} {}", reference.collection, reference.group);
                Some(command_row(
                    reference.clone(),
                    command,
                    read_only(&reference),
                    Some(location),
                ))
            })
            .collect();
    }
    let mut rows = Vec::new();
    for collection in store.collections() {
        let section = Section::Collection(collection.name.clone());
        let folded = collapsed.contains(&section);
        rows.push(Row::Collection {
            name: collection.name.clone(),
            read_only: collection.is_read_only(),
            collapsed: folded,
            commands: collection.commands().count(),
        });
        if folded {
            continue;
        }
        for group in &collection.groups {
            let section = Section::Group {
                collection: collection.name.clone(),
                group: group.name.clone(),
            };
            let folded = collapsed.contains(&section);
            rows.push(Row::Group {
                collection: collection.name.clone(),
                name: group.name.clone(),
                collapsed: folded,
            });
            if folded {
                continue;
            }
            for command in &group.commands {
                rows.push(command_row(
                    collection.command_ref(group, command),
                    command,
                    collection.is_read_only(),
                    None,
                ));
            }
        }
    }
    rows
}

/// The collection new commands go to by default: the first the user can write.
pub fn default_collection(store: &CommandStore) -> String {
    store
        .collections()
        .iter()
        .find(|collection| !collection.is_read_only())
        .map_or_else(|| DEFAULT_COLLECTION.to_owned(), |c| c.name.clone())
}

/// A name for a copy of `name` that no collection has yet.
fn copy_name(store: &CommandStore, name: &str) -> String {
    let mut candidate = format!("{name} (copy)");
    let mut number = 2;
    while store.collection(&candidate).is_some() {
        candidate = format!("{name} (copy {number})");
        number += 1;
    }
    candidate
}

fn commands_of(cx: &App) -> std::sync::Arc<CommandStore> {
    cx.try_global::<Config>()
        .map(|config| config.commands().clone())
        .unwrap_or_default()
}

/// Write a copy of the collection called `name` (the bundled examples, say) as a new
/// collection of the user's. Returns the copy's name. The watcher's reload shows it.
pub fn copy_collection(store: &CommandStore, name: &str) -> Result<String, String> {
    let mut store = store.clone();
    let source = store
        .collection(name)
        .cloned()
        .ok_or_else(|| format!("there is no collection called `{name}`"))?;
    let copy_name = copy_name(&store, name);
    let mut copy = store
        .create_collection(&copy_name)
        .map_err(|error| error.to_string())?
        .clone();
    copy.groups = source.groups;
    store.save(&copy).map_err(|error| error.to_string())?;
    Ok(copy_name)
}

/// Write a new, empty collection called `name`.
pub fn create_collection(store: &CommandStore, name: &str) -> Result<(), String> {
    let mut store = store.clone();
    let created = store
        .create_collection(name)
        .map_err(|error| error.to_string())?
        .clone();
    store.save(&created).map_err(|error| error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandsPanelEvent {
    /// Send this command on the session (after asking for its parameters).
    Send(CommandRef),
}

pub struct CommandsPanel {
    filter: Entity<InputState>,
    collapsed: HashSet<Section>,
    selected: Option<CommandRef>,
    rows: Vec<Row>,
    notice: Option<Notice>,
    /// The editor open in a dialog, if any.
    editor: Option<Entity<CommandEditor>>,
    /// The new-collection prompt open in a dialog, if any.
    name_prompt: Option<Entity<InputState>>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CommandsPanelEvent> for CommandsPanel {}

impl Focusable for CommandsPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CommandsPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter commands"));
        // Enter in the field is `commands::SendSelected` from the `CommandsPanel` keymap
        // section: a single-line input lets Enter through to its parents' bindings.
        let filter_events = cx.subscribe_in(&filter, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event {
                this.refresh(cx);
            }
        });
        // A reload (a file saved here or by hand) rebuilds the list.
        let config_changes = cx.observe_global::<Config>(|this, cx| this.refresh(cx));
        let mut panel = Self {
            filter,
            collapsed: HashSet::new(),
            selected: None,
            rows: Vec::new(),
            notice: None,
            editor: None,
            name_prompt: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _subscriptions: vec![filter_events, config_changes],
        };
        panel.refresh(cx);
        panel
    }

    // --- Reading -----------------------------------------------------------------------

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&CommandRef> {
        self.selected.as_ref()
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub fn filter_input(&self) -> &Entity<InputState> {
        &self.filter
    }

    pub fn filter_text(&self, cx: &App) -> String {
        self.filter.read(cx).value().to_string()
    }

    /// The command editor, while its dialog is open.
    pub fn editor(&self) -> Option<&Entity<CommandEditor>> {
        self.editor.as_ref()
    }

    /// The new collection's name field, while its dialog is open.
    pub fn name_prompt(&self) -> Option<&Entity<InputState>> {
        self.name_prompt.as_ref()
    }

    pub fn set_notice(&mut self, notice: Option<Notice>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    // --- The list ----------------------------------------------------------------------

    /// Rebuild the rows from the store, the filter and the collapsed sections. The
    /// selection stays if its command is still listed, else moves to the first one.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let store = commands_of(cx);
        let query = self.filter_text(cx);
        self.rows = build_rows(&store, &query, &self.collapsed);
        let listed =
            |reference: &CommandRef| self.rows.iter().any(|row| row.command() == Some(reference));
        if !self.selected.as_ref().is_some_and(listed) {
            self.selected = self.rows.iter().find_map(Row::command).cloned();
        }
        cx.notify();
    }

    pub fn set_filter(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        self.refresh(cx);
    }

    pub fn toggle_section(&mut self, section: Section, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&section) {
            self.collapsed.insert(section);
        }
        self.refresh(cx);
    }

    pub fn select(&mut self, reference: CommandRef, cx: &mut Context<Self>) {
        self.selected = Some(reference);
        self.scroll_to_selection();
        cx.notify();
    }

    fn selected_index(&self) -> Option<usize> {
        let selected = self.selected.as_ref()?;
        self.rows
            .iter()
            .position(|row| row.command() == Some(selected))
    }

    fn scroll_to_selection(&self) {
        if let Some(ix) = self.selected_index() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    /// Move the selection by `step` commands, skipping section headers.
    fn move_selection(&mut self, forward: bool, cx: &mut Context<Self>) {
        let commands: Vec<&CommandRef> = self.rows.iter().filter_map(Row::command).collect();
        if commands.is_empty() {
            return;
        }
        let at = self
            .selected
            .as_ref()
            .and_then(|selected| commands.iter().position(|c| *c == selected));
        let next = match (at, forward) {
            (None, _) => 0,
            (Some(at), true) => (at + 1).min(commands.len() - 1),
            (Some(at), false) => at.saturating_sub(1),
        };
        self.selected = Some(commands[next].clone());
        self.scroll_to_selection();
        cx.notify();
    }

    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.move_selection(true, cx);
    }

    pub fn select_previous(&mut self, cx: &mut Context<Self>) {
        self.move_selection(false, cx);
    }

    /// Ask the workspace to send `reference`.
    pub fn send(&mut self, reference: CommandRef, cx: &mut Context<Self>) {
        self.selected = Some(reference.clone());
        self.notice = None;
        cx.emit(CommandsPanelEvent::Send(reference));
        cx.notify();
    }

    pub fn send_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(reference) = self.selected.clone() {
            self.send(reference, cx);
        }
    }

    // --- Editing -----------------------------------------------------------------------

    /// Open the editor on `seed` in a dialog.
    pub fn open_editor(&mut self, seed: EditorSeed, window: &mut Window, cx: &mut Context<Self>) {
        let title = seed.title();
        let editor = cx.new(|cx| CommandEditor::new(seed, window, cx));
        self.editor = Some(editor.clone());
        let panel = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let save = editor.clone();
            let closed = panel.clone();
            dialog
                .title(title.clone())
                .w(px(560.))
                .child(editor.clone())
                .footer(DialogButtons::new("Save"))
                // Enter and the Save button; a bad value keeps the dialog open.
                .on_ok(move |_, _, cx| save.update(cx, |editor, cx| editor.save(cx)))
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |panel, cx| {
                            panel.editor = None;
                            cx.notify();
                        })
                        .ok();
                })
        });
        cx.notify();
    }

    /// Edit `reference`. A read-only (bundled) command opens as a copy for a collection
    /// of the user's.
    pub fn edit(&mut self, reference: &CommandRef, window: &mut Window, cx: &mut Context<Self>) {
        let store = commands_of(cx);
        let Some(command) = store.get(reference).cloned() else {
            return;
        };
        let read_only = store
            .collection(&reference.collection)
            .is_some_and(|collection| collection.is_read_only());
        let seed = if read_only {
            EditorSeed::copy_of(reference, command, default_collection(&store))
        } else {
            EditorSeed::edit(reference.clone(), command)
        };
        self.open_editor(seed, window, cx);
    }

    /// Open the editor on a new, empty command.
    pub fn new_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = commands_of(cx);
        self.open_editor(
            EditorSeed::new_command(default_collection(&store)),
            window,
            cx,
        );
    }

    /// Open the editor on a new command that sends `text`, as "save as command" does.
    pub fn save_text_as_command(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = commands_of(cx);
        self.open_editor(
            EditorSeed::from_text(text, default_collection(&store)),
            window,
            cx,
        );
    }

    /// Ask for a name, then write an empty collection with it.
    pub fn new_collection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Collection name"));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.name_prompt = Some(input.clone());
        let panel = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let create = panel.clone();
            let closed = panel.clone();
            dialog
                .title("New collection")
                .w(px(360.))
                .child(Input::new(&input).id("new-collection-name"))
                .footer(DialogButtons::new("Create"))
                .on_ok(move |_, _, cx| {
                    create
                        .update(cx, |panel, cx| panel.confirm_new_collection(cx))
                        .unwrap_or(true)
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |panel, cx| {
                            panel.name_prompt = None;
                            cx.notify();
                        })
                        .ok();
                })
        });
        cx.notify();
    }

    /// Create the collection the name prompt names. Returns whether the prompt can close.
    pub fn confirm_new_collection(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(input) = &self.name_prompt else {
            return true;
        };
        let name = input.read(cx).value().trim().to_owned();
        match create_collection(&commands_of(cx), &name) {
            Ok(()) => {
                self.notice = Some(Notice::info(format!("Created {name}")));
                cx.notify();
                true
            }
            Err(error) => {
                self.notice = Some(Notice::error(format!("Could not create {name:?}: {error}")));
                cx.notify();
                false
            }
        }
    }

    /// Write the collection called `name` to a new collection of the user's.
    pub fn copy_to_user(&mut self, name: &str, cx: &mut Context<Self>) {
        self.notice = Some(match copy_collection(&commands_of(cx), name) {
            Ok(copy) => Notice::info(format!("Copied {name} to {copy}")),
            Err(error) => Notice::error(format!("Could not copy {name}: {error}")),
        });
        cx.notify();
    }

    // --- Actions -----------------------------------------------------------------------

    fn select_next_action(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.select_next(cx);
    }

    fn select_previous_action(
        &mut self,
        _: &SelectPrevious,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_previous(cx);
    }

    fn send_selected_action(&mut self, _: &SendSelected, _: &mut Window, cx: &mut Context<Self>) {
        self.send_selected(cx);
    }

    fn edit_selected_action(
        &mut self,
        _: &EditSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reference) = self.selected.clone() {
            self.edit(&reference, window, cx);
        }
    }

    fn new_command_action(&mut self, _: &NewCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.new_command(window, cx);
    }

    fn new_collection_action(
        &mut self,
        _: &NewCollection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_collection(window, cx);
    }

    // --- Rendering ---------------------------------------------------------------------

    fn render_row(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        // Rows are observable in UI tests (a right click on a row, say).
        self.row_element(ix, row, cx)
            .test_support()
            .into_any_element()
    }

    fn row_element(&self, ix: usize, row: &Row, cx: &mut Context<Self>) -> Stateful<Div> {
        let base = h_flex()
            .id(("command-row", ix))
            .relative()
            .w_full()
            .h(chrome::ROW_HEIGHT)
            .pr_2()
            .gap_1p5()
            .items_center()
            .overflow_hidden();
        match row {
            Row::Collection {
                name,
                read_only,
                collapsed,
                commands,
            } => {
                let section = Section::Collection(name.clone());
                let copy = name.clone();
                let theme = cx.theme();
                let (muted, hover) = (theme.muted_foreground, theme.list_hover);
                base.pl_3()
                    .cursor_pointer()
                    .hover(|style| style.bg(hover))
                    .child(fold_icon(*collapsed, muted))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(chrome::section_label(name, cx)),
                    )
                    .when(*read_only, |row| {
                        row.child(chrome::quiet_chip("examples", cx))
                    })
                    .child(div().flex_1())
                    .when(*read_only, |row| {
                        row.child(
                            chrome::icon_button(("copy-collection", ix), IconName::Copy, cx)
                                .xsmall()
                                .tooltip("Copy these commands to a collection of your own")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Not the row's click, which folds the section.
                                    cx.stop_propagation();
                                    this.copy_to_user(&copy, cx);
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(chrome::LABEL_SIZE)
                            .text_color(muted)
                            .child(SharedString::from(commands.to_string())),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_section(section.clone(), cx);
                    }))
            }
            Row::Group {
                collection,
                name,
                collapsed,
            } => {
                let section = Section::Group {
                    collection: collection.clone(),
                    group: name.clone(),
                };
                let theme = cx.theme();
                base.pl(px(24.))
                    .cursor_pointer()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .hover(|style| style.bg(theme.list_hover))
                    .child(fold_icon(*collapsed, theme.muted_foreground))
                    .child(div().truncate().child(SharedString::from(name.clone())))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_section(section.clone(), cx);
                    }))
            }
            Row::Command {
                reference,
                keybinding,
                description,
                read_only,
                location,
            } => {
                let selected = self.selected.as_ref() == Some(reference);
                let send = reference.clone();
                let send_button = reference.clone();
                let edit = reference.clone();
                let edit_button = reference.clone();
                let tooltip = SharedString::from(if description.is_empty() {
                    reference.to_string()
                } else {
                    description.clone()
                });
                let group = SharedString::from(format!("command-row-{ix}"));
                let overlay = chrome::overlay_background(selected, cx);
                let (row_background, active_border, hover, transparent) = {
                    let theme = cx.theme();
                    (
                        theme.list_active,
                        theme.list_active_border,
                        theme.list_hover,
                        theme.transparent,
                    )
                };
                // Send and edit come up over the row's right end while it is hovered
                // (or selected).
                let actions = h_flex()
                    .id(("command-actions", ix))
                    .absolute()
                    .top_0()
                    .right_0()
                    .h_full()
                    .pl_4()
                    .pr_1()
                    .gap_0p5()
                    .items_center()
                    .bg(overlay)
                    .opacity(if selected { 1. } else { 0. })
                    .group_hover(group.clone(), |style| style.opacity(1.))
                    .child(
                        chrome::icon_button(("send-command", ix), IconName::SendHorizontal, cx)
                            .xsmall()
                            .tooltip_with_action(
                                "Send",
                                &SendSelected,
                                Some(context::COMMANDS_PANEL),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&this.focus_handle, cx);
                                this.send(send_button.clone(), cx);
                            })),
                    )
                    .child(
                        chrome::icon_button(("edit-command", ix), IconName::Pencil, cx)
                            .xsmall()
                            .tooltip(if *read_only {
                                "Edit a copy in a collection of your own"
                            } else {
                                "Edit this command"
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // Not the row's click, which sends the command.
                                cx.stop_propagation();
                                this.edit(&edit_button, window, cx);
                            })),
                    );
                let theme = cx.theme();
                let mono = theme.mono_font_family.clone();
                base.group(group)
                    .pl(px(36.))
                    .border_l_2()
                    .map(|row| {
                        if selected {
                            row.bg(row_background).border_color(active_border)
                        } else {
                            row.border_color(transparent).hover(|style| style.bg(hover))
                        }
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(relative(0.7))
                            .truncate()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(SharedString::from(reference.name.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(location.clone().map(SharedString::from)),
                    )
                    .children(keybinding.clone().map(|keys| {
                        chrome::quiet_chip(keybinding_text(&keys), cx).font_family(mono)
                    }))
                    .child(actions)
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.send(send.clone(), cx);
                    }))
                    .on_aux_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.is_right_click() {
                            this.select(edit.clone(), cx);
                            this.edit(&edit, window, cx);
                        }
                    }))
            }
        }
    }
}

/// A section's fold chevron.
fn fold_icon(collapsed: bool, color: Hsla) -> Icon {
    Icon::new(if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    })
    .size_3()
    .text_color(color)
}

/// A command's keybinding as the platform writes it: `cmd-1` reads `⌘1` on macOS.
fn keybinding_text(keys: &str) -> String {
    keys.split_whitespace()
        .map(|key| Keystroke::parse(key).map_or_else(|_| key.to_owned(), |key| Kbd::format(&key)))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Render for CommandsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = cx.entity().downgrade();
        let header = chrome::panel_header("Commands", cx).child(
            div().ml_auto().child(
                chrome::icon_button("commands-add", IconName::Plus, cx)
                    .tooltip("New command or collection")
                    .dropdown_menu_with_anchor(Anchor::TopRight, move |menu, _, _| {
                        let (command, collection) = (panel.clone(), panel.clone());
                        menu.min_w(px(200.))
                            .item(
                                PopupMenuItem::new("New command\u{2026}")
                                    .icon(IconName::Plus)
                                    .action(Box::new(NewCommand))
                                    .on_click(move |_, window, cx| {
                                        command
                                            .update(cx, |panel, cx| panel.new_command(window, cx))
                                            .ok();
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("New collection\u{2026}")
                                    .icon(IconName::ListPlus)
                                    .action(Box::new(NewCollection))
                                    .on_click(move |_, window, cx| {
                                        collection
                                            .update(cx, |panel, cx| {
                                                panel.new_collection(window, cx)
                                            })
                                            .ok();
                                    }),
                            )
                    }),
            ),
        );
        let theme = cx.theme();
        let filter = div().flex_none().px_2().pb_1().child(
            Input::new(&self.filter)
                .id("commands-filter")
                .small()
                .prefix(Icon::new(IconName::Search).text_color(theme.muted_foreground)),
        );
        let body = if self.rows.is_empty() {
            v_flex()
                .flex_1()
                .px_3()
                .py_2()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if self.filter_text(cx).trim().is_empty() {
                    "No saved commands."
                } else {
                    "No command matches."
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "command-list",
                        self.rows.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                            let rows: Vec<Row> = this.rows[range.clone()].to_vec();
                            range
                                .zip(rows.iter())
                                .map(|(ix, row)| this.render_row(ix, row, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .track_scroll(&self.scroll)
                    .size_full(),
                )
                .into_any_element()
        };
        let theme = cx.theme();
        v_flex()
            .id("commands-panel")
            .key_context(context::COMMANDS_PANEL)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next_action))
            .on_action(cx.listener(Self::select_previous_action))
            .on_action(cx.listener(Self::send_selected_action))
            .on_action(cx.listener(Self::edit_selected_action))
            .on_action(cx.listener(Self::new_command_action))
            .on_action(cx.listener(Self::new_collection_action))
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(header)
            .child(filter)
            .child(body)
            .children(self.notice.clone().map(|notice| {
                div()
                    .flex_none()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(if notice.is_error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(SharedString::from(notice.text))
            }))
    }
}

// --- The editor --------------------------------------------------------------------

/// What the editor opens with.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorSeed {
    /// The command being edited; `None` for a new one (or a copy).
    pub target: Option<CommandRef>,
    pub collection: String,
    pub group: String,
    pub command: Command,
}

impl EditorSeed {
    /// Edit `command`, which `target` names.
    pub fn edit(target: CommandRef, command: Command) -> Self {
        Self {
            collection: target.collection.clone(),
            group: target.group.clone(),
            target: Some(target),
            command,
        }
    }

    /// A new command, empty, for `collection`.
    pub fn new_command(collection: String) -> Self {
        Self {
            target: None,
            collection,
            group: DEFAULT_GROUP.to_owned(),
            command: Command::text("", ""),
        }
    }

    /// A new command sending `text`, named after it.
    pub fn from_text(text: &str, collection: String) -> Self {
        let name: String = text.trim().chars().take(40).collect();
        Self {
            target: None,
            collection,
            group: SAVED_GROUP.to_owned(),
            command: Command::text(name, text),
        }
    }

    /// A copy of a read-only command, to save into `collection`.
    pub fn copy_of(reference: &CommandRef, command: Command, collection: String) -> Self {
        Self {
            target: None,
            collection,
            group: reference.group.clone(),
            command,
        }
    }

    fn title(&self) -> SharedString {
        match &self.target {
            Some(target) => format!("Edit {}", target.name).into(),
            None if self.command.name.is_empty() => "New command".into(),
            None => format!("Save {} as a command", self.command.name).into(),
        }
    }
}

/// Whether the payload field holds text or hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    Text,
    Hex,
}

/// The fields of the editor, for tests and for [`CommandEditor::set_field`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Collection,
    Group,
    Description,
    Payload,
    ExpectPattern,
    ExpectTimeout,
    Keybinding,
}

/// The command form: name, collection and group, description, payload (text or hex),
/// line ending, expected reply and its timeout, and keybinding. Parameters are kept as
/// the command has them (edit the file for those).
pub struct CommandEditor {
    target: Option<CommandRef>,
    /// Kept from the command being edited.
    params: Vec<serialist_core::Param>,
    /// A codec or script payload the form cannot edit, kept as it is.
    codec: Option<Payload>,
    /// A frame predicate the command waits for, which the form cannot edit, kept as it
    /// is (with the form's timeout).
    expect_frame: Option<serde_json::Map<String, serde_json::Value>>,
    name: Entity<InputState>,
    collection: Entity<InputState>,
    group: Entity<InputState>,
    description: Entity<InputState>,
    payload: Entity<InputState>,
    payload_kind: PayloadKind,
    /// `None` uses the session's line ending (text) or none (hex).
    eol: Option<LineEnding>,
    expect_pattern: Entity<InputState>,
    expect_timeout: Entity<InputState>,
    keybinding: Entity<InputState>,
    error: Option<String>,
    /// Where the last save went.
    saved: Option<CommandRef>,
}

impl CommandEditor {
    pub fn new(seed: EditorSeed, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let command = seed.command;
        let field =
            |value: &str, placeholder: &str, window: &mut Window, cx: &mut Context<Self>| {
                let value = value.to_owned();
                let placeholder = placeholder.to_owned();
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .placeholder(placeholder)
                        .default_value(value)
                })
            };
        let (payload_text, payload_kind, codec) = match &command.payload {
            Payload::Text(text) => (text.clone(), PayloadKind::Text, None),
            Payload::Hex(hex) => (hex.clone(), PayloadKind::Hex, None),
            kept @ (Payload::Codec { .. } | Payload::Script { .. }) => {
                (String::new(), PayloadKind::Text, Some(kept.clone()))
            }
        };
        let (pattern, timeout) = match &command.expect {
            Some(expect) => (expect.pattern.clone(), expect.timeout_ms.to_string()),
            None => (String::new(), String::new()),
        };
        let name = field(&command.name, "Name", window, cx);
        let editor = Self {
            target: seed.target,
            params: command.params.clone(),
            codec,
            expect_frame: command.expect.as_ref().and_then(|e| e.frame.clone()),
            collection: field(&seed.collection, "Collection", window, cx),
            group: field(&seed.group, "Group", window, cx),
            description: field(
                &command.description,
                "Description (shown as a tooltip)",
                window,
                cx,
            ),
            payload: field(&payload_text, "AT+VER?  or  05 5A 02 00 {{id}}", window, cx),
            payload_kind,
            eol: command.eol,
            expect_pattern: field(
                &pattern,
                "Reply to wait for (regex), e.g. ^OK|^ERROR",
                window,
                cx,
            ),
            expect_timeout: field(
                &timeout,
                &format!("{DEFAULT_EXPECT_TIMEOUT_MS} ms"),
                window,
                cx,
            ),
            keybinding: field(
                command.keybinding.as_deref().unwrap_or_default(),
                "Keybinding, e.g. cmd-1",
                window,
                cx,
            ),
            name,
            error: None,
            saved: None,
        };
        editor.name.update(cx, |input, cx| input.focus(window, cx));
        editor
    }

    fn input(&self, field: Field) -> &Entity<InputState> {
        match field {
            Field::Name => &self.name,
            Field::Collection => &self.collection,
            Field::Group => &self.group,
            Field::Description => &self.description,
            Field::Payload => &self.payload,
            Field::ExpectPattern => &self.expect_pattern,
            Field::ExpectTimeout => &self.expect_timeout,
            Field::Keybinding => &self.keybinding,
        }
    }

    pub fn field(&self, field: Field, cx: &App) -> String {
        self.input(field).read(cx).value().to_string()
    }

    pub fn set_field(
        &mut self,
        field: Field,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input(field)
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    pub fn payload_kind(&self) -> PayloadKind {
        self.payload_kind
    }

    pub fn set_payload_kind(&mut self, kind: PayloadKind, cx: &mut Context<Self>) {
        self.payload_kind = kind;
        cx.notify();
    }

    pub fn eol(&self) -> Option<LineEnding> {
        self.eol
    }

    pub fn set_eol(&mut self, eol: Option<LineEnding>, cx: &mut Context<Self>) {
        self.eol = eol;
        cx.notify();
    }

    /// The problem that stopped the last save.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The command the last successful save wrote.
    pub fn saved(&self) -> Option<&CommandRef> {
        self.saved.as_ref()
    }

    /// The collection, group and command the form describes, or what is wrong with it.
    pub fn build(&self, cx: &App) -> Result<(String, String, Command), String> {
        let text = |field| self.field(field, cx).trim().to_owned();
        let name = text(Field::Name);
        if name.is_empty() {
            return Err("A command needs a name".to_owned());
        }
        let collection = text(Field::Collection);
        if collection.is_empty() {
            return Err("A command needs a collection".to_owned());
        }
        let group = match text(Field::Group) {
            group if group.is_empty() => DEFAULT_GROUP.to_owned(),
            group => group,
        };
        let payload = match (&self.codec, self.payload_kind) {
            (Some(codec), _) => codec.clone(),
            // The payload keeps its spaces: they may be meant.
            (None, PayloadKind::Text) => Payload::Text(self.field(Field::Payload, cx)),
            (None, PayloadKind::Hex) => Payload::Hex(text(Field::Payload)),
        };
        let pattern = self.field(Field::ExpectPattern, cx);
        let expect = if pattern.trim().is_empty() && self.expect_frame.is_none() {
            None
        } else {
            let timeout = text(Field::ExpectTimeout);
            let timeout_ms = if timeout.is_empty() {
                DEFAULT_EXPECT_TIMEOUT_MS
            } else {
                timeout
                    .trim_end_matches("ms")
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| {
                        format!("The timeout {timeout:?} is not a number of milliseconds")
                    })?
            };
            let pattern = if pattern.trim().is_empty() {
                String::new()
            } else {
                pattern
            };
            Some(Expect {
                frame: self.expect_frame.clone(),
                ..Expect::new(pattern, timeout_ms)
            })
        };
        let keybinding = match text(Field::Keybinding) {
            keys if keys.is_empty() => None,
            keys => {
                for stroke in keys.split_whitespace() {
                    Keystroke::parse(stroke)
                        .map_err(|_| format!("The keybinding {keys:?} is not a keystroke"))?;
                }
                Some(keys)
            }
        };
        let command = Command {
            name,
            description: text(Field::Description),
            payload,
            eol: self.eol,
            expect,
            keybinding,
            params: self.params.clone(),
        };
        if let Some(problem) = command.problems().into_iter().next() {
            return Err(problem);
        }
        Ok((collection, group, command))
    }

    /// Write the command to its collection's file. Returns whether it saved; if not,
    /// [`Self::error`] says why. The store in memory is left alone: the config watcher's
    /// reload brings the change in.
    pub fn save(&mut self, cx: &mut Context<Self>) -> bool {
        let result = self.build(cx).and_then(|(collection, group, command)| {
            let store = commands_of(cx);
            write_command(&store, self.target.as_ref(), &collection, &group, command)
        });
        match result {
            Ok(reference) => {
                tracing::info!(command = %reference, "saved command");
                self.saved = Some(reference);
                self.error = None;
                cx.notify();
                true
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                false
            }
        }
    }
}

/// Put `command` into `group` of `collection` in a copy of `store`, replacing `target`
/// if given (moving it if its collection or group changed), and write the collections
/// that changed. A collection that does not exist yet is created.
pub fn write_command(
    store: &CommandStore,
    target: Option<&CommandRef>,
    collection: &str,
    group: &str,
    command: Command,
) -> Result<CommandRef, String> {
    let mut store = store.clone();
    let text = |error: serialist_core::EditError| error.to_string();
    match store.collection(collection) {
        None => {
            store.create_collection(collection).map_err(text)?;
        }
        Some(existing) if existing.is_read_only() => {
            return Err(format!(
                "{collection} is read-only; save into a collection of your own"
            ));
        }
        Some(_) => {}
    }
    let reference = match target {
        Some(target) if target.collection == collection && target.group == group => {
            store.update_command(target, command).map_err(text)?
        }
        Some(target) => {
            store.remove_command(target).map_err(text)?;
            let added = store
                .add_command(collection, group, command)
                .map_err(text)?;
            if target.collection != collection {
                store
                    .save_collection(&target.collection)
                    .map_err(|error| error.to_string())?;
            }
            added
        }
        None => store
            .add_command(collection, group, command)
            .map_err(text)?,
    };
    store
        .save_collection(collection)
        .map_err(|error| error.to_string())?;
    Ok(reference)
}

fn eol_label(eol: Option<LineEnding>) -> &'static str {
    match eol {
        None => "Session EOL",
        Some(eol) => eol.label(),
    }
}

fn next_eol(eol: Option<LineEnding>) -> Option<LineEnding> {
    match eol {
        None => Some(LineEnding::None),
        Some(LineEnding::None) => Some(LineEnding::Cr),
        Some(LineEnding::Cr) => Some(LineEnding::Lf),
        Some(LineEnding::Lf) => Some(LineEnding::Crlf),
        Some(LineEnding::Crlf) => None,
    }
}

impl Render for CommandEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let label = |text: &'static str| {
            div()
                .w(px(96.))
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let row = |text: &'static str, input: &Entity<InputState>, id: &'static str| {
            h_flex()
                .gap_2()
                .child(label(text))
                .child(div().flex_1().child(Input::new(input).id(id).small()))
        };
        let hex = self.payload_kind == PayloadKind::Hex;
        v_flex()
            .id("command-editor")
            .gap_2()
            .child(row("Name", &self.name, "command-name"))
            .child(row("Collection", &self.collection, "command-collection"))
            .child(row("Group", &self.group, "command-group"))
            .child(row("Description", &self.description, "command-description"))
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Payload"))
                    .child(
                        div()
                            .flex_1()
                            .font_family(theme.mono_font_family.clone())
                            .child(Input::new(&self.payload).id("command-payload").small()),
                    )
                    .child(
                        Button::new("command-payload-kind")
                            .label(if hex { "Hex" } else { "Text" })
                            .tooltip(
                                "Text (with \\r \\n \\xNN escapes) or hex digits; click to switch",
                            )
                            .small()
                            .ghost()
                            .disabled(self.codec.is_some())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let kind = if hex {
                                    PayloadKind::Text
                                } else {
                                    PayloadKind::Hex
                                };
                                this.set_payload_kind(kind, cx);
                            })),
                    )
                    .child(
                        Button::new("command-eol")
                            .label(eol_label(self.eol))
                            .tooltip("Line ending after the payload; click to cycle")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_eol(next_eol(this.eol), cx);
                            })),
                    ),
            )
            .child(row("Expect", &self.expect_pattern, "command-expect"))
            .child(row("Timeout (ms)", &self.expect_timeout, "command-timeout"))
            .child(row("Keybinding", &self.keybinding, "command-keybinding"))
            .when(!self.params.is_empty(), |form| {
                let names: Vec<&str> = self.params.iter().map(|p| p.name.as_str()).collect();
                form.child(div().text_xs().text_color(theme.muted_foreground).child(
                    SharedString::from(format!(
                        "Parameters (asked for when sent): {}",
                        names.join(", ")
                    )),
                ))
            })
            .children(self.error.clone().map(|error| {
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(SharedString::from(error))
            }))
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::{CollectionSource, CommandCollection};

    use super::*;

    fn store() -> CommandStore {
        let mut store = CommandStore::empty();
        let mut mine =
            CommandCollection::new("Mine", CollectionSource::User("/tmp/mine.json".into()));
        mine.groups.push(serialist_core::CommandGroup {
            name: "Modem".into(),
            commands: vec![
                Command::text("Version", "AT+VER?").with_keybinding("cmd-1"),
                Command::text("Reset", "ATZ").with_description("Restart the modem"),
            ],
        });
        store.set_collection(mine);
        store.set_collection(CommandCollection::bundled_examples());
        store
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                Row::Collection { name, .. } => format!("# {name}"),
                Row::Group { name, .. } => format!("## {name}"),
                Row::Command { reference, .. } => reference.name.clone(),
            })
            .collect()
    }

    #[test]
    fn rows_list_sections_then_commands_and_fold() {
        let store = store();
        let rows = build_rows(&store, "", &HashSet::new());
        assert_eq!(
            names(&rows)[..4],
            ["# Mine", "## Modem", "Version", "Reset"].map(str::to_owned)
        );
        assert!(names(&rows).contains(&"# AT basics".to_owned()));
        let Row::Command { keybinding, .. } = &rows[2] else {
            panic!("a command row");
        };
        assert_eq!(keybinding.as_deref(), Some("cmd-1"));
        let bundled = rows
            .iter()
            .find(|row| matches!(row, Row::Collection { name, .. } if name == "AT basics"))
            .unwrap();
        assert!(matches!(
            bundled,
            Row::Collection {
                read_only: true,
                ..
            }
        ));

        let collapsed = HashSet::from([
            Section::Group {
                collection: "Mine".into(),
                group: "Modem".into(),
            },
            Section::Collection("AT basics".into()),
        ]);
        let rows = build_rows(&store, "", &collapsed);
        assert_eq!(names(&rows), ["# Mine", "## Modem", "# AT basics"]);
    }

    #[test]
    fn a_query_lists_matches_best_first_with_their_place() {
        let rows = build_rows(&store(), "ver", &HashSet::new());
        assert_eq!(names(&rows)[0], "Version");
        let Row::Command { location, .. } = &rows[0] else {
            panic!("a command row");
        };
        assert_eq!(location.as_deref(), Some("Mine \u{203a} Modem"));
        assert!(
            rows.iter().all(|row| row.command().is_some()),
            "no headers while filtering"
        );
        assert!(build_rows(&store(), "zzzz", &HashSet::new()).is_empty());
    }

    #[test]
    fn new_commands_go_to_the_first_collection_of_the_users() {
        assert_eq!(default_collection(&store()), "Mine");
        let mut only_bundled = CommandStore::empty();
        only_bundled.set_collection(CommandCollection::bundled_examples());
        assert_eq!(default_collection(&only_bundled), DEFAULT_COLLECTION);
        assert_eq!(copy_name(&store(), "AT basics"), "AT basics (copy)");
    }

    #[test]
    fn saving_into_the_bundled_collection_is_refused() {
        let error = write_command(
            &store(),
            None,
            "AT basics",
            "Basics",
            Command::text("Mine", "AT"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "AT basics is read-only; save into a collection of your own"
        );
    }

    #[test]
    fn seeds_name_and_place_the_command() {
        let seed = EditorSeed::from_text("AT+CSQ", "Mine".into());
        assert_eq!(seed.command.name, "AT+CSQ");
        assert_eq!(seed.command.payload, Payload::Text("AT+CSQ".into()));
        assert_eq!(seed.group, SAVED_GROUP);
        assert_eq!(seed.target, None);
        let reference = CommandRef::new("AT basics", "Basics", "AT");
        let copy = EditorSeed::copy_of(&reference, Command::text("AT", "AT"), "Mine".into());
        assert_eq!(
            (copy.collection.as_str(), copy.group.as_str()),
            ("Mine", "Basics")
        );
        assert_eq!(next_eol(None), Some(LineEnding::None));
        assert_eq!(next_eol(Some(LineEnding::Crlf)), None);
    }
}
