//! Comment-preserving edits to the JSONC configuration files.
//!
//! [`SettingsEditor`] changes one key of `settings.json` and leaves everything else the
//! user wrote exactly as it was: comments, blank lines, key order, indentation (spaces or
//! tabs), line endings (LF or CRLF) and the trailing-comma style. [`KeymapEditor`]
//! (in `keymap_edit.rs`) does the same for `keymap.json`. Both sit on `jsonc-parser`'s
//! concrete syntax tree (the `cst` feature of 0.34): each edit parses the current text,
//! changes the tree, and prints it back, so only the touched value differs.
//!
//! # Paths
//!
//! A key is named by a JSON pointer (RFC 6901): `/buffer_font_size`,
//! `/display/timestamps`, `/devices/0/baud`. `~1` stands for `/` and `~0` for `~` inside
//! a key. On arrays, `/devices/-` appends and `/devices/2` replaces the third element;
//! an index equal to the length appends too, and a larger one is an error. A missing
//! intermediate is created: an object, or an array when the next step is `-` or `0`. A
//! `null` on the way is replaced by the new container (`null` means "unset"), but a
//! string, number or boolean is never overwritten to make room: that is an error.
//!
//! # The commented template
//!
//! A new `settings.json` is the commented template ([`settings_template`]): every
//! default is a `// "key": value,` line. Setting a key that is not live in the file first
//! looks for such a line among the comments of the object it belongs in:
//!
//! * A single-line member (`// "buffer_font_size": 15.0,`) is uncommented in place, and
//!   its value then replaced. The comments above it stay where they are, so the key
//!   keeps its explanation.
//! * A member that opens a block (`// "display": {` with a matching `// },` at the same
//!   indent) is uncommented as an empty object, its other lines still commented, and the
//!   edit continues inside it.
//!
//! The commas around an uncommented line follow the file: one is added after the previous
//! member when it had none, and the trailing comma matches the file's style. Every such
//! step is checked by parsing the result; if it does not leave the document as it was
//! plus that one member, it is dropped and the key is added instead.
//!
//! When there is no matching comment (a key the template does not mention, or a file that
//! was never a template), the key goes after the object's last member, or, in an object
//! whose only content is comments, at the top, right under the opening brace.
//!
//! This applies to any file, not just the template: a commented-out `// "theme": ...`
//! line the user left behind is uncommented the same way.
//!
//! # What is not preserved
//!
//! The comments above a key stay when the key is removed. The keys of an object value
//! are written in the order the settings documentation lists them for device profiles
//! (`name`, `match`, `vid`, `pid`, ..., `baud`, ..., `plugin`, `on_connect`) and then in
//! the [`Value`]'s own order, which is alphabetical unless `serde_json`'s
//! `preserve_order` is on. Set keys one at a time to control the order.
//!
//! Every edit is checked before it is kept: the result must parse and must equal the old
//! document with exactly that change. Otherwise the edit is refused with
//! [`EditError::Rejected`] and the text is unchanged.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::ops::Range;
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use jsonc_parser::cst::{
    CstArray, CstComment, CstContainerNode, CstInputValue, CstLeafNode, CstNode, CstObject,
    CstRootNode,
};
use serde_json::{Map, Value};

use super::paths::settings_template;
use crate::commands::write_atomic;

/// Why a configuration file could not be opened, edited or saved.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file does not parse, or has the wrong shape at the top. `line` and `column`
    /// count from 1; `message` is the parser's own text.
    #[error("{}:{line}:{column}: {message}", path.display())]
    Invalid {
        path: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
    /// The pointer is malformed, or names a place that cannot hold the value: a step
    /// through a string, an array index out of range.
    #[error("{}: cannot edit `{pointer}`: {message}", path.display())]
    Pointer {
        path: PathBuf,
        pointer: String,
        message: String,
    },
    /// The file on disk is no longer what [`SettingsEditor::open`] read, so saving would
    /// overwrite someone else's change. Open it again and repeat the edit.
    #[error("{}: the file changed on disk since it was opened", path.display())]
    Changed { path: PathBuf },
    /// The edit would not have produced the document it should (a bug, or a file with
    /// duplicate keys). The text is unchanged.
    #[error("{}: the edit was not applied: {message}", path.display())]
    Rejected { path: PathBuf, message: String },
}

impl EditError {
    /// The file the problem is in.
    pub fn path(&self) -> &Path {
        match self {
            EditError::Io { path, .. }
            | EditError::Invalid { path, .. }
            | EditError::Pointer { path, .. }
            | EditError::Changed { path }
            | EditError::Rejected { path, .. } => path,
        }
    }

    /// The line and column (from 1) of a syntax problem, when it is one.
    pub fn position(&self) -> Option<(usize, usize)> {
        match self {
            EditError::Invalid { line, column, .. } => Some((*line, *column)),
            _ => None,
        }
    }
}

/// An [`EditError`] before it knows its file.
#[derive(Debug)]
pub(super) enum Fail {
    Invalid {
        line: usize,
        column: usize,
        message: String,
    },
    Pointer {
        pointer: String,
        message: String,
    },
    Rejected(String),
}

