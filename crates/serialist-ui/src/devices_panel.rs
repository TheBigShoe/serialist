//! The Devices panel: the live port list, the baud field and the Connect button.
//!
//! Each port is one 40 px row of two lines. The first has a dot (green while a tab has
//! it open, a ring otherwise), the name, and right-aligned chips for its device profile
//! or codec; the name is what gives way when the row is narrow. The second is muted
//! small text, under the name: the port id, the USB `VID:PID`, and, on the selected
//! and the connected rows, the line settings the next connect uses, each cut short
//! from its end. Hovering a row brings up its gear (those settings) and Connect over
//! the right end of the first line, in place of the chips, so nothing moves. Simulated
//! ports (`--virtual`) sit under a 28 px "Simulated" header that folds, open while
//! nothing else is listed. The list scrolls; the baud field and Connect stay below it.
//!
//! The list is a plain model ([`DeviceList`]) fed from a [`PortSource`] subscription;
//! the panel only renders it and turns a connect request into a
//! [`DevicesPanelEvent::Connect`] for the workspace, which owns sessions.
//!
//! Device profiles from the settings show here: a port a profile matches is listed
//! under the profile's `name` with a "profile" badge (and a badge naming its `plugin`,
//! the codec the session decodes with), and selecting it fills the baud field with the
//! profile's rate. Connecting uses the profile's framing and flow control with the rate
//! in the field.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use serialist_core::{LineEnding, PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig};

use crate::actions::{Connect, SelectNext, SelectPrevious, context};
use crate::chrome;
use crate::config::Config;
use crate::port_settings::{PortSettings, PortSettingsEvent, PortSettingsForm};
use crate::prelude::*;

