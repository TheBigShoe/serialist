//! The bridge from a Zed theme to gpui-kit's theme, so panels, inputs, buttons, the
//! status line and the scrollbars follow the same file the terminal does.
//!
//! gpui-kit keeps its colors in a data-driven [`ThemeConfig`] (a theme file of its own,
//! with keys such as `background`, `primary.background`, `sidebar.background`) and
//! resolves each missing key from related ones. The bridge fills a `ThemeConfig` from
//! the Zed keys that mean the same thing, leaves the rest to gpui-kit's own fallbacks,
//! and installs it with [`Theme::apply_config`] inside [`Theme::update`], which also
//! rebuilds the Base layer's scrollbar styles and repaints every window. Because the
//! config is installed as gpui-kit's light or dark theme, gpui-kit's own mode changes
//! reload it rather than a bundled one.
//!
//! Fonts ride along: the UI family and size become gpui-kit's `font.family` and
//! `font.size` (the root view turns the size into the window's rem size), and the
//! terminal family and size become `mono_font.*` for the monospace bits of the chrome.

use std::rc::Rc;

use crate::fonts::{TerminalFont, UiFont};
use crate::prelude::*;
use crate::terminal::palette::contrast_ratio;

/// The dimming behind a dialog: black at 60% on a dark theme, 35% on a light one.
const DARK_OVERLAY: &str = "#00000099";
const LIGHT_OVERLAY: &str = "#00000059";

/// `#rrggbbaa`, the form gpui-kit's theme files use.
pub fn hex(color: Hsla) -> SharedString {
    let rgba = color.to_rgb();
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    SharedString::from(format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        byte(rgba.r),
        byte(rgba.g),
        byte(rgba.b),
        byte(rgba.a)
    ))
}

/// A Zed theme as the bridge needs it: a name, an appearance and a key lookup.
pub struct ZedColors<'a> {
    pub name: &'a str,
    pub dark: bool,
    pub lookup: &'a dyn Fn(&str) -> Option<Hsla>,
}

impl ZedColors<'_> {
    fn first(&self, keys: &[&str]) -> Option<Hsla> {
        keys.iter().find_map(|key| (self.lookup)(key))
    }

    fn hex(&self, keys: &[&str]) -> Option<SharedString> {
        self.first(keys).map(hex)
    }
}

