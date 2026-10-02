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

/// What a frame, a field name or a field value costs besides its text; about what the
/// host spends on one.
const UNIT: usize = 32;

/// What the frames of one `decode` call have left to take, of
/// [`LuaLimits::max_frame_bytes`](super::LuaLimits::max_frame_bytes).
///
/// The VM's memory limit bounds what a plugin builds, not what it returns: a table or a
/// string can be shared by many fields and frames, and every use is copied into a frame.
/// A few lines of Lua make a list of 13 lists of 13 lists (and so on) that holds one
/// number and converts to hundreds of millions of values. The budget is spent as the
/// frames are converted, so such a call fails with an error frame instead of exhausting
/// the app's memory or its ingest thread's time.
///
/// It is spent frame after frame, whether or not a frame turns out well:
///
/// - a frame costs [`UNIT`], then the length of its `kind`, `severity` and `summary`
///   text;
/// - each pair in its `fields` costs `UNIT` and the length of the key, whether or not
///   it is a usable field;
/// - each value costs `UNIT` (a list's items are values too), and the length of a string
///   or bytes value.
///
/// Text is charged before it is copied. The order is fixed (declared fields, then the
/// others by name), so what a call spends does not depend on how Lua orders a table.
pub(super) struct Budget {
    left: usize,
    limit: usize,
}

impl Budget {
    pub(super) fn new(limit: usize) -> Self {
        Self { left: limit, limit }
    }

    /// Take `bytes`, or take everything that is left and say no.
    fn spend(&mut self, bytes: usize) -> Result<(), String> {
        match self.left.checked_sub(bytes) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => {
                self.left = 0;
                Err(format!(
                    "the frames of one decode call hold more than {} bytes",
                    self.limit
                ))
            }
        }
    }

    /// A Lua string as text, charged for before it is copied.
    fn string(&mut self, s: &LuaString) -> Result<String, String> {
        self.spend(s.as_bytes().len())?;
        Ok(s.to_string_lossy())
    }

    /// A value that must be a string, as text.
    fn text(&mut self, value: &LuaValue, what: &str) -> Result<String, String> {
        match value {
            LuaValue::String(s) => self.string(s),
            other => Err(format!(
                "{what} must be a string, not {}",
                other.type_name()
            )),
        }
    }
}

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
fn to_value(
    value: &LuaValue,
    ty: Option<FieldType>,
    depth: usize,
    budget: &mut Budget,
) -> Result<Value, String> {
    budget.spend(UNIT)?;
    let mismatch = |ty: FieldType| {
        Err(format!(
            "is a {} but is declared {}",
            value.type_name(),
            ty.name()
        ))
    };
    match ty {
        Some(FieldType::Bytes) => match value {
            LuaValue::String(s) => {
                budget.spend(s.as_bytes().len())?;
                Ok(Value::Bytes(s.as_bytes().to_vec()))
            }
            _ => mismatch(FieldType::Bytes),
        },
        Some(FieldType::Str) => match value {
            LuaValue::String(s) => budget.string(s).map(Value::Str),
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
            LuaValue::Table(t) => list_value(t, depth, budget),
            _ => mismatch(FieldType::List),
        },
        None => match value {
            LuaValue::Boolean(b) => Ok(Value::Bool(*b)),
            LuaValue::Integer(n) => Ok(Value::Int(*n)),
            LuaValue::Number(x) => Ok(Value::Float(*x)),
            LuaValue::String(s) => budget.string(s).map(Value::Str),
            LuaValue::Table(t) => list_value(t, depth, budget),
            other => Err(format!(
                "is a {}, which a field cannot hold",
                other.type_name()
            )),
        },
    }
}

fn list_value(table: &Table, depth: usize, budget: &mut Budget) -> Result<Value, String> {
    if depth >= MAX_DEPTH {
        return Err(format!("nests lists more than {MAX_DEPTH} deep"));
    }
    table
        .sequence_values::<LuaValue>()
        .map(|item| to_value(&item.map_err(lua_err)?, None, depth + 1, budget))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::List)
}

