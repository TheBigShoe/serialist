//! Values across the Lua boundary: plugin descriptions and frames coming out, encode
//! requests going in.

use std::collections::HashMap;
use std::time::Instant;

use mlua::{Lua, LuaString, Table, Value as LuaValue};
use serde_json::Value as JsonValue;
use serialist_core::codec::{
    CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Severity,
    SmolStr, Value,
};

use super::vm::{bytes_arg, describe_error};

/// Deepest nesting of lists (frames) or arrays and objects (requests) converted.
const MAX_DEPTH: usize = 32;

/// Declared fields of each frame kind, in order.
pub(super) type Schema = HashMap<String, Vec<(SmolStr, FieldType, bool)>>;

pub(super) fn schema(info: &CodecInfo) -> Schema {
    info.kinds
        .iter()
        .map(|kind| {
            let fields = kind
                .fields
                .iter()
                .map(|f| (SmolStr::new(&f.name), f.ty, f.optional))
                .collect();
            (kind.kind.clone(), fields)
        })
        .collect()
}

fn lua_err(err: mlua::Error) -> String {
    describe_error(&err)
}

fn string_of(value: &LuaValue, what: &str) -> Result<String, String> {
    match value {
        LuaValue::String(s) => Ok(s.to_string_lossy()),
        other => Err(format!(
            "{what} must be a string, not {}",
            other.type_name()
        )),
    }
}

fn opt_string(table: &Table, key: &str, what: &str) -> Result<Option<String>, String> {
    match table.raw_get::<LuaValue>(key).map_err(lua_err)? {
        LuaValue::Nil => Ok(None),
        value => string_of(&value, &format!("{what}.{key}")).map(Some),
    }
}

fn req_string(table: &Table, key: &str, what: &str) -> Result<String, String> {
    opt_string(table, key, what)?.ok_or_else(|| format!("{what}.{key} is missing"))
}

fn list(table: &Table, key: &str, what: &str) -> Result<Vec<Table>, String> {
    match table.raw_get::<LuaValue>(key).map_err(lua_err)? {
        LuaValue::Nil => Ok(Vec::new()),
        LuaValue::Table(items) => items
            .sequence_values::<LuaValue>()
            .enumerate()
            .map(|(i, item)| match item.map_err(lua_err)? {
                LuaValue::Table(t) => Ok(t),
                other => Err(format!(
                    "{what}.{key}[{}] must be a table, not {}",
                    i + 1,
                    other.type_name()
                )),
            })
            .collect(),
        other => Err(format!(
            "{what}.{key} must be a list, not {}",
            other.type_name()
        )),
    }
}

fn fields_info(table: &Table, what: &str) -> Result<Vec<FieldInfo>, String> {
    list(table, "fields", what)?
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let what = format!("{what}.fields[{}]", i + 1);
            let name = req_string(field, "name", &what)?;
            let ty = req_string(field, "type", &what)?;
            let ty = FieldType::from_name(&ty).ok_or_else(|| {
                format!("{what}.type is {ty:?}; use bool, int, uint, float, str, bytes or list")
            })?;
            let optional = match field.raw_get::<LuaValue>("optional").map_err(lua_err)? {
                LuaValue::Nil => false,
                LuaValue::Boolean(b) => b,
                other => {
                    return Err(format!(
                        "{what}.optional must be a boolean, not {}",
                        other.type_name()
                    ));
                }
            };
            Ok(FieldInfo {
                name,
                ty,
                description: opt_string(field, "description", &what)?.unwrap_or_default(),
                optional,
            })
        })
        .collect()
}

