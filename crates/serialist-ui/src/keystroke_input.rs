//! A field that records a keystroke by having it pressed, as Zed's keystroke input does:
//! click it (or [`KeystrokeInput::start`]), press the chord, and it emits
//! [`KeystrokeInputEvent::Captured`].
//!
//! While it records, it sees every keystroke before the key bindings do (GPUI's
//! keystroke interceptors), so a chord that is bound elsewhere, `cmd-w` say, is recorded
//! rather than run. Modifier keys alone are not keystrokes; the first key pressed with
//! them is. Clicking the field again, or moving the focus away, stops recording
//! without a capture.

use crate::chrome;
use crate::prelude::*;

/// What a [`KeystrokeInput`] tells its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeystrokeInputEvent {
    /// A keystroke was pressed while recording.
    Captured(Keystroke),
}

/// Keys GPUI reports for a modifier pressed and released on its own.
const MODIFIER_KEYS: [&str; 10] = [
    "shift", "control", "ctrl", "alt", "platform", "cmd", "function", "fn", "super", "win",
];

pub struct KeystrokeInput {
    id: SharedString,
    keystroke: Option<Keystroke>,
    recording: bool,
    focus_handle: FocusHandle,
    _intercept: Option<Subscription>,
    _blur: Option<Subscription>,
}

impl EventEmitter<KeystrokeInputEvent> for KeystrokeInput {}

impl Focusable for KeystrokeInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl KeystrokeInput {
    /// An input with the element id `id`, showing `keystroke`.
    pub fn new(id: impl Into<SharedString>, keystroke: Option<Keystroke>, cx: &mut Context<Self>) -> Self {
        Self {
            id: id.into(),
            keystroke,
            recording: false,
            focus_handle: cx.focus_handle(),
            _intercept: None,
            _blur: None,
        }
    }

    pub fn keystroke(&self) -> Option<&Keystroke> {
        self.keystroke.as_ref()
    }

    pub fn is_recording(&self) -> bool {
        self.recording
    }

    /// Show `keystroke`, emitting nothing.
    pub fn set_keystroke(&mut self, keystroke: Option<Keystroke>, cx: &mut Context<Self>) {
        if self.keystroke != keystroke {
            self.keystroke = keystroke;
            cx.notify();
        }
    }

    /// Focus the field and record the next keystroke.
    pub fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle, cx);
        if self.recording {
            return;
        }
        self.recording = true;
        let this = cx.entity().downgrade();
        let focus = self.focus_handle.clone();
        self._intercept = Some(cx.intercept_keystrokes(move |event, window, cx| {
            if !focus.is_focused(window) {
                return;
            }
            let keystroke = event.keystroke.clone();
            if MODIFIER_KEYS.contains(&keystroke.key.as_str()) {
                return;
            }
            cx.stop_propagation();
            this.update(cx, |this, cx| this.capture(keystroke, cx)).ok();
        }));
        self._blur = Some(cx.on_blur(&self.focus_handle, window, |this, _, cx| {
            this.stop(cx);
        }));
        cx.notify();
    }

    /// Stop recording without a capture.
    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if self.recording {
            self.recording = false;
            self._intercept = None;
            self._blur = None;
            cx.notify();
        }
    }

    fn capture(&mut self, keystroke: Keystroke, cx: &mut Context<Self>) {
        tracing::debug!(keystroke = %keystroke.unparse(), "recorded a keystroke");
        self.keystroke = Some(keystroke.clone());
        self.stop(cx);
        cx.emit(KeystrokeInputEvent::Captured(keystroke));
    }
}

impl Render for KeystrokeInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let recording = self.recording;
        let border = if recording { theme.ring } else { theme.input };
        let content: AnyElement = if recording {
            div()
                .text_color(theme.muted_foreground)
                .child("Press a key\u{2026}")
                .into_any_element()
        } else {
            match &self.keystroke {
                Some(keystroke) => Kbd::new(keystroke.clone()).into_any_element(),
                None => div()
                    .text_color(theme.muted_foreground)
                    .child("Click to record")
                    .into_any_element(),
            }
        };
        h_flex()
            .id(ElementId::Name(self.id.clone()))
            .test_support()
            .track_focus(&self.focus_handle)
            .h(chrome::ROW_HEIGHT)
            .min_w(px(120.))
            .px_2()
            .gap_2()
            .items_center()
            .rounded(theme.radius)
            .border_1()
            .border_color(border)
            .bg(theme.background)
            .text_sm()
            .cursor_pointer()
            .child(content)
            .on_click(cx.listener(|this, _, window, cx| {
                if this.recording {
                    this.stop(cx);
                } else {
                    this.start(window, cx);
                }
            }))
    }
}
