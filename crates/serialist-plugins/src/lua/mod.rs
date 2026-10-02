//! Tier 1 plugins: a codec written in Lua, adapted to the [`Codec`] trait.
//!
//! # The plugin contract
//!
//! A plugin is a folder with a `plugin.lua` that returns a table of three functions:
//!
//! ```lua
//! local M = {}
//!
//! function M.describe()
//!   return {
//!     name = "my-proto", version = "1.0.0", description = "…",
//!     kinds = {      -- the frames decode produces, fields in display order
//!       { kind = "packet", description = "…", fields = {
//!           { name = "id", type = "uint", description = "…" },
//!           { name = "body", type = "bytes", description = "…", optional = true },
//!       } },
//!     },
//!     commands = {   -- what encode accepts; the first is the default
//!       { name = "send", description = "…", fields = {
//!           { name = "id", type = "uint", description = "…" },
//!       } },
//!     },
//!   }
//! end
//!
//! -- bytes: whatever was held back last time followed by the new chunk, as a string.
//! -- state: a table kept between calls (fresh after a reset or an error).
//! -- Returns the frames it found and the bytes to hold back (a suffix of `bytes`).
//! function M.decode(bytes, state)
//!   local frames = {}
//!   frames[#frames + 1] = {
//!     kind = "packet",
//!     pos = 1, len = 4,          -- where in `bytes` (1-based, like string.sub)
//!     severity = "info",         -- optional: info, warning or error
//!     summary = "packet 7",      -- optional one-line description
//!     fields = { id = 7, body = "\1\2" },
//!   }
//!   return frames, bytes:sub(5)
//! end
//!
//! -- request: { command = "send", fields = { id = 7 } }, the fields as JSON gave them.
//! -- Returns the bytes, or nil and an error.
//! function M.encode(request)
//!   if request.command ~= "send" then return nil, codec.unknown_command(request.command) end
//!   local id = request.fields.id
//!   if id == nil then return nil, codec.missing_field("id") end
//!   return string.pack("<BI2", 0x7E, id)
//! end
//!
//! return M
//! ```
//!
//! **Kinds.** A frame's `kind` must be one `describe` lists; a frame of another kind is
//! a [`PLUGIN_ERROR_KIND`] frame over its bytes.
//!
//! **Field types** are `bool`, `int`, `uint`, `float`, `str`, `bytes` and `list`. A frame's
//! declared fields come out in declared order and are converted to their declared types
//! (a Lua string is `bytes` or `str` as declared; an integer is `int` or `uint`); a
//! declared field that is not optional must be present. Fields a kind does not declare
//! follow, sorted by name, with their types inferred (string `str`, integer `int`,
//! float `float`, boolean `bool`, sequence `list`).
//!
//! **Offsets.** The adapter prepends what `decode` held back to the next chunk, so a
//! plugin sees one contiguous stream and never needs to know where chunks were cut; it
//! maps `pos` and `len` onto global stream offsets itself.
//!
//! **Requests** arrive as Lua values made from JSON: numbers as integers when they are
//! whole, `null` as `codec.null`, arrays as sequences for which `codec.is_array(t)` is
//! true, objects as tables. `encode` reports errors with `codec.unknown_command(name)`,
//! `codec.missing_field(name)` or `codec.bad_field(name, reason)`; any other error
//! value (or a raised error) is [`CodecError::Internal`].
//!
//! # Sandbox
//!
//! The same as scripts: `string`, `table`, `math`, `utf8`, `coroutine`, and `os.time`,
//! `os.clock`, `os.date` only. No `io`, `debug`, `package`, `require`, `dofile`,
//! `loadfile` or `string.dump`; `load` takes text only. `string.pack` and
//! `string.unpack` read and write binary headers, `hex.encode(bytes, sep)` and
//! `hex.decode(text)` convert hex text, `bytes.from_table` and `bytes.to_table`
//! convert byte lists, and `print` and `log.debug/info/warn/error` go to the app's log.
//!
//! [`LuaLimits`] caps memory (64 MiB) and the instructions one call may run. A call that
//! raises an error, runs out of memory or spends its budget does not stall ingest: the
//! bytes it was given become one [`PLUGIN_ERROR_KIND`] frame (severity error), and the
//! plugin starts over with a fresh `state` and nothing held back.
//!
//! What `decode` returns is capped too. A table or a string can be shared by many fields
//! and frames, so a few lines of Lua can describe far more than the VM's memory holds
//! once every use is copied into a frame. The frames of one call may take
//! [`max_frame_bytes`](LuaLimits::max_frame_bytes) (64 MiB) between them: a value, a
//! field name or a frame counts for 32 bytes, and text for its length (the exact charges
//! are in the `Budget` docs in `convert.rs`). A frame past that, and every frame after
//! it in the call, is a [`PLUGIN_ERROR_KIND`] frame over its bytes. The next call starts
//! over with a full budget.
//!
//! # Threads
//!
//! The codec runs on the ingest thread, synchronously: no executor, no helper thread.
//! Its VM stays on the thread that made it, so a `LuaCodec` is not `Send`: the app
//! hands the ingest thread a [`LuaCodecFactory`] (which is `Send + Sync`) and the codec
//! is made there, by
//! [`CodecSink::from_factory`](serialist_core::CodecSink::from_factory) inside
//! [`Ingest::spawn_with`](serialist_core::Ingest::spawn_with).

