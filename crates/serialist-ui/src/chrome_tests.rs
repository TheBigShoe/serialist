//! The window's chrome against the real engine: the session toolbar's overflow menu, the
//! docks' breakpoints and saved widths, the Decoded panel following the codec, the
//! command palette, the Devices list scrolling above its fixed footer, and its two-line
//! rows.

use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, PortInfo, PortKind, UsbInfo};
use serialist_sim::{AtDevice, EchoDevice, LinkConfig, SimWorld};

use crate::actions::keys;
use crate::chrome::{ROW_HEIGHT, TWO_LINE_ROW_HEIGHT};
use crate::config::{self, Config};
use crate::devices_panel::DeviceRow;
use crate::docks::{CENTER_MIN, DockPanel, DockSide, RAIL_WIDTH};
use crate::prelude::*;
use crate::session_state::{SessionState, state_path};
use crate::session_view::SessionView;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, draw, has_rx_line, open_test_window_sized,
    race_config_dir, resize_window, run_until, type_line, wait_connected,
};
use crate::toolbar::{ToolbarItem, ToolbarLayout};
use crate::workspace::{AppOptions, Workspace};

const WIDE: (f32, f32) = (1440., 900.);

/// A workspace over `world` in a `size` window, opening `ports` at startup, with the
/// configuration under `dir` loaded if there is one.
fn open(
    cx: &mut TestAppContext,
    world: &SimWorld,
    dir: Option<&TestDir>,
    ports: &[&str],
    size: (f32, f32),
) -> (AnyWindowHandle, Entity<Workspace>) {
    open_selecting(cx, world, dir, ports, None, size)
}

/// [`open`], with `select` chosen in Devices (before its port is listed, as `--port` does).
fn open_selecting(
    cx: &mut TestAppContext,
    world: &SimWorld,
    dir: Option<&TestDir>,
    ports: &[&str],
    select: Option<&str>,
    size: (f32, f32),
) -> (AnyWindowHandle, Entity<Workspace>) {
    allow_engine_threads(cx);
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: select.map(PortId::new),
        open_ports: ports.iter().map(|port| PortId::new(*port)).collect(),
        store: None,
    };
    let paths = dir.map(|dir| ConfigPaths::new(dir.path()));
    let (window, workspace) = open_test_window_sized(cx, size, move |window, cx| {
        if let Some(paths) = paths {
            config::install(Config::load(paths, false), cx);
        }
        Workspace::new(options, window, cx)
    });
    cx.run_until_parked();
    (window, workspace)
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
    cx.run_until_parked();
}

fn shown(cx: &mut TestAppContext, workspace: &Entity<Workspace>, panel: DockPanel) -> bool {
    workspace.read_with(cx, |w, _| w.is_panel_shown(panel))
}

fn layout(cx: &mut TestAppContext, view: &Entity<SessionView>) -> ToolbarLayout {
    view.read_with(cx, |v, _| v.toolbar_layout().clone())
}

/// The id of the element that shows `item` in the toolbar (its right end, for the mode).
fn item_id(item: ToolbarItem) -> &'static str {
    match item {
        ToolbarItem::Mode => "mode-inline",
        ToolbarItem::Pause => "pause",
        ToolbarItem::Record => "record",
        ToolbarItem::Clear => "clear",
        ToolbarItem::Search => "search",
        ToolbarItem::Hex => "hex-view",
        ToolbarItem::Timestamps => "timestamps",
        ToolbarItem::Wrap => "wrap",
        ToolbarItem::Export => "export",
        ToolbarItem::Codec => "codec-picker",
    }
}

/// Every control the toolbar shows lies inside the toolbar, as drawn, and the layout's
/// estimate fits the width it was given.
fn assert_toolbar_fits(cx: &mut TestAppContext, window: AnyWindowHandle, layout: &ToolbarLayout) {
    assert!(
        layout.width <= layout.available,
        "estimated {} for {}",
        layout.width,
        layout.available
    );
    let mut ids: Vec<&str> = vec!["port-settings", "session-disconnect"];
    ids.extend(layout.shown.iter().map(|item| item_id(*item)));
    if !layout.overflow.is_empty() {
        ids.push("toolbar-overflow");
    }
    cx.update_window(window, |_, window, _| {
        let bar = window.find("session-toolbar").bounds();
        let viewport = window.viewport_size();
        assert!(
            bar.right() <= viewport.width + px(0.5),
            "{bar:?} in {viewport:?}"
        );
        let scoped = window.within("session-toolbar");
        for id in ids {
            let bounds = scoped.find(id).bounds();
            assert!(
                bounds.left() >= bar.left() - px(0.5) && bounds.right() <= bar.right() + px(0.5),
                "{id} at {bounds:?} is outside the toolbar at {bar:?}"
            );
        }
    })
    .unwrap();
}

