//! Terminal colors: the palette the element draws with, and the mapping from the
//! store's [`Color`] and [`Style`] to GPUI colors.
//!
//! [`TerminalPalette::from_lookup`] fills a palette from a Zed theme's `terminal.*`,
//! `players[0]` and `search.*` keys; [`TerminalPalette::default`] is a One Dark-like set
//! and the fallback for every key a theme leaves out.

use serialist_core::{Color, Direction, Style, StyleFlags};

use crate::prelude::*;

/// Every color the terminal element paints with.
#[derive(Clone, Debug, PartialEq)]
pub struct TerminalPalette {
    pub foreground: Hsla,
    pub background: Hsla,
    /// Faint text (SGR 2) with the default color, the placeholder glyphs for control
    /// characters, and the timestamp gutter.
    pub dim_foreground: Hsla,
    /// Bold text with the default color, when `bold_is_bright` is on.
    pub bright_foreground: Hsla,
    /// Black, red, green, yellow, blue, magenta, cyan, white, then the bright eight.
    pub ansi: [Hsla; 16],
    pub selection: Hsla,
    pub cursor: Hsla,
    pub search_match: Hsla,
    pub active_match: Hsla,
    /// Default-colored text of lines the user sent.
    pub tx: Hsla,
    /// Default-colored text of app notices (connected, timeouts).
    pub notice: Hsla,
    /// The plugin color: default-colored text of decoded frames' summary lines.
    pub decoded: Hsla,
    /// Draw bold text in ANSI colors 0..=7 with their bright variants, as xterm does.
    pub bold_is_bright: bool,
    /// Minimum WCAG contrast ratio between a glyph and what is behind it; 1.0 turns the
    /// guard off. Text an application colors unreadably (dark blue on the dark
    /// background) is lightened or darkened until it reaches this.
    pub minimum_contrast: f32,
}

impl Default for TerminalPalette {
    fn default() -> Self {
        let hex = |value: u32| Hsla::from(rgb(value));
        Self {
            foreground: hex(0xdcdfe4),
            background: hex(0x1e2127),
            dim_foreground: hex(0x7f848e),
            bright_foreground: hex(0xffffff),
            ansi: [
                hex(0x3f4451),
                hex(0xe05561),
                hex(0x8cc265),
                hex(0xd18f52),
                hex(0x4aa5f0),
                hex(0xc162de),
                hex(0x42b3c2),
                hex(0xd7dae0),
                hex(0x4f5666),
                hex(0xff616e),
                hex(0xa5e075),
                hex(0xf0a45d),
                hex(0x4dc4ff),
                hex(0xde73ff),
                hex(0x4cd1e0),
                hex(0xe6e6e6),
            ],
            selection: Hsla::from(rgba(0x4b5a78cc)),
            cursor: hex(0x74ade8),
            search_match: Hsla::from(rgba(0xd0a04d55)),
            active_match: Hsla::from(rgba(0xe8a33dcc)),
            tx: hex(0x74ade8),
            notice: hex(0xa9afbc),
            decoded: hex(0xc162de),
            bold_is_bright: true,
            minimum_contrast: 3.0,
        }
    }
}

/// The Zed theme keys of the sixteen ANSI colors, in [`TerminalPalette::ansi`] order.
pub const ANSI_KEYS: [&str; 16] = [
    "terminal.ansi.black",
    "terminal.ansi.red",
    "terminal.ansi.green",
    "terminal.ansi.yellow",
    "terminal.ansi.blue",
    "terminal.ansi.magenta",
    "terminal.ansi.cyan",
    "terminal.ansi.white",
    "terminal.ansi.bright_black",
    "terminal.ansi.bright_red",
    "terminal.ansi.bright_green",
    "terminal.ansi.bright_yellow",
    "terminal.ansi.bright_blue",
    "terminal.ansi.bright_magenta",
    "terminal.ansi.bright_cyan",
    "terminal.ansi.bright_white",
];