/// The height of a row's first line: the dot, the name and the chips.
const NAME_LINE: Pixels = px(20.);
/// The height of a row's second line, the muted details.
const DETAIL_LINE: Pixels = px(16.);
/// The space above the first line and below the second. With the two lines it makes
/// [`chrome::TWO_LINE_ROW_HEIGHT`].
const LINE_PAD: Pixels = px(2.);
/// How far the details are indented to line up under the name: the dot and the gap
/// after it.
const DETAIL_INDENT: Pixels = px(16.);

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
                        let had_selection = panel.list.selected_index().is_some();
                        for event in batch {
                            panel.list.apply(event);
                        }
                        // A port selected before it was listed (`--port`) scrolls into
                        // view now that there is a row for it.
                        if !had_selection && panel.list.selected_index().is_some() {
                            panel.scroll_to_selection();
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
    /// Open `port` with `serial`. `settings` are what the row's port settings set, when
    /// they were opened: the session takes their line ending, echo and control levels.
    Connect {
        port: PortId,
        serial: SerialConfig,
        settings: Option<PortSettings>,
    },
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
    /// The ports open in a tab, which get the green dot.
    connected: Vec<PortId>,
    /// What a row's port settings set, for the next connect to that port.
    port_settings: HashMap<PortId, PortSettings>,
    /// The form the rows' gear opens, and the port it is open for.
    port_form: Entity<PortSettingsForm>,
    editing: Option<PortId>,
    /// Whether the user unfolded (or folded) the simulated devices; `None` follows the
    /// default (open while nothing else is listed, or a simulated port is selected).
    simulated_open: Option<bool>,
    focus_handle: FocusHandle,
    scroll: ScrollHandle,
    /// Whether the selected row is to be scrolled into view, at the next render.
    reveal_selected: bool,
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
        let defaults = PortSettings::new(
            SerialConfig {
                baud: initial,
                ..SerialConfig::default()
            },
            LineEnding::default(),
            false,
        );
        let port_form = cx.new(|cx| PortSettingsForm::new(defaults, false, window, cx));
        let port_form_events = cx.subscribe_in(
            &port_form,
            window,
            |this, _, event: &PortSettingsEvent, window, cx| {
                this.port_settings_changed(event, window, cx);
            },
        );

        Self {
            list: DeviceList::default(),
            baud,
            baud_override,
            prefilled: None,
            notice: None,
            connected: Vec::new(),
            port_settings: HashMap::new(),
            port_form,
            editing: None,
            simulated_open: None,
            focus_handle: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            reveal_selected: false,
            _source: source,
            _port_events: port_events,
            _subscriptions: vec![baud_events, config_changes, port_form_events],
        }
    }

    // --- Port settings -----------------------------------------------------------------

    /// The form the rows' gear opens.
    pub fn port_form(&self) -> &Entity<PortSettingsForm> {
        &self.port_form
    }

    /// What the next connect to `info` uses: the settings its row set, else its device
    /// profile's (with the rate the baud field starts from) and the global line ending
    /// and echo.
    pub fn port_settings_for(&self, info: &PortInfo, cx: &App) -> PortSettings {
        if let Some(settings) = self.port_settings.get(&info.id) {
            return settings.clone();
        }
        let serial = SerialConfig {
            baud: self.baud_for(info, cx),
            ..self.serial_for(info, cx)
        };
        let (line_ending, local_echo) = settings_of(cx)
            .map_or((LineEnding::default(), false), |s| {
                (s.line_ending_for(info), s.local_echo)
            });
        PortSettings::new(serial, line_ending, local_echo)
    }

    /// Point the form at `port`'s row, as its gear opens it.
    pub fn edit_port_settings(
        &mut self,
        port: PortId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = self
            .list
            .get(&port)
            .map(|entry| entry.info.clone())
            .unwrap_or_else(|| PortInfo {
                id: port.clone(),
                kind: PortKind::Unknown,
                display_name: port.to_string(),
            });
        let settings = self.port_settings_for(&info, cx);
        self.editing = Some(port);
        self.port_form.update(cx, |form, cx| {
            form.set_settings(settings, window, cx);
            form.set_live(false, cx);
            form.set_status(None, cx);
        });
    }

    /// The port the form is open for.
    pub fn editing(&self) -> Option<&PortId> {
        self.editing.as_ref()
    }

    /// Keep what the form changed for the port it is open for; a new rate shows in the
    /// baud field if that port is selected.
    fn port_settings_changed(
        &mut self,
        event: &PortSettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(port) = self.editing.clone() else {
            return;
        };
        let settings = self.port_form.read(cx).settings().clone();
        if let PortSettingsEvent::Serial(serial) = event
            && self.list.selected_id() == Some(&port)
        {
            let baud = serial.baud.to_string();
            if self.baud_text(cx) != baud {
                self.set_baud_text(&baud, window, cx);
            }
        }
        self.port_form.update(cx, |form, cx| {
            form.set_status(Some("Used by the next connect to this port".into()), cx);
        });
        self.port_settings.insert(port, settings);
        cx.notify();
    }

    pub fn list(&self) -> &DeviceList {
        &self.list
    }

    pub fn notice(&self) -> Option<&SharedString> {
        self.notice.as_ref()
    }

    pub fn select_port(&mut self, id: PortId, window: &mut Window, cx: &mut Context<Self>) {
        self.list.select(id);
        self.reveal_selection();
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

    /// The codec the device profile matching `info` decodes with (its `plugin`), shown as
    /// a badge on the row.
    pub fn plugin_for(&self, info: &PortInfo, cx: &App) -> Option<String> {
        settings_of(cx)?
            .profile_for(info)?
            .plugin
            .clone()
            .filter(|plugin| !plugin.trim().is_empty())
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

    /// The ports open in a tab now, each marked with a dot.
    pub fn set_connected(&mut self, ports: Vec<PortId>, cx: &mut Context<Self>) {
        if self.connected != ports {
            self.connected = ports;
            cx.notify();
        }
    }

    /// The ports marked open.
    pub fn connected(&self) -> &[PortId] {
        &self.connected
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
        // The row's port settings, if they were set, over the device profile.
        let settings = self.port_settings.get(&info.id).cloned();
        let base = settings
            .as_ref()
            .map_or_else(|| self.serial_for(&info, cx), |s| s.serial.clone());
        let serial = SerialConfig { baud, ..base };
        self.notice = None;
        cx.emit(DevicesPanelEvent::Connect {
            port: info.id,
            serial,
            settings,
        });
        cx.notify();
        true
    }

    /// Whether the simulated devices' group is unfolded.
    pub fn simulated_is_open(&self) -> bool {
        self.simulated_open.unwrap_or_else(|| {
            self.list
                .entries()
                .iter()
                .all(|entry| is_simulated(&entry.info))
                || self
                    .list
                    .selected()
                    .is_some_and(|entry| is_simulated(&entry.info))
        })
    }

    /// Fold or unfold the simulated devices.
    pub fn set_simulated_open(&mut self, open: bool, cx: &mut Context<Self>) {
        self.simulated_open = Some(open);
        cx.notify();
    }

    /// The rows the list shows: the other ports, then the simulated ones under their
    /// header (unless folded), each part in the list's order.
    pub fn rows(&self) -> Vec<DeviceRow> {
        let entries = self.list.entries();
        let (simulated, others): (Vec<usize>, Vec<usize>) =
            (0..entries.len()).partition(|ix| is_simulated(&entries[*ix].info));
        let mut rows: Vec<DeviceRow> = others.into_iter().map(DeviceRow::Entry).collect();
        if !simulated.is_empty() {
            let open = self.simulated_is_open();
            rows.push(DeviceRow::Simulated {
                count: simulated.len(),
                open,
            });
            if open {
                rows.extend(simulated.into_iter().map(DeviceRow::Entry));
            }
        }
        rows
    }

    /// Unfold the simulated devices if the selection is one of them.
    fn reveal_selection(&mut self) {
        if self
            .list
            .selected()
            .is_some_and(|entry| is_simulated(&entry.info))
            && self.simulated_open == Some(false)
        {
            self.simulated_open = Some(true);
        }
    }

    /// Ask for the selected row to be scrolled into view by the next render.
    fn scroll_to_selection(&mut self) {
        self.reveal_selected = self.list.selected_index().is_some();
    }

    /// Do what [`Self::scroll_to_selection`] asked. The scroll handle drops a request
    /// that reaches its list before the list's first frame (it has no size to scroll by
    /// yet), as a port selected before it was listed does, so wait for that frame.
    fn reveal_selected_row(&mut self, cx: &mut Context<Self>) {
        if !self.reveal_selected {
            return;
        }
        let Some(selected) = self.list.selected_index() else {
            self.reveal_selected = false;
            return;
        };
        if self.scroll.bounds().size.width <= px(0.) {
            cx.notify();
            return;
        }
        self.reveal_selected = false;
        if let Some(row) = self
            .rows()
            .iter()
            .position(|row| *row == DeviceRow::Entry(selected))
        {
            self.scroll.scroll_to_item(row);
        }
    }

    /// Move the selection to the next (or previous) row on screen, stopping at the ends.
    fn step_selection(&mut self, forward: bool) {
        let visible: Vec<usize> = self
            .rows()
            .into_iter()
            .filter_map(|row| match row {
                DeviceRow::Entry(ix) => Some(ix),
                DeviceRow::Simulated { .. } => None,
            })
            .collect();
        if visible.is_empty() {
            return;
        }
        let at = self
            .list
            .selected_index()
            .and_then(|selected| visible.iter().position(|ix| *ix == selected));
        let next = match at {
            None => 0,
            Some(at) if forward => (at + 1).min(visible.len() - 1),
            Some(at) => at.saturating_sub(1),
        };
        self.list.select_index(visible[next]);
    }

    fn select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(true);
        self.scroll_to_selection();
        self.prefill_baud(window, cx);
        cx.notify();
    }

    fn select_previous(&mut self, _: &SelectPrevious, window: &mut Window, cx: &mut Context<Self>) {
        self.step_selection(false);
        self.scroll_to_selection();
        self.prefill_baud(window, cx);
        cx.notify();
    }

    fn connect_action(&mut self, _: &Connect, _: &mut Window, cx: &mut Context<Self>) {
        self.connect_selected(cx);
    }

    /// The row's gear: the port settings the next connect to it uses.
    fn render_gear(&self, ix: usize, entry: &DeviceEntry, cx: &mut Context<Self>) -> Popover {
        let form = self.port_form.clone();
        let port = entry.info.id.clone();
        let set = self.port_settings.contains_key(&port);
        Popover::new(("device-settings", ix))
            .trigger(
                chrome::toggle_button(("device-gear", ix), IconName::Settings2, set, cx)
                    .xsmall()
                    .tooltip(if set {
                        "Port settings (set for the next connect)"
                    } else {
                        "Port settings for the next connect"
                    }),
            )
            .content(move |_, _, _| form.clone())
            .on_open_change(cx.listener(move |this, open: &bool, window, cx| {
                if *open {
                    this.edit_port_settings(port.clone(), window, cx);
                }
            }))
    }

    /// The line settings the next connect to `info` uses, with the rate in the baud field.
    fn next_settings(&self, info: &PortInfo, cx: &App) -> SerialConfig {
        let base = self
            .port_settings
            .get(&info.id)
            .map_or_else(|| self.serial_for(info, cx), |s| s.serial.clone());
        match parse_baud(&self.baud_text(cx)) {
            Ok(baud) => SerialConfig { baud, ..base },
            Err(_) => base,
        }
    }

    /// The line settings shown on the row of `info`: what the next connect uses, which for
    /// the selected row takes the rate from the baud field. Another row shows its own
    /// (its port settings, else its device profile's).
    fn row_settings(&self, info: &PortInfo, selected: bool, cx: &App) -> SerialConfig {
        if selected {
            self.next_settings(info, cx)
        } else {
            self.port_settings_for(info, cx).serial
        }
    }

    /// Entry `ix` of the list (its index in [`DeviceList::entries`]) as a row.
    fn render_row(&self, ix: usize, entry: &DeviceEntry, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.list.selected_index() == Some(ix);
        let connected = self.connected.contains(&entry.info.id);
        let can_connect = entry.present;
        let group = SharedString::from(format!("device-row-{ix}"));
        let row_background = chrome::overlay_background(selected, cx);
        // Hovering a row brings up its actions, over the right end of its first line.
        let actions = h_flex()
            .id(("device-actions", ix))
            .test_support()
            .absolute()
            .top(LINE_PAD)
            .right_0()
            .h(NAME_LINE)
            .pl_4()
            .pr_1()
            .gap_0p5()
            .items_center()
            .bg(row_background)
            .opacity(0.)
            .group_hover(group.clone(), |style| style.opacity(1.))
            .child(self.render_gear(ix, entry, cx))
            .child(
                chrome::icon_button(("device-connect", ix), IconName::Plug, cx)
                    .xsmall()
                    .disabled(!can_connect)
                    .tooltip_with_action(
                        if connected {
                            "Go to its tab"
                        } else {
                            "Connect"
                        },
                        &Connect,
                        Some(context::DEVICES_PANEL),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        window.focus(&this.focus_handle, cx);
                        this.list.select_index(ix);
                        this.prefill_baud(window, cx);
                        this.connect_selected(cx);
                    })),
            );

        let simulated = is_simulated(&entry.info);
        let (name_color, detail_color) = {
            let theme = cx.theme();
            if entry.present {
                (theme.foreground, theme.muted_foreground)
            } else {
                (theme.muted_foreground, theme.muted_foreground.opacity(0.7))
            }
        };
        let (dot_color, hollow) = {
            let theme = cx.theme();
            if connected {
                (theme.success, false)
            } else if entry.present {
                (theme.muted_foreground, true)
            } else {
                (theme.muted_foreground.opacity(0.5), true)
            }
        };
        let mono = cx.theme().mono_font_family.clone();
        let name = SharedString::from(self.display_name(&entry.info, cx));
        let profiled = self.has_profile(&entry.info, cx);
        let plugin = self.plugin_for(&entry.info, cx);
        let summary =
            (selected || connected).then(|| self.row_settings(&entry.info, selected, cx).summary());
        let port = entry.info.id.to_string();
        let tooltip = SharedString::from(match entry.usb_ids() {
            Some(ids) => format!("{port} \u{00b7} USB {ids}"),
            None => port.clone(),
        });

        // The chips sit at the right end of the first line. They are hidden (not moved)
        // while the row is hovered, so the actions can take their place.
        let chips = h_flex()
            .id(("device-chips", ix))
            .flex_none()
            .gap_1()
            .items_center()
            .group_hover(group.clone(), |style| style.opacity(0.))
            .when(profiled && plugin.is_none(), |row| {
                row.child(chrome::quiet_chip("profile", cx))
            })
            .when_some(plugin, |row, plugin| {
                row.child(
                    chrome::chip(cx.theme().info)
                        .id(("device-plugin", ix))
                        .test_support()
                        .child(SharedString::from(plugin)),
                )
            });

        // The second line: muted and small, every part cut short from its end. The port id
        // gives way first; the USB ids and the settings keep their width until the line
        // itself is out of room.
        let separator = || div().flex_none().child("\u{00b7}").into_any_element();
        let details = h_flex()
            .id(("device-detail", ix))
            .test_support()
            .flex_none()
            .w_full()
            .h(DETAIL_LINE)
            .pl(DETAIL_INDENT)
            .gap_1p5()
            .items_center()
            .overflow_hidden()
            .font_family(mono)
            .text_xs()
            .line_height(DETAIL_LINE)
            .text_color(detail_color)
            .child(
                div()
                    .id(("device-port", ix))
                    .test_support()
                    .flex_shrink_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(port)),
            )
            .children(entry.usb_ids().into_iter().flat_map(|ids| {
                [
                    separator(),
                    div()
                        .id(("device-usb", ix))
                        .test_support()
                        .flex_none()
                        .child(SharedString::from(ids))
                        .into_any_element(),
                ]
            }))
            .children(summary.into_iter().flat_map(|summary| {
                [
                    separator(),
                    div()
                        .id(("device-summary", ix))
                        .test_support()
                        .flex_none()
                        .child(SharedString::from(summary))
                        .into_any_element(),
                ]
            }));

        let theme = cx.theme();
        v_flex()
            .id(("device-row", ix))
            .test_support()
            .group(group)
            .relative()
            .flex_none()
            .w_full()
            .h(chrome::TWO_LINE_ROW_HEIGHT)
            .pl(if simulated { px(20.) } else { px(12.) })
            .pr_2()
            .py(LINE_PAD)
            .overflow_hidden()
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
                    .flex_none()
                    .w_full()
                    .h(NAME_LINE)
                    .gap_2()
                    .items_center()
                    .child(chrome::state_dot(dot_color, hollow))
                    .child(
                        div()
                            .id(("device-name", ix))
                            .test_support()
                            .flex_1()
                            .min_w(px(40.))
                            .truncate()
                            .text_sm()
                            .line_height(NAME_LINE)
                            .text_color(name_color)
                            .when(!entry.present, |name| name.italic())
                            .child(name),
                    )
                    .child(chips),
            )
            .child(details)
            .child(actions)
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
                this.list.select_index(ix);
                this.prefill_baud(window, cx);
                if event.click_count() >= 2 {
                    this.connect_selected(cx);
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// The header of the simulated devices' group, which folds it.
    fn render_group(&self, count: usize, open: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id("device-group-simulated")
            .test_support()
            .flex_none()
            .w_full()
            .h(chrome::ROW_HEIGHT)
            .pl_3()
            .pr_3()
            .gap_1()
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size_3()
                .text_color(theme.muted_foreground),
            )
            .child(chrome::section_label("Simulated", cx))
            .child(
                div()
                    .ml_auto()
                    .text_size(chrome::LABEL_SIZE)
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(count.to_string())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.simulated_open = Some(!open);
                cx.notify();
            }))
            .into_any_element()
    }
}