/// A frame's fields: the kind's declared fields first, in declared order and converted
/// to their declared types, then any others sorted by name with their types inferred.
fn fields_from_lua(
    table: &Table,
    declared: &[(SmolStr, FieldType, bool)],
    budget: &mut Budget,
) -> Result<Vec<(SmolStr, Value)>, String> {
    let mut given: Vec<(LuaString, LuaValue)> = Vec::new();
    // A key that is not a string fails the frame, but only after every pair is counted:
    // what the call spends must not depend on where Lua puts that key.
    let mut bad_key = None;
    for pair in table.pairs::<LuaValue, LuaValue>() {
        let (key, value) = pair.map_err(lua_err)?;
        match key {
            LuaValue::String(key) => {
                budget.spend(UNIT + key.as_bytes().len())?;
                given.push((key, value));
            }
            other => {
                budget.spend(UNIT)?;
                bad_key.get_or_insert(other.type_name());
            }
        }
    }
    if let Some(ty) = bad_key {
        return Err(format!("field names must be strings, not {ty}"));
    }
    let mut out = Vec::with_capacity(given.len());
    let mut used = vec![false; given.len()];
    for (name, ty, optional) in declared {
        match given
            .iter()
            .position(|(key, _)| *key.as_bytes() == *name.as_bytes())
        {
            Some(i) => {
                used[i] = true;
                let value = to_value(&given[i].1, Some(*ty), 0, budget)
                    .map_err(|err| format!("field `{name}` {err}"))?;
                out.push((name.clone(), value));
            }
            None if *optional => {}
            None => return Err(format!("field `{name}` is declared but missing")),
        }
    }
    // By name; names that differ only in invalid UTF-8 are one name once converted, and
    // then the bytes decide, not the order Lua happens to keep the table in.
    let mut extra: Vec<(SmolStr, &LuaString, &LuaValue)> = given
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((key, value), _)| (SmolStr::new(key.to_string_lossy()), key, value))
        .collect();
    extra.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| (*a.1.as_bytes()).cmp(&*b.1.as_bytes()))
    });
    for (name, _, value) in extra {
        let value =
            to_value(value, None, 0, budget).map_err(|err| format!("field `{name}` {err}"))?;
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
    // A plugin may say any integer: checked sums, so `pos` or `len` near the edges of
    // an i64 is an error, not an overflow (a panic here, a wrapped range in a release).
    let end = pos.checked_sub(1).and_then(|start| start.checked_add(len));
    match end {
        Some(end) if pos >= 1 && len >= 0 && end <= input_len as i64 => {
            Ok(((pos - 1) as usize, len as usize))
        }
        _ => Err(format!(
            "pos {pos} and len {len} fall outside the {input_len} bytes given to decode"
        )),
    }
}

/// A Lua frame table as a [`Frame`] covering `raw`.
pub(super) fn frame_from_lua(
    table: &Table,
    schema: &Schema,
    raw: std::ops::Range<u64>,
    at: Instant,
    budget: &mut Budget,
) -> Result<Frame, String> {
    budget.spend(UNIT)?;
    let kind = budget.text(
        &table.raw_get::<LuaValue>("kind").map_err(lua_err)?,
        "a frame's kind",
    )?;
    let severity = match table.raw_get::<LuaValue>("severity").map_err(lua_err)? {
        LuaValue::Nil => Severity::Info,
        value => {
            let name = budget.text(&value, "a frame's severity")?;
            Severity::from_name(&name)
                .ok_or_else(|| format!("severity {name:?} is not info, warning or error"))?
        }
    };
    let summary = match table.raw_get::<LuaValue>("summary").map_err(lua_err)? {
        LuaValue::Nil => String::new(),
        value => budget.text(&value, "a frame's summary")?,
    };
    let Some(declared) = schema.get(&kind).map(Vec::as_slice) else {
        return Err(format!("kind {kind:?} is not one describe() lists"));
    };
    let fields = match table.raw_get::<LuaValue>("fields").map_err(lua_err)? {
        LuaValue::Nil => {
            if let Some(missing) = declared.iter().find(|f| !f.2) {
                return Err(format!("field `{}` is declared but missing", missing.0));
            }
            Vec::new()
        }
        LuaValue::Table(fields) => fields_from_lua(&fields, declared, budget)?,
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
