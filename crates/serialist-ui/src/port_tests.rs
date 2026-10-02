//! The port settings popover, the toolbar's Disconnect and Connect, and connecting
//! again in the same tab, against the real engine over `SimWorld` links.

use std::sync::Arc;

use parking_lot::Mutex;
use serialist_core::{ControlLine, LineEnding, Parity, PortId, SerialConfig};
use serialist_sim::{AtDevice, DeviceOutput, LinkConfig, SimDevice, SimWorld};

use crate::port_settings::PortSettingsForm;
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::tabs::TabState;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, has_rx_line, open_test_window, run_until, type_line,
};
use crate::workspace::{AppOptions, Workspace};

/// An AT modem that also records every control-line change the host makes.
struct Probe {
    modem: AtDevice,
    controls: Arc<Mutex<Vec<(ControlLine, bool)>>>,
}

impl SimDevice for Probe {
    fn name(&self) -> &str {
        "probe"
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        self.modem.on_receive(bytes, out);
    }

    fn on_control(&mut self, line: ControlLine, asserted: bool, _: &mut dyn DeviceOutput) {
        self.controls.lock().push((line, asserted));
    }
}

const PROBE: &str = "virtual:probe";

type Controls = Arc<Mutex<Vec<(ControlLine, bool)>>>;

/// The simulator's built-ins plus `virtual:probe`, whose control-line changes land in
/// the returned list.
fn world() -> (SimWorld, Controls) {
    let world = SimWorld::new();
    let controls: Controls = Arc::default();
    let seen = controls.clone();
    world.add_virtual("probe", "Probe modem", LinkConfig::unpaced(), move || {
        Box::new(Probe {
            modem: AtDevice::new(),
            controls: seen.clone(),
        })
    });
    (world, controls)
}

fn open(
    cx: &mut TestAppContext,
    world: &SimWorld,
    ports: &[&str],
) -> (AnyWindowHandle, Entity<Workspace>) {
    allow_engine_threads(cx);
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: None,
        open_ports: ports.iter().map(|port| PortId::new(*port)).collect(),
        store: None,
        replay: None,
    };
    open_test_window(cx, move |window, cx| Workspace::new(options, window, cx))
}

fn wait_view(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    port: &str,
) -> Entity<SessionView> {
    let id = PortId::new(port);
    run_until(cx, "the port to open", |cx| {
        workspace
            .read_with(cx, |w, _| w.session_for_port(&id).cloned())
            .is_some_and(|view| view.read_with(cx, |v, _| v.connection().description.is_some()))
    });
    workspace.read_with(cx, |w, _| w.session_for_port(&id).cloned().unwrap())
}

fn form_of(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Entity<PortSettingsForm> {
    view.read_with(cx, |v, _| v.port_form().clone())
}

fn status(cx: &mut TestAppContext, form: &Entity<PortSettingsForm>) -> Option<String> {
    form.read_with(cx, |f, _| f.status().map(str::to_owned))
}

fn error(cx: &mut TestAppContext, form: &Entity<PortSettingsForm>) -> Option<String> {
    form.read_with(cx, |f, _| f.error().map(str::to_owned))
}

fn tab_state(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> TabState {
    workspace.read_with(cx, |w, cx| w.tab_labels(cx)[0].state)
}

#[gpui_test]
fn the_popover_reconfigures_the_open_port_live(cx: &mut TestAppContext) {
    let (world, _) = world();
    let (window, workspace) = open(cx, &world, &["virtual:at"]);
    let view = wait_view(cx, &workspace, "virtual:at");
    let form = form_of(cx, &view);
    let link = world.link(&PortId::new("virtual:at")).unwrap();
    assert_eq!(link.link_config().serial.baud, 115_200);
    assert!(form.read_with(cx, |f, _| f.is_live()));

    // A rate typed in the field, then even parity from its list: each goes to the port.
    cx.update_window(window, |_, window, cx| {
        form.update(cx, |f, cx| f.enter_baud("57600", window, cx));
    })
    .unwrap();
    run_until(cx, "the new rate to apply", |cx| {
        status(cx, &form).as_deref() == Some("57600 8N1 applied")
    });
    assert_eq!(
        link.link_config().serial.baud,
        57_600,
        "the link runs at the new rate"
    );
    assert_eq!(view.read_with(cx, |v, _| v.serial().baud), 57_600);
    let line = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(
        line.title, "virtual:at @ 57600 8N1",
        "the status line's summary follows"
    );
    assert_eq!(line.settings, None);

    cx.update_window(window, |_, window, cx| {
        form.update(cx, |f, cx| {
            let serial = SerialConfig {
                parity: Parity::Even,
                ..f.settings().serial.clone()
            };
            f.change_serial(serial, window, cx);
        });
    })
    .unwrap();
    run_until(cx, "even parity to apply", |cx| {
        status(cx, &form).as_deref() == Some("57600 8E1 applied")
    });
    assert_eq!(link.link_config().serial.parity, Parity::Even);
    assert_eq!(
        workspace
            .read_with(cx, |w, cx| w.status_line(cx))
            .unwrap()
            .title,
        "virtual:at @ 57600 8E1"
    );

    // The port still talks at the new settings.
    type_line(cx, window, "AT");
    run_until(cx, "OK", |cx| has_rx_line(cx, &view, "OK"));

    // A change the transport refuses shows in the form and puts the controls back.
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |v, cx| {
            v.reconfigure(
                SerialConfig {
                    baud: 0,
                    ..SerialConfig::default()
                },
                window,
                cx,
            );
        });
    })
    .unwrap();
    run_until(cx, "the refusal", |cx| error(cx, &form).is_some());
    let refusal = error(cx, &form).unwrap();
    assert!(
        refusal.contains("baud must be greater than zero"),
        "{refusal}"
    );
    form.read_with(cx, |f, cx| {
        assert_eq!(f.settings().serial.baud, 57_600, "back to what is in force");
        assert_eq!(f.baud_text(cx), "57600");
    });
    assert_eq!(view.read_with(cx, |v, _| v.serial().baud), 57_600);
    assert_eq!(link.link_config().serial.baud, 57_600);

    // The simulator has no break: the form says so.
    form.update(cx, |f, cx| f.send_break(cx));
    assert_eq!(error(cx, &form), None, "a new try clears the last error");
    run_until(cx, "the break's refusal", |cx| error(cx, &form).is_some());
    assert!(error(cx, &form).unwrap().contains("break"));

    // Line ending and echo go to the compose bar.
    form.update(cx, |f, cx| {
        f.change_line_ending(LineEnding::Lf, cx);
        f.change_local_echo(true, cx);
    });
    cx.run_until_parked();
    view.read_with(cx, |v, cx| {
        assert_eq!(v.line_ending(cx), LineEnding::Lf);
        assert!(v.compose().read(cx).local_echo());
    });

    // Changed elsewhere (the compose bar's own picker), the form catches up when the
    // toolbar button opens it.
    let compose = view.read_with(cx, |v, _| v.compose().clone());
    compose.update(cx, |c, cx| c.set_line_ending(LineEnding::Cr, cx));
    cx.update_window(window, |_, window, cx| window.click("port-settings", cx))
        .unwrap();
    cx.run_until_parked();
    form.read_with(cx, |f, _| {
        assert_eq!(f.settings().line_ending, LineEnding::Cr);
        assert_eq!(f.settings().serial.baud, 57_600);
    });
}