impl Fail {
    pub(super) fn pointer(pointer: &str, message: impl Into<String>) -> Self {
        Fail::Pointer {
            pointer: pointer.to_owned(),
            message: message.into(),
        }
    }

    pub(super) fn into_error(self, path: &Path) -> EditError {
        let path = path.to_path_buf();
        match self {
            Fail::Invalid {
                line,
                column,
                message,
            } => EditError::Invalid {
                path,
                line,
                column,
                message,
            },
            Fail::Pointer { pointer, message } => EditError::Pointer {
                path,
                pointer,
                message,
            },
            Fail::Rejected(message) => EditError::Rejected { path, message },
        }
    }
}

/// One key whose value differs from what the file held when it was opened.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyChange {
    /// The key as a JSON pointer, such as `/display/timestamps` or `/devices/1`.
    pub pointer: String,
    /// The value when the file was opened; `None` if the key was not there.
    pub before: Option<Value>,
    /// The value now; `None` if the key is gone.
    pub after: Option<Value>,
}

impl fmt::Display for KeyChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let show = |value: &Option<Value>, none: &str| {
            value
                .as_ref()
                .map_or_else(|| none.to_owned(), ToString::to_string)
        };
        write!(
            f,
            "{}: {} -> {}",
            self.pointer,
            show(&self.before, "(unset)"),
            show(&self.after, "(removed)")
        )
    }
}

// ---- The editors ----

/// An open `settings.json` (or a project `.serialist/settings.json`), edited in memory
/// and written back by [`save`](Self::save).
///
/// See the [module documentation](self) for the path syntax and for what happens to the
/// commented template.
#[derive(Debug)]
pub struct SettingsEditor {
    doc: Document,
}

impl SettingsEditor {
    /// Reads the file at `path`; a missing file is first created from the commented
    /// template ([`settings_template`]), with its directory, as
    /// [`ConfigPaths::ensure_settings_file`](super::ConfigPaths::ensure_settings_file)
    /// does. A file that does not parse is an [`EditError::Invalid`] carrying the line
    /// and column, so the settings screen can offer to open the file instead.
    pub fn open(path: &Path) -> Result<Self, EditError> {
        Ok(Self {
            doc: Document::open(path, RootKind::Object, settings_template)?,
        })
    }

    /// Opens `path`, sets `pointer` to `value`, and saves, in one call.
    pub fn set_in_file(
        path: &Path,
        pointer: &str,
        value: impl Into<Value>,
    ) -> Result<(), EditError> {
        let mut editor = Self::open(path)?;
        editor.set(pointer, value)?;
        editor.save()
    }

    /// Opens `path`, removes `pointer`, and saves, in one call. Returns whether the key
    /// was there.
    pub fn remove_in_file(path: &Path, pointer: &str) -> Result<bool, EditError> {
        let mut editor = Self::open(path)?;
        let removed = editor.remove(pointer)?;
        editor.save()?;
        Ok(removed)
    }

    /// The file this editor reads and writes.
    pub fn path(&self) -> &Path {
        &self.doc.path
    }

    /// The value at `pointer` as the file holds it now, not merged with the defaults.
    /// `""` is the whole document.
    pub fn get(&self, pointer: &str) -> Option<Value> {
        self.doc.get(pointer)
    }

    /// Sets the key at `pointer`, creating it and any intermediate objects. Setting a
    /// value equal to the current one changes nothing. See the [module
    /// documentation](self) for formatting, arrays and the template.
    pub fn set(&mut self, pointer: &str, value: impl Into<Value>) -> Result<(), EditError> {
        self.doc.set(pointer, value.into())
    }

    /// Removes the key at `pointer`; returns whether it was there. An object that
    /// becomes empty stays: see [`remove_and_prune`](Self::remove_and_prune).
    pub fn remove(&mut self, pointer: &str) -> Result<bool, EditError> {
        self.doc.remove(pointer, false)
    }

    /// Like [`remove`](Self::remove), and then removes each enclosing object or array
    /// the removal left empty, up to but not including the top object. One that holds a
    /// comment is not empty and stays.
    pub fn remove_and_prune(&mut self, pointer: &str) -> Result<bool, EditError> {
        self.doc.remove(pointer, true)
    }

    /// The text as it would be saved.
    pub fn text(&self) -> &str {
        &self.doc.text
    }

    /// Whether the text differs from what the file held when it was last read or saved.
    pub fn is_dirty(&self) -> bool {
        self.doc.text != self.doc.base
    }

    /// Writes the text back: into a temporary file next to the original, then renamed
    /// over it, so a reader never sees half a file. Line endings are whatever the text
    /// has, which is what the file had. Does nothing when nothing changed. Fails with
    /// [`EditError::Changed`] if the file was modified since it was read.
    pub fn save(&mut self) -> Result<(), EditError> {
        self.doc.save()
    }

    /// The keys whose values differ from the file as opened, for the settings screen's
    /// notice. Objects are compared key by key and arrays element by element; a key
    /// added or removed with its whole subtree is one entry.
    pub fn diff_summary(&self) -> Vec<KeyChange> {
        self.doc.diff_summary()
    }
}

