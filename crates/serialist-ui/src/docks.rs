//! The two docks around the session: which panels are open, how wide each dock is, and
//! how much of that the window leaves room for.
//!
//! Each dock sits next to a 36 px rail of icons, one per panel: Devices and Commands on
//! the left, Decoded and Scripts on the right. Clicking an icon opens or closes its
//! panel; a dock with no panel open is its rail alone. Devices and Commands start open;
//! the workspace opens Decoded when the active session gets a codec (and closes it when
//! the session has none), and Scripts when a script runs or the user opens it.
//!
//! Narrow windows collapse the docks to their rails: the right one below
//! [`RIGHT_BREAKPOINT`], the left one below [`LEFT_BREAKPOINT`]. Collapsing is what
//! crossing the breakpoint does, not a rule: a panel opened from the rail while the
//! window is narrow stays open, and widening the window past the breakpoint brings back
//! what the narrowing collapsed. Whatever is open, the center keeps [`CENTER_MIN`]: the
//! docks give up width to it (the right one first), and a dock that cannot keep its
//! minimum stays on its rail.
//!
//! No GPUI here beyond `Pixels`, so the arithmetic tests as plain Rust. The widths are
//! kept in `state.json` with the tabs (see [`session_state`](crate::session_state)).

use serde::{Deserialize, Serialize};

use crate::prelude::*;

/// The width of a dock's rail.
pub const RAIL_WIDTH: Pixels = px(36.);
/// Below this window width the right dock collapses to its rail.
pub const RIGHT_BREAKPOINT: Pixels = px(1100.);
/// Below this window width the left dock collapses to its rail.
pub const LEFT_BREAKPOINT: Pixels = px(900.);
/// The narrowest the center (the session and its toolbar) gets.
pub const CENTER_MIN: Pixels = px(480.);

pub const LEFT_DEFAULT: f32 = 280.;
pub const LEFT_MIN: f32 = 220.;
pub const LEFT_MAX: f32 = 520.;
pub const RIGHT_DEFAULT: f32 = 360.;
pub const RIGHT_MIN: f32 = 260.;
pub const RIGHT_MAX: f32 = 960.;

/// A panel in one of the docks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockPanel {
    Devices,
    Commands,
    Decoded,
    Scripts,
}

impl DockPanel {
    pub const ALL: [DockPanel; 4] = [
        DockPanel::Devices,
        DockPanel::Commands,
        DockPanel::Decoded,
        DockPanel::Scripts,
    ];

    pub fn side(self) -> DockSide {
        match self {
            DockPanel::Devices | DockPanel::Commands => DockSide::Left,
            DockPanel::Decoded | DockPanel::Scripts => DockSide::Right,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            DockPanel::Devices => "Devices",
            DockPanel::Commands => "Commands",
            DockPanel::Decoded => "Decoded",
            DockPanel::Scripts => "Scripts",
        }
    }

    /// The id of its rail button, for tests that click it.
    pub fn rail_id(self) -> &'static str {
        match self {
            DockPanel::Devices => "rail-devices",
            DockPanel::Commands => "rail-commands",
            DockPanel::Decoded => "rail-decoded",
            DockPanel::Scripts => "rail-scripts",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DockSide {
    Left,
    Right,
}

/// What `state.json` keeps of the docks: their widths, and which left panels are open.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedDocks {
    pub left_width: f32,
    pub right_width: f32,
    #[serde(default = "yes")]
    pub devices: bool,
    #[serde(default = "yes")]
    pub commands: bool,
}

fn yes() -> bool {
    true
}

/// The docks' state: the panels open, the widths the user dragged them to, and what the
/// window width collapsed.
#[derive(Clone, Debug, PartialEq)]
pub struct Docks {
    devices: bool,
    commands: bool,
    decoded: bool,
    scripts: bool,
    /// The width the left dock takes when there is room, as last dragged.
    left_width: f32,
    right_width: f32,
    /// Collapsed by the window narrowing past the side's breakpoint.
    left_collapsed: bool,
    right_collapsed: bool,
    /// The window width last seen, to tell a crossing from a resize on one side.
    window_width: Option<f32>,
}

impl Default for Docks {
    fn default() -> Self {
        Self {
            devices: true,
            commands: true,
            decoded: false,
            scripts: false,
            left_width: LEFT_DEFAULT,
            right_width: RIGHT_DEFAULT,
            left_collapsed: false,
            right_collapsed: false,
            window_width: None,
        }
    }
}

/// The widths the docks take in a window, `None` for a dock on its rail alone.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DockWidths {
    pub left: Option<Pixels>,
    pub right: Option<Pixels>,
}

impl Docks {
    /// The docks as `saved` left them.
    pub fn restored(saved: &SavedDocks) -> Self {
        Self {
            devices: saved.devices,
            commands: saved.commands,
            left_width: saved.left_width.clamp(LEFT_MIN, LEFT_MAX),
            right_width: saved.right_width.clamp(RIGHT_MIN, RIGHT_MAX),
            ..Self::default()
        }
    }

