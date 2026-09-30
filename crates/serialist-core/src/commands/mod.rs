//! Saved commands: collections of named, repeatable sends, kept as JSON with comments.
//!
//! A collection is one file. The user's live in `commands/*.json` in the config
//! directory (one file per collection), and a project can check one in as
//! `.serialist/commands.json`. [`CommandStore::load`] reads them all, plus a read-only
//! bundled example collection, and never fails: a file it cannot use becomes a
//! [`CommandWarning`].
//!
//! ```jsonc
//! {
//!   "name": "Airoha bring-up",
//!   "groups": [
//!     {
//!       "name": "Basics",
//!       "commands": [
//!         {
//!           "name": "Version",
//!           "description": "Ask the device for its firmware version",
//!           "payload": { "text": "AT+VER?" },       // or { "hex": "05 5A 02 00 15 0F" },
//!                                                   // or { "script": "probe.lua" } to run it
//!           "eol": "crlf",                          // none | cr | lf | crlf; else the session's
//!           "expect": { "pattern": "^OK|^ERROR", "timeout_ms": 1000 },
//!           "keybinding": "cmd-1"
//!         },
//!         {
//!           "name": "RACE query",
//!           "payload": { "hex": "05 5A 02 00 {{id}}" },
//!           "params": [
//!             { "name": "id", "label": "Command id", "default": "0x0F15", "kind": "hex16" }
//!           ]
//!         }
//!       ]
//!     }
//!   ]
//! }
//! ```
//!
//! Reading accepts comments and trailing commas. Writing does not preserve them: see
//! [`CommandCollection::to_json`]. The payload forms, escapes and placeholders are in
//! [`Command::encode`]'s module docs ([`payload`]), and the reply a command waits for
//! goes through [`MatcherHandle::expect`](crate::MatcherHandle::expect).
//!
//! The panel's flow is: [`CommandStore::filter`] to list, [`Command::prompt_params`] to
//! ask, [`Command::encode`] for the bytes, then register an expectation *before* writing
//! them so the reply cannot slip past.

mod file;
mod filter;
mod model;
pub mod payload;
mod store;

#[cfg(test)]
mod tests;

pub(crate) use file::write_atomic;
pub use file::{CommandWarning, CommandsError, LoadedCollection};
pub use filter::{FuzzyMatch, fuzzy_match};
pub use model::{
    CollectionSource, Command, CommandCollection, CommandGroup, CommandRef,
    DEFAULT_EXPECT_TIMEOUT_MS, Expect, Param, ParamKind, Payload,
};
pub use payload::{ParamValues, PayloadError};
pub use store::{CommandStore, EditError};
