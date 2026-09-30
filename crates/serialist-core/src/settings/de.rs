//! Small serde helpers that read one scalar and validate it inside the visitor.
//!
//! The validation has to happen inside the visitor rather than after `T::deserialize`
//! returns: `jsonc-parser` stamps a line and column on an error as it leaves the token
//! that produced it, so an error raised later would point at the enclosing object.

use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, Deserializer, Visitor};

/// A scalar as the deserializer hands it over.
#[derive(Clone, Copy, Debug)]
pub(super) enum Raw<'a> {
    Bool(bool),
    Int(i64),
    Uint(u64),
    Float(f64),
    Str(&'a str),
}

impl Raw<'_> {
    /// The value as a float when it is a number.
    pub(super) fn number(self) -> Option<f64> {
        match self {
            Raw::Int(v) => Some(v as f64),
            Raw::Uint(v) => Some(v as f64),
            Raw::Float(v) => Some(v),
            _ => None,
        }
    }

    /// The value as a non-negative integer when it is one.
    pub(super) fn unsigned(self) -> Option<u64> {
        match self {
            Raw::Uint(v) => Some(v),
            Raw::Int(v) => u64::try_from(v).ok(),
            Raw::Float(v) if v >= 0.0 && v.fract() == 0.0 && v <= u64::MAX as f64 => Some(v as u64),
            _ => None,
        }
    }
}

/// Converts a raw scalar or says what was wrong with it.
pub(super) type Convert<T> = fn(Raw<'_>) -> Result<T, String>;

struct Scalar<T> {
    expecting: &'static str,
    convert: Convert<T>,
}

impl<'de, T> Visitor<'de> for Scalar<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.expecting)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<T, E> {
        (self.convert)(Raw::Bool(v)).map_err(E::custom)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<T, E> {
        (self.convert)(Raw::Int(v)).map_err(E::custom)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<T, E> {
        (self.convert)(Raw::Uint(v)).map_err(E::custom)
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<T, E> {
        (self.convert)(Raw::Float(v)).map_err(E::custom)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<T, E> {
        (self.convert)(Raw::Str(v)).map_err(E::custom)
    }
}

/// Reads one scalar with `convert`. `null`, arrays and objects are errors that name
/// `expecting`.
pub(super) fn scalar<'de, D, T>(
    d: D,
    expecting: &'static str,
    convert: Convert<T>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_any(Scalar { expecting, convert })
}

struct OptionalScalar<T> {
    expecting: &'static str,
    convert: Convert<T>,
    marker: PhantomData<T>,
}

impl<'de, T> Visitor<'de> for OptionalScalar<T> {
    type Value = Option<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} or null", self.expecting)
    }

    fn visit_none<E: de::Error>(self) -> Result<Option<T>, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Option<T>, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Option<T>, D::Error> {
        scalar(d, self.expecting, self.convert).map(Some)
    }
}

/// Like [`scalar`] but `null` reads as `None`.
pub(super) fn optional_scalar<'de, D, T>(
    d: D,
    expecting: &'static str,
    convert: Convert<T>,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
{
    d.deserialize_option(OptionalScalar {
        expecting,
        convert,
        marker: PhantomData,
    })
}

fn positive_float(raw: Raw<'_>, what: &str) -> Result<f32, String> {
    match raw.number() {
        Some(v) if v.is_finite() && v > 0.0 => Ok(v as f32),
        Some(v) => Err(format!("a {what} must be a positive number, got {v}")),
        None => Err(format!(
            "invalid value, expected a {what} as a positive number"
        )),
    }
}

fn weight(raw: Raw<'_>) -> Result<f32, String> {
    match raw.number() {
        Some(v) if (100.0..=900.0).contains(&v) => Ok(v as f32),
        Some(v) => Err(format!("a font weight must be from 100 to 900, got {v}")),
        None => Err("invalid value, expected a font weight from 100 to 900".to_string()),
    }
}

fn size(raw: Raw<'_>) -> Result<f32, String> {
    positive_float(raw, "font size")
}

fn baud(raw: Raw<'_>) -> Result<u32, String> {
    match raw.unsigned() {
        Some(v) if v > 0 && v <= u64::from(u32::MAX) => Ok(v as u32),
        Some(v) => Err(format!(
            "a baud rate must be from 1 to {}, got {v}",
            u32::MAX
        )),
        None => Err("invalid value, expected a baud rate as a positive integer".to_string()),
    }
}

fn hex_bytes_per_row(raw: Raw<'_>) -> Result<usize, String> {
    match raw.unsigned() {
        Some(v) if (1..=256).contains(&v) => Ok(v as usize),
        Some(v) => Err(format!("hex_bytes_per_row must be from 1 to 256, got {v}")),
        None => Err("invalid value, expected hex_bytes_per_row as an integer".to_string()),
    }
}

const WEIGHT: &str = "a font weight from 100 to 900";
const SIZE: &str = "a positive font size";
const BAUD: &str = "a baud rate";
const ROW: &str = "a byte count from 1 to 256";

pub(super) fn font_weight<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    scalar(d, WEIGHT, weight)
}

pub(super) fn opt_font_weight<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    optional_scalar(d, WEIGHT, weight)
}

pub(super) fn font_size<'de, D: Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    scalar(d, SIZE, size)
}

pub(super) fn opt_font_size<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    optional_scalar(d, SIZE, size)
}

pub(super) fn baud_rate<'de, D: Deserializer<'de>>(d: D) -> Result<u32, D::Error> {
    scalar(d, BAUD, baud)
}

pub(super) fn opt_baud_rate<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    optional_scalar(d, BAUD, baud)
}

pub(super) fn row_bytes<'de, D: Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    scalar(d, ROW, hex_bytes_per_row)
}

/// A line height multiplier: any positive finite number.
pub(super) fn line_height_number(raw: Raw<'_>) -> Result<f32, String> {
    positive_float(raw, "line height")
}