impl TerminalPalette {
    /// A palette from a Zed theme's style keys, looked up by name (`terminal.foreground`,
    /// `players[0].cursor`). Each color falls back through related keys, then to the
    /// [`Default`] palette, so a theme that sets nothing terminal-specific still draws.
    ///
    /// | Palette | Keys, first set wins |
    /// | --- | --- |
    /// | background | `terminal.background`, `editor.background`, `background` |
    /// | foreground | `terminal.foreground`, `editor.foreground`, `text` |
    /// | bright foreground | `terminal.bright_foreground`, then the foreground |
    /// | dim foreground | `terminal.dim_foreground`, `text.muted` |
    /// | ANSI 0 to 15 | `terminal.ansi.black` … `terminal.ansi.bright_white` |
    /// | cursor | `players[0].cursor`, `text.accent` |
    /// | selection | `players[0].selection`, `element.selected` |
    /// | search match | `search.match_background` |
    /// | active match | `search.active_match_background`, then the search match |
    /// | sent lines | `info`, `text.accent` |
    /// | notices | `text.muted`, `hint` |
    /// | decoded frame summaries | `syntax.keyword`, `terminal.ansi.magenta` |
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<Hsla>) -> Self {
        let defaults = Self::default();
        let first = |keys: &[&str]| keys.iter().find_map(|key| lookup(key));
        let foreground = first(&["terminal.foreground", "editor.foreground", "text"])
            .unwrap_or(defaults.foreground);
        let search_match = first(&["search.match_background"]).unwrap_or(defaults.search_match);
        let mut ansi = defaults.ansi;
        for (slot, key) in ansi.iter_mut().zip(ANSI_KEYS) {
            if let Some(color) = lookup(key) {
                *slot = color;
            }
        }
        Self {
            foreground,
            background: first(&["terminal.background", "editor.background", "background"])
                .unwrap_or(defaults.background),
            dim_foreground: first(&["terminal.dim_foreground", "text.muted"])
                .unwrap_or(defaults.dim_foreground),
            bright_foreground: first(&["terminal.bright_foreground"]).unwrap_or(
                if lookup("terminal.foreground").is_some() {
                    foreground
                } else {
                    defaults.bright_foreground
                },
            ),
            ansi,
            selection: first(&["players[0].selection", "element.selected"])
                .unwrap_or(defaults.selection),
            cursor: first(&["players[0].cursor", "text.accent"]).unwrap_or(defaults.cursor),
            search_match,
            active_match: first(&["search.active_match_background"]).unwrap_or(
                if lookup("search.match_background").is_some() {
                    search_match.opacity(1.0)
                } else {
                    defaults.active_match
                },
            ),
            tx: first(&["info", "text.accent"]).unwrap_or(defaults.tx),
            notice: first(&["text.muted", "hint"]).unwrap_or(defaults.notice),
            decoded: first(&["syntax.keyword", "terminal.ansi.magenta"])
                .unwrap_or(defaults.decoded),
            ..defaults
        }
    }
}

/// The colors and decorations one run is drawn with, after inverse, dim, hidden and
/// the contrast guard are applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedStyle {
    pub foreground: Hsla,
    /// `None` when the run sits on the terminal background.
    pub background: Option<Hsla>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
}

fn from_bytes(r: u8, g: u8, b: u8) -> Hsla {
    Hsla::from(rgb(u32::from_be_bytes([0, r, g, b])))
}

