//! Encode requests: a command and its JSON fields, read through borrowed views.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::bindings::{self, CodecError, Span};
use crate::hex;

type Nodes = [(String, bindings::Json)];

/// A JSON value of a request, borrowed from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Json<'a> {
    Null,
    Bool(bool),
    /// An integer that fits an `i64`.
    Int(i64),
    /// An integer larger than any `i64`.
    UInt(u64),
    /// Any other number.
    Float(f64),
    Str(&'a str),
    Array(Array<'a>),
    Object(Object<'a>),
}

impl<'a> Json<'a> {
    fn new(value: &'a bindings::Json, nodes: &'a Nodes) -> Self {
        match value {
            bindings::Json::Null => Json::Null,
            bindings::Json::Bool(b) => Json::Bool(*b),
            bindings::Json::Int(n) => Json::Int(*n),
            bindings::Json::Uint(n) => Json::UInt(*n),
            bindings::Json::Float(x) => Json::Float(*x),
            bindings::Json::Str(s) => Json::Str(s),
            bindings::Json::Array(span) => Json::Array(Array(members(nodes, span))),
            bindings::Json::Object(span) => Json::Object(Object(members(nodes, span))),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match *self {
            Json::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// A non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match *self {
            Json::Int(n) => u64::try_from(n).ok(),
            Json::UInt(n) => Some(n),
            _ => None,
        }
    }

    /// An integer that fits an `i64`.
    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Json::Int(n) => Some(n),
            _ => None,
        }
    }

    /// Any number.
    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Json::Int(n) => Some(n as f64),
            Json::UInt(n) => Some(n as f64),
            Json::Float(x) => Some(x),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&'a str> {
        match *self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<Array<'a>> {
        match *self {
            Json::Array(array) => Some(array),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<Object<'a>> {
        match *self {
            Json::Object(object) => Some(object),
            _ => None,
        }
    }
}

/// The entries `span` names, with the nodes they may refer to. A span the host got wrong
/// (it never does) is empty rather than a panic.
fn members<'a>(nodes: &'a Nodes, span: &Span) -> Members<'a> {
    let first = span.first as usize;
    let entries = first
        .checked_add(span.len as usize)
        .and_then(|end| nodes.get(first..end))
        .unwrap_or_default();
    Members { entries, nodes }
}

#[derive(Clone, Copy, Debug)]
struct Members<'a> {
    entries: &'a Nodes,
    nodes: &'a Nodes,
}

impl PartialEq for Members<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self
                .iter()
                .zip(other.iter())
                .all(|(a, b)| a.0 == b.0 && a.1 == b.1)
    }
}

impl<'a> Members<'a> {
    fn iter(&self) -> impl Iterator<Item = (&'a str, Json<'a>)> + 'a {
        let nodes = self.nodes;
        self.entries
            .iter()
            .map(move |(key, value)| (key.as_str(), Json::new(value, nodes)))
    }
}

/// A JSON array of a request.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Array<'a>(Members<'a>);

impl<'a> Array<'a> {
    pub fn len(&self) -> usize {
        self.0.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.entries.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<Json<'a>> {
        let (_, value) = self.0.entries.get(index)?;
        Some(Json::new(value, self.0.nodes))
    }

    pub fn iter(&self) -> impl Iterator<Item = Json<'a>> + 'a {
        self.0.iter().map(|(_, value)| value)
    }
}

/// A JSON object of a request, its members in the order the request gave them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Object<'a>(Members<'a>);

impl<'a> Object<'a> {
    pub fn len(&self) -> usize {
        self.0.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.entries.is_empty()
    }

    /// The first member called `key`.
    pub fn get(&self, key: &str) -> Option<Json<'a>> {
        self.0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, value)| value)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'a str, Json<'a>)> + 'a {
        self.0.iter()
    }
}