impl Render for DevicesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reveal_selected_row(cx);
        let rows = self.rows();
        let present = self.list.entries().iter().filter(|e| e.present).count();
        let can_connect = self.list.selected().is_some_and(|e| e.present);
        let muted = cx.theme().muted_foreground;

        let header = chrome::panel_header("Devices", cx).child(
            div()
                .ml_auto()
                .pr_2()
                .text_size(chrome::LABEL_SIZE)
                .text_color(muted)
                .child(SharedString::from(format!("{present} available"))),
        );

        let body =
            if self.list.is_empty() {
                v_flex()
                    .flex_1()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .text_sm()
                    .text_color(muted)
                    .child("No serial ports found.")
                    .child(div().text_xs().child(
                        "Plug in a device, or start with --virtual <name> or --port <path>.",
                    ))
                    .into_any_element()
            } else {
                // A plain scrolling column rather than a uniform list: the rows are 40 px
                // and the "Simulated" header 28, and a uniform list gives every item the
                // first one's height. A list has tens of rows, so nothing is lost by
                // drawing them all.
                let rows: Vec<AnyElement> = rows
                    .into_iter()
                    .map(|row| match row {
                        DeviceRow::Entry(ix) => self.render_row(ix, &self.list.entries()[ix], cx),
                        DeviceRow::Simulated { count, open } => self.render_group(count, open, cx),
                    })
                    .collect();
                div()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("device-list")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .children(rows),
                    )
                    .into_any_element()
            };

        let theme = cx.theme();
        let footer = v_flex()
            .id("devices-footer")
            .flex_none()
            .gap_1()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.baud).id("baud-input").small()),
                    )
                    .child(
                        Button::new("connect")
                            .icon(IconName::Plug)
                            .label("Connect")
                            .small()
                            .primary()
                            .disabled(!can_connect)
                            .tooltip_with_action(
                                "Open the selected port at this rate",
                                &Connect,
                                Some(context::DEVICES_PANEL),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.connect_selected(cx);
                            })),
                    ),
            )
            .when_some(self.notice.clone(), |this, notice| {
                this.child(div().text_xs().text_color(theme.danger).child(notice))
            })
            .test_support();

        v_flex()
            .id("devices-panel")
            .test_support()
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

/// A simulated port (`--virtual`), listed in the "Simulated" group.
pub fn is_simulated(info: &PortInfo) -> bool {
    matches!(info.kind, PortKind::Virtual) || info.id.as_str().starts_with("virtual:")
}

/// A row of the Devices list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceRow {
    /// The entry at this index of [`DeviceList::entries`].
    Entry(usize),
    /// The header of the simulated devices, which fold.
    Simulated { count: usize, open: bool },
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
                settings: None,
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