// ---- The shared document ----

/// Whether the top-level value is an object (settings) or an array (keymap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RootKind {
    Object,
    Array,
}

impl RootKind {
    fn noun(self) -> &'static str {
        match self {
            RootKind::Object => "object",
            RootKind::Array => "array",
        }
    }

    fn empty(self) -> Value {
        match self {
            RootKind::Object => Value::Object(Map::new()),
            RootKind::Array => Value::Array(Vec::new()),
        }
    }
}

/// A file's text and what it takes to write it back.
#[derive(Debug)]
pub(super) struct Document {
    pub(super) path: PathBuf,
    pub(super) kind: RootKind,
    /// The text when the file was opened, for the diff.
    opened: String,
    /// What the file held when last read or written, to notice outside changes.
    pub(super) base: String,
    pub(super) text: String,
}

impl Document {
    pub(super) fn open(
        path: &Path,
        kind: RootKind,
        template: impl FnOnce() -> String,
    ) -> Result<Self, EditError> {
        let io_error = |source| EditError::Io {
            path: path.to_path_buf(),
            source,
        };
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                create_new(path, template().as_bytes()).map_err(io_error)?;
                fs::read_to_string(path).map_err(io_error)?
            }
            Err(err) => return Err(io_error(err)),
        };
        check_shape(&text, kind).map_err(|fail| fail.into_error(path))?;
        Ok(Self {
            path: path.to_path_buf(),
            kind,
            opened: text.clone(),
            base: text.clone(),
            text,
        })
    }

    pub(super) fn get(&self, pointer: &str) -> Option<Value> {
        parse_value(&self.text, self.kind)
            .ok()?
            .pointer(pointer)
            .cloned()
    }

    fn set(&mut self, pointer: &str, value: Value) -> Result<(), EditError> {
        let fail = |fail: Fail| fail.into_error(&self.path);
        let segments = parse_pointer(pointer).map_err(fail)?;
        if segments.is_empty() {
            return Err(Fail::pointer(
                pointer,
                "name a key; the whole document cannot be replaced",
            )
            .into_error(&self.path));
        }
        if self
            .get(pointer)
            .is_some_and(|existing| values_equal(&existing, &value))
        {
            return Ok(());
        }
        self.text = edit_set(&self.text, self.kind, &segments, &value)
            .map_err(|failure| failure.into_error(&self.path))?;
        Ok(())
    }

    fn remove(&mut self, pointer: &str, prune: bool) -> Result<bool, EditError> {
        let segments = parse_pointer(pointer).map_err(|failure| failure.into_error(&self.path))?;
        if segments.is_empty() {
            return Err(
                Fail::pointer(pointer, "name a key; the whole document cannot be removed")
                    .into_error(&self.path),
            );
        }
        match edit_remove(&self.text, self.kind, &segments, prune)
            .map_err(|failure| failure.into_error(&self.path))?
        {
            Some(text) => {
                self.text = text;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Replaces the text with one an editor built and checked itself.
    pub(super) fn replace_text(&mut self, text: String) {
        self.text = text;
    }

    pub(super) fn save(&mut self) -> Result<(), EditError> {
        if self.text == self.base {
            return Ok(());
        }
        let io_error = |source| EditError::Io {
            path: self.path.clone(),
            source,
        };
        // Write through a symlink (a dotfiles manager's) rather than replacing it.
        let target = fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        match fs::read_to_string(&target) {
            Ok(disk) if disk != self.base => {
                return Err(EditError::Changed {
                    path: self.path.clone(),
                });
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(err)),
        }
        let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());
        write_atomic(&target, self.text.as_bytes()).map_err(io_error)?;
        if let Some(permissions) = permissions {
            // Best effort: the new file should be as private as the old one was.
            let _ = fs::set_permissions(&target, permissions);
        }
        self.base.clone_from(&self.text);
        Ok(())
    }

    pub(super) fn diff_summary(&self) -> Vec<KeyChange> {
        let before = parse_value(&self.opened, self.kind).ok();
        let after = parse_value(&self.text, self.kind).ok();
        let mut changes = Vec::new();
        diff_values("", before.as_ref(), after.as_ref(), &mut changes);
        changes.sort_by(|a, b| a.pointer.cmp(&b.pointer));
        changes
    }
}

/// Creates `path` with `contents` unless it exists, making its directory.
fn create_new(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => file.write_all(contents),
        // Created by someone else a moment ago: theirs wins.
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

// ---- Parsing and pointers ----

pub(super) fn parse_root(text: &str) -> Result<CstRootNode, Fail> {
    CstRootNode::parse(text, &ParseOptions::default()).map_err(|err| Fail::Invalid {
        line: err.line_display(),
        column: err.column_display(),
        message: err.kind().to_string(),
    })
}

/// The document as plain JSON; an empty document is an empty object or array.
pub(super) fn parse_value(text: &str, kind: RootKind) -> Result<Value, Fail> {
    let value: Value =
        jsonc_parser::parse_to_serde_value(text, &ParseOptions::default()).map_err(|err| {
            Fail::Invalid {
                line: err.line_display(),
                column: err.column_display(),
                message: err.kind().to_string(),
            }
        })?;
    Ok(if value.is_null() { kind.empty() } else { value })
}

/// Checks that the text parses and holds nothing but an object (or array) at the top.
fn check_shape(text: &str, kind: RootKind) -> Result<(), Fail> {
    let root = parse_root(text)?;
    let Some(node) = root.value() else {
        return Ok(());
    };
    let fits = match kind {
        RootKind::Object => node.as_object().is_some(),
        RootKind::Array => node.as_array().is_some(),
    };
    if fits {
        return Ok(());
    }
    let (line, column) = line_column(text, offset_of(&node));
    Err(Fail::Invalid {
        line,
        column,
        message: format!("the top level must be a JSON {}", kind.noun()),
    })
}

/// A 1-based line and column (in characters) for a byte offset.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, column)
}

/// The byte offset of a node in the printed document.
fn offset_of(node: &CstNode) -> usize {
    let mut offset = 0;
    let mut current = node.clone();
    while let Some(parent) = current.parent() {
        let index = current.child_index();
        offset += parent
            .children()
            .iter()
            .take(index)
            .map(|sibling| sibling.to_string().len())
            .sum::<usize>();
        current = CstNode::from(parent);
    }
    offset
}

/// RFC 6901 tokens of `pointer`; `""` is the whole document.
pub(super) fn parse_pointer(pointer: &str) -> Result<Vec<String>, Fail> {
    if pointer.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = pointer.strip_prefix('/') else {
        return Err(Fail::pointer(pointer, "a pointer starts with `/`"));
    };
    Ok(rest
        .split('/')
        .map(|token| token.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn join_pointer(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| format!("/{}", escape_token(segment)))
        .collect()
}

/// An array index as written in a pointer: digits, no leading zero.
fn parse_index(segment: &str) -> Option<usize> {
    let digits = !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
    if !digits || (segment.len() > 1 && segment.starts_with('0')) {
        return None;
    }
    segment.parse().ok()
}

/// Whether a missing container on the way is an array: the next step appends or opens
/// the first element.
fn creates_array(next_segment: &str) -> bool {
    next_segment == "-" || next_segment == "0"
}

/// Equality that does not tell `15` from `15.0`.
pub(super) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if x.is_f64() || y.is_f64() {
                x.as_f64() == y.as_f64()
            } else {
                x == y
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, p)| y.get(key).is_some_and(|q| values_equal(p, q)))
        }
        _ => a == b,
    }
}

