//! Every collection the app knows, loaded from disk, with the edits the Commands panel
//! makes.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::file::{CommandWarning, CommandsError, write_atomic};
use super::model::{CollectionSource, Command, CommandCollection, CommandGroup, CommandRef};
use crate::settings::ConfigPaths;

/// The bundled examples, `assets/commands/examples.json`.
const EXAMPLES: &str = include_str!("../../assets/commands/examples.json");

impl CommandCollection {
    /// The read-only examples compiled into the binary: AT basics, a command with a
    /// parameter, and a hex payload. Their source is [`CollectionSource::Bundled`].
    pub fn bundled_examples() -> CommandCollection {
        match CommandCollection::parse(
            EXAMPLES,
            CollectionSource::Bundled,
            Path::new("<bundled commands/examples.json>"),
        ) {
            Ok(loaded) => loaded.collection,
            Err(err) => panic!("the bundled example commands are invalid: {err}"),
        }
    }
}

/// Why an edit to the store was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("there is no collection called `{0}`")]
    NoCollection(String),
    #[error("`{collection}` has no group called `{group}`")]
    NoGroup { collection: String, group: String },
    #[error("`{collection}` has no command called `{name}`")]
    NoCommand { collection: String, name: String },
    /// The bundled examples cannot change. Copy a command into a collection of your own.
    #[error("`{0}` is read-only")]
    ReadOnly(String),
    #[error("`{collection}` already has a command called `{name}`")]
    DuplicateCommand { collection: String, name: String },
    #[error("`{collection}` already has a group called `{group}`")]
    DuplicateGroup { collection: String, group: String },
    #[error("there is already a collection called `{0}`")]
    DuplicateCollection(String),
    #[error("a name cannot be empty")]
    EmptyName,
    /// The store was not loaded from a config directory, so it has nowhere to put a new
    /// collection's file.
    #[error("this store has no commands directory")]
    NoDirectory,
}

/// The loaded collections, in the order the panel lists them: the user's files by file
/// name, then the project's file, then the bundled examples.
///
/// Loading never fails. A file that cannot be read or parsed is left out and reported in
/// [`warnings`](Self::warnings), as are problems inside files that did load. Edits change
/// the copy in memory; [`save`](Self::save) or [`save_collection`](Self::save_collection)
/// writes a collection back to its file. Reload with [`CommandStore::load`] when a
/// [`ConfigEvent::Commands`](crate::ConfigEvent::Commands) arrives.
///
/// Collections are found by name. If two share one (a project file named like a user
/// file), lookups and edits reach the first.
#[derive(Clone, Debug, Default)]
pub struct CommandStore {
    collections: Vec<CommandCollection>,
    warnings: Vec<CommandWarning>,
    commands_dir: Option<PathBuf>,
}

impl CommandStore {
    /// A store with nothing in it, not even the bundled examples.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Reads every `*.json` file in the user's commands directory, then the project's
    /// commands file if `paths` names one, then adds the bundled examples. A missing
    /// directory is fine.
    pub fn load(paths: &ConfigPaths) -> CommandStore {
        let dir = paths.commands_dir();
        let mut store = CommandStore {
            commands_dir: Some(dir.clone()),
            ..CommandStore::default()
        };
        for path in store.user_files(&dir) {
            let source = CollectionSource::User(path.clone());
            store.load_file(&path, source);
        }
        if let Some(path) = &paths.project_commands
            && path.is_file()
        {
            store.load_file(path, CollectionSource::Project(path.clone()));
        }
        store
            .collections
            .push(CommandCollection::bundled_examples());
        store.report_conflicts();
        store
    }

