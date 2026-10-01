//! The port settings form: everything about a port beyond its rate field, in a popover.
//!
//! The same form sits behind two buttons. In a session's toolbar it edits the open port
//! live: a line setting goes to the session's writer thread as a reconfigure, DTR and
//! RTS are toggled on the wire, and Send break holds the line; the session view waits
//! for the writer thread's verdict and shows a refusal (an `Unsupported` parity, say)
//! here, putting the control back (see [`SessionView`](crate::SessionView)). On a Devices
//! panel row it sets what the next connect to that port uses.
//!
//! The form only shows and asks: it keeps the [`PortSettings`] it shows, and every
//! change the user makes goes out as a [`PortSettingsEvent`] for its owner to apply.
//! Controls a user did not touch are set with [`PortSettingsForm::set_settings`],
//! which emits nothing.
//!
//! Baud is a text field that takes any positive integer (Enter or leaving the field
//! applies it) beside a list of the standard rates.
//!
//! The Settings view's device-profile form uses the same form for a profile's line
//! settings ([`PortSettingsForm::for_profile`]): the rate, framing, flow control and line
//! ending only, since a profile has no echo or control-line keys and no port to break.

use serialist_core::{
    ControlLine, DataBits, FlowControl, LineEnding, Parity, SerialConfig, StopBits,
};

use crate::devices_panel::parse_baud;
use crate::prelude::*;

/// The rates the baud list offers; the field takes any other.
pub const STANDARD_BAUDS: &[u32] = &[
    300, 600, 1200, 2400, 4800, 9600, 14_400, 19_200, 28_800, 38_400, 57_600, 115_200, 230_400,
    460_800, 921_600, 1_000_000, 1_500_000, 2_000_000, 3_000_000,
];

const LABEL_WIDTH: Pixels = px(96.);

/// Everything the form edits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortSettings {
    /// Baud, data bits, parity, stop bits and flow control.
    pub serial: SerialConfig,
    /// What Enter (and the compose bar) sends after a line.
    pub line_ending: LineEnding,
    pub local_echo: bool,
    /// The control lines' levels. Opening a port asserts both.
    pub dtr: bool,
    pub rts: bool,
}

impl PortSettings {
    /// `serial` with the line ending and echo given, and both control lines asserted.
    pub fn new(serial: SerialConfig, line_ending: LineEnding, local_echo: bool) -> Self {
        Self {
            serial,
            line_ending,
            local_echo,
            dtr: true,
            rts: true,
        }
    }
}

/// A change the user made in the form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortSettingsEvent {
    /// Baud, framing or flow control changed.
    Serial(SerialConfig),
    LineEnding(LineEnding),
    LocalEcho(bool),
    /// DTR or RTS switched.
    Control(ControlLine, bool),
    SendBreak,
}

fn data_bits_label(bits: DataBits) -> &'static str {
    match bits {
        DataBits::Five => "5",
        DataBits::Six => "6",
        DataBits::Seven => "7",
        DataBits::Eight => "8",
    }
}

const DATA_BITS: [DataBits; 4] = [
    DataBits::Five,
    DataBits::Six,
    DataBits::Seven,
    DataBits::Eight,
];

fn parity_label(parity: Parity) -> &'static str {
    match parity {
        Parity::None => "None",
        Parity::Odd => "Odd",
        Parity::Even => "Even",
        Parity::Mark => "Mark",
        Parity::Space => "Space",
    }
}

const PARITIES: [Parity; 5] = [
    Parity::None,
    Parity::Odd,
    Parity::Even,
    Parity::Mark,
    Parity::Space,
];

fn stop_bits_label(bits: StopBits) -> &'static str {
    match bits {
        StopBits::One => "1",
        StopBits::Two => "2",
    }
}

const STOP_BITS: [StopBits; 2] = [StopBits::One, StopBits::Two];

fn flow_label(flow: FlowControl) -> &'static str {
    match flow {
        FlowControl::None => "None",
        FlowControl::Hardware => "RTS/CTS",
        FlowControl::Software => "XON/XOFF",
    }
}

