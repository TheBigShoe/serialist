//! The Settings view's side of the configuration files: reading `settings.json` (and a
//! project's `.serialist/settings.json`) to say where each value comes from, and writing
//! one key at a time through the comment-preserving editors in `serialist-core`.
//!
//! The files stay the source of truth. Every write goes to disk; the configuration's
//! watcher sees the save and reloads it into the running app like any other edit (see
//! [`config`](crate::config)). Each edit is made in memory first and loaded the way the
//! app loads the file; one the loader rejects is never saved, and the loader's message is
//! returned for the view to show next to the control. Should the saved file fail to load
//! anyway (another program wrote it meanwhile), its previous text is put back.

use std::path::{Path, PathBuf};

use serde_json::Value;
use serialist_core::settings::{ConfigPaths, SettingsLayer, load_settings_from_layers};
use serialist_core::{
    ActionRef, Keymap, KeymapEditor, Settings, SettingsEditError, SettingsEditor, load_keymap,
    load_settings,
};

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

    /// The bundled defaults alone, for a configuration not read from a directory: nothing
    /// is read, and every key is the default.
    pub fn bundled() -> SettingsFiles {
        SettingsFiles {
            settings: Settings::default(),
            user: None,
            project: None,
        }
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

/// How often an edit is tried again when the file changed on disk between reading and
/// saving it (an editor saved at the same moment).
const ATTEMPTS: usize = 3;

/// The project layer as the loader takes it, if there is one.
fn project_layer(paths: &ConfigPaths) -> Result<Option<SettingsLayer>, String> {
    let Some(path) = &paths.project_settings else {
        return Ok(None);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(SettingsLayer::new(path, text))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Set `pointer` in the user's `settings.json` to `value`, or remove it (`None`, which
/// puts the default back, and drops an enclosing object the removal left empty). A file
/// that does not exist is first written from the commented template, so the key's
/// commented line is uncommented in place. `Err` is the loader's (or the editor's)
/// message; the file is then as it was.
pub fn write_setting(
    paths: &ConfigPaths,
    pointer: &str,
    value: Option<Value>,
) -> Result<(), String> {
    let project = project_layer(paths)?;
    for attempt in 1..=ATTEMPTS {
        let mut editor =
            SettingsEditor::open(&paths.settings).map_err(|error| error.to_string())?;
        let before = editor.text().to_owned();
        match &value {
            Some(value) => editor.set(pointer, value.clone()),
            None => editor.remove_and_prune(pointer).map(drop),
        }
        .map_err(|error| error.to_string())?;
        if !editor.is_dirty() {
            return Ok(());
        }
        // Load the edited text as the app will before it reaches the disk.
        let mut layers = vec![SettingsLayer::new(&paths.settings, editor.text())];
        layers.extend(project.clone());
        load_settings_from_layers(&layers).map_err(|error| error.to_string())?;
        match editor.save() {
            Ok(()) => {}
            Err(SettingsEditError::Changed { .. }) if attempt < ATTEMPTS => {
                tracing::debug!(pointer, "settings.json changed on disk; editing again");
                continue;
            }
            Err(error) => return Err(error.to_string()),
        }
        if let Err(error) = load_settings(Some(&paths.settings), paths.project_settings.as_deref())
        {
            tracing::warn!(pointer, %error, "undoing a settings write that does not load");
            if let Err(error) = std::fs::write(&paths.settings, before) {
                tracing::error!(%error, "could not put settings.json back");
            }
            return Err(error.to_string());
        }
        for change in editor.diff_summary() {
            tracing::info!("settings.json: {change}");
        }
        return Ok(());
    }
    Err(format!(
        "{}: the file kept changing on disk; try again",
        paths.settings.display()
    ))
}

/// Bind `keystrokes` to `action` in `context` in the user's `keymap.json`, and take
/// `replaces` (the chord the action had there) off it: removed from the file when the
/// user's own file bound it, so a bundled binding of that chord applies again, else bound
/// to `null` so the bundled binding stops applying. `Err` is the loader's (or the
/// editor's) message; the file is then as it was.
pub fn rebind(
    paths: &ConfigPaths,
    context: Option<&str>,
    keystrokes: &str,
    action: &ActionRef,
    replaces: Option<(&str, bool)>,
) -> Result<(), String> {
    let action_value = match &action.args {
        Some(args) => Value::Array(vec![Value::String(action.name.clone()), args.clone()]),
        None => Value::String(action.name.clone()),
    };
    for attempt in 1..=ATTEMPTS {
        let mut editor = KeymapEditor::open(&paths.keymap).map_err(|error| error.to_string())?;
        if let Some((old, user_owned)) = replaces
            && old != keystrokes
        {
            if user_owned {
                editor.remove(context, old).map(drop)
            } else {
                editor.unbind(context, old)
            }
            .map_err(|error| error.to_string())?;
        }
        editor
            .bind(context, keystrokes, action_value.clone())
            .map_err(|error| error.to_string())?;
        Keymap::parse(editor.text(), &paths.keymap).map_err(|error| error.to_string())?;
        match editor.save() {
            Ok(()) => {}
            Err(SettingsEditError::Changed { .. }) if attempt < ATTEMPTS => continue,
            Err(error) => return Err(error.to_string()),
        }
        load_keymap(Some(&paths.keymap)).map_err(|error| error.to_string())?;
        tracing::info!(keystrokes, action = %action.name, ?context, "rebound a key");
        return Ok(());
    }
    Err(format!(
        "{}: the file kept changing on disk; try again",
        paths.keymap.display()
    ))
}
