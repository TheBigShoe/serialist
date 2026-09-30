//! Values across the component boundary: descriptions and frames coming out, encode
//! requests going in. Nothing a plugin returns is trusted: spans are checked against
//! the input, list items against the arena, fields against the declared kinds.

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{Map, Value as JsonValue};
use serialist_core::codec::{
    CodecError, CodecInfo, CommandInfo, FieldInfo, FieldType, Frame, FrameKindInfo, Severity,
    SmolStr, Value,
};

use super::wit;

/// Deepest nesting of lists (frames) or arrays and objects (requests) converted, as for
/// Lua plugins.
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

fn field_type(ty: wit::FieldType) -> FieldType {
    match ty {
        wit::FieldType::Bool => FieldType::Bool,
        wit::FieldType::Int => FieldType::Int,
        wit::FieldType::Uint => FieldType::UInt,
        wit::FieldType::Float => FieldType::Float,
        wit::FieldType::Str => FieldType::Str,
        wit::FieldType::Bytes => FieldType::Bytes,
        wit::FieldType::List => FieldType::List,
    }
}

fn fields_info(fields: Vec<wit::FieldInfo>) -> Vec<FieldInfo> {
    fields
        .into_iter()
        .map(|f| FieldInfo {
            name: f.name,
            ty: field_type(f.ty),
            description: f.description,
            optional: f.optional,
        })
        .collect()
}

/// What `describe()` returned, as a [`CodecInfo`].
pub(super) fn info_from_wit(info: wit::CodecInfo) -> Result<CodecInfo, String> {
    if info.name.trim().is_empty() {
        return Err("describe().name is empty".to_owned());
    }
    Ok(CodecInfo {
        name: info.name,
        version: info.version,
        description: info.description,
        kinds: info
            .kinds
            .into_iter()
            .map(|k| FrameKindInfo {
                kind: k.kind,
                description: k.description,
                fields: fields_info(k.fields),
            })
            .collect(),
        commands: info
            .commands
            .into_iter()
            .map(|c| CommandInfo {
                name: c.name,
                description: c.description,
                fields: fields_info(c.fields),
            })
            .collect(),
    })
}

/// Where a frame sits in the input: `offset .. offset + len`, checked against its length.
pub(super) fn frame_span(frame: &wit::Frame, input_len: usize) -> Result<(u64, u64), String> {
    let start = u64::from(frame.offset);
    let end = start + u64::from(frame.len);
    if end > input_len as u64 {
        return Err(format!(
            "offset {} and len {} fall outside the {input_len} bytes given to decode",
            frame.offset, frame.len
        ));
    }
    Ok((start, end))
}

/// The list items of one frame, each usable once.
struct Items<'a> {
    items: &'a [wit::Value],
    used: Vec<bool>,
}

impl Items<'_> {
    fn value(&mut self, value: &wit::Value, depth: usize) -> Result<Value, String> {
        Ok(match value {
            wit::Value::Bool(b) => Value::Bool(*b),
            wit::Value::Int(n) => Value::Int(*n),
            wit::Value::Uint(n) => Value::UInt(*n),
            wit::Value::Float(x) => Value::Float(*x),
            wit::Value::Str(s) => Value::Str(s.clone()),
            wit::Value::Bytes(b) => Value::Bytes(b.clone()),
            wit::Value::List(span) => {
                if depth >= MAX_DEPTH {
                    return Err(format!("nests lists more than {MAX_DEPTH} deep"));
                }
                let first = span.first as usize;
                let end = first + span.len as usize;
                if end > self.items.len() {
                    return Err(format!(
                        "lists items {first}..{end} of the frame's {}",
                        self.items.len()
                    ));
                }
                let mut list = Vec::with_capacity(end - first);
                for i in first..end {
                    if std::mem::replace(&mut self.used[i], true) {
                        return Err(format!("uses list item {i} twice"));
                    }
                    let items = self.items;
                    let item = &items[i];
                    list.push(self.value(item, depth + 1)?);
                }
                Value::List(list)
            }
        })
    }
}

/// `value` as a field declared `ty`: its own type, or an integer that fits the other
/// integer type.
fn declared(value: Value, ty: FieldType) -> Result<Value, String> {
    let value = match (value, ty) {
        (Value::Int(n), FieldType::UInt) if n >= 0 => Value::UInt(n as u64),
        (Value::UInt(n), FieldType::Int) if i64::try_from(n).is_ok() => Value::Int(n as i64),
        (value, _) => value,
    };
    if value.ty() == ty {
        Ok(value)
    } else {
        Err(format!("is of type {} but is declared {ty}", value.ty()))
    }
}