/// A command for [`Plugin::encode`](crate::Plugin::encode): its name and JSON fields.
///
/// The helpers read fields with the conventions of Serialist's built-in codecs and
/// report problems with the same errors, so a plugin behaves like them.
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
    raw: &'a bindings::EncodeRequest,
}

impl<'a> Request<'a> {
    pub fn new(raw: &'a bindings::EncodeRequest) -> Self {
        Self { raw }
    }

    /// The command's name, such as `race_version`.
    pub fn command(&self) -> &'a str {
        &self.raw.command
    }

    /// The field called `name`.
    pub fn field(&self, name: &str) -> Option<Json<'a>> {
        self.fields()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value)
    }

    /// Every field, in the order the request gave them.
    pub fn fields(&self) -> impl Iterator<Item = (&'a str, Json<'a>)> + 'a {
        let nodes = &self.raw.nodes[..];
        self.raw
            .fields
            .iter()
            .map(move |(name, value)| (name.as_str(), Json::new(value, nodes)))
    }

    /// Fail with a bad-field error if any field is not in `allowed`. With several, the
    /// error names the alphabetically first, so the result never depends on order.
    pub fn check_fields(&self, allowed: &[&str]) -> Result<(), CodecError> {
        let first = self
            .fields()
            .map(|(name, _)| name)
            .filter(|name| !allowed.contains(name))
            .min();
        match first {
            None => Ok(()),
            Some(name) => Err(CodecError::bad_field(
                name,
                format!("not a field of `{}`", self.command()),
            )),
        }
    }

    /// An unsigned integer field no larger than `max`: a JSON integer, or a string of hex
    /// digits with or without a `0x` prefix (`"0x0F15"`, `"0f15"`). `None` if absent.
    pub fn uint(&self, name: &str, max: u64) -> Result<Option<u64>, CodecError> {
        let Some(value) = self.field(name) else {
            return Ok(None);
        };
        let n = match value {
            Json::Int(_) | Json::UInt(_) | Json::Float(_) => value.as_u64().ok_or_else(|| {
                let shown = match value {
                    Json::Int(n) => format!("{n}"),
                    _ => format!("{}", value.as_f64().unwrap_or_default()),
                };
                CodecError::bad_field(name, format!("{shown} is not a non-negative integer"))
            })?,
            Json::Str(s) => hex::parse_uint(s).ok_or_else(|| {
                CodecError::bad_field(
                    name,
                    format!("{s:?} is not hex digits (with or without 0x)"),
                )
            })?,
            _ => {
                return Err(CodecError::bad_field(
                    name,
                    "must be an integer or a hex string",
                ));
            }
        };
        if n > max {
            return Err(CodecError::bad_field(
                name,
                format!("{n:#X} is larger than {max:#X}"),
            ));
        }
        Ok(Some(n))
    }

    /// A bytes field: hex text as [`hex::decode`] reads it (`"05 5A 00"`), or a list of
    /// integers from 0 to 255. `None` if absent.
    pub fn bytes(&self, name: &str) -> Result<Option<Vec<u8>>, CodecError> {
        match self.field(name) {
            None => Ok(None),
            Some(Json::Str(text)) => hex::decode(text)
                .map(Some)
                .map_err(|err| CodecError::bad_field(name, format!("{err}"))),
            Some(Json::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    item.as_u64()
                        .and_then(|n| u8::try_from(n).ok())
                        .ok_or_else(|| {
                            CodecError::bad_field(
                                name,
                                format!("item {i} is not a byte (an integer from 0 to 255)"),
                            )
                        })
                })
                .collect::<Result<Vec<u8>, _>>()
                .map(Some),
            Some(_) => Err(CodecError::bad_field(
                name,
                "must be hex text or a list of bytes",
            )),
        }
    }

    /// A string field. `None` if absent.
    pub fn str(&self, name: &str) -> Result<Option<&'a str>, CodecError> {
        match self.field(name) {
            None => Ok(None),
            Some(Json::Str(s)) => Ok(Some(s)),
            Some(_) => Err(CodecError::bad_field(name, "must be a string")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn span(first: u32, len: u32) -> Span {
        Span { first, len }
    }

    /// `{ "a": 3861, "b": "0x0F15", "neg": -1, "big": 70000, "hex": "01 02 ff",
    ///    "list": [1, 2, 255], "bad": [256], "obj": { "k": null }, "f": 1.5 }`
    fn sample() -> bindings::EncodeRequest {
        use bindings::Json as J;
        bindings::EncodeRequest {
            command: "cmd".into(),
            fields: vec![
                ("a".into(), J::Int(3861)),
                ("b".into(), J::Str("0x0F15".into())),
                ("neg".into(), J::Int(-1)),
                ("big".into(), J::Int(70000)),
                ("hex".into(), J::Str("01 02 ff".into())),
                ("list".into(), J::Array(span(0, 3))),
                ("bad".into(), J::Array(span(3, 1))),
                ("obj".into(), J::Object(span(4, 1))),
                ("f".into(), J::Float(1.5)),
                ("huge".into(), J::Uint(u64::MAX)),
            ],
            nodes: vec![
                (String::new(), J::Int(1)),
                (String::new(), J::Int(2)),
                (String::new(), J::Int(255)),
                (String::new(), J::Int(256)),
                ("k".into(), J::Null),
            ],
        }
    }

    fn bad_field(result: Result<impl core::fmt::Debug, CodecError>) -> String {
        match result {
            Err(CodecError::BadField(bad)) => bad.field,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn fields_read_with_the_builtin_conventions() {
        let raw = sample();
        let r = Request::new(&raw);
        assert_eq!(r.command(), "cmd");
        assert_eq!(r.uint("a", 0xFFFF), Ok(Some(0x0F15)));
        assert_eq!(r.uint("b", 0xFFFF), Ok(Some(0x0F15)));
        assert_eq!(r.uint("missing", 0xFFFF), Ok(None));
        for name in ["neg", "big", "hex", "list", "f", "huge"] {
            assert_eq!(bad_field(r.uint(name, 0xFFFF)), name);
        }
        assert_eq!(r.bytes("hex"), Ok(Some(vec![1, 2, 255])));
        assert_eq!(r.bytes("list"), Ok(Some(vec![1, 2, 255])));
        assert_eq!(bad_field(r.bytes("bad")), "bad");
        assert_eq!(bad_field(r.bytes("a")), "a");
        assert_eq!(r.str("hex"), Ok(Some("01 02 ff")));
        assert_eq!(bad_field(r.str("a")), "a");
        let obj = r.field("obj").and_then(|v| v.as_object()).unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.get("k").unwrap().is_null());
        let list = r.field("list").and_then(|v| v.as_array()).unwrap();
        assert_eq!(list.get(2).and_then(|v| v.as_u64()), Some(255));
        assert_eq!(list.get(3), None);
        assert_eq!(r.field("huge").and_then(|v| v.as_u64()), Some(u64::MAX));
    }

    #[test]
    fn unknown_fields_are_reported_alphabetically_first() {
        let raw = sample();
        let r = Request::new(&raw);
        assert_eq!(bad_field(r.check_fields(&["a", "b"])), "bad");
        let all = [
            "a", "b", "neg", "big", "hex", "list", "bad", "obj", "f", "huge",
        ];
        assert_eq!(r.check_fields(&all), Ok(()));
    }

    #[test]
    fn a_bad_span_reads_as_empty() {
        let raw = bindings::EncodeRequest {
            command: "c".into(),
            fields: vec![("x".into(), bindings::Json::Array(span(u32::MAX, 2)))],
            nodes: vec![],
        };
        let r = Request::new(&raw);
        assert!(r.field("x").and_then(|v| v.as_array()).unwrap().is_empty());
    }
}