impl TerminalPalette {
    /// The color of an ANSI or indexed color number, 0..=255: the sixteen theme colors,
    /// then the 6x6x6 cube, then 24 greys.
    pub fn indexed(&self, index: u8) -> Hsla {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let cube = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
                from_bytes(level(cube / 36), level((cube / 6) % 6), level(cube % 6))
            }
            232..=255 => {
                let grey = 8 + 10 * (index - 232);
                from_bytes(grey, grey, grey)
            }
        }
    }

    /// A foreground color; `Default` takes the line's own default so sent lines and
    /// notices stand apart from received text.
    pub fn foreground_of(&self, color: Color, bold: bool, direction: Direction) -> Hsla {
        match color {
            Color::Default => match direction {
                Direction::Tx => self.tx,
                Direction::Notice => self.notice,
                Direction::Rx if bold && self.bold_is_bright => self.bright_foreground,
                Direction::Rx => self.foreground,
            },
            Color::Ansi(n) | Color::Indexed(n) if n < 8 && bold && self.bold_is_bright => {
                self.ansi[n as usize + 8]
            }
            Color::Ansi(n) | Color::Indexed(n) => self.indexed(n),
            Color::Rgb(r, g, b) => from_bytes(r, g, b),
        }
    }

    /// A background color; `None` for the default, which the element does not paint.
    pub fn background_of(&self, color: Color) -> Option<Hsla> {
        match color {
            Color::Default => None,
            Color::Ansi(n) | Color::Indexed(n) => Some(self.indexed(n)),
            Color::Rgb(r, g, b) => Some(from_bytes(r, g, b)),
        }
    }

    /// Resolve a run's style: colors, then inverse, dim, hidden, then the contrast guard
    /// against whatever the glyph will actually sit on.
    pub fn resolve(&self, style: &Style, direction: Direction) -> ResolvedStyle {
        self.resolve_with(style, direction, None)
    }

    /// [`Self::resolve`] for a run of a decoded frame's summary line (a notice), whose
    /// default color is the plugin color, [`Self::decoded`].
    pub fn resolve_decoded(&self, style: &Style) -> ResolvedStyle {
        self.resolve_with(style, Direction::Notice, Some(self.decoded))
    }

    fn resolve_with(
        &self,
        style: &Style,
        direction: Direction,
        default_foreground: Option<Hsla>,
    ) -> ResolvedStyle {
        let flags = style.flags;
        let bold = flags.contains(StyleFlags::BOLD);
        let inverse = flags.contains(StyleFlags::INVERSE);
        let mut foreground = match (style.fg, default_foreground) {
            (Color::Default, Some(color)) => color,
            _ => self.foreground_of(style.fg, bold, direction),
        };
        let mut background = self.background_of(style.bg);
        if inverse {
            let behind = background.unwrap_or(self.background);
            background = Some(foreground);
            foreground = behind;
        }
        if flags.contains(StyleFlags::DIM) {
            foreground = if style.fg == Color::Default && !inverse {
                self.dim_foreground
            } else {
                foreground.opacity(0.7)
            };
        }
        if flags.contains(StyleFlags::CONTROL) {
            // A placeholder for a control byte is always the dim color, whatever the pen
            // was when the byte arrived: it is not the device's text.
            foreground = self.dim_foreground;
        }
        let behind = background.unwrap_or(self.background);
        foreground = if flags.contains(StyleFlags::HIDDEN) {
            behind
        } else {
            ensure_contrast(foreground, behind, self.minimum_contrast)
        };
        ResolvedStyle {
            foreground,
            background,
            bold,
            italic: flags.contains(StyleFlags::ITALIC),
            underline: flags.contains(StyleFlags::UNDERLINE),
            strikethrough: flags.contains(StyleFlags::STRIKETHROUGH),
        }
    }
}

