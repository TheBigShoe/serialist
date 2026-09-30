//! The scripts folder on disk: listing its `*.lua` files for the Script console and
//! the Scripts menu, and resolving the paths that key bindings, saved commands and
//! device profiles name. No GPUI here.
//!
//! Paths in `scripts::Run { path }`, a saved command's `{ "script": … }` payload and a
//! device profile's `on_connect` are relative to the scripts folder
//! ([`ConfigPaths::scripts_dir`]). An absolute path is used as it is, and a relative
//! one that is not in the scripts folder but is in the config directory (such as
//! `scripts/init.lua`, spelled from the config directory) is found there too.

use std::path::{Component, Path, PathBuf};

use serialist_core::settings::ConfigPaths;

/// How deep the listing looks under the scripts folder.
const MAX_DEPTH: usize = 8;
/// Most scripts listed; a folder with more is cut off (and says so in the log).
const MAX_SCRIPTS: usize = 1000;

/// One script in the scripts folder.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScriptEntry {
    /// The path under the scripts folder with `/` separators, as the console lists it
    /// and as `scripts::Run { path }` takes it: `lib/probe.lua`.
    pub relative: String,
    /// Where the file is.
    pub path: PathBuf,
}

/// Every `*.lua` file under `dir`, at any depth up to eight folders, sorted by their
/// relative path. Hidden files and folders (a leading `.`) and symlinked folders are
/// skipped, the last so a link cannot make the walk go round in circles. A missing
/// folder lists nothing.
pub fn list_scripts(dir: &Path) -> Vec<ScriptEntry> {
    let mut found = Vec::new();
    walk(dir, dir, 0, &mut found);
    if found.len() > MAX_SCRIPTS {
        tracing::warn!(
            dir = %dir.display(),
            count = found.len(),
            "more scripts than the console lists; showing the first {MAX_SCRIPTS}"
        );
    }
    found.sort();
    found.truncate(MAX_SCRIPTS);
    found
}

fn walk(root: &Path, dir: &Path, depth: usize, found: &mut Vec<ScriptEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if depth < MAX_DEPTH {
                walk(root, &path, depth + 1, found);
            }
            continue;
        }
        // A file, or a symlink to one (a symlinked folder is skipped).
        if kind.is_symlink() && path.is_dir() {
            continue;
        }
        let is_lua = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("lua"));
        if !is_lua {
            continue;
        }
        if let Ok(relative) = path.strip_prefix(root) {
            found.push(ScriptEntry {
                relative: slash_path(relative),
                path: path.clone(),
            });
        }
        if found.len() > MAX_SCRIPTS {
            return;
        }
    }
}

/// `relative` with `/` between its parts on every platform.
pub fn slash_path(relative: &Path) -> String {
    relative
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The file a script path names: absolute as it is; relative under the scripts
/// folder, or, when it is not there but is under the config directory, there.
pub fn resolve_script(paths: &ConfigPaths, path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let in_scripts = paths.scripts_dir().join(path);
    if in_scripts.is_file() {
        return in_scripts;
    }
    let in_config = paths.dir.join(path);
    if in_config.is_file() {
        return in_config;
    }
    in_scripts
}

/// What the console calls a script at `path`: its path under the scripts folder if it
/// is in there, else its file name.
pub fn display_name(paths: &ConfigPaths, path: &Path) -> String {
    match path.strip_prefix(paths.scripts_dir()) {
        Ok(relative) => slash_path(relative),
        Err(_) => path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn lists_lua_files_at_any_depth_sorted_and_skips_the_rest() {
        let dir = TestDir::new("script-list");
        let write = |relative: &str| {
            let path = dir.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "print(1)").unwrap();
        };
        write("version_probe.lua");
        write("lib/util.lua");
        write("a/b/c/deep.LUA");
        write("notes.txt");
        write(".hidden.lua");
        write(".git/x.lua");
        let names: Vec<String> = list_scripts(dir.path())
            .into_iter()
            .map(|entry| entry.relative)
            .collect();
        assert_eq!(
            names,
            ["a/b/c/deep.LUA", "lib/util.lua", "version_probe.lua"]
        );
        assert!(list_scripts(&dir.join("missing")).is_empty());
    }

    #[test]
    fn paths_resolve_under_the_scripts_folder_then_the_config_directory() {
        let dir = TestDir::new("script-resolve");
        let paths = ConfigPaths::new(dir.path());
        std::fs::create_dir_all(paths.scripts_dir()).unwrap();
        std::fs::write(paths.scripts_dir().join("probe.lua"), "").unwrap();
        assert_eq!(
            resolve_script(&paths, Path::new("probe.lua")),
            paths.scripts_dir().join("probe.lua")
        );
        // Spelled from the config directory, as a profile might.
        assert_eq!(
            resolve_script(&paths, Path::new("scripts/probe.lua")),
            dir.join("scripts/probe.lua")
        );
        // Missing: where it would be, so the error names the scripts folder.
        assert_eq!(
            resolve_script(&paths, Path::new("nope.lua")),
            paths.scripts_dir().join("nope.lua")
        );
        let absolute = dir.join("elsewhere.lua");
        assert_eq!(resolve_script(&paths, &absolute), absolute);
        assert_eq!(
            display_name(&paths, &paths.scripts_dir().join("lib").join("x.lua")),
            "lib/x.lua"
        );
        assert_eq!(display_name(&paths, &absolute), "elsewhere.lua");
    }
}
