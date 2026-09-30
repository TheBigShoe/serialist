//! The Devices panel: the live port list, the baud field and the Connect button.
//!
//! The list is a plain model ([`DeviceList`]) fed from a [`PortSource`] subscription;
//! the panel only renders it and turns a connect request into a
//! [`DevicesPanelEvent::Connect`] for the workspace, which owns sessions.
//!
//! Device profiles from the settings show here: a port a profile matches is listed
//! under the profile's `name` with a "profile" badge, and selecting it fills the baud
//! field with the profile's rate. Connecting uses the profile's framing and flow
//! control with the rate in the field.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use serialist_core::{PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig};

use crate::actions::{Connect, SelectNext, SelectPrevious, context};
use crate::config::Config;
use crate::prelude::*;

const ROW_HEIGHT: Pixels = px(44.);

/// Longest the port-event worker blocks waiting for an event, so it notices a dropped
/// panel.
const PORT_EVENT_WAIT: Duration = Duration::from_millis(50);

/// Pause between batches of port events, about a frame: a burst of hotplug events
/// costs one repaint.
pub(crate) const PORT_EVENT_FRAME: Duration = Duration::from_millis(8);

/// Apply `events` to the panel's list until the source or the panel goes away. The
/// wait happens on the background executor (crossbeam channels have no async receive),
/// never on the main thread, and each wake applies everything queued at once.
fn follow_port_events(
    events: Receiver<PortEvent>,
    window: &mut Window,
    cx: &mut Context<DevicesPanel>,
) -> Task<()> {
    cx.spawn_in(window, async move |this, cx| {
        loop {
            let rx = events.clone();
            let (batch, closed) = cx
                .background_spawn(async move {
                    match rx.recv_timeout(PORT_EVENT_WAIT) {
                        Ok(first) => {
                            let mut batch = vec![first];
                            batch.extend(rx.try_iter());
                            (batch, false)
                        }
                        Err(RecvTimeoutError::Timeout) => (Vec::new(), false),
                        Err(RecvTimeoutError::Disconnected) => (Vec::new(), true),
                    }
                })
                .await;
            let alive = this
                .update_in(cx, |panel, window, cx| {
                    if !batch.is_empty() {
                        for event in batch {
                            panel.list.apply(event);
                        }
                        // A port selected before it was listed has its profile now.
                        panel.prefill_baud(window, cx);
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive || closed {
                break;
            }
            cx.background_executor().timer(PORT_EVENT_FRAME).await;
        }
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEntry {
    pub info: PortInfo,
    /// False once the port has been unplugged; the row stays, greyed, for a quick reconnect.
    pub present: bool,
}

impl DeviceEntry {
    /// `vid:pid` in the lowercase hex form `lsusb` prints, for USB ports.
    pub fn usb_ids(&self) -> Option<String> {
        match &self.info.kind {
            PortKind::Usb(usb) => Some(format!("{:04x}:{:04x}", usb.vid, usb.pid)),
            _ => None,
        }
    }
}

/// Every port seen since launch, ordered by id so rows keep their place when a device
/// comes and goes.
#[derive(Clone, Debug, Default)]
pub struct DeviceList {
    entries: Vec<DeviceEntry>,
    /// Tracked by id rather than index so it survives inserts, and may name a port that
    /// has not appeared yet (a `--port` or `--virtual` pre-selection).
    selected: Option<PortId>,
}

impl DeviceList {
    pub fn apply(&mut self, event: PortEvent) {
        match event {
            PortEvent::Snapshot(ports) => {
                for entry in &mut self.entries {
                    entry.present = false;
                }
                for info in ports {
                    self.upsert(info);
                }
            }
            PortEvent::Added(info) => self.upsert(info),
            PortEvent::Removed(id) => {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.info.id == id) {
                    entry.present = false;
                }
            }
        }
        if self.selected.is_none() {
            self.selected = self
                .entries
                .iter()
                .find(|e| e.present)
                .map(|e| e.info.id.clone());
        }
    }

    fn upsert(&mut self, info: PortInfo) {
        match self.entries.binary_search_by(|e| e.info.id.cmp(&info.id)) {
            Ok(ix) => {
                let entry = &mut self.entries[ix];
                entry.info = info;
                entry.present = true;
            }
            Err(ix) => self.entries.insert(
                ix,
                DeviceEntry {
                    info,
                    present: true,
                },
            ),
        }
    }

    pub fn entries(&self) -> &[DeviceEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &PortId) -> Option<&DeviceEntry> {
        self.entries.iter().find(|e| &e.info.id == id)
    }

    pub fn selected_id(&self) -> Option<&PortId> {
        self.selected.as_ref()
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected.as_ref()?;
        self.entries.iter().position(|e| &e.info.id == id)
    }

    pub fn selected(&self) -> Option<&DeviceEntry> {
        self.selected_index().map(|ix| &self.entries[ix])
    }

    pub fn select(&mut self, id: PortId) {
        self.selected = Some(id);
    }

    pub fn select_index(&mut self, ix: usize) -> bool {
        match self.entries.get(ix) {
            Some(entry) => {
                self.selected = Some(entry.info.id.clone());
                true
            }
            None => false,
        }
    }

    pub fn select_next(&mut self) {
        let next = match self.selected_index() {
            Some(ix) => (ix + 1).min(self.entries.len().saturating_sub(1)),
            None => 0,
        };
        self.select_index(next);
    }

    pub fn select_previous(&mut self) {
        let previous = self.selected_index().map_or(0, |ix| ix.saturating_sub(1));
        self.select_index(previous);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BaudError {
    Empty,
    NotANumber(String),
    Zero,
}

impl fmt::Display for BaudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaudError::Empty => f.write_str("enter a baud rate"),
            BaudError::NotANumber(text) => write!(f, "\"{text}\" is not a whole number"),
            BaudError::Zero => f.write_str("baud rate must be above zero"),
        }
    }
}

impl std::error::Error for BaudError {}

/// Any positive integer is a valid rate: custom rates are the transport's business.
/// Underscores are accepted as digit separators (`1_000_000`).
pub fn parse_baud(text: &str) -> Result<u32, BaudError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(BaudError::Empty);
    }
    let digits: String = trimmed.chars().filter(|c| *c != '_').collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(BaudError::NotANumber(trimmed.to_owned()));
    }
    match digits.parse::<u32>() {
        Ok(0) => Err(BaudError::Zero),
        Ok(baud) => Ok(baud),
        Err(_) => Err(BaudError::NotANumber(trimmed.to_owned())),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicesPanelEvent {
    Connect { port: PortId, serial: SerialConfig },
}

pub struct DevicesPanel {
    list: DeviceList,
    baud: Entity<InputState>,
    /// The `--baud` flag: the baud field always starts from it, profiles or not.
    baud_override: Option<u32>,
    /// The port and configuration generation the baud field was last filled for, so a
    /// rate the user typed survives hotplug events and is replaced only when the
    /// selection or the settings change.
    prefilled: Option<(PortId, u64)>,
    notice: Option<SharedString>,
    connected: Option<PortId>,
    focus_handle: FocusHandle,
    scroll: UniformListScrollHandle,
    /// Held for the panel's lifetime: a source may stop reporting once dropped
    /// (`RealPortSource` stops its hotplug monitor), even with a subscription open.
    _source: Arc<dyn PortSource>,
    _port_events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<DevicesPanelEvent> for DevicesPanel {}

impl Focusable for DevicesPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The settings in force, or the bundled ones for a panel built without the app's
/// configuration.
fn settings_of(cx: &App) -> Option<&serialist_core::Settings> {
    cx.try_global::<Config>()
        .map(|config| config.settings().as_ref())
}

impl DevicesPanel {
    /// A panel listing `source`'s ports. `baud_override` (the `--baud` flag) fixes the
    /// rate the baud field starts from; without it the field shows the selected port's
    /// profile rate, else the `default_baud` setting.
    pub fn new(
        source: Arc<dyn PortSource>,
        baud_override: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let initial = baud_override
            .or_else(|| settings_of(cx).map(|settings| settings.default_baud))
            .unwrap_or(SerialConfig::default().baud);
        let baud = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Baud")
                .default_value(initial.to_string())
        });
        let baud_events = cx.subscribe_in(&baud, window, |this, _, event, _, cx| match event {
            InputEvent::PressEnter { .. } => {
                this.connect_selected(cx);
            }
            InputEvent::Change if this.notice.is_some() => {
                this.notice = None;
                cx.notify();
            }
            _ => {}
        });
        // Profiles may have changed: refill the field and redraw the names and badges.
        let config_changes = cx.observe_global_in::<Config>(window, |this, window, cx| {
            this.prefill_baud(window, cx);
            cx.notify();
        });

        let port_events = follow_port_events(source.subscribe(), window, cx);

        Self {
            list: DeviceList::default(),
            baud,
            baud_override,
            prefilled: None,
            notice: None,
            connected: None,
            focus_handle: cx.focus_handle(),
            scroll: UniformListScrollHandle::new(),
            _source: source,
            _port_events: port_events,
            _subscriptions: vec![baud_events, config_changes],
        }
    }

    pub fn list(&self) -> &DeviceList {
        &self.list
    }

    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    pub fn select_port(&mut self, id: PortId, window: &mut Window, cx: &mut Context<Self>) {
        self.list.select(id);
        self.scroll_to_selection();
        self.prefill_baud(window, cx);
        cx.notify();
    }

    /// The name a port is listed under: its device profile's `name`, else the
    /// transport's display name.
    pub fn display_name(&self, info: &PortInfo, cx: &App) -> String {
        settings_of(cx)
            .and_then(|settings| settings.device_name_for(info))
            .map_or_else(|| info.display_name.clone(), str::to_owned)
    }

    /// Whether a device profile matches `info`.
    pub fn has_profile(&self, info: &PortInfo, cx: &App) -> bool {
        settings_of(cx).is_some_and(|settings| settings.profile_for(info).is_some())
    }

    /// The line settings to open `info` with, other than the rate: its device profile's
    /// framing and flow control over 8N1.
    fn serial_for(&self, info: &PortInfo, cx: &App) -> SerialConfig {
        settings_of(cx).map_or_else(SerialConfig::default, |settings| {
            settings.serial_config_for(info)
        })
    }

    /// The rate the baud field starts from for `info`.
    fn baud_for(&self, info: &PortInfo, cx: &App) -> u32 {
        self.baud_override
            .unwrap_or_else(|| self.serial_for(info, cx).baud)
    }

    /// Fill the baud field for the selected port, unless it was already filled for
    /// this port under the current settings.
    fn prefill_baud(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.list.selected() else {
            return;
        };
        let generation = cx
            .try_global::<Config>()
            .map_or(0, |config| config.generation());
        let key = (entry.info.id.clone(), generation);
        if self.prefilled.as_ref() == Some(&key) {
            return;
        }
        let baud = self.baud_for(&entry.info, cx).to_string();
        self.prefilled = Some(key);
        if self.baud_text(cx) != baud {
            self.set_baud_text(&baud, window, cx);
        }
    }

    pub fn set_connected(&mut self, port: Option<PortId>, cx: &mut Context<Self>) {
        if self.connected != port {
            self.connected = port;
            cx.notify();
        }
    }

    pub fn set_notice(&mut self, notice: Option<SharedString>, cx: &mut Context<Self>) {
        self.notice = notice;
        cx.notify();
    }

    pub fn baud_text(&self, cx: &App) -> String {
        self.baud.read(cx).value().to_string()
    }

    pub fn set_baud_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.baud
            .update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Validate the selection and baud, then ask the workspace to connect. Returns whether
    /// a [`DevicesPanelEvent::Connect`] was emitted; otherwise the reason is in [`Self::notice`].
    pub fn connect_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let info = match self.list.selected() {
            None => {
                self.set_notice(Some("Select a port first".into()), cx);
                return false;
            }
            Some(entry) if !entry.present => {
                let notice = format!("{} is unplugged", self.display_name(&entry.info, cx));
                self.set_notice(Some(notice.into()), cx);
                return false;
            }
            Some(entry) => entry.info.clone(),
        };
        let baud = match parse_baud(&self.baud_text(cx)) {
            Ok(baud) => baud,
            Err(error) => {
                self.set_notice(Some(format!("Baud: {error}").into()), cx);
                return false;
            }
        };
        let serial = SerialConfig {
            baud,
            ..self.serial_for(&info, cx)
        };
        self.notice = None;
        cx.emit(DevicesPanelEvent::Connect {
            port: info.id,
            serial,
        });
        cx.notify();
        true
    }

    fn scroll_to_selection(&self) {
        if let Some(ix) = self.list.selected_index() {
            self.scroll.scroll_to_item(ix, ScrollStrategy::Nearest);
        }
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.list.select_next();
        self.scroll_to_selection();
        self.prefill_baud(window, cx);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.list.select_previous();
        self.scroll_to_selection();
        self.prefill_baud(window, cx);
        cx.notify();
    }

    fn connect_action(&mut self, _: &Connect, _: &mut Window, cx: &mut Context<Self>) {
        self.connect_selected(cx);
    }

    fn render_row(&self, ix: usize, entry: &DeviceEntry, cx: &mut Context<Self>) -> Stateful<Div> {
        let theme = cx.theme();
        let selected = self.list.selected_index() == Some(ix);
        let connected = self.connected.as_ref() == Some(&entry.info.id);
        let mono = theme.mono_font_family.clone();
        let (name_color, detail_color) = if entry.present {
            (theme.foreground, theme.muted_foreground)
        } else {
            (theme.muted_foreground, theme.muted_foreground.opacity(0.7))
        };

        let mut details = h_flex().gap_2().text_xs().text_color(detail_color).child(
            div()
                .font_family(mono)
                .truncate()
                .child(SharedString::from(entry.info.id.to_string())),
        );
        if let Some(ids) = entry.usb_ids() {
            details = details.child(div().flex_none().child(SharedString::from(ids)));
        }
        if !entry.present {
            details = details.child(div().flex_none().italic().child("unplugged"));
        }
        let name = SharedString::from(self.display_name(&entry.info, cx));
        let profiled = self.has_profile(&entry.info, cx);

        v_flex()
            .id(("device-row", ix))
            .w_full()
            .h(ROW_HEIGHT)
            .px_3()
            .justify_center()
            .border_l_2()
            .map(|row| {
                if selected {
                    row.bg(theme.list_active)
                        .border_color(theme.list_active_border)
                } else {
                    row.border_color(theme.transparent)
                        .hover(|style| style.bg(theme.list_hover))
                }
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .truncate()
                            .text_color(name_color)
                            .child(name),
                    )
                    .when(profiled, |this| {
                        this.child(
                            div()
                                .id(("device-profile", ix))
                                .flex_none()
                                .px_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("profile"),
                        )
                    })
                    .when(connected, |this| {
                        this.child(div().flex_none().size_2().rounded_full().bg(theme.success))
                    }),
            )
            .child(details)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.list.select_index(ix);
                this.prefill_baud(window, cx);
                if event.click_count() >= 2 {
                    this.connect_selected(cx);
                }
                cx.notify();
            }))
    }
}