fn diff_values(
    pointer: &str,
    before: Option<&Value>,
    after: Option<&Value>,
    out: &mut Vec<KeyChange>,
) {
    match (before, after) {
        (None, None) => {}
        (Some(Value::Object(old)), Some(Value::Object(new))) => {
            for key in old
                .keys()
                .chain(new.keys().filter(|k| !old.contains_key(*k)))
            {
                let child = format!("{pointer}/{}", escape_token(key));
                diff_values(&child, old.get(key), new.get(key), out);
            }
        }
        (Some(Value::Array(old)), Some(Value::Array(new))) => {
            for index in 0..old.len().max(new.len()) {
                let child = format!("{pointer}/{index}");
                diff_values(&child, old.get(index), new.get(index), out);
            }
        }
        (Some(old), Some(new)) if values_equal(old, new) => {}
        _ => out.push(KeyChange {
            pointer: pointer.to_owned(),
            before: before.cloned(),
            after: after.cloned(),
        }),
    }
}

// ---- Values going into the tree ----

/// The order the settings documentation gives for device profiles and their `match`
/// keys, then keymap sections; other keys follow in the value's own order.
const PREFERRED_KEY_ORDER: &[&str] = &[
    "name",
    "context",
    "match",
    "vid",
    "pid",
    "product",
    "manufacturer",
    "serial_number",
    "path",
    "baud",
    "data_bits",
    "parity",
    "stop_bits",
    "flow_control",
    "eol",
    "plugin",
    "on_connect",
    "bindings",
];

pub(super) fn to_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(flag) => CstInputValue::Bool(*flag),
        Value::Number(number) => CstInputValue::Number(number.to_string()),
        Value::String(text) => CstInputValue::String(text.clone()),
        Value::Array(items) => CstInputValue::Array(items.iter().map(to_input).collect()),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            // Stable: keys outside the list keep the value's order.
            entries.sort_by_key(|(key, _)| {
                PREFERRED_KEY_ORDER
                    .iter()
                    .position(|preferred| preferred == key)
                    .unwrap_or(usize::MAX)
            });
            CstInputValue::Object(
                entries
                    .into_iter()
                    .map(|(key, child)| (key.clone(), to_input(child)))
                    .collect(),
            )
        }
    }
}

