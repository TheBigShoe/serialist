//! Pause, export, record, hex view and search against the real engine: `Session` and
//! ingest threads over `SimWorld` links, driven through the workspace as in `gate.rs`.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use serialist_core::{LineSource, PortId, StoreConfig};
use serialist_sim::{
    DeviceOutput, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator,
    FirehoseVerifier, LinkConfig, SimDevice, SimWorld,
};

use crate::export::ExportFormat;
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::status::{Notice, format_bytes};
use crate::terminal::{DisplayMode, TerminalView, TimestampMode};
use crate::test_support::{
    TestDir, displayed, enable_local_echo, has_rx_line, open_workspace, open_workspace_with,
    run_until, step, type_line, wait_connected, wait_for_received,
};

/// Fast enough to fill a few screens in a fraction of a second, slow enough that every
/// step while paused brings something new.
const FAST: u64 = 4 * 1024 * 1024;

/// A firehose on an unpaced link, sending text at `rate` bytes per second.
fn firehose_world(rate: u64) -> SimWorld {
    let world = SimWorld::empty();
    world.add_virtual(
        SimWorld::FIREHOSE,
        "Firehose (virtual)",
        LinkConfig::unpaced(),
        move || {
            Box::new(FirehoseDevice::new(
                FirehoseConfig::new(FirehoseContent::Text).with_rate(rate),
            ))
        },
    );
    world
}

/// A firehose that stays silent until the host sends its first byte, so a test can get
/// ready (start a recording, open a search) before the stream begins.
struct GatedFirehose {
    inner: FirehoseDevice,
    open: bool,
}

impl SimDevice for GatedFirehose {
    fn name(&self) -> &str {
        "gated-firehose"
    }

    fn on_receive(&mut self, _: &[u8], _: &mut dyn DeviceOutput) {
        self.open = true;
    }

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        if self.open {
            self.inner.on_tick(now, out)
        } else {
            None
        }
    }
}

fn gated_world(config: FirehoseConfig) -> SimWorld {
    let world = SimWorld::empty();
    world.add_virtual(
        "gated",
        "Gated firehose",
        LinkConfig::unpaced(),
        move || {
            Box::new(GatedFirehose {
                inner: FirehoseDevice::new(config.clone()),
                open: false,
            })
        },
    );
    world
}

