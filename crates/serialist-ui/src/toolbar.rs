//! Which of the session toolbar's controls fit, and which go to its overflow menu.
//!
//! The toolbar is one 28 px row of groups, with a thin rule between them: the
//! connection (a state dot, the line settings that open the port settings, Connect or
//! Disconnect), the mode (Command / Inline), capture (Pause, Record, Clear), the view
//! (Search, Hex, Timestamps, Wrap), Export, and the codec menu when a codec besides
//! `none` is loaded. The connection always shows. When the rest does not fit the width
//! the toolbar has, controls leave for the overflow menu at the right end, in
//! [`ToolbarItem::OVERFLOW_ORDER`] (the view toggles first, the mode last), until what
//! stays fits beside the menu's button. Nothing is ever cut off.
//!
//! Widths are estimates from the control kinds and the label lengths, so the layout is
//! decided before anything is drawn and needs no second pass.

/// A control that can move to the overflow menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ToolbarItem {
    Mode,
    Pause,
    Record,
    Clear,
    Search,
    Hex,
    Timestamps,
    Wrap,
    Export,
    Codec,
}

use ToolbarItem::*;

impl ToolbarItem {
    /// The groups, in the order they show, left to right.
    pub const GROUPS: [&'static [ToolbarItem]; 5] = [
        &[Mode],
        &[Pause, Record, Clear],
        &[Search, Hex, Timestamps, Wrap],
        &[Export],
        &[Codec],
    ];

    /// The order controls leave for the overflow menu, first to go first.
    pub const OVERFLOW_ORDER: [ToolbarItem; 10] = [
        Wrap, Timestamps, Hex, Codec, Export, Search, Clear, Record, Pause, Mode,
    ];

    fn display_index(self) -> usize {
        Self::GROUPS
            .iter()
            .flat_map(|group| group.iter())
            .position(|item| *item == self)
            .unwrap_or(usize::MAX)
    }
}

/// An icon button's side.
pub const ICON: f32 = 24.;
/// Between the controls of a group.
pub const GAP: f32 = 2.;
/// A rule between groups, with 6 px either side.
pub const SEPARATOR: f32 = 13.;
/// The overflow menu's button and the gap before it.
pub const OVERFLOW: f32 = ICON + 8.;
/// The toolbar's padding, both sides.
pub const PADDING: f32 = 16.;

/// The estimated widths of the controls whose width depends on their labels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToolbarMetrics {
    /// The connection group: dot, settings button, Connect or Disconnect.
    pub connection: f32,
    /// The Command / Inline control.
    pub mode: f32,
    /// The codec menu's button, or `None` when only `none` is loaded (it does not show).
    pub codec: Option<f32>,
}

impl ToolbarMetrics {
    fn width(&self, item: ToolbarItem) -> f32 {
        match item {
            Mode => self.mode,
            Codec => self.codec.unwrap_or_default(),
            _ => ICON,
        }
    }

    /// The toolbar's width with `shown` in it (and the connection), padding included.
    pub fn width_of(&self, shown: &[ToolbarItem]) -> f32 {
        let mut width = PADDING + self.connection;
        for group in ToolbarItem::GROUPS {
            let items: Vec<ToolbarItem> = group
                .iter()
                .copied()
                .filter(|item| shown.contains(item))
                .collect();
            if items.is_empty() {
                continue;
            }
            width += SEPARATOR
                + items.iter().map(|item| self.width(*item)).sum::<f32>()
                + GAP * (items.len() - 1) as f32;
        }
        width
    }
}

/// What the toolbar shows and what its overflow menu holds, each in display order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolbarLayout {
    pub shown: Vec<ToolbarItem>,
    pub overflow: Vec<ToolbarItem>,
    /// The estimated width of what shows, the overflow button included.
    pub width: f32,
    /// The width it was laid out in.
    pub available: f32,
}

impl ToolbarLayout {
    pub fn shows(&self, item: ToolbarItem) -> bool {
        self.shown.contains(&item)
    }
}

/// Lay the toolbar out in `available` pixels.
pub fn lay_out(available: f32, metrics: &ToolbarMetrics) -> ToolbarLayout {
    let mut shown: Vec<ToolbarItem> = ToolbarItem::GROUPS
        .iter()
        .flat_map(|group| group.iter().copied())
        .filter(|item| *item != Codec || metrics.codec.is_some())
        .collect();
    let mut overflow = Vec::new();
    if metrics.width_of(&shown) > available {
        for item in ToolbarItem::OVERFLOW_ORDER {
            if metrics.width_of(&shown) + OVERFLOW <= available {
                break;
            }
            if let Some(ix) = shown.iter().position(|shown| *shown == item) {
                overflow.push(shown.remove(ix));
            }
        }
        overflow.sort_by_key(|item| item.display_index());
    }
    let width = metrics.width_of(&shown) + if overflow.is_empty() { 0. } else { OVERFLOW };
    ToolbarLayout {
        shown,
        overflow,
        width,
        available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const METRICS: ToolbarMetrics = ToolbarMetrics {
        connection: 130.,
        mode: 124.,
        codec: Some(120.),
    };

    #[test]
    fn everything_fits_a_wide_toolbar() {
        let layout = lay_out(1100., &METRICS);
        assert!(layout.overflow.is_empty());
        assert_eq!(layout.shown.len(), 10);
        assert!(layout.width <= 1100.);
    }

    #[test]
    fn the_codec_menu_shows_only_with_a_codec_to_pick() {
        let layout = lay_out(
            1100.,
            &ToolbarMetrics {
                codec: None,
                ..METRICS
            },
        );
        assert!(!layout.shows(Codec));
        assert!(!layout.overflow.contains(&Codec));
    }

    #[test]
    fn a_short_toolbar_moves_the_view_toggles_first_and_never_overflows() {
        let layout = lay_out(560., &METRICS);
        assert!(!layout.overflow.is_empty());
        assert_eq!(
            layout.overflow,
            [Hex, Timestamps, Wrap, Codec],
            "the view toggles, then the codec, in display order"
        );
        assert!(layout.shows(Mode) && layout.shows(Pause));
        for available in (200..1200).step_by(10) {
            let available = available as f32;
            let layout = lay_out(available, &METRICS);
            let fits = layout.width <= available;
            let nothing_left = layout.shown.is_empty();
            assert!(fits || nothing_left, "{available}: {layout:?}");
            let mut all = layout.shown.clone();
            all.extend(&layout.overflow);
            assert_eq!(all.len(), 10, "every control is somewhere");
        }
    }
}
