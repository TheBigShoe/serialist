//! The Devices section: the `devices` profiles, listed in the order they are tried, and
//! the form that adds or edits one.
//!
//! The form is the port settings form (rate, framing, flow control, line ending) plus a
//! name, the `match` keys (with a picker that fills them from a port listed now, and the
//! listed ports' values as hints), a plugin picker over the installed plugins and an
//! `on_connect` picker over the scripts folder. A profile is written whole when the form
//! is saved, not key by key: a half-typed `vid` would match the wrong devices meanwhile.
//! A line setting the profile did not set and the form leaves at the global default is
//! not written, so the profile keeps following the default.
//!
//! Rows reorder by dragging (the first profile that matches wins), which writes the
//! list again in its new order.

use serde_json::{Map, Value, json};
use serialist_core::{
    DataBits, DeviceProfile, FlowControl, LineEnding, Parity, PortInfo, PortKind, SerialConfig,
    Settings, StopBits,
};

use super::SettingsView;
use crate::chrome;
use crate::config::Config;
use crate::dialog_footer::DialogButtons;
use crate::port_settings::{PortSettings, PortSettingsEvent, PortSettingsForm};
use crate::prelude::*;
use crate::settings_io::Origin;

/// What the plugin and script pickers call "none".
const NONE: &str = "None";

/// The Devices section's state beside the files.
#[derive(Default)]
pub(super) struct DevicesState {
    /// The profile form, while its dialog is open.
    editor: Option<Entity<ProfileEditor>>,
}

/// A profile row being dragged to a new place.
#[derive(Clone)]
struct DraggedProfile {
    index: usize,
    title: SharedString,
}

impl Render for DraggedProfile {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .px_3()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .child(self.title.clone())
    }
}

/// A one-line account of what a profile matches: `vid 0x0e8d · product "Airoha"`.
pub fn match_summary(profile: &DeviceProfile) -> String {
    let m = &profile.r#match;
    let mut parts = Vec::new();
    if let Some(vid) = m.vid {
        parts.push(format!("vid {vid}"));
    }
    if let Some(pid) = m.pid {
        parts.push(format!("pid {pid}"));
    }
    for (key, value) in [
        ("product", &m.product),
        ("manufacturer", &m.manufacturer),
        ("serial", &m.serial_number),
        ("path", &m.path),
    ] {
        if let Some(value) = value {
            parts.push(format!("{key} \"{value}\""));
        }
    }
    if parts.is_empty() {
        "every port".to_owned()
    } else {
        parts.join(" \u{b7} ")
    }
}

/// What a profile sets, against the global defaults: `921600 8E1 · CR`.
fn settings_summary(profile: &DeviceProfile, settings: &Settings) -> String {
    let base = SerialConfig {
        baud: settings.default_baud,
        ..SerialConfig::default()
    };
    let serial = profile.apply_to(&base);
    let eol = profile.eol.unwrap_or(settings.line_ending);
    format!("{} \u{b7} {}", serial.summary(), eol.label())
}

impl SettingsView {
    /// The user's `devices` array as JSON, or an empty one.
    fn user_devices(&self) -> Vec<Value> {
        match self
            .files
            .as_ref()
            .and_then(|files| files.user_value("/devices"))
        {
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        }
    }

    /// The profile form, while it is open.
    pub fn profile_editor(&self) -> Option<&Entity<ProfileEditor>> {
        self.devices.editor.as_ref()
    }