/// The first `len` bytes every text firehose sends.
fn text_stream(len: usize) -> Vec<u8> {
    let mut stream = Vec::new();
    FirehoseGenerator::new(FirehoseContent::Text, 0).fill(&mut stream, len);
    stream
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

/// The text of every line the terminal displays, read with the view's own snapshot.
fn shown(view: &SessionView, cx: &App) -> Vec<String> {
    let terminal = view.terminal().read(cx);
    let mut lines = Vec::new();
    terminal
        .source()
        .lines(terminal.displayed_span().range(), &mut lines);
    lines.into_iter().map(|line| line.text).collect()
}

fn terminal_of(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Entity<TerminalView> {
    view.read_with(cx, |v, _| v.terminal().clone())
}

fn received(cx: &mut TestAppContext, view: &Entity<SessionView>) -> u64 {
    view.read_with(cx, |v, _| v.snapshot().raw_range().end)
}

fn wait_lines(cx: &mut TestAppContext, view: &Entity<SessionView>, lines: u64) {
    run_until(cx, "enough lines", |cx| {
        view.read_with(cx, |v, _| v.snapshot().end().0 >= lines)
    });
}

fn wait_notice(cx: &mut TestAppContext, view: &Entity<SessionView>, needle: &str) -> Notice {
    run_until(cx, needle, |cx| {
        view.read_with(cx, |v, _| {
            v.notice().is_some_and(|n| n.text.contains(needle))
        })
    });
    view.read_with(cx, |v, _| v.notice().cloned().unwrap())
}

fn wait_recording(cx: &mut TestAppContext, view: &Entity<SessionView>) {
    run_until(cx, "the recording file to open", |cx| {
        view.read_with(cx, |v, _| v.recording().is_some_and(|r| r.stats.is_some()))
    });
}

fn as_text_file(rows: &[String]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

/// Export in `format` to `path`, and return what the terminal displayed at that moment.
fn export_now(
    cx: &mut TestAppContext,
    view: &Entity<SessionView>,
    path: PathBuf,
    format: ExportFormat,
) -> Vec<String> {
    let (lines, task) = view.update(cx, |v, cx| (shown(v, cx), v.export_to(path, format, cx)));
    task.detach();
    lines
}

fn focus_terminal(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<SessionView>) {
    let terminal = terminal_of(cx, view);
    cx.update_window(window, |_, window, cx| {
        let handle = terminal.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    })
    .unwrap();
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
    cx.run_until_parked();
}

#[gpui_test]
fn pause_holds_the_rendered_range_while_the_stats_grow(cx: &mut TestAppContext) {
    let world = firehose_world(FAST);
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:firehose"));
    let view = wait_connected(cx, &workspace);
    wait_lines(cx, &view, 2000);
    let terminal = terminal_of(cx, &view);
    let rendered = |cx: &mut TestAppContext| {
        draw(cx, window);
        terminal.read_with(cx, |t, _| {
            (t.displayed_span(), t.scroll_handle().position())
        })
    };

    view.update(cx, |v, cx| v.pause(cx));
    let (span, top) = rendered(cx);
    let mark = view.read_with(cx, |v, _| v.pause_mark().unwrap());
    assert_eq!(span.end.0, mark.lines, "frozen at the snapshot's end");
    let rx_at_pause = view.read_with(cx, |v, _| v.stats().rx_bytes);

    let mut snapshots = 0;
    let mut seen = 0;
    run_until(cx, "three snapshots while paused", |cx| {
        assert_eq!(rendered(cx), (span, top), "the paused range moved");
        let bytes = view.read_with(cx, |v, _| mark.since(&v.snapshot().stats()).1);
        if bytes > seen {
            seen = bytes;
            snapshots += 1;
        }
        snapshots >= 3
    });

    view.read_with(cx, |v, _| {
        let (lines, bytes) = mark.since(&v.snapshot().stats());
        assert!(lines > 0 && bytes > 0);
        assert!(v.stats().rx_bytes > rx_at_pause, "RX keeps counting");
        assert_eq!(
            v.status_line().paused,
            Some(format!("Paused, +{lines} lines, +{}", format_bytes(bytes)))
        );
    });
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert!(status.paused.unwrap().starts_with("Paused, +"));

    view.update(cx, |v, cx| v.resume(cx));
    step(cx, 2);
    let (live, live_top) = rendered(cx);
    view.read_with(cx, |v, cx| {
        assert!(!v.is_paused());
        assert!(v.is_following_tail(cx));
        assert_eq!(v.status_line().paused, None);
        assert_eq!(live.end, v.snapshot().end(), "resume follows the tail");
    });
    assert!(live.end > span.end);
    assert!(live_top.line > top.line, "with the newest lines on screen");
}

#[gpui_test]
fn exports_while_paused_write_the_frozen_lines_and_bytes(cx: &mut TestAppContext) {
    let dir = TestDir::new("export-paused");
    let world = firehose_world(FAST);
    let (_window, workspace) = open_workspace(cx, &world, Some("virtual:firehose"));
    let view = wait_connected(cx, &workspace);
    wait_lines(cx, &view, 2000);

    view.update(cx, |v, cx| v.pause(cx));
    let mark = view.read_with(cx, |v, _| v.pause_mark().unwrap());
    let before = received(cx, &view);
    run_until(cx, "more data behind the pause", |cx| {
        received(cx, &view) > before
    });
    let frozen = export_now(cx, &view, dir.join("paused.txt"), ExportFormat::Text);
    let notice = wait_notice(cx, &view, "paused.txt");
    assert_eq!(
        notice,
        Notice::info(format!("Exported {} lines to paused.txt", frozen.len()))
    );
    assert_eq!(
        frozen.len() as u64,
        mark.lines,
        "every line up to the pause"
    );
    assert_eq!(
        fs::read_to_string(dir.join("paused.txt")).unwrap(),
        as_text_file(&frozen),
        "exactly the frozen lines"
    );
    assert!(frozen[0].starts_with("Connected to virtual:firehose"));

    export_now(cx, &view, dir.join("paused.bin"), ExportFormat::Raw);
    wait_notice(cx, &view, "paused.bin");
    let raw = fs::read(dir.join("paused.bin")).unwrap();
    assert_eq!(raw.len() as u64, mark.bytes, "the bytes up to the pause");
    assert!(raw == text_stream(raw.len()));

    view.update(cx, |v, cx| v.resume(cx));
    step(cx, 3);
    let live = export_now(cx, &view, dir.join("live.txt"), ExportFormat::Text);
    wait_notice(cx, &view, "live.txt");
    assert!(live.len() > frozen.len());
    assert_eq!(
        fs::read_to_string(dir.join("live.txt")).unwrap(),
        as_text_file(&live),
        "exactly the live lines at the moment of export"
    );
}

#[gpui_test]
fn text_export_stamps_lines_as_the_gutter_does(cx: &mut TestAppContext) {
    let dir = TestDir::new("export-stamped");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "hello");
    run_until(cx, "the echo", |cx| has_rx_line(cx, &view, "hello"));

    let terminal = terminal_of(cx, &view);
    terminal.update(cx, |t, cx| t.set_timestamps(TimestampMode::Relative, cx));
    let lines = export_now(cx, &view, dir.join("stamped.log"), ExportFormat::Text);
    wait_notice(cx, &view, "stamped.log");
    let text = fs::read_to_string(dir.join("stamped.log")).unwrap();
    let written: Vec<&str> = text.lines().collect();
    assert_eq!(written.len(), lines.len());
    let stamp = regex::Regex::new(r"^\[\+\d+\.\d{6}\] (.*)$").unwrap();
    for (written, shown) in written.iter().zip(&lines) {
        let captures = stamp.captures(written).expect("a relative stamp");
        assert_eq!(&captures[1], shown);
    }
}

#[gpui_test]
fn raw_export_is_exactly_what_the_device_sent(cx: &mut TestAppContext) {
    let dir = TestDir::new("export-raw");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "hello");
    type_line(cx, window, "world");
    // The link is paced, so the echo arrives in pieces: a line reading "world" may still
    // lack its line ending. Wait for every byte.
    wait_for_received(cx, &view, 14);

    export_now(cx, &view, dir.join("echo.bin"), ExportFormat::Raw);
    let notice = wait_notice(cx, &view, "echo.bin");
    assert_eq!(notice, Notice::info("Exported 14 B raw to echo.bin"));

    let exported = fs::read(dir.join("echo.bin")).unwrap();
    assert_eq!(exported, b"hello\r\nworld\r\n");
    let link = world.link(&PortId::new("virtual:echo")).expect("open link");
    assert_eq!(exported.len() as u64, link.stats().device_to_host_bytes);
}

#[gpui_test]
fn raw_export_of_a_firehose_verifies_clean(cx: &mut TestAppContext) {
    const TOTAL: u64 = 1024 * 1024;
    let dir = TestDir::new("export-firehose");
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_total(TOTAL));
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:gated"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "go");
    run_until(cx, "the whole stream", |cx| received(cx, &view) == TOTAL);

    export_now(cx, &view, dir.join("firehose.bin"), ExportFormat::Raw);
    let notice = wait_notice(cx, &view, "firehose.bin");
    assert_eq!(notice, Notice::info("Exported 1.0 MiB raw to firehose.bin"));
    let exported = fs::read(dir.join("firehose.bin")).unwrap();
    assert!(exported == text_stream(TOTAL as usize), "byte for byte");
    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    verifier.feed(&exported);
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    assert!(report.records > 1000, "{report:?}");
}