/// gpui-kit's theme for `zed`, with the UI and terminal fonts.
///
/// | gpui-kit | Zed keys, first set wins |
/// | --- | --- |
/// | `background` | `background` |
/// | `foreground`, `*.foreground` of panels, popovers, buttons | `text` |
/// | `muted.foreground`, `tab.foreground` | `text.muted` |
/// | `muted.background` | `surface.background` |
/// | `popover.background` | `elevated_surface.background` |
/// | `sidebar.background`, `list.background` | `panel.background` |
/// | `title_bar.background` | `title_bar.background` |
/// | `status_bar.background` | `status_bar.background` |
/// | `tab_bar.background`, `tab.background`, `tab.active.background` | `tab_bar.background`, `tab.inactive_background`, `tab.active_background` |
/// | `button.*`, `secondary.*` backgrounds | `element.background`, `element.hover`, `element.active` |
/// | `accent.background`, `list.hover.background` | `ghost_element.hover` |
/// | `list.active.background` | `ghost_element.selected`, `element.selected` |
/// | `border` and the panel borders | `border.variant`, `border` |
/// | `input.border`, `window_border` | `border` |
/// | `ring`, `drag.border` | `border.focused` |
/// | `list.active.border` | `border.selected`, `border.focused` |
/// | `primary.background`, `link` | `text.accent`, `icon.accent` |
/// | `scrollbar.*` | `scrollbar.track.background`, `scrollbar.thumb.background`, `scrollbar.thumb.hover_background` |
/// | `selection.background`, `caret` | `players[0].selection`, `players[0].cursor` |
/// | `success`, `danger`, `warning`, `info` backgrounds | `success`, `error`, `warning`, `info` |
/// | `overlay` (a dialog's backdrop) | none: black at 60% (dark) or 35% (light) |
/// | highlight `editor.background`, `editor.foreground` | `editor.background`, `editor.foreground` |
///
/// The primary foreground has no Zed key: it is whichever of `background` and `text`
/// reads better on the primary color.
pub fn kit_theme_config(zed: &ZedColors<'_>, ui: &UiFont, terminal: &TerminalFont) -> ThemeConfig {
    let border = zed.hex(&["border.variant", "border"]);
    let text = zed.hex(&["text"]);
    let primary = zed.first(&["text.accent", "icon.accent"]);
    let primary_foreground = primary.and_then(|primary| {
        let candidates = [zed.first(&["background"]), zed.first(&["text"])];
        candidates
            .into_iter()
            .flatten()
            .max_by(|a, b| contrast_ratio(*a, primary).total_cmp(&contrast_ratio(*b, primary)))
            .map(hex)
    });

    // gpui-kit keeps its base palette (`base.red` and so on) private, which rules out
    // struct-update syntax; it only feeds charts and the fallbacks of keys set here.
    let mut c = ThemeConfigColors::default();
    c.background = zed.hex(&["background"]);
    c.foreground = text.clone();
    c.muted = zed.hex(&["surface.background"]);
    c.muted_foreground = zed.hex(&["text.muted"]);
    c.border = border.clone();
    c.input = zed.hex(&["border"]);
    c.ring = zed.hex(&["border.focused"]);
    c.drag_border = zed.hex(&["border.focused"]);
    c.window_border = zed.hex(&["border"]);

    c.accent = zed.hex(&["ghost_element.hover", "element.hover"]);
    c.accent_foreground = text.clone();
    c.button = zed.hex(&["element.background"]);
    c.button_hover = zed.hex(&["element.hover"]);
    c.button_active = zed.hex(&["element.active"]);
    c.button_foreground = text.clone();
    c.secondary = zed.hex(&["element.background"]);
    c.secondary_hover = zed.hex(&["element.hover"]);
    c.secondary_active = zed.hex(&["element.active"]);
    c.secondary_foreground = text.clone();
    c.primary = primary.map(hex);
    c.primary_foreground = primary_foreground;
    c.link = primary.map(hex);

    c.popover = zed.hex(&["elevated_surface.background"]);
    c.popover_foreground = text.clone();
    c.sidebar = zed.hex(&["panel.background"]);
    c.sidebar_foreground = text.clone();
    c.sidebar_border = border.clone();
    c.sidebar_accent = zed.hex(&["ghost_element.hover", "element.hover"]);
    c.sidebar_accent_foreground = text.clone();
    c.list = zed.hex(&["panel.background"]);
    c.list_hover = zed.hex(&["ghost_element.hover", "element.hover"]);
    c.list_active = zed.hex(&["ghost_element.selected", "element.selected"]);
    c.list_active_border = zed.hex(&["border.selected", "border.focused"]);

    c.title_bar = zed.hex(&["title_bar.background"]);
    c.title_bar_border = border.clone();
    c.status_bar = zed.hex(&["status_bar.background"]);
    c.status_bar_border = border;
    c.tab_bar = zed.hex(&["tab_bar.background"]);
    c.tab = zed.hex(&["tab.inactive_background"]);
    c.tab_active = zed.hex(&["tab.active_background"]);
    c.tab_foreground = zed.hex(&["text.muted"]);
    c.tab_active_foreground = text;

    c.scrollbar = zed.hex(&["scrollbar.track.background"]);
    c.scrollbar_thumb = zed.hex(&["scrollbar.thumb.background"]);
    c.scrollbar_thumb_hover = zed.hex(&["scrollbar.thumb.hover_background"]);
    c.selection = zed.hex(&["players[0].selection"]);
    c.caret = zed.hex(&["players[0].cursor"]);

    // Zed has no key for a modal's backdrop. gpui-kit's own default (20% black on dark,
    // 5% on light) barely shows on a window that is dark to begin with, so a dialog would
    // not read as modal.
    c.overlay = Some(SharedString::from(if zed.dark {
        DARK_OVERLAY
    } else {
        LIGHT_OVERLAY
    }));

    c.success = zed.hex(&["success"]);
    c.danger = zed.hex(&["error"]);
    c.warning = zed.hex(&["warning"]);
    c.info = zed.hex(&["info"]);
    let colors = c;

    ThemeConfig {
        is_default: false,
        name: SharedString::from(zed.name.to_owned()),
        mode: if zed.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        font_size: Some(f32::from(ui.size)),
        font_family: Some(ui.font.family.clone()),
        mono_font_family: Some(terminal.font.family.clone()),
        mono_font_size: Some(f32::from(terminal.size)),
        colors,
        highlight: Some(HighlightThemeStyle {
            editor_background: zed.first(&["editor.background"]),
            editor_foreground: zed.first(&["editor.foreground"]),
            ..HighlightThemeStyle::default()
        }),
        ..ThemeConfig::default()
    }
}

/// Install `config` as gpui-kit's theme, switching to its mode, and repaint every
/// window.
pub fn apply_kit_theme(config: ThemeConfig, cx: &mut App) {
    let config = Rc::new(config);
    Theme::update(cx, |theme| theme.apply_config(&config));
}