#[gpui_test]
fn dtr_and_rts_toggle_on_the_wire(cx: &mut TestAppContext) {
    let (world, controls) = world();
    let (_window, workspace) = open(cx, &world, &[PROBE]);
    let view = wait_view(cx, &workspace, PROBE);
    let form = form_of(cx, &view);
    let link = world.link(&PortId::new(PROBE)).unwrap();
    run_until(cx, "the opener's DTR and RTS", |_| {
        controls.lock().len() >= 2
    });
    assert!(link.control_line(ControlLine::Dtr));

    form.update(cx, |f, cx| f.change_control(ControlLine::Dtr, false, cx));
    run_until(cx, "DTR to drop", |_| {
        controls.lock().last() == Some(&(ControlLine::Dtr, false))
    });
    assert!(!link.control_line(ControlLine::Dtr));
    run_until(cx, "the form to say so", |cx| {
        status(cx, &form).as_deref() == Some("DTR released")
    });
    assert_eq!(view.read_with(cx, |v, _| v.control_levels()), (false, true));

    form.update(cx, |f, cx| f.change_control(ControlLine::Rts, false, cx));
    run_until(cx, "RTS to drop", |_| {
        controls.lock().last() == Some(&(ControlLine::Rts, false))
    });
    form.update(cx, |f, cx| f.change_control(ControlLine::Dtr, true, cx));
    run_until(cx, "DTR back up", |_| {
        controls.lock().last() == Some(&(ControlLine::Dtr, true))
    });
    assert!(link.control_line(ControlLine::Dtr));
    assert!(!link.control_line(ControlLine::Rts));
}

