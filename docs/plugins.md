# Plugins

A Serialist plugin is a **codec**: it turns the bytes a device sends into structured
frames (a kind, named fields, a severity, a one-line summary, and the bytes the frame came
from), and it turns a structured command into the bytes to send. Airoha RACE is the
reference plugin and ships in three forms.

There are three ways to write a codec, all behind one Rust trait, `serialist_core::Codec`:

| Tier | You write | It lives in | Needs |
|---|---|---|---|
| Built-in Rust | Rust, inside `serialist-plugins` | compiled in: `text-lines` and `airoha-race` | nothing |
| 1. Lua | `plugin.lua` returning `describe`, `decode` and `encode` | `plugins/<name>/plugin.lua` | nothing |
| 2. WebAssembly | a component exporting the `serialist:codec/plugin` world, with a `plugin.toml` | `plugins/<name>/plugin.wasm` and `plugin.toml` | the `wasm` feature |

`plugins/` is a folder in the config directory (`~/.config/serialist` on macOS and Linux,
`%APPDATA%\Serialist` on Windows). Each plugin is a folder directly under it. A folder with
a `plugin.lua` is a Lua plugin (even if it also has a `plugin.wasm`); otherwise a folder
with a `plugin.wasm` is a WebAssembly plugin. A plugin registers under the name its
`describe` returns, and replaces a built-in of the same name, so a plugin that describes
itself as `airoha-race` replaces the built-in one.

> **Status.** The plugin machinery is in the tree and tested: the `Codec` trait, the frame
> store and the ingest-thread sink in `serialist-core`; the Rust, Lua and WebAssembly
> adapters, plugin folder discovery and codec-payload encoding in `serialist-plugins`; the
> guest crate `serialist-plugin-sdk`. The app's own use of it, the Decoded panel, loading
> the `plugins/` folder, a device profile's `plugin`, and codec payloads in saved commands,
> is being finished separately. Sections headed "as designed" describe that from the plan
> (`docs/plan.md`), not from code you can run yet. Where this page says what a crate does,
> it is describing the code as of commit `68ede78`.

## What a codec does

```rust
pub trait Codec {
    fn describe(&self) -> CodecInfo;
    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>);
    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError>;
    fn reset(&mut self);
}
```

- **`describe`** says what the codec is and speaks: a name (what saved commands and
  settings use, such as `airoha-race`), a version, a description, the frame **kinds**
  `decode` produces with their fields in display order, and the **commands** `encode`
  accepts with their fields. The first command is the default for a saved command that does
  not name one.
- **`decode`** is a stateful framer, called once per received chunk and never per byte, on
  the ingest thread. Bytes that do not yet make a whole frame are held back and finished by
  a later chunk. A frame's `raw` range is in stream offsets, the same offsets the
  scrollback uses, so the UI can re-read a frame's bytes without the codec copying them.
- **`encode`** takes a command name and JSON fields, the shape of a saved command's codec
  payload, and returns bytes or a `CodecError` (unknown command, missing field, bad field,
  or anything else).
- **`reset`** forgets everything held back: the stream starts over.

A codec is not `Send` (a Lua codec keeps its VM on one thread). The app hands the ingest
thread a `CodecFactory`, which is `Send + Sync`, and the codec is made there.

A frame's field values have seven types: `bool`, `int`, `uint`, `float`, `str`, `bytes`
and `list`. A frame's severity is `info`, `warning` or `error`.

## Writing a Lua plugin (tier 1)

Make `plugins/my-proto/plugin.lua`. It returns a table of three functions:

