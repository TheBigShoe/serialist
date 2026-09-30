//! Key bindings in Zed's keymap format, with no GPUI dependency.
//!
//! A keymap file is a JSON array of sections, each an optional key context and a map of
//! keystrokes to actions:
//!
//! ```jsonc
//! [
//!   { "context": "Terminal",
//!     "bindings": {
//!       "cmd-k": "terminal::Clear",                    // an action by name
//!       "cmd-p": ["terminal::Pause", { "arg": 1 }],   // an action with arguments
//!       "ctrl-x": null                                 // unbind
//!     } }
//! ]
//! ```
//!
//! [`load_keymap`] returns the bundled defaults for the platform followed by the user's
//! entries, in that order, so applying [`Keymap::entries`] front to back gives the
//! user's bindings precedence. Turning entries into GPUI bindings is the UI's job.

use std::io;
use std::path::{Path, PathBuf};

use jsonc_parser::ParseOptions;
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::settings::Platform;

const MACOS_DEFAULTS: &str = include_str!("../assets/keymaps/macos.json");
const LINUX_DEFAULTS: &str = include_str!("../assets/keymaps/linux.json");
const WINDOWS_DEFAULTS: &str = include_str!("../assets/keymaps/windows.json");

/// An action named in a keymap, with optional arguments.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRef {
    /// `namespace::Action`, such as `terminal::Clear`.
    pub name: String,
    /// The JSON argument of the `[name, args]` form. A `null` argument reads as `None`.
    pub args: Option<Value>,
}

impl ActionRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            args: None,
        }
    }
}

/// One keystroke sequence bound to an action in a context.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyBinding {
    /// The key context predicate, such as `Terminal` or `ComposeBar > Input`. `None`
    /// binds everywhere.
    pub context: Option<String>,
    /// One or more space-separated keystrokes, such as `cmd-k` or `ctrl-x ctrl-c`.
    pub keystrokes: String,
    /// The action to run. `None` means unbind: the entry cancels any earlier binding of
    /// the same keystrokes in the same context.
    pub action: Option<ActionRef>,
    /// The section's `use_key_equivalents` flag: match keys by their equivalents on
    /// non-Latin layouts.
    pub use_key_equivalents: bool,
}

impl KeyBinding {
    /// A binding of `keystrokes` to `action` in `context`.
    pub fn bind(context: Option<&str>, keystrokes: &str, action: &str) -> Self {
        Self {
            context: context.map(str::to_string),
            keystrokes: keystrokes.to_string(),
            action: Some(ActionRef::new(action)),
            use_key_equivalents: false,
        }
    }

    /// An entry that unbinds `keystrokes` in `context`.
    pub fn unbind(context: Option<&str>, keystrokes: &str) -> Self {
        Self {
            action: None,
            ..Self::bind(context, keystrokes, "")
        }
    }
}

/// Bindings in the order they apply. Later entries take precedence over earlier ones.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Keymap {
    pub entries: Vec<KeyBinding>,
}

/// Why a keymap could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// Bad syntax or a value of the wrong shape. `line` and `column` count from 1.
    #[error("{}:{line}:{column}: {message}", file.display())]
    Invalid {
        file: PathBuf,
        line: usize,
        column: usize,
        message: String,
    },
}

// ---- Parsing ----

/// A binding's target: an action, or `null` for unbind.
enum ActionSpec {
    Unbind,
    Action(ActionRef),
}

impl<'de> Deserialize<'de> for ActionSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ActionVisitor;

        impl<'de> Visitor<'de> for ActionVisitor {
            type Value = ActionSpec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an action name, [action name, arguments], or null to unbind")
            }

            fn visit_unit<E: de::Error>(self) -> Result<ActionSpec, E> {
                Ok(ActionSpec::Unbind)
            }

            fn visit_none<E: de::Error>(self) -> Result<ActionSpec, E> {
                Ok(ActionSpec::Unbind)
            }

            fn visit_str<E: de::Error>(self, name: &str) -> Result<ActionSpec, E> {
                action_name(name).map(|name| ActionSpec::Action(ActionRef { name, args: None }))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<ActionSpec, A::Error> {
                let name: String = seq.next_element()?.ok_or_else(|| {
                    de::Error::custom("an action list needs the action name first")
                })?;
                let name = action_name(&name)?;
                // A sequence must not be polled again once it has said it is finished.
                let args = match seq.next_element::<Value>()? {
                    None => None,
                    Some(args) => {
                        if seq.next_element::<de::IgnoredAny>()?.is_some() {
                            return Err(de::Error::custom(
                                "an action list is [action name] or [action name, arguments]",
                            ));
                        }
                        Some(args).filter(|args| !args.is_null())
                    }
                };
                Ok(ActionSpec::Action(ActionRef { name, args }))
            }
        }

        d.deserialize_any(ActionVisitor)
    }
}

fn action_name<E: de::Error>(name: &str) -> Result<String, E> {
    let name = name.trim();
    if name.is_empty() {
        Err(E::custom("an action name cannot be empty"))
    } else {
        Ok(name.to_string())
    }
}

/// A section's `bindings` map, in file order.
#[derive(Default)]
struct Bindings(Vec<(String, ActionSpec)>);

impl<'de> Deserialize<'de> for Bindings {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BindingsVisitor;

