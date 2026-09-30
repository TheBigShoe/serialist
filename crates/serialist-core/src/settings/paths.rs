//! Where the configuration lives on disk.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::defaults::DEFAULT_SETTINGS_JSONC;
use crate::keymap::Keymap;

/// Set to a directory to use it as the config directory on any platform.
pub const CONFIG_DIR_ENV: &str = "SERIALIST_CONFIG_DIR";

/// The operating systems the defaults differ on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
}

impl Platform {
    /// The platform this binary was built for. Anything that is not macOS or Windows
    /// counts as Linux.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::MacOs
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// The user's config directory and the files inside it.
///
/// The directory is `~/.config/serialist` on macOS and Linux (Zed uses `~/.config/zed`
/// on macOS too, so a Zed user finds it where they expect) and `%APPDATA%\Serialist` on
/// Windows. `SERIALIST_CONFIG_DIR` overrides both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    /// The config directory.
    pub dir: PathBuf,
    /// `settings.json` in `dir`.
    pub settings: PathBuf,
    /// `keymap.json` in `dir`.
    pub keymap: PathBuf,
    /// `themes/` in `dir`: Zed theme family files, one `*.json` each.
    pub themes: PathBuf,
    /// A project-local `.serialist/settings.json`, when one was found.
    pub project_settings: Option<PathBuf>,
    /// A project-local `.serialist/commands.json`, when one was found.
    pub project_commands: Option<PathBuf>,
}

