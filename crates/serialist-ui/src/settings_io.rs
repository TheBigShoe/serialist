//! The Settings view's side of the configuration files: reading `settings.json` (and a
//! project's `.serialist/settings.json`) to say where each value comes from, and writing
//! one key at a time through the comment-preserving editors in `serialist-core`.
//!
//! The files stay the source of truth. Every write goes to disk; the configuration's
//! watcher sees the save and reloads it into the running app like any other edit (see
//! [`config`](crate::config)). A write that leaves the file unloadable is undone at once,
//! the file's previous text put back, and the loader's message returned for the view to
//! show next to the control.

use std::path::{Path, PathBuf};

use serde_json::Value;
use serialist_core::keymap::ActionRef;
use serialist_core::settings::ConfigPaths;
use serialist_core::{Settings, load_keymap, load_settings};

use self::editor::{KeymapEditor, SettingsEditor};

/// Where a setting's value comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Not set in any file: the bundled default.
    Default,
    /// Set in the user's `settings.json`.
    User,
    /// Set in a project's `.serialist/settings.json`, which the view does not edit.
    Project(PathBuf),
}

/// The settings files as they are on disk now: the merged settings and each file's own
/// keys.
pub struct SettingsFiles {
    /// The bundled defaults, the user's file and the project's, merged.
    pub settings: Settings,
    user: Option<SettingsEditor>,
    project: Option<(PathBuf, SettingsEditor)>,
}

impl SettingsFiles {
    /// Read the files `paths` names. `Err` is the loader's message for a file that does
    /// not load (bad syntax, or a value of the wrong type).
    pub fn read(paths: &ConfigPaths) -> Result<SettingsFiles, String> {
        let settings = load_settings(Some(&paths.settings), paths.project_settings.as_deref())
            .map_err(|error| error.to_string())?;
        let user = open_existing(&paths.settings)?;
        let project = match &paths.project_settings {
            Some(path) => open_existing(path)?.map(|editor| (path.clone(), editor)),
            None => None,
        };
        Ok(SettingsFiles {
            settings,
            user,
            project,
        })
    }

    /// Where the value at `pointer` (`/buffer_font_size`, `/display/wrap`) comes from.
    /// A key the project file sets wins over the user's, as the loader layers them.
    pub fn origin(&self, pointer: &str) -> Origin {
        if let Some((path, project)) = &self.project
            && project.get(pointer).is_some()
        {
            return Origin::Project(path.clone());
        }
        if self.user_value(pointer).is_some() {
            Origin::User
        } else {
            Origin::Default
        }
    }

    /// The value the user's file sets at `pointer`, if it sets one.
    pub fn user_value(&self, pointer: &str) -> Option<Value> {
        self.user.as_ref().and_then(|user| user.get(pointer))
    }
}

/// An editor for `path`, or `None` when there is no such file.
fn open_existing(path: &Path) -> Result<Option<SettingsEditor>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    SettingsEditor::open(path)
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// Set `pointer` in the user's `settings.json` to `value`, or remove it (`None`, which
/// puts the default back). The commented template is written first if there is no file.
/// If the file does not load afterwards, its previous text is put back and the loader's
/// message returned.
pub fn write_setting(
    paths: &ConfigPaths,
    pointer: &str,
    value: Option<Value>,
) -> Result<(), String> {
    paths
        .ensure_settings_file()
        .map_err(|error| format!("{}: {error}", paths.settings.display()))?;
    let before = std::fs::read_to_string(&paths.settings)
        .map_err(|error| format!("{}: {error}", paths.settings.display()))?;
    let written = match value {
        Some(value) => SettingsEditor::set_in_file(&paths.settings, pointer, value)
            .map_err(|error| error.to_string()),
        None => SettingsEditor::open(&paths.settings)
            .and_then(|mut editor| {
                editor.remove(pointer)?;
                editor.save()
            })
            .map_err(|error| error.to_string()),
    };
    let checked = written.and_then(|()| {
        load_settings(Some(&paths.settings), paths.project_settings.as_deref())
            .map(drop)
            .map_err(|error| error.to_string())
    });
    if let Err(message) = checked {
        tracing::warn!(pointer, %message, "undoing a settings write that does not load");
        if let Err(error) = std::fs::write(&paths.settings, before) {
            tracing::error!(%error, "could not put settings.json back");
        }
        return Err(message);
    }
    tracing::info!(pointer, "wrote a setting");
    Ok(())
}

