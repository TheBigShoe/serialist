//! One plugin's sandboxed Lua VM: the same library set and limits as the script host
//! (`serialist-script`), plus a per-call instruction budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mlua::chunk::ChunkMode;
use mlua::{
    FromLuaMulti, Function, HookTriggers, IntoLuaMulti, Lua, LuaOptions, LuaString, StdLib, Table,
    Value as LuaValue, Variadic, VmState,
};
use serialist_core::codec::{decode_hex, encode_hex};

use super::LuaLimits;

/// Wraps `pcall`, `xpcall`, `coroutine.resume` and `load`, and builds the `codec` table.
/// Runs once per VM, before the plugin, and returns the metatable that marks tables made
/// from JSON arrays.
///
/// - A plugin cannot swallow the end of its budget: an error caught by `pcall`, `xpcall`
///   or `coroutine.resume` is raised again once the budget is spent.
/// - `load` only takes text chunks.
const PRELUDE: &str = r##"
local raw_pcall, raw_xpcall, raw_resume, raw_load, spent, null = ...

local function recheck(ok, ...)
  if not ok and spent() then
    error((...), 0)
  end
  return ok, ...
end

pcall = function(f, ...) return recheck(raw_pcall(f, ...)) end
xpcall = function(f, handler, ...) return recheck(raw_xpcall(f, handler, ...)) end
coroutine.resume = function(co, ...) return recheck(raw_resume(co, ...)) end

load = function(chunk, name, mode, ...)
  if select("#", ...) > 0 then
    return raw_load(chunk, name, "t", ...)
  end
  return raw_load(chunk, name, "t")
end

local ARRAY = {}
codec = {
  null = null,
  is_array = function(t) return type(t) == "table" and getmetatable(t) == ARRAY end,
  unknown_command = function(name) return { code = "unknown_command", command = name } end,
  missing_field = function(name) return { code = "missing_field", field = name } end,
  bad_field = function(name, reason) return { code = "bad_field", field = name, reason = reason } end,
}
return ARRAY
"##;

/// A plugin's instruction budget for one call into it.
pub(super) struct Budget {
    checks: AtomicU64,
    max_checks: u64,
    spent: AtomicBool,
    instructions: u64,
}

impl Budget {
    fn new(limits: &LuaLimits) -> Self {
        let every = u64::from(limits.check_every.max(1));
        Self {
            checks: AtomicU64::new(0),
            max_checks: limits.instructions_per_call.div_ceil(every).max(1),
            spent: AtomicBool::new(false),
            instructions: limits.instructions_per_call,
        }
    }

    fn start(&self) {
        self.checks.store(0, Ordering::Relaxed);
        self.spent.store(false, Ordering::Relaxed);
    }