    /// Open the profile form on profile `index`, or on a new profile (`None`).
    pub fn open_profile_editor(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.locked("/devices") {
            return;
        }
        let settings = self.settings();
        let profile = index
            .and_then(|ix| settings.devices.get(ix).cloned())
            .unwrap_or_default();
        let original = index.and_then(|ix| self.user_devices().get(ix).cloned());
        let ports = self
            .port_source
            .as_ref()
            .map(|source| source.snapshot())
            .unwrap_or_default();
        let (plugins, scripts) = cx
            .try_global::<Config>()
            .map(|config| {
                (
                    config
                        .codecs()
                        .plugins()
                        .iter()
                        .map(|plugin| plugin.name.clone())
                        .collect::<Vec<_>>(),
                    config
                        .scripts()
                        .iter()
                        .map(|script| script.relative.clone())
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        let seed = ProfileSeed {
            index,
            profile,
            original,
            defaults: (settings.default_baud, settings.line_ending),
            ports,
            plugins,
            scripts,
        };
        let editor = cx.new(|cx| ProfileEditor::new(seed, window, cx));
        self.devices.editor = Some(editor.clone());
        let title: SharedString = if index.is_some() {
            "Edit device profile".into()
        } else {
            "New device profile".into()
        };
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, _| {
            let save = view.clone();
            let closed = view.clone();
            dialog
                .title(title.clone())
                .w(px(600.))
                .child(editor.clone())
                .footer(DialogButtons::new("Save"))
                // A profile that does not check keeps the dialog open with the reason.
                .on_ok(move |_, window, cx| {
                    save.update(cx, |view, cx| view.save_profile(window, cx))
                        .unwrap_or(true)
                })
                .on_close(move |_, _, cx| {
                    closed
                        .update(cx, |view, cx| {
                            view.devices.editor = None;
                            cx.notify();
                        })
                        .ok();
                })
        });
        cx.notify();
    }

    /// Write the profile form's profile: in its place, or after the others for a new
    /// one. Returns whether it was written (the dialog then closes).
    pub fn save_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(editor) = self.devices.editor.clone() else {
            return true;
        };
        let (index, profile) = match editor.read(cx).to_json(cx) {
            Ok(profile) => (editor.read(cx).index, profile),
            Err(message) => {
                editor.update(cx, |editor, cx| editor.set_error(Some(message), cx));
                return false;
            }
        };
        let mut devices = self.user_devices();
        match index {
            Some(ix) if ix < devices.len() => {
                self.write_profile_at(&format!("/devices/{ix}"), profile, window, cx);
            }
            _ => {
                devices.push(profile);
                self.write("/devices", Some(Value::Array(devices)), window, cx);
            }
        }
        match self.errors.get("/devices").cloned() {
            Some(message) => {
                editor.update(cx, |editor, cx| editor.set_error(Some(message), cx));
                false
            }
            None => {
                tracing::info!(?index, "saved a device profile");
                true
            }
        }
    }

    /// Write one profile in place, reporting a problem under the list.
    fn write_profile_at(
        &mut self,
        pointer: &str,
        profile: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write(pointer, Some(profile), window, cx);
        if let Some(message) = self.errors.remove(pointer) {
            self.errors.insert("/devices".to_owned(), message);
        } else {
            self.errors.remove("/devices");
        }
    }

    /// Remove profile `index`.
    pub fn remove_profile(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut devices = self.user_devices();
        if index >= devices.len() {
            return;
        }
        devices.remove(index);
        let value = (!devices.is_empty()).then_some(Value::Array(devices));
        self.write("/devices", value, window, cx);
    }

    /// Move profile `from` to `to`, which changes which one a port gets first.
    pub fn move_profile(
        &mut self,
        from: usize,
        to: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut devices = self.user_devices();
        if from >= devices.len() || from == to {
            return;
        }
        let profile = devices.remove(from);
        devices.insert(to.min(devices.len()), profile);
        self.write("/devices", Some(Value::Array(devices)), window, cx);
    }

    pub(super) fn render_devices(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let settings = self.settings();
        let locked = self.locked("/devices");
        let theme = cx.theme();
        let (muted, border, drag_border, hover) = (
            theme.muted_foreground,
            theme.border,
            theme.drag_border,
            theme.list_hover,
        );
        let installed = |name: &str| {
            cx.try_global::<Config>()
                .is_some_and(|config| config.codec_registry().contains(name.trim()))
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, profile) in settings.devices.iter().enumerate() {
            let title: SharedString = profile
                .name
                .clone()
                .unwrap_or_else(|| "Unnamed profile".to_owned())
                .into();
            let detail = format!(
                "{} \u{2014} {}",
                match_summary(profile),
                settings_summary(profile, &settings)
            );
            let plugin = profile.plugin.clone().map(|plugin| {
                let color = if installed(&plugin) {
                    cx.theme().info
                } else {
                    muted
                };
                chrome::chip(color).child(SharedString::from(plugin))
            });
            let script = profile
                .on_connect
                .as_ref()
                .map(|path| chrome::quiet_chip(path.display().to_string(), cx));
            let dragged = DraggedProfile {
                index,
                title: title.clone(),
            };
            rows.push(
                h_flex()
                    .id(("settings-profile", index))
                    .test_support()
                    .w_full()
                    .h(chrome::TWO_LINE_ROW_HEIGHT)
                    .px_1()
                    .gap_2()
                    .items_center()
                    .rounded(px(4.))
                    .hover(|row| row.bg(hover))
                    .child(
                        div()
                            .id(("settings-profile-grip", index))
                            .flex_none()
                            .text_color(muted)
                            .when(!locked, |grip| grip.cursor_grab())
                            .child(Icon::new(IconName::GripVertical).size_4()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().truncate().child(title))
                            .child(
                                div()
                                    .text_xs()
                                    .truncate()
                                    .text_color(muted)
                                    .child(SharedString::from(detail)),
                            ),
                    )
                    .children(plugin)
                    .children(script)
                    .when(!locked, |row| {
                        row.child(
                            chrome::icon_button(("settings-profile-edit", index), IconName::Pencil, cx)
                                .xsmall()
                                .tooltip("Edit")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_profile_editor(Some(index), window, cx);
                                })),
                        )
                        .child(
                            chrome::icon_button(("settings-profile-remove", index), IconName::Trash, cx)
                                .xsmall()
                                .tooltip("Remove")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.remove_profile(index, window, cx);
                                })),
                        )
                        .on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
                        .drag_over::<DraggedProfile>(move |style, _, _, _| {
                            style.border_t_2().border_color(drag_border)
                        })
                        .on_drop(cx.listener(move |this, dragged: &DraggedProfile, window, cx| {
                            this.move_profile(dragged.index, index, window, cx);
                        }))
                    })
                    .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .py_2()
                    .text_sm()
                    .text_color(muted)
                    .child("No profiles: every port opens at the default baud, 8N1.")
                    .into_any_element(),
            );
        }
        let origin = self.origin("/devices");
        let note = self
            .errors
            .get("/devices")
            .cloned()
            .map(|error| (error, cx.theme().danger))
            .or_else(|| match &origin {
                Origin::Project(path) => Some((
                    format!("Read-only here: the profiles are set in {}", path.display()),
                    muted,
                )),
                _ => None,
            });
        let list = v_flex()
            .w_full()
            .gap_1()
            .py_1()
            .border_t_1()
            .border_b_1()
            .border_color(border)
            .children(rows);
        let actions = h_flex()
            .gap_2()
            .pt_1()
            .child(
                Button::new("settings-profile-add")
                    .icon(IconName::Plus)
                    .label("Add profile")
                    .small()
                    .disabled(locked)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_profile_editor(None, window, cx);
                    })),
            )
            .when(matches!(origin, Origin::User), |row| {
                row.child(
                    Button::new("settings-profiles-reset")
                        .icon(IconName::RotateCcw)
                        .label("Remove all")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.reset("/devices", window, cx);
                        })),
                )
            });
        vec![
            v_flex()
                .w_full()
                .gap_2()
                .pt_4()
                .child(chrome::section_label("Profiles", cx))
                .child(list)
                .children(note.map(|(text, color)| {
                    div()
                        .id("settings-profiles-note")
                        .test_support()
                        .text_xs()
                        .text_color(color)
                        .child(SharedString::from(text))
                }))
                .child(actions)
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Drag a profile to change the order they are tried in."),
                )
                .into_any_element(),
        ]
    }
}