#[gpui_test]
fn the_toolbar_moves_what_does_not_fit_into_its_overflow_menu(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let dir = race_config_dir("chrome-overflow");
    let (window, workspace) = open(cx, &world, Some(&dir), &["virtual:at"], WIDE);
    let view = wait_connected(cx, &workspace);
    // Decoding, as a device profile's plugin would: the codec menu names its codec.
    view.update(cx, |v, cx| assert!(v.set_codec(Some("airoha-race"), cx)));
    draw(cx, window);
    let wide = layout(cx, &view);
    assert!(
        wide.overflow.is_empty(),
        "everything fits at 1440: {wide:?}"
    );
    assert!(
        wide.shows(ToolbarItem::Codec),
        "the installed plugin is there to pick"
    );
    assert_toolbar_fits(cx, window, &wide);

    // 1024: the right dock is on its rail and the toolbar is short of room.
    resize_window(cx, window, (1024., 700.));
    let narrow = layout(cx, &view);
    assert!(
        !narrow.overflow.is_empty(),
        "at 1024 something goes to the overflow menu: {narrow:?}"
    );
    assert_eq!(
        narrow.overflow[0],
        ToolbarItem::Hex,
        "the view toggles go first"
    );
    for item in [ToolbarItem::Mode, ToolbarItem::Pause, ToolbarItem::Record] {
        assert!(narrow.shows(item), "{item:?} stays");
    }
    let mut every: Vec<ToolbarItem> = narrow.shown.clone();
    every.extend(&narrow.overflow);
    assert_eq!(every.len(), wide.shown.len(), "nothing is lost");
    assert_toolbar_fits(cx, window, &narrow);

    // Narrower still, with both docks on their rails: still inside.
    resize_window(cx, window, (720., 600.));
    let narrowest = layout(cx, &view);
    assert!(narrowest.overflow.len() >= narrow.overflow.len());
    assert_toolbar_fits(cx, window, &narrowest);

    // The overflow menu opens from its button, and Escape closes it.
    cx.update_window(window, |_, window, cx| window.click("toolbar-overflow", cx))
        .unwrap();
    cx.run_until_parked();
    draw(cx, window);
    press(cx, window, "escape");
}