/// WCAG relative luminance of an opaque color.
pub fn relative_luminance(color: Hsla) -> f32 {
    let rgba = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

/// WCAG contrast ratio, 1.0 (none) to 21.0 (black on white). A translucent foreground
/// is judged by what it looks like over `background`.
pub fn contrast_ratio(foreground: Hsla, background: Hsla) -> f32 {
    let fg = relative_luminance(flatten(foreground, background));
    let bg = relative_luminance(background);
    let (light, dark) = if fg > bg { (fg, bg) } else { (bg, fg) };
    (light + 0.05) / (dark + 0.05)
}

/// `color` composited over an opaque `background`.
fn flatten(color: Hsla, background: Hsla) -> Hsla {
    if color.a >= 1.0 {
        return color;
    }
    let (fg, bg) = (color.to_rgb(), background.to_rgb());
    let mix = |f: f32, b: f32| f * color.a + b * (1.0 - color.a);
    Hsla::from(Rgba {
        r: mix(fg.r, bg.r),
        g: mix(fg.g, bg.g),
        b: mix(fg.b, bg.b),
        a: 1.0,
    })
}

/// Move `foreground`'s lightness away from `background` just far enough to reach
/// `minimum` contrast, keeping hue and saturation, as Zed's terminal does. Colors that
/// already pass are returned unchanged.
pub fn ensure_contrast(foreground: Hsla, background: Hsla, minimum: f32) -> Hsla {
    if minimum <= 1.0 || contrast_ratio(foreground, background) >= minimum {
        return foreground;
    }
    let solid = Hsla {
        a: 1.0,
        ..flatten(foreground, background)
    };
    let at = |l: f32| Hsla { l, ..solid };
    // Bisect between the current lightness and an extreme for the smallest change
    // that passes.
    let toward = |extreme: f32| {
        if contrast_ratio(at(extreme), background) < minimum {
            return None;
        }
        let (mut failing, mut passing) = (solid.l, extreme);
        for _ in 0..24 {
            let mid = (failing + passing) / 2.0;
            if contrast_ratio(at(mid), background) >= minimum {
                passing = mid;
            } else {
                failing = mid;
            }
        }
        Some(at(passing))
    };
    // Lighten on dark backgrounds and darken on light ones; if that direction cannot
    // reach the target the other one might, and if neither can, take the extreme.
    let (first, second) = if relative_luminance(background) < 0.18 {
        (1.0, 0.0)
    } else {
        (0.0, 1.0)
    };
    toward(first)
        .or_else(|| toward(second))
        .unwrap_or_else(|| at(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_bytes(color: Hsla) -> (u8, u8, u8) {
        let c = color.to_rgb();
        let byte = |v: f32| (v * 255.0).round() as u8;
        (byte(c.r), byte(c.g), byte(c.b))
    }

    #[test]
    fn control_glyphs_are_the_dim_color_whatever_the_pen_was() {
        let palette = TerminalPalette::default();
        let control = StyleFlags(StyleFlags::DIM.0 | StyleFlags::CONTROL.0);
        let style = |fg, flags| Style {
            fg,
            bg: Color::Default,
            flags,
        };
        for direction in [Direction::Rx, Direction::Tx, Direction::Notice] {
            let glyph = palette.resolve(&style(Color::Default, control), direction);
            assert_eq!(glyph.foreground, palette.dim_foreground, "{direction:?}");
            assert_eq!(glyph.background, None);
            assert!(!glyph.bold && !glyph.italic && !glyph.underline);
        }
        // A colored pen, or bold, does not tint or brighten a placeholder.
        let red = style(Color::Ansi(1), StyleFlags(control.0 | StyleFlags::BOLD.0));
        assert_eq!(
            palette.resolve(&red, Direction::Rx).foreground,
            palette.dim_foreground
        );
        // Ordinary text next to it is still the foreground, and plain dim text is the
        // same color, so glyphs read as the faint text they are.
        let text = palette.resolve(&Style::default(), Direction::Rx);
        assert_eq!(text.foreground, palette.foreground);
        let faint = palette.resolve(&style(Color::Default, StyleFlags::DIM), Direction::Rx);
        assert_eq!(faint.foreground, palette.dim_foreground);
        // A theme's dim color is the glyphs' color.
        let themed = TerminalPalette {
            dim_foreground: Hsla::from(rgb(0x808040)),
            ..palette
        };
        let glyph = themed.resolve(&style(Color::Default, control), Direction::Rx);
        assert_eq!(glyph.foreground, themed.dim_foreground);
    }

    #[test]
    fn theme_keys_fill_the_palette_and_the_rest_falls_back() {
        assert_eq!(
            TerminalPalette::from_lookup(|_| None),
            TerminalPalette::default(),
            "a theme with no keys draws with the defaults"
        );

        let hex = |value: u32| Hsla::from(rgb(value));
        let keys = std::collections::HashMap::from([
            ("terminal.background", hex(0x101010)),
            ("terminal.foreground", hex(0xe0e0e0)),
            ("terminal.ansi.red", hex(0xaa0000)),
            ("terminal.ansi.bright_white", hex(0xfefefe)),
            ("players[0].cursor", hex(0x00ff00)),
            ("players[0].selection", Hsla::from(rgba(0x3355ff40))),
            ("search.match_background", Hsla::from(rgba(0xffff0040))),
            ("text.muted", hex(0x808080)),
            ("info", hex(0x2080ff)),
        ]);
        let palette = TerminalPalette::from_lookup(|key| keys.get(key).copied());
        let defaults = TerminalPalette::default();
        assert_eq!(palette.background, keys["terminal.background"]);
        assert_eq!(palette.foreground, keys["terminal.foreground"]);
        assert_eq!(palette.ansi[1], keys["terminal.ansi.red"]);
        assert_eq!(palette.ansi[15], keys["terminal.ansi.bright_white"]);
        assert_eq!(
            palette.ansi[2], defaults.ansi[2],
            "unset ANSI colors keep theirs"
        );
        assert_eq!(palette.cursor, keys["players[0].cursor"]);
        assert_eq!(palette.selection, keys["players[0].selection"]);
        assert_eq!(palette.search_match, keys["search.match_background"]);
        assert_eq!(
            palette.active_match,
            keys["search.match_background"].opacity(1.0)
        );
        // Related keys stand in for the missing terminal ones.
        assert_eq!(palette.bright_foreground, palette.foreground);
        assert_eq!(palette.dim_foreground, keys["text.muted"]);
        assert_eq!(palette.notice, keys["text.muted"]);
        assert_eq!(palette.tx, keys["info"]);
        assert_eq!(palette.decoded, defaults.decoded);
        let keyword =
            TerminalPalette::from_lookup(|key| (key == "syntax.keyword").then(|| hex(0xc678dd)));
        assert_eq!(keyword.decoded, hex(0xc678dd), "the plugin color");
    }

    #[test]
    fn decoded_summaries_take_the_plugin_color_unless_colored() {
        let palette = TerminalPalette::default();
        let plain = palette.resolve_decoded(&Style::default());
        assert_eq!(plain.foreground, palette.decoded);
        let red = palette.resolve_decoded(&Style {
            fg: Color::Ansi(1),
            ..Style::default()
        });
        assert_eq!(red.foreground, palette.ansi[1]);
    }

    #[test]
    fn the_256_color_cube_and_greys() {
        let palette = TerminalPalette::default();
        assert_eq!(rgb_bytes(palette.indexed(16)), (0, 0, 0));
        assert_eq!(rgb_bytes(palette.indexed(17)), (0, 0, 95));
        assert_eq!(rgb_bytes(palette.indexed(21)), (0, 0, 255));
        assert_eq!(rgb_bytes(palette.indexed(22)), (0, 95, 0));
        assert_eq!(rgb_bytes(palette.indexed(52)), (95, 0, 0));
        assert_eq!(rgb_bytes(palette.indexed(196)), (255, 0, 0));
        assert_eq!(rgb_bytes(palette.indexed(208)), (255, 135, 0));
        assert_eq!(rgb_bytes(palette.indexed(231)), (255, 255, 255));
        assert_eq!(rgb_bytes(palette.indexed(232)), (8, 8, 8));
        assert_eq!(rgb_bytes(palette.indexed(244)), (128, 128, 128));
        assert_eq!(rgb_bytes(palette.indexed(255)), (238, 238, 238));
        // The first sixteen are the theme's, through either spelling.
        assert_eq!(palette.indexed(1), palette.ansi[1]);
        assert_eq!(palette.indexed(15), palette.ansi[15]);
        assert_eq!(
            palette.foreground_of(Color::Indexed(4), false, Direction::Rx),
            palette.ansi[4]
        );
    }

    #[test]
    fn default_colors_follow_the_line_direction_and_bold_brightens() {
        let palette = TerminalPalette::default();
        let fg = |color, bold, direction| palette.foreground_of(color, bold, direction);
        assert_eq!(fg(Color::Default, false, Direction::Rx), palette.foreground);
        assert_eq!(
            fg(Color::Default, true, Direction::Rx),
            palette.bright_foreground
        );
        assert_eq!(fg(Color::Default, false, Direction::Tx), palette.tx);
        assert_eq!(fg(Color::Default, false, Direction::Notice), palette.notice);
        assert_eq!(fg(Color::Ansi(1), true, Direction::Rx), palette.ansi[9]);
        assert_eq!(fg(Color::Ansi(9), true, Direction::Rx), palette.ansi[9]);
        let plain = TerminalPalette {
            bold_is_bright: false,
            ..TerminalPalette::default()
        };
        assert_eq!(
            plain.foreground_of(Color::Ansi(1), true, Direction::Rx),
            plain.ansi[1]
        );
        assert_eq!(
            rgb_bytes(fg(Color::Rgb(1, 2, 3), false, Direction::Rx)),
            (1, 2, 3)
        );
        assert_eq!(palette.background_of(Color::Default), None);
        assert_eq!(
            palette.background_of(Color::Indexed(196)).map(rgb_bytes),
            Some((255, 0, 0))
        );
    }

    #[test]
    fn contrast_ratio_matches_wcag() {
        let black = Hsla::from(rgb(0x000000));
        let white = Hsla::from(rgb(0xffffff));
        assert!((contrast_ratio(white, black) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(black, white) - 21.0).abs() < 0.01);
        assert!((contrast_ratio(black, black) - 1.0).abs() < 0.001);
        // #777 on white is the classic 4.48:1.
        let grey = Hsla::from(rgb(0x777777));
        assert!((contrast_ratio(grey, white) - 4.48).abs() < 0.01);
    }

    #[test]
    fn minimum_contrast_lifts_unreadable_text_and_leaves_readable_text_alone() {
        let palette = TerminalPalette::default();
        let background = palette.background;
        // ANSI black on the dark background is nearly invisible.
        let black = palette.ansi[0];
        assert!(contrast_ratio(black, background) < 2.0);
        let fixed = ensure_contrast(black, background, 3.0);
        let ratio = contrast_ratio(fixed, background);
        assert!(
            (3.0..3.05).contains(&ratio),
            "just enough, not more: {ratio}"
        );
        assert!((fixed.h - black.h).abs() < 1e-3, "hue kept");
        assert!(fixed.l > black.l, "lightened on a dark background");

        // Already readable: untouched.
        assert_eq!(
            ensure_contrast(palette.foreground, background, 3.0),
            palette.foreground
        );
        // Guard off.
        assert_eq!(ensure_contrast(black, background, 1.0), black);

        // Light background: darkened instead.
        let paper = Hsla::from(rgb(0xfafafa));
        let yellow = Hsla::from(rgb(0xf0e060));
        let fixed = ensure_contrast(yellow, paper, 4.5);
        assert!(contrast_ratio(fixed, paper) >= 4.5);
        assert!(fixed.l < yellow.l);

        // A translucent color is judged as it will look.
        let faint = palette.foreground.opacity(0.1);
        assert!(contrast_ratio(ensure_contrast(faint, background, 3.0), background) >= 3.0);

        // Unreachable targets saturate at the extreme instead of looping.
        let fixed = ensure_contrast(background, background, 30.0);
        assert!((fixed.l - 1.0).abs() < 1e-6);
    }

    #[test]
    fn resolve_applies_inverse_dim_hidden_and_the_guard() {
        let palette = TerminalPalette::default();
        let style = |fg, bg, flags| Style { fg, bg, flags };

        let inverse = palette.resolve(
            &style(Color::Default, Color::Default, StyleFlags::INVERSE),
            Direction::Rx,
        );
        assert_eq!(inverse.background, Some(palette.foreground));
        assert_eq!(inverse.foreground, palette.background);

        let dim = palette.resolve(
            &style(Color::Default, Color::Default, StyleFlags::DIM),
            Direction::Rx,
        );
        assert_eq!(dim.foreground, palette.dim_foreground);

        let hidden = palette.resolve(
            &style(Color::Ansi(2), Color::Ansi(4), StyleFlags::HIDDEN),
            Direction::Rx,
        );
        assert_eq!(Some(hidden.foreground), hidden.background);

        // Dark blue text on a dark blue cube background is made readable.
        let clash = palette.resolve(
            &style(Color::Indexed(18), Color::Indexed(17), StyleFlags::NONE),
            Direction::Rx,
        );
        assert!(contrast_ratio(clash.foreground, clash.background.unwrap()) >= 3.0);

        let mut flags = StyleFlags::BOLD;
        flags.insert(StyleFlags::UNDERLINE);
        flags.insert(StyleFlags::ITALIC);
        flags.insert(StyleFlags::STRIKETHROUGH);
        let decorated =
            palette.resolve(&style(Color::Default, Color::Default, flags), Direction::Rx);
        assert!(decorated.bold && decorated.italic);
        assert!(decorated.underline && decorated.strikethrough);
        assert_eq!(decorated.background, None);
    }
}