const FLOWS: [FlowControl; 3] = [
    FlowControl::None,
    FlowControl::Hardware,
    FlowControl::Software,
];

/// The value of `options` whose label is `label`.
fn by_label<T: Copy>(options: &[T], label: impl Fn(T) -> &'static str, name: &str) -> Option<T> {
    options
        .iter()
        .copied()
        .find(|option| label(*option) == name)
}

fn labels<T: Copy>(options: &[T], label: impl Fn(T) -> &'static str) -> Vec<String> {
    options
        .iter()
        .map(|option| label(*option).to_owned())
        .collect()
}

type Choice = Entity<SelectState<Vec<String>>>;

fn choice(
    items: Vec<String>,
    selected: &str,
    window: &mut Window,
    cx: &mut Context<PortSettingsForm>,
) -> Choice {
    let index = items.iter().position(|item| item == selected);
    cx.new(|cx| SelectState::new(items, index.map(IndexPath::new), window, cx).searchable(false))
}

pub struct PortSettingsForm {
    settings: PortSettings,
    /// An open session: the control lines and break act on it now.
    live: bool,
    /// A device profile's line settings: no echo, control lines or break.
    profile: bool,
    baud: Entity<InputState>,
    baud_list: Choice,
    data_bits: Choice,
    parity: Choice,
    stop_bits: Choice,
    flow: Choice,
    line_ending: Choice,
    /// Why the last change did not take, shown in the danger color.
    error: Option<String>,
    /// What the last change did.
    status: Option<String>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PortSettingsEvent> for PortSettingsForm {}

impl Focusable for PortSettingsForm {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl PortSettingsForm {
    /// A form showing `settings`; `live` for an open session.
    pub fn new(
        settings: PortSettings,
        live: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let serial = &settings.serial;
        let baud = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Baud")
                .default_value(serial.baud.to_string())
        });
        let baud_events = cx.subscribe_in(&baud, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } | InputEvent::Blur = event {
                this.commit_baud(window, cx);
            }
        });
        let bauds: Vec<String> = STANDARD_BAUDS.iter().map(u32::to_string).collect();
        let baud_list = choice(bauds, &serial.baud.to_string(), window, cx);
        let data_bits = choice(
            labels(&DATA_BITS, data_bits_label),
            data_bits_label(serial.data_bits),
            window,
            cx,
        );
        let parity = choice(
            labels(&PARITIES, parity_label),
            parity_label(serial.parity),
            window,
            cx,
        );
        let stop_bits = choice(
            labels(&STOP_BITS, stop_bits_label),
            stop_bits_label(serial.stop_bits),
            window,
            cx,
        );
        let flow = choice(
            labels(&FLOWS, flow_label),
            flow_label(serial.flow_control),
            window,
            cx,
        );
        let line_ending = choice(
            labels(&LineEnding::ALL, LineEnding::label),
            settings.line_ending.label(),
            window,
            cx,
        );
        let picked =
            |select: &Choice,
             cx: &mut Context<Self>,
             apply: fn(&mut Self, &str, &mut Window, &mut Context<Self>)| {
                cx.subscribe_in(
                    select,
                    window,
                    move |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                        if let SelectEvent::Confirm(Some(value)) = event {
                            apply(this, value, window, cx);
                        }
                    },
                )
            };
        let subscriptions = vec![
            baud_events,
            picked(&baud_list, cx, |this, value, window, cx| {
                if let Ok(baud) = value.parse::<u32>() {
                    this.baud.update(cx, |input, cx| {
                        input.set_value(value.to_owned(), window, cx)
                    });
                    this.change_serial(
                        SerialConfig {
                            baud,
                            ..this.settings.serial.clone()
                        },
                        window,
                        cx,
                    );
                }
            }),
            picked(&data_bits, cx, |this, value, window, cx| {
                if let Some(data_bits) = by_label(&DATA_BITS, data_bits_label, value) {
                    let serial = SerialConfig {
                        data_bits,
                        ..this.settings.serial.clone()
                    };
                    this.change_serial(serial, window, cx);
                }
            }),
            picked(&parity, cx, |this, value, window, cx| {
                if let Some(parity) = by_label(&PARITIES, parity_label, value) {
                    let serial = SerialConfig {
                        parity,
                        ..this.settings.serial.clone()
                    };
                    this.change_serial(serial, window, cx);
                }
            }),
            picked(&stop_bits, cx, |this, value, window, cx| {
                if let Some(stop_bits) = by_label(&STOP_BITS, stop_bits_label, value) {
                    let serial = SerialConfig {
                        stop_bits,
                        ..this.settings.serial.clone()
                    };
                    this.change_serial(serial, window, cx);
                }
            }),
            picked(&flow, cx, |this, value, window, cx| {
                if let Some(flow_control) = by_label(&FLOWS, flow_label, value) {
                    let serial = SerialConfig {
                        flow_control,
                        ..this.settings.serial.clone()
                    };
                    this.change_serial(serial, window, cx);
                }
            }),
            picked(&line_ending, cx, |this, value, _, cx| {
                if let Some(ending) = by_label(&LineEnding::ALL, LineEnding::label, value) {
                    this.change_line_ending(ending, cx);
                }
            }),
        ];
        Self {
            settings,
            live,
            profile: false,
            baud,
            baud_list,
            data_bits,
            parity,
            stop_bits,
            flow,
            line_ending,
            error: None,
            status: None,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        }
    }

    /// A form for a device profile's line settings: the rate, framing, flow control and
    /// line ending, without the echo, control-line and break rows.
    pub fn for_profile(settings: PortSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {
            profile: true,
            ..Self::new(settings, false, window, cx)
        }
    }

    // --- Reading -----------------------------------------------------------------------

    pub fn settings(&self) -> &PortSettings {
        &self.settings
    }

    pub fn is_live(&self) -> bool {
        self.live
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// The baud field's text.
    pub fn baud_text(&self, cx: &App) -> String {
        self.baud.read(cx).value().to_string()
    }

    // --- Set by the owner (no events) ------------------------------------------------------

    /// Show `settings` in every control, emitting nothing: the settings in force, after
    /// a change was refused or made elsewhere.
    pub fn set_settings(
        &mut self,
        settings: PortSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let serial = settings.serial.clone();
        let baud = serial.baud.to_string();
        if self.baud_text(cx) != baud {
            self.baud
                .update(cx, |input, cx| input.set_value(baud.clone(), window, cx));
        }
        let select = |choice: &Choice, value: &str, window: &mut Window, cx: &mut App| {
            let value = value.to_owned();
            choice.update(cx, |select, cx| {
                if select.selected_value() != Some(&value) {
                    select.set_selected_value(&value, window, cx);
                }
            });
        };
        select(&self.baud_list, &baud, window, cx);
        select(
            &self.data_bits,
            data_bits_label(serial.data_bits),
            window,
            cx,
        );
        select(&self.parity, parity_label(serial.parity), window, cx);
        select(
            &self.stop_bits,
            stop_bits_label(serial.stop_bits),
            window,
            cx,
        );
        select(&self.flow, flow_label(serial.flow_control), window, cx);
        select(&self.line_ending, settings.line_ending.label(), window, cx);
        self.settings = settings;
        cx.notify();
    }

    /// Whether the control lines and break act on an open session now.
    pub fn set_live(&mut self, live: bool, cx: &mut Context<Self>) {
        if self.live != live {
            self.live = live;
            cx.notify();
        }
    }

    /// Say why the last change did not take (and clear the status), or clear it.
    pub fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if error.is_some() {
            self.status = None;
        }
        self.error = error;
        cx.notify();
    }

    /// Say what the last change did (and clear the error), or clear it.
    pub fn set_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        if status.is_some() {
            self.error = None;
        }
        self.status = status;
        cx.notify();
    }

    // --- Changed by the user (events) ----------------------------------------------------

    /// The line settings are now `serial`, as picking them in the controls does.
    pub fn change_serial(
        &mut self,
        serial: SerialConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if serial == self.settings.serial {
            return;
        }
        let settings = PortSettings {
            serial: serial.clone(),
            ..self.settings.clone()
        };
        self.set_settings(settings, window, cx);
        self.error = None;
        self.status = None;
        cx.emit(PortSettingsEvent::Serial(serial));
    }

    /// Take the baud field's text as the rate, if it is one.
    pub fn commit_baud(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.baud_text(cx);
        match parse_baud(&text) {
            Ok(baud) => {
                let serial = SerialConfig {
                    baud,
                    ..self.settings.serial.clone()
                };
                self.change_serial(serial, window, cx);
            }
            Err(error) => self.set_error(Some(format!("Baud: {error}")), cx),
        }
    }

    /// Type `text` in the baud field and apply it, as Enter does.
    pub fn enter_baud(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.baud
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
        self.commit_baud(window, cx);
    }

    pub fn change_line_ending(&mut self, ending: LineEnding, cx: &mut Context<Self>) {
        if self.settings.line_ending != ending {
            self.settings.line_ending = ending;
            cx.emit(PortSettingsEvent::LineEnding(ending));
            cx.notify();
        }
    }

    pub fn change_local_echo(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.settings.local_echo != on {
            self.settings.local_echo = on;
            cx.emit(PortSettingsEvent::LocalEcho(on));
            cx.notify();
        }
    }

    /// Switch DTR or RTS, as its switch does.
    pub fn change_control(&mut self, line: ControlLine, on: bool, cx: &mut Context<Self>) {
        let level = match line {
            ControlLine::Dtr => &mut self.settings.dtr,
            ControlLine::Rts => &mut self.settings.rts,
        };
        if *level != on {
            *level = on;
            self.error = None;
            self.status = None;
            cx.emit(PortSettingsEvent::Control(line, on));
            cx.notify();
        }
    }

    pub fn send_break(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.status = None;
        cx.emit(PortSettingsEvent::SendBreak);
        cx.notify();
    }

    // --- Rendering -----------------------------------------------------------------------

    fn row(label: &'static str, control: impl IntoElement) -> Div {
        h_flex()
            .w_full()
            .gap_2()
            .items_center()
            .child(div().w(LABEL_WIDTH).flex_none().text_sm().child(label))
            .child(div().flex_1().min_w_0().child(control))
    }
}