mod convert;
mod vm;

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use mlua::{MultiValue, Table, Value as LuaValue};
use serialist_core::codec::{
    Codec, CodecError, CodecFactory, CodecInfo, EncodeRequest, Frame, Severity,
};

use convert::Schema;
use vm::Vm;

/// Kind of the frame a Lua codec emits for bytes its plugin failed on.
pub const PLUGIN_ERROR_KIND: &str = "plugin_error";

/// What a Lua plugin may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LuaLimits {
    /// The VM's memory cap in bytes; 0 means no cap.
    pub memory_bytes: usize,
    /// Instructions one call into the plugin may run (`describe`, one `decode`, one
    /// `encode`, or loading `plugin.lua`).
    pub instructions_per_call: u64,
    /// How many instructions run between checks of the budget.
    pub check_every: u32,
    /// Most bytes `decode` may hold back; a plugin that holds back more gets a
    /// [`PLUGIN_ERROR_KIND`] frame for them instead.
    pub max_held_back: usize,
    /// Most bytes the frames of one `decode` call may take on the host between them,
    /// counted as the sandbox notes in the module docs say; a frame past it, and the rest
    /// of the call, are [`PLUGIN_ERROR_KIND`] frames. The VM's memory cap bounds what a
    /// plugin builds, not what it returns.
    pub max_frame_bytes: usize,
}

impl LuaLimits {
    pub const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
    /// Generous: a 64 KiB chunk costs the reference RACE plugin well under a million.
    /// Twenty million is about a tenth of a second of Lua.
    pub const DEFAULT_INSTRUCTIONS_PER_CALL: u64 = 20_000_000;
    pub const DEFAULT_CHECK_EVERY: u32 = 10_000;
    pub const DEFAULT_MAX_HELD_BACK: usize = 1024 * 1024;
    pub const DEFAULT_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
}

impl Default for LuaLimits {
    fn default() -> Self {
        Self {
            memory_bytes: Self::DEFAULT_MEMORY_BYTES,
            instructions_per_call: Self::DEFAULT_INSTRUCTIONS_PER_CALL,
            check_every: Self::DEFAULT_CHECK_EVERY,
            max_held_back: Self::DEFAULT_MAX_HELD_BACK,
            max_frame_bytes: Self::DEFAULT_MAX_FRAME_BYTES,
        }
    }
}

/// Where a plugin's code comes from.
#[derive(Clone, Debug)]
enum Origin {
    File(PathBuf),
    Source { name: String, code: Arc<str> },
}

impl Origin {
    fn name(&self) -> String {
        match self {
            Origin::File(path) => path.display().to_string(),
            Origin::Source { name, .. } => name.clone(),
        }
    }

    fn code(&self) -> Result<Arc<str>, CodecError> {
        match self {
            Origin::File(path) => std::fs::read_to_string(path)
                .map(Arc::from)
                .map_err(|err| CodecError::Internal(format!("{}: {err}", path.display()))),
            Origin::Source { code, .. } => Ok(Arc::clone(code)),
        }
    }
}

/// A loaded plugin: its VM, what it described, and its decode state.
struct Loaded {
    vm: Vm,
    info: CodecInfo,
    schema: Schema,
    state: Table,
}

impl Loaded {
    fn load(origin: &Origin, limits: &LuaLimits) -> Result<Self, CodecError> {
        let name = origin.name();
        let code = origin.code()?;
        let fail = |message: String| CodecError::Internal(format!("{name}: {message}"));
        let vm = Vm::load(&name, &code, limits).map_err(fail)?;
        let described = vm.call::<LuaValue>(&vm.describe, ()).map_err(fail)?;
        let info = convert::info_from_lua(described).map_err(fail)?;
        let state = vm
            .lua
            .create_table()
            .map_err(|err| fail(vm::describe_error(&err)))?;
        Ok(Self {
            schema: convert::schema(&info),
            vm,
            info,
            state,
        })
    }

    /// Forget the plugin's decode state.
    fn fresh_state(&mut self) {
        match self.vm.lua.create_table() {
            Ok(state) => self.state = state,
            Err(err) => tracing::warn!(error = %err, "could not make a fresh plugin state"),
        }
    }
}

