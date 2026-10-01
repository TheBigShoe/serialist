//! The Keymap section: every binding in effect, by context, from the resolved keymap (the
//! bundled defaults, then the user's `keymap.json`), marked where the user's file binds
//! the key and where that overrides a bundled binding. A filter narrows the list.
//!
//! The list is read-only except for one edit: "Rebind…" on the selected row records a
//! keystroke and writes it to `keymap.json` through the comment-preserving keymap
//! editor, taking the old chord off the action (unbinding a bundled chord with `null`,
//! or removing the user's own entry). Anything else is done in the file ("Edit
//! keymap.json"). Single keystrokes only; a sequence is typed into the file.

use serialist_core::{ActionRef, Keymap};

use super::SettingsView;
use crate::actions;
use crate::chrome;
use crate::config::Config;
use crate::keystroke_input::{KeystrokeInput, KeystrokeInputEvent};
use crate::palette::humanize;
use crate::prelude::*;
use crate::settings_io;

/// Where a binding in effect comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingSource {
    /// The bundled keymap.
    Default,
    /// The user's `keymap.json`, for a chord the bundled keymap does not bind there.
    User,
    /// The user's `keymap.json`, replacing a bundled binding of the same chord.
    Overrides,
}

impl BindingSource {
    fn label(self) -> &'static str {
        match self {
            BindingSource::Default => "default",
            BindingSource::User => "user",
            BindingSource::Overrides => "overrides default",
        }
    }
}

/// One binding in effect.
#[derive(Clone, Debug, PartialEq)]
pub struct BindingRow {
    pub context: Option<String>,
    pub keystrokes: String,
    pub action: ActionRef,
    pub source: BindingSource,
}

impl BindingRow {
    fn key(&self) -> (Option<String>, String) {
        (self.context.clone(), self.keystrokes.clone())
    }
}

