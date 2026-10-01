//! Comment-preserving edits to `keymap.json`, with the same mechanics as
//! [`SettingsEditor`](super::SettingsEditor).
//!
//! A keymap is an array of sections, each an optional `"context"` and a `"bindings"` map
//! of keystrokes to actions (`null` unbinds). A context is `Some("Terminal")`, or `None`
//! for the sections with no `"context"`, which bind everywhere. Keystrokes are compared
//! exactly as written.
//!
//! A binding that is in the file is changed where it is (in the last section that has
//! it). A new one goes into the last section of its context that has a `"bindings"` map,
//! skipping sections that set `use_key_equivalents`, whose meaning a new key should not
//! change; with none, a new section is appended to the array. The commented template
//! ([`keymap_template`](super::keymap_template)) is an array whose entries are all
//! comments, so the first binding written into it becomes the array's first live section,
//! after the commented defaults.

use std::collections::BTreeMap;
use std::path::Path;

use jsonc_parser::cst::{CstInputValue, CstNode, CstObject};
use serde_json::Value;

use super::edit::{
    Container, Document, EditError, Fail, KeyChange, RootKind, parse_root, parse_value,
    root_container, to_input, values_equal,
};
use super::paths::{Platform, keymap_template};

/// An open `keymap.json`, edited in memory and written back by [`save`](Self::save).
#[derive(Debug)]
pub struct KeymapEditor {
    doc: Document,
}

impl KeymapEditor {
    /// Reads the file at `path`; a missing file is first created from the commented
    /// template ([`keymap_template`](super::keymap_template)). A file that does not
    /// parse is an [`EditError::Invalid`] with the line and column.
    pub fn open(path: &Path) -> Result<Self, EditError> {
        Ok(Self {
            doc: Document::open(path, RootKind::Array, || {
                keymap_template(Platform::current())
            })?,
        })
    }

    /// The file this editor reads and writes.
    pub fn path(&self) -> &Path {
        &self.doc.path
    }

    /// The contexts the file has sections for, in file order, `None` for the sections
    /// that bind everywhere.
    pub fn contexts(&self) -> Vec<Option<String>> {
        let mut contexts = Vec::new();
        for section in sections(&self.doc.text) {
            if !contexts.contains(&section.context) {
                contexts.push(section.context);
            }
        }
        contexts
    }