// --- The profile form ----------------------------------------------------------------------

/// What the form opens with.
struct ProfileSeed {
    index: Option<usize>,
    profile: DeviceProfile,
    /// The profile's JSON in the user's file, whose keys are kept.
    original: Option<Value>,
    /// `default_baud` and `line_ending`: a line setting left at these is not written.
    defaults: (u32, LineEnding),
    ports: Vec<PortInfo>,
    plugins: Vec<String>,
    scripts: Vec<String>,
}

/// A `match` key the form edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MatchKey {
    Vid,
    Pid,
    Product,
    Manufacturer,
    SerialNumber,
    Path,
}

impl MatchKey {
    pub const ALL: [MatchKey; 6] = [
        MatchKey::Vid,
        MatchKey::Pid,
        MatchKey::Product,
        MatchKey::Manufacturer,
        MatchKey::SerialNumber,
        MatchKey::Path,
    ];

    fn key(self) -> &'static str {
        match self {
            MatchKey::Vid => "vid",
            MatchKey::Pid => "pid",
            MatchKey::Product => "product",
            MatchKey::Manufacturer => "manufacturer",
            MatchKey::SerialNumber => "serial_number",
            MatchKey::Path => "path",
        }
    }

    fn label(self) -> &'static str {
        match self {
            MatchKey::Vid => "USB vendor id",
            MatchKey::Pid => "USB product id",
            MatchKey::Product => "Product contains",
            MatchKey::Manufacturer => "Maker contains",
            MatchKey::SerialNumber => "Serial contains",
            MatchKey::Path => "Path starts with",
        }
    }

    /// What `port` has for this key, as the field takes it.
    fn of(self, port: &PortInfo) -> Option<String> {
        let usb = match &port.kind {
            PortKind::Usb(usb) => Some(usb),
            _ => None,
        };
        match self {
            MatchKey::Vid => usb.map(|usb| format!("0x{:04x}", usb.vid)),
            MatchKey::Pid => usb.map(|usb| format!("0x{:04x}", usb.pid)),
            MatchKey::Product => usb.and_then(|usb| usb.product.clone()),
            MatchKey::Manufacturer => usb.and_then(|usb| usb.manufacturer.clone()),
            MatchKey::SerialNumber => usb.and_then(|usb| usb.serial_number.clone()),
            MatchKey::Path => Some(port.id.to_string()),
        }
    }

    fn of_profile(self, profile: &DeviceProfile) -> Option<String> {
        let m = &profile.r#match;
        match self {
            MatchKey::Vid => m.vid.map(|id| id.to_string()),
            MatchKey::Pid => m.pid.map(|id| id.to_string()),
            MatchKey::Product => m.product.clone(),
            MatchKey::Manufacturer => m.manufacturer.clone(),
            MatchKey::SerialNumber => m.serial_number.clone(),
            MatchKey::Path => m.path.clone(),
        }
    }
}