/// A codec implemented by a Lua plugin. See the [module docs](self) for the contract.
pub struct LuaCodec {
    origin: Origin,
    limits: LuaLimits,
    loaded: Loaded,
    /// Bytes `decode` held back, prepended to the next chunk.
    held: Vec<u8>,
    /// Scratch for joining `held` and a chunk.
    input: Vec<u8>,
}

impl fmt::Debug for LuaCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LuaCodec")
            .field("origin", &self.origin)
            .field("name", &self.loaded.info.name)
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl LuaCodec {
    /// Load the plugin at `path` (a `plugin.lua`) with the default limits.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CodecError> {
        Self::load_with(path, LuaLimits::default())
    }

    pub fn load_with(path: impl AsRef<Path>, limits: LuaLimits) -> Result<Self, CodecError> {
        Self::new(Origin::File(path.as_ref().to_owned()), limits)
    }

    /// A plugin from source text; `name` appears in error messages.
    pub fn from_source(name: &str, code: &str, limits: LuaLimits) -> Result<Self, CodecError> {
        Self::new(
            Origin::Source {
                name: name.to_owned(),
                code: Arc::from(code),
            },
            limits,
        )
    }

    fn new(origin: Origin, limits: LuaLimits) -> Result<Self, CodecError> {
        let loaded = Loaded::load(&origin, &limits)?;
        Ok(Self {
            origin,
            limits,
            loaded,
            held: Vec::new(),
            input: Vec::new(),
        })
    }

    /// Load the plugin again (re-reading its file) into a fresh VM, and start decoding
    /// over. If it fails to load, the old one keeps running and the error comes back.
    pub fn reload(&mut self) -> Result<(), CodecError> {
        self.loaded = Loaded::load(&self.origin, &self.limits)?;
        self.held.clear();
        Ok(())
    }

    /// The plugin's file, if it was loaded from one.
    pub fn path(&self) -> Option<&Path> {
        match &self.origin {
            Origin::File(path) => Some(path),
            Origin::Source { .. } => None,
        }
    }

    /// What the plugin described when it was loaded.
    pub fn info(&self) -> &CodecInfo {
        &self.loaded.info
    }

    pub fn limits(&self) -> LuaLimits {
        self.limits
    }

    /// Bytes held back for the next chunk.
    pub fn held_back(&self) -> usize {
        self.held.len()
    }

    fn fail(
        &mut self,
        raw: std::ops::Range<u64>,
        at: Instant,
        message: &str,
        out: &mut Vec<Frame>,
    ) {
        tracing::warn!(plugin = %self.loaded.info.name, %message, "a Lua codec failed");
        let first_line = message.lines().next().unwrap_or_default();
        out.push(
            Frame::new(PLUGIN_ERROR_KIND, raw, at)
                .with_severity(Severity::Error)
                .with_summary(format!("plugin error: {first_line}"))
                .with_field("error", message),
        );
    }

    /// One call to the plugin's `decode` over `input`, which starts at stream offset `base`.
    fn decode_input(&mut self, input: &[u8], base: u64, at: Instant, out: &mut Vec<Frame>) {
        let end = base + input.len() as u64;
        let result = {
            let loaded = &self.loaded;
            loaded
                .vm
                .lua
                .create_string(input)
                .map_err(|err| vm::describe_error(&err))
                .and_then(|bytes| {
                    loaded
                        .vm
                        .call::<(LuaValue, LuaValue)>(&loaded.vm.decode, (bytes, &loaded.state))
                })
        };
        let (frames, rest) = match result {
            Ok(values) => values,
            Err(message) => {
                self.fail(base..end, at, &message, out);
                self.loaded.fresh_state();
                return;
            }
        };
        if let Err(message) = self.collect_frames(frames, input.len(), base, at, out) {
            self.fail(base..end, at, &message, out);
        }
        match rest {
            LuaValue::Nil => {}
            LuaValue::String(rest) => {
                let rest = rest.as_bytes();
                if !input.ends_with(&rest) {
                    let message = "decode must hold back a suffix of the bytes it was given";
                    self.fail(base..end, at, message, out);
                } else if rest.len() > self.limits.max_held_back {
                    let message = format!(
                        "decode held back {} bytes, more than the limit of {}",
                        rest.len(),
                        self.limits.max_held_back
                    );
                    self.fail(end - rest.len() as u64..end, at, &message, out);
                } else {
                    self.held.extend_from_slice(&rest);
                }
            }
            other => {
                let message = format!(
                    "decode's second result must be the bytes to hold back, not {}",
                    other.type_name()
                );
                self.fail(base..end, at, &message, out);
            }
        }
    }

    fn collect_frames(
        &mut self,
        frames: LuaValue,
        input_len: usize,
        base: u64,
        at: Instant,
        out: &mut Vec<Frame>,
    ) -> Result<(), String> {
        let frames = match frames {
            LuaValue::Nil => return Ok(()),
            LuaValue::Table(frames) => frames,
            other => {
                return Err(format!(
                    "decode must return a list of frames, not {}",
                    other.type_name()
                ));
            }
        };
        let mut budget = convert::Budget::new(self.limits.max_frame_bytes);
        for (i, item) in frames.sequence_values::<LuaValue>().enumerate() {
            let item = item.map_err(|err| vm::describe_error(&err))?;
            let LuaValue::Table(table) = item else {
                return Err(format!(
                    "frame {} must be a table, not {}",
                    i + 1,
                    item.type_name()
                ));
            };
            let (start, len) = convert::frame_span(&table, input_len)
                .map_err(|err| format!("frame {}: {err}", i + 1))?;
            let raw = base + start as u64..base + (start + len) as u64;
            let frame =
                convert::frame_from_lua(&table, &self.loaded.schema, raw.clone(), at, &mut budget);
            match frame {
                Ok(frame) => out.push(frame),
                Err(err) => self.fail(raw, at, &format!("frame {}: {err}", i + 1), out),
            }
        }
        Ok(())
    }
}