    /// The bindings the file has for `context`, as `(keystrokes, action)` in file order.
    /// The action is a name, a `[name, args]` array, or `null` for an unbinding. When
    /// several sections have the context, a later one replaces the action of an earlier
    /// one with the same keystrokes, in the earlier one's place. These are the user's
    /// bindings only, not the bundled defaults they sit on.
    pub fn bindings_for<'a>(&self, context: impl Into<Option<&'a str>>) -> Vec<(String, Value)> {
        let context = context.into();
        let mut out: Vec<(String, Value)> = Vec::new();
        for section in sections(&self.doc.text) {
            if section.context.as_deref() != context {
                continue;
            }
            for (keystrokes, action) in section.bindings {
                match out.iter_mut().find(|(known, _)| *known == keystrokes) {
                    Some(slot) => slot.1 = action,
                    None => out.push((keystrokes, action)),
                }
            }
        }
        out
    }

    /// Binds `keystrokes` in `context` to `action`: a name such as `"terminal::Clear"`,
    /// a `[name, args]` array, or `null` (see [`unbind`](Self::unbind)). A new context
    /// gets a section appended to the file.
    pub fn bind<'a>(
        &mut self,
        context: impl Into<Option<&'a str>>,
        keystrokes: &str,
        action: impl Into<Value>,
    ) -> Result<(), EditError> {
        let text = bind_in(&self.doc.text, context.into(), keystrokes, &action.into())
            .map_err(|fail| fail.into_error(&self.doc.path))?;
        self.doc.replace_text(text);
        Ok(())
    }

    /// Writes `null` for `keystrokes` in `context`, which cancels the bundled binding
    /// of the same keystrokes.
    pub fn unbind<'a>(
        &mut self,
        context: impl Into<Option<&'a str>>,
        keystrokes: &str,
    ) -> Result<(), EditError> {
        self.bind(context, keystrokes, Value::Null)
    }

    /// Removes `keystrokes` from every section of `context` that has it, so the bundled
    /// binding applies again; returns whether any did. The sections stay, empty if need
    /// be, and so do their comments.
    pub fn remove<'a>(
        &mut self,
        context: impl Into<Option<&'a str>>,
        keystrokes: &str,
    ) -> Result<bool, EditError> {
        match remove_in(&self.doc.text, context.into(), keystrokes)
            .map_err(|fail| fail.into_error(&self.doc.path))?
        {
            Some(text) => {
                self.doc.replace_text(text);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// The text as it would be saved.
    pub fn text(&self) -> &str {
        &self.doc.text
    }

    /// Whether the text differs from what the file held when it was last read or saved.
    pub fn is_dirty(&self) -> bool {
        self.doc.text != self.doc.base
    }

    /// Writes the text back atomically; see [`SettingsEditor::save`](super::SettingsEditor::save).
    pub fn save(&mut self) -> Result<(), EditError> {
        self.doc.save()
    }

    /// What differs from the file as opened; pointers are `/<section>/bindings/<keys>`.
    pub fn diff_summary(&self) -> Vec<KeyChange> {
        self.doc.diff_summary()
    }
}

// ---- Reading ----

struct Section {
    context: Option<String>,
    bindings: Vec<(String, Value)>,
}

/// The sections of a keymap document, as plain data.
fn sections(text: &str) -> Vec<Section> {
    let Ok(Value::Array(items)) = parse_value(text, RootKind::Array) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| {
            let Value::Object(section) = item else {
                return None;
            };
            let context = section
                .get("context")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let bindings = match section.get("bindings") {
                Some(Value::Object(map)) => {
                    map.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                }
                _ => Vec::new(),
            };
            Some(Section { context, bindings })
        })
        .collect()
}

/// Every binding by context and keystrokes, later sections replacing earlier ones.
fn effective(text: &str) -> BTreeMap<(Option<String>, String), Value> {
    let mut map = BTreeMap::new();
    for section in sections(text) {
        for (keystrokes, action) in section.bindings {
            map.insert((section.context.clone(), keystrokes), action);
        }
    }
    map
}

/// A section's CST object with what the edits need to know about it.
struct SectionNode {
    object: CstObject,
    context: Option<String>,
    key_equivalents: bool,
}

fn section_nodes(array: &jsonc_parser::cst::CstArray) -> Vec<SectionNode> {
    array
        .elements()
        .iter()
        .filter_map(|element| {
            let object = element.as_object()?;
            let context = object
                .get("context")
                .and_then(|prop| prop.value())
                .and_then(|node| node.as_string_lit())
                .and_then(|lit| lit.decoded_value().ok());
            let key_equivalents = object
                .get("use_key_equivalents")
                .and_then(|prop| prop.value())
                .and_then(|node| node.as_boolean_lit())
                .is_some_and(|lit| lit.value());
            Some(SectionNode {
                object,
                context,
                key_equivalents,
            })
        })
        .collect()
}

fn bindings_object(section: &CstObject) -> Option<CstObject> {
    section.get("bindings")?.value()?.as_object()
}

// ---- Editing ----

fn bind_in(
    text: &str,
    context: Option<&str>,
    keystrokes: &str,
    action: &Value,
) -> Result<String, Fail> {
    let mut expected = effective(text);
    expected.insert(
        (context.map(str::to_owned), keystrokes.to_owned()),
        action.clone(),
    );

    let root = parse_root(text)?;
    let Container::Array(array) = root_container(&root, RootKind::Array)? else {
        return Err(Fail::pointer("", "the top level is not an array"));
    };
    let nodes = section_nodes(&array);
    let matching: Vec<&SectionNode> = nodes
        .iter()
        .filter(|node| node.context.as_deref() == context)
        .collect();
    let input = to_input(action);

    // Already bound: change it where it is.
    let existing = matching
        .iter()
        .rev()
        .find_map(|node| bindings_object(&node.object)?.get(keystrokes));
    if let Some(prop) = existing {
        prop.set_value(input);
    } else if let Some(bindings) = matching
        .iter()
        .rev()
        .filter(|node| !node.key_equivalents)
        .find_map(|node| bindings_object(&node.object))
    {
        bindings.append(keystrokes, input);
    } else if let Some(section) = matching
        .iter()
        .rev()
        .find(|node| !node.key_equivalents && node.object.get("bindings").is_none())
    {
        section.object.append(
            "bindings",
            CstInputValue::Object(vec![(keystrokes.to_owned(), input)]),
        );
    } else {
        let mut members = Vec::new();
        if let Some(context) = context {
            members.push((
                "context".to_owned(),
                CstInputValue::String(context.to_owned()),
            ));
        }
        members.push((
            "bindings".to_owned(),
            CstInputValue::Object(vec![(keystrokes.to_owned(), input)]),
        ));
        array.append(CstInputValue::Object(members));
    }

    let new_text = root.to_string();
    check(&new_text, &expected)?;
    Ok(new_text)
}

fn remove_in(text: &str, context: Option<&str>, keystrokes: &str) -> Result<Option<String>, Fail> {
    let mut expected = effective(text);
    expected.remove(&(context.map(str::to_owned), keystrokes.to_owned()));

    let root = parse_root(text)?;
    let Some(Container::Array(array)) = root
        .value()
        .and_then(|node: CstNode| Container::from_node(&node))
    else {
        return Ok(None);
    };
    let mut removed = false;
    for node in section_nodes(&array) {
        if node.context.as_deref() != context {
            continue;
        }
        if let Some(prop) = bindings_object(&node.object).and_then(|b| b.get(keystrokes)) {
            prop.remove();
            removed = true;
        }
    }
    if !removed {
        return Ok(None);
    }
    let new_text = root.to_string();
    check(&new_text, &expected)?;
    Ok(Some(new_text))
}

/// Fails unless the keymap in `text` has the bindings `expected` and the sections still
/// parse as an array.
fn check(text: &str, expected: &BTreeMap<(Option<String>, String), Value>) -> Result<(), Fail> {
    if parse_value(text, RootKind::Array).is_err() {
        return Err(Fail::Rejected("the result would not parse".to_owned()));
    }
    let actual = effective(text);
    let same = actual.len() == expected.len()
        && actual.iter().all(|(key, value)| {
            expected
                .get(key)
                .is_some_and(|want| values_equal(value, want))
        });
    if same {
        Ok(())
    } else {
        Err(Fail::Rejected(
            "the result would differ from the old keymap in more than that binding".to_owned(),
        ))
    }
}
