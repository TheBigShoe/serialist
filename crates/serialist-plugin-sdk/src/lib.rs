//! Write Serialist codec plugins as WebAssembly components.
//!
//! A plugin implements [`Plugin`] and hands its type to [`export_plugin!`]; built for
//! `wasm32-wasip2`, the crate is then a component exporting the `serialist:codec/plugin`
//! world (API version 1, `wit/v1/serialist-codec.wit` in `serialist-plugins`). Put the
//! component in a folder as `plugin.wasm` next to a `plugin.toml`:
//!
//! ```toml
//! name = "my-proto"       # the same name describe() returns
//! version = "1.0.0"       # the same version describe() returns
//! api = "1"               # the WIT this SDK speaks
//! description = "My protocol"
//! ```
//!
//! # The contract
//!
//! - [`Plugin::decode`] gets whatever it held back last time followed by the new bytes,
//!   pushes the [`Frame`]s it completes (offsets are into that input) and returns how
//!   many trailing bytes to hold back. A plugin can so be stateless, like the Lua tier's,
//!   or keep state in `self`; the host never splits the input into frames for it.
//! - [`Plugin::encode`] turns a [`Request`] (a command and its JSON fields) into bytes.
//!   [`Request::uint`], [`Request::bytes`] and [`Request::check_fields`] follow the
//!   conventions of the built-in codecs: integers as numbers or hex strings, bytes as
//!   hex text or a list.
//! - A panic, an endless loop or running out of memory traps. The host turns the input
//!   of that call into a `plugin_error` frame and starts the plugin over with
//!   [`Plugin::new`].
//!
//! # `no_std`
//!
//! The crate is `no_std` (it needs `alloc`). The host provides no WASI, so a plugin must
//! not import any, and std's panic path writes to stderr through WASI. So a plugin is
//! `#![cfg_attr(target_arch = "wasm32", no_std)]` and turns on this crate's `rt` feature
//! for wasm32, which supplies the allocator, `cabi_realloc` and a panic handler that
//! logs the panic through [`log`] and traps; natively it stays an ordinary std crate
//! whose logic unit tests can call directly:
//!
//! ```toml
//! [lib]
//! crate-type = ["cdylib"]
//!
//! [dependencies]
//! serialist-plugin-sdk = "0.1"
//!
//! [target.'cfg(target_arch = "wasm32")'.dependencies]
//! serialist-plugin-sdk = { version = "0.1", features = ["rt"] }
//! ```
//!
//! A whole plugin, a line framer (`cargo build --target wasm32-wasip2 --release`):
//!
//! ```
//! extern crate alloc;
//!
//! use alloc::vec::Vec;
//! use serialist_plugin_sdk::{
//!     CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Plugin,
//!     Request,
//! };
//!
//! struct Lines;
//!
//! impl Plugin for Lines {
//!     fn new() -> Self {
//!         Lines
//!     }
//!
//!     fn describe(&self) -> CodecInfo {
//!         CodecInfo::new("lines", "1.0.0", "One frame per line")
//!             .with_kind(
//!                 FrameKindInfo::new("line", "A line")
//!                     .with_field(FieldInfo::new("text", FieldType::Str, "The line")),
//!             )
//!             .with_command(
//!                 CommandInfo::new("say", "Send a line")
//!                     .with_field(FieldInfo::new("text", FieldType::Str, "The line")),
//!             )
//!     }
//!
//!     fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> usize {
//!         let mut start = 0;
//!         while let Some(i) = input[start..].iter().position(|&b| b == b'\n') {
//!             let text = String::from_utf8_lossy(&input[start..start + i]);
//!             frames.push(Frame::new("line", start, i + 1).with_field("text", &*text));
//!             start += i + 1;
//!         }
//!         input.len() - start // hold back the unfinished line
//!     }
//!
//!     fn encode(&mut self, request: &Request<'_>) -> Result<Vec<u8>, CodecError> {
//!         match request.command() {
//!             "say" => {
//!                 request.check_fields(&["text"])?;
//!                 let text = request
//!                     .str("text")?
//!                     .ok_or_else(|| CodecError::missing_field("text"))?;
//!                 Ok([text.as_bytes(), b"\n"].concat())
//!             }
//!             other => Err(CodecError::unknown_command(other)),
//!         }
//!     }
//! }
//!
//! serialist_plugin_sdk::export_plugin!(Lines);
//!
//! // Natively the logic is plain Rust.
//! let mut frames = Vec::new();
//! assert_eq!(Lines.decode(b"one\ntw", &mut frames), 2);
//! assert_eq!(frames[0].range(), 0..4);
//! ```

#![no_std]

extern crate alloc;
// The generated bindings and the export macro name this crate by its path, so they work
// the same inside it and in a plugin.
extern crate self as serialist_plugin_sdk;

mod frame;
pub mod hex;
mod info;
pub mod log;
mod request;
#[cfg(all(feature = "rt", target_arch = "wasm32"))]
mod rt;