#[cfg(test)]
mod tests {
    use serialist_core::theme::ThemeRegistry;

    use super::*;
    use crate::config::hsla;
    use crate::terminal::palette::MINIMUM_CONTRAST;

    #[test]
    fn hex_round_trips_through_gpui_colors() {
        let color = Hsla::from(rgba(0x3b414dff));
        assert_eq!(hex(color).as_ref(), "#3b414dff");
        assert_eq!(hex(Hsla::from(rgba(0x74ade840))).as_ref(), "#74ade840");
    }

    /// WCAG's level for UI components and large text.
    const COMPONENT_CONTRAST: f32 = 3.0;

    /// The surfaces the chrome draws text on.
    const SURFACES: [&str; 10] = [
        "background",
        "surface.background",
        "elevated_surface.background",
        "panel.background",
        "title_bar.background",
        "status_bar.background",
        "tab_bar.background",
        "tab.inactive_background",
        "tab.active_background",
        "toolbar.background",
    ];

    /// Buttons, inputs and selected rows, which body text also sits on.
    const ELEMENTS: [&str; 4] = [
        "element.background",
        "element.hover",
        "element.active",
        "element.selected",
    ];

    /// The floors a bundled theme's chrome is held to, for text (body, muted, links and
    /// icons), for the status colors (connection state, script results, chips) and for
    /// the focus ring: text to AA and the focus ring to the level for UI components. The
    /// status colors are text too, except in Serialist Dark and Light, which hold them to
    /// the components' level.
    fn floors(name: &str) -> (f32, f32, f32) {
        match name {
            "Serialist Dark" | "Serialist Light" => {
                (MINIMUM_CONTRAST, COMPONENT_CONTRAST, COMPONENT_CONTRAST)
            }
            _ => (MINIMUM_CONTRAST, MINIMUM_CONTRAST, COMPONENT_CONTRAST),
        }
    }

    fn parse(hex: &SharedString) -> Hsla {
        let value = u32::from_str_radix(hex.trim_start_matches('#'), 16).expect("a hex color");
        Hsla::from(rgba(value))
    }

    /// `top` composited over an opaque `under`.
    fn over(top: Hsla, under: Hsla) -> Hsla {
        let (t, u) = (top.to_rgb(), under.to_rgb());
        let mix = |a: f32, b: f32| a * t.a + b * (1.0 - t.a);
        Hsla::from(Rgba {
            r: mix(t.r, u.r),
            g: mix(t.g, u.g),
            b: mix(t.b, u.b),
            a: 1.0,
        })
    }

    #[test]
    fn every_bundled_theme_reads_in_the_chrome() {
        let registry = ThemeRegistry::bundled();
        for theme in registry.themes() {
            let name = theme.name.as_str();
            let (text_floor, status_floor, ring_floor) = floors(name);
            let lookup = |key: &str| theme.color(key).map(hsla);
            let color = |key: &str| lookup(key).unwrap_or_else(|| panic!("{name} lacks {key}"));
            let check = |what: &str, fg: Hsla, behind: &str, bg: Hsla, floor: f32| {
                let ratio = contrast_ratio(fg, bg);
                assert!(
                    ratio >= floor,
                    "{name}: {what} on {behind} reads at {ratio:.2}, below {floor}"
                );
            };
            for surface in SURFACES {
                let bg = color(surface);
                for key in ["text", "text.muted", "text.accent", "icon"] {
                    check(key, color(key), surface, bg, text_floor);
                }
                for key in ["success", "warning", "error", "info"] {
                    check(key, color(key), surface, bg, status_floor);
                }
                for key in ["border.focused", "panel.focused_border"] {
                    check(key, color(key), surface, bg, ring_floor);
                }
            }
            for element in ELEMENTS {
                check("text", color("text"), element, color(element), text_floor);
            }
            // Hovered and selected list rows are translucent over the panel.
            for ghost in ["ghost_element.hover", "ghost_element.selected"] {
                let bg = over(color(ghost), color("panel.background"));
                check("text", color("text"), ghost, bg, text_floor);
            }

            // The primary button's label, which the bridge picks.
            let config = kit_theme_config(
                &ZedColors {
                    name,
                    dark: theme.is_dark(),
                    lookup: &lookup,
                },
                &UiFont::default(),
                &TerminalFont::default(),
            );
            let primary = parse(config.colors.primary.as_ref().expect("primary"));
            let label = parse(
                config
                    .colors
                    .primary_foreground
                    .as_ref()
                    .expect("primary foreground"),
            );
            check("the primary label", label, "primary", primary, text_floor);
        }
    }
}
