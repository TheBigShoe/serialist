//! Plugins on disk: one folder per plugin, found by its entry file.
//!
//! ```text
//! plugins/
//!   airoha-race/plugin.lua     tier 1, loaded by the Lua adapter
//!   my-proto/plugin.wasm       tier 2, recognised but not loaded yet
//! ```
//!
//! # The WebAssembly seam
//!
//! Tier 2 plugins are recognised here ([`PluginKind::Wasm`]) and reported as not
//! supported yet. They arrive as a second [`CodecFactory`](serialist_core::CodecFactory)
//! next to [`LuaCodecFactory`]: a wasmtime component whose WIT world mirrors the Lua
//! contract (`describe() -> codec-info`, `decode(list<u8>) -> tuple<list<frame>, u32>`
//! returning frames and the count of bytes held back, `encode(request) -> result<list<u8>,
//! codec-error>`), wrapped in the same [`Codec`](serialist_core::Codec) trait. Nothing
//! above this module changes: [`load_plugins`] dispatches on the kind, and the registry,
//! the sink and the conformance tests take any factory.

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
    /// Tier 2: `plugin.wasm`. Not loaded yet.
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
            PluginKind::Wasm => warnings.push(PluginWarning {
                path: plugin.entry,
                message: "WebAssembly plugins are not supported yet".to_owned(),
            }),
        }
    }
    warnings
}
