//! `codecs.<name>.encode(fields)`: the bytes of a command, built by an installed codec
//! plugin from the same fields a saved command's `{ "codec": …, "fields": { … } }`
//! payload holds.
//!
//! `codecs` is an empty table whose `__index` looks the name up in
//! [`HostServices::codecs`](crate::HostServices::codecs) on every access, so a plugin
//! installed or reloaded while the script runs is seen at its next lookup, and a name
//! that is not installed reads as `nil` (`if codecs["airoha-race"] then ... end`).
//!
//! A codec is not `Send`, so each run makes the codecs it encodes with on the script
//! thread, one per name, from the factory the source hands out, and keeps it for the
//! run; a reload that hands out a different factory gets a fresh codec. A codec that
//! panics is dropped and the call fails; the next call makes a new one.

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

use mlua::{Lua, MetaMethod, Table, UserData, UserDataFields, UserDataMethods, Value};
use serde_json::{Map, Number, Value as JsonValue};
use serialist_core::{Codec, CodecFactory, EncodeRequest};

use crate::vm::{RunCtx, runtime_error};

/// Deepest nesting of tables a fields table may have.
const MAX_DEPTH: usize = 16;

/// A codec one run made, with the factory it came from.
type Made = (Arc<dyn CodecFactory>, Box<dyn Codec>);

/// The codecs one run has made, by name.
#[derive(Default)]
pub(crate) struct CodecCache {
    codecs: HashMap<String, Made>,
}

/// How a script names a codec in messages: `codecs.race`, or `codecs["airoha-race"]`
/// for a name that is not a Lua identifier.
fn spelled(name: &str) -> String {
    let identifier = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if identifier {
        format!("codecs.{name}")
    } else {
        format!("codecs[{name:?}]")
    }
}

pub(crate) fn codecs_table(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Table> {
    let codecs = lua.create_table()?;
    let meta = lua.create_table()?;
    let ctx = Rc::clone(ctx);
    meta.set(
        MetaMethod::Index.name(),
        lua.create_function(move |lua, (_, key): (Table, Value)| {
            let Value::String(name) = key else {
                return Ok(Value::Nil);
            };
            let Ok(name) = name.to_str() else {
                return Ok(Value::Nil);
            };
            let installed = ctx
                .services
                .codecs
                .as_ref()
                .is_some_and(|source| source.codec(&name).is_some());
            if !installed {
                return Ok(Value::Nil);
            }
            lua.create_userdata(CodecHandle {
                name: name.to_owned(),
                ctx: Rc::clone(&ctx),
            })
            .map(Value::UserData)
        })?,
    )?;
    codecs.set_metatable(Some(meta))?;
    Ok(codecs)
}

/// What `codecs.<name>` reads as: the codec's `name` and its `encode` function.
struct CodecHandle {
    name: String,
    ctx: Rc<RunCtx>,
}

impl UserData for CodecHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("name", |_, this| Ok(this.name.clone()));
        fields.add_field_method_get("encode", |lua, this| {
            let name = this.name.clone();
            let ctx = Rc::clone(&this.ctx);
            lua.create_function(move |lua, (first, second): (Value, Value)| {
                // `codecs.race.encode{...}` as the plan spells it, and the method call
                // `codecs.race:encode{...}` as well.
                let fields = match &first {
                    Value::UserData(data) if data.is::<CodecHandle>() => second,
                    _ => first,
                };
                let what = format!("{}.encode", spelled(&name));
                let bytes = encode(&ctx, &name, &fields)
                    .map_err(|message| runtime_error(format!("{what}: {message}")))?;
                lua.create_string(&bytes)
            })
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("codec {}", this.name))
        });
    }
}