#[gpui_test]
fn docks_collapse_below_their_breakpoints_and_keep_their_widths(cx: &mut TestAppContext) {
    let dir = TestDir::new("chrome-docks");
    std::fs::write(dir.join("settings.json"), "{}").unwrap();
    let world = SimWorld::new();
    let (window, workspace) = open(cx, &world, Some(&dir), &["virtual:at"], WIDE);
    wait_connected(cx, &workspace);
    assert!(shown(cx, &workspace, DockPanel::Devices));
    assert!(shown(cx, &workspace, DockPanel::Commands));
    assert!(
        !shown(cx, &workspace, DockPanel::Decoded),
        "no codec, no Decoded"
    );
    assert!(
        !shown(cx, &workspace, DockPanel::Scripts),
        "no script, no console"
    );

    // The Scripts rail opens the console; the docks are dragged to new widths.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(DockPanel::Scripts.rail_id(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, &workspace, DockPanel::Scripts));
    workspace.update(cx, |w, cx| {
        w.resize_dock(DockSide::Left, px(333.), cx);
        w.resize_dock(DockSide::Right, px(420.), cx);
    });
    draw(cx, window);
    cx.update_window(window, |_, window, _| {
        assert_eq!(window.find("left-dock").bounds().size.width, px(333.));
        assert_eq!(window.find("right-dock").bounds().size.width, px(420.));
    })
    .unwrap();

    // Below 1100 the right dock goes to its rail; the left stays.
    resize_window(cx, window, (1024., 700.));
    assert!(!shown(cx, &workspace, DockPanel::Scripts));
    assert!(shown(cx, &workspace, DockPanel::Devices));
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("right-dock").is_none());
        assert!(window.find("center").bounds().size.width >= CENTER_MIN);
    })
    .unwrap();

    // Below 900 the left one does too, and the center has the window.
    resize_window(cx, window, (860., 700.));
    assert!(!shown(cx, &workspace, DockPanel::Devices));
    assert!(!shown(cx, &workspace, DockPanel::Commands));
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("left-dock").is_none());
        assert_eq!(
            window.find("center").bounds().size.width,
            px(860.) - RAIL_WIDTH * 2.
        );
    })
    .unwrap();
    // The rail opens a panel in a narrow window too.
    cx.update_window(window, |_, window, cx| {
        window.click(DockPanel::Devices.rail_id(), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(shown(cx, &workspace, DockPanel::Devices));

    // Wide again: what the narrowing collapsed comes back.
    resize_window(cx, window, WIDE);
    assert!(shown(cx, &workspace, DockPanel::Scripts));
    assert!(shown(cx, &workspace, DockPanel::Commands));

    // Closing the window writes the widths to state.json with the tab...
    drop(workspace);
    cx.update_window(window, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    let state = SessionState::load(&state_path(&ConfigPaths::new(dir.path()))).expect("state.json");
    let docks = state.docks.expect("the docks");
    assert_eq!((docks.left_width, docks.right_width), (333., 420.));
    assert_eq!(state.tabs.len(), 1);

    // ...and the next start takes them back.
    let (_window, workspace) = open(cx, &world, Some(&dir), &[], WIDE);
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.docks().left_width(), px(333.));
        assert_eq!(w.docks().right_width(), px(420.));
        assert_eq!(w.tab_count(), 1, "and the tab");
    });
}

#[gpui_test]
fn the_decoded_panel_opens_with_a_codec_and_closes_without_one(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let dir = race_config_dir("chrome-decoded");
    let (window, workspace) = open(cx, &world, Some(&dir), &["virtual:at"], WIDE);
    let view = wait_connected(cx, &workspace);
    assert!(!shown(cx, &workspace, DockPanel::Decoded));

    view.update(cx, |v, cx| assert!(v.set_codec(Some("airoha-race"), cx)));
    cx.run_until_parked();
    assert!(shown(cx, &workspace, DockPanel::Decoded));
    assert!(
        view.read_with(cx, |v, _| v.hides_framed_bytes()),
        "framed bytes hide by default: the panel shows the frames"
    );
    draw(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("right-dock").is_some());
    })
    .unwrap();

    view.update(cx, |v, cx| v.set_codec(None, cx));
    cx.run_until_parked();
    assert!(!shown(cx, &workspace, DockPanel::Decoded));

    // The user's choice stands until the codec changes again.
    workspace.update(cx, |w, cx| w.set_panel_open(DockPanel::Decoded, true, cx));
    view.update(cx, |v, cx| v.set_decoded_inline(false, cx));
    cx.run_until_parked();
    assert!(shown(cx, &workspace, DockPanel::Decoded));
}

#[gpui_test]
fn the_palette_runs_an_action_and_a_saved_command(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (window, workspace) = open(cx, &world, None, &["virtual:at"], WIDE);
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "AT");
    run_until(cx, "OK", |cx| has_rx_line(cx, &view, "OK"));

    press(cx, window, keys::COMMAND_PALETTE);
    let palette = workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .expect("the palette is open");
    let (labels, clear_binding) = palette.read_with(cx, |p, _| {
        (
            p.labels(),
            p.matches()
                .find(|entry| entry.label == "Terminal: Clear")
                .and_then(|entry| entry.binding.clone()),
        )
    });
    assert!(
        labels.contains(&"Serial: Disconnect".to_owned()),
        "{labels:?}"
    );
    assert!(labels.contains(&"Send: ATI".to_owned()), "{labels:?}");
    assert!(clear_binding.is_some(), "Clear shows its keystrokes");

    // Typed into the palette's field, as a user would, then Enter.
    cx.update_window(window, |_, window, cx| window.input("terminal clear", cx))
        .unwrap();
    cx.run_until_parked();
    let selected = palette.read_with(cx, |p, _| p.selected().map(|e| e.label.clone()));
    assert_eq!(selected.as_deref(), Some("Terminal: Clear"));
    press(cx, window, "enter");
    assert!(
        workspace.read_with(cx, |w, _| w.palette().is_none()),
        "it closes"
    );
    run_until(cx, "the scrollback to clear", |cx| {
        displayed(cx, &view).is_empty()
    });

    // A saved command, found by its name and sent.
    press(cx, window, keys::COMMAND_PALETTE);
    cx.update_window(window, |_, window, cx| window.input("send ati", cx))
        .unwrap();
    cx.run_until_parked();
    let palette = workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .expect("open again");
    let selected = palette.read_with(cx, |p, _| p.selected().map(|e| e.label.clone()));
    assert_eq!(selected.as_deref(), Some("Send: ATI"));
    press(cx, window, "enter");
    run_until(cx, "the modem's identity", |cx| {
        has_rx_line(cx, &view, AtDevice::DEFAULT_IDENTITY)
    });

    // Down moves the selection; Escape closes without running anything.
    press(cx, window, keys::COMMAND_PALETTE);
    let palette = workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .unwrap();
    let first = palette.read_with(cx, |p, _| p.selected().map(|e| e.label.clone()));
    press(cx, window, "down");
    let second = palette.read_with(cx, |p, _| p.selected().map(|e| e.label.clone()));
    assert_ne!(first, second);
    press(cx, window, "escape");
    assert!(workspace.read_with(cx, |w, _| w.palette().is_none()));
}

