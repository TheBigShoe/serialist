//! Fonts from settings: the terminal's font (the buffer font with the `terminal`
//! overrides) and the UI font, as GPUI [`Font`] values.
//!
//! Zed's font keys map one to one onto [`Font`]: the family, the weight, OpenType
//! features and fallbacks. The size and line height travel next to it, since GPUI takes
//! the size per text run and the terminal turns the line height into its row pitch.

use std::sync::{Arc, OnceLock};

use crate::prelude::*;

/// The terminal font when settings name none: the platform's usual monospace face.
#[cfg(target_os = "macos")]
pub const DEFAULT_MONO_FAMILY: &str = "Menlo";
#[cfg(target_os = "windows")]
pub const DEFAULT_MONO_FAMILY: &str = "Cascadia Mono";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_MONO_FAMILY: &str = "DejaVu Sans Mono";

/// Tried in order when [`DEFAULT_MONO_FAMILY`] is not installed.
#[cfg(target_os = "macos")]
pub const DEFAULT_MONO_FALLBACKS: &[&str] = &[];
#[cfg(target_os = "windows")]
pub const DEFAULT_MONO_FALLBACKS: &[&str] = &["Consolas"];
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub const DEFAULT_MONO_FALLBACKS: &[&str] = &["monospace"];

/// GPUI's name for the platform's UI face.
pub const SYSTEM_UI_FAMILY: &str = ".SystemUIFont";

/// Zed's "standard" line height, a multiple of the font size.
pub const STANDARD_LINE_HEIGHT: f32 = 1.3;

/// Zed's default buffer font size.
pub const DEFAULT_BUFFER_FONT_SIZE: f32 = 15.0;

/// Zed's default UI font size.
pub const DEFAULT_UI_FONT_SIZE: f32 = 16.0;

/// Which default a font falls back to when settings leave the family unset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontRole {
    /// The terminal: the platform monospace face.
    Mono,
    /// Panels, tabs and the status line: the system UI face.
    Ui,
}

impl FontRole {
    fn default_family(self) -> &'static str {
        match self {
            FontRole::Mono => DEFAULT_MONO_FAMILY,
            FontRole::Ui => SYSTEM_UI_FAMILY,
        }
    }

    fn default_fallbacks(self) -> &'static [&'static str] {
        match self {
            FontRole::Mono => DEFAULT_MONO_FALLBACKS,
            FontRole::Ui => &[],
        }
    }
}

/// The parts of a font as settings describe them, before defaults.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontParts<'a> {
    pub family: Option<&'a str>,
    /// CSS weight, 100 to 900.
    pub weight: Option<f32>,
    /// OpenType feature tags and values: 0 off, 1 on, higher picks an alternate.
    pub features: Vec<(String, u32)>,
    pub fallbacks: &'a [String],
}

/// Build a GPUI [`Font`]. An unset or blank family takes the role's default, and then
/// the default's fallbacks too, so a Linux box without DejaVu still gets a monospace
/// face. A named family keeps exactly the fallbacks the settings list.
pub fn build_font(parts: &FontParts<'_>, role: FontRole) -> Font {
    let named = parts.family.map(str::trim).filter(|f| !f.is_empty());
    let family = named.unwrap_or(role.default_family());
    let mut fallbacks: Vec<String> = parts.fallbacks.to_vec();
    if named.is_none() {
        fallbacks.extend(role.default_fallbacks().iter().map(|f| f.to_string()));
    }
    let mut features = parts.features.clone();
    features.sort_by(|a, b| a.0.cmp(&b.0));
    features.dedup_by(|a, b| a.0 == b.0);
    Font {
        family: SharedString::from(family.to_owned()),
        features: FontFeatures(Arc::new(features)),
        fallbacks: (!fallbacks.is_empty()).then(|| FontFallbacks::from_fonts(fallbacks)),
        weight: parts
            .weight
            .map_or(FontWeight::default(), |w| FontWeight(w.clamp(100.0, 900.0))),
        style: FontStyle::default(),
    }
}

/// What the terminal draws with.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalFont {
    pub font: Font,
    pub size: Pixels,
    /// Row pitch as a multiple of `size`. A row is never shorter than the font's own
    /// ascent plus descent, whatever this says.
    pub line_height: f32,
}

impl Default for TerminalFont {
    fn default() -> Self {
        Self {
            font: build_font(&FontParts::default(), FontRole::Mono),
            size: px(DEFAULT_BUFFER_FONT_SIZE),
            line_height: STANDARD_LINE_HEIGHT,
        }
    }
}

/// What panels, tabs and the status line draw with. gpui-kit takes the family and the
/// size (which also becomes the window's rem size); the weight and features apply at
/// the workspace root, from where every child inherits them.
#[derive(Clone, Debug, PartialEq)]
pub struct UiFont {
    pub font: Font,
    pub size: Pixels,
}

impl Default for UiFont {
    fn default() -> Self {
        Self {
            font: build_font(&FontParts::default(), FontRole::Ui),
            size: px(DEFAULT_UI_FONT_SIZE),
        }
    }
}

/// The font families installed on this machine, listed once per process: listing
/// costs around a hundred milliseconds on macOS, so it only happens when settings name
/// a family (see [`substitute_missing_family`]).
pub fn installed_families(cx: &App) -> &'static [String] {
    static NAMES: OnceLock<Vec<String>> = OnceLock::new();
    NAMES.get_or_init(|| cx.text_system().all_font_names())
}