/// The value to insert for the part of a path that does not exist yet.
fn build_input(rest: &[String], value: &Value) -> CstInputValue {
    match rest.split_first() {
        None => to_input(value),
        Some((segment, tail)) if creates_array(segment) => {
            CstInputValue::Array(vec![build_input(tail, value)])
        }
        Some((segment, tail)) => {
            CstInputValue::Object(vec![(segment.clone(), build_input(tail, value))])
        }
    }
}

// ---- Walking the tree ----

#[derive(Clone)]
pub(super) enum Container {
    Object(CstObject),
    Array(CstArray),
}

impl Container {
    pub(super) fn from_node(node: &CstNode) -> Option<Self> {
        node.as_object()
            .map(Container::Object)
            .or_else(|| node.as_array().map(Container::Array))
    }

    fn is_empty(&self) -> bool {
        match self {
            Container::Object(object) => object.properties().is_empty(),
            Container::Array(array) => array.elements().is_empty(),
        }
    }

    fn has_comment(&self) -> bool {
        let children = match self {
            Container::Object(object) => object.children(),
            Container::Array(array) => array.children(),
        };
        children.iter().any(CstNode::is_comment)
    }
}

enum Step {
    Found(CstNode),
    Missing,
}

fn step(container: &Container, segment: &str, walked: &[String]) -> Result<Step, Fail> {
    match container {
        Container::Object(object) => Ok(object
            .get(segment)
            .and_then(|prop| prop.value())
            .map_or(Step::Missing, Step::Found)),
        Container::Array(array) => {
            if segment == "-" {
                return Ok(Step::Missing);
            }
            let Some(index) = parse_index(segment) else {
                return Err(Fail::pointer(
                    &join_pointer(walked),
                    format!("`{segment}` is not an array index"),
                ));
            };
            let elements = array.elements();
            match index.cmp(&elements.len()) {
                std::cmp::Ordering::Less => Ok(Step::Found(elements[index].clone())),
                std::cmp::Ordering::Equal => Ok(Step::Missing),
                std::cmp::Ordering::Greater => Err(Fail::pointer(
                    &join_pointer(walked),
                    format!(
                        "index {index} is out of range: the array has {} elements",
                        elements.len()
                    ),
                )),
            }
        }
    }
}

fn describe(node: &CstNode) -> &'static str {
    if node.as_string_lit().is_some() {
        "a string"
    } else if node.as_number_lit().is_some() {
        "a number"
    } else if node.as_boolean_lit().is_some() {
        "a boolean"
    } else if node.as_null_keyword().is_some() {
        "null"
    } else {
        "a value"
    }
}

/// The top-level container, creating an empty object (or array) in an empty document.
pub(super) fn root_container(root: &CstRootNode, kind: RootKind) -> Result<Container, Fail> {
    if root.value().is_none() {
        root.set_value(match kind {
            RootKind::Object => CstInputValue::Object(Vec::new()),
            RootKind::Array => CstInputValue::Array(Vec::new()),
        });
    }
    root.value()
        .and_then(|node| Container::from_node(&node))
        .filter(|container| {
            matches!(
                (kind, container),
                (RootKind::Object, Container::Object(_)) | (RootKind::Array, Container::Array(_))
            )
        })
        .ok_or_else(|| Fail::pointer("", format!("the top level is not an {}", kind.noun())))
}

fn replace_member(container: &Container, segment: &str, node: CstNode, input: CstInputValue) {
    match container {
        Container::Object(object) => {
            if let Some(prop) = object.get(segment) {
                prop.set_value(input);
            }
        }
        Container::Array(_) => replace_node(node, input),
    }
}

fn replace_node(node: CstNode, input: CstInputValue) {
    match node {
        CstNode::Container(CstContainerNode::Object(object)) => {
            object.replace_with(input);
        }
        CstNode::Container(CstContainerNode::Array(array)) => {
            array.replace_with(input);
        }
        CstNode::Leaf(CstLeafNode::StringLit(leaf)) => {
            leaf.replace_with(input);
        }
        CstNode::Leaf(CstLeafNode::NumberLit(leaf)) => {
            leaf.replace_with(input);
        }
        CstNode::Leaf(CstLeafNode::BooleanLit(leaf)) => {
            leaf.replace_with(input);
        }
        CstNode::Leaf(CstLeafNode::NullKeyword(leaf)) => {
            leaf.replace_with(input);
        }
        CstNode::Leaf(CstLeafNode::WordLit(leaf)) => {
            leaf.replace_with(input);
        }
        _ => {}
    }
}

/// The key that stands in while a member is put at the top of an object.
const TOP_INSERT_PLACEHOLDER: &str = "__serialist_top_insert__";

fn add_member(container: &Container, segment: &str, input: CstInputValue) {
    match container {
        Container::Array(array) => {
            array.append(input);
        }
        Container::Object(object) => {
            if object.properties().is_empty() && Container::Object(object.clone()).has_comment() {
                // Only comments inside (a template): the member goes at the top, under
                // the brace. `insert(0)` needs a member to go before, so one stands in
                // and is removed again, which also takes the comma it brought.
                let placeholder = object.append(TOP_INSERT_PLACEHOLDER, CstInputValue::Null);
                object.insert(0, segment, input);
                placeholder.remove();
            } else {
                object.append(segment, input);
            }
        }
    }
}