/// Bind `keystrokes` to `action` in `context` in the user's `keymap.json` and take
/// `replaces` (the chord the action had there) off it: removed when the user's own file
/// bound it, else bound to `null` so the bundled binding stops applying. If the keymap
/// does not load afterwards, its previous text is put back and the loader's message
/// returned.
pub fn rebind(
    paths: &ConfigPaths,
    context: Option<&str>,
    keystrokes: &str,
    action: &ActionRef,
    replaces: Option<(&str, bool)>,
) -> Result<(), String> {
    paths
        .ensure_keymap_file()
        .map_err(|error| format!("{}: {error}", paths.keymap.display()))?;
    let before = std::fs::read_to_string(&paths.keymap)
        .map_err(|error| format!("{}: {error}", paths.keymap.display()))?;
    let written = KeymapEditor::open(&paths.keymap)
        .and_then(|mut editor| {
            if let Some((old, user_owned)) = replaces
                && old != keystrokes
            {
                if user_owned {
                    editor.remove(context, old)?;
                } else {
                    editor.unbind(context, old)?;
                }
            }
            editor.bind(context, keystrokes, action)?;
            editor.save()
        })
        .map_err(|error| error.to_string());
    let checked = written.and_then(|()| {
        load_keymap(Some(&paths.keymap))
            .map(drop)
            .map_err(|error| error.to_string())
    });
    if let Err(message) = checked {
        tracing::warn!(keystrokes, %message, "undoing a keymap write that does not load");
        if let Err(error) = std::fs::write(&paths.keymap, before) {
            tracing::error!(%error, "could not put keymap.json back");
        }
        return Err(message);
    }
    tracing::info!(keystrokes, action = %action.name, "rebound a key");
    Ok(())
}

/// TEMPORARY stand-in for `serialist_core::settings::{SettingsEditor, KeymapEditor}`
/// with the same signatures, until that branch lands. It does not keep comments.
mod editor {
    use std::fmt;
    use std::path::{Path, PathBuf};

    use serde_json::{Map, Value};
    use serialist_core::keymap::ActionRef;

    #[derive(Debug)]
    pub struct EditError(String);