/// A USB id typed as hex (`0x0e8d`, `0e8d`), written in the `0x` form.
fn usb_id(text: &str, what: &str) -> Result<String, String> {
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u16::from_str_radix(digits, 16)
        .map(|id| format!("0x{id:04x}"))
        .map_err(|_| format!("{what} {text:?} is not a hex id up to 0xffff"))
}

/// The form for one device profile, shown in a dialog.
pub struct ProfileEditor {
    /// The profile it edits; `None` for a new one.
    index: Option<usize>,
    original: Option<Value>,
    defaults: (u32, LineEnding),
    name: Entity<InputState>,
    matches: Vec<(MatchKey, Entity<InputState>)>,
    ports: Vec<PortInfo>,
    from_port: Entity<SelectState<Vec<String>>>,
    port_form: Entity<PortSettingsForm>,
    plugin: Entity<SelectState<Vec<String>>>,
    on_connect: Entity<SelectState<Vec<String>>>,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

fn picker(
    items: Vec<String>,
    selected: Option<&str>,
    window: &mut Window,
    cx: &mut Context<ProfileEditor>,
) -> Entity<SelectState<Vec<String>>> {
    let index = selected
        .and_then(|value| items.iter().position(|item| item == value))
        .map(IndexPath::new);
    cx.new(|cx| SelectState::new(items, index, window, cx).searchable(false))
}

impl ProfileEditor {
    fn new(seed: ProfileSeed, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let profile = &seed.profile;
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Shown in the Devices panel")
                .default_value(profile.name.clone().unwrap_or_default())
        });
        let hint_port = seed
            .ports
            .iter()
            .find(|port| matches!(port.kind, PortKind::Usb(_)))
            .or_else(|| seed.ports.first());
        let matches = MatchKey::ALL
            .into_iter()
            .map(|key| {
                let hint = hint_port
                    .and_then(|port| key.of(port))
                    .map(|value| format!("e.g. {value}"))
                    .unwrap_or_default();
                let value = key.of_profile(profile).unwrap_or_default();
                let input =
                    cx.new(|cx| InputState::new(window, cx).placeholder(hint).default_value(value));
                (key, input)
            })
            .collect();
        let port_names: Vec<String> = seed
            .ports
            .iter()
            .map(|port| format!("{} ({})", port.display_name, port.id))
            .collect();
        let from_port = picker(port_names, None, window, cx);
        let base = SerialConfig {
            baud: seed.defaults.0,
            ..SerialConfig::default()
        };
        let line = PortSettings::new(
            profile.apply_to(&base),
            profile.eol.unwrap_or(seed.defaults.1),
            false,
        );
        let port_form = cx.new(|cx| PortSettingsForm::for_profile(line, window, cx));