// ---- Set ----

/// `text` with `segments` set to `value`, checked.
pub(super) fn edit_set(
    text: &str,
    kind: RootKind,
    segments: &[String],
    value: &Value,
) -> Result<String, Fail> {
    let before = parse_value(text, kind)?;

    // Uncomment the template lines the path runs through, one parse at a time.
    let mut text = text.to_owned();
    for _ in 0..=segments.len() {
        match uncomment_step(&text, kind, segments)? {
            Some(next) => text = next,
            None => break,
        }
    }

    let root = parse_root(&text)?;
    let mut container = root_container(&root, kind)?;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let walked = &segments[..index];
        match step(&container, segment, walked)? {
            Step::Found(node) if last => {
                replace_member(&container, segment, node, to_input(value));
                break;
            }
            Step::Found(node) => {
                if let Some(next) = Container::from_node(&node) {
                    container = next;
                } else if node.as_null_keyword().is_some() {
                    // Null is "unset": make room for the new container.
                    let input = build_input(&segments[index + 1..], value);
                    replace_member(&container, segment, node, input);
                    break;
                } else {
                    return Err(Fail::pointer(
                        &join_pointer(&segments[..=index]),
                        format!(
                            "`{}` holds {}, not an object or array",
                            join_pointer(&segments[..=index]),
                            describe(&node)
                        ),
                    ));
                }
            }
            Step::Missing => {
                add_member(
                    &container,
                    segment,
                    build_input(&segments[index + 1..], value),
                );
                break;
            }
        }
    }
    let new_text = root.to_string();

    let mut expected = before;
    model_set(&mut expected, segments, value)?;
    verify(&new_text, kind, &expected)?;
    Ok(new_text)
}

/// Applies `set` to plain JSON the way the tree edit does, for checking it.
fn model_set(root: &mut Value, segments: &[String], value: &Value) -> Result<(), Fail> {
    let mut cursor = root;
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let next_container = || {
            segments.get(index + 1).map_or(Value::Null, |next| {
                if creates_array(next) {
                    Value::Array(Vec::new())
                } else {
                    Value::Object(Map::new())
                }
            })
        };
        match cursor {
            Value::Object(map) => {
                if last {
                    map.insert(segment.clone(), value.clone());
                    return Ok(());
                }
                let entry = map.entry(segment.clone()).or_insert(Value::Null);
                if entry.is_null() {
                    *entry = next_container();
                }
                cursor = entry;
            }
            Value::Array(items) => {
                let append = segment == "-" || parse_index(segment) == Some(items.len());
                if append {
                    items.push(if last {
                        value.clone()
                    } else {
                        next_container()
                    });
                    if last {
                        return Ok(());
                    }
                    let end = items.len() - 1;
                    cursor = &mut items[end];
                    continue;
                }
                let Some(at) = parse_index(segment).filter(|at| *at < items.len()) else {
                    return Err(Fail::Rejected(format!(
                        "`{segment}` is not an index into the array"
                    )));
                };
                if last {
                    items[at] = value.clone();
                    return Ok(());
                }
                if items[at].is_null() {
                    items[at] = next_container();
                }
                cursor = &mut items[at];
            }
            _ => {
                return Err(Fail::Rejected(format!(
                    "`{}` holds a scalar",
                    join_pointer(&segments[..index])
                )));
            }
        }
    }
    Ok(())
}

/// Fails unless `text` parses to `expected`.
pub(super) fn verify(text: &str, kind: RootKind, expected: &Value) -> Result<(), Fail> {
    let actual = parse_value(text, kind)
        .map_err(|_| Fail::Rejected("the result would not parse".to_owned()))?;
    if values_equal(&actual, expected) {
        Ok(())
    } else {
        Err(Fail::Rejected(
            "the result would differ from the old document in more than that key".to_owned(),
        ))
    }
}

// ---- Remove ----

/// `text` without the key at `segments`, or `None` if it is not there.
pub(super) fn edit_remove(
    text: &str,
    kind: RootKind,
    segments: &[String],
    prune: bool,
) -> Result<Option<String>, Fail> {
    let before = parse_value(text, kind)?;
    let root = parse_root(text)?;
    let Some(root_node) = root.value() else {
        return Ok(None);
    };
    let Some(mut container) = Container::from_node(&root_node) else {
        return Ok(None);
    };

    // The containers on the way down, each with the step taken out of it.
    let mut chain: Vec<(Container, String, CstNode)> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let node = match step(&container, segment, &segments[..index]) {
            Ok(Step::Found(node)) => node,
            Ok(Step::Missing) | Err(_) => return Ok(None),
        };
        chain.push((container.clone(), segment.clone(), node.clone()));
        if index + 1 < segments.len() {
            match Container::from_node(&node) {
                Some(next) => container = next,
                None => return Ok(None),
            }
        }
    }

    let mut pruned = Vec::new();
    remove_member(&chain, chain.len() - 1);
    if prune {
        for depth in (1..chain.len()).rev() {
            let (parent_container, _, node) = &chain[depth - 1];
            let Some(emptied) = Container::from_node(node) else {
                break;
            };
            let _ = parent_container;
            if !emptied.is_empty() || emptied.has_comment() {
                break;
            }
            remove_member(&chain, depth - 1);
            pruned.push(depth);
        }
    }
    let new_text = root.to_string();

    let mut expected = before;
    model_remove(&mut expected, segments)?;
    for depth in pruned {
        model_remove_empty(&mut expected, &segments[..depth])?;
    }
    verify(&new_text, kind, &expected)?;
    Ok(Some(new_text))
}

