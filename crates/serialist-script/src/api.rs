//! The global Lua API: `serial`, `sleep`, `print`, `log`, `ui`, `commands`, `hex`,
//! `bytes`, and the sandboxed `require` and `dofile`. Port methods live in
//! [`crate::port`].

use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use mlua::chunk::ChunkMode;
use mlua::{Function, IntoLuaMulti, Lua, LuaString, MultiValue, Table, Value, Variadic};
use serialist_core::{DeviceMatch, ParamValues, PortId, PortInfo, PortKind, SerialConfig, UsbId};

use crate::hex::{decode_hex, encode_hex};
use crate::host::ScriptEvent;
use crate::port::{Port, PortState, bytes_arg, duration_ms};
use crate::services::{LogLevel, OpenRequest};
use crate::vm::{RunCtx, runtime_error};

pub(crate) fn install(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<()> {
    let globals = lua.globals();
    let tostring: Function = globals.get("tostring")?;
    globals.set("print", print_fn(lua, ctx, tostring.clone())?)?;
    globals.set("log", log_table(lua, ctx, &tostring)?)?;
    globals.set("sleep", sleep_fn(lua, ctx)?)?;
    globals.set("ui", ui_table(lua, ctx)?)?;
    globals.set("commands", commands_table(lua, ctx, tostring)?)?;
    globals.set("hex", hex_table(lua)?)?;
    globals.set("bytes", bytes_table(lua)?)?;
    globals.set("serial", serial_table(lua, ctx)?)?;
    globals.set("require", require_fn(lua, ctx)?)?;
    globals.set("dofile", dofile_fn(lua, ctx)?)?;
    Ok(())
}

/// A value as `tostring` shows it; strings as they are (invalid UTF-8 replaced).
fn display(tostring: &Function, value: &Value) -> mlua::Result<String> {
    Ok(match value {
        Value::String(s) => s.to_string_lossy(),
        Value::Integer(n) => n.to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::Nil => "nil".to_owned(),
        other => tostring.call::<LuaString>(other.clone())?.to_string_lossy(),
    })
}

fn join(tostring: &Function, values: &[Value], separator: &str) -> mlua::Result<String> {
    let mut out = String::new();
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        out.push_str(&display(tostring, value)?);
    }
    Ok(out)
}

fn print_fn(lua: &Lua, ctx: &Rc<RunCtx>, tostring: Function) -> mlua::Result<Function> {
    let ctx = Rc::clone(ctx);
    lua.create_function(move |_, values: Variadic<Value>| {
        let text = join(&tostring, &values, "\t")?;
        ctx.emit(ScriptEvent::Output(text));
        Ok(())
    })
}

fn log_table(lua: &Lua, ctx: &Rc<RunCtx>, tostring: &Function) -> mlua::Result<Table> {
    let log = lua.create_table()?;
    for level in [
        LogLevel::Debug,
        LogLevel::Info,
        LogLevel::Warn,
        LogLevel::Error,
    ] {
        let ctx = Rc::clone(ctx);
        let tostring = tostring.clone();
        let function = lua.create_function(move |_, values: Variadic<Value>| {
            let text = join(&tostring, &values, " ")?;
            ctx.services.ui.log(level, &text);
            ctx.emit(ScriptEvent::Output(format!("[{level}] {text}")));
            Ok(())
        })?;
        log.set(level.as_str(), function)?;
    }
    Ok(log)
}