        let mut plugins = vec![NONE.to_owned()];
        plugins.extend(seed.plugins.iter().cloned());
        if let Some(named) = &profile.plugin
            && !plugins.contains(named)
        {
            plugins.push(named.clone());
        }
        let plugin = picker(
            plugins,
            Some(profile.plugin.as_deref().unwrap_or(NONE)),
            window,
            cx,
        );
        let mut scripts = vec![NONE.to_owned()];
        scripts.extend(seed.scripts.iter().cloned());
        let current_script = profile
            .on_connect
            .as_ref()
            .map(|path| path.display().to_string());
        if let Some(named) = &current_script
            && !scripts.contains(named)
        {
            scripts.push(named.clone());
        }
        let on_connect = picker(
            scripts,
            Some(current_script.as_deref().unwrap_or(NONE)),
            window,
            cx,
        );

        let subscriptions = vec![
            cx.subscribe_in(
                &from_port,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, window, cx| {
                    if let SelectEvent::Confirm(Some(name)) = event {
                        this.fill_from_port(name, window, cx);
                    }
                },
            ),
            // The form's own events keep its settings; this only clears a stale error.
            cx.subscribe(&port_form, |this, _, _: &PortSettingsEvent, cx| {
                this.set_error(None, cx);
            }),
        ];
        Self {
            index: seed.index,
            original: seed.original,
            defaults: seed.defaults,
            name,
            matches,
            ports: seed.ports,
            from_port,
            port_form,
            plugin,
            on_connect,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    /// The profile it edits; `None` for a new one.
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, error: Option<String>, cx: &mut Context<Self>) {
        if self.error != error {
            self.error = error;
            cx.notify();
        }
    }

    /// The line settings form.
    pub fn port_form(&self) -> &Entity<PortSettingsForm> {
        &self.port_form
    }

    pub fn name_input(&self) -> &Entity<InputState> {
        &self.name
    }

    pub fn match_input(&self, key: MatchKey) -> &Entity<InputState> {
        &self
            .matches
            .iter()
            .find(|(k, _)| *k == key)
            .expect("every match key has a field")
            .1
    }

    /// Set a field, as typing in it does.
    pub fn set_text(
        input: &Entity<InputState>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        input.update(cx, |input, cx| input.set_value(text.to_owned(), window, cx));
    }

    /// Pick the plugin (`None` for none).
    pub fn set_plugin(&mut self, name: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let value = name.unwrap_or(NONE).to_owned();
        self.plugin
            .update(cx, |select, cx| select.set_selected_value(&value, window, cx));
    }

