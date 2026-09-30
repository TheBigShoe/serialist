//! A GPUI-free RGBA color and the hex forms Zed theme files use.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserialize, Deserializer};
use serde::ser::{Serialize, Serializer};

use crate::text::Color;

/// A color with each channel from 0.0 to 1.0. Not premultiplied.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

/// Why a string is not a color.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{input:?} is not a color: {reason}")]
pub struct ColorParseError {
    pub input: String,
    pub reason: &'static str,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba::new(0.0, 0.0, 0.0, 0.0);
    pub const BLACK: Rgba = Rgba::new(0.0, 0.0, 0.0, 1.0);
    pub const WHITE: Rgba = Rgba::new(1.0, 1.0, 1.0, 1.0);

    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// From 8-bit channels.
    pub fn from_u8(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            f32::from(a) / 255.0,
        )
    }

    /// Parses `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`, in either case.
    pub fn parse_hex(input: &str) -> Result<Self, ColorParseError> {
        let fail = |reason| ColorParseError {
            input: input.to_string(),
            reason,
        };
        let digits = input
            .trim()
            .strip_prefix('#')
            .ok_or_else(|| fail("a color starts with #"))?;
        if !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(fail("only hex digits may follow the #"));
        }
        // ASCII was checked above, so byte slicing is on character boundaries.
        let pair = |i: usize| u8::from_str_radix(&digits[i..i + 2], 16).unwrap_or(0);
        let nibble = |i: usize| u8::from_str_radix(&digits[i..=i], 16).unwrap_or(0) * 17;
        match digits.len() {
            3 => Ok(Self::from_u8(nibble(0), nibble(1), nibble(2), 255)),
            4 => Ok(Self::from_u8(nibble(0), nibble(1), nibble(2), nibble(3))),
            6 => Ok(Self::from_u8(pair(0), pair(2), pair(4), 255)),
            8 => Ok(Self::from_u8(pair(0), pair(2), pair(4), pair(6))),
            _ => Err(fail("expected 3, 4, 6 or 8 hex digits")),
        }
    }

    /// The channels as 8-bit values, rounded and clamped.
    pub fn to_u8(self) -> [u8; 4] {
        let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        [
            channel(self.r),
            channel(self.g),
            channel(self.b),
            channel(self.a),
        ]
    }

    /// `#rrggbb`, or `#rrggbbaa` when not fully opaque.
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.to_u8();
        if a == 255 {
            format!("#{r:02x}{g:02x}{b:02x}")
        } else {
            format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
        }
    }

    pub fn with_alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    /// This color drawn over `below`, as a normal alpha blend.
    pub fn over(self, below: Rgba) -> Rgba {
        let a = self.a + below.a * (1.0 - self.a);
        if a <= 0.0 {
            return Rgba::TRANSPARENT;
        }
        let mix = |top: f32, bottom: f32| (top * self.a + bottom * below.a * (1.0 - self.a)) / a;
        Rgba::new(
            mix(self.r, below.r),
            mix(self.g, below.g),
            mix(self.b, below.b),
            a,
        )
    }

    /// The terminal color for this one, dropping alpha.
    pub fn to_text_color(self) -> Color {
        let [r, g, b, _] = self.to_u8();
        Color::Rgb(r, g, b)
    }
}

impl FromStr for Rgba {
    type Err = ColorParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(s)
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Rgba {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgba {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        Rgba::parse_hex(&text).map_err(de::Error::custom)
    }
}