```lua
local M = {}

function M.describe()
  return {
    name = "my-proto", version = "1.0.0", description = "…",
    kinds = {      -- the frames decode produces, fields in display order
      { kind = "packet", description = "…", fields = {
          { name = "id", type = "uint", description = "…" },
          { name = "body", type = "bytes", description = "…", optional = true },
      } },
    },
    commands = {   -- what encode accepts; the first is the default
      { name = "send", description = "…", fields = {
          { name = "id", type = "uint", description = "…" },
      } },
    },
  }
end

-- bytes: whatever was held back last time followed by the new chunk, as a string.
-- state: a table kept between calls (fresh after a reset or an error).
-- Returns the frames it found and the bytes to hold back (a suffix of `bytes`).
function M.decode(bytes, state)
  local frames = {}
  frames[#frames + 1] = {
    kind = "packet",
    pos = 1, len = 4,          -- where in `bytes` (1-based, like string.sub)
    severity = "info",         -- optional: info, warning or error
    summary = "packet 7",      -- optional one-line description
    fields = { id = 7, body = "\1\2" },
  }
  return frames, bytes:sub(5)
end

-- request: { command = "send", fields = { id = 7 } }, the fields as JSON gave them.
-- Returns the bytes, or nil and an error.
function M.encode(request)
  if request.command ~= "send" then return nil, codec.unknown_command(request.command) end
  local id = request.fields.id
  if id == nil then return nil, codec.missing_field("id") end
  return string.pack("<BI2", 0x7E, id)
end

return M
```

**The frame table.** `decode` returns a list of frame tables and the bytes to hold back:

| Key | Required | Meaning |
|---|---|---|
| `kind` | yes | A string, normally one of the kinds `describe` lists. |
| `pos`, `len` | yes | Where the frame sits in `bytes`: `pos` is 1-based, like `string.sub`, and `len` is a byte count. They must lie inside `bytes`. |
| `severity` | no | `"info"` (the default), `"warning"` or `"error"`. |
| `summary` | no | A one-line description for a list or the terminal. |
| `fields` | no | A table of named values. Required if the kind declares a field that is not `optional`. |

- **Fields.** Declared fields come out in declared order and are converted to their
  declared types: a Lua string is `bytes` or `str` as declared, an integer is `int` or
  `uint`, a sequence is a `list`. Fields the kind does not declare follow, sorted by name,
  with their types inferred (string `str`, integer `int`, float `float`, boolean `bool`,
  sequence `list`). Lists nest at most 32 deep.
- **Held-back bytes.** The second result of `decode` is a suffix of `bytes` (or `nil`) that
  the host prepends to the next chunk, so your plugin sees one contiguous stream and never
  needs to know where chunks were cut. It must be a suffix of what you were given and at
  most 1 MiB.
- **`state`** is a table kept between calls. It is fresh after a `reset` and after an error.
- **Requests.** `encode` gets `{ command = "send", fields = { ... } }` with the fields as
  JSON gave them: whole numbers are Lua integers, `null` is `codec.null`, arrays are
  sequences for which `codec.is_array(t)` is true, objects are tables. Return the bytes (a
  string or a table of byte values), or `nil` and an error: `codec.unknown_command(name)`,
  `codec.missing_field(name)` or `codec.bad_field(name, reason)`. Any other error value, or
  a raised error, is an internal error. Requests nest at most 32 deep.
- **Sandbox.** The same as scripts (see [`scripting.md`](scripting.md)): `string`, `table`,
  `math`, `utf8`, `coroutine`, and `os.time`, `os.clock`, `os.date` only; no `io`, `debug`,
  `package`, `require`, `dofile`, `loadfile` or `string.dump`; `load` takes text only. You
  get `string.pack` and `string.unpack` for binary headers, `hex.encode(bytes, sep)` and
  `hex.decode(text)`, `bytes.from_table` and `bytes.to_table`, `codec.*`, and `print` and
  `log.debug/info/warn/error`, which go to the app's log. There are no serial or UI calls:
  a plugin only frames and encodes.
- **Limits.** 64 MiB of memory, 20 000 000 Lua instructions per call (`describe`, one
  `decode`, one `encode`, or loading the file), checked every 10 000. A call that raises,
  runs out of memory or spends its budget does not stall ingest: the bytes it was given
  become one `plugin_error` frame (severity `error`, its `error` field holds the message)
  and the plugin starts over with a fresh `state` and nothing held back. A frame whose
  `fields` are wrong becomes a `plugin_error` frame for just its own bytes; a frame outside
  `bytes`, or a result that is not a list of tables, fails the whole call.