    pub fn saved(&self) -> SavedDocks {
        SavedDocks {
            left_width: self.left_width,
            right_width: self.right_width,
            devices: self.devices,
            commands: self.commands,
        }
    }

    pub fn is_open(&self, panel: DockPanel) -> bool {
        match panel {
            DockPanel::Devices => self.devices,
            DockPanel::Commands => self.commands,
            DockPanel::Decoded => self.decoded,
            DockPanel::Scripts => self.scripts,
        }
    }

    fn open_flag(&mut self, panel: DockPanel) -> &mut bool {
        match panel {
            DockPanel::Devices => &mut self.devices,
            DockPanel::Commands => &mut self.commands,
            DockPanel::Decoded => &mut self.decoded,
            DockPanel::Scripts => &mut self.scripts,
        }
    }

    fn collapsed_flag(&mut self, side: DockSide) -> &mut bool {
        match side {
            DockSide::Left => &mut self.left_collapsed,
            DockSide::Right => &mut self.right_collapsed,
        }
    }

    /// Open or close `panel`. Opening one uncollapses its dock: the user asked to see it.
    /// Returns whether anything changed.
    pub fn set_open(&mut self, panel: DockPanel, open: bool) -> bool {
        let changed = *self.open_flag(panel) != open;
        *self.open_flag(panel) = open;
        if open && std::mem::take(self.collapsed_flag(panel.side())) {
            return true;
        }
        changed
    }

    /// Open or close `panel` for the app (a codec appearing, a script starting). Unlike
    /// [`Self::set_open`], this leaves a dock the window collapsed on its rail.
    pub fn show_by_app(&mut self, panel: DockPanel, open: bool) -> bool {
        let changed = *self.open_flag(panel) != open;
        *self.open_flag(panel) = open;
        changed
    }

    /// The rail's click: close `panel` if it shows, else open it.
    pub fn toggle(&mut self, panel: DockPanel) {
        let shown = self.is_shown(panel);
        self.set_open(panel, !shown);
    }

    /// Whether `panel` is on screen: open, in a dock the window has not collapsed.
    pub fn is_shown(&self, panel: DockPanel) -> bool {
        self.is_open(panel) && !self.is_collapsed(panel.side())
    }

    /// Whether the window width collapsed the dock on `side`.
    pub fn is_collapsed(&self, side: DockSide) -> bool {
        match side {
            DockSide::Left => self.left_collapsed,
            DockSide::Right => self.right_collapsed,
        }
    }

    fn has_open(&self, side: DockSide) -> bool {
        match side {
            DockSide::Left => self.devices || self.commands,
            DockSide::Right => self.decoded || self.scripts,
        }
    }

    /// Whether the dock on `side` shows a panel (before the center's minimum is taken).
    pub fn is_expanded(&self, side: DockSide) -> bool {
        self.has_open(side) && !self.is_collapsed(side)
    }

    /// The window is `width` wide: collapse a dock whose breakpoint it went below, and
    /// bring back one whose breakpoint it went above. Returns whether anything changed.
    pub fn observe_window_width(&mut self, width: Pixels) -> bool {
        let width = f32::from(width);
        let before = self.window_width.replace(width);
        if before == Some(width) {
            return false;
        }
        let mut changed = false;
        for (side, breakpoint) in [
            (DockSide::Left, f32::from(LEFT_BREAKPOINT)),
            (DockSide::Right, f32::from(RIGHT_BREAKPOINT)),
        ] {
            let narrow = width < breakpoint;
            let was_narrow = before.map(|before| before < breakpoint);
            if was_narrow != Some(narrow) {
                let flag = self.collapsed_flag(side);
                changed |= *flag != narrow;
                *flag = narrow;
            }
        }
        changed
    }

    /// The width the left dock takes when there is room.
    pub fn left_width(&self) -> Pixels {
        px(self.left_width)
    }

    pub fn right_width(&self) -> Pixels {
        px(self.right_width)
    }

    /// Drag the dock on `side` to `width`, within its range.
    pub fn resize(&mut self, side: DockSide, width: Pixels) {
        let width = f32::from(width);
        match side {
            DockSide::Left => self.left_width = width.clamp(LEFT_MIN, LEFT_MAX),
            DockSide::Right => self.right_width = width.clamp(RIGHT_MIN, RIGHT_MAX),
        }
    }