/// The generated bindings of the v1 world. The SDK's types cover everything a plugin
/// needs; these are here for the rare plugin that wants the raw WIT shapes.
pub mod bindings {
    wit_bindgen::generate!({
        path: "../serialist-plugins/wit/v1",
        world: "serialist:codec/plugin",
        pub_export_macro: true,
        export_macro_name: "__export_world_plugin",
        default_bindings_module: "::serialist_plugin_sdk::bindings",
        runtime_path: "::serialist_plugin_sdk::__private::wit_bindgen::rt",
        additional_derives: [PartialEq],
    });
}

use alloc::vec::Vec;

pub use bindings::{
    BadField, CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, FrameKindInfo, LogLevel,
    Severity,
};
pub use frame::{Frame, Value};
pub use request::{Array, Json, Object, Request};

/// The version of the plugin API (`api` in `plugin.toml`) this SDK implements.
pub const API_VERSION: &str = "1";

/// A codec plugin. One instance lives in the component for as long as the host keeps
/// it; after a trap the host makes a new component instance and so a new plugin.
pub trait Plugin: Sized + 'static {
    /// A plugin with nothing held back and nothing seen.
    fn new() -> Self;

    /// Name, version, the frame kinds it produces and the commands it encodes. Name and
    /// version must match `plugin.toml`.
    fn describe(&self) -> CodecInfo;

    /// Decode `input`: the bytes held back by the last call followed by the bytes
    /// received since. Push the frames it completes, in stream order, with offsets into
    /// `input`, and return how many trailing bytes of `input` to hold back for the next
    /// call (at most `input.len()`).
    fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> usize;

    /// The bytes for one command, or why it cannot be encoded.
    fn encode(&mut self, request: &Request<'_>) -> Result<Vec<u8>, CodecError>;

    /// The stream starts over: forget all decode state. By default, a fresh [`new`](Self::new).
    fn reset(&mut self) {
        *self = Self::new();
    }
}

/// Export a [`Plugin`] as the component's `serialist:codec/plugin` world.
///
/// Expands to nothing on targets other than wasm32, so a plugin crate also builds (and
/// runs its unit tests) natively.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        #[cfg(target_arch = "wasm32")]
        const _: () = {
            static PLUGIN: $crate::__private::Slot<$plugin> = $crate::__private::Slot::new();

            struct Exports;

            impl $crate::bindings::Guest for Exports {
                fn describe() -> $crate::CodecInfo {
                    PLUGIN.with(|plugin| $crate::Plugin::describe(plugin))
                }

                fn decode(chunk: $crate::__private::Vec<u8>) -> $crate::bindings::DecodeResult {
                    PLUGIN.with(|plugin| $crate::__private::decode(plugin, &chunk))
                }

                fn encode(
                    request: $crate::bindings::EncodeRequest,
                ) -> ::core::result::Result<$crate::__private::Vec<u8>, $crate::CodecError>
                {
                    PLUGIN.with(|plugin| {
                        $crate::Plugin::encode(plugin, &$crate::Request::new(&request))
                    })
                }

                fn reset() {
                    PLUGIN.with(|plugin| $crate::Plugin::reset(plugin))
                }
            }

            $crate::bindings::__export_world_plugin!(Exports with_types_in $crate::bindings);
};
    };
}

/// Used by [`export_plugin!`]; not part of the API.
#[doc(hidden)]
pub mod __private {
    use core::cell::RefCell;

    pub use alloc::vec::Vec;
    pub use wit_bindgen;

    use crate::{Frame, Plugin, bindings};

    /// The plugin instance of a component.
    pub struct Slot<P>(RefCell<Option<P>>);

    // SAFETY: wasm32 without the atomics feature has no threads, so the cell is never
    // touched from two threads. (The v1 world has no re-entrant calls either: its one
    // import, `log`, never calls back into the plugin.) Elsewhere `Slot` is not `Sync`,
    // and `export_plugin!` only makes its static on wasm32.
    #[cfg(all(target_arch = "wasm32", not(target_feature = "atomics")))]
    unsafe impl<P> Sync for Slot<P> {}

    impl<P: Plugin> Slot<P> {
        pub const fn new() -> Self {
            Self(RefCell::new(None))
        }

        pub fn with<R>(&self, f: impl FnOnce(&mut P) -> R) -> R {
            let mut slot = self.0.borrow_mut();
            f(slot.get_or_insert_with(P::new))
        }
    }

    impl<P: Plugin> Default for Slot<P> {
        fn default() -> Self {
            Self::new()
        }
    }

    pub fn decode<P: Plugin>(plugin: &mut P, chunk: &[u8]) -> bindings::DecodeResult {
        let mut frames = Vec::new();
        let held = plugin.decode(chunk, &mut frames);
        bindings::DecodeResult {
            frames: frames.into_iter().map(Frame::into_wit).collect(),
            held: crate::frame::to_u32(held),
        }
    }
}