impl Render for DevicesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let present = self.list.entries().iter().filter(|e| e.present).count();
        let can_connect = self.list.selected().is_some_and(|e| e.present);

        let header = h_flex()
            .justify_between()
            .px_3()
            .h(px(32.))
            .text_xs()
            .text_color(theme.muted_foreground)
            .child("DEVICES")
            .child(SharedString::from(format!("{present} available")));

        let body =
            if self.list.is_empty() {
                v_flex()
                    .flex_1()
                    .p_3()
                    .gap_1()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("No serial ports found.")
                    .child(div().text_xs().child(
                        "Plug in a device, or start with --virtual <name> or --port <path>.",
                    ))
                    .into_any_element()
            } else {
                uniform_list(
                    "device-list",
                    self.list.len(),
                    cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                        let entries: Vec<DeviceEntry> = this.list.entries()[range.clone()].to_vec();
                        range
                            .zip(entries.iter())
                            .map(|(ix, entry)| this.render_row(ix, entry, cx))
                            .collect::<Vec<_>>()
                    }),
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .into_any_element()
            };

        let footer = v_flex()
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .w(px(120.))
                            .child(Input::new(&self.baud).id("baud-input").small()),
                    )
                    .child(
                        Button::new("connect")
                            .label("Connect")
                            .small()
                            .primary()
                            .disabled(!can_connect)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.connect_selected(cx);
                            })),
                    ),
            )
            .when_some(self.notice.clone(), |this, notice| {
                this.child(div().text_xs().text_color(theme.danger).child(notice))
            });

        v_flex()
            .id("devices-panel")
            .key_context(context::DEVICES_PANEL)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::connect_action))
            .size_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .child(header)
            .child(body)
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use serialist_core::UsbInfo;

    use super::*;
    use crate::test_support::{FakePortSource, open_test_window, port, usb_port};

    fn ids(list: &DeviceList) -> Vec<(&str, bool)> {
        list.entries()
            .iter()
            .map(|e| (e.info.id.as_str(), e.present))
            .collect()
    }

    #[test]
    fn snapshot_added_removed_transitions() {
        let mut list = DeviceList::default();
        list.apply(PortEvent::Snapshot(vec![port("/dev/b"), port("/dev/a")]));
        assert_eq!(ids(&list), [("/dev/a", true), ("/dev/b", true)]);
        assert_eq!(list.selected_id().map(PortId::as_str), Some("/dev/a"));

        list.apply(PortEvent::Added(port("/dev/c")));
        list.apply(PortEvent::Removed(PortId::new("/dev/a")));
        assert_eq!(
            ids(&list),
            [("/dev/a", false), ("/dev/b", true), ("/dev/c", true)]
        );
        assert_eq!(
            list.selected_id().map(PortId::as_str),
            Some("/dev/a"),
            "an unplugged selection stays selected for a quick reconnect"
        );

        list.apply(PortEvent::Added(port("/dev/a")));
        assert!(list.get(&PortId::new("/dev/a")).unwrap().present);
    }

    #[test]
    fn a_new_snapshot_greys_out_ports_it_no_longer_lists() {
        let mut list = DeviceList::default();
        list.apply(PortEvent::Snapshot(vec![port("/dev/a"), port("/dev/b")]));
        list.apply(PortEvent::Snapshot(vec![port("/dev/b")]));
        assert_eq!(ids(&list), [("/dev/a", false), ("/dev/b", true)]);
    }

    #[test]
    fn re_adding_updates_metadata() {
        let mut list = DeviceList::default();
        list.apply(PortEvent::Added(port("/dev/a")));
        list.apply(PortEvent::Added(usb_port(
            "/dev/a", "Board", 0x0403, 0x6001,
        )));
        let entry = list.get(&PortId::new("/dev/a")).unwrap();
        assert_eq!(entry.info.display_name, "Board");
        assert_eq!(entry.usb_ids().as_deref(), Some("0403:6001"));
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn preselection_waits_for_the_port_to_appear() {
        let mut list = DeviceList::default();
        list.select(PortId::new("virtual:echo"));
        list.apply(PortEvent::Snapshot(vec![port("/dev/a")]));
        assert_eq!(list.selected_index(), None);
        list.apply(PortEvent::Added(port("virtual:echo")));
        assert_eq!(
            list.selected().map(|e| e.info.id.as_str()),
            Some("virtual:echo")
        );
    }

    #[test]
    fn keyboard_selection_clamps_at_both_ends() {
        let mut list = DeviceList::default();
        list.select_next();
        assert_eq!(list.selected_index(), None, "nothing to select");
        list.apply(PortEvent::Snapshot(vec![
            port("/dev/a"),
            port("/dev/b"),
            port("/dev/c"),
        ]));
        list.select_next();
        list.select_next();
        list.select_next();
        assert_eq!(list.selected_index(), Some(2));
        list.select_previous();
        list.select_previous();
        list.select_previous();
        assert_eq!(list.selected_index(), Some(0));
    }

    #[test]
    fn usb_ids_are_lowercase_hex() {
        let entry = DeviceEntry {
            info: PortInfo {
                id: PortId::new("/dev/cu.usbmodem1"),
                kind: PortKind::Usb(UsbInfo {
                    vid: 0x0E8D,
                    pid: 0x2000,
                    serial_number: None,
                    manufacturer: None,
                    product: None,
                }),
                display_name: "Airoha".into(),
            },
            present: true,
        };
        assert_eq!(entry.usb_ids().as_deref(), Some("0e8d:2000"));
        assert_eq!(
            DeviceEntry {
                info: port("/dev/x"),
                present: true
            }
            .usb_ids(),
            None
        );
    }

    #[test]
    fn baud_accepts_any_positive_integer() {
        assert_eq!(parse_baud("115200"), Ok(115_200));
        assert_eq!(parse_baud(" 250000 "), Ok(250_000));
        assert_eq!(parse_baud("1_000_000"), Ok(1_000_000));
        assert_eq!(parse_baud("31250"), Ok(31_250));
        assert_eq!(parse_baud(""), Err(BaudError::Empty));
        assert_eq!(parse_baud("0"), Err(BaudError::Zero));
        assert!(matches!(parse_baud("-9600"), Err(BaudError::NotANumber(_))));
        assert!(matches!(
            parse_baud("9600.5"),
            Err(BaudError::NotANumber(_))
        ));
        assert!(matches!(parse_baud("_"), Err(BaudError::NotANumber(_))));
        assert!(matches!(
            parse_baud("99999999999"),
            Err(BaudError::NotANumber(_))
        ));
    }

    #[gpui_test]
    fn panel_follows_the_port_source(cx: &mut TestAppContext) {
        let source = FakePortSource::new([port("/dev/a")]);
        let (_window, panel) = open_test_window(cx, |window, cx| {
            DevicesPanel::new(source.clone(), None, window, cx)
        });
        cx.run_until_parked();
        let read = |cx: &mut TestAppContext| {
            panel.read_with(cx, |p, _| {
                ids(p.list())
                    .iter()
                    .map(|(id, present)| (id.to_string(), *present))
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(read(cx), [("/dev/a".to_string(), true)]);

        source.plug(usb_port("/dev/b", "Board", 0x10c4, 0xea60));
        source.unplug(&PortId::new("/dev/a"));
        cx.executor().advance_clock(PORT_EVENT_FRAME);
        cx.run_until_parked();
        assert_eq!(
            read(cx),
            [("/dev/a".to_string(), false), ("/dev/b".to_string(), true)]
        );
    }

    #[gpui_test]
    fn connect_emits_the_selection_and_typed_baud(cx: &mut TestAppContext) {
        let source = FakePortSource::new([port("/dev/a"), port("/dev/b")]);
        let (window, panel) = open_test_window(cx, |window, cx| {
            DevicesPanel::new(source.clone(), None, window, cx)
        });
        cx.run_until_parked();

        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&panel, move |_, event: &DevicesPanelEvent, _| {
                events.borrow_mut().push(event.clone());
            })
            .detach();
        });

        cx.update_window(window, |_, window, cx| {
            panel.update(cx, |panel, cx| {
                panel.list.select_next();
                panel.set_baud_text("921600", window, cx);
                assert!(panel.connect_selected(cx));
                panel.set_baud_text("fast", window, cx);
                assert!(!panel.connect_selected(cx));
            });
        })
        .unwrap();
        cx.run_until_parked();

        assert_eq!(
            events.borrow().as_slice(),
            [DevicesPanelEvent::Connect {
                port: PortId::new("/dev/b"),
                serial: SerialConfig {
                    baud: 921_600,
                    ..SerialConfig::default()
                },
            }]
        );
        let notice = panel.read_with(cx, |p, _| p.notice().cloned());
        assert_eq!(
            notice.as_deref(),
            Some("Baud: \"fast\" is not a whole number")
        );
    }

    #[gpui_test]
    fn the_panel_keeps_its_port_source_alive(cx: &mut TestAppContext) {
        // RealPortSource stops hotplug monitoring when dropped, so the panel must own
        // its source rather than only the subscription.
        let source = FakePortSource::new([port("/dev/a")]);
        let (_window, panel) = open_test_window(cx, |window, cx| {
            DevicesPanel::new(source.clone(), None, window, cx)
        });
        cx.run_until_parked();
        assert_eq!(Arc::strong_count(&source), 2);
        drop(panel);
    }

    #[gpui_test]
    fn unplugged_ports_do_not_connect(cx: &mut TestAppContext) {
        let source = FakePortSource::new([port("/dev/a")]);
        let (_window, panel) = open_test_window(cx, |window, cx| {
            DevicesPanel::new(source.clone(), None, window, cx)
        });
        cx.run_until_parked();
        source.unplug(&PortId::new("/dev/a"));
        cx.executor().advance_clock(PORT_EVENT_FRAME);
        cx.run_until_parked();
        let connected = panel.update(cx, |panel, cx| panel.connect_selected(cx));
        assert!(!connected);
        let notice = panel.read_with(cx, |p, _| p.notice().cloned());
        assert_eq!(notice.as_deref(), Some("/dev/a is unplugged"));
    }
}
