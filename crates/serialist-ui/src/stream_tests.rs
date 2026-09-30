//! Pause, export and record against the real engine: `Session` threads over `SimWorld`
//! links, driven through the workspace as in `gate.rs`.

use std::fs;
use std::time::Instant;

use serialist_core::PortId;
use serialist_sim::{
    DeviceOutput, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator,
    FirehoseVerifier, LinkConfig, SimDevice, SimWorld,
};

use crate::capture::RawRing;
use crate::export::ExportFormat;
use crate::line_buffer::DEFAULT_MAX_LINES;
use crate::prelude::*;
use crate::session_model::{Notice, format_bytes};
use crate::session_view::SessionView;
use crate::test_support::{
    TestDir, has_rx_line, open_workspace, run_until, type_line, wait_connected,
};

/// Fast enough to fill the 10 000-line scrollback in a fraction of a second, slow enough
/// that every batch while paused brings something new.
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
/// ready (start a recording, shrink the raw ring) before the stream begins.
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

fn shown(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<String> {
    view.read_with(cx, |v, _| v.model().displayed_text())
}

fn live(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<String> {
    view.read_with(cx, |v, _| {
        v.model().buffer.rows().map(|r| r.text.to_owned()).collect()
    })
}

fn wait_full(cx: &mut TestAppContext, view: &Entity<SessionView>) {
    run_until(cx, "a full scrollback", |cx| {
        view.read_with(cx, |v, _| v.model().buffer.len() == DEFAULT_MAX_LINES)
    });
}

fn wait_notice(cx: &mut TestAppContext, view: &Entity<SessionView>, needle: &str) -> Notice {
    run_until(cx, needle, |cx| {
        view.read_with(cx, |v, _| {
            v.model()
                .notice
                .as_ref()
                .is_some_and(|n| n.text.contains(needle))
        })
    });
    view.read_with(cx, |v, _| v.model().notice.clone().unwrap())
}

fn as_text_file(rows: &[String]) -> String {
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn step(cx: &mut TestAppContext, frames: usize) {
    for _ in 0..frames {
        cx.executor().advance_clock(crate::drain::FRAME);
        cx.run_until_parked();
    }
}

fn raw_bytes(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<u8> {
    view.read_with(cx, |v, _| {
        v.model()
            .raw
            .chunks()
            .flat_map(|chunk| chunk.iter().copied())
            .collect()
    })
}

#[gpui_test]
fn pause_freezes_the_screen_while_the_stream_keeps_arriving(cx: &mut TestAppContext) {
    let world = firehose_world(FAST);
    let (_window, workspace) = open_workspace(cx, &world, Some("virtual:firehose"));
    let view = wait_connected(cx, &workspace);
    wait_full(cx, &view);

    view.update(cx, |v, cx| v.pause(cx));
    let frozen = shown(cx, &view);
    assert_eq!(frozen.len(), DEFAULT_MAX_LINES);
    let rx_at_pause = view.read_with(cx, |v, _| v.model().stats.rx_bytes);

    let mut batches = 0;
    let mut seen = 0;
    run_until(cx, "three batches while paused", |cx| {
        assert!(shown(cx, &view) == frozen, "the paused rows moved");
        let bytes = view.read_with(cx, |v, _| v.model().received_since_pause().unwrap().1);
        if bytes > seen {
            seen = bytes;
            batches += 1;
        }
        batches >= 3
    });

    view.read_with(cx, |v, _| {
        let model = v.model();
        let (lines, bytes) = model.received_since_pause().unwrap();
        assert!(lines > 0 && bytes > 0);
        assert!(model.stats.rx_bytes > rx_at_pause, "RX keeps counting");
        assert_eq!(
            model.buffer.len(),
            DEFAULT_MAX_LINES,
            "and the buffer capping"
        );
        assert_eq!(
            model.status_line().paused,
            Some(format!("Paused, +{lines} lines, +{}", format_bytes(bytes)))
        );
    });
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert!(status.paused.unwrap().starts_with("Paused, +"));

    view.update(cx, |v, cx| v.resume(cx));
    let now_shown = shown(cx, &view);
    assert_eq!(now_shown, live(cx, &view), "resume shows the live buffer");
    assert_eq!(now_shown.len(), DEFAULT_MAX_LINES, "still capped");
    assert_ne!(now_shown.last(), frozen.last(), "with the newest rows");
    view.read_with(cx, |v, _| {
        assert!(!v.model().is_paused());
        assert!(v.is_following_tail());
        assert_eq!(v.model().status_line().paused, None);
    });
}

#[gpui_test]
fn text_export_writes_the_rows_on_screen(cx: &mut TestAppContext) {
    let dir = TestDir::new("export-text");
    let world = firehose_world(FAST);
    let (_window, workspace) = open_workspace(cx, &world, Some("virtual:firehose"));
    let view = wait_full_view(cx, &workspace);

    view.update(cx, |v, cx| v.pause(cx));
    let frozen = shown(cx, &view);
    view.update(cx, |v, cx| {
        v.export_to(dir.join("paused.txt"), ExportFormat::Text, cx)
    })
    .detach();
    let notice = wait_notice(cx, &view, "paused.txt");
    assert_eq!(notice, Notice::info("Exported 10000 lines to paused.txt"));
    step(cx, 3);
    assert_eq!(
        fs::read_to_string(dir.join("paused.txt")).unwrap(),
        as_text_file(&frozen),
        "exactly the frozen rows"
    );

    view.update(cx, |v, cx| v.resume(cx));
    step(cx, 3);
    let (rows, export) = view.update(cx, |v, cx| {
        let rows = v.model().displayed_text();
        (
            rows,
            v.export_to(dir.join("live.txt"), ExportFormat::Text, cx),
        )
    });
    export.detach();
    wait_notice(cx, &view, "live.txt");
    assert_ne!(rows, frozen);
    assert_eq!(
        fs::read_to_string(dir.join("live.txt")).unwrap(),
        as_text_file(&rows),
        "exactly the live rows at the moment of export"
    );
}

fn wait_full_view(
    cx: &mut TestAppContext,
    workspace: &Entity<crate::workspace::Workspace>,
) -> Entity<SessionView> {
    let view = wait_connected(cx, workspace);
    wait_full(cx, &view);
    view
}

#[gpui_test]
fn raw_export_is_exactly_what_the_device_sent(cx: &mut TestAppContext) {
    let dir = TestDir::new("export-raw");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "hello");
    type_line(cx, window, "world");
    run_until(cx, "both echoes", |cx| has_rx_line(cx, &view, "world"));

    view.update(cx, |v, cx| {
        v.export_to(dir.join("echo.bin"), ExportFormat::Raw, cx)
    })
    .detach();
    let notice = wait_notice(cx, &view, "echo.bin");
    assert_eq!(notice, Notice::info("Exported 14 B raw to echo.bin"));

    let exported = fs::read(dir.join("echo.bin")).unwrap();
    assert_eq!(exported, b"hello\r\nworld\r\n");
    let link = world.link(&PortId::new("virtual:echo")).expect("open link");
    assert_eq!(exported.len() as u64, link.stats().device_to_host_bytes);
}

#[gpui_test]
fn recording_writes_the_raw_stream_until_stopped(cx: &mut TestAppContext) {
    let dir = TestDir::new("record");
    // Slow enough that the recorder's 256 KiB buffer never fills, so bytes on disk
    // before the stop can only come from the timed flush on the drain path.
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_rate(64 * 1024));
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:gated"));
    let view = wait_connected(cx, &workspace);

    let path = dir.join("capture.bin");
    view.update(cx, |v, cx| v.start_recording(path.clone(), cx));
    run_until(cx, "the recording file to open", |cx| {
        view.read_with(cx, |v, _| {
            v.model()
                .recording
                .as_ref()
                .is_some_and(|r| r.stats.is_some())
        })
    });

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
    step(cx, 5);
    assert_eq!(
        fs::read(&path).unwrap().len(),
        recorded.len(),
        "nothing is written after the stop"
    );

    let mut stream = Vec::new();
    FirehoseGenerator::new(FirehoseContent::Text, 0).fill(&mut stream, recorded.len());
    assert!(
        recorded == stream,
        "the file is the stream from its first byte"
    );
    assert!(
        raw_bytes(cx, &view).starts_with(&recorded),
        "and what the session received"
    );

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    verifier.feed(&recorded);
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.missing_records, 0);
    assert!(report.records > 10, "{report:?}");
}

#[gpui_test]
fn the_raw_ring_keeps_the_newest_bytes_and_counts_the_rest(cx: &mut TestAppContext) {
    const TOTAL: u64 = 1024 * 1024;
    const CAPACITY: usize = 64 * 1024;
    let world = gated_world(FirehoseConfig::new(FirehoseContent::Text).with_total(TOTAL));
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:gated"));
    let view = wait_connected(cx, &workspace);
    view.update(cx, |v, _| v.model_mut().raw = RawRing::new(CAPACITY));

    type_line(cx, window, "go");
    run_until(cx, "the whole stream", |cx| {
        view.read_with(cx, |v, _| v.model().raw.end_offset() == TOTAL)
    });

    let kept = raw_bytes(cx, &view);
    let evicted = view.read_with(cx, |v, _| v.model().raw.evicted_bytes());
    assert!(kept.len() <= CAPACITY && !kept.is_empty());
    assert_eq!(evicted, TOTAL - kept.len() as u64);
    let mut stream = Vec::new();
    FirehoseGenerator::new(FirehoseContent::Text, 0).fill(&mut stream, TOTAL as usize);
    assert!(
        kept == stream[evicted as usize..],
        "the newest bytes, in order"
    );

    let dir = TestDir::new("export-evicted");
    view.update(cx, |v, cx| {
        v.export_to(dir.join("tail.bin"), ExportFormat::Raw, cx)
    })
    .detach();
    let notice = wait_notice(cx, &view, "tail.bin");
    assert!(
        notice.text.ends_with(&format!(
            "{} older were already dropped",
            format_bytes(evicted)
        )),
        "{notice:?}"
    );
    assert_eq!(fs::read(dir.join("tail.bin")).unwrap(), kept);
}

#[gpui_test]
fn pause_export_and_record_have_default_keys(cx: &mut TestAppContext) {
    let dir = TestDir::new("keys");
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let view = wait_connected(cx, &workspace);
    type_line(cx, window, "hello");
    run_until(cx, "the echo", |cx| has_rx_line(cx, &view, "hello"));

    let (pause, export, record) = if cfg!(target_os = "macos") {
        ("cmd-p", "cmd-s", "cmd-shift-r")
    } else {
        ("ctrl-p", "ctrl-shift-s", "ctrl-shift-r")
    };
    let press = |cx: &mut TestAppContext, key: &str| {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    };
    let paused = |cx: &mut TestAppContext| view.read_with(cx, |v, _| v.model().is_paused());

    press(cx, pause);
    assert!(paused(cx));
    press(cx, pause);
    assert!(!paused(cx));

    press(cx, export);
    assert!(cx.did_prompt_for_new_path(), "export asks where to save");
    let text_path = dir.join("keys.txt");
    let chosen = text_path.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    wait_notice(cx, &view, "keys.txt");
    let rows = shown(cx, &view);
    assert_eq!(fs::read_to_string(&text_path).unwrap(), as_text_file(&rows));

    press(cx, record);
    assert!(cx.did_prompt_for_new_path(), "record asks where to save");
    let raw_path = dir.join("keys.bin");
    let chosen = raw_path.clone();
    cx.simulate_new_path_selection(move |_| Some(chosen));
    run_until(cx, "the recording to start", |cx| {
        view.read_with(cx, |v, _| {
            v.model()
                .recording
                .as_ref()
                .is_some_and(|r| r.stats.is_some())
        })
    });
    type_line(cx, window, "again");
    run_until(cx, "the second echo", |cx| has_rx_line(cx, &view, "again"));
    press(cx, record);
    wait_notice(cx, &view, "Recorded");
    assert_eq!(fs::read(&raw_path).unwrap(), b"again\r\n");
    assert!(view.read_with(cx, |v, _| v.model().recording.is_none()));
}

/// Start recording an echo session, get one line echoed, then end the session with `end`.
/// The line is far smaller than the recorder's buffer, so it only reaches the file
/// through the final flush.
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
    run_until(cx, "the recording to start", |cx| {
        view.read_with(cx, |v, _| {
            v.model()
                .recording
                .as_ref()
                .is_some_and(|r| r.stats.is_some())
        })
    });
    type_line(cx, window, "bye");
    run_until(cx, "the echo", |cx| has_rx_line(cx, &view, "bye"));

    end(cx, &world, &view);
    let notice = wait_notice(cx, &view, "Recorded");
    assert_eq!(notice, Notice::info("Recorded 5 B to session.bin"));
    assert_eq!(fs::read(&path).unwrap(), b"bye\r\n");
    assert!(view.read_with(cx, |v, _| v.model().recording.is_none()));
}

#[gpui_test]
fn disconnecting_stops_the_recording_with_a_final_flush(cx: &mut TestAppContext) {
    recording_survives(cx, "record-disconnect", |cx, _, view| {
        view.update(cx, |v, cx| v.disconnect(cx));
    });
}

#[gpui_test]
fn losing_the_device_stops_the_recording_with_a_final_flush(cx: &mut TestAppContext) {
    recording_survives(cx, "record-unplug", |_, world, _| {
        world.unplug(&PortId::new("virtual:echo"));
    });
}