#[gpui_test]
fn the_devices_list_scrolls_above_its_footer(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    for (name, display) in [("extra-one", "Extra one"), ("extra-two", "Extra two")] {
        world.add_virtual(name, display, LinkConfig::default(), || {
            Box::new(EchoDevice::new())
        });
    }
    let (window, workspace) = open(cx, &world, None, &[], (1280., 720.));
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let ports = world.port_source().snapshot().len();
    run_until(cx, "every port listed", |cx| {
        devices.read_with(cx, |d, _| d.list().len() == ports)
    });
    draw(cx, window);
    let rows = devices.read_with(cx, |d, _| d.rows());
    assert!(
        matches!(rows[0], DeviceRow::Simulated { open: true, .. }),
        "only simulated ports: their group is open"
    );
    let DeviceRow::Entry(last) = *rows.last().unwrap() else {
        panic!("the last row is a port");
    };

    let bounds = |cx: &mut TestAppContext, id: ElementId| {
        cx.update_window(window, |_, window, _| {
            window.try_find(id).map(|found| found.bounds())
        })
        .unwrap()
    };
    let panel = bounds(cx, "devices-panel".into()).expect("the panel");
    let footer = bounds(cx, "devices-footer".into()).expect("the footer");
    assert!(
        footer.bottom() <= panel.bottom() + px(0.5),
        "{footer:?} in {panel:?}"
    );
    // A port is a two-line row; the group's header is still a one-line one.
    let header = bounds(cx, "device-group-simulated".into()).expect("the group header");
    assert_eq!(header.size.height, ROW_HEIGHT);
    let DeviceRow::Entry(first) = rows[1] else {
        panic!("a port follows the header");
    };
    let first_row = bounds(cx, ("device-row", first).into()).expect("the first port's row");
    assert_eq!(first_row.size.height, TWO_LINE_ROW_HEIGHT);
    assert_eq!(first_row.top(), header.bottom(), "rows follow one another");
    let hidden = bounds(cx, ("device-row", last).into());
    assert!(
        hidden.is_none_or(|row| row.top() >= footer.top()),
        "{} rows do not fit above the footer: {hidden:?}",
        rows.len()
    );

    // Down to the last port: the list scrolls, the footer stays.
    for _ in 0..rows.len() {
        press(cx, window, "down");
    }
    assert_eq!(
        devices.read_with(cx, |d, _| d.list().selected_index()),
        Some(last)
    );
    draw(cx, window);
    let row = bounds(cx, ("device-row", last).into()).expect("the last row, scrolled to");
    let footer_after = bounds(cx, "devices-footer".into()).expect("the footer");
    assert_eq!(footer_after, footer, "the footer does not move");
    assert!(
        row.bottom() <= footer.top() + px(0.5) && row.top() >= panel.top(),
        "{row:?} shows above the footer at {footer:?}"
    );

    // A real device folds the simulated ones away until asked for.
    world.add_device(
        PortInfo {
            id: PortId::new("/dev/cu.usbserial-1420"),
            kind: PortKind::Usb(UsbInfo {
                vid: 0x0403,
                pid: 0x6001,
                serial_number: None,
                manufacturer: None,
                product: Some("FT232R".into()),
            }),
            display_name: "FT232R".into(),
        },
        LinkConfig::default(),
        || Box::new(EchoDevice::new()),
    );
    run_until(cx, "the adapter listed", |cx| {
        devices.read_with(cx, |d, _| d.list().len() == ports + 1)
    });
    devices.update(cx, |d, cx| d.set_simulated_open(false, cx));
    let rows = devices.read_with(cx, |d, _| d.rows());
    assert_eq!(rows.len(), 2, "the adapter and the folded group: {rows:?}");
    // Selecting a simulated port unfolds them.
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| {
            d.select_port(PortId::new("virtual:echo"), window, cx)
        });
    })
    .unwrap();
    assert!(devices.read_with(cx, |d, _| d.simulated_is_open()));
}