/// When `font` names a family `installed` does not list, switch to the first of its
/// fallbacks that is installed, else the role's default, and return the family that
/// was missing. GPUI would otherwise draw a missing family in its own fallback stack,
/// which starts with a proportional face (Helvetica on macOS) and ruins a terminal.
///
/// Families GPUI provides itself (`.SystemUIFont`, `.ZedMono`) are never missing, and
/// an empty `installed` (a text system that cannot list fonts) changes nothing.
pub fn substitute_missing_family(
    font: &mut Font,
    role: FontRole,
    installed: &[String],
) -> Option<SharedString> {
    let is_installed = |family: &str| {
        family.starts_with('.')
            || installed
                .iter()
                .any(|name| name.eq_ignore_ascii_case(family))
    };
    if installed.is_empty() || is_installed(&font.family) {
        return None;
    }
    let fallbacks = font
        .fallbacks
        .as_ref()
        .map(|f| f.fallback_list().to_vec())
        .unwrap_or_default();
    let substitute = fallbacks
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(role.default_family()))
        .chain(role.default_fallbacks().iter().copied())
        .find(|family| is_installed(family))
        .unwrap_or(SYSTEM_UI_FAMILY);
    Some(std::mem::replace(
        &mut font.family,
        SharedString::from(substitute.to_owned()),
    ))
}

/// Sizes outside this range are clamped: a zero or negative size from a typo would
/// make every row zero pixels tall.
pub fn clamp_font_size(size: f32) -> Pixels {
    px(if size.is_finite() {
        size.clamp(4.0, 128.0)
    } else {
        DEFAULT_BUFFER_FONT_SIZE
    })
}

/// A line height multiple, clamped to something drawable.
pub fn clamp_line_height(multiple: f32) -> f32 {
    if multiple.is_finite() {
        multiple.clamp(1.0, 4.0)
    } else {
        STANDARD_LINE_HEIGHT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_families_take_the_platform_defaults() {
        let mono = build_font(&FontParts::default(), FontRole::Mono);
        assert_eq!(mono.family.as_ref(), DEFAULT_MONO_FAMILY);
        let expected: Vec<String> = DEFAULT_MONO_FALLBACKS
            .iter()
            .map(|f| f.to_string())
            .collect();
        assert_eq!(
            mono.fallbacks
                .as_ref()
                .map(|f| f.fallback_list().to_vec())
                .unwrap_or_default(),
            expected
        );
        let ui = build_font(
            &FontParts {
                family: Some("  "),
                ..FontParts::default()
            },
            FontRole::Ui,
        );
        assert_eq!(ui.family.as_ref(), SYSTEM_UI_FAMILY);
        assert_eq!(ui.fallbacks, None);
    }

    #[test]
    fn named_fonts_keep_their_weight_features_and_fallbacks() {
        let fallbacks = vec!["Menlo".to_owned(), "Symbols Nerd Font".to_owned()];
        let font = build_font(
            &FontParts {
                family: Some("Berkeley Mono"),
                weight: Some(450.0),
                features: vec![("ss01".into(), 1), ("calt".into(), 0)],
                fallbacks: &fallbacks,
            },
            FontRole::Mono,
        );
        assert_eq!(font.family.as_ref(), "Berkeley Mono");
        assert_eq!(font.weight, FontWeight(450.0));
        assert_eq!(font.features.is_calt_enabled(), Some(false));
        assert_eq!(
            font.features.tag_value_list(),
            [("calt".to_owned(), 0), ("ss01".to_owned(), 1)]
        );
        assert_eq!(
            font.fallbacks.unwrap().fallback_list(),
            ["Menlo", "Symbols Nerd Font"]
        );
    }

    #[test]
    fn a_missing_family_falls_to_an_installed_fallback_then_the_default() {
        // The platform default is part of the fake installed set so the second half of
        // the test means the same thing on every OS (Windows CI has no Cascadia Mono).
        let installed: Vec<String> = ["Menlo", "Monaco", "DejaVu Sans Mono", "Consolas"]
            .iter()
            .map(|f| f.to_string())
            .chain(std::iter::once(DEFAULT_MONO_FAMILY.to_string()))
            .collect();
        let fallbacks = vec!["Nope Mono".to_owned(), "Monaco".to_owned()];
        let mut font = build_font(
            &FontParts {
                family: Some("Berkeley Mono"),
                fallbacks: &fallbacks,
                ..FontParts::default()
            },
            FontRole::Mono,
        );
        let missing = substitute_missing_family(&mut font, FontRole::Mono, &installed);
        assert_eq!(missing.as_deref(), Some("Berkeley Mono"));
        assert_eq!(
            font.family.as_ref(),
            "Monaco",
            "the first installed fallback"
        );

        let mut font = build_font(
            &FontParts {
                family: Some("Berkeley Mono"),
                ..FontParts::default()
            },
            FontRole::Mono,
        );
        substitute_missing_family(&mut font, FontRole::Mono, &installed);
        assert_eq!(font.family.as_ref(), DEFAULT_MONO_FAMILY);

        // Installed (in any case), virtual, or nothing to compare with: unchanged.
        for (family, list) in [
            ("menlo", installed.clone()),
            (".ZedMono", installed.clone()),
            ("Berkeley Mono", Vec::new()),
        ] {
            let mut font = build_font(
                &FontParts {
                    family: Some(family),
                    ..FontParts::default()
                },
                FontRole::Mono,
            );
            assert_eq!(
                substitute_missing_family(&mut font, FontRole::Mono, &list),
                None
            );
            assert_eq!(font.family.as_ref(), family);
        }
    }

    #[test]
    fn sizes_and_line_heights_are_clamped() {
        assert_eq!(clamp_font_size(0.0), px(4.0));
        assert_eq!(clamp_font_size(20.0), px(20.0));
        assert_eq!(clamp_font_size(f32::NAN), px(DEFAULT_BUFFER_FONT_SIZE));
        assert_eq!(clamp_line_height(0.5), 1.0);
        assert_eq!(clamp_line_height(1.618), 1.618);
    }
}