#[gpui_test]
fn raw_export_counts_what_the_budget_evicted(cx: &mut TestAppContext) {
    const TOTAL: u64 = 4 * 1024 * 1024;
    let dir = TestDir::new("export-evicted");
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_total(TOTAL));
    // The smallest store there is, about 1.5 MiB.
    let (window, workspace) = open_workspace_with(
        cx,
        &world,
        Some("virtual:gated"),
        StoreConfig::with_budget(0),
    );
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "go");
    run_until(cx, "the whole stream", |cx| received(cx, &view) == TOTAL);

    let kept = view.read_with(cx, |v, _| v.snapshot().raw_range());
    assert!(kept.start > 0, "the budget evicted the oldest bytes");
    export_now(cx, &view, dir.join("tail.bin"), ExportFormat::Raw);
    let notice = wait_notice(cx, &view, "tail.bin");
    assert_eq!(
        notice,
        Notice::info(format!(
            "Exported {} raw to tail.bin; {} older were already dropped",
            format_bytes(kept.end - kept.start),
            format_bytes(kept.start)
        ))
    );
    let stream = text_stream(TOTAL as usize);
    assert!(
        fs::read(dir.join("tail.bin")).unwrap() == stream[kept.start as usize..],
        "the newest bytes, in order"
    );
}