    impl fmt::Display for EditError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }

    fn parse(path: &Path) -> Result<Value, EditError> {
        let text = std::fs::read_to_string(path).map_err(|e| EditError(e.to_string()))?;
        jsonc_parser::parse_to_serde_value::<Value>(&text, &Default::default())
            .map_err(|e| EditError(e.to_string()))
    }

    fn tokens(pointer: &str) -> Vec<String> {
        pointer
            .split('/')
            .skip(1)
            .map(|t| t.replace("~1", "/").replace("~0", "~"))
            .collect()
    }

    pub struct SettingsEditor {
        path: PathBuf,
        doc: Value,
    }

    impl SettingsEditor {
        pub fn open(path: &Path) -> Result<Self, EditError> {
            let doc = match parse(path)? {
                Value::Null => Value::Object(Map::new()),
                doc => doc,
            };
            Ok(Self {
                path: path.to_path_buf(),
                doc,
            })
        }

        pub fn get(&self, pointer: &str) -> Option<Value> {
            self.doc.pointer(pointer).cloned()
        }

        pub fn set(&mut self, pointer: &str, value: Value) -> Result<(), EditError> {
            let tokens = tokens(pointer);
            let mut node = &mut self.doc;
            for (ix, token) in tokens.iter().enumerate() {
                let last = ix + 1 == tokens.len();
                match node {
                    Value::Object(map) => {
                        if last {
                            map.insert(token.clone(), value);
                            return Ok(());
                        }
                        let child = map
                            .entry(token.clone())
                            .or_insert_with(|| Value::Object(Map::new()));
                        if !child.is_object() && !child.is_array() {
                            *child = Value::Object(Map::new());
                        }
                        node = child;
                    }
                    Value::Array(items) => {
                        let index: usize = token
                            .parse()
                            .map_err(|_| EditError(format!("bad index {token}")))?;
                        if index > items.len() {
                            return Err(EditError(format!("index {index} out of range")));
                        }
                        if index == items.len() {
                            items.push(Value::Null);
                        }
                        if last {
                            items[index] = value;
                            return Ok(());
                        }
                        node = &mut items[index];
                    }
                    _ => return Err(EditError("not a container".into())),
                }
            }
            self.doc = value;
            Ok(())
        }

        pub fn remove(&mut self, pointer: &str) -> Result<(), EditError> {
            let tokens = tokens(pointer);
            let Some((last, parents)) = tokens.split_last() else {
                return Ok(());
            };
            let parent = format!(
                "{}",
                parents
                    .iter()
                    .map(|t| format!("/{t}"))
                    .collect::<String>()
            );
            match self.doc.pointer_mut(&parent) {
                Some(Value::Object(map)) => {
                    map.remove(last);
                }
                Some(Value::Array(items)) => {
                    if let Ok(index) = last.parse::<usize>()
                        && index < items.len()
                    {
                        items.remove(index);
                    }
                }
                _ => {}
            }
            Ok(())
        }

        pub fn save(&self) -> Result<(), EditError> {
            let text = serde_json::to_string_pretty(&self.doc).map_err(|e| EditError(e.to_string()))?;
            std::fs::write(&self.path, text + "\n").map_err(|e| EditError(e.to_string()))
        }

        pub fn set_in_file(path: &Path, pointer: &str, value: Value) -> Result<(), EditError> {
            let mut editor = Self::open(path)?;
            editor.set(pointer, value)?;
            editor.save()
        }
    }

    pub struct KeymapEditor {
        path: PathBuf,
        doc: Vec<Value>,
    }

    impl KeymapEditor {
        pub fn open(path: &Path) -> Result<Self, EditError> {
            let doc = match parse(path)? {
                Value::Array(items) => items,
                _ => Vec::new(),
            };
            Ok(Self {
                path: path.to_path_buf(),
                doc,
            })
        }

        fn section(&mut self, context: Option<&str>) -> &mut Map<String, Value> {
            let position = self.doc.iter().position(|section| {
                section.get("context").and_then(Value::as_str) == context
                    && section.get("use_key_equivalents").is_none()
            });
            let ix = match position {
                Some(ix) => ix,
                None => {
                    let mut section = Map::new();
                    if let Some(context) = context {
                        section.insert("context".into(), Value::String(context.into()));
                    }
                    section.insert("bindings".into(), Value::Object(Map::new()));
                    self.doc.push(Value::Object(section));
                    self.doc.len() - 1
                }
            };
            let section = self.doc[ix].as_object_mut().expect("a section");
            section
                .entry("bindings")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("bindings")
        }

        pub fn bind(
            &mut self,
            context: Option<&str>,
            keystrokes: &str,
            action: &ActionRef,
        ) -> Result<(), EditError> {
            let value = match &action.args {
                Some(args) => Value::Array(vec![Value::String(action.name.clone()), args.clone()]),
                None => Value::String(action.name.clone()),
            };
            self.section(context).insert(keystrokes.into(), value);
            Ok(())
        }

        pub fn unbind(&mut self, context: Option<&str>, keystrokes: &str) -> Result<(), EditError> {
            self.section(context).insert(keystrokes.into(), Value::Null);
            Ok(())
        }

        pub fn remove(&mut self, context: Option<&str>, keystrokes: &str) -> Result<(), EditError> {
            self.section(context).remove(keystrokes);
            Ok(())
        }

        pub fn save(&self) -> Result<(), EditError> {
            let text = serde_json::to_string_pretty(&self.doc).map_err(|e| EditError(e.to_string()))?;
            std::fs::write(&self.path, text + "\n").map_err(|e| EditError(e.to_string()))
        }
    }
}