        impl<'de> Visitor<'de> for BindingsVisitor {
            type Value = Bindings;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object mapping keystrokes to actions")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Bindings, E> {
                Ok(Bindings::default())
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Bindings, A::Error> {
                let mut bindings = Vec::new();
                while let Some(keystrokes) = map.next_key::<String>()? {
                    let keystrokes = keystrokes.trim().to_string();
                    if keystrokes.is_empty() {
                        return Err(de::Error::custom("a keystroke cannot be empty"));
                    }
                    bindings.push((keystrokes, map.next_value::<ActionSpec>()?));
                }
                Ok(Bindings(bindings))
            }
        }

        d.deserialize_any(BindingsVisitor)
    }
}

#[derive(Deserialize)]
struct Section {
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    use_key_equivalents: Option<bool>,
    #[serde(default)]
    bindings: Bindings,
}

/// The file: a list of sections, or nothing.
struct Sections(Vec<Section>);

impl<'de> Deserialize<'de> for Sections {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct SectionsVisitor;

        impl<'de> Visitor<'de> for SectionsVisitor {
            type Value = Sections;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a keymap: a list of { \"context\", \"bindings\" } objects")
            }

            fn visit_unit<E: de::Error>(self) -> Result<Sections, E> {
                Ok(Sections(Vec::new()))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Sections, A::Error> {
                let mut sections = Vec::new();
                while let Some(section) = seq.next_element::<Section>()? {
                    sections.push(section);
                }
                Ok(Sections(sections))
            }
        }

        d.deserialize_any(SectionsVisitor)
    }
}

impl Keymap {
    /// Parses one keymap document. `origin` names it in errors. Empty text, or text
    /// with only comments, is an empty keymap.
    pub fn parse(text: &str, origin: &Path) -> Result<Keymap, KeymapError> {
        let Sections(sections) = jsonc_parser::parse_to_serde_value(text, &ParseOptions::default())
            .map_err(|err| KeymapError::Invalid {
                file: origin.to_path_buf(),
                line: err.line_display(),
                column: err.column_display(),
                message: err.kind().to_string(),
            })?;
        let mut entries = Vec::new();
        for section in sections {
            let context = section
                .context
                .map(|context| context.trim().to_string())
                .filter(|context| !context.is_empty());
            for (keystrokes, spec) in section.bindings.0 {
                entries.push(KeyBinding {
                    context: context.clone(),
                    keystrokes,
                    action: match spec {
                        ActionSpec::Unbind => None,
                        ActionSpec::Action(action) => Some(action),
                    },
                    use_key_equivalents: section.use_key_equivalents.unwrap_or(false),
                });
            }
        }
        Ok(Keymap { entries })
    }

    /// The bundled defaults for `platform`.
    pub fn bundled(platform: Platform) -> Keymap {
        let (text, name) = match platform {
            Platform::MacOs => (MACOS_DEFAULTS, "macos.json"),
            Platform::Linux => (LINUX_DEFAULTS, "linux.json"),
            Platform::Windows => (WINDOWS_DEFAULTS, "windows.json"),
        };
        match Keymap::parse(text, Path::new(name)) {
            Ok(keymap) => keymap,
            Err(err) => panic!("the bundled keymap {name} is invalid: {err}"),
        }
    }

    /// The bundled defaults for the platform this binary runs on.
    pub fn bundled_default() -> Keymap {
        Keymap::bundled(Platform::current())
    }

    /// Appends `other`'s entries, which then take precedence over the existing ones.
    pub fn extend(&mut self, other: Keymap) {
        self.entries.extend(other.entries);
    }

    /// The bindings in effect once later entries have overridden earlier ones: the last
    /// entry for each context and keystroke sequence, in the order those last entries
    /// appear, with unbound entries removed.
    pub fn resolved(&self) -> Vec<&KeyBinding> {
        let mut last = std::collections::HashMap::new();
        for (index, entry) in self.entries.iter().enumerate() {
            last.insert((entry.context.as_deref(), entry.keystrokes.as_str()), index);
        }
        self.entries
            .iter()
            .enumerate()
            .filter(|(index, entry)| {
                last.get(&(entry.context.as_deref(), entry.keystrokes.as_str())) == Some(index)
                    && entry.action.is_some()
            })
            .map(|(_, entry)| entry)
            .collect()
    }

    /// The names of every action the keymap binds, without repeats, in first-use order.
    pub fn action_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for action in self
            .entries
            .iter()
            .filter_map(|entry| entry.action.as_ref())
        {
            if !names.contains(&action.name.as_str()) {
                names.push(&action.name);
            }
        }
        names
    }
}

/// Loads the bundled defaults for `platform`, then the user's keymap file if there is
/// one. A missing file adds nothing; a file with errors is an error, so the caller can
/// keep the keymap it already has and report the problem.
pub fn load_keymap_for(
    platform: Platform,
    user_path: Option<&Path>,
) -> Result<Keymap, KeymapError> {
    let mut keymap = Keymap::bundled(platform);
    if let Some(path) = user_path {
        match std::fs::read_to_string(path) {
            Ok(text) => keymap.extend(Keymap::parse(&text, path)?),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(KeymapError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
    Ok(keymap)
}

/// [`load_keymap_for`] the current platform.
pub fn load_keymap(user_path: Option<&Path>) -> Result<Keymap, KeymapError> {
    load_keymap_for(Platform::current(), user_path)
}

#[cfg(test)]
mod tests;