impl ConfigPaths {
    /// Paths inside `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            settings: dir.join("settings.json"),
            keymap: dir.join("keymap.json"),
            themes: dir.join("themes"),
            project_settings: None,
            project_commands: None,
            dir,
        }
    }

    /// The directory for this platform, honoring `SERIALIST_CONFIG_DIR`.
    pub fn default_for_platform() -> Self {
        Self::from_environment(Platform::current(), &|name| std::env::var_os(name))
    }

    /// Like [`default_for_platform`](Self::default_for_platform) with the platform and
    /// environment supplied, so the rules can be tested on any host.
    pub fn from_environment(platform: Platform, env: &dyn Fn(&str) -> Option<OsString>) -> Self {
        let lookup = |name: &str| env(name).filter(|value| !value.is_empty());
        if let Some(dir) = lookup(CONFIG_DIR_ENV) {
            return Self::new(dir);
        }
        let dir = match platform {
            Platform::MacOs | Platform::Linux => {
                lookup("HOME").map(|home| PathBuf::from(home).join(".config").join("serialist"))
            }
            Platform::Windows => lookup("APPDATA")
                .map(PathBuf::from)
                .or_else(|| {
                    lookup("USERPROFILE")
                        .map(|profile| PathBuf::from(profile).join("AppData").join("Roaming"))
                })
                .map(|base| base.join("Serialist")),
        };
        Self::new(dir.unwrap_or_else(|| {
            tracing::warn!("no home directory found; keeping the config in the temp directory");
            std::env::temp_dir().join("serialist-config")
        }))
    }

    /// `commands/` in `dir`: saved-command collections, one `*.json` each.
    pub fn commands_dir(&self) -> PathBuf {
        self.dir.join("commands")
    }

    /// `history.jsonl` in `dir`: the compose bar's history, one JSON string per line.
    pub fn history_path(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    /// `scripts/` in `dir`: Lua scripts, `*.lua` at any depth. The Script console lists
    /// them, `scripts::Run` paths and saved-command `script` payloads are relative to
    /// it, and it is the only directory a script's `require` and `dofile` read from.
    pub fn scripts_dir(&self) -> PathBuf {
        self.dir.join("scripts")
    }

    /// `plugins/` in `dir`: codec plugins, one folder each (`plugins/<name>/plugin.lua`).
    /// The app registers each under its folder's name, so `plugins/airoha-race/`
    /// replaces the built-in `airoha-race` codec and `plugins/my-proto/` adds
    /// `my-proto`. Saving a file in there reloads the plugins
    /// ([`ConfigEvent::Plugins`](crate::ConfigEvent::Plugins)).
    pub fn plugins_dir(&self) -> PathBuf {
        self.dir.join("plugins")
    }

    /// The example scripts that ship with the app, as `(file name, source)`.
    pub const EXAMPLE_SCRIPTS: &'static [(&'static str, &'static str)] = &[
        (
            "version_probe.lua",
            include_str!("../../assets/scripts/version_probe.lua"),
        ),
        (
            "firehose_stats.lua",
            include_str!("../../assets/scripts/firehose_stats.lua"),
        ),
    ];

    /// Creates `scripts/` and, if it holds no `*.lua` file directly inside it, writes
    /// the [example scripts](Self::EXAMPLE_SCRIPTS) there. Returns the files it wrote.
    ///
    /// So a new config directory gets the examples the first time the scripts folder is
    /// opened, and a folder with scripts of the user's own is never touched. A file
    /// that already exists is never overwritten.
    ///
    /// Safe to call while a [`ConfigWatcher`](crate::ConfigWatcher) runs: it creates
    /// the folder at spawn too, `create_dir_all` accepts a folder another thread made
    /// first, and a scripts folder made after its watch went away (deleted while the app
    /// ran) is watched again and reported as changed, examples and all. The caller may
    /// still list the folder itself right after, as the app's "Open scripts folder"
    /// does, rather than wait for the watcher.
    pub fn ensure_example_scripts(&self) -> io::Result<Vec<PathBuf>> {
        let dir = self.scripts_dir();
        std::fs::create_dir_all(&dir)?;
        let has_scripts = std::fs::read_dir(&dir)?.any(|entry| {
            entry.is_ok_and(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lua"))
            })
        });
        if has_scripts {
            return Ok(Vec::new());
        }
        let mut written = Vec::new();
        for (name, source) in Self::EXAMPLE_SCRIPTS {
            let path = dir.join(name);
            if self.create_new(&path, source)? {
                written.push(path);
            }
        }
        Ok(written)
    }

    /// Adds the project settings and commands files found by searching up from `cwd`.
    pub fn with_project_from(mut self, cwd: &Path) -> Self {
        self.project_settings = Self::project_settings_path(cwd);
        self.project_commands = Self::project_commands_path(cwd);
        self
    }

    /// The nearest `.serialist/settings.json` in `cwd` or any of its ancestors.
    pub fn project_settings_path(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .map(|dir| dir.join(".serialist").join("settings.json"))
            .find(|candidate| candidate.is_file())
    }

    /// The nearest `.serialist/commands.json` in `cwd` or any of its ancestors.
    pub fn project_commands_path(cwd: &Path) -> Option<PathBuf> {
        cwd.ancestors()
            .map(|dir| dir.join(".serialist").join("commands.json"))
            .find(|candidate| candidate.is_file())
    }

    /// Writes the commented settings template if `settings.json` does not exist,
    /// creating the config directory as needed. Returns whether it wrote the file.
    /// An existing file is never touched.
    pub fn ensure_settings_file(&self) -> io::Result<bool> {
        self.create_new(&self.settings, &settings_template())
    }

    /// Writes the commented keymap template if `keymap.json` does not exist, creating
    /// the config directory as needed. Returns whether it wrote the file. An existing
    /// file is never touched. The template is an empty keymap, so it changes nothing
    /// until a section is uncommented; see [`keymap_template`].
    pub fn ensure_keymap_file(&self) -> io::Result<bool> {
        self.create_new(&self.keymap, &keymap_template(Platform::current()))
    }

    fn create_new(&self, path: &Path, text: &str) -> io::Result<bool> {
        std::fs::create_dir_all(&self.dir)?;
        match OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                Ok(true)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// A keymap file that is valid and changes nothing: the header explains the format, and
/// the bundled defaults for `platform` follow with every line commented out, so a section
/// can be uncommented and edited to override it. The array brackets stay live.
pub fn keymap_template(platform: Platform) -> String {
    let mut out = String::from(
        "// Serialist key bindings, in Zed's keymap format: a list of sections, each with an\n\
         // optional key context and a map of keystrokes to actions. This file is applied\n\
         // after the bundled defaults, so its bindings win. Bind a key to null to unbind it.\n\
         // The defaults for this platform are listed below, commented out: uncomment a\n\
         // section and change a key to override it. A saved command can also carry its own\n\
         // \"keybinding\" in its commands/*.json file.\n\n",
    );
    let source = Keymap::bundled_source(platform);
    // Drop the bundled file's own header, which describes the bundled copy.
    let body = source.split_once("\n[\n").map_or(source, |(_, rest)| rest);
    out.push_str("[\n");
    for line in body.lines() {
        let trimmed = line.trim_start();
        // The outer closing bracket is the only line in column 0 that is code.
        if line == "]" || trimmed.is_empty() || trimmed.starts_with("//") {
            out.push_str(line);
        } else {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("// ");
            out.push_str(trimmed);
        }
        out.push('\n');
    }
    out
}

/// The bundled defaults with every setting commented out, so the file is valid, changes
/// nothing, and shows each key to uncomment.
pub fn settings_template() -> String {
    let mut out = String::from(
        "// Serialist settings. Everything below is commented out: remove the leading\n\
         // `//` from a line to override that default. Comments and trailing commas are\n\
         // fine, and the key names are Zed's. A project can override these keys with a\n\
         // .serialist/settings.json of its own.\n\n",
    );
    // Drop the defaults file's own header, which describes the bundled copy.
    let body = DEFAULT_SETTINGS_JSONC
        .split_once("\n{\n")
        .map_or(DEFAULT_SETTINGS_JSONC, |(_, rest)| rest);
    out.push_str("{\n");
    for line in body.lines() {
        let trimmed = line.trim_start();
        // The outer closing brace stays live so the template is an empty object.
        let is_code = !trimmed.is_empty() && !trimmed.starts_with("//") && line.starts_with(' ');
        if is_code {
            // Comment out after the indent so a line uncomments by deleting `// `.
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("// ");
            out.push_str(trimmed);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    #[test]
    fn scripts_live_in_the_config_directory() {
        let paths = ConfigPaths::new("/cfg/serialist");
        assert_eq!(paths.scripts_dir(), Path::new("/cfg/serialist/scripts"));
    }

    #[test]
    fn the_examples_go_into_a_folder_without_scripts_only() {
        let root = TempDir::new("example-scripts");
        let paths = ConfigPaths::new(root.path().join("config"));
        let written = paths.ensure_example_scripts().unwrap();
        let names: Vec<_> = written
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["version_probe.lua", "firehose_stats.lua"]);
        let probe = std::fs::read_to_string(paths.scripts_dir().join("version_probe.lua"));
        assert!(probe.unwrap().contains(r"^\+VER: (\S+)"));

        // Once there are scripts, nothing is written again, even a deleted example.
        std::fs::remove_file(paths.scripts_dir().join("firehose_stats.lua")).unwrap();
        assert!(paths.ensure_example_scripts().unwrap().is_empty());
        assert!(!paths.scripts_dir().join("firehose_stats.lua").exists());

        // A folder with only the user's own scripts is left alone too.
        let other = ConfigPaths::new(root.path().join("other"));
        root.write("other/scripts/mine.LUA", "print(1)");
        assert!(other.ensure_example_scripts().unwrap().is_empty());
        assert!(!other.scripts_dir().join("version_probe.lua").exists());
    }
}
