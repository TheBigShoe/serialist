//! The footer of the app's dialogs: a Cancel button and a primary one (Save, Send).
//!
//! gpui-component's plain [`Dialog`] draws no buttons of its own: `on_ok` and
//! `button_props` only configure what Enter and Escape do, and a visible footer is one the
//! caller builds (`Dialog::footer`). [`DialogButtons`] is that footer. Its buttons do what
//! the keys do: Save dispatches [`Confirm`] and Cancel dispatches [`Cancel`] on the dialog
//! they sit in, so the dialog's `on_ok` (which may veto closing, as a form with a bad value
//! does) and `on_cancel`/`on_close` handlers run exactly as they do for Enter and Escape.
//!
//! Each button carries a focus node of its own and dispatches from it, as gpui-component's
//! own footer does: a dispatch from the window's focus would miss the dialog whenever
//! something else held focus at the moment of the click.

use crate::prelude::*;

/// The id of the primary button, for tests that click it.
pub const OK_BUTTON: &str = "dialog-ok";
/// The id of the Cancel button.
pub const CANCEL_BUTTON: &str = "dialog-cancel";

/// Cancel and a primary button, right-aligned, for [`Dialog::footer`] (or the bottom of a
/// dialog's content, when the dialog is built where the footer cannot be reached).
#[derive(IntoElement)]
pub struct DialogButtons {
    ok: SharedString,
}

impl DialogButtons {
    /// A footer whose primary button says `ok` ("Save", "Send", "Create").
    pub fn new(ok: impl Into<SharedString>) -> Self {
        Self { ok: ok.into() }
    }
}

impl RenderOnce for DialogButtons {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let cancel = Anchor::new("dialog-cancel-anchor", window, cx);
        let ok = Anchor::new("dialog-ok-anchor", window, cx);
        DialogFooter::new()
            .child(
                Button::new(CANCEL_BUTTON)
                    .label("Cancel")
                    .child(cancel.element())
                    .on_click(move |_, window, cx| cancel.dispatch(&Cancel, window, cx)),
            )
            .child(
                Button::new(OK_BUTTON)
                    .primary()
                    .label(self.ok)
                    .child(ok.element())
                    .on_click(move |_, window, cx| {
                        ok.dispatch(&Confirm { secondary: false }, window, cx)
                    }),
            )
    }
}

/// A focus node inside a button, to dispatch an action from the dialog's own dispatch path.
#[derive(Clone)]
struct Anchor(FocusHandle);

impl Anchor {
    /// Created where the button renders, so the keyed state lives in that button's scope.
    fn new(key: &'static str, window: &mut Window, cx: &mut App) -> Self {
        Self(
            window
                .use_keyed_state(key, cx, |_, cx| cx.focus_handle())
                .read(cx)
                .clone(),
        )
    }

    /// A zero-size, out-of-flow node that tracks the handle: never hovered, so it takes no
    /// focus of its own and stays out of the Tab order.
    fn element(&self) -> impl IntoElement {
        div().absolute().size_0().track_focus(&self.0)
    }

    /// Dispatch `action` from the anchor; from the focused element if it was never drawn.
    fn dispatch(&self, action: &dyn Action, window: &mut Window, cx: &mut App) {
        if self.0.is_focused(window) || self.0.contains(&self.0, window) {
            self.0.dispatch_action(action, window, cx);
        } else {
            window.dispatch_action(action.boxed_clone(), cx);
        }
    }
}