impl Render for PortSettingsForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let live = self.live;
        let muted = theme.muted_foreground;
        let danger = theme.danger;
        let settings = self.settings.clone();
        let profile = self.profile;
        v_flex()
            .id("port-settings")
            .track_focus(&self.focus_handle)
            .w(px(330.))
            .gap_2()
            .child(div().text_xs().text_color(muted).child(if profile {
                "LINE SETTINGS (when the device connects)"
            } else if live {
                "PORT SETTINGS (applied now)"
            } else {
                "PORT SETTINGS (for the next connect)"
            }))
            .child(Self::row(
                "Baud",
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .w(px(100.))
                            .child(Input::new(&self.baud).id("port-baud").small()),
                    )
                    .child(
                        div().flex_1().child(
                            Select::new(&self.baud_list)
                                .small()
                                .placeholder("Standard")
                                .menu_width(px(140.)),
                        ),
                    ),
            ))
            .child(Self::row("Data bits", Select::new(&self.data_bits).small()))
            .child(Self::row("Parity", Select::new(&self.parity).small()))
            .child(Self::row("Stop bits", Select::new(&self.stop_bits).small()))
            .child(Self::row("Flow control", Select::new(&self.flow).small()))
            .child(Self::row(
                "Line ending",
                Select::new(&self.line_ending).small(),
            ))
            .when(!profile, |form| form.child(Self::row(
                "Local echo",
                Switch::new("port-local-echo")
                    .checked(settings.local_echo)
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.change_local_echo(*on, cx);
                    })),
            ))
            .child(Self::row(
                "DTR",
                Switch::new("port-dtr")
                    .checked(settings.dtr)
                    .disabled(!live)
                    .tooltip("Data Terminal Ready")
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.change_control(ControlLine::Dtr, *on, cx);
                    })),
            ))
            .child(Self::row(
                "RTS",
                Switch::new("port-rts")
                    .checked(settings.rts)
                    .disabled(!live)
                    .tooltip("Request To Send")
                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                        this.change_control(ControlLine::Rts, *on, cx);
                    })),
            ))
            .child(
                h_flex().justify_end().child(
                    Button::new("port-send-break")
                        .label("Send break")
                        .tooltip("Hold the line in the break condition for 250 ms")
                        .small()
                        .disabled(!live)
                        .on_click(cx.listener(|this, _, _, cx| this.send_break(cx))),
                ),
            ))
            .children(self.error.clone().map(|error| {
                div()
                    .id("port-settings-error")
                    .text_xs()
                    .text_color(danger)
                    .child(SharedString::from(error))
            }))
            .children(self.status.clone().map(|status| {
                div()
                    .id("port-settings-status")
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(status))
            }))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::test_support::open_test_window;

    #[test]
    fn every_option_has_a_label_and_back() {
        for bits in DATA_BITS {
            assert_eq!(
                by_label(&DATA_BITS, data_bits_label, data_bits_label(bits)),
                Some(bits)
            );
        }
        for parity in PARITIES {
            assert_eq!(
                by_label(&PARITIES, parity_label, parity_label(parity)),
                Some(parity)
            );
        }
        for flow in FLOWS {
            assert_eq!(by_label(&FLOWS, flow_label, flow_label(flow)), Some(flow));
        }
        assert_eq!(STANDARD_BAUDS.first(), Some(&300));
        assert_eq!(STANDARD_BAUDS.last(), Some(&3_000_000));
        assert!(STANDARD_BAUDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[gpui_test]
    fn changes_go_out_as_events_and_set_settings_emits_none(cx: &mut TestAppContext) {
        let settings = PortSettings::new(SerialConfig::default(), LineEnding::Crlf, false);
        let (window, form) = open_test_window(cx, |window, cx| {
            PortSettingsForm::new(settings.clone(), true, window, cx)
        });
        let events = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&form, move |_, event: &PortSettingsEvent, _| {
                events.borrow_mut().push(event.clone());
            })
            .detach();
        });
        cx.update_window(window, |_, window, cx| {
            form.update(cx, |form, cx| {
                form.enter_baud("250000", window, cx);
                form.enter_baud("fast", window, cx);
                form.change_serial(
                    SerialConfig {
                        baud: 250_000,
                        parity: Parity::Even,
                        ..SerialConfig::default()
                    },
                    window,
                    cx,
                );
                form.change_line_ending(LineEnding::Lf, cx);
                form.change_local_echo(true, cx);
                form.change_control(ControlLine::Dtr, false, cx);
                form.change_control(ControlLine::Dtr, false, cx);
                form.send_break(cx);
                form.set_settings(settings.clone(), window, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        let baud = |baud| SerialConfig {
            baud,
            ..SerialConfig::default()
        };
        assert_eq!(
            events.borrow().as_slice(),
            [
                PortSettingsEvent::Serial(baud(250_000)),
                PortSettingsEvent::Serial(SerialConfig {
                    parity: Parity::Even,
                    ..baud(250_000)
                }),
                PortSettingsEvent::LineEnding(LineEnding::Lf),
                PortSettingsEvent::LocalEcho(true),
                PortSettingsEvent::Control(ControlLine::Dtr, false),
                PortSettingsEvent::SendBreak,
            ],
            "a bad rate and a repeat send nothing; set_settings sends nothing"
        );
        form.read_with(cx, |form, cx| {
            assert_eq!(form.settings(), &settings);
            assert_eq!(form.baud_text(cx), "115200");
        });
    }

    #[gpui_test]
    fn a_rate_that_is_not_a_number_says_so(cx: &mut TestAppContext) {
        let settings = PortSettings::new(SerialConfig::default(), LineEnding::Crlf, false);
        let (window, form) = open_test_window(cx, |window, cx| {
            PortSettingsForm::new(settings, false, window, cx)
        });
        cx.update_window(window, |_, window, cx| {
            form.update(cx, |form, cx| form.enter_baud("0", window, cx));
        })
        .unwrap();
        form.read_with(cx, |form, _| {
            assert_eq!(form.error(), Some("Baud: baud rate must be above zero"));
            assert_eq!(form.settings().serial.baud, 115_200);
            assert!(!form.is_live());
        });
    }
}