impl Codec for LuaCodec {
    fn describe(&self) -> CodecInfo {
        self.loaded.info.clone()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        if chunk.is_empty() {
            return;
        }
        let base = raw_offset.saturating_sub(self.held.len() as u64);
        if self.held.is_empty() {
            self.decode_input(chunk, base, at, out);
        } else {
            let mut input = std::mem::take(&mut self.input);
            input.clear();
            input.append(&mut self.held);
            input.extend_from_slice(chunk);
            self.decode_input(&input, base, at, out);
            self.input = input;
        }
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        let vm = &self.loaded.vm;
        let table = (|| {
            let table = vm
                .lua
                .create_table()
                .map_err(|err| vm::describe_error(&err))?;
            table
                .raw_set("command", request.command.as_str())
                .map_err(|err| vm::describe_error(&err))?;
            let fields = convert::object_to_lua(&vm.lua, &request.fields, &vm.array_mt, 0)?;
            table
                .raw_set("fields", fields)
                .map_err(|err| vm::describe_error(&err))?;
            Ok::<_, String>(table)
        })()
        .map_err(CodecError::Internal)?;
        let mut results = vm
            .call::<MultiValue>(&vm.encode, table)
            .map_err(CodecError::Internal)?
            .into_iter();
        let first = results.next().unwrap_or(LuaValue::Nil);
        let second = results.next().unwrap_or(LuaValue::Nil);
        convert::encode_result(first, second)
    }

    fn reset(&mut self) {
        self.held.clear();
        self.loaded.fresh_state();
    }
}

/// Makes [`LuaCodec`]s from one plugin. Loads the plugin once up front to learn what it
/// describes (and to report a broken plugin early); each [`create`](CodecFactory::create)
/// loads it again into a fresh VM, re-reading a file so edits since are picked up.
#[derive(Clone, Debug)]
pub struct LuaCodecFactory {
    origin: Origin,
    limits: LuaLimits,
    info: CodecInfo,
}

impl LuaCodecFactory {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CodecError> {
        Self::load_with(path, LuaLimits::default())
    }

    pub fn load_with(path: impl AsRef<Path>, limits: LuaLimits) -> Result<Self, CodecError> {
        Self::new(Origin::File(path.as_ref().to_owned()), limits)
    }

    pub fn from_source(name: &str, code: &str, limits: LuaLimits) -> Result<Self, CodecError> {
        Self::new(
            Origin::Source {
                name: name.to_owned(),
                code: Arc::from(code),
            },
            limits,
        )
    }

    fn new(origin: Origin, limits: LuaLimits) -> Result<Self, CodecError> {
        let info = LuaCodec::new(origin.clone(), limits)?.loaded.info;
        Ok(Self {
            origin,
            limits,
            info,
        })
    }

    /// The plugin's file, if it was loaded from one.
    pub fn path(&self) -> Option<&Path> {
        match &self.origin {
            Origin::File(path) => Some(path),
            Origin::Source { .. } => None,
        }
    }

    /// A codec that is a [`LuaCodec`] rather than a trait object.
    pub fn create_lua(&self) -> Result<LuaCodec, CodecError> {
        LuaCodec::new(self.origin.clone(), self.limits)
    }
}

impl CodecFactory for LuaCodecFactory {
    fn info(&self) -> CodecInfo {
        self.info.clone()
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        Ok(Box::new(self.create_lua()?))
    }
}