/// Removes the member `chain[at]` names from the container it was taken from.
fn remove_member(chain: &[(Container, String, CstNode)], at: usize) {
    let (container, segment, node) = &chain[at];
    match container {
        Container::Object(object) => {
            if let Some(prop) = object.get(segment) {
                prop.remove();
            }
        }
        Container::Array(_) => node.clone().remove(),
    }
}

fn model_remove(root: &mut Value, segments: &[String]) -> Result<(), Fail> {
    let Some((last, parents)) = segments.split_last() else {
        return Ok(());
    };
    let mut cursor = root;
    for segment in parents {
        cursor = match cursor {
            Value::Object(map) => map.get_mut(segment),
            Value::Array(items) => parse_index(segment).and_then(|at| items.get_mut(at)),
            _ => None,
        }
        .ok_or_else(|| Fail::Rejected("the path is not in the document".to_owned()))?;
    }
    match cursor {
        Value::Object(map) => {
            map.remove(last);
        }
        Value::Array(items) => {
            if let Some(at) = parse_index(last).filter(|at| *at < items.len()) {
                items.remove(at);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Removes the container at `segments` from the model, which must be empty there.
fn model_remove_empty(root: &mut Value, segments: &[String]) -> Result<(), Fail> {
    let at_path = {
        let mut cursor: &Value = root;
        for segment in segments {
            cursor = match cursor {
                Value::Object(map) => map.get(segment),
                Value::Array(items) => parse_index(segment).and_then(|at| items.get(at)),
                _ => None,
            }
            .ok_or_else(|| Fail::Rejected("a pruned path is not in the document".to_owned()))?;
        }
        cursor.clone()
    };
    let is_empty = match &at_path {
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => false,
    };
    if !is_empty {
        return Err(Fail::Rejected(
            "a container that still has members was pruned".to_owned(),
        ));
    }
    model_remove(root, segments)
}

// ---- Uncommenting template lines ----

/// A member written out in a comment.
enum Commented {
    /// `"key": value,` on one line.
    Single(Value),
    /// `"key": {` or `"key": [`, closed by a later comment line.
    Open(char),
}

/// Reads `// "key": value,` or `// "key": {` from a line comment's raw text.
fn parse_commented_member(raw: &str) -> Option<(String, Commented)> {
    let body = raw.strip_prefix("//")?.trim();
    let rest = body.strip_prefix('"')?;
    let mut escaped = false;
    let mut end = None;
    for (index, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            end = Some(index);
            break;
        }
    }
    let end = end?;
    let key: String = serde_json::from_str(&format!("\"{}\"", &rest[..end])).ok()?;
    let after = rest[end + 1..].trim_start().strip_prefix(':')?.trim();
    if after == "{" || after == "[" {
        return Some((key, Commented::Open(after.chars().next()?)));
    }
    let text = after.strip_suffix(',').map_or(after, str::trim_end);
    let value: Value = serde_json::from_str(text).ok()?;
    Some((key, Commented::Single(value)))
}

/// Whether only whitespace precedes the comment on its line.
fn on_own_line(comment: &CstComment) -> bool {
    for sibling in comment.previous_siblings() {
        if sibling.is_whitespace() {
            continue;
        }
        return sibling.is_newline();
    }
    false
}

fn find_commented(object: &CstObject, key: &str) -> Option<(CstComment, Commented)> {
    for child in object.children() {
        let CstNode::Leaf(CstLeafNode::Comment(comment)) = child else {
            continue;
        };
        if !comment.is_line_comment() || !on_own_line(&comment) {
            continue;
        }
        if let Some((found, kind)) = parse_commented_member(&comment.raw_value())
            && found == key
        {
            return Some((comment, kind));
        }
    }
    None
}

/// The line comment without its `//` and the one space after it, and without a trailing
/// comma; `comma` says whether to end it with one.
fn uncommented(raw: &str, comma: bool) -> String {
    let body = raw.strip_prefix("//").unwrap_or(raw);
    let body = body.strip_prefix(' ').unwrap_or(body).trim_end();
    let body = body.strip_suffix(',').unwrap_or(body);
    if comma {
        format!("{body},")
    } else {
        body.to_owned()
    }
}

/// Whether a member uncommented at `comment` takes a comma after it: yes when another
/// member follows, else as the object's other members do. `default` is what the comment
/// had, for an object with no members.
fn comma_after(object: &CstObject, comment: &CstComment, default: bool) -> bool {
    let has_next = comment
        .next_siblings()
        .any(|sibling| sibling.as_object_prop().is_some());
    let has_previous = comment
        .previous_siblings()
        .any(|sibling| sibling.as_object_prop().is_some());
    if has_next {
        true
    } else if has_previous {
        object.uses_trailing_commas()
    } else {
        default
    }
}

/// The closing comment of a commented block: an own-line `}` or `]` at the opener's
/// indent, with nothing live in between.
fn find_closer(opener: &CstComment, open: char) -> Option<CstComment> {
    let close = if open == '{' { '}' } else { ']' };
    let indent = opener.indent_text();
    for sibling in opener.next_siblings() {
        match sibling {
            CstNode::Leaf(CstLeafNode::Comment(comment)) => {
                if !comment.is_line_comment() || !on_own_line(&comment) {
                    continue;
                }
                let raw = comment.raw_value();
                let body = raw.strip_prefix("//")?.trim();
                let body = body.strip_suffix(',').unwrap_or(body);
                if body.trim_end() == close.to_string() && comment.indent_text() == indent {
                    return Some(comment);
                }
            }
            node if node.is_trivia() => {}
            _ => return None,
        }
    }
    None
}

/// One step of uncommenting for `segments`: the text with the first missing member on
/// the path uncommented, or `None` when there is nothing to uncomment.
fn uncomment_step(text: &str, kind: RootKind, segments: &[String]) -> Result<Option<String>, Fail> {
    let root = parse_root(text)?;
    let Some(mut container) = root.value().and_then(|node| Container::from_node(&node)) else {
        return Ok(None);
    };
    for (index, segment) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let next = match step(&container, segment, &segments[..index])? {
            Step::Found(node) if !last => match Container::from_node(&node) {
                Some(next) => next,
                None => return Ok(None),
            },
            Step::Found(_) => return Ok(None),
            Step::Missing => {
                let Container::Object(object) = &container else {
                    return Ok(None);
                };
                return uncomment_member(text, kind, &root, object, &segments[..=index], last);
            }
        };
        container = next;
    }
    Ok(None)
}

/// Uncomments the line (or block) for the last of `path` inside `object`, if the file
/// has one and uncommenting it leaves the document as it was plus that member.
fn uncomment_member(
    text: &str,
    kind: RootKind,
    root: &CstRootNode,
    object: &CstObject,
    path: &[String],
    last: bool,
) -> Result<Option<String>, Fail> {
    let _ = root;
    let Some(key) = path.last() else {
        return Ok(None);
    };
    let Some((comment, commented)) = find_commented(object, key) else {
        return Ok(None);
    };
    let raw = comment.raw_value();
    let start = offset_of(&CstNode::from(comment.clone()));
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();

    let member = match &commented {
        Commented::Single(value) => {
            if !last && !value.is_object() && !value.is_array() {
                return Ok(None);
            }
            let comma = comma_after(object, &comment, raw.trim_end().ends_with(','));
            edits.push((start..start + raw.len(), uncommented(&raw, comma)));
            value.clone()
        }
        Commented::Open(open) => {
            let Some(closer) = find_closer(&comment, *open) else {
                return Ok(None);
            };
            let closer_raw = closer.raw_value();
            let closer_start = offset_of(&CstNode::from(closer.clone()));
            let comma = comma_after(object, &closer, closer_raw.trim_end().ends_with(','));
            edits.push((start..start + raw.len(), uncommented(&raw, false)));
            edits.push((
                closer_start..closer_start + closer_raw.len(),
                uncommented(&closer_raw, comma),
            ));
            if *open == '{' {
                Value::Object(Map::new())
            } else {
                Value::Array(Vec::new())
            }
        }
    };

    // The member before it needs a comma now.
    let previous = comment
        .previous_siblings()
        .find_map(|sibling| sibling.as_object_prop());
    if let Some(previous) = previous
        && previous.trailing_comma().is_none()
    {
        let end =
            offset_of(&CstNode::from(previous.clone())) + CstNode::from(previous).to_string().len();
        edits.push((end..end, ",".to_owned()));
    }

    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    let mut new_text = text.to_owned();
    for (range, replacement) in edits {
        new_text.replace_range(range, &replacement);
    }

    // Keep it only if the document is the old one plus that member.
    let mut expected = parse_value(text, kind)?;
    if !insert_member(&mut expected, path, member) {
        return Ok(None);
    }
    match parse_value(&new_text, kind) {
        Ok(actual) if values_equal(&actual, &expected) => Ok(Some(new_text)),
        _ => Ok(None),
    }
}

/// Inserts `member` at `path` (all but the last step must exist).
fn insert_member(root: &mut Value, path: &[String], member: Value) -> bool {
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    let mut cursor = root;
    for segment in parents {
        let next = match cursor {
            Value::Object(map) => map.get_mut(segment),
            Value::Array(items) => parse_index(segment).and_then(|at| items.get_mut(at)),
            _ => None,
        };
        match next {
            Some(next) => cursor = next,
            None => return false,
        }
    }
    match cursor {
        Value::Object(map) => {
            map.insert(last.clone(), member);
            true
        }
        _ => false,
    }
}