- **Refused at load.** A `plugin.lua` that does not return a table with `describe`,
  `decode` and `encode` functions, or whose `describe` result is not valid.
- **Reloading.** `LuaCodec::reload` loads the file again into a fresh VM and starts decoding
  over; if the new file fails to load, the old one keeps running. Reloading a plugin when
  its file is saved, as the plan describes, belongs to the app's plugin wiring (see the
  status note above).

The reference is `crates/serialist-plugins/assets/plugins/airoha-race/plugin.lua`, a
resynchronising framer for RACE with an encoder for two commands.

## Writing a WebAssembly plugin (tier 2)

A WebAssembly plugin is a folder with a compiled component, `plugin.wasm`, and a manifest,
`plugin.toml`, in the layout of Zed's extensions:

```text
plugins/
  my-proto/
    plugin.toml     name, version, api = "1", description
    plugin.wasm     a component exporting serialist:codec/plugin
```

**The manifest.**

```toml
name = "my-proto"       # the same name describe() returns
version = "1.0.0"       # the same version describe() returns
api = "1"               # the WIT this plugin was built against (wit/v1)
description = "My protocol"
```

`name` must not be empty, `api` must be `"1"`, and `name` and `version` must match what the
plugin's `describe` returns. Other keys are ignored, so a later manifest can add some
without breaking this host. The `description` key is optional.

**The contract** is the WIT world `serialist:codec/plugin` in
`crates/serialist-plugins/wit/v1/serialist-codec.wit`. It exports the same three functions
as the Lua tier plus `reset`, and imports one function, `log`:

```wit
export describe: func() -> codec-info;
export decode: func(chunk: list<u8>) -> decode-result;   // { frames, held }
export encode: func(request: encode-request) -> result<list<u8>, codec-error>;
export reset: func();
import log: func(level: log-level, message: string);
```

`decode` gets whatever it held back last time followed by the new chunk, as the Lua tier
does, so a plugin never needs to know where chunks were cut. Each frame names its bytes by
offset and length in that input, and `held` says how many trailing bytes to present again
next time. Frame fields are typed (`bool`, `int`, `uint`, `float`, `str`, `bytes`, `list`);
declared fields come out in declared order and must have their declared type (an integer
may cross between `int` and `uint` if it fits), and other fields follow in the plugin's
order. Requests arrive as typed JSON: numbers as `int` when they fit an `s64`, `uint` when
they are larger, `float` otherwise.

Any language that can build a component for that world will do. In Rust, use the SDK.

**The Rust SDK.** `serialist-plugin-sdk` wraps the generated bindings in a `Plugin` trait
(`new`, `describe`, `decode`, `encode`, and an optional `reset`), frame builders, request
helpers (`Request::uint`, `bytes`, `str` and `check_fields`, which follow the conventions
of the built-in codecs: integers as numbers or hex strings, bytes as hex text or a list),
`hex`, and `log`. `export_plugin!` exports your type as the component's world; it expands
to nothing on other targets, so the crate also builds and unit-tests natively.

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
serialist-plugin-sdk = "0.1"

[target.'cfg(target_arch = "wasm32")'.dependencies]
serialist-plugin-sdk = { version = "0.1", features = ["rt"] }
```

Nothing in this repository says the SDK is published to crates.io. From a checkout, point
the dependency at `crates/serialist-plugin-sdk` with `path = "..."` instead.

The host gives a plugin no WASI, and std's panic path writes to stderr through WASI, so a
Rust plugin is `#![no_std]` on wasm32 and turns on the SDK's `rt` feature, which supplies
the allocator, `cabi_realloc` and a panic handler that logs the panic through `log` and
traps. A whole plugin, a line framer:

