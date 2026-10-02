//! The small form behind "Connect to TCP…": one field for `host:port`.
//!
//! The field takes `host:port` (a DNS name, an IPv4 address, or an IPv6 address in
//! brackets) and also a pasted `tcp:host:port`, which is what the port id and the
//! status line call the endpoint. What is typed is parsed with
//! [`TcpAddress::from_port_id`], the grammar the transport itself opens by, so the form
//! accepts exactly what would connect. Confirming (Enter, which is the dialog's own Enter
//! binding, or the Connect button the prompt ends in) emits
//! [`TcpPromptEvent::Confirmed`] with the address when it parses; when it does not, the
//! [`AddressError`]'s text shows under the field and the dialog stays open. The error
//! clears as soon as the text changes. Cancel, Escape and the dialog's close button
//! dismiss it.

use serialist_core::{AddressError, PortId, TcpAddress};

use crate::dialog_footer::DialogButtons;
use crate::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TcpPromptEvent {
    Confirmed(TcpAddress),
}

/// Parse what the field holds: `host:port`, or the same with a `tcp:` in front.
pub fn parse_endpoint(text: &str) -> Result<TcpAddress, AddressError> {
    let text = text.trim();
    let text = text.strip_prefix("tcp:").unwrap_or(text);
    TcpAddress::from_port_id(&PortId::new(format!("tcp:{text}")))
}

pub struct TcpPrompt {
    input: Entity<InputState>,
    /// Why the last confirm did not connect.
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<TcpPromptEvent> for TcpPrompt {}

impl TcpPrompt {
    /// An empty prompt.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("host:port, for example 192.168.1.5:4000")
        });
        // A new edit is a new try: the old complaint no longer applies. Enter reaches the
        // dialog's own Enter binding, whose OK handler confirms.
        let changes = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
            if let InputEvent::Change = event
                && this.error.take().is_some()
            {
                cx.notify();
            }
        });
        Self {
            input,
            error: None,
            _subscriptions: vec![changes],
        }
    }

    /// Put the focus in the field. The workspace calls it once the dialog is open: a
    /// dialog takes the focus itself as it opens, so asking before would be lost.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// What is typed in the field.
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// The text under the field saying why the last confirm did not connect.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Emit the address if the field holds one. Returns whether it did.
    pub fn confirm(&mut self, cx: &mut Context<Self>) -> bool {
        match parse_endpoint(&self.text(cx)) {
            Ok(address) => {
                self.error = None;
                cx.emit(TcpPromptEvent::Confirmed(address));
                true
            }
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                false
            }
        }
    }
}

impl Render for TcpPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("tcp-prompt")
            .gap_2()
            .child(
                div()
                    .font_family(theme.mono_font_family.clone())
                    .child(Input::new(&self.input).id("tcp-endpoint").small()),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .id("tcp-prompt-error")
                    .text_xs()
                    .text_color(theme.danger)
                    .child(SharedString::from(error))
            }))
            // The dialog is opened by the workspace, which sets no footer of its own, so
            // the prompt ends in its own: Connect dispatches the dialog's Confirm, as
            // Enter does, and Cancel its Cancel.
            .child(div().mt_2().child(DialogButtons::new("Connect")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_takes_host_port_with_or_without_the_scheme() {
        let expected = TcpAddress::new("192.168.1.5", 4000);
        assert_eq!(parse_endpoint("192.168.1.5:4000"), Ok(expected.clone()));
        assert_eq!(
            parse_endpoint("  192.168.1.5:4000 \n"),
            Ok(expected.clone())
        );
        assert_eq!(parse_endpoint("tcp:192.168.1.5:4000"), Ok(expected.clone()));
        assert_eq!(parse_endpoint("tcp://192.168.1.5:4000"), Ok(expected));
        assert_eq!(
            parse_endpoint("bridge.local:2323"),
            Ok(TcpAddress::new("bridge.local", 2323))
        );
        assert_eq!(
            parse_endpoint("[::1]:4000"),
            Ok(TcpAddress::new("::1", 4000))
        );
    }

    #[test]
    fn what_cannot_connect_is_an_address_error() {
        for bad in [
            "",
            "host",
            "host:",
            ":4000",
            "host:0",
            "host:99999",
            "ho st:1",
        ] {
            assert!(parse_endpoint(bad).is_err(), "{bad:?} should not parse");
        }
        let error = parse_endpoint("host").unwrap_err().to_string();
        assert!(
            error.contains("tcp port id is tcp:<host>:<port>"),
            "{error}"
        );
    }
}
