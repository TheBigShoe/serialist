//! Tier 2 plugins: WebAssembly components, adapted to the [`Codec`] trait. Behind the
//! `wasm` feature, which is off by default: wasmtime and Cranelift are most of a clean
//! build.
//!
//! # The plugin contract
//!
//! A plugin is a folder with the component as `plugin.wasm` and a [`PluginManifest`] as
//! `plugin.toml`, following Zed's extension layout:
//!
//! ```text
//! plugins/
//!   airoha-race-wasm/
//!     plugin.toml     name, version, api = "1", description
//!     plugin.wasm     a component exporting serialist:codec/plugin
//! ```
//!
//! The component exports the `serialist:codec/plugin` world of
//! `wit/v1/serialist-codec.wit`, the same three functions as the Lua tier plus `reset`:
//!
//! ```wit
//! export describe: func() -> codec-info;
//! export decode: func(chunk: list<u8>) -> decode-result;   // { frames, held }
//! export encode: func(request: encode-request) -> result<list<u8>, codec-error>;
//! export reset: func();
//! import log: func(level: log-level, message: string);
//! ```
//!
//! - `decode` gets whatever it held back last time followed by the new chunk, as the Lua
//!   tier does, so a plugin never needs to know where chunks were cut. Each frame names
//!   its bytes by offset and length in that input, and `held` says how many trailing
//!   bytes to present again next time. The adapter maps both onto stream offsets.
//! - Frame fields are typed ([`Value`](serialist_core::Value)'s seven types; lists of
//!   any depth live in the frame's `items` arena). Declared fields come out in declared
//!   order and must have their declared type (an integer may cross between `int` and
//!   `uint` if it fits); other fields follow in the plugin's order.
//! - Requests arrive as typed JSON (arrays and objects in the request's `nodes` arena):
//!   numbers as `int` when they fit an `s64`, `uint` when they are larger, `float`
//!   otherwise.
//!
//! Plugins are written against `serialist-plugin-sdk`, which wraps the bindings in a
//! `Plugin` trait with frame builders, request helpers matching
//! [`EncodeRequest`](serialist_core::EncodeRequest)'s, and hex. The reference plugin is
//! `examples/plugins/airoha-race-wasm`, checked byte for byte against the Rust RACE
//! codec by `tests/conformance.rs`.
//!
//! # Sandbox
//!
//! A plugin gets no WASI at all: no files, network, clocks, randomness, environment or
//! stdio. Its one import is `log`, which goes to the app's log (at most
//! [`WasmLimits::log_lines_per_call`] lines per call). A component that imports anything
//! else is refused at load with the names of the imports. Rust plugins are `#![no_std]`
//! on wasm32 with the SDK's `rt` feature, since std's panic path imports WASI.
//!
//! Each codec has its own [`wasmtime::Store`] and instance. [`WasmLimits`] caps linear
//! memory (64 MiB) and the wall time of one call (50 ms, by epoch interruption: see
//! [`WasmEngine`]). A call that traps (a panic, a stack overflow), runs out of memory or
//! runs out of time does not stall ingest: the bytes it was given become one
//! [`PLUGIN_ERROR_KIND`](crate::PLUGIN_ERROR_KIND) frame (severity error) whose message
//! includes the plugin's last `error` log line (the SDK's panic handler logs the panic
//! there), and the codec starts over from a fresh instance with nothing held back.
//!
//! # Compiling once
//!
//! [`WasmCodecFactory`] compiles and links the component once and keeps the result in
//! memory; each codec it creates is a new instance of that, which costs microseconds.
//! Compiled code is not cached on disk: wasmtime's precompiled artifacts are loaded with
//! an `unsafe` call that trusts the file, and the reference plugin compiles in
//! milliseconds, so a cache would buy little for the risk.
//!
//! # Threads
//!
//! The codec runs on the ingest thread, synchronously. As with every codec, the app hands
//! the ingest thread the [`WasmCodecFactory`] (`Send + Sync`) and the codec is made
//! there, by [`CodecSink::from_factory`](serialist_core::CodecSink::from_factory). A
//! [`WasmCodec`] happens to be `Send` as well, without any `unsafe`: a wasmtime store
//! holds no thread-bound state. The only other thread is the engine's epoch ticker.

mod codec;
mod convert;
mod engine;
mod manifest;
mod wit;

use serialist_core::codec::CodecError;

pub use codec::{WasmCodec, WasmCodecFactory, WasmLimits};
pub use engine::{TICK, WasmEngine};
pub use manifest::{MANIFEST_FILE, PluginManifest};

/// The plugin API version this host loads (`api` in `plugin.toml`, `wit/v1`).
pub const API_VERSION: &str = "1";

/// Load the plugin folder `dir` with the shared engine and default limits: what
/// [`load_plugins`](crate::load_plugins) does for a folder with a `plugin.wasm`.
pub fn load_plugin_dir(dir: &std::path::Path) -> Result<WasmCodecFactory, CodecError> {
    WasmCodecFactory::load_dir(dir)
}