/// The bindings in effect in `keymap`, whose first `bundled` entries are the bundled
/// defaults, grouped by context (no context first) and kept in keymap order within one,
/// narrowed to those matching every word of `query` (keystrokes, action or context).
pub fn binding_rows(keymap: &Keymap, bundled: usize, query: &str) -> Vec<BindingRow> {
    use std::collections::HashMap;
    let mut last = HashMap::new();
    for (index, entry) in keymap.entries.iter().enumerate() {
        last.insert((entry.context.as_deref(), entry.keystrokes.as_str()), index);
    }
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut rows: Vec<BindingRow> = keymap
        .entries
        .iter()
        .enumerate()
        .filter(|(index, entry)| {
            last.get(&(entry.context.as_deref(), entry.keystrokes.as_str())) == Some(index)
        })
        .filter_map(|(index, entry)| {
            let action = entry.action.clone()?;
            let source = if index < bundled {
                BindingSource::Default
            } else if keymap.entries[..bundled.min(keymap.entries.len())]
                .iter()
                .any(|bundled| {
                    bundled.context == entry.context && bundled.keystrokes == entry.keystrokes
                })
            {
                BindingSource::Overrides
            } else {
                BindingSource::User
            };
            Some(BindingRow {
                context: entry.context.clone(),
                keystrokes: entry.keystrokes.clone(),
                action,
                source,
            })
        })
        .filter(|row| {
            let haystack = format!(
                "{} {} {} {}",
                row.keystrokes,
                row.action.name,
                humanize(&row.action.name),
                row.context.as_deref().unwrap_or("global")
            )
            .to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect();
    // Stable, so each context keeps the keymap's order.
    rows.sort_by(|a, b| a.context.cmp(&b.context));
    rows
}

/// The Keymap section's state.
pub(super) struct KeymapState {
    query: Entity<InputState>,
    /// The selected binding, by context and keystrokes.
    selected: Option<(Option<String>, String)>,
    recorder: Entity<KeystrokeInput>,
    /// The binding "Rebind…" is recording a chord for.
    rebinding: Option<BindingRow>,
    error: Option<String>,
    /// How many entries the bundled keymap has, so the rest are the user's.
    bundled: usize,
}

impl KeymapState {
    pub(super) fn new(
        window: &mut Window,
        cx: &mut Context<SettingsView>,
        subscriptions: &mut Vec<Subscription>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Filter bindings"));
        subscriptions.push(
            cx.subscribe_in(&query, window, |_, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        );
        let recorder = cx.new(|cx| KeystrokeInput::new("settings-rebind-keystroke", None, cx));
        subscriptions.push(cx.subscribe_in(
            &recorder,
            window,
            |this, _, event: &KeystrokeInputEvent, window, cx| match event {
                KeystrokeInputEvent::Captured(keystroke) => {
                    this.rebind_selected(
                        &crate::keystroke_input::keystroke_text(keystroke),
                        window,
                        cx,
                    );
                }
            },
        ));
        Self {
            query,
            selected: None,
            recorder,
            rebinding: None,
            error: None,
            bundled: Keymap::bundled_default().entries.len(),
        }
    }

    pub(super) fn stop_recording(&mut self, cx: &mut App) {
        self.rebinding = None;
        self.recorder.update(cx, |recorder, cx| recorder.stop(cx));
    }
}

/// Keystrokes as keycaps, each stroke of a sequence its own.
fn keycaps(keystrokes: &str) -> Div {
    h_flex()
        .gap_1()
        .children(keystrokes.split_whitespace().map(|stroke| {
            match Keystroke::parse(stroke) {
                Ok(keystroke) => Kbd::new(keystroke).into_any_element(),
                Err(_) => div()
                    .child(SharedString::from(stroke.to_owned()))
                    .into_any_element(),
            }
        }))
}

impl SettingsView {
    /// The bindings the Keymap section lists now, filtered by its query.
    pub fn binding_rows_now(&self, cx: &App) -> Vec<BindingRow> {
        let query = self.keymap.query.read(cx).value().to_string();
        cx.try_global::<Config>()
            .map(|config| binding_rows(config.keymap(), self.keymap.bundled, &query))
            .unwrap_or_default()
    }

    /// The Keymap section's filter field.
    pub fn keymap_query(&self) -> &Entity<InputState> {
        &self.keymap.query
    }

    /// The Keymap section's keystroke recorder.
    pub fn rebind_recorder(&self) -> &Entity<KeystrokeInput> {
        &self.keymap.recorder
    }

    /// What the last rebind said, if it failed.
    pub fn keymap_error(&self) -> Option<&str> {
        self.keymap.error.as_deref()
    }

    /// Select the binding of `keystrokes` in `context`.
    pub fn select_binding(
        &mut self,
        context: Option<&str>,
        keystrokes: &str,
        cx: &mut Context<Self>,
    ) {
        self.keymap.selected = Some((context.map(str::to_owned), keystrokes.to_owned()));
        self.keymap.stop_recording(cx);
        cx.notify();
    }

    fn selected_binding(&self, cx: &App) -> Option<BindingRow> {
        let selected = self.keymap.selected.as_ref()?;
        let keymap = cx.try_global::<Config>()?.keymap().clone();
        binding_rows(&keymap, self.keymap.bundled, "")
            .into_iter()
            .find(|row| &row.key() == selected)
    }

    /// Record a new chord for the selected binding: the next keystroke pressed.
    pub fn start_rebind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_binding(cx) else {
            return;
        };
        self.keymap.error = None;
        self.keymap.rebinding = Some(row);
        self.keymap.recorder.update(cx, |recorder, cx| {
            recorder.set_keystroke(None, cx);
            recorder.start(window, cx);
        });
        cx.notify();
    }

    /// Bind the selected binding's action to `keystrokes` instead, in `keymap.json`.
    pub fn rebind_selected(
        &mut self,
        keystrokes: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .keymap
            .rebinding
            .take()
            .or_else(|| self.selected_binding(cx))
        else {
            return;
        };
        // The recorder leaves the row with the recording; keep the focus in the screen
        // so the window's bindings (the new chord among them) still reach it.
        window.focus(&self.focus_handle, cx);
        let user_owned = row.source != BindingSource::Default;
        let paths = self.paths(cx);
        match settings_io::rebind(
            &paths,
            row.context.as_deref(),
            keystrokes,
            &row.action,
            Some((&row.keystrokes, user_owned)),
        ) {
            Ok(()) => {
                self.keymap.error = None;
                self.keymap.selected = Some((row.context.clone(), keystrokes.to_owned()));
            }
            Err(message) => self.keymap.error = Some(message),
        }
        cx.notify();
    }

    pub(super) fn render_keymap(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let rows = self.binding_rows_now(cx);
        let theme = cx.theme();
        let (muted, active, hover, info, danger) = (
            theme.muted_foreground,
            theme.list_active,
            theme.list_hover,
            theme.info,
            theme.danger,
        );
        let selected = self.keymap.selected.clone();
        let recording = self.keymap.rebinding.is_some();
        let mut list: Vec<AnyElement> = Vec::new();
        let mut context: Option<Option<String>> = None;
        for (ix, row) in rows.iter().enumerate() {
            if context.as_ref() != Some(&row.context) {
                context = Some(row.context.clone());
                let title = row.context.clone().unwrap_or_else(|| "Global".to_owned());
                list.push(
                    div()
                        .pt_3()
                        .pb_1()
                        .child(chrome::section_label(&title, cx))
                        .into_any_element(),
                );
            }
            let is_selected = selected.as_ref() == Some(&row.key());
            let key = row.key();
            let source = match row.source {
                BindingSource::Default => chrome::quiet_chip(row.source.label(), cx),
                _ => chrome::chip(info).child(row.source.label()),
            };
            let action = row.action.name.clone();
            list.push(
                h_flex()
                    .id(("settings-binding", ix))
                    .test_support()
                    .w_full()
                    .h(chrome::ROW_HEIGHT)
                    .px_2()
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .when(is_selected, |this| this.bg(active))
                    .when(!is_selected, |this| this.hover(|this| this.bg(hover)))
                    .child(
                        div()
                            .w(px(150.))
                            .flex_none()
                            .child(keycaps(&row.keystrokes)),
                    )
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .child(SharedString::from(humanize(&action))),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(muted)
                                    .child(SharedString::from(action.clone())),
                            ),
                    )
                    .child(source)
                    .when(is_selected, |this| {
                        if recording {
                            this.child(self.keymap.recorder.clone())
                        } else {
                            this.child(
                                Button::new("settings-rebind")
                                    .label("Rebind\u{2026}")
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.start_rebind(window, cx);
                                    })),
                            )
                        }
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.keymap.selected.as_ref() != Some(&key) {
                            this.select_binding(key.0.as_deref(), &key.1, cx);
                        }
                    }))
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            list.push(
                div()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child("No binding matches the filter.")
                    .into_any_element(),
            );
        }
        let toolbar = h_flex()
            .pt_4()
            .gap_2()
            .items_center()
            .child(
                div().flex_1().max_w(px(320.)).child(
                    Input::new(&self.keymap.query)
                        .id("settings-keymap-filter")
                        .small(),
                ),
            )
            .child(
                Button::new("settings-edit-keymap")
                    .icon(IconName::FileCode)
                    .label("Edit keymap.json")
                    .small()
                    .on_click(|_, _, cx| actions::open_keymap(cx)),
            );
        let note = self
            .keymap
            .error
            .clone()
            .map(|error| (error, danger))
            .or_else(|| {
                recording.then(|| {
                    (
                        "Press the new chord for the selected binding (click the box to cancel)"
                            .to_owned(),
                        muted,
                    )
                })
            });
        vec![
            toolbar.into_any_element(),
            div()
                .children(note.map(|(text, color)| {
                    div()
                        .id("settings-keymap-note")
                        .test_support()
                        .pt_1()
                        .text_xs()
                        .text_color(color)
                        .child(SharedString::from(text))
                }))
                .into_any_element(),
            v_flex().w_full().children(list).into_any_element(),
        ]
    }
}
