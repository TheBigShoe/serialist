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
//! after `gpui_kit::init`), then the bundled defaults, then the user's file. Nothing is
//! bound twice however often the file is saved.

use std::rc::Rc;

use serialist_core::Keymap;

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

/// Replace every key binding with gpui-kit's followed by `keymap`'s. Returns the
/// entries that could not be bound.
pub fn apply(keymap: &Keymap, cx: &mut App) -> Vec<String> {
    let (bindings, problems) = gpui_bindings(keymap, cx);
    let kit = cx
        .try_global::<KitBindings>()
        .map(|kit| kit.0.clone())
        .unwrap_or_default();
    cx.clear_key_bindings();
    cx.bind_keys(kit);
    cx.bind_keys(bindings);
    problems
}
