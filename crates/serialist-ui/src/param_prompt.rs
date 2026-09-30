//! The small form that asks for a saved command's parameters before it is sent.
//!
//! One input per [`Param`], labelled and prefilled with the value used last in this
//! session (the session view remembers them) or the parameter's default. Each value is
//! checked against its kind as the user types; confirming (Enter in any field, which is
//! the dialog's own Enter binding, or the dialog's Send button) emits
//! [`ParamPromptEvent::Confirmed`] with the values, and the workspace sends and closes
//! the dialog.

use serialist_core::{CommandRef, Param, ParamValues};

use crate::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParamPromptEvent {
    Confirmed {
        reference: CommandRef,
        values: ParamValues,
    },
}

pub struct ParamPrompt {
    reference: CommandRef,
    params: Vec<Param>,
    inputs: Vec<Entity<InputState>>,
    /// What is wrong with the values, by parameter.
    errors: Vec<Option<String>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ParamPromptEvent> for ParamPrompt {}

impl ParamPrompt {
    /// A prompt for `params` of the command `reference` names, starting from `values`
    /// (a parameter missing there starts from its default).
    pub fn new(
        reference: CommandRef,
        params: Vec<Param>,
        values: &ParamValues,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut inputs = Vec::with_capacity(params.len());
        let mut subscriptions = Vec::with_capacity(params.len());
        for param in &params {
            let value = values
                .get(&param.name)
                .map(str::to_owned)
                .or_else(|| param.default.clone())
                .unwrap_or_default();
            let placeholder = param.kind.label().to_owned();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            });
            // Enter reaches the dialog's own Enter binding, whose OK handler confirms.
            subscriptions.push(cx.subscribe_in(&input, window, |this, _, event, _, cx| {
                if let InputEvent::Change = event {
                    this.check(cx);
                }
            }));
            inputs.push(input);
        }
        if let Some(first) = inputs.first() {
            first.update(cx, |input, cx| input.focus(window, cx));
        }
        let errors = vec![None; params.len()];
        let mut prompt = Self {
            reference,
            params,
            inputs,
            errors,
            _subscriptions: subscriptions,
        };
        prompt.check(cx);
        prompt
    }

    pub fn reference(&self) -> &CommandRef {
        &self.reference
    }

    pub fn params(&self) -> &[Param] {
        &self.params
    }

    /// The value typed for the parameter called `name`.
    pub fn value(&self, name: &str, cx: &App) -> Option<String> {
        let ix = self.params.iter().position(|p| p.name == name)?;
        Some(self.inputs[ix].read(cx).value().to_string())
    }

    pub fn set_value(
        &mut self,
        name: &str,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.params.iter().position(|p| p.name == name) {
            self.inputs[ix].update(cx, |input, cx| {
                input.set_value(value.to_owned(), window, cx)
            });
            self.check(cx);
        }
    }

    /// Every value, as typed.
    pub fn values(&self, cx: &App) -> ParamValues {
        let mut values = ParamValues::new();
        for (param, input) in self.params.iter().zip(&self.inputs) {
            values.set(param.name.clone(), input.read(cx).value().trim().to_owned());
        }
        values
    }

    /// The problems with the values, one per parameter.
    pub fn errors(&self) -> &[Option<String>] {
        &self.errors
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        self.errors = self
            .params
            .iter()
            .zip(&self.inputs)
            .map(|(param, input)| {
                param
                    .validate(input.read(cx).value().trim())
                    .err()
                    .map(|error| error.to_string())
            })
            .collect();
        cx.notify();
    }

    /// Emit the values if they are all valid. Returns whether it did.
    pub fn confirm(&mut self, cx: &mut Context<Self>) -> bool {
        self.check(cx);
        if self.errors.iter().any(Option::is_some) {
            return false;
        }
        cx.emit(ParamPromptEvent::Confirmed {
            reference: self.reference.clone(),
            values: self.values(cx),
        });
        true
    }
}

impl Render for ParamPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex().id("param-prompt").gap_2().children(
            self.params
                .iter()
                .zip(&self.inputs)
                .zip(&self.errors)
                .enumerate()
                .map(|(ix, ((param, input), error))| {
                    v_flex()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div().w(px(120.)).flex_none().text_sm().truncate().child(
                                        SharedString::from(param.display_label().to_owned()),
                                    ),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .font_family(theme.mono_font_family.clone())
                                        .child(Input::new(input).id(("param", ix)).small()),
                                ),
                        )
                        .children(error.clone().map(|error| {
                            div()
                                .text_xs()
                                .text_color(theme.danger)
                                .child(SharedString::from(error))
                        }))
                }),
        )
    }
}