/// The bytes `name` encodes `fields` into, as a saved command's codec payload would be.
fn encode(ctx: &RunCtx, name: &str, fields: &Value) -> Result<Vec<u8>, String> {
    let factory = ctx
        .services
        .codecs
        .as_ref()
        .and_then(|source| source.codec(name))
        .ok_or_else(|| format!("no codec named {name:?} is installed"))?;
    let fields = match fields {
        Value::Nil => Map::new(),
        Value::Table(table) => object(table, 0)?,
        other => {
            return Err(format!(
                "fields must be a table such as {{ cmd_id = 0x0F15 }}, got {}",
                other.type_name()
            ));
        }
    };
    let info = factory.info();
    let default = match info.default_command() {
        Some(command) => command.name.as_str(),
        None if fields.contains_key("command") => "",
        None => return Err(format!("{name} has no commands to encode")),
    };
    let request = EncodeRequest::from_payload(&fields, default).map_err(|err| err.to_string())?;
    let mut cache = ctx.codecs.borrow_mut();
    let fresh = cache
        .codecs
        .get(name)
        .is_none_or(|(made_by, _)| !std::ptr::addr_eq(Arc::as_ptr(made_by), Arc::as_ptr(&factory)));
    if fresh {
        let codec = match catch_unwind(AssertUnwindSafe(|| factory.create())) {
            Ok(Ok(codec)) => codec,
            Ok(Err(err)) => return Err(format!("the codec could not be made: {err}")),
            Err(_) => return Err("the codec panicked while being made".to_owned()),
        };
        cache
            .codecs
            .insert(name.to_owned(), (Arc::clone(&factory), codec));
    }
    let (_, codec) = cache.codecs.get_mut(name).expect("the codec just made");
    match catch_unwind(AssertUnwindSafe(|| codec.encode(&request))) {
        Ok(result) => result.map_err(|err| err.to_string()),
        Err(_) => {
            cache.codecs.remove(name);
            Err("the codec panicked".to_owned())
        }
    }
}

/// A Lua table as a JSON object: string keys only.
pub(crate) fn object(table: &Table, depth: usize) -> Result<Map<String, JsonValue>, String> {
    let mut out = Map::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|err| err.to_string())?;
        let key = match key {
            Value::String(key) => key
                .to_str()
                .map_err(|_| "a field name is not UTF-8 text".to_owned())?
                .to_owned(),
            other => {
                return Err(format!(
                    "field names must be strings, got {}",
                    other.type_name()
                ));
            }
        };
        let value = json(&value, &key, depth + 1)?;
        out.insert(key, value);
    }
    Ok(out)
}

/// A Lua value as JSON, the way a saved command's fields would spell it: numbers as
/// numbers, text as strings, a sequence as a list (an empty table too), a table with
/// string keys as an object.
fn json(value: &Value, field: &str, depth: usize) -> Result<JsonValue, String> {
    if depth > MAX_DEPTH {
        return Err(format!(
            "field `{field}` nests tables more than {MAX_DEPTH} deep"
        ));
    }
    Ok(match value {
        Value::Boolean(b) => JsonValue::Bool(*b),
        Value::Integer(n) => JsonValue::from(*n),
        Value::Number(x) => Number::from_f64(*x)
            .map(JsonValue::Number)
            .ok_or_else(|| format!("field `{field}`: {x} is not a finite number"))?,
        Value::String(s) => JsonValue::String(
            s.to_str()
                .map_err(|_| {
                    format!(
                        "field `{field}` is not UTF-8 text; give bytes as hex text \
                         (\"05 5A\") or a table of byte values"
                    )
                })?
                .to_owned(),
        ),
        Value::Table(table) => {
            let len = table.raw_len();
            let pairs = table.pairs::<Value, Value>().count();
            if pairs == len {
                let mut items = Vec::with_capacity(len);
                for item in table.sequence_values::<Value>() {
                    let item = item.map_err(|err| err.to_string())?;
                    items.push(json(&item, field, depth + 1)?);
                }
                JsonValue::Array(items)
            } else {
                JsonValue::Object(object(table, depth)?)
            }
        }
        other => {
            return Err(format!("field `{field}` cannot be a {}", other.type_name()));
        }
    })
}