fn sleep_fn(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Function> {
    let ctx = Rc::clone(ctx);
    lua.create_async_function(move |lua, ms: f64| {
        let ctx = Rc::clone(&ctx);
        let checked = ctx
            .check_waitable(&lua, "sleep")
            .and_then(|()| duration_ms(ms, "sleep"));
        async move {
            let duration = checked?;
            ctx.or_stop(tokio::time::sleep(duration)).await
        }
    })
}

fn ui_table(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Table> {
    let ui = lua.create_table()?;
    let prompt_ctx = Rc::clone(ctx);
    ui.set(
        "prompt",
        lua.create_async_function(move |lua, (label, default): (String, Option<String>)| {
            let ctx = Rc::clone(&prompt_ctx);
            let checked = ctx.check_waitable(&lua, "ui.prompt");
            async move {
                checked?;
                ctx.emit(ScriptEvent::Prompt {
                    id: ctx.next_prompt_id(),
                    label: label.clone(),
                    default: default.clone(),
                });
                let answer = ctx.services.ui.prompt(&label, default.as_deref());
                ctx.or_stop(answer).await
            }
        })?,
    )?;
    let notify_ctx = Rc::clone(ctx);
    ui.set(
        "notify",
        lua.create_function(move |_, text: String| {
            notify_ctx.services.ui.notify(&text);
            Ok(())
        })?,
    )?;
    Ok(ui)
}

fn commands_table(lua: &Lua, ctx: &Rc<RunCtx>, tostring: Function) -> mlua::Result<Table> {
    let commands = lua.create_table()?;
    let ctx = Rc::clone(ctx);
    commands.set(
        "send",
        lua.create_function(move |lua, (name, params): (String, Option<Table>)| {
            let Some(sender) = &ctx.services.commands else {
                return Err(runtime_error(
                    "commands.send: saved commands are not available in this host",
                ));
            };
            let mut values = ParamValues::new();
            if let Some(params) = params {
                for pair in params.pairs::<Value, Value>() {
                    let (key, value) = pair?;
                    let Value::String(key) = key else {
                        return Err(runtime_error(format!(
                            "commands.send: parameter names must be strings, got {}",
                            key.type_name()
                        )));
                    };
                    values.set(key.to_string_lossy(), display(&tostring, &value)?);
                }
            }
            match sender.send(&name, &values) {
                Ok(()) => true.into_lua_multi(lua),
                Err(err) => (Value::Nil, err.0).into_lua_multi(lua),
            }
        })?,
    )?;
    Ok(commands)
}

fn hex_table(lua: &Lua) -> mlua::Result<Table> {
    let hex = lua.create_table()?;
    hex.set(
        "encode",
        lua.create_function(|_, (data, separator): (Value, Option<String>)| {
            let bytes = bytes_arg(&data, "hex.encode")?;
            Ok(encode_hex(&bytes, separator.as_deref().unwrap_or(" ")))
        })?,
    )?;
    hex.set(
        "decode",
        lua.create_function(|lua, text: String| {
            let bytes =
                decode_hex(&text).map_err(|err| runtime_error(format!("hex.decode: {err}")))?;
            lua.create_string(&bytes)
        })?,
    )?;
    Ok(hex)
}

fn bytes_table(lua: &Lua) -> mlua::Result<Table> {
    let bytes = lua.create_table()?;
    bytes.set(
        "from_table",
        lua.create_function(|lua, data: Value| {
            lua.create_string(bytes_arg(&data, "bytes.from_table")?)
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

fn serial_table(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Table> {
    let serial = lua.create_table()?;

    let current_ctx = Rc::clone(ctx);
    serial.set(
        "current",
        lua.create_function(move |lua, ()| match &current_ctx.current {
            Some(state) => Port {
                state: Rc::clone(state),
                ctx: Rc::clone(&current_ctx),
            }
            .into_lua_multi(lua),
            None => (Value::Nil, "no session is attached to this script").into_lua_multi(lua),
        })?,
    )?;

    let ports_ctx = Rc::clone(ctx);
    serial.set(
        "ports",
        lua.create_function(move |lua, ()| {
            let list = lua.create_table()?;
            if let Some(source) = &ports_ctx.services.ports {
                for info in source.snapshot() {
                    list.raw_push(port_entry(lua, &info)?)?;
                }
            }
            Ok(list)
        })?,
    )?;

    let open_ctx = Rc::clone(ctx);
    serial.set(
        "open",
        lua.create_async_function(move |lua, options: Table| {
            let ctx = Rc::clone(&open_ctx);
            let checked = ctx
                .check_waitable(&lua, "serial.open")
                .and_then(|()| open_request(&options));
            async move {
                let request = checked?;
                let Some(opener) = ctx.services.opener.clone() else {
                    return (Value::Nil, "serial.open is not supported here").into_lua_multi(&lua);
                };
                let opening = tokio::task::spawn_blocking(move || opener.open(&request));
                let opened = ctx
                    .or_stop(opening)
                    .await?
                    .map_err(|err| runtime_error(format!("serial.open: {err}")))?;
                match opened {
                    Ok(session) => {
                        let state = Rc::new(PortState::new(session, true));
                        ctx.adopt(&state);
                        Port { state, ctx }.into_lua_multi(&lua)
                    }
                    Err(err) => (Value::Nil, err.to_string()).into_lua_multi(&lua),
                }
            }
        })?,
    )?;
    Ok(serial)
}

fn port_entry(lua: &Lua, info: &PortInfo) -> mlua::Result<Table> {
    let entry = lua.create_table()?;
    entry.set("id", info.id.as_str())?;
    entry.set("display_name", info.display_name.as_str())?;
    let kind = match &info.kind {
        PortKind::Usb(usb) => {
            entry.set("vid", usb.vid)?;
            entry.set("pid", usb.pid)?;
            entry.set("serial_number", usb.serial_number.as_deref())?;
            entry.set("manufacturer", usb.manufacturer.as_deref())?;
            entry.set("product", usb.product.as_deref())?;
            "usb"
        }
        PortKind::Bluetooth => "bluetooth",
        PortKind::Pci => "pci",
        PortKind::Virtual => "virtual",
        PortKind::Unknown => "unknown",
    };
    entry.set("kind", kind)?;
    Ok(entry)
}

fn open_request(options: &Table) -> mlua::Result<OpenRequest> {
    let mut request = OpenRequest {
        port: None,
        matching: None,
        serial: SerialConfig::default(),
    };
    for pair in options.pairs::<String, Value>() {
        let (key, value) = pair?;
        match (key.as_str(), value) {
            ("port", Value::String(id)) => {
                request.port = Some(PortId::new(id.to_str()?.to_owned()))
            }
            ("baud", Value::Integer(baud)) if baud > 0 => {
                request.serial.baud = u32::try_from(baud)
                    .map_err(|_| runtime_error(format!("serial.open: baud {baud} is too high")))?;
            }
            ("match", Value::Table(fields)) => request.matching = Some(device_match(&fields)?),
            ("port" | "baud" | "match", other) => {
                return Err(runtime_error(format!(
                    "serial.open: {key} has the wrong type ({})",
                    other.type_name()
                )));
            }
            (other, _) => {
                return Err(runtime_error(format!(
                    "serial.open: unknown option {other:?} (port, match and baud are known)"
                )));
            }
        }
    }
    if request.port.is_none() && request.matching.is_none() {
        return Err(runtime_error(
            "serial.open: give port = \"<id>\" or match = { product = ..., vid = ... }",
        ));
    }
    Ok(request)
}

fn device_match(fields: &Table) -> mlua::Result<DeviceMatch> {
    let mut matching = DeviceMatch::default();
    for pair in fields.pairs::<String, Value>() {
        let (key, value) = pair?;
        match key.as_str() {
            "vid" => matching.vid = Some(usb_id(&value, "vid")?),
            "pid" => matching.pid = Some(usb_id(&value, "pid")?),
            "product" | "manufacturer" | "serial_number" | "path" => {
                let Value::String(text) = value else {
                    return Err(runtime_error(format!(
                        "serial.open: match.{key} must be a string"
                    )));
                };
                let text = Some(text.to_str()?.to_owned());
                match key.as_str() {
                    "product" => matching.product = text,
                    "manufacturer" => matching.manufacturer = text,
                    "serial_number" => matching.serial_number = text,
                    _ => matching.path = text,
                }
            }
            other => {
                return Err(runtime_error(format!(
                    "serial.open: unknown match key {other:?} \
                     (vid, pid, product, manufacturer, serial_number and path are known)"
                )));
            }
        }
    }
    Ok(matching)
}

/// A USB id from an integer or a hex string (`"0x0e8d"` or `"0e8d"`).
fn usb_id(value: &Value, key: &str) -> mlua::Result<UsbId> {
    let parsed = match value {
        Value::Integer(n) => u16::try_from(*n).ok(),
        Value::String(text) => {
            let text = text.to_str()?;
            let digits = text.trim();
            let digits = digits
                .strip_prefix("0x")
                .or_else(|| digits.strip_prefix("0X"))
                .unwrap_or(digits);
            u16::from_str_radix(digits, 16).ok()
        }
        _ => None,
    };
    parsed.map(UsbId).ok_or_else(|| {
        runtime_error(format!(
            "serial.open: match.{key} must be a USB id (an integer, or hex like \"0x0e8d\")"
        ))
    })
}

/// `lib.util` names `lib/util.lua` or `lib/util/init.lua` inside the scripts directory.
fn module_candidates(name: &str) -> mlua::Result<Vec<PathBuf>> {
    let valid = !name.is_empty()
        && name.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    if !valid {
        return Err(runtime_error(format!(
            "require: {name:?} is not a module name; modules are dotted names such as \
             \"lib.util\" (lib/util.lua) inside the scripts directory"
        )));
    }
    let base: PathBuf = name.split('.').collect();
    Ok(vec![base.with_extension("lua"), base.join("init.lua")])
}

/// A relative path with no `..`, for `dofile`.
fn file_candidate(path: &str) -> mlua::Result<PathBuf> {
    let relative = Path::new(path);
    let valid = !path.is_empty()
        && relative
            .components()
            .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if !valid {
        return Err(runtime_error(format!(
            "dofile: {path:?} must be a relative path inside the scripts directory, without \"..\""
        )));
    }
    Ok(relative.to_path_buf())
}

/// Reads the first of `candidates` (relative to `dir`) that exists, on a worker thread,
/// refusing anything that resolves outside `dir` (through a symlink, say). Returns the
/// relative name for tracebacks and the code.
async fn read_script(
    ctx: &RunCtx,
    what: &'static str,
    candidates: Vec<PathBuf>,
) -> mlua::Result<(String, String)> {
    let Some(dir) = ctx.services.scripts_dir.clone() else {
        return Err(runtime_error(format!(
            "{what}: no scripts directory is configured, so nothing can be loaded"
        )));
    };
    let reading = tokio::task::spawn_blocking(move || -> Result<(String, String), String> {
        let root = dir.canonicalize().map_err(|err| {
            format!(
                "the scripts directory {} cannot be read: {err}",
                dir.display()
            )
        })?;
        for relative in &candidates {
            let Ok(full) = root.join(relative).canonicalize() else {
                continue;
            };
            if !full.starts_with(&root) {
                return Err(format!(
                    "{} resolves outside the scripts directory",
                    relative.display()
                ));
            }
            let code = std::fs::read_to_string(&full)
                .map_err(|err| format!("cannot read {}: {err}", relative.display()))?;
            let name = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            return Ok((name, code));
        }
        let tried = candidates
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("not found in {} (tried {tried})", root.display()))
    });
    ctx.or_stop(reading)
        .await?
        .map_err(|err| runtime_error(format!("{what}: {err}")))?
        .map_err(|err| runtime_error(format!("{what}: {err}")))
}

fn require_fn(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Function> {
    let loaded = lua.create_table()?;
    let loading = lua.create_table()?;
    let ctx = Rc::clone(ctx);
    lua.create_async_function(move |lua, name: String| {
        let ctx = Rc::clone(&ctx);
        let loaded = loaded.clone();
        let loading = loading.clone();
        let checked = ctx.check_waitable(&lua, "require");
        async move {
            checked?;
            let cached: Value = loaded.raw_get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            if loading.raw_get::<bool>(name.as_str())? {
                return Err(runtime_error(format!(
                    "require: {name:?} is already being loaded (a circular require)"
                )));
            }
            let candidates = module_candidates(&name)?;
            let (file, code) = read_script(&ctx, "require", candidates).await?;
            loading.raw_set(name.as_str(), true)?;
            let result = match lua
                .load(code)
                .set_name(format!("@{file}"))
                .set_mode(ChunkMode::Text)
                .into_function()
            {
                Ok(chunk) => chunk.call_async::<Value>(name.as_str()).await,
                Err(err) => Err(err),
            };
            loading.raw_set(name.as_str(), Value::Nil)?;
            let value = match result? {
                Value::Nil => Value::Boolean(true),
                value => value,
            };
            loaded.raw_set(name.as_str(), value.clone())?;
            Ok(value)
        }
    })
}

fn dofile_fn(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Function> {
    let ctx = Rc::clone(ctx);
    lua.create_async_function(move |lua, path: String| {
        let ctx = Rc::clone(&ctx);
        let checked = ctx
            .check_waitable(&lua, "dofile")
            .and_then(|()| file_candidate(&path));
        async move {
            let candidate = checked?;
            let (file, code) = read_script(&ctx, "dofile", vec![candidate]).await?;
            let chunk = lua
                .load(code)
                .set_name(format!("@{file}"))
                .set_mode(ChunkMode::Text)
                .into_function()?;
            chunk.call_async::<MultiValue>(()).await
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_names_stay_inside_the_directory() {
        assert_eq!(
            module_candidates("lib.util").unwrap(),
            [
                PathBuf::from("lib/util.lua"),
                PathBuf::from("lib/util/init.lua")
            ]
        );
        for bad in [
            "",
            "../x",
            "..x",
            "a..b",
            "/etc/passwd",
            "a/b",
            "a.",
            ".a",
            "x y",
        ] {
            assert!(module_candidates(bad).is_err(), "{bad:?} accepted");
        }
        assert!(file_candidate("sub/file.lua").is_ok());
        for bad in ["", "../x.lua", "sub/../../x.lua", "/etc/passwd"] {
            assert!(file_candidate(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn open_requests_parse() {
        let lua = Lua::new();
        let options: Table = lua
            .load(r#"return { port = "virtual:at", baud = 9600 }"#)
            .eval()
            .unwrap();
        let request = open_request(&options).unwrap();
        assert_eq!(request.port, Some(PortId::new("virtual:at")));
        assert_eq!(request.serial.baud, 9600);

        let options: Table = lua
            .load(r#"return { match = { product = "Airoha", vid = "0x0e8d", pid = 3 } }"#)
            .eval()
            .unwrap();
        let matching = open_request(&options).unwrap().matching.unwrap();
        assert_eq!(matching.product.as_deref(), Some("Airoha"));
        assert_eq!(matching.vid, Some(UsbId(0x0e8d)));
        assert_eq!(matching.pid, Some(UsbId(3)));

        for bad in [
            "return {}",
            "return { baud = 9600 }",
            "return { port = 3 }",
            "return { port = 'x', speed = 1 }",
            "return { match = { vid = 'zz' } }",
        ] {
            let options: Table = lua.load(bad).eval().unwrap();
            assert!(open_request(&options).is_err(), "{bad} accepted");
        }
    }
}
