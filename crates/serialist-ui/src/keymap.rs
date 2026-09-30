//! Turning a Zed-format [`Keymap`] into GPUI key bindings.
//!
//! GPUI builds any registered action from its name (`cx.build_action("terminal::Clear",
//! args)`): every `actions!` or `#[derive(Action)]` type registers itself under its
//! `namespace::Name` at startup, so a keymap file can name any of the app's actions,
//! and gpui-kit's own. A `null` entry becomes GPUI's `NoAction`, which hides the
//! bindings of the same keystrokes that it outranks, the same rule Zed applies.
//!
//! GPUI has no way to remove some bindings and keep others, so a reload clears the
//! whole keymap and binds again: first gpui-kit's own bindings (snapshotted once, right
//! after `gpui_kit::init`), then the bundled defaults, then the user's file, then the
//! saved commands' keybindings. Nothing is bound twice however often a file is saved.
//!
//! A saved command's `keybinding` becomes a [`commands::Send`](crate::actions::commands::Send)
//! binding in the `Workspace` context, so it works wherever the focus is in the window.
//! Keystrokes some other binding already uses are still bound (the command wins where
//! both apply at the same depth) and reported, so the status line can say which.

use std::rc::Rc;

use serialist_core::{CommandStore, Keymap};

use crate::actions::{self, context};
use crate::prelude::*;

/// gpui-kit's bindings as they stood after its `init`, before any of the app's.
#[derive(Clone, Default)]
pub struct KitBindings(pub Vec<KeyBinding>);

impl Global for KitBindings {}

/// Remember the bindings registered so far as gpui-kit's. Call once, right after
/// `gpui_kit::init` and before the app binds anything.
pub fn snapshot_kit_bindings(cx: &mut App) {
    let bindings: Vec<KeyBinding> = cx.key_bindings().borrow().bindings().cloned().collect();
    cx.set_global(KitBindings(bindings));
}

/// GPUI bindings for `keymap`, in its order, and a message for every entry that could
/// not be used (an action no one registered, arguments the action rejects, a keystroke
/// or context that does not parse). The rest still bind.
pub fn gpui_bindings(keymap: &Keymap, cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = Vec::with_capacity(keymap.entries.len());
    let mut problems = Vec::new();
    for entry in &keymap.entries {
        let where_ = match &entry.context {
            Some(context) => format!("\"{}\" in {context}", entry.keystrokes),
            None => format!("\"{}\"", entry.keystrokes),
        };
        let (action, input): (Box<dyn Action>, Option<SharedString>) = match &entry.action {
            None => (Box::new(NoAction), None),
            Some(action) => match cx.build_action(&action.name, action.args.clone()) {
                Ok(built) => (
                    built,
                    action.args.as_ref().map(|args| args.to_string().into()),
                ),
                Err(error) => {
                    problems.push(format!("Keymap: {where_}: {error}"));
                    continue;
                }
            },
        };
        let predicate = match &entry.context {
            None => None,
            Some(context) => match KeyBindingContextPredicate::parse(context) {
                Ok(predicate) => Some(Rc::new(predicate)),
                Err(error) => {
                    problems.push(format!("Keymap: {where_}: bad context: {error}"));
                    continue;
                }
            },
        };
        match KeyBinding::load(
            &entry.keystrokes,
            action,
            predicate,
            entry.use_key_equivalents,
            input,
            cx.keyboard_mapper().as_ref(),
        ) {
            Ok(binding) => bindings.push(binding),
            Err(error) => problems.push(format!("Keymap: {where_}: {error}")),
        }
    }
    (bindings, problems)
}

/// GPUI bindings for the saved commands' keybindings, in the `Workspace` context, and
/// a message for every keybinding that does not parse or that collides with one of
/// `existing` (the keymap's bindings, which these are layered after).
pub fn command_bindings(
    commands: &CommandStore,
    existing: &[KeyBinding],
    cx: &App,
) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = Vec::new();
    let mut problems = Vec::new();
    let predicate = KeyBindingContextPredicate::parse(context::WORKSPACE)
        .ok()
        .map(Rc::new);
    for (keystrokes, reference) in commands.all_keybindings() {
        let action = actions::commands::Send::from(&reference);
        let binding = match KeyBinding::load(
            &keystrokes,
            Box::new(action),
            predicate.clone(),
            false,
            None,
            cx.keyboard_mapper().as_ref(),
        ) {
            Ok(binding) => binding,
            Err(error) => {
                problems.push(format!(
                    "Command {}: keybinding \"{keystrokes}\": {error}",
                    reference.name
                ));
                continue;
            }
        };
        let taken = existing
            .iter()
            .filter(|other| other.keystrokes() == binding.keystrokes())
            .filter(|other| !other.action().partial_eq(&NoAction))
            .map(|other| other.action().name())
            .collect::<Vec<_>>();
        if !taken.is_empty() {
            problems.push(format!(
                "Command {} ({keystrokes}) conflicts with {}",
                reference.name,
                taken.join(", ")
            ));
        }
        bindings.push(binding);
    }
    (bindings, problems)
}

/// Replace every key binding with gpui-kit's, then `keymap`'s, then the saved
/// commands'. Returns the entries that could not be bound and the command keybindings
/// that collide with a binding already there.
pub fn apply(keymap: &Keymap, commands: &CommandStore, cx: &mut App) -> Vec<String> {
    let (bindings, mut problems) = gpui_bindings(keymap, cx);
    let kit = cx
        .try_global::<KitBindings>()
        .map(|kit| kit.0.clone())
        .unwrap_or_default();
    let existing: Vec<KeyBinding> = kit.iter().chain(&bindings).cloned().collect();
    let (commands, command_problems) = command_bindings(commands, &existing, cx);
    problems.extend(command_problems);
    cx.clear_key_bindings();
    cx.bind_keys(kit);
    cx.bind_keys(bindings);
    cx.bind_keys(commands);
    problems
}