#[gpui_test]
fn recording_writes_the_raw_stream_until_stopped(cx: &mut TestAppContext) {
    let dir = TestDir::new("record");
    // Slow enough that the recorder's 256 KiB buffer never fills, so bytes on disk
    // before the stop can only come from the timed flush on the ingest thread.
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_rate(64 * 1024));
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:gated"));
    let view = wait_connected(cx, &workspace);

    let path = dir.join("capture.bin");
    view.update(cx, |v, cx| v.start_recording(path.clone(), cx));
    wait_recording(cx, &view);

    // The device starts streaming on the first byte it hears.
    type_line(cx, window, "go");
    run_until(cx, "a timed flush while recording", |_| {
        fs::metadata(&path).map_or(0, |m| m.len()) >= 4096
    });
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert!(
        status
            .recording
            .as_deref()
            .is_some_and(|rec| rec.starts_with("REC capture.bin ")),
        "{status:?}"
    );

    view.update(cx, |v, cx| v.stop_recording(cx));
    let notice = wait_notice(cx, &view, "Recorded");
    let recorded = fs::read(&path).unwrap();
    assert_eq!(
        notice,
        Notice::info(format!(
            "Recorded {} to capture.bin",
            format_bytes(recorded.len() as u64)
        ))
    );
    let after_stop = received(cx, &view);
    run_until(cx, "more data after the stop", |cx| {
        received(cx, &view) > after_stop
    });
    // The view's snapshot follows the ingest thread by a wake, so it may not hold yet
    // everything the recorder took before the stop.
    wait_for_received(cx, &view, recorded.len() as u64);
    assert_eq!(
        fs::read(&path).unwrap().len(),
        recorded.len(),
        "nothing is written after the stop"
    );

    assert!(
        recorded == text_stream(recorded.len()),
        "the file is the stream from its first byte"
    );
    let snapshot = view.read_with(cx, |v, _| v.snapshot().clone());
    let received: Vec<u8> = snapshot
        .raw(0..recorded.len() as u64)
        .flat_map(|slice| slice.iter().copied())
        .collect();
    assert!(received == recorded, "and what the session received");

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    verifier.feed(&recorded);
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.missing_records, 0);
    assert!(report.records > 10, "{report:?}");
}