```rust
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use serialist_plugin_sdk::{
    CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Plugin,
    Request,
};

struct Lines;

impl Plugin for Lines {
    fn new() -> Self {
        Lines
    }

    fn describe(&self) -> CodecInfo {
        CodecInfo::new("lines", "1.0.0", "One frame per line")
            .with_kind(
                FrameKindInfo::new("line", "A line")
                    .with_field(FieldInfo::new("text", FieldType::Str, "The line")),
            )
            .with_command(
                CommandInfo::new("say", "Send a line")
                    .with_field(FieldInfo::new("text", FieldType::Str, "The line")),
            )
    }

    fn decode(&mut self, input: &[u8], frames: &mut Vec<Frame>) -> usize {
        let mut start = 0;
        while let Some(i) = input[start..].iter().position(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(&input[start..start + i]);
            frames.push(Frame::new("line", start, i + 1).with_field("text", &*text));
            start += i + 1;
        }
        input.len() - start // hold back the unfinished line
    }

    fn encode(&mut self, request: &Request<'_>) -> Result<Vec<u8>, CodecError> {
        match request.command() {
            "say" => {
                request.check_fields(&["text"])?;
                let text = request
                    .str("text")?
                    .ok_or_else(|| CodecError::missing_field("text"))?;
                Ok([text.as_bytes(), b"\n"].concat())
            }
            other => Err(CodecError::unknown_command(other)),
        }
    }
}

serialist_plugin_sdk::export_plugin!(Lines);
```

Its `plugin.toml` is the manifest above with `name = "lines"` and `version = "1.0.0"`.

**Build** it as a component with a plain cargo build, no cargo-component:

```sh
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 --release
```

The component is `target/wasm32-wasip2/release/<crate_name>.wasm` (dashes in the crate name
become underscores). Copy it into the plugin folder as `plugin.wasm`, next to `plugin.toml`.
The reference plugin, `examples/plugins/airoha-race-wasm`, builds with the workspace's
`wasm-plugin` profile (smaller, no unwinding, no symbols, reproducible):

```sh
cargo build -p airoha-race-wasm --target wasm32-wasip2 --profile wasm-plugin
```

The output is `target/wasm32-wasip2/wasm-plugin/airoha_race_wasm.wasm`, a component that
imports nothing but `log`.

**Limits.** Each codec has its own wasmtime store and instance.

| Limit | Default |
|---|---|
| Linear memory | 64 MiB (growing past it traps) |
| Wall time of one call (instantiation, `describe`, one `decode`, `encode` or `reset`) | 50 ms, enforced to within 10 ms by epoch interruption |
| Wasm stack | 512 KiB |
| Bytes `decode` may hold back | 1 MiB |
| Log lines per call | 64 (later ones are counted and dropped); a line is cut at 1024 bytes |

A call that traps (a panic, a stack overflow), runs out of memory or runs out of time does
not stall ingest: the bytes it was given become one `plugin_error` frame (severity
`error`) whose message includes the plugin's last `error` log line, and the codec starts
over from a fresh instance with nothing held back.

**What is refused.** A plugin gets no WASI at all: no files, no network, no clocks, no
randomness, no environment, no stdio. Its one import is `log`. At load, the host refuses:

- a folder with no readable `plugin.toml`, or one with an empty `name`, a missing `name`,
  `version` or `api`, or an `api` other than `"1"`;
- a `plugin.wasm` that is not a component;
- a component that imports anything but `log` (the error names the imports; this is what
  a std Rust plugin gets, since std imports WASI);
- a component that does not export the `serialist:codec/plugin` world;
- a plugin whose `describe` fails, or whose name or version differs from `plugin.toml`'s.

Without the `wasm` feature, a `plugin.wasm` folder is found but reported as needing it.

**The `wasm` feature and its build cost.** The WebAssembly tier is the `wasm` feature of
`serialist-plugins`, off by default. It pulls in wasmtime and Cranelift, which are most of a
clean build and add minutes to it; the Lua tier needs nothing extra. CI builds and tests it
in its own job, and `just test-wasm` does the same locally
(`cargo test -p serialist-plugins --features wasm`). The tests run the committed plugin
builds under `crates/serialist-plugins/tests/fixtures`, so they need no wasm toolchain;
`just wasm-fixtures` rebuilds them (it needs the `wasm32-wasip2` target). The component is
compiled once per plugin and kept in memory; each codec is a new instance of it, which
costs microseconds. Compiled code is not cached on disk. Loading, linking and describing
the reference plugin takes about 60 ms in a release build.