/// A frame from the plugin, covering stream offsets `raw`. Its fields come out as the
/// kind declares them (in order, of their types, the required ones present), followed by
/// any others in the plugin's order.
pub(super) fn frame_from_wit(
    frame: &wit::Frame,
    schema: &Schema,
    raw: std::ops::Range<u64>,
    at: Instant,
) -> Result<Frame, String> {
    let mut items = Items {
        items: &frame.items,
        used: vec![false; frame.items.len()],
    };
    let mut given = Vec::with_capacity(frame.fields.len());
    for (name, value) in &frame.fields {
        let value = items
            .value(value, 0)
            .map_err(|err| format!("field `{name}` {err}"))?;
        given.push(Some((name.as_str(), value)));
    }
    let mut fields = Vec::with_capacity(given.len());
    if let Some(declared_fields) = schema.get(&frame.kind) {
        for (name, ty, optional) in declared_fields {
            let found = given
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|(n, _)| *n == name.as_str()));
            match found.and_then(Option::take) {
                Some((_, value)) => {
                    let value =
                        declared(value, *ty).map_err(|err| format!("field `{name}` {err}"))?;
                    fields.push((name.clone(), value));
                }
                None if *optional => {}
                None => return Err(format!("field `{name}` is declared but missing")),
            }
        }
    }
    fields.extend(
        given
            .into_iter()
            .flatten()
            .map(|(name, value)| (SmolStr::new(name), value)),
    );
    let severity = match frame.severity {
        wit::Severity::Info => Severity::Info,
        wit::Severity::Warning => Severity::Warning,
        wit::Severity::Error => Severity::Error,
    };
    let mut out = Frame::new(frame.kind.as_str(), raw, at)
        .with_severity(severity)
        .with_summary(frame.summary.clone());
    out.fields = fields;
    Ok(out)
}

/// An encode request in the WIT shape: arrays' items and objects' members go to `nodes`.
pub(super) fn request_to_wit(
    command: &str,
    fields: &Map<String, JsonValue>,
) -> Result<wit::EncodeRequest, String> {
    let mut nodes = Vec::new();
    let fields = fields
        .iter()
        .map(|(key, value)| Ok((key.clone(), json_to_wit(value, &mut nodes, 1)?)))
        .collect::<Result<Vec<_>, String>>()?;
    Ok(wit::EncodeRequest {
        command: command.to_owned(),
        fields,
        nodes,
    })
}

fn json_to_wit(
    value: &JsonValue,
    nodes: &mut Vec<(String, wit::Json)>,
    depth: usize,
) -> Result<wit::Json, String> {
    if depth > MAX_DEPTH {
        return Err(format!("the request nests more than {MAX_DEPTH} deep"));
    }
    Ok(match value {
        JsonValue::Null => wit::Json::Null,
        JsonValue::Bool(b) => wit::Json::Bool(*b),
        JsonValue::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => wit::Json::Int(i),
            (None, Some(u)) => wit::Json::Uint(u),
            (None, None) => wit::Json::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        JsonValue::String(s) => wit::Json::Str(s.clone()),
        JsonValue::Array(items) => {
            let span = reserve(nodes, items.len())?;
            for (i, item) in items.iter().enumerate() {
                let item = json_to_wit(item, nodes, depth + 1)?;
                nodes[span.first as usize + i].1 = item;
            }
            wit::Json::Array(span)
        }
        JsonValue::Object(members) => {
            let span = reserve(nodes, members.len())?;
            for (i, (key, member)) in members.iter().enumerate() {
                let member = json_to_wit(member, nodes, depth + 1)?;
                nodes[span.first as usize + i] = (key.clone(), member);
            }
            wit::Json::Object(span)
        }
    })
}

/// The next `len` slots of `nodes`, for one array's items or one object's members.
fn reserve(nodes: &mut Vec<(String, wit::Json)>, len: usize) -> Result<wit::Span, String> {
    let first = nodes.len();
    let too_big = || "the request is too large".to_owned();
    let span = wit::Span {
        first: u32::try_from(first).map_err(|_| too_big())?,
        len: u32::try_from(len).map_err(|_| too_big())?,
    };
    nodes.resize(first + len, (String::new(), wit::Json::Null));
    Ok(span)
}