/// Where the parts of a Devices row are, as drawn.
struct RowParts {
    row: Bounds<Pixels>,
    name: Bounds<Pixels>,
    chip: Bounds<Pixels>,
    details: Bounds<Pixels>,
    port: Bounds<Pixels>,
    usb: Bounds<Pixels>,
    summary: Bounds<Pixels>,
    actions: Bounds<Pixels>,
}

fn row_parts(cx: &mut TestAppContext, window: AnyWindowHandle, ix: usize) -> RowParts {
    cx.update_window(window, |_, window, _| {
        let bounds = |id: &'static str| window.find((id, ix)).bounds();
        RowParts {
            row: bounds("device-row"),
            name: bounds("device-name"),
            chip: bounds("device-plugin"),
            details: bounds("device-detail"),
            port: bounds("device-port"),
            usb: bounds("device-usb"),
            summary: bounds("device-summary"),
            actions: bounds("device-actions"),
        }
    })
    .unwrap()
}

fn assert_inside(inner: Bounds<Pixels>, outer: Bounds<Pixels>, what: &str) {
    let slack = px(0.5);
    assert!(
        inner.left() >= outer.left() - slack
            && inner.right() <= outer.right() + slack
            && inner.top() >= outer.top() - slack
            && inner.bottom() <= outer.bottom() + slack,
        "{what}: {inner:?} is not inside {outer:?}"
    );
}

