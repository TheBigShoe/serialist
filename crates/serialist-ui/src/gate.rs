//! The milestone 1 gate, headless: the real engine (`Session` reader and writer threads
//! over the simulator's virtual links, and the ingest thread with its page store) driven
//! through the real workspace, keyboard included. No fake sits anywhere between the
//! compose bar and the terminal element's source. The stepping helpers are in
//! `test_support`.

use std::time::{Duration, Instant};

use serialist_core::{ControlLine, Direction, LineSource, PortId, StoreConfig};
use serialist_sim::{
    FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator, LinkConfig, SimWorld,
};

use crate::prelude::*;
use crate::status::format_bytes;
use crate::test_support::{
    displayed, enable_local_echo, has_rx_line, open_workspace, open_workspace_with, run_until,
    type_line, wait_connected,
};

#[gpui_test]
fn echo_round_trip_appears_in_the_elements_source(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, None);
    let echo = PortId::new("virtual:echo");
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());

    run_until(cx, "the simulator's ports to be listed", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&echo).is_some_and(|e| e.present))
    });
    // Select the echo device and press Enter in the (focused) Devices panel.
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| d.select_port(echo.clone(), window, cx));
        window.press("enter", cx);
    })
    .unwrap();
    let view = wait_connected(cx, &workspace);

    // Local echo is off by default; this test is about the echo's order.
    enable_local_echo(cx, &view);
    // Connecting focused the compose bar.
    type_line(cx, window, "hello");
    run_until(cx, "the echoed line", |cx| has_rx_line(cx, &view, "hello"));

    let lines: Vec<(Direction, String)> = displayed(cx, &view)
        .into_iter()
        .map(|line| (line.direction, line.text))
        .collect();
    assert_eq!(
        lines,
        [
            (
                Direction::Notice,
                "Connected to virtual:echo @ 115200 8N1".into()
            ),
            (Direction::Tx, "hello".into()),
            (Direction::Rx, "hello".into()),
        ],
        "the sent line is echoed before the reply"
    );
    run_until(cx, "hello plus CRLF counted out", |cx| {
        view.read_with(cx, |v, _| v.stats().tx_bytes == 7)
    });
    let link = world.link(&echo).expect("an open link");
    assert!(link.control_line(ControlLine::Dtr));
    assert!(link.control_line(ControlLine::Rts));
}

#[gpui_test]
fn at_modem_answers_ok(cx: &mut TestAppContext) {
    let world = SimWorld::new();
    // The `--port` path: preselected and opened at startup.
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:at"));
    let view = wait_connected(cx, &workspace);

    type_line(cx, window, "AT");
    run_until(cx, "OK from the modem", |cx| has_rx_line(cx, &view, "OK"));
    run_until(cx, "the command counted out", |cx| {
        view.read_with(cx, |v, _| v.stats().tx_bytes == 4)
    });

    let status = workspace
        .read_with(cx, |w, cx| w.status_line(cx))
        .expect("a status line");
    assert_eq!(status.state, "Connected");
    assert_eq!(status.title, "virtual:at @ 115200 8N1");
    assert_eq!(status.settings, None, "already in the description");
    assert_eq!(status.tx, "TX 4 B");
    assert_eq!(status.evicted, None);
}

/// Stress test, sized to stay fast: 8 MiB from an unpaced firehose into a 4 MiB store.
/// Every byte must arrive and be counted, the store must stay within its budget at
/// every snapshot the view takes, and what it keeps must be the newest bytes and lines,
/// in order.
#[gpui_test]
fn stress_firehose_stays_in_budget_and_counts_every_byte(cx: &mut TestAppContext) {
    const CAP: u64 = 8 * 1024 * 1024;
    const BUDGET: usize = 4 * 1024 * 1024;
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

    // The same stream, generated here, split into rows independently of the store:
    // text records end in CRLF and never contain CR otherwise.
    let mut stream = Vec::new();
    FirehoseGenerator::new(FirehoseContent::Text, 0).fill(&mut stream, CAP as usize);
    let text = String::from_utf8(stream.clone()).expect("text firehose is ASCII");
    let mut rows: Vec<&str> = text
        .split('\n')
        .map(|row| row.trim_end_matches('\r'))
        .collect();
    if rows.last() == Some(&"") {
        rows.pop();
    }

    let started = Instant::now();
    let (_window, workspace) = open_workspace_with(
        cx,
        &world,
        Some("virtual:firehose"),
        StoreConfig::with_budget(BUDGET),
    );
    let view = wait_connected(cx, &workspace);
    let mut snapshots = 0;
    run_until(cx, "all 8 MiB to be ingested", |cx| {
        view.read_with(cx, |v, _| {
            let stats = v.snapshot().stats();
            assert!(
                stats.memory <= stats.budget,
                "{} B over a {} B budget",
                stats.memory,
                stats.budget
            );
            snapshots += 1;
            v.stats().rx_bytes == CAP && stats.raw_len == CAP
        })
    });
    let elapsed = started.elapsed();

    let snapshot = view.read_with(cx, |v, _| v.snapshot().clone());
    let stats = snapshot.stats();
    assert_eq!(stats.budget, BUDGET);
    assert!(stats.raw_start > 0, "the budget evicted the oldest pages");
    let kept: Vec<u8> = snapshot
        .raw(0..CAP)
        .flat_map(|slice| slice.iter().copied())
        .collect();
    assert!(
        kept == stream[stats.raw_start as usize..],
        "the newest bytes, in order"
    );
    let mut lines = Vec::new();
    snapshot.lines(snapshot.first_line()..snapshot.end(), &mut lines);
    let shown: Vec<&str> = lines
        .iter()
        .filter(|line| line.direction == Direction::Rx)
        .map(|line| line.text.as_str())
        .collect();
    assert!(!shown.is_empty());
    assert!(
        shown == rows[rows.len() - shown.len()..],
        "the newest lines, in order"
    );
    // Every row received, plus the connect notice, is either retained or evicted.
    assert_eq!(
        stats.evicted_lines as usize + lines.len(),
        rows.len() + 1,
        "{stats:?}"
    );

    let status = workspace
        .read_with(cx, |w, cx| w.status_line(cx))
        .expect("a status line");
    assert_eq!(status.rx, format!("RX {}", format_bytes(CAP)));
    assert_eq!(status.rx, "RX 8.0 MiB");
    assert_eq!(
        status.retained,
        format!(
            "{} lines, {} kept",
            lines.len(),
            format_bytes(stats.retained_bytes())
        )
    );
    assert_eq!(
        status.evicted,
        Some(format!(
            "evicted {} lines, {}",
            stats.evicted_lines,
            format_bytes(stats.raw_start)
        ))
    );
    // The ingest thread counts a chunk after it publishes it, so a snapshot holding every
    // byte can be read a moment before the counter has them.
    run_until(cx, "ingest to have counted every byte", |cx| {
        view.read_with(cx, |v, _| v.ingest_stats().unwrap().bytes == CAP)
    });
    let ingest = view.read_with(cx, |v, _| v.ingest_stats().unwrap());
    assert_eq!(ingest.bytes, CAP, "ingest took in every byte");
    assert!(snapshots > 0);
    assert!(
        elapsed < Duration::from_secs(10),
        "8 MiB took {elapsed:?}; the stress test must stay under 10 s"
    );
}
