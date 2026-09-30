//! The pieces the window's chrome is built from, so every panel, toolbar and list shares
//! one rhythm: an 8 px grid, 28 px rows and headers, 11 px uppercase labels in the
//! theme's muted text, icon buttons that are ghost until pressed, and borders only where a
//! dock or a toolbar ends (the theme's `border`, which the bridge fills from Zed's
//! `border.variant`).

use crate::prelude::*;

/// The height of every list row: devices, commands, scripts, decoded frames.
pub const ROW_HEIGHT: Pixels = px(28.);
/// The height of a panel header and of the session toolbar.
pub const HEADER_HEIGHT: Pixels = px(28.);
/// The height of the status bar.
pub const STATUS_HEIGHT: Pixels = px(24.);
/// The size of a header label and of a chip's text.
pub const LABEL_SIZE: Pixels = px(11.);
/// The side of an icon button in a toolbar, a header or a row.
pub const ICON_BUTTON: Pixels = px(24.);

/// An 11 px uppercase label in the muted text color, for a panel's or a group's name.
pub fn section_label(text: &str, cx: &App) -> Div {
    div()
        .flex_none()
        .text_size(LABEL_SIZE)
        .font_semibold()
        .line_height(px(16.))
        .text_color(cx.theme().muted_foreground)
        .child(SharedString::from(text.to_uppercase()))
}

/// A panel's header row: its label on the left; add the actions with `.child`, and
/// push them right with an `ml_auto` on the first.
pub fn panel_header(title: &str, cx: &App) -> Div {
    h_flex()
        .flex_none()
        .w_full()
        .h(HEADER_HEIGHT)
        .pl_3()
        .pr_1()
        .gap_2()
        .items_center()
        .child(section_label(title, cx))
}

/// A ghost icon button in the muted text color. Pair it with a `tooltip` or
/// `tooltip_with_action`: an icon alone does not say what it does.
pub fn icon_button(id: impl Into<ElementId>, icon: IconName, cx: &App) -> Button {
    Button::new(id)
        .icon(icon)
        .ghost()
        .small()
        .text_color(cx.theme().muted_foreground)
}

/// An icon button that shows a state: pressed (the theme's selected element color, in
/// the foreground color) while `on`.
pub fn toggle_button(id: impl Into<ElementId>, icon: IconName, on: bool, cx: &App) -> Button {
    let button = Button::new(id)
        .icon(icon)
        .ghost()
        .small()
        .selected(on)
        .toggled(on);
    if on {
        button.text_color(cx.theme().foreground)
    } else {
        button.text_color(cx.theme().muted_foreground)
    }
}

/// The background of a row's hover actions, laid over the row's right end: the row's
/// own hover (or selected) color, made opaque over the panel so nothing shows through.
pub fn overlay_background(selected: bool, cx: &App) -> Hsla {
    let theme = cx.theme();
    let wash = if selected {
        theme.list_active
    } else {
        theme.list_hover
    };
    theme.sidebar.blend(wash)
}

/// A connection-state dot: filled, or a ring for a state with nothing behind it
/// (a lost device, a port not opened yet).
pub fn state_dot(color: Hsla, hollow: bool) -> Div {
    div().flex_none().size_2().rounded_full().map(|dot| {
        if hollow {
            dot.border_1().border_color(color)
        } else {
            dot.bg(color)
        }
    })
}

/// A small rounded tag in `color` on a faint wash of it: add its text (and an icon).
pub fn chip(color: Hsla) -> Div {
    h_flex()
        .flex_none()
        .h(px(16.))
        .px_1p5()
        .gap_1()
        .items_center()
        .rounded(px(4.))
        .bg(color.opacity(0.14))
        .text_size(LABEL_SIZE)
        .line_height(px(14.))
        .text_color(color)
}

/// A tag in the muted text color with a hairline border, for secondary facts (a
/// profile, "examples", a keybinding).
pub fn quiet_chip(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h(px(16.))
        .px_1()
        .items_center()
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border)
        .text_size(LABEL_SIZE)
        .line_height(px(14.))
        .text_color(theme.muted_foreground)
        .child(text.into())
}

/// A thin vertical rule between groups of a toolbar, with 6 px on each side (8 px from
/// the icons' own edges to the rule's, counting their padding).
pub fn toolbar_separator(cx: &App) -> Div {
    div()
        .flex_none()
        .mx_1p5()
        .w(px(1.))
        .h(px(16.))
        .bg(cx.theme().border)
}

/// How a keystroke reads on this platform (`⌘⇧P`, `Ctrl+Shift+P`).
pub fn keystroke_text(binding: &KeyBinding) -> Option<String> {
    let texts: Vec<String> = binding
        .keystrokes()
        .iter()
        .map(|key| Kbd::format(key.as_keystroke()))
        .collect();
    (!texts.is_empty()).then(|| texts.join(" "))
}

/// The keystrokes that run `action` where `focus` is, as text, if any are bound.
pub fn binding_text_in(
    action: &dyn Action,
    focus: &FocusHandle,
    window: &Window,
) -> Option<String> {
    window
        .highest_precedence_binding_for_action_in(action, focus)
        .as_ref()
        .and_then(keystroke_text)
}