/// What `describe()` returned, as a [`CodecInfo`].
pub(super) fn info_from_lua(value: LuaValue) -> Result<CodecInfo, String> {
    let LuaValue::Table(table) = value else {
        return Err(format!(
            "describe() must return a table, not {}",
            value.type_name()
        ));
    };
    let what = "describe()";
    let name = req_string(&table, "name", what)?;
    if name.trim().is_empty() {
        return Err("describe().name is empty".to_owned());
    }
    let kinds = list(&table, "kinds", what)?
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let what = format!("describe().kinds[{}]", i + 1);
            Ok(FrameKindInfo {
                kind: req_string(kind, "kind", &what)?,
                description: opt_string(kind, "description", &what)?.unwrap_or_default(),
                fields: fields_info(kind, &what)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let commands = list(&table, "commands", what)?
        .iter()
        .enumerate()
        .map(|(i, command)| {
            let what = format!("describe().commands[{}]", i + 1);
            Ok(CommandInfo {
                name: req_string(command, "name", &what)?,
                description: opt_string(command, "description", &what)?.unwrap_or_default(),
                fields: fields_info(command, &what)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(CodecInfo {
        name,
        version: opt_string(&table, "version", what)?.unwrap_or_default(),
        description: opt_string(&table, "description", what)?.unwrap_or_default(),
        kinds,
        commands,
    })
}

/// An integral Lua number as an `i64`.
fn integer(value: &LuaValue) -> Option<i64> {
    match *value {
        LuaValue::Integer(n) => Some(n),
        LuaValue::Number(x) if x.fract() == 0.0 && x >= -(2f64.powi(63)) && x < 2f64.powi(63) => {
            Some(x as i64)
        }
        _ => None,
    }
}

/// A Lua value as a field value, of type `ty` if the kind declares one.
fn to_value(value: &LuaValue, ty: Option<FieldType>, depth: usize) -> Result<Value, String> {
    let mismatch = |ty: FieldType| {
        Err(format!(
            "is a {} but is declared {}",
            value.type_name(),
            ty.name()
        ))
    };
    match ty {
        Some(FieldType::Bytes) => match value {
            LuaValue::String(s) => Ok(Value::Bytes(s.as_bytes().to_vec())),
            _ => mismatch(FieldType::Bytes),
        },
        Some(FieldType::Str) => match value {
            LuaValue::String(s) => Ok(Value::Str(s.to_string_lossy())),
            _ => mismatch(FieldType::Str),
        },
        Some(FieldType::UInt) => match integer(value) {
            Some(n) if n >= 0 => Ok(Value::UInt(n as u64)),
            Some(n) => Err(format!("is {n} but is declared uint")),
            None => mismatch(FieldType::UInt),
        },
        Some(FieldType::Int) => integer(value)
            .map(Value::Int)
            .map_or_else(|| mismatch(FieldType::Int), Ok),
        Some(FieldType::Float) => match *value {
            LuaValue::Integer(n) => Ok(Value::Float(n as f64)),
            LuaValue::Number(x) => Ok(Value::Float(x)),
            _ => mismatch(FieldType::Float),
        },
        Some(FieldType::Bool) => match *value {
            LuaValue::Boolean(b) => Ok(Value::Bool(b)),
            _ => mismatch(FieldType::Bool),
        },
        Some(FieldType::List) => match value {
            LuaValue::Table(t) => list_value(t, depth),
            _ => mismatch(FieldType::List),
        },
        None => match value {
            LuaValue::Boolean(b) => Ok(Value::Bool(*b)),
            LuaValue::Integer(n) => Ok(Value::Int(*n)),
            LuaValue::Number(x) => Ok(Value::Float(*x)),
            LuaValue::String(s) => Ok(Value::Str(s.to_string_lossy())),
            LuaValue::Table(t) => list_value(t, depth),
            other => Err(format!(
                "is a {}, which a field cannot hold",
                other.type_name()
            )),
        },
    }
}

fn list_value(table: &Table, depth: usize) -> Result<Value, String> {
    if depth >= MAX_DEPTH {
        return Err(format!("nests lists more than {MAX_DEPTH} deep"));
    }
    table
        .sequence_values::<LuaValue>()
        .map(|item| to_value(&item.map_err(lua_err)?, None, depth + 1))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::List)
}

/// A frame's fields: the kind's declared fields first, in declared order and converted
/// to their declared types, then any others sorted by name with their types inferred.
fn fields_from_lua(
    table: &Table,
    declared: Option<&[(SmolStr, FieldType, bool)]>,
) -> Result<Vec<(SmolStr, Value)>, String> {
    let mut given: Vec<(LuaString, LuaValue)> = Vec::new();
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair.map_err(lua_err)?;
        let LuaValue::String(key) = key else {
            return Err(format!(
                "field names must be strings, not {}",
                key.type_name()
            ));
        };
        given.push((key, value));
    }
    let mut out = Vec::with_capacity(given.len());
    let mut used = vec![false; given.len()];
    for (name, ty, optional) in declared.unwrap_or_default() {
        match given
            .iter()
            .position(|(key, _)| *key.as_bytes() == *name.as_bytes())
        {
            Some(i) => {
                used[i] = true;
                let value = to_value(&given[i].1, Some(*ty), 0)
                    .map_err(|err| format!("field `{name}` {err}"))?;
                out.push((name.clone(), value));
            }
            None if *optional => {}
            None => return Err(format!("field `{name}` is declared but missing")),
        }
    }
    let mut extra: Vec<(SmolStr, &LuaValue)> = given
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((key, value), _)| (SmolStr::new(key.to_string_lossy()), value))
        .collect();
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in extra {
        let value = to_value(value, None, 0).map_err(|err| format!("field `{name}` {err}"))?;
        out.push((name, value));
    }
    Ok(out)
}

/// Where a Lua frame sits in the input: `pos` (1-based) and `len` checked against it.
pub(super) fn frame_span(table: &Table, input_len: usize) -> Result<(usize, usize), String> {
    let pos = integer(&table.raw_get::<LuaValue>("pos").map_err(lua_err)?);
    let len = integer(&table.raw_get::<LuaValue>("len").map_err(lua_err)?);
    let (Some(pos), Some(len)) = (pos, len) else {
        return Err("a frame needs integer `pos` and `len`".to_owned());
    };
    let start = pos - 1;
    if start < 0 || len < 0 || start + len > input_len as i64 {
        return Err(format!(
            "pos {pos} and len {len} fall outside the {input_len} bytes given to decode"
        ));
    }
    Ok((start as usize, len as usize))
}

/// A Lua frame table as a [`Frame`] covering `raw`.
pub(super) fn frame_from_lua(
    table: &Table,
    schema: &Schema,
    raw: std::ops::Range<u64>,
    at: Instant,
) -> Result<Frame, String> {
    let kind = string_of(
        &table.raw_get::<LuaValue>("kind").map_err(lua_err)?,
        "a frame's kind",
    )?;
    let severity = match table.raw_get::<LuaValue>("severity").map_err(lua_err)? {
        LuaValue::Nil => Severity::Info,
        value => {
            let name = string_of(&value, "a frame's severity")?;
            Severity::from_name(&name)
                .ok_or_else(|| format!("severity {name:?} is not info, warning or error"))?
        }
    };
    let summary = match table.raw_get::<LuaValue>("summary").map_err(lua_err)? {
        LuaValue::Nil => String::new(),
        value => string_of(&value, "a frame's summary")?,
    };
    let declared = schema.get(&kind).map(Vec::as_slice);
    let fields = match table.raw_get::<LuaValue>("fields").map_err(lua_err)? {
        LuaValue::Nil => {
            if let Some(missing) = declared.and_then(|d| d.iter().find(|f| !f.2)) {
                return Err(format!("field `{}` is declared but missing", missing.0));
            }
            Vec::new()
        }
        LuaValue::Table(fields) => fields_from_lua(&fields, declared)?,
        other => {
            return Err(format!(
                "a frame's fields must be a table, not {}",
                other.type_name()
            ));
        }
    };
    let mut frame = Frame::new(kind.as_str(), raw, at)
        .with_severity(severity)
        .with_summary(summary);
    frame.fields = fields;
    Ok(frame)
}

/// A JSON value for a plugin: `null` as `codec.null`, arrays as sequences marked for
/// `codec.is_array`, objects as tables.
pub(super) fn json_to_lua(
    lua: &Lua,
    value: &JsonValue,
    array_mt: &Table,
    depth: usize,
) -> Result<LuaValue, String> {
    if depth > MAX_DEPTH {
        return Err(format!("the request nests more than {MAX_DEPTH} deep"));
    }
    Ok(match value {
        JsonValue::Null => LuaValue::NULL,
        JsonValue::Bool(b) => LuaValue::Boolean(*b),
        JsonValue::Number(n) => match n.as_i64() {
            Some(i) => LuaValue::Integer(i),
            None => LuaValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        },
        JsonValue::String(s) => LuaValue::String(lua.create_string(s).map_err(lua_err)?),
        JsonValue::Array(items) => {
            let table = lua
                .create_table_with_capacity(items.len(), 0)
                .map_err(lua_err)?;
            for item in items {
                table
                    .raw_push(json_to_lua(lua, item, array_mt, depth + 1)?)
                    .map_err(lua_err)?;
            }
            table
                .set_metatable(Some(array_mt.clone()))
                .map_err(lua_err)?;
            LuaValue::Table(table)
        }
        JsonValue::Object(map) => LuaValue::Table(object_to_lua(lua, map, array_mt, depth + 1)?),
    })
}

pub(super) fn object_to_lua(
    lua: &Lua,
    map: &serde_json::Map<String, JsonValue>,
    array_mt: &Table,
    depth: usize,
) -> Result<Table, String> {
    let table = lua
        .create_table_with_capacity(0, map.len())
        .map_err(lua_err)?;
    for (key, value) in map {
        table
            .raw_set(key.as_str(), json_to_lua(lua, value, array_mt, depth)?)
            .map_err(lua_err)?;
    }
    Ok(table)
}

/// What `encode` returned: bytes (a string or a byte table), or `nil, err`.
pub(super) fn encode_result(first: LuaValue, second: LuaValue) -> Result<Vec<u8>, CodecError> {
    match first {
        LuaValue::String(_) | LuaValue::Table(_) => {
            bytes_arg(&first, "encode's result").map_err(CodecError::Internal)
        }
        LuaValue::Nil => Err(encode_error(second)),
        other => Err(CodecError::Internal(format!(
            "encode must return bytes or nil and an error, not {}",
            other.type_name()
        ))),
    }
}

/// An error value from `encode`: a `codec.*` error table, or a message.
fn encode_error(value: LuaValue) -> CodecError {
    let table = match value {
        LuaValue::Table(table) => table,
        LuaValue::String(s) => return CodecError::Internal(s.to_string_lossy()),
        LuaValue::Nil => {
            return CodecError::Internal("encode failed without saying why".to_owned());
        }
        other => {
            return CodecError::Internal(format!(
                "encode failed with a {} instead of an error",
                other.type_name()
            ));
        }
    };
    let text = |key: &str| match table.raw_get::<LuaValue>(key) {
        Ok(LuaValue::String(s)) => s.to_string_lossy(),
        _ => String::new(),
    };
    match text("code").as_str() {
        "unknown_command" => CodecError::UnknownCommand(text("command")),
        "missing_field" => CodecError::MissingField(text("field")),
        "bad_field" => CodecError::BadField {
            field: text("field"),
            reason: text("reason"),
        },
        code => CodecError::Internal(format!("encode failed with an error table (code {code:?})")),
    }
}