#[gpui_test]
fn pause_export_and_record_have_default_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new("keys");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "hello");
    wait_for_received(cx, &view, 7);

    let (pause, export, record) = if cfg!(target_os = "macos") {
        ("cmd-p", "cmd-s", "cmd-shift-r")
    } else {
        ("ctrl-p", "ctrl-shift-s", "ctrl-shift-r")
    };
    let paused = |cx: &mut TestAppContext| view.read_with(cx, |v, _| v.is_paused());

    press(cx, window, pause);
    assert!(paused(cx));
    press(cx, window, pause);
    assert!(!paused(cx));

    press(cx, window, export);
    assert!(cx.did_prompt_for_new_path(), "export asks where to save");
    let text_path = dir.join("keys.txt");
    let chosen = text_path.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    wait_notice(cx, &view, "keys.txt");
    let rows = view.read_with(cx, shown);
    assert_eq!(fs::read_to_string(&text_path).unwrap(), as_text_file(&rows));

    press(cx, window, record);
    assert!(cx.did_prompt_for_new_path(), "record asks where to save");
    let raw_path = dir.join("keys.bin");
    let chosen = raw_path.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    wait_recording(cx, &view);
    type_line(cx, window, "again");
    wait_for_received(cx, &view, 14);
    press(cx, window, record);
    wait_notice(cx, &view, "Recorded");
    assert_eq!(fs::read(&raw_path).unwrap(), b"again\r\n");
    assert!(view.read_with(cx, |v, _| v.recording().is_none()));
}

/// Start recording an echo session, get one line echoed, then end the session with `end`.
/// The line is far smaller than the recorder's buffer, so it only reaches the file
/// through a flush on disconnect or the final flush.
fn recording_survives(
    cx: &mut TestAppContext,
    name: &str,
    end: impl FnOnce(&mut TestAppContext, &SimWorld, &Entity<SessionView>),
) {
    let dir = TestDir::new(name);
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    let path = dir.join("session.bin");
    view.update(cx, |v, cx| v.start_recording(path.clone(), cx));
    wait_recording(cx, &view);
    type_line(cx, window, "bye");
    wait_for_received(cx, &view, 5);

    end(cx, &world, &view);
    let notice = wait_notice(cx, &view, "Recorded");
    assert_eq!(notice, Notice::info("Recorded 5 B to session.bin"));
    assert_eq!(fs::read(&path).unwrap(), b"bye\r\n");
    assert!(view.read_with(cx, |v, _| v.recording().is_none()));
}

#[gpui_test]
fn disconnecting_stops_the_recording_with_a_final_flush(cx: &mut TestAppContext) {
    recording_survives(cx, "record-disconnect", |cx, _, view| {
        view.update(cx, |v, cx| v.disconnect(cx));
    });
}

#[gpui_test]
fn losing_the_device_stops_the_recording_with_a_final_flush(cx: &mut TestAppContext) {
    recording_survives(cx, "record-unplug", |cx, world, view| {
        world.unplug(&PortId::new("virtual:echo"));
        run_until(cx, "the lost session", |cx| {
            view.read_with(cx, |v, _| v.state().is_disconnected())
        });
        let state = view.read_with(cx, |v, _| v.state().clone());
        assert!(
            matches!(
                &state,
                crate::status::ConnectionState::Disconnected { error: Some(_) }
            ),
            "{state:?}"
        );
    });
}