    /// The `*.json` files directly in `dir`, by lower-cased file name. Dot files are
    /// skipped: they are an editor's or this store's own temp files.
    fn user_files(&mut self, dir: &Path) -> Vec<PathBuf> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Vec::new(),
            Err(err) => {
                self.warn(dir, format!("cannot read the commands directory: {err}"));
                return Vec::new();
            }
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        files.sort_by_key(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        });
        files
    }

    fn warn(&mut self, file: &Path, message: String) {
        self.warnings.push(CommandWarning {
            file: file.to_path_buf(),
            message,
        });
    }

    fn load_file(&mut self, path: &Path, source: CollectionSource) {
        match CommandCollection::load(path, source) {
            Ok(loaded) => {
                for message in loaded.warnings {
                    self.warn(path, message);
                }
                self.collections.push(loaded.collection);
            }
            Err(CommandsError::Io { source, .. }) => {
                self.warn(path, format!("cannot read the file: {source}"));
            }
            Err(CommandsError::Invalid {
                line,
                column,
                message,
                ..
            }) => self.warn(
                path,
                format!("{line}:{column}: {message}; the file is not loaded"),
            ),
        }
    }

    /// Warnings for names and keystrokes that two files both claim.
    fn report_conflicts(&mut self) {
        let mut found = Vec::new();
        let mut keys: Vec<(String, CommandRef)> = Vec::new();
        for (index, collection) in self.collections.iter().enumerate() {
            let Some(file) = collection.path() else {
                continue;
            };
            if self.collections[..index]
                .iter()
                .any(|earlier| earlier.name == collection.name)
            {
                found.push((
                    file.to_path_buf(),
                    format!(
                        "another collection is also called `{}`; lookups by name reach the first",
                        collection.name
                    ),
                ));
            }
            for (group, command) in collection.commands() {
                let Some(stroke) = command.keybinding.as_deref().map(str::trim) else {
                    continue;
                };
                if stroke.is_empty() {
                    continue;
                }
                let this = collection.command_ref(group, command);
                if let Some((_, other)) = keys.iter().find(|(known, _)| known == stroke) {
                    found.push((
                        file.to_path_buf(),
                        format!("`{stroke}` is also bound to {other}; this later one wins"),
                    ));
                }
                keys.push((stroke.to_owned(), this));
            }
        }
        for (file, message) in found {
            self.warn(&file, message);
        }
    }

    /// Every collection, in panel order.
    pub fn collections(&self) -> &[CommandCollection] {
        &self.collections
    }

    /// What loading and reporting found wrong, in the order it was found.
    pub fn warnings(&self) -> &[CommandWarning] {
        &self.warnings
    }

    /// The directory new collections are created in.
    pub fn commands_dir(&self) -> Option<&Path> {
        self.commands_dir.as_deref()
    }

    /// The first collection called `name`.
    pub fn collection(&self, name: &str) -> Option<&CommandCollection> {
        self.collections.iter().find(|c| c.name == name)
    }

    /// The first command called `name` in the collection called `collection`, in any
    /// group.
    pub fn find(&self, collection: &str, name: &str) -> Option<(CommandRef, &Command)> {
        let collection = self.collection(collection)?;
        let (group, command) = collection.find(name)?;
        Some((collection.command_ref(group, command), command))
    }

    /// The command `reference` names.
    pub fn get(&self, reference: &CommandRef) -> Option<&Command> {
        self.collection(&reference.collection)?
            .group(&reference.group)?
            .commands
            .iter()
            .find(|command| command.name == reference.name)
    }

    /// Every command, in panel order.
    pub fn commands(&self) -> impl Iterator<Item = (CommandRef, &Command)> {
        self.collections.iter().flat_map(|collection| {
            collection
                .commands()
                .map(|(group, command)| (collection.command_ref(group, command), command))
        })
    }

    /// The keystrokes commands are bound to, in panel order. If two commands share
    /// keystrokes, both are listed and the later should win, as in a keymap.
    pub fn all_keybindings(&self) -> Vec<(String, CommandRef)> {
        self.commands()
            .filter_map(|(reference, command)| {
                let keys = command.keybinding.as_deref()?.trim();
                (!keys.is_empty()).then(|| (keys.to_owned(), reference))
            })
            .collect()
    }

    // ---- Saving ----

    /// Writes `collection` to its file, atomically (a temp file, then a rename) and
    /// creating the directory if needed. The file is rewritten as plain JSON: comments
    /// and unknown keys in it are lost. The bundled collection is refused with
    /// [`io::ErrorKind::PermissionDenied`]. This touches only the file; the store's own
    /// copy is not changed (see [`set_collection`](Self::set_collection)).
    pub fn save(&self, collection: &CommandCollection) -> io::Result<()> {
        let Some(path) = collection.path() else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the bundled examples are read-only; keep your commands in a collection of your own",
            ));
        };
        write_atomic(path, collection.to_json().as_bytes())
    }

    /// [`save`](Self::save) the store's copy of the collection called `name`.
    pub fn save_collection(&self, name: &str) -> io::Result<()> {
        match self.collection(name) {
            Some(collection) => self.save(collection),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                EditError::NoCollection(name.to_owned()).to_string(),
            )),
        }
    }

    // ---- Editing (in memory) ----

    /// Replaces the collection that came from the same file with `collection`, or adds
    /// it in its place in the panel order. The bundled examples are never replaced.
    pub fn set_collection(&mut self, collection: CommandCollection) {
        let same = |c: &CommandCollection| {
            c.source != CollectionSource::Bundled && c.source == collection.source
        };
        if let Some(slot) = self.collections.iter_mut().find(|c| same(c)) {
            *slot = collection;
            return;
        }
        let at = match collection.source {
            CollectionSource::User(_) => self
                .collections
                .iter()
                .position(|c| !matches!(c.source, CollectionSource::User(_))),
            CollectionSource::Project(_) => self
                .collections
                .iter()
                .position(|c| c.source == CollectionSource::Bundled),
            CollectionSource::Bundled => None,
        }
        .unwrap_or(self.collections.len());
        self.collections.insert(at, collection);
    }

    /// Adds an empty user collection called `name`, whose file will be
    /// `<commands dir>/<name as a slug>.json` (numbered if that exists). Nothing is
    /// written until it is saved.
    pub fn create_collection(&mut self, name: &str) -> Result<&CommandCollection, EditError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(EditError::EmptyName);
        }
        if self.collection(name).is_some() {
            return Err(EditError::DuplicateCollection(name.to_owned()));
        }
        let dir = self.commands_dir.clone().ok_or(EditError::NoDirectory)?;
        let slug = slug(name);
        let taken =
            |path: &Path| path.exists() || self.collections.iter().any(|c| c.path() == Some(path));
        let mut path = dir.join(format!("{slug}.json"));
        let mut number = 2;
        while taken(&path) {
            path = dir.join(format!("{slug}-{number}.json"));
            number += 1;
        }
        let source = CollectionSource::User(path);
        self.set_collection(CommandCollection::new(name, source.clone()));
        Ok(self
            .collections
            .iter()
            .find(|c| c.source == source)
            .expect("the collection was just added"))
    }

    fn editable(&mut self, name: &str) -> Result<&mut CommandCollection, EditError> {
        let collection = self
            .collections
            .iter_mut()
            .find(|c| c.name == name)
            .ok_or_else(|| EditError::NoCollection(name.to_owned()))?;
        if collection.is_read_only() {
            return Err(EditError::ReadOnly(name.to_owned()));
        }
        Ok(collection)
    }

    /// Adds `command` to `group` of `collection`, creating the group if it is missing.
    /// Command names are unique within a collection. Returns the new command's
    /// reference.
    pub fn add_command(
        &mut self,
        collection: &str,
        group: &str,
        command: Command,
    ) -> Result<CommandRef, EditError> {
        let target = self.editable(collection)?;
        let group = group.trim();
        if group.is_empty() || command.name.trim().is_empty() {
            return Err(EditError::EmptyName);
        }
        if target.find(&command.name).is_some() {
            return Err(EditError::DuplicateCommand {
                collection: collection.to_owned(),
                name: command.name,
            });
        }
        let reference = CommandRef::new(collection, group, &command.name);
        match target.groups.iter_mut().find(|g| g.name == group) {
            Some(existing) => existing.commands.push(command),
            None => target.groups.push(CommandGroup {
                name: group.to_owned(),
                commands: vec![command],
            }),
        }
        Ok(reference)
    }

    /// Replaces the command `target` names with `command`, in place. The new command may
    /// have a new name, if no other command in the collection has it. Returns its
    /// reference.
    pub fn update_command(
        &mut self,
        target: &CommandRef,
        command: Command,
    ) -> Result<CommandRef, EditError> {
        let collection = self.editable(&target.collection)?;
        if command.name.trim().is_empty() {
            return Err(EditError::EmptyName);
        }
        let renamed = command.name != target.name;
        if renamed && collection.find(&command.name).is_some() {
            return Err(EditError::DuplicateCommand {
                collection: target.collection.clone(),
                name: command.name,
            });
        }
        let group = group_mut(collection, target)?;
        let slot = group
            .commands
            .iter_mut()
            .find(|c| c.name == target.name)
            .ok_or_else(|| no_command(target))?;
        let reference = CommandRef::new(&target.collection, &target.group, &command.name);
        *slot = command;
        Ok(reference)
    }

    /// Removes the command `target` names and returns it. An emptied group stays.
    pub fn remove_command(&mut self, target: &CommandRef) -> Result<Command, EditError> {
        let collection = self.editable(&target.collection)?;
        let group = group_mut(collection, target)?;
        let at = group
            .commands
            .iter()
            .position(|c| c.name == target.name)
            .ok_or_else(|| no_command(target))?;
        Ok(group.commands.remove(at))
    }

    /// Renames the command `target` names. Returns its new reference.
    pub fn rename_command(
        &mut self,
        target: &CommandRef,
        new_name: &str,
    ) -> Result<CommandRef, EditError> {
        let mut command = self
            .get(target)
            .cloned()
            .ok_or_else(|| no_command(target))?;
        command.name = new_name.trim().to_owned();
        self.update_command(target, command)
    }

    /// Renames a group.
    pub fn rename_group(
        &mut self,
        collection: &str,
        group: &str,
        new_name: &str,
    ) -> Result<(), EditError> {
        let new_name = new_name.trim();
        let target = self.editable(collection)?;
        if new_name.is_empty() {
            return Err(EditError::EmptyName);
        }
        if new_name != group && target.group(new_name).is_some() {
            return Err(EditError::DuplicateGroup {
                collection: collection.to_owned(),
                group: new_name.to_owned(),
            });
        }
        let found = target
            .groups
            .iter_mut()
            .find(|g| g.name == group)
            .ok_or_else(|| EditError::NoGroup {
                collection: collection.to_owned(),
                group: group.to_owned(),
            })?;
        found.name = new_name.to_owned();
        Ok(())
    }

    /// Removes a group and the commands in it.
    pub fn remove_group(&mut self, collection: &str, group: &str) -> Result<(), EditError> {
        let target = self.editable(collection)?;
        let at = target
            .groups
            .iter()
            .position(|g| g.name == group)
            .ok_or_else(|| EditError::NoGroup {
                collection: collection.to_owned(),
                group: group.to_owned(),
            })?;
        target.groups.remove(at);
        Ok(())
    }

    /// Renames a collection. Its file keeps its name; only the `name` inside changes.
    pub fn rename_collection(&mut self, collection: &str, new_name: &str) -> Result<(), EditError> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(EditError::EmptyName);
        }
        if new_name != collection && self.collection(new_name).is_some() {
            return Err(EditError::DuplicateCollection(new_name.to_owned()));
        }
        self.editable(collection)?.name = new_name.to_owned();
        Ok(())
    }
}

fn group_mut<'a>(
    collection: &'a mut CommandCollection,
    target: &CommandRef,
) -> Result<&'a mut CommandGroup, EditError> {
    collection
        .groups
        .iter_mut()
        .find(|g| g.name == target.group)
        .ok_or_else(|| EditError::NoGroup {
            collection: target.collection.clone(),
            group: target.group.clone(),
        })
}

fn no_command(target: &CommandRef) -> EditError {
    EditError::NoCommand {
        collection: target.collection.clone(),
        name: target.name.clone(),
    }
}

/// A file-name-safe form of `name`: lower-case letters and digits, dashes between.
fn slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            slug.extend(c.to_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "commands".to_owned()
    } else {
        slug.to_owned()
    }
}
