//! Codec plugins for Serialist: the Lua adapter (tier 1), the WebAssembly adapter (tier 2,
//! the `wasm` feature), plugin folders on disk, the example plugins the app bundles, and
//! the Airoha RACE reference codec in Rust, Lua and WebAssembly.
//!
//! Decoders are plugins the user installs and enables: the app ships with none active,
//! and nothing in this crate registers a codec by itself. The app loads the folders in
//! its config directory's `plugins/` ([`find_plugins`]) into a [`CodecRegistry`], which
//! starts empty, and runs the codec a session picked on its ingest thread through a
//! [`CodecSink`](serialist_core::CodecSink), which fills a
//! [`FrameStore`](serialist_core::FrameStore) the UI reads. Codecs are not `Send` (a Lua
//! codec's VM stays on one thread): the factory crosses to the ingest thread and the
//! codec is made there, with
//! [`CodecSink::from_factory`](serialist_core::CodecSink::from_factory) inside
//! [`Ingest::spawn_with`](serialist_core::Ingest::spawn_with).
//!
//! | Item | What it is |
//! |---|---|
//! | [`LuaCodec`], [`LuaCodecFactory`], [`LuaLimits`] | A codec written in Lua; the contract is in [`lua`] |
//! | `WasmCodec`, `WasmCodecFactory`, `WasmLimits` | A codec compiled to a WebAssembly component (`wasm` feature); the contract is in `wasm` and `wit/v1/serialist-codec.wit` |
//! | [`find_plugins`], [`load_plugins`] | Plugin folders on disk, Lua and WebAssembly ([`dir`]) |
//! | [`EXAMPLE_PLUGINS`], [`example_plugin`] | The example plugins the app bundles and installs on request ([`examples`]) |
//! | [`AIROHA_RACE_LUA`], [`bundled_race_lua`] | The reference RACE plugin in Lua, the `airoha-race` example |
//! | [`AirohaRace`], [`TextLines`] | Rust codecs, kept as references for the conformance tests and benchmarks; never registered |
//! | [`encode_payload`], [`PayloadEncoder`] | A saved command's `{ "codec": …, "fields": … }` payload to bytes |
//! | [`corpus`] | Deterministic RACE captures for conformance tests and benchmarks |
//!
//! The Rust, Lua and WebAssembly RACE codecs must agree byte for byte: the same
//! description, the same frames for every way a capture is cut into chunks, and the same
//! bytes (or the same error) for every encode request. `tests/conformance.rs` checks all
//! three (the WebAssembly one with the `wasm` feature). The Rust one is public so the
//! tests of this crate and the app can hold the plugins to it, but no registry the app
//! builds contains it.
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
pub mod examples;
pub mod lua;
mod payload;
pub mod race;
pub mod text_lines;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use serialist_core::codec::{CodecFactory, CodecRegistry};

pub use dir::{PluginDir, PluginKind, PluginWarning, find_plugins, load_plugins};
pub use examples::{EXAMPLE_PLUGINS, ExamplePlugin, example_plugin};
pub use lua::{LuaCodec, LuaCodecFactory, LuaLimits, PLUGIN_ERROR_KIND};
pub use payload::{PayloadEncoder, PayloadError, encode_payload};
pub use race::AirohaRace;
pub use text_lines::TextLines;
#[cfg(feature = "wasm")]
pub use wasm::{WasmCodec, WasmCodecFactory, WasmEngine, WasmLimits};

/// The reference RACE plugin's source, `assets/plugins/airoha-race/plugin.lua`: the
/// `airoha-race` example plugin.
pub const AIROHA_RACE_LUA: &str = include_str!("../assets/plugins/airoha-race/plugin.lua");

/// The bundled Lua RACE plugin as a factory, registered as `airoha-race` (the name it
/// describes), without going through a plugin folder.
pub fn bundled_race_lua(limits: LuaLimits) -> Result<LuaCodecFactory, serialist_core::CodecError> {
    LuaCodecFactory::from_source("airoha-race/plugin.lua", AIROHA_RACE_LUA, limits)
}