#[gpui_test]
fn hex_view_shows_the_same_snapshot_and_keeps_pause_and_selection(cx: &mut TestAppContext) {
    let dir = TestDir::new("hex");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    // The text view below shows each sent line and its echo.
    enable_local_echo(cx, &view);
    // Sixteen bytes with the line ending: exactly one full hex row.
    type_line(cx, window, "hello world 12");
    // All sixteen bytes: the hex row below needs the line ending too, which a paced link
    // may deliver after the text.
    wait_for_received(cx, &view, 16);

    focus_terminal(cx, window, &view);
    press(cx, window, "alt-h");
    let terminal = terminal_of(cx, &view);
    assert_eq!(
        terminal.read_with(cx, |t, _| t.display_mode()),
        DisplayMode::Hex
    );
    let rows = displayed(cx, &view);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].text,
        "00000000  68 65 6c 6c 6f 20 77 6f  72 6c 64 20 31 32 0d 0a  |hello world 12..|"
    );
    let same = view.read_with(cx, |v, _| v.snapshot().hex_view(16).line(rows[0].id));
    assert_eq!(same.map(|row| row.text), Some(rows[0].text.clone()));

    // Paused in hex view, a new row arrives behind the frozen end.
    view.update(cx, |v, cx| v.pause(cx));
    view.update(cx, |v, cx| v.send("again", b"again\r\n".to_vec(), cx));
    run_until(cx, "the second echo", |cx| received(cx, &view) == 23);
    assert_eq!(
        displayed(cx, &view).len(),
        1,
        "the frozen end holds in hex too"
    );

    // Selection copies the row text; exports follow the display.
    terminal.update(cx, |t, cx| t.select_all(cx));
    assert_eq!(
        terminal.read_with(cx, |t, _| t.selection_text()),
        Some(rows[0].text.clone())
    );
    export_now(cx, &view, dir.join("rows.txt"), ExportFormat::Text);
    assert_eq!(
        wait_notice(cx, &view, "rows.txt"),
        Notice::info("Exported 1 hex rows to rows.txt")
    );
    assert_eq!(
        fs::read_to_string(dir.join("rows.txt")).unwrap(),
        format!("{}\n", rows[0].text)
    );
    export_now(cx, &view, dir.join("row.bin"), ExportFormat::Raw);
    wait_notice(cx, &view, "row.bin");
    assert_eq!(
        fs::read(dir.join("row.bin")).unwrap(),
        b"hello world 12\r\n",
        "raw export of a selected row is its bytes"
    );

    view.update(cx, |v, cx| v.resume(cx));
    step(cx, 1);
    assert_eq!(displayed(cx, &view).len(), 2, "resumed: the new row shows");
    press(cx, window, "alt-h");
    assert_eq!(
        terminal.read_with(cx, |t, _| t.display_mode()),
        DisplayMode::Text
    );
    let texts: Vec<String> = displayed(cx, &view).into_iter().map(|l| l.text).collect();
    assert_eq!(
        texts[1..],
        ["hello world 12", "hello world 12", "again", "again"]
    );
}

#[gpui_test]
fn an_open_search_finds_lines_that_arrive_after_the_query(cx: &mut TestAppContext) {
    const TOTAL: u64 = 512 * 1024;
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_total(TOTAL));
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:gated"));
    let view = wait_connected(cx, &workspace);
    let terminal = terminal_of(cx, &view);

    // Search for a record the device has not sent yet.
    cx.update_window(window, |_, window, cx| {
        terminal.update(cx, |t, cx| t.deploy_search(window, cx));
        window.input("#00000800 ", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        terminal.read_with(cx, |t, _| t.search_results().count_label()),
        "0/0"
    );

    view.update(cx, |v, cx| v.send("go", b"go\r\n".to_vec(), cx));
    run_until(cx, "the whole stream", |cx| received(cx, &view) == TOTAL);
    run_until(cx, "the search to catch up", |cx| {
        terminal.read_with(cx, |t, _| {
            !t.search_results().pending && t.search_results().matches.len() == 1
        })
    });
    let found = terminal.read_with(cx, |t, _| {
        let results = t.search_results();
        assert_eq!(results.count_label(), "1/1");
        results.matches[0].clone()
    });
    let line = view
        .read_with(cx, |v, _| v.snapshot().line(found.line))
        .expect("a retained line");
    assert!(line.text.starts_with("#00000800 "), "{:?}", line.text);
    assert!(
        view.read_with(cx, |v, cx| v.is_following_tail(cx)),
        "new matches do not pull the view away from the tail"
    );
}