Nothing in this tree forwards the feature from the app binary, so how an app build turns
the WebAssembly tier on is not settled here.

## The Airoha RACE reference plugin

RACE frames are `[0x05][type: u8][len: u16 LE][cmd_id: u16 LE][payload]`, with
`len = payload length + 2`:

| Field | Size | Value |
|---|---|---|
| Sync | 1 byte | `0x05` |
| Type | 1 byte | `0x5A` command, `0x5B` response, `0x5C` indication, `0x5D` log |
| Length | u16 little-endian | payload length plus 2, 2 to 4096 |
| Command id | u16 little-endian | for example `0x0F15`, query version and build time |
| Payload | length minus 2 bytes | command-specific |

The codec is a resynchronising framer: every received byte ends up in exactly one frame, so
nothing is hidden. Its frame kinds are `command`, `response`, `indication` and `log` (fields
`type`, `cmd_id`, `cmd_id_hex`, `payload`, `payload_len`), `malformed` (a sync byte and a
known type followed by an impossible length; severity `warning`), and `text` (the bytes
between frames, a line or 1024 bytes at a time). Its commands are `race` (any RACE frame:
`type`, `cmd_id`, `payload`) and `race_version` (command `0x0F15`, no payload). The Rust
codec (`crates/serialist-plugins/src/race.rs`), the Lua plugin and the WebAssembly plugin
must agree byte for byte: the same description, the same frames for every way a capture is
cut into chunks, and the same bytes or error for every encode request.
`crates/serialist-plugins/tests/conformance.rs` checks all three (the WebAssembly one with
the `wasm` feature). The simulated device `serialist --virtual race` sends RACE frames mixed
with text.

The built-in `text-lines` codec is the simplest: one `line` frame per received line, and one
command, `line`, which sends `text` and a line ending (`eol`: `crlf` by default, `lf`, `cr`
or `none`).

## Using plugins in the app (as designed)

This part is the plan's design; the UI for it is being finished, so treat the details as
subject to change.

- **Activating a codec.** A device profile in `settings.json` names one, so a board gets its
  baud, plugin and line ending the moment it is plugged in:
  `{ "match": { "vid": "0x0e8d", "product": "Airoha" }, "baud": 921600, "plugin": "airoha-race", "eol": "crlf" }`.
  The status line shows the active plugin. The plan does not say how to pick a plugin by
  hand.
- **The Decoded panel** is a table in the right dock (above the Script console) of the
  active plugin's frames: timestamp, direction, frame type, decoded fields and raw hex, with
  a filter. A frame carries a timestamp, a direction, a kind, named fields, a severity and
  a byte range into the scrollback rather than a copy of the bytes.
- **In the terminal**, a frame's one-line summary can be shown in a plugin color, and framed
  bytes can be hidden from the text view. These are the display settings
  `display.decoded_inline` and `display.hide_framed_bytes`; they are not in this tree's
  `default_settings.jsonc` yet.
- **Export.** Decoded frames can be exported as CSV or JSON once a plugin is active, next to
  the raw and text exports.
- **Codec payloads in saved commands.** A saved command can name a codec instead of giving
  text or hex. The file form, which the command loader already parses, is
  `{ "codec": "airoha-race", "fields": { "cmd_id": "0x0F15" } }`. A `command` key inside
  `fields` picks one of the codec's commands (`{ "command": "race_version" }`); without it
  the codec's first command is used. The plugin encodes the fields to bytes, and a codec
  payload sends no line ending unless the command sets `eol`. Each encode uses a fresh codec
  instance, so it never disturbs a session's decoding state. `serialist_plugins::encode_payload`
  does this against a codec registry, and returns an error for an unknown codec, a codec
  with no commands, or whatever the codec refuses. In this tree `Command::encode` in
  `serialist-core` still returns `PayloadError::CodecUnavailable` for a codec payload; the
  encoding lives in `serialist-plugins`, and connecting the two is the app's wiring.
- **Frame predicates.** The plan lets a saved command's expected response be a frame
  predicate (same command id, type response) instead of a regex. In this tree an `expect`
  is a `pattern` and a `timeout_ms` only.