    /// Called from the hook: `false` once the budget is spent (and on every later check).
    fn tick(&self) -> bool {
        let checks = self.checks.fetch_add(1, Ordering::Relaxed) + 1;
        if checks > self.max_checks {
            self.spent.store(true, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    fn is_spent(&self) -> bool {
        self.spent.load(Ordering::Relaxed)
    }

    fn message(&self) -> String {
        format!(
            "the plugin ran past its budget of {} instructions in one call",
            self.instructions
        )
    }
}

/// A loaded plugin: its VM and the functions it returned.
pub(super) struct Vm {
    pub(super) lua: Lua,
    pub(super) describe: Function,
    pub(super) decode: Function,
    pub(super) encode: Function,
    /// Metatable of tables made from JSON arrays (`codec.is_array`).
    pub(super) array_mt: Table,
    budget: Arc<Budget>,
}

impl Vm {
    /// A fresh sandbox running `code` (the plugin's `plugin.lua`), which must return a
    /// table with `describe`, `decode` and `encode` functions.
    pub(super) fn load(name: &str, code: &str, limits: &LuaLimits) -> Result<Vm, String> {
        let budget = Arc::new(Budget::new(limits));
        let (lua, array_mt) =
            sandbox(name, limits, Arc::clone(&budget)).map_err(|err| describe_error(&err))?;
        budget.start();
        let module = lua
            .load(code)
            .set_name(format!("@{name}"))
            .set_mode(ChunkMode::Text)
            .eval::<LuaValue>();
        if budget.is_spent() {
            return Err(budget.message());
        }
        let module = match module.map_err(|err| describe_error(&err))? {
            LuaValue::Table(module) => module,
            other => {
                return Err(format!(
                    "plugin.lua must return a table with describe, decode and encode, not {}",
                    other.type_name()
                ));
            }
        };
        let function = |key: &str| match module.raw_get::<LuaValue>(key) {
            Ok(LuaValue::Function(f)) => Ok(f),
            Ok(other) => Err(format!(
                "the plugin's `{key}` must be a function, not {}",
                other.type_name()
            )),
            Err(err) => Err(describe_error(&err)),
        };
        Ok(Vm {
            describe: function("describe")?,
            decode: function("decode")?,
            encode: function("encode")?,
            lua,
            array_mt,
            budget,
        })
    }

    /// Call `f` under a fresh instruction budget.
    pub(super) fn call<R: FromLuaMulti>(
        &self,
        f: &Function,
        args: impl IntoLuaMulti,
    ) -> Result<R, String> {
        self.budget.start();
        let result = f.call::<R>(args);
        if self.budget.is_spent() {
            return Err(self.budget.message());
        }
        result.map_err(|err| describe_error(&err))
    }
}

fn sandbox(name: &str, limits: &LuaLimits, budget: Arc<Budget>) -> mlua::Result<(Lua, Table)> {
    let libs = StdLib::COROUTINE
        | StdLib::TABLE
        | StdLib::STRING
        | StdLib::UTF8
        | StdLib::MATH
        | StdLib::OS;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    if limits.memory_bytes > 0 {
        lua.set_memory_limit(limits.memory_bytes)?;
    }
    let hook_budget = Arc::clone(&budget);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(limits.check_every.max(1)),
        move |_lua, _debug| {
            if hook_budget.tick() {
                Ok(VmState::Continue)
            } else {
                Err(mlua::Error::runtime(hook_budget.message()))
            }
        },
    )?;

    let globals = lua.globals();
    let os: Table = globals.get("os")?;
    let safe_os = lua.create_table()?;
    for key in ["time", "clock", "date"] {
        safe_os.set(key, os.get::<LuaValue>(key)?)?;
    }
    globals.set("os", safe_os)?;
    for key in ["io", "package", "debug", "loadfile", "dofile", "require"] {
        globals.set(key, mlua::Nil)?;
    }
    let string: Table = globals.get("string")?;
    string.set("dump", mlua::Nil)?;

    globals.set("hex", hex_table(&lua)?)?;
    globals.set("bytes", bytes_table(&lua)?)?;
    install_logging(&lua, name)?;

    let coroutine: Table = globals.get("coroutine")?;
    let spent = lua.create_function(move |_, ()| Ok(budget.is_spent()))?;
    let array_mt: Table = lua.load(PRELUDE).set_name("=prelude").call((
        globals.get::<Function>("pcall")?,
        globals.get::<Function>("xpcall")?,
        coroutine.get::<Function>("resume")?,
        globals.get::<Function>("load")?,
        spent,
        LuaValue::NULL,
    ))?;
    Ok((lua, array_mt))
}

/// A string, or a table of byte values, as bytes.
pub(super) fn bytes_arg(value: &LuaValue, what: &str) -> Result<Vec<u8>, String> {
    match value {
        LuaValue::String(s) => Ok(s.as_bytes().to_vec()),
        LuaValue::Table(table) => {
            let mut out = Vec::with_capacity(table.raw_len());
            for (i, item) in table.sequence_values::<LuaValue>().enumerate() {
                let item = item.map_err(|err| describe_error(&err))?;
                let byte = match item {
                    LuaValue::Integer(n) => u8::try_from(n).ok(),
                    LuaValue::Number(n) if n.fract() == 0.0 && (0.0..=255.0).contains(&n) => {
                        Some(n as u8)
                    }
                    _ => None,
                };
                let Some(byte) = byte else {
                    return Err(format!(
                        "{what}: item {} is not a byte (an integer from 0 to 255)",
                        i + 1
                    ));
                };
                out.push(byte);
            }
            Ok(out)
        }
        other => Err(format!(
            "{what}: expected a string or a table of bytes, got {}",
            other.type_name()
        )),
    }
}

fn hex_table(lua: &Lua) -> mlua::Result<Table> {
    let hex = lua.create_table()?;
    hex.set(
        "encode",
        lua.create_function(|_, (data, separator): (LuaValue, Option<String>)| {
            let bytes = bytes_arg(&data, "hex.encode").map_err(mlua::Error::runtime)?;
            Ok(encode_hex(&bytes, separator.as_deref().unwrap_or(" ")))
        })?,
    )?;
    hex.set(
        "decode",
        lua.create_function(|lua, text: LuaString| {
            let text = text.to_str()?;
            let bytes = decode_hex(&text)
                .map_err(|err| mlua::Error::runtime(format!("hex.decode: {err}")))?;
            lua.create_string(&bytes)
        })?,
    )?;
    Ok(hex)
}

fn bytes_table(lua: &Lua) -> mlua::Result<Table> {
    let bytes = lua.create_table()?;
    bytes.set(
        "from_table",
        lua.create_function(|lua, data: LuaValue| {
            let bytes = bytes_arg(&data, "bytes.from_table").map_err(mlua::Error::runtime)?;
            lua.create_string(&bytes)
        })?,
    )?;
    bytes.set(
        "to_table",
        lua.create_function(|lua, data: LuaString| {
            lua.create_sequence_from(data.as_bytes().iter().copied())
        })?,
    )?;
    Ok(bytes)
}

/// `print(...)` and `log.debug/info/warn/error(...)` go to `tracing`, tagged with the
/// plugin's name.
fn install_logging(lua: &Lua, name: &str) -> mlua::Result<()> {
    fn join(values: &Variadic<LuaValue>) -> String {
        values
            .iter()
            .map(|v| v.to_string().unwrap_or_else(|_| v.type_name().to_owned()))
            .collect::<Vec<_>>()
            .join(" ")
    }
    let globals = lua.globals();
    let plugin = name.to_owned();
    globals.set(
        "print",
        lua.create_function(move |_, values: Variadic<LuaValue>| {
            tracing::info!(target: "serialist_plugins::lua", plugin = %plugin, "{}", join(&values));
            Ok(())
        })?,
    )?;
    let log = lua.create_table()?;
    for level in ["debug", "info", "warn", "error"] {
        let plugin = name.to_owned();
        log.set(
            level,
            lua.create_function(move |_, values: Variadic<LuaValue>| {
                let text = join(&values);
                match level {
                    "debug" => tracing::debug!(target: "serialist_plugins::lua", plugin = %plugin, "{text}"),
                    "info" => tracing::info!(target: "serialist_plugins::lua", plugin = %plugin, "{text}"),
                    "warn" => tracing::warn!(target: "serialist_plugins::lua", plugin = %plugin, "{text}"),
                    _ => tracing::error!(target: "serialist_plugins::lua", plugin = %plugin, "{text}"),
                }
                Ok(())
            })?,
        )?;
    }
    globals.set("log", log)
}

/// An error as one message: mlua's text without the traceback it appends.
pub(super) fn describe_error(err: &mlua::Error) -> String {
    let text = match err {
        mlua::Error::MemoryError(message) => format!("memory error: {message}"),
        other => other.to_string(),
    };
    let text = match text.find("\nstack traceback:") {
        Some(cut) => &text[..cut],
        None => &text,
    };
    text.trim_end().to_owned()
}