    /// Fill the match fields from the listed port `name` (as the picker writes it): its
    /// USB ids and product for a USB port, else its path.
    pub fn fill_from_port(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(port) = self
            .ports
            .iter()
            .find(|port| format!("{} ({})", port.display_name, port.id) == name)
            .cloned()
        else {
            return;
        };
        let usb = matches!(port.kind, PortKind::Usb(_));
        for (key, input) in &self.matches {
            let value = match key {
                MatchKey::Vid | MatchKey::Pid | MatchKey::Product if usb => key.of(&port),
                MatchKey::Path if !usb => key.of(&port),
                _ => None,
            }
            .unwrap_or_default();
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        if self.name.read(cx).value().trim().is_empty() {
            let title = port.display_name.clone();
            self.name
                .update(cx, |input, cx| input.set_value(title, window, cx));
        }
        cx.notify();
    }

    fn kept(&self, key: &str) -> bool {
        self.original
            .as_ref()
            .and_then(|original| original.get(key))
            .is_some()
    }

    /// The profile as the file will hold it, checked as the loader checks it.
    pub fn to_json(&self, cx: &App) -> Result<Value, String> {
        let mut out = Map::new();
        let name = self.name.read(cx).value().trim().to_owned();
        if !name.is_empty() {
            out.insert("name".into(), json!(name));
        }
        let mut matched = Map::new();
        for (key, input) in &self.matches {
            let text = input.read(cx).value().trim().to_owned();
            if text.is_empty() {
                continue;
            }
            let value = match key {
                MatchKey::Vid => json!(usb_id(&text, "Vendor id")?),
                MatchKey::Pid => json!(usb_id(&text, "Product id")?),
                _ => json!(text),
            };
            matched.insert(key.key().into(), value);
        }
        out.insert("match".into(), Value::Object(matched));

        let line = self.port_form.read(cx);
        let baud_text = line.baud_text(cx);
        let settings = line.settings();
        if baud_text.trim() != settings.serial.baud.to_string() {
            return Err(format!("Baud: {baud_text:?} is not applied; press Enter in the field"));
        }
        let serial = &settings.serial;
        let (default_baud, default_eol) = self.defaults;
        let defaults = SerialConfig::default();
        let mut put = |key: &str, differs: bool, value: Value| {
            if differs || self.kept(key) {
                out.insert(key.into(), value);
            }
        };
        put("baud", serial.baud != default_baud, json!(serial.baud));
        put(
            "data_bits",
            serial.data_bits != defaults.data_bits,
            json!(match serial.data_bits {
                DataBits::Five => 5,
                DataBits::Six => 6,
                DataBits::Seven => 7,
                DataBits::Eight => 8,
            }),
        );
        put(
            "parity",
            serial.parity != defaults.parity,
            json!(match serial.parity {
                Parity::None => "none",
                Parity::Odd => "odd",
                Parity::Even => "even",
                Parity::Mark => "mark",
                Parity::Space => "space",
            }),
        );
        put(
            "stop_bits",
            serial.stop_bits != defaults.stop_bits,
            json!(match serial.stop_bits {
                StopBits::One => 1,
                StopBits::Two => 2,
            }),
        );
        put(
            "flow_control",
            serial.flow_control != defaults.flow_control,
            json!(match serial.flow_control {
                FlowControl::None => "none",
                FlowControl::Hardware => "hardware",
                FlowControl::Software => "software",
            }),
        );
        let eol = settings.line_ending;
        put(
            "eol",
            eol != default_eol,
            json!(match eol {
                LineEnding::None => "none",
                LineEnding::Cr => "cr",
                LineEnding::Lf => "lf",
                LineEnding::Crlf => "crlf",
            }),
        );
        let picked = |select: &Entity<SelectState<Vec<String>>>| {
            select
                .read(cx)
                .selected_value()
                .filter(|value| value.as_str() != NONE)
                .cloned()
        };
        if let Some(plugin) = picked(&self.plugin) {
            out.insert("plugin".into(), json!(plugin));
        }
        if let Some(script) = picked(&self.on_connect) {
            out.insert("on_connect".into(), json!(script));
        }
        let value = Value::Object(out);
        profile_json(&value)?;
        Ok(value)
    }
}

/// `value` as a device profile, or why the loader would refuse it.
pub fn profile_json(value: &Value) -> Result<DeviceProfile, String> {
    serde_json::from_value::<DeviceProfile>(value.clone()).map_err(|error| error.to_string())
}

impl Render for ProfileEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let label = |text: &'static str| {
            div()
                .w(px(120.))
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(text)
        };
        let row = |text: &'static str, control: AnyElement| {
            h_flex()
                .gap_2()
                .items_center()
                .child(label(text))
                .child(div().flex_1().min_w_0().child(control))
        };
        let match_rows: Vec<_> = self
            .matches
            .iter()
            .map(|(key, input)| {
                row(
                    key.label(),
                    Input::new(input)
                        .id(SharedString::from(format!("profile-match-{}", key.key())))
                        .small()
                        .into_any_element(),
                )
            })
            .collect();
        v_flex()
            .id("profile-editor")
            .gap_2()
            .child(row(
                "Name",
                Input::new(&self.name)
                    .id("profile-name")
                    .small()
                    .into_any_element(),
            ))
            .child(div().pt_1().child(chrome::section_label("Match", cx)))
            .child(row(
                "From a port",
                Select::new(&self.from_port)
                    .small()
                    .placeholder(if self.ports.is_empty() {
                        "No port is listed"
                    } else {
                        "Fill from a listed port"
                    })
                    .disabled(self.ports.is_empty())
                    .into_any_element(),
            ))
            .children(match_rows)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Every key filled in must match; none matches every port."),
            )
            .child(
                h_flex()
                    .pt_1()
                    .gap_4()
                    .items_start()
                    .child(self.port_form.clone())
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_2()
                            .child(div().text_xs().text_color(muted).child("DECODING"))
                            .child(
                                Select::new(&self.plugin)
                                    .small()
                                    .placeholder("Plugin")
                                    .into_any_element(),
                            )
                            .child(div().text_xs().text_color(muted).child("ON CONNECT"))
                            .child(
                                Select::new(&self.on_connect)
                                    .small()
                                    .placeholder("Script")
                                    .into_any_element(),
                            ),
                    ),
            )
            .children(self.error.clone().map(|error| {
                div()
                    .id("profile-error")
                    .text_xs()
                    .text_color(danger)
                    .child(SharedString::from(error))
            }))
    }
}
