//! Frames and field values, and their flattening into the WIT shapes.

use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

use crate::bindings::{self, Severity, Span};

/// A field's value: the same seven types as Serialist's own `Value`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(n: i64) -> Self {
        Value::Int(n)
    }
}

impl From<i32> for Value {
    fn from(n: i32) -> Self {
        Value::Int(n.into())
    }
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Value::UInt(n)
    }
}

impl From<u32> for Value {
    fn from(n: u32) -> Self {
        Value::UInt(n.into())
    }
}

impl From<u16> for Value {
    fn from(n: u16) -> Self {
        Value::UInt(n.into())
    }
}

/// A byte as a number. (A byte string is `Vec<u8>` or `&[u8]`.)
impl From<u8> for Value {
    fn from(n: u8) -> Self {
        Value::UInt(n.into())
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Value::UInt(n as u64)
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Self {
        Value::Float(x)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.into())
    }
}

impl From<Vec<u8>> for Value {
    fn from(b: Vec<u8>) -> Self {
        Value::Bytes(b)
    }
}

impl From<&[u8]> for Value {
    fn from(b: &[u8]) -> Self {
        Value::Bytes(b.to_vec())
    }
}

impl From<Vec<Value>> for Value {
    fn from(items: Vec<Value>) -> Self {
        Value::List(items)
    }
}

/// One decoded frame: where it sits in the input `decode` was given, its kind, fields,
/// severity and summary.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub kind: String,
    /// Offset of the frame's first byte in `decode`'s input.
    pub offset: usize,
    /// Bytes the frame covers.
    pub len: usize,
    pub severity: Severity,
    pub summary: String,
    /// Named values, in the order the kind declares them.
    pub fields: Vec<(String, Value)>,
}

impl Frame {
    /// An `info` frame of `kind` covering `len` bytes from `offset`, with no fields and
    /// no summary.
    pub fn new(kind: impl Into<String>, offset: usize, len: usize) -> Self {
        Self {
            kind: kind.into(),
            offset,
            len,
            severity: Severity::Info,
            summary: String::new(),
            fields: Vec::new(),
        }
    }

    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn with_field(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.push_field(name, value);
        self
    }

    pub fn push_field(&mut self, name: impl Into<String>, value: impl Into<Value>) {
        self.fields.push((name.into(), value.into()));
    }

    /// The first field called `name`.
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value)
    }

    /// The bytes the frame covers, as a range of `decode`'s input.
    pub fn range(&self) -> Range<usize> {
        self.offset..self.offset + self.len
    }

    /// The WIT shape: lists flattened into the frame's `items`.
    pub(crate) fn into_wit(self) -> bindings::Frame {
        let mut items = Vec::new();
        let fields = self
            .fields
            .into_iter()
            .map(|(name, value)| (name, lower(value, &mut items)))
            .collect();
        bindings::Frame {
            kind: self.kind,
            offset: to_u32(self.offset),
            len: to_u32(self.len),
            severity: self.severity,
            summary: self.summary,
            fields,
            items,
        }
    }
}

/// A count or offset for the WIT's `u32`s. Inputs never reach 4 GiB on wasm32; a value
/// that does not fit becomes `u32::MAX`, which the host rejects as out of range.
pub(crate) fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A value in the WIT shape. A list's items take the next `len` slots of `items`,
/// followed by the items of any lists among them, so every item belongs to one list.
fn lower(value: Value, items: &mut Vec<bindings::Value>) -> bindings::Value {
    match value {
        Value::Bool(b) => bindings::Value::Bool(b),
        Value::Int(n) => bindings::Value::Int(n),
        Value::UInt(n) => bindings::Value::Uint(n),
        Value::Float(x) => bindings::Value::Float(x),
        Value::Str(s) => bindings::Value::Str(s),
        Value::Bytes(b) => bindings::Value::Bytes(b),
        Value::List(list) => {
            let first = items.len();
            let len = list.len();
            items.resize(first + len, bindings::Value::Bool(false));
            for (i, item) in list.into_iter().enumerate() {
                let lowered = lower(item, items);
                items[first + i] = lowered;
            }
            bindings::Value::List(Span {
                first: to_u32(first),
                len: to_u32(len),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn nested_lists_flatten_into_items_one_list_each() {
        let frame = Frame::new("k", 2, 3)
            .with_severity(Severity::Warning)
            .with_summary("s")
            .with_field("n", 7u16)
            .with_field(
                "l",
                vec![
                    Value::from(1i32),
                    vec![Value::from("a")].into(),
                    true.into(),
                ],
            )
            .with_field("b", &b"\x01"[..]);
        assert_eq!(frame.range(), 2..5);
        assert_eq!(frame.field("n"), Some(&Value::UInt(7)));
        let wit = frame.into_wit();
        assert_eq!((wit.offset, wit.len), (2, 3));
        assert_eq!(wit.fields[0], ("n".into(), bindings::Value::Uint(7)));
        assert_eq!(
            wit.fields[1],
            ("l".into(), bindings::Value::List(Span { first: 0, len: 3 }))
        );
        assert_eq!(wit.fields[2], ("b".into(), bindings::Value::Bytes(vec![1])));
        assert_eq!(
            wit.items,
            [
                bindings::Value::Int(1),
                bindings::Value::List(Span { first: 3, len: 1 }),
                bindings::Value::Bool(true),
                bindings::Value::Str("a".into()),
            ]
        );
    }
}
