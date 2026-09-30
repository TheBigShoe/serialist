//! Font types shared by the buffer, UI and terminal settings.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};

use super::de::{Raw, line_height_number, scalar};

/// OpenType feature settings by four-character tag, the file form of Zed's
/// `buffer_font_features`: `{"calt": false, "ss01": true, "cv01": 7}`.
///
/// Booleans read as 0 and 1, integers pass through. A tag is exactly four ASCII
/// letters or digits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontFeatures(BTreeMap<String, u32>);

impl FontFeatures {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, tag: impl Into<String>, value: u32) {
        self.0.insert(tag.into(), value);
    }

    /// The value set for `tag`, if any.
    pub fn get(&self, tag: &str) -> Option<u32> {
        self.0.get(tag).copied()
    }

    /// Features sorted by tag.
    pub fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        self.0.iter().map(|(tag, value)| (tag.as_str(), *value))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// False when `calt` or `liga` is explicitly off. Ligatures are on when neither
    /// is mentioned, as in Zed.
    pub fn ligatures_enabled(&self) -> bool {
        self.get("calt") != Some(0) && self.get("liga") != Some(0)
    }
}

impl FromIterator<(String, u32)> for FontFeatures {
    fn from_iter<I: IntoIterator<Item = (String, u32)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

fn feature_value(raw: Raw<'_>) -> Result<u32, String> {
    match raw {
        Raw::Bool(b) => Ok(u32::from(b)),
        other => match other.unsigned() {
            Some(v) if v <= u64::from(u32::MAX) => Ok(v as u32),
            _ => Err("a font feature value must be true, false or a non-negative integer".into()),
        },
    }
}

struct FeatureValue(u32);

impl<'de> Deserialize<'de> for FeatureValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        scalar(d, "true, false or a non-negative integer", feature_value).map(FeatureValue)
    }
}

impl<'de> Deserialize<'de> for FontFeatures {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct FeaturesVisitor;

        impl<'de> Visitor<'de> for FeaturesVisitor {
            type Value = FontFeatures;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of OpenType feature tags to true, false or an integer")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<FontFeatures, A::Error> {
                let mut features = BTreeMap::new();
                while let Some(tag) = map.next_key::<String>()? {
                    if tag.len() != 4 || !tag.bytes().all(|b| b.is_ascii_alphanumeric()) {
                        return Err(de::Error::custom(format!(
                            "font feature tag {tag:?} must be four ASCII letters or digits"
                        )));
                    }
                    let FeatureValue(value) = map.next_value()?;
                    features.insert(tag, value);
                }
                Ok(FontFeatures(features))
            }
        }

        d.deserialize_map(FeaturesVisitor)
    }
}

impl Serialize for FontFeatures {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (tag, value) in &self.0 {
            map.serialize_entry(tag, value)?;
        }
        map.end()
    }
}

/// Line height as a multiple of the font size.
///
/// The file form is `"comfortable"`, `"standard"`, a number, or Zed's `{"custom": 1.5}`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LineHeight {
    /// 1.618, the golden ratio.
    Comfortable,
    /// 1.3.
    #[default]
    Standard,
    Custom(f32),
}

impl LineHeight {
    pub const COMFORTABLE_VALUE: f32 = 1.618;
    pub const STANDARD_VALUE: f32 = 1.3;

    /// The multiplier to apply to the font size.
    pub fn value(self) -> f32 {
        match self {
            LineHeight::Comfortable => Self::COMFORTABLE_VALUE,
            LineHeight::Standard => Self::STANDARD_VALUE,
            LineHeight::Custom(v) => v,
        }
    }
}

fn line_height_scalar(raw: Raw<'_>) -> Result<LineHeight, String> {
    match raw {
        Raw::Str("comfortable") => Ok(LineHeight::Comfortable),
        Raw::Str("standard") => Ok(LineHeight::Standard),
        Raw::Str(other) => Err(format!(
            "unknown line height {other:?}, expected \"comfortable\", \"standard\" or a number"
        )),
        other => line_height_number(other).map(LineHeight::Custom),
    }
}

impl<'de> Deserialize<'de> for LineHeight {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct LineHeightVisitor;

        impl<'de> Visitor<'de> for LineHeightVisitor {
            type Value = LineHeight;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("\"comfortable\", \"standard\", a number or {\"custom\": number}")
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<LineHeight, E> {
                line_height_scalar(Raw::Int(v)).map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<LineHeight, E> {
                line_height_scalar(Raw::Uint(v)).map_err(E::custom)
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<LineHeight, E> {
                line_height_scalar(Raw::Float(v)).map_err(E::custom)
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<LineHeight, E> {
                line_height_scalar(Raw::Str(v)).map_err(E::custom)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LineHeight, A::Error> {
                let mut custom = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "custom" {
                        custom = Some(scalar_line_height(&mut map)?);
                    } else {
                        return Err(de::Error::custom(format!(
                            "unknown key {key:?} in a line height, expected \"custom\""
                        )));
                    }
                }
                custom
                    .map(LineHeight::Custom)
                    .ok_or_else(|| de::Error::custom("a line height object needs a \"custom\" key"))
            }
        }

        d.deserialize_any(LineHeightVisitor)
    }
}

fn scalar_line_height<'de, A: MapAccess<'de>>(map: &mut A) -> Result<f32, A::Error> {
    struct Number(f32);

    impl<'de> Deserialize<'de> for Number {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            scalar(d, "a positive number", line_height_number).map(Number)
        }
    }

    map.next_value::<Number>().map(|n| n.0)
}

impl Serialize for LineHeight {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            LineHeight::Comfortable => s.serialize_str("comfortable"),
            LineHeight::Standard => s.serialize_str("standard"),
            LineHeight::Custom(v) => s.serialize_f32(*v),
        }
    }
}

/// A font with every setting resolved, ready for the UI to turn into a text style.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    /// `None` means the platform or bundled default family.
    pub family: Option<String>,
    /// Size in points.
    pub size: f32,
    /// 100 to 900.
    pub weight: f32,
    pub features: FontFeatures,
    /// Families tried in order for glyphs the main family lacks.
    pub fallbacks: Vec<String>,
    /// Line height as a multiple of `size`.
    pub line_height: f32,
}

impl FontSpec {
    /// Line height in points: `size * line_height`.
    pub fn line_height_px(&self) -> f32 {
        self.size * self.line_height
    }
}
