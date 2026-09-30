//! The milestone 0 gate, headless: the real engine (`Session` reader and writer threads
//! over the simulator's virtual links) driven through the real workspace, keyboard
//! included. No fake sits anywhere between the compose bar and the scrollback.
//!
//! The engine runs on real threads, so waits are bounded in real time. Each step advances
//! the test clock by one frame, which fires the drain loop's pacing timer; the drain
//! worker then blocks for at most its idle wait and returns the moment data arrives, so
//! nothing here sleeps longer than the engine takes.

use std::time::{Duration, Instant};

use serialist_core::{PortId, SerialConfig};
use serialist_sim::{
    FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator, LinkConfig, SimWorld,
};

use crate::drain::FRAME;
use crate::line_buffer::{DEFAULT_MAX_LINES, LineKind};
use crate::prelude::*;
use crate::session_view::{ConnectionState, SessionView, format_bytes};
use crate::test_support::open_test_window;
use crate::workspace::{AppOptions, Workspace};

/// Generous failure bound; the tests finish far sooner.
const LIMIT: Duration = Duration::from_secs(10);

fn run_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + LIMIT;
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {LIMIT:?} waiting for {what}"
        );
        cx.executor().advance_clock(FRAME);
    }
}

fn open_workspace(
    cx: &mut TestAppContext,
    world: &SimWorld,
    connect_to: Option<&str>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        serial: SerialConfig::default(),
        select_port: connect_to.map(PortId::new),
        connect_on_start: connect_to.is_some(),
    };
    open_test_window(cx, move |window, cx| Workspace::new(options, window, cx))
}

fn session(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |w, _| w.session().cloned())
}

fn wait_connected(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Entity<SessionView> {
    run_until(cx, "the session to connect", |cx| {
        session(cx, workspace)
            .is_some_and(|s| s.read_with(cx, |v, _| v.model().state == ConnectionState::Connected))
    });
    session(cx, workspace).expect("session view")
}

fn has_rx_line(cx: &mut TestAppContext, view: &Entity<SessionView>, text: &str) -> bool {
    view.read_with(cx, |v, _| {
        v.model()
            .buffer
            .rows()
            .any(|r| r.kind == LineKind::Rx && r.text == text)
    })
}

/// Type into whatever has focus and press Enter, as a user would.
fn type_line(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    cx.update_window(window, |_, window, cx| {
        window.input(text, cx);
        window.press("enter", cx);
    })
    .unwrap();
}

#[gpui_test]
fn echo_device_returns_what_the_compose_bar_sends(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, None);
    let echo = PortId::new("virtual:echo");
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());

    run_until(cx, "the simulator's ports to be listed", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&echo).is_some_and(|e| e.present))
    });
    // Select the echo device and press Enter in the (focused) Devices panel.
    devices.update(cx, |d, cx| d.select_port(echo.clone(), cx));
    cx.update_window(window, |_, window, cx| window.press("enter", cx))
        .unwrap();
    let view = wait_connected(cx, &workspace);

    // Connecting focused the compose bar.
    type_line(cx, window, "hello");
    run_until(cx, "the echoed line", |cx| has_rx_line(cx, &view, "hello"));

    view.read_with(cx, |v, _| {
        let rows: Vec<_> = v.model().buffer.rows().map(|r| (r.kind, r.text)).collect();
        let tx = rows.iter().position(|r| *r == (LineKind::Tx, "hello"));
        let rx = rows.iter().position(|r| *r == (LineKind::Rx, "hello"));
        assert!(
            tx < rx,
            "the sent line is echoed before the reply: {rows:?}"
        );
        assert_eq!(v.model().stats.tx_bytes, 7, "hello plus CRLF went out");
    });
    let link = world.link(&echo).expect("an open link");
    assert!(link.control_line(serialist_core::ControlLine::Dtr));
    assert!(link.control_line(serialist_core::ControlLine::Rts));
}

#[gpui_test]
fn at_modem_answers_ok(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    // The `--port` path: preselected and opened at startup.
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:at"));
    let view = wait_connected(cx, &workspace);

    type_line(cx, window, "AT");
    run_until(cx, "OK from the modem", |cx| has_rx_line(cx, &view, "OK"));

    let status = workspace
        .read_with(cx, |w, cx| w.status_line(cx))
        .expect("a status line");
    assert_eq!(status.state, "Connected");
    assert_eq!(status.title, "virtual:at @ 115200 8N1");
    assert_eq!(status.settings, None, "already in the description");
    assert_eq!(status.tx, "TX 4 B");
}

/// Stress test, sized to stay fast: 8 MiB from an unpaced firehose must all arrive, be
/// counted, and leave exactly the newest 10 000 lines in the scrollback.
#[gpui_test]
fn stress_firehose_keeps_the_newest_lines_and_counts_every_byte(cx: &mut TestAppContext) {
    const CAP: u64 = 8 * 1024 * 1024;
    let world = SimWorld::empty();
    world.add_virtual(
        SimWorld::FIREHOSE,
        "Firehose (virtual)",
        LinkConfig::unpaced(),
        || {
            Box::new(FirehoseDevice::new(
                FirehoseConfig::new(FirehoseContent::Text).with_total(CAP),
            ))
        },
    );

    // The same stream, generated here, split into rows independently of LineSplitter:
    // text records end in CRLF and never contain CR otherwise.
    let mut stream = Vec::new();
    FirehoseGenerator::new(FirehoseContent::Text, 0).fill(&mut stream, CAP as usize);
    let text = String::from_utf8(stream).expect("text firehose is ASCII");
    let mut rows: Vec<&str> = text
        .split('\n')
        .map(|row| row.trim_end_matches('\r'))
        .collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }
    let expected = &rows[rows.len() - DEFAULT_MAX_LINES..];
    let last = *expected.last().unwrap();

    let started = Instant::now();
    let (_window, workspace) = open_workspace(cx, &world, Some("virtual:firehose"));
    let view = wait_connected(cx, &workspace);
    run_until(cx, "all 8 MiB to be drained", |cx| {
        view.read_with(cx, |v, _| {
            let model = v.model();
            model.stats.rx_bytes == CAP
                && model.buffer.row(model.buffer.len() - 1).map(|r| r.text) == Some(last)
        })
    });
    let elapsed = started.elapsed();

    view.read_with(cx, |v, _| {
        let buffer = &v.model().buffer;
        assert_eq!(buffer.len(), DEFAULT_MAX_LINES);
        let shown: Vec<&str> = buffer.rows().map(|r| r.text).collect();
        assert!(shown == expected, "the newest 10 000 rows, in order");
        // Every row ever received, plus the "Connected" notice, was either kept or evicted.
        assert_eq!(
            buffer.evicted() as usize,
            rows.len() + 1 - DEFAULT_MAX_LINES
        );
    });
    let status = workspace
        .read_with(cx, |w, cx| w.status_line(cx))
        .expect("a status line");
    assert_eq!(status.rx, format!("RX {}", format_bytes(CAP)));
    assert_eq!(status.rx, "RX 8.0 MiB");
    assert!(
        elapsed < Duration::from_secs(5),
        "8 MiB took {elapsed:?}; the stress test must stay under 5 s"
    );
}
