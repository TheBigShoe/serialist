//! Themes in Zed's file format, with no GPUI types.
//!
//! A theme file is a Zed theme family (schema v0.2.0): flat dotted color keys plus
//! `players` and `syntax`. [`ThemeFamily::parse`] reads one, [`ThemeRegistry`] holds the
//! bundled `Serialist Dark` and `Serialist Light` themes plus the user's
//! `themes/*.json`, and [`ThemeRegistry::resolve`] picks the theme a `theme` setting
//! asks for. Colors are [`Rgba`]; turning them into GPUI's types is the UI's job.
//!
//! The app reads the keys listed in [`USED_STYLE_KEYS`]; the bundled themes define all
//! of them. A user theme may leave some out, so read colors with
//! [`Theme::color_or`] and a sensible fallback.

mod color;
mod family;
mod keys;
mod registry;

#[cfg(test)]
mod tests;

pub use color::{ColorParseError, Rgba};
pub use family::{
    Appearance, FontStyle, PlayerColors, SyntaxStyle, Theme, ThemeError, ThemeFamily, ThemeWarning,
};
pub use keys::{TERMINAL_ANSI_KEYS, USED_STYLE_KEYS, USED_SYNTAX_KEYS};
pub use registry::ThemeRegistry;