/// An error from `encode`.
pub(super) fn error_from_wit(error: wit::CodecError) -> CodecError {
    match error {
        wit::CodecError::UnknownCommand(name) => CodecError::UnknownCommand(name),
        wit::CodecError::MissingField(field) => CodecError::MissingField(field),
        wit::CodecError::BadField(bad) => CodecError::BadField {
            field: bad.field,
            reason: bad.reason,
        },
        wit::CodecError::Internal(message) => CodecError::Internal(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wit_frame(fields: Vec<(&str, wit::Value)>, items: Vec<wit::Value>) -> wit::Frame {
        wit::Frame {
            kind: "k".into(),
            offset: 1,
            len: 2,
            severity: wit::Severity::Warning,
            summary: "s".into(),
            fields: fields.into_iter().map(|(n, v)| (n.into(), v)).collect(),
            items,
        }
    }

    fn kind_schema() -> Schema {
        let info = CodecInfo {
            name: "x".into(),
            kinds: vec![
                FrameKindInfo::new("k", "")
                    .field(FieldInfo::new("a", FieldType::UInt, ""))
                    .field(FieldInfo::new("b", FieldType::List, "").optional()),
            ],
            ..CodecInfo::default()
        };
        schema(&info)
    }

    #[test]
    fn frames_come_out_in_declared_order_with_lists_rebuilt() {
        let span = |first, len| wit::Span { first, len };
        let frame = wit_frame(
            vec![
                ("extra", wit::Value::Str("e".into())),
                ("b", wit::Value::List(span(0, 2))),
                ("a", wit::Value::Int(7)),
            ],
            vec![
                wit::Value::List(span(2, 1)),
                wit::Value::Bool(true),
                wit::Value::Uint(1),
            ],
        );
        let at = Instant::now();
        let out = frame_from_wit(&frame, &kind_schema(), 10..12, at).unwrap();
        assert_eq!(out.raw, 10..12);
        assert_eq!(out.severity, Severity::Warning);
        assert_eq!(
            out.fields,
            [
                ("a".into(), Value::UInt(7)),
                (
                    "b".into(),
                    Value::List(vec![Value::List(vec![Value::UInt(1)]), Value::Bool(true)])
                ),
                ("extra".into(), Value::Str("e".into())),
            ]
        );
    }

    #[test]
    fn frames_that_break_the_contract_say_how() {
        let span = |first, len| wit::Span { first, len };
        let at = Instant::now();
        let cases = [
            (
                wit_frame(vec![], vec![]),
                "field `a` is declared but missing",
            ),
            (
                wit_frame(vec![("a", wit::Value::Int(-1))], vec![]),
                "field `a` is of type int but is declared uint",
            ),
            (
                wit_frame(vec![("a", wit::Value::List(span(0, 1)))], vec![]),
                "lists items 0..1",
            ),
            (
                wit_frame(
                    vec![
                        ("a", wit::Value::Uint(1)),
                        ("b", wit::Value::List(span(0, 1))),
                    ],
                    vec![wit::Value::List(span(0, 1))],
                ),
                "uses list item 0 twice",
            ),
        ];
        for (frame, hint) in cases {
            let err = frame_from_wit(&frame, &kind_schema(), 0..1, at).unwrap_err();
            assert!(err.contains(hint), "{err}");
        }
        let deep = wit_frame(
            vec![
                ("a", wit::Value::Uint(1)),
                ("b", wit::Value::List(span(0, 1))),
            ],
            (1..=40).map(|i| wit::Value::List(span(i, 1))).collect(),
        );
        let err = frame_from_wit(&deep, &kind_schema(), 0..1, at).unwrap_err();
        assert!(err.contains("deep") || err.contains("lists items"), "{err}");
        assert!(frame_span(&wit_frame(vec![], vec![]), 2).is_err());
        assert_eq!(frame_span(&wit_frame(vec![], vec![]), 3), Ok((1, 3)));
    }

    #[test]
    fn requests_flatten_into_nodes() {
        let JsonValue::Object(fields) = json!({
            "a": [1, [true], { "k": null }], "b": -1, "c": u64::MAX, "d": 1.5, "e": "s"
        }) else {
            unreachable!()
        };
        let request = request_to_wit("cmd", &fields).unwrap();
        let span = |first, len| wit::Span { first, len };
        assert_eq!(request.command, "cmd");
        assert_eq!(
            request.fields[0],
            ("a".into(), wit::Json::Array(span(0, 3)))
        );
        assert_eq!(request.fields[1], ("b".into(), wit::Json::Int(-1)));
        assert_eq!(request.fields[2], ("c".into(), wit::Json::Uint(u64::MAX)));
        assert_eq!(request.fields[3], ("d".into(), wit::Json::Float(1.5)));
        assert_eq!(
            request.nodes,
            [
                (String::new(), wit::Json::Int(1)),
                (String::new(), wit::Json::Array(span(3, 1))),
                (String::new(), wit::Json::Object(span(4, 1))),
                (String::new(), wit::Json::Bool(true)),
                ("k".into(), wit::Json::Null),
            ]
        );
        let mut deep = json!(1);
        for _ in 0..40 {
            deep = json!([deep]);
        }
        let JsonValue::Object(fields) = json!({ "deep": deep }) else {
            unreachable!()
        };
        assert!(request_to_wit("cmd", &fields).unwrap_err().contains("deep"));
    }
}
