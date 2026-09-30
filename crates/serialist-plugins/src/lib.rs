//! Codec plugins for Serialist: built-in Rust codecs, the Lua adapter (tier 1), and the
//! Airoha RACE reference plugin in both.
//!
//! Every codec implements [`serialist_core::Codec`]; the app keeps them in a
//! [`CodecRegistry`] and runs the active one on the ingest thread through a
//! [`CodecSink`](serialist_core::CodecSink), which fills a
//! [`FrameStore`](serialist_core::FrameStore) the UI reads.
//!
//! | Item | What it is |
//! |---|---|
//! | [`builtin_registry`], [`register_builtins`] | The Rust codecs: [`TextLines`] (`text-lines`) and [`AirohaRace`] (`airoha-race`) |
//! | [`LuaCodec`], [`LuaCodecFactory`], [`LuaLimits`] | A codec written in Lua; the contract is in [`lua`] |
//! | [`AIROHA_RACE_LUA`], [`bundled_race_lua`] | The reference RACE plugin in Lua, bundled |
//! | [`find_plugins`], [`load_plugins`] | Plugin folders on disk, and the WebAssembly seam ([`dir`]) |
//! | [`encode_payload`], [`PayloadEncoder`] | A saved command's `{ "codec": …, "fields": … }` payload to bytes |
//! | [`corpus`] | Deterministic RACE captures for conformance tests and benchmarks |
//!
//! The Rust and Lua RACE codecs must agree byte for byte: the same description, the same
//! frames for every way a capture is cut into chunks, and the same bytes (or the same
//! error) for every encode request. `tests/conformance.rs` checks all three.
//!
//! ```
//! use std::time::Instant;
//! use serialist_core::{Codec, EncodeRequest};
//! use serialist_plugins::AirohaRace;
//!
//! let mut race = AirohaRace::new();
//! let bytes = race.encode(&EncodeRequest::new("race_version")).unwrap();
//! assert_eq!(bytes, [0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F]);
//!
//! let mut frames = Vec::new();
//! race.decode(&bytes, Instant::now(), 0, &mut frames);
//! assert_eq!(frames[0].kind, "command");
//! assert_eq!(frames[0].raw, 0..6);
//! ```

pub mod corpus;
pub mod dir;
pub mod lua;
mod payload;
pub mod race;
pub mod text_lines;

pub use serialist_core::codec::{CodecFactory, CodecRegistry};

pub use dir::{PluginDir, PluginKind, PluginWarning, find_plugins, load_plugins};
pub use lua::{LuaCodec, LuaCodecFactory, LuaLimits, PLUGIN_ERROR_KIND};
pub use payload::{PayloadEncoder, PayloadError, encode_payload};
pub use race::AirohaRace;
pub use text_lines::TextLines;

/// The reference RACE plugin's source, `assets/plugins/airoha-race/plugin.lua`.
pub const AIROHA_RACE_LUA: &str = include_str!("../assets/plugins/airoha-race/plugin.lua");

/// The bundled Lua RACE plugin as a factory. It registers as `airoha-race`, the same
/// name as the Rust codec, so registering it after [`register_builtins`] replaces that.
pub fn bundled_race_lua(limits: LuaLimits) -> Result<LuaCodecFactory, serialist_core::CodecError> {
    LuaCodecFactory::from_source("airoha-race/plugin.lua", AIROHA_RACE_LUA, limits)
}

/// Add the built-in Rust codecs to `registry`.
pub fn register_builtins(registry: &mut CodecRegistry) {
    registry.register_fn(TextLines::info(), || Ok(Box::new(TextLines::new())));
    registry.register_fn(race::race_info(), || Ok(Box::new(AirohaRace::new())));
}

/// A registry holding the built-in Rust codecs.
pub fn builtin_registry() -> CodecRegistry {
    let mut registry = CodecRegistry::new();
    register_builtins(&mut registry);
    registry
}
