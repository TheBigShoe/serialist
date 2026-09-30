//! Plugins on disk: one folder per plugin, found by its entry file.
//!
//! ```text
//! plugins/
//!   airoha-race/plugin.lua     tier 1, loaded by the Lua adapter
//!   my-proto/plugin.wasm       tier 2, next to my-proto/plugin.toml, loaded by the
//!                              WebAssembly adapter (the `wasm` feature)
//! ```
//!
//! # WebAssembly
//!
//! Tier 2 plugins ([`PluginKind::Wasm`]) load through `WasmCodecFactory` (in the `wasm`
//! module), a second [`CodecFactory`](serialist_core::CodecFactory) next to
//! [`LuaCodecFactory`]. The folder's `plugin.toml` is read first, and a plugin API other
//! than `api = "1"` is refused with a warning that says so. Without the `wasm` feature
//! they are found but reported as needing it. Nothing above this module changes:
//! [`load_plugins`] dispatches on the kind, and the registry, the sink and the
//! conformance tests take any factory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serialist_core::codec::CodecRegistry;

use crate::lua::{LuaCodecFactory, LuaLimits};

/// Entry file of a Lua plugin.
pub const LUA_ENTRY: &str = "plugin.lua";
/// Entry file of a WebAssembly plugin.
pub const WASM_ENTRY: &str = "plugin.wasm";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginKind {
    /// Tier 1: `plugin.lua`.
    Lua,
    /// Tier 2: `plugin.wasm` and `plugin.toml`. Loaded with the `wasm` feature.
    Wasm,
}

/// A plugin folder and its entry file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginDir {
    pub dir: PathBuf,
    pub entry: PathBuf,
    pub kind: PluginKind,
}

/// A plugin that could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginWarning {
    pub path: PathBuf,
    pub message: String,
}

/// The plugin folders directly under `root`, sorted by name. A folder with both entry
/// files is a Lua plugin. A missing or unreadable `root` has no plugins.
pub fn find_plugins(root: &Path) -> Vec<PluginDir> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<PluginDir> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.is_dir())
        .filter_map(|dir| {
            let lua = dir.join(LUA_ENTRY);
            let wasm = dir.join(WASM_ENTRY);
            let (entry, kind) = if lua.is_file() {
                (lua, PluginKind::Lua)
            } else if wasm.is_file() {
                (wasm, PluginKind::Wasm)
            } else {
                return None;
            };
            Some(PluginDir { dir, entry, kind })
        })
        .collect();
    found.sort_by(|a, b| a.dir.cmp(&b.dir));
    found
}

/// Load every plugin under `root` into `registry`, each under the name it describes
/// (replacing a built-in of the same name). Returns what could not be loaded.
pub fn load_plugins(
    root: &Path,
    registry: &mut CodecRegistry,
    limits: LuaLimits,
) -> Vec<PluginWarning> {
    let mut warnings = Vec::new();
    for plugin in find_plugins(root) {
        match plugin.kind {
            PluginKind::Lua => match LuaCodecFactory::load_with(&plugin.entry, limits) {
                Ok(factory) => {
                    registry.register(Arc::new(factory));
                }
                Err(err) => warnings.push(PluginWarning {
                    path: plugin.entry,
                    message: err.to_string(),
                }),
            },
            #[cfg(feature = "wasm")]
            PluginKind::Wasm => match crate::wasm::load_plugin_dir(&plugin.dir) {
                Ok(factory) => {
                    registry.register(Arc::new(factory));
                }
                Err(err) => warnings.push(PluginWarning {
                    path: plugin.entry,
                    message: err.to_string(),
                }),
            },
            #[cfg(not(feature = "wasm"))]
            PluginKind::Wasm => warnings.push(PluginWarning {
                path: plugin.entry,
                message: "WebAssembly plugins need Serialist built with the `wasm` feature"
                    .to_owned(),
            }),
        }
    }
    warnings
}