#[gpui_test]
fn a_device_row_keeps_its_two_lines_apart(cx: &mut TestAppContext) {
    // A long product name and a long port id on a USB device a profile gives a codec:
    // the first line holds the name and the chip, the second the port, its ids and the
    // settings of the selected row, and none of them may touch another.
    let dir = TestDir::new("chrome-device-row");
    std::fs::write(
        dir.join("settings.json"),
        r#"{ "devices": [ { "name": "Airoha Headphones Charging Case Development Board",
                            "match": { "vid": "0x0e8d", "pid": "0x2000" },
                            "plugin": "airoha-race" } ] }"#,
    )
    .unwrap();
    let world = SimWorld::new();
    let port = PortId::new("/dev/cu.usbmodem-AIROHA-CASE-2000-0001");
    world.add_device(
        PortInfo {
            id: port.clone(),
            kind: PortKind::Usb(UsbInfo {
                vid: 0x0e8d,
                pid: 0x2000,
                serial_number: None,
                manufacturer: None,
                product: Some("Airoha Headphones Charging Case".into()),
            }),
            display_name: "Airoha Headphones Charging Case".into(),
        },
        LinkConfig::default(),
        || Box::new(EchoDevice::new()),
    );
    let (window, workspace) = open(cx, &world, Some(&dir), &[], WIDE);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let ports = world.port_source().snapshot().len();
    run_until(cx, "every port listed", |cx| {
        devices.read_with(cx, |d, _| d.list().len() == ports)
    });
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| d.select_port(port.clone(), window, cx));
    })
    .unwrap();
    draw(cx, window);
    let ix = devices
        .read_with(cx, |d, _| {
            d.list().entries().iter().position(|e| e.info.id == port)
        })
        .expect("the adapter is listed");

    let at_rest = row_parts(cx, window, ix);
    let RowParts {
        row,
        name,
        chip,
        details,
        port: port_text,
        usb,
        summary,
        actions,
    } = &at_rest;
    assert_eq!(row.size.height, TWO_LINE_ROW_HEIGHT);
    for (part, what) in [
        (name, "the name"),
        (chip, "the chip"),
        (details, "the details"),
        (actions, "the actions"),
    ] {
        assert_inside(*part, *row, what);
    }
    for (part, what) in [
        (port_text, "the port id"),
        (usb, "the USB ids"),
        (summary, "the settings"),
    ] {
        assert_inside(*part, *details, what);
    }

    // The first line: the name, then the chip, and the name keeps a readable width (it
    // was squeezed to "Airoha..." beside the chip and the settings on one line).
    assert!(
        name.right() <= chip.left() + px(0.5),
        "the name {name:?} runs into the chip {chip:?}"
    );
    assert!(
        name.size.width >= px(100.),
        "the name is squeezed: {name:?}"
    );
    assert!(chip.size.width > px(0.), "the chip has a width: {chip:?}");
    // The second line lies under it, without touching it.
    assert!(
        details.top() >= name.bottom() - px(0.5) && details.top() >= chip.bottom() - px(0.5),
        "the details {details:?} overlap the first line {name:?}"
    );
    // In the details the port id gives way first; the ids and the settings stay whole, in
    // order, apart.
    assert!(
        port_text.size.width >= px(40.),
        "the port id is squeezed: {port_text:?}"
    );
    assert!(
        port_text.right() <= usb.left() && usb.right() <= summary.left(),
        "{port_text:?} {usb:?} {summary:?} are not in order"
    );
    assert!(summary.size.width > px(0.) && usb.size.width > px(0.));
    // The hover actions are on the first line, at the row's right end.
    assert!(
        actions.bottom() <= details.top() + px(0.5),
        "the actions {actions:?} reach the second line {details:?}"
    );
    assert!(actions.right() >= row.right() - px(0.5));

    // Hovering brings the actions up over the chip's place without moving anything.
    cx.update_window(window, |_, window, cx| {
        window.hover(("device-row", ix), cx);
    })
    .unwrap();
    draw(cx, window);
    let hovered = row_parts(cx, window, ix);
    for (before, after, what) in [
        (at_rest.row, hovered.row, "row"),
        (at_rest.name, hovered.name, "name"),
        (at_rest.chip, hovered.chip, "chip"),
        (at_rest.details, hovered.details, "details"),
        (at_rest.port, hovered.port, "port"),
        (at_rest.summary, hovered.summary, "summary"),
        (at_rest.actions, hovered.actions, "actions"),
    ] {
        assert_eq!(before, after, "hovering moves the {what}");
    }

    // Enter connects the selected port, as before; the row then shows its settings
    // when it is not the selected one.
    press(cx, window, "enter");
    wait_connected(cx, &workspace);
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| {
            d.select_port(PortId::new("virtual:echo"), window, cx)
        });
    })
    .unwrap();
    draw(cx, window);
    cx.update_window(window, |_, window, _| {
        assert!(
            window.try_find(("device-summary", ix)).is_some(),
            "a connected row keeps its settings when another is selected"
        );
    })
    .unwrap();
}

#[gpui_test]
fn a_port_selected_before_it_is_listed_scrolls_into_view(cx: &mut TestAppContext) {
    // More ports than the panel's list shows, the selected one last: it is chosen before
    // the list has a row for it, and scrolls into view when its row appears.
    let world = SimWorld::new();
    for name in ["zz-one", "zz-two", "zz-three", "zz-four"] {
        world.add_virtual(name, name, LinkConfig::default(), || {
            Box::new(EchoDevice::new())
        });
    }
    let (window, workspace) = open_selecting(cx, &world, None, &[], Some("virtual:zz-four"), WIDE);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let ports = world.port_source().snapshot().len();
    run_until(cx, "every port listed", |cx| {
        devices.read_with(cx, |d, _| d.list().len() == ports)
    });
    draw(cx, window);
    let last = devices
        .read_with(cx, |d, _| d.list().selected_index())
        .expect("the selected port is listed");
    cx.update_window(window, |_, window, _| {
        let footer = window.find("devices-footer").bounds();
        let panel = window.find("devices-panel").bounds();
        let row = window.find(("device-row", last)).bounds();
        assert!(
            row.bottom() <= footer.top() + px(0.5) && row.top() >= panel.top(),
            "the selected row {row:?} is not above the footer at {footer:?}"
        );
    })
    .unwrap();
}