#[gpui_test]
fn disconnect_then_connect_keeps_the_scrollback_and_the_settings(cx: &mut TestAppContext) {
    let (world, controls) = world();
    let (window, workspace) = open(cx, &world, &[PROBE]);
    let view = wait_view(cx, &workspace, PROBE);
    let form = form_of(cx, &view);
    type_line(cx, window, "AT");
    run_until(cx, "OK", |cx| has_rx_line(cx, &view, "OK"));
    cx.update_window(window, |_, window, cx| {
        form.update(cx, |f, cx| f.enter_baud("9600", window, cx));
    })
    .unwrap();
    run_until(cx, "9600 to apply", |cx| {
        status(cx, &form).as_deref() == Some("9600 8N1 applied")
    });
    form.update(cx, |f, cx| f.change_control(ControlLine::Rts, false, cx));
    run_until(cx, "RTS to drop", |_| {
        controls.lock().last() == Some(&(ControlLine::Rts, false))
    });

    // Nothing records or runs a script: the toolbar's Disconnect is immediate.
    cx.update_window(window, |_, window, cx| {
        window.click("session-disconnect", cx)
    })
    .unwrap();
    run_until(cx, "the tab to read disconnected", |cx| {
        tab_state(cx, &workspace) == TabState::Disconnected
    });
    assert!(!view.read_with(cx, |v, _| v.pending_disconnect()));
    assert!(!form.read_with(cx, |f, _| f.is_live()));
    run_until(cx, "the disconnect notice", |cx| {
        displayed(cx, &view)
            .iter()
            .any(|line| line.text == "Disconnected")
    });
    let before = displayed(cx, &view);

    // Connect, in its place, opens the port again into the same view.
    controls.lock().clear();
    cx.update_window(window, |_, window, cx| window.click("session-connect", cx))
        .unwrap();
    run_until(cx, "the session to come back", |cx| {
        tab_state(cx, &workspace) == TabState::Connected
            && displayed(cx, &view)
                .iter()
                .filter(|line| line.text.starts_with("Connected to "))
                .count()
                == 2
    });
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.tab_count(), 1);
        assert_eq!(w.session().unwrap(), &view, "the same view");
    });
    let after = displayed(cx, &view);
    assert_eq!(after[..before.len()], before[..], "the scrollback is kept");
    assert_eq!(
        after.last().unwrap().text,
        "Connected to virtual:probe @ 9600 8N1",
        "opened at the rate set before"
    );
    let link = world.link(&PortId::new(PROBE)).unwrap();
    assert_eq!(link.link_config().serial.baud, 9600);
    run_until(cx, "RTS released again", |_| {
        controls.lock().contains(&(ControlLine::Rts, false))
    });
    assert!(!link.control_line(ControlLine::Rts));
    assert!(form.read_with(cx, |f, _| f.is_live()));

    // And it talks.
    type_line(cx, window, "ATI");
    run_until(cx, "the modem's name", |cx| {
        has_rx_line(cx, &view, "Serialist Virtual Modem")
    });
    assert!(has_rx_line(cx, &view, "OK"));
}

#[gpui_test]
fn disconnect_asks_first_only_while_recording(cx: &mut TestAppContext) {
    let dir = TestDir::new("port-disconnect");
    let (world, _) = world();
    let (window, workspace) = open(cx, &world, &["virtual:at"]);
    let view = wait_view(cx, &workspace, "virtual:at");
    let recording = dir.join("at.bin");
    view.update(cx, |v, cx| v.start_recording(recording.clone(), cx));
    run_until(cx, "the recording to open", |cx| {
        view.read_with(cx, |v, _| v.recording().is_some_and(|r| r.stats.is_some()))
    });

    cx.update_window(window, |_, window, cx| {
        window.click("session-disconnect", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert!(view.read_with(cx, |v, _| v.pending_disconnect()), "it asks");
    assert_eq!(tab_state(cx, &workspace), TabState::Recording, "still open");
    cx.update_window(window, |_, window, cx| {
        assert!(window.has_active_dialog(cx));
        view.update(cx, |v, cx| v.confirm_disconnect(window, cx));
        assert!(!window.has_active_dialog(cx));
    })
    .unwrap();
    run_until(cx, "the tab to read disconnected", |cx| {
        tab_state(cx, &workspace) == TabState::Disconnected
    });
    assert!(view.read_with(cx, |v, _| v.recording().is_none()));
    run_until(cx, "the recording file", |_| recording.is_file());
}

#[gpui_test]
fn a_devices_row_sets_what_the_next_connect_uses(cx: &mut TestAppContext) {
    let (world, controls) = world();
    let (window, workspace) = open(cx, &world, &[]);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let probe = PortId::new(PROBE);
    run_until(cx, "the probe to be listed", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&probe).is_some())
    });
    let form = devices.read_with(cx, |d, _| d.port_form().clone());
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| {
            d.select_port(probe.clone(), window, cx);
            d.edit_port_settings(probe.clone(), window, cx);
        });
        assert!(
            !form.read(cx).is_live(),
            "a row's settings are for the next connect"
        );
        form.update(cx, |f, cx| {
            f.change_serial(
                SerialConfig {
                    baud: 38_400,
                    parity: Parity::Odd,
                    ..SerialConfig::default()
                },
                window,
                cx,
            );
            f.change_control(ControlLine::Dtr, false, cx);
            f.change_line_ending(LineEnding::Cr, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        devices.read_with(cx, |d, cx| d.baud_text(cx)),
        "38400",
        "the baud field follows"
    );

    devices.update(cx, |d, cx| assert!(d.connect_selected(cx)));
    let view = wait_view(cx, &workspace, PROBE);
    view.read_with(cx, |v, cx| {
        assert_eq!(v.serial().baud, 38_400);
        assert_eq!(v.serial().parity, Parity::Odd);
        assert_eq!(v.line_ending(cx), LineEnding::Cr);
        assert_eq!(v.control_levels(), (false, true));
    });
    let link = world.link(&probe).unwrap();
    assert_eq!(link.link_config().serial.baud, 38_400);
    run_until(cx, "DTR released after the open", |_| {
        controls.lock().last() == Some(&(ControlLine::Dtr, false))
    });
    assert!(!link.control_line(ControlLine::Dtr));
}