    /// The widths the docks take in a window `window_width` wide: each expanded dock at
    /// its width, given up to the center's minimum (the right dock first), and a dock
    /// that cannot keep its own minimum left on its rail.
    pub fn widths(&self, window_width: Pixels) -> DockWidths {
        let room =
            (f32::from(window_width) - 2. * f32::from(RAIL_WIDTH) - f32::from(CENTER_MIN)).max(0.);
        let mut left = self.is_expanded(DockSide::Left).then_some(self.left_width);
        let mut right = self
            .is_expanded(DockSide::Right)
            .then_some(self.right_width);
        let used = |left: Option<f32>, right: Option<f32>| {
            left.unwrap_or_default() + right.unwrap_or_default()
        };
        if used(left, right) > room
            && let Some(width) = right
        {
            let fits = room - left.unwrap_or_default();
            right = (fits >= RIGHT_MIN).then_some(width.min(fits));
        }
        if used(left, right) > room
            && let Some(width) = left
        {
            let fits = room - right.unwrap_or_default();
            left = (fits >= LEFT_MIN).then_some(width.min(fits));
        }
        DockWidths {
            left: left.map(px),
            right: right.map(px),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_and_commands_start_open_and_the_right_dock_closed() {
        let mut docks = Docks::default();
        docks.observe_window_width(px(1440.));
        assert!(docks.is_shown(DockPanel::Devices));
        assert!(docks.is_shown(DockPanel::Commands));
        assert!(!docks.is_shown(DockPanel::Decoded));
        assert!(!docks.is_shown(DockPanel::Scripts));
        assert_eq!(
            docks.widths(px(1440.)),
            DockWidths {
                left: Some(px(LEFT_DEFAULT)),
                right: None
            }
        );
    }

    #[test]
    fn narrowing_past_the_breakpoints_collapses_and_widening_restores() {
        let mut docks = Docks::default();
        docks.set_open(DockPanel::Decoded, true);
        docks.observe_window_width(px(1440.));
        assert!(docks.is_shown(DockPanel::Decoded));

        assert!(docks.observe_window_width(px(1024.)));
        assert!(
            !docks.is_shown(DockPanel::Decoded),
            "right collapses below 1100"
        );
        assert!(
            docks.is_open(DockPanel::Decoded),
            "but stays open underneath"
        );
        assert!(docks.is_shown(DockPanel::Devices), "left stays at 1024");

        docks.observe_window_width(px(880.));
        assert!(
            !docks.is_shown(DockPanel::Devices),
            "left collapses below 900"
        );
        assert_eq!(
            docks.widths(px(880.)),
            DockWidths {
                left: None,
                right: None
            }
        );

        docks.observe_window_width(px(1200.));
        assert!(docks.is_shown(DockPanel::Devices));
        assert!(docks.is_shown(DockPanel::Decoded));
    }

    #[test]
    fn a_window_that_opens_narrow_starts_collapsed() {
        let mut docks = Docks::default();
        docks.observe_window_width(px(1000.));
        docks.set_open(DockPanel::Scripts, true);
        assert!(
            docks.is_shown(DockPanel::Scripts),
            "opening from the rail wins"
        );
        let mut docks = Docks::default();
        docks.set_open(DockPanel::Scripts, true);
        docks.observe_window_width(px(1000.));
        assert!(!docks.is_shown(DockPanel::Scripts));
        docks.toggle(DockPanel::Scripts);
        assert!(
            docks.is_shown(DockPanel::Scripts),
            "the rail opens it again"
        );
        docks.toggle(DockPanel::Scripts);
        assert!(!docks.is_open(DockPanel::Scripts));
    }

    #[test]
    fn the_app_opening_a_panel_leaves_a_collapsed_dock_alone() {
        let mut docks = Docks::default();
        docks.observe_window_width(px(1000.));
        assert!(docks.show_by_app(DockPanel::Decoded, true));
        assert!(docks.is_open(DockPanel::Decoded));
        assert!(
            !docks.is_shown(DockPanel::Decoded),
            "the narrow window wins"
        );
        docks.observe_window_width(px(1300.));
        assert!(docks.is_shown(DockPanel::Decoded));
    }

    #[test]
    fn the_center_keeps_its_minimum() {
        let mut docks = Docks::default();
        docks.set_open(DockPanel::Scripts, true);
        docks.observe_window_width(px(1000.));
        docks.set_open(DockPanel::Scripts, true);
        // 1000 - 72 rails - 480 center = 448 for both docks: the left keeps 280, the
        // right cannot keep 260 in the 168 left over.
        let widths = docks.widths(px(1000.));
        assert_eq!(widths.left, Some(px(280.)));
        assert_eq!(widths.right, None);
        // A little wider: the right takes what is left.
        let widths = docks.widths(px(1100.));
        assert_eq!(widths.right, Some(px(268.)));
        for width in [900., 1024., 1100., 1280., 1440.] {
            let widths = docks.widths(px(width));
            let docked = f32::from(widths.left.unwrap_or_default())
                + f32::from(widths.right.unwrap_or_default());
            assert!(width - 72. - docked >= 480., "{width}: {widths:?}");
        }
    }

    #[test]
    fn widths_are_clamped_and_saved() {
        let mut docks = Docks::default();
        docks.resize(DockSide::Left, px(90.));
        docks.resize(DockSide::Right, px(2000.));
        assert_eq!(docks.left_width(), px(LEFT_MIN));
        assert_eq!(docks.right_width(), px(RIGHT_MAX));
        docks.resize(DockSide::Left, px(333.));
        docks.set_open(DockPanel::Commands, false);
        let saved = docks.saved();
        let json = serde_json::to_string(&saved).unwrap();
        let back: SavedDocks = serde_json::from_str(&json).unwrap();
        let restored = Docks::restored(&back);
        assert_eq!(restored.left_width(), px(333.));
        assert!(!restored.is_open(DockPanel::Commands));
        assert!(restored.is_open(DockPanel::Devices));
    }
}
