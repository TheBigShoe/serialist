//! Codecs, the Decoded panel, codec-encoded saved commands, plugin reload, hidden frames
//! and decoded export against the real engine: `Session` and ingest threads over a
//! `SimWorld` link to the simulated Airoha RACE device, with the configuration (device
//! profiles, plugins) in a temporary config directory loaded by the real loaders.

use std::fs;

use serde_json::Value as JsonValue;
use serialist_core::settings::ConfigPaths;
use serialist_core::{
    Codec, CommandRef, Direction, Frame, FrameSnapshot, LineSource, PortId, Snapshot, Value,
};
use serialist_plugins::race::race_info;
use serialist_plugins::{AIROHA_RACE_LUA, AirohaRace};
use serialist_sim::SimWorld;

use crate::codecs::{DECODED_MARK, hides_bytes};
use crate::config::{self, Config, ConfigPiece};
use crate::decoded_panel::{DecodedPanel, cell_advance};
use crate::export::{ExportFormat, ExportJob};
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::terminal::TimestampMode;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, open_test_window, parse_csv, run_until,
    wait_connected,
};
use crate::workspace::{AppOptions, Workspace};

/// What [`open`] opens: the window, the workspace, the session on `virtual:race`, and the
/// simulator behind it, which the test keeps.
struct Opened {
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    view: Entity<SessionView>,
    _world: SimWorld,
}

/// A device profile for the simulated RACE device decoding with `plugin`.
fn race_profile(plugin: &str) -> String {
    format!(
        r#"{{ "devices": [ {{ "name": "RACE board", "match": {{ "path": "virtual:race" }},
                            "plugin": "{plugin}" }} ] }}"#
    )
}

/// A config directory with `settings` as its settings file.
fn config_dir(name: &str, settings: &str) -> TestDir {
    let dir = TestDir::new(name);
    fs::write(dir.join("settings.json"), settings).unwrap();
    dir
}

/// The bundled Lua RACE plugin in `plugins/<folder>/plugin.lua`, as `source`.
fn write_plugin(dir: &TestDir, folder: &str, source: &str) {
    let plugin = dir.join("plugins").join(folder);
    fs::create_dir_all(&plugin).unwrap();
    fs::write(plugin.join("plugin.lua"), source).unwrap();
}

/// A workspace over the simulator with the configuration under `dir` loaded (and
/// watched, with `watch`), opening `virtual:race` at startup.
fn open(cx: &mut TestAppContext, dir: &TestDir, watch: bool) -> Opened {
    allow_engine_threads(cx);
    let world = SimWorld::new();
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: Some(PortId::new("virtual:race")),
        connect_on_start: true,
        store: None,
    };
    let paths = ConfigPaths::new(dir.path());
    let (window, workspace) = open_test_window(cx, move |window, cx| {
        if watch {
            config::start(paths, cx);
        } else {
            config::install(Config::load(paths, false), cx);
        }
        Workspace::new(options, window, cx)
    });
    let view = wait_connected(cx, &workspace);
    Opened {
        window,
        workspace,
        view,
        _world: world,
    }
}

fn panel(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Entity<DecodedPanel> {
    workspace.read_with(cx, |w, _| w.decoded().clone())
}

/// The frames the Decoded panel shows.
fn panel_frames(cx: &mut TestAppContext, workspace: &Entity<Workspace>) -> Vec<Frame> {
    let panel = panel(cx, workspace);
    panel.read_with(cx, |panel, cx| panel.frames(cx))
}

fn uint(frame: &Frame, name: &str) -> Option<u64> {
    frame.field(name).and_then(Value::as_u64)
}

fn is_log(frame: &Frame) -> bool {
    frame.kind == "log" && uint(frame, "cmd_id") == Some(0x0F40)
}

/// Wait until the Decoded panel shows frames satisfying `done`, and return them.
fn wait_frames(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
    what: &str,
    mut done: impl FnMut(&[Frame]) -> bool,
) -> Vec<Frame> {
    let mut frames = Vec::new();
    run_until(cx, what, |cx| {
        frames = panel_frames(cx, workspace);
        done(&frames)
    });
    frames
}

fn notice(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Option<String> {
    view.read_with(cx, |v, _| v.notice().map(|notice| notice.text.clone()))
}

/// The bytes of `frame` from the store.
fn raw_bytes(snapshot: &Snapshot, frame: &Frame) -> Vec<u8> {
    snapshot.raw(frame.raw.clone()).flatten().copied().collect()
}

#[gpui_test]
fn race_frames_fill_the_panel_and_a_frame_reply_resolves_the_send(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-race", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir, false);
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        Some("airoha-race".into())
    );

    // The banner is a text frame, then the device's logs.
    let frames = wait_frames(cx, &workspace, "the banner and two logs", |frames| {
        frames.iter().filter(|frame| is_log(frame)).count() >= 2
    });
    let banner = frames
        .iter()
        .find(|frame| frame.kind == "text")
        .expect("the banner's text frame");
    assert_eq!(
        banner.field("text").and_then(Value::as_str),
        Some("Airoha RACE simulator SIM-RACE 1.4.2")
    );
    let log = frames.iter().find(|frame| is_log(frame)).unwrap();
    assert!(
        log.summary.starts_with("log 0x0F40 len "),
        "{}",
        log.summary
    );
    // The panel draws it: a time, the direction, the kind and, once the raw column is
    // turned on (it is off by default), its hex bytes.
    let decoded = panel(cx, &workspace);
    let (headers, _) = decoded.read_with(cx, |panel, cx| panel.dump(cx));
    assert_eq!(headers, ["Time", "Dir", "Kind", "Summary", "Fields"]);
    decoded.update(cx, |panel, cx| panel.set_raw_column(true, cx));
    let (headers, rows) = decoded.read_with(cx, |panel, cx| panel.dump(cx));
    assert_eq!(headers, ["Time", "Dir", "Kind", "Summary", "Fields", "Raw"]);
    let row = rows.iter().find(|row| row[2] == "log").unwrap();
    assert_eq!(row[1], "RX");
    assert!(row[4].contains("cmd_id=3904"), "{row:?}");
    assert!(row[5].starts_with("05 5D "), "{row:?}");
    assert!(!row[0].is_empty());
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();

    // The bundled RACE version command, from the Commands panel: a codec payload,
    // echoed as hex, answered by a response frame the send waits for.
    let commands = workspace.read_with(cx, |w, _| w.commands().clone());
    commands.update(cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "RACE", "RACE version"), cx);
    });
    let frames = wait_frames(cx, &workspace, "the version response", |frames| {
        frames
            .iter()
            .any(|frame| frame.kind == "response" && uint(frame, "cmd_id") == Some(0x0F15))
    });
    let response = frames
        .iter()
        .find(|frame| frame.kind == "response")
        .unwrap();
    let payload = response.field("payload").and_then(Value::as_bytes).unwrap();
    assert_eq!(payload[0], 0x00, "status OK");
    assert!(
        payload.windows(14).any(|w| w == b"SIM-RACE 1.4.2"),
        "{payload:?}"
    );
    run_until(cx, "the frame reply", |cx| {
        notice(cx, &view).is_some_and(|text| text.starts_with("RACE version: OK in "))
    });
    let lines = displayed(cx, &view);
    assert!(
        lines
            .iter()
            .any(|line| line.direction == Direction::Tx && line.text == "05 5A 02 00 15 0F"),
        "the echo shows the encoded bytes"
    );
    // The response's bytes are marked in the terminal, as a matched reply is.
    view.read_with(cx, |view, cx| {
        let marks = view.terminal().read(cx).marks().clone();
        let snapshot = view.snapshot();
        assert!(
            marks.iter().any(
                |mark| snapshot
                    .line(mark.line)
                    .is_some_and(|line| line.raw.start < response.raw.end
                        && response.raw.start < line.raw.end)
            ),
            "{marks:?}"
        );
        assert_eq!(
            view.status_line().codec.as_deref(),
            Some("Codec: airoha-race")
        );
    });
}

#[gpui_test]
fn a_frame_reply_that_never_comes_times_out(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-timeout", &race_profile("airoha-race"));
    fs::create_dir_all(dir.join("commands")).unwrap();
    fs::write(
        dir.join("commands").join("race.json"),
        r#"{ "name": "Mine", "groups": [ { "name": "G", "commands": [
            { "name": "Nothing", "payload": { "codec": "airoha-race", "fields": { "cmd_id": "0x0F15" } },
              "expect": { "frame": { "kind": "indication" }, "timeout_ms": 300 } } ] } ] }"#,
    )
    .unwrap();
    let Opened {
        workspace,
        view,
        _world,
        ..
    } = open(cx, &dir, false);
    let commands = workspace.read_with(cx, |w, _| w.commands().clone());
    commands.update(cx, |panel, cx| {
        panel.send(CommandRef::new("Mine", "G", "Nothing"), cx);
    });
    run_until(cx, "the timeout", |cx| {
        notice(cx, &view).is_some_and(|text| text == "Nothing: no response within 300 ms")
    });
    run_until(cx, "the timeout's notice line", |cx| {
        displayed(cx, &view).iter().any(|line| {
            line.direction == Direction::Notice && line.text == "Nothing: no response within 300 ms"
        })
    });
}

/// Every frame decoded after `from` (by the codec under test) against the Rust codec
/// decoding the same bytes: kind, fields and summary alike.
fn assert_like_the_rust_codec(frames: &[Frame], snapshot: &Snapshot) {
    let mut checked = 0;
    for frame in frames
        .iter()
        .filter(|frame| frame.kind != "text" && frame.kind != "malformed")
    {
        let bytes = raw_bytes(snapshot, frame);
        let mut rust = AirohaRace::new();
        let mut out = Vec::new();
        rust.decode(&bytes, frame.at, frame.raw.start, &mut out);
        assert_eq!(out.len(), 1, "{frame:?}");
        assert_eq!(out[0].kind, frame.kind);
        assert_eq!(out[0].fields, frame.fields);
        assert_eq!(out[0].summary, frame.summary);
        assert_eq!(out[0].raw, frame.raw);
        checked += 1;
    }
    assert!(checked > 0, "no binary frame to compare");
}

#[gpui_test]
fn a_lua_plugin_decodes_the_same_and_reloads_when_saved(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-lua", &race_profile("airoha-race"));
    write_plugin(&dir, "airoha-race-lua", AIROHA_RACE_LUA);
    let Opened {
        workspace,
        view,
        _world,
        ..
    } = open(cx, &dir, true);
    // The plugin is registered under its folder, beside the built-in.
    let choices = cx.update(|cx| cx.global::<Config>().codecs().choices());
    assert_eq!(
        choices,
        ["none", "airoha-race", "airoha-race-lua", "text-lines"]
    );
    wait_frames(cx, &workspace, "a log from the Rust codec", |frames| {
        frames.iter().any(is_log)
    });

    // Switching: the frames so far stay, and the Lua codec takes over at the next chunk.
    let before = view.read_with(cx, |v, _| v.frames().end());
    assert!(view.update(cx, |v, cx| v.set_codec(Some("airoha-race-lua"), cx)));
    assert_eq!(
        view.read_with(cx, |v, _| v.status_line().codec),
        Some("Codec: airoha-race-lua".into())
    );
    let frames = wait_frames(cx, &workspace, "two logs from the Lua codec", |frames| {
        frames.len() as u64 > before.0 + 2
            && frames[before.0 as usize..]
                .iter()
                .filter(|f| is_log(f))
                .count()
                >= 2
    });
    assert!(frames.len() as u64 >= before.0, "earlier frames kept");
    let snapshot = view.read_with(cx, |v, _| v.snapshot().clone());
    // The first frame after the switch may begin mid-frame; compare the whole ones after.
    assert_like_the_rust_codec(&frames[before.0 as usize + 1..], &snapshot);

    // Saving an edit reloads the plugin; later frames use its new summary.
    let edited = AIROHA_RACE_LUA.replace(
        r#"local s = format("%s 0x%04X len %d", kind, cmd_id, #payload)"#,
        r#"local s = format("LUA %s 0x%04X len %d", kind, cmd_id, #payload)"#,
    );
    assert_ne!(
        edited, AIROHA_RACE_LUA,
        "the summary format is in the plugin"
    );
    write_plugin(&dir, "airoha-race-lua", &edited);
    wait_frames(cx, &workspace, "a log with the new summary", |frames| {
        frames
            .iter()
            .any(|frame| frame.summary.starts_with("LUA log 0x0F40 len "))
    });
    assert_eq!(
        notice(cx, &view).as_deref(),
        Some("Reloaded airoha-race-lua")
    );

    // A broken edit is a problem in the status line, and the codec that runs stays.
    write_plugin(&dir, "airoha-race-lua", "return {");
    run_until(cx, "the plugin problem", |cx| {
        workspace.read_with(cx, |w, cx| {
            w.config_notice(cx).is_some_and(|notice| {
                notice.is_error
                    && notice.text.contains("airoha-race-lua")
                    && notice.text.contains("not loaded")
            })
        })
    });
    let seen = view.read_with(cx, |v, _| v.frames().end());
    let frames = wait_frames(
        cx,
        &workspace,
        "more logs after the broken edit",
        |frames| frames.len() as u64 > seen.0 + 1 && frames.last().is_some_and(is_log),
    );
    assert!(
        frames.last().unwrap().summary.starts_with("LUA log"),
        "the last good version keeps decoding: {:?}",
        frames.last()
    );
    // Reloading by hand finds the same problem, and changes nothing.
    cx.update(|cx| config::reload(ConfigPiece::Plugins, cx));
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        Some("airoha-race-lua".into())
    );
}

/// Whether every byte of `line` is in a frame that may hide it.
fn line_hidden_by(frames: &FrameSnapshot, raw: std::ops::Range<u64>) -> bool {
    let info = race_info();
    let mut pos = raw.start;
    for (_, frame) in frames.frames() {
        if frame.raw.is_empty() || frame.raw.end <= pos {
            continue;
        }
        if frame.raw.start > pos || !hides_bytes(frame, Some(&info)) {
            return false;
        }
        pos = frame.raw.end;
        if pos >= raw.end {
            return true;
        }
    }
    false
}

#[gpui_test]
fn summaries_go_into_the_scrollback_and_framed_lines_can_be_hidden(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-inline", &race_profile("airoha-race"));
    let Opened {
        workspace,
        view,
        _world,
        ..
    } = open(cx, &dir, false);
    assert!(
        view.read_with(cx, |v, _| v.decoded_inline()),
        "on by default"
    );

    // Summaries of the logs, as notice lines; none for the text frames.
    run_until(cx, "two log summaries", |cx| {
        displayed(cx, &view)
            .iter()
            .filter(|line| {
                line.direction == Direction::Notice
                    && line
                        .text
                        .starts_with(&format!("{DECODED_MARK}log 0x0F40 len "))
            })
            .count()
            >= 2
    });
    let lines = displayed(cx, &view);
    assert!(
        !lines
            .iter()
            .any(|line| line.text == format!("{DECODED_MARK}Airoha RACE simulator SIM-RACE 1.4.2")),
        "text frames have no summary"
    );
    // A summary cuts the line of frames it follows, so lines of binary frames alone
    // show until they are hidden.
    let frames = view.read_with(cx, |v, _| v.frame_reader().snapshot());
    let binary = |lines: &[serialist_core::StyledLine], frames: &FrameSnapshot| {
        lines
            .iter()
            .filter(|line| {
                line.direction == Direction::Rx
                    && !line.raw.is_empty()
                    && line_hidden_by(frames, line.raw.clone())
            })
            .count()
    };
    assert!(binary(&lines, &frames) > 0, "{lines:?}");

    view.update(cx, |v, cx| v.set_hide_framed_bytes(true, cx));
    // Let more logs and a heartbeat line (after every fourth log) arrive, then look.
    let seen = view.read_with(cx, |v, _| v.frames().end());
    wait_frames(
        cx,
        &workspace,
        "more logs and a heartbeat while hiding",
        |frames| {
            frames.len() as u64 >= seen.0 + 3
                && frames
                    .iter()
                    .any(|frame| frame.kind == "text" && frame.summary.contains("heartbeat"))
        },
    );
    let (lines, frames, total) = view.read_with(cx, |v, cx| {
        let terminal = v.terminal().read(cx);
        let mut lines = Vec::new();
        terminal
            .source()
            .lines(terminal.displayed_span().range(), &mut lines);
        (lines, v.frames().clone(), v.snapshot().line_count())
    });
    assert!(lines.len() < total, "some lines are hidden");
    assert_eq!(
        binary(&lines, &frames),
        0,
        "no line of binary frames alone shows"
    );
    assert!(
        lines.iter().any(|line| line.direction == Direction::Rx
            && line.text == "Airoha RACE simulator SIM-RACE 1.4.2"),
        "text lines stay"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.direction == Direction::Rx && line.text.contains("sim: heartbeat")),
        "the device's text lines stay: {lines:?}"
    );
    assert!(lines.iter().any(|line| line.text.starts_with(DECODED_MARK)));

    // Showing them again brings every line back.
    view.update(cx, |v, cx| v.set_hide_framed_bytes(false, cx));
    let shown = displayed(cx, &view).len();
    let total = view.read_with(cx, |v, _| v.snapshot().line_count());
    assert_eq!(shown, total);
}

#[gpui_test]
fn selecting_a_decoded_row_marks_its_bytes_in_the_terminal(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-select", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir, false);
    wait_frames(cx, &workspace, "three logs", |frames| {
        frames.iter().filter(|f| is_log(f)).count() >= 3
    });
    let panel = panel(cx, &workspace);
    let (rows, frames) = panel.read_with(cx, |p, cx| (p.rows(cx), p.frames(cx)));
    let ix = frames.iter().position(is_log).unwrap();
    let (id, frame) = (rows[ix], frames[ix].clone());

    // A click on the row, as a user would.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
    })
    .unwrap();
    panel.update(cx, |panel, cx| panel.follow(cx));
    let clicked = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let found = window.try_find(("decoded-row", ix)).is_some();
            if found {
                window.click(("decoded-row", ix), cx);
            }
            found
        })
        .unwrap();
    assert!(clicked, "the row is on screen");
    cx.run_until_parked();
    assert!(
        !panel.read_with(cx, |p, _| p.is_following()),
        "a selection stops following"
    );
    view.read_with(cx, |view, cx| {
        assert_eq!(view.selected_frame(), Some(id));
        let marks = view.frame_marks().to_vec();
        assert!(!marks.is_empty());
        let snapshot = view.snapshot();
        for mark in &marks {
            let line = snapshot.line(mark.line).unwrap();
            assert!(
                line.raw.start < frame.raw.end && frame.raw.start < line.raw.end,
                "the marked line holds the frame's bytes"
            );
            assert_eq!(mark.range, 0..line.text.len());
        }
        let shown = view.terminal().read(cx).marks().clone();
        assert!(marks.iter().all(|mark| shown.contains(mark)));
    });

    // In hex view the terminal scrolls to the frame's first byte instead.
    view.update(cx, |v, cx| {
        v.terminal().update(cx, |t, cx| {
            t.set_display_mode(crate::terminal::DisplayMode::Hex, cx)
        });
        v.select_frame(id, cx);
    });
    assert_eq!(view.read_with(cx, |v, _| v.selected_frame()), Some(id));
    // Follow brings the table back to the newest frame.
    panel.update(cx, |panel, cx| panel.follow(cx));
    assert!(panel.read_with(cx, |p, _| p.is_following()));
}

/// The width of the column headed `name`.
fn width_of(columns: &[(String, f32)], name: &str) -> f32 {
    columns
        .iter()
        .find(|(header, _)| header == name)
        .unwrap_or_else(|| panic!("no {name} column in {columns:?}"))
        .1
}

#[gpui_test]
fn the_time_column_fits_its_stamp_and_summary_takes_the_rest(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-widths", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir, false);
    wait_frames(cx, &workspace, "the banner and two logs", |frames| {
        frames.iter().filter(|frame| is_log(frame)).count() >= 2
    });
    let decoded = panel(cx, &workspace);
    let draw = |cx: &mut TestAppContext| {
        // One frame to measure the table, one to lay it out with the answer.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
    };
    let advance = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| f32::from(cell_advance(window, cx)))
            .unwrap()
    };
    draw(cx);

    // The column holds the longest stamp shown, in cells of the monospace font.
    let (_, rows) = decoded.read_with(cx, |panel, cx| panel.dump(cx));
    let longest = rows.iter().map(|row| row[0].chars().count()).max().unwrap();
    assert_eq!(longest, 12, "HH:MM:SS.mmm");
    let columns = decoded.read_with(cx, |panel, cx| panel.column_widths(cx));
    let time = width_of(&columns, "Time");
    assert!(
        time >= advance(cx) * longest as f32,
        "{time} px holds {longest} cells of {}",
        advance(cx)
    );
    assert!(time <= advance(cx) * longest as f32 + 20.);

    // Summary is the rest of what the table has, after Time, Dir and Kind.
    let viewport = decoded.read_with(cx, |panel, cx| panel.viewport_width(cx));
    assert!(viewport > 0., "the table was measured");
    let (dir, kind, summary) = (
        width_of(&columns, "Dir"),
        width_of(&columns, "Kind"),
        width_of(&columns, "Summary"),
    );
    let expected = (viewport - 12. - time - dir - kind).max(160.);
    assert!(
        (summary - expected).abs() < 1.,
        "Summary {summary} px, {expected} px left of {viewport}"
    );

    // Another stamp mode, another width: a relative stamp is one character longer.
    view.update(cx, |view, cx| {
        view.terminal().update(cx, |terminal, cx| {
            terminal.set_timestamps(TimestampMode::Relative, cx)
        });
    });
    draw(cx);
    let (_, rows) = decoded.read_with(cx, |panel, cx| panel.dump(cx));
    let longest = rows.iter().map(|row| row[0].chars().count()).max().unwrap();
    assert_eq!(longest, 13, "+HH:MM:SS.mmm");
    let relative = width_of(
        &decoded.read_with(cx, |panel, cx| panel.column_widths(cx)),
        "Time",
    );
    assert!(
        (relative - time - advance(cx)).abs() < 1.,
        "{relative} px against {time} px and a cell of {}",
        advance(cx)
    );
}

#[gpui_test]
fn the_raw_column_is_off_until_the_toggle_turns_it_on(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-raw-toggle", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        _world,
        ..
    } = open(cx, &dir, false);
    wait_frames(cx, &workspace, "the banner and two logs", |frames| {
        frames.iter().filter(|frame| is_log(frame)).count() >= 2
    });
    let decoded = panel(cx, &workspace);
    let headers = |cx: &mut TestAppContext| decoded.read_with(cx, |p, cx| p.dump(cx).0);
    assert!(!decoded.read_with(cx, |p, cx| p.raw_column(cx)));
    assert!(!headers(cx).contains(&"Raw".to_owned()));

    // The Hex toggle, clicked as a user would.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("decoded-raw", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(decoded.read_with(cx, |p, cx| p.raw_column(cx)));
    assert_eq!(headers(cx).last().map(String::as_str), Some("Raw"));

    // It survives a kind filter (whose columns are the kind's fields) and goes again.
    decoded.update(cx, |p, cx| p.set_kind(Some("log".into()), cx));
    assert_eq!(headers(cx).last().map(String::as_str), Some("Raw"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("decoded-raw", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert!(!headers(cx).contains(&"Raw".to_owned()));
}

#[gpui_test]
fn the_hex_preview_opens_on_a_selection_and_closes_with_it(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-preview", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir, false);
    let frames = wait_frames(cx, &workspace, "the banner and two logs", |frames| {
        frames.iter().filter(|frame| is_log(frame)).count() >= 2
    });
    let decoded = panel(cx, &workspace);
    let previewed = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find("decoded-preview").is_some()
        })
        .unwrap()
    };

    // Nothing is selected: no preview, and no raw column either.
    assert_eq!(decoded.read_with(cx, |p, cx| p.selection_preview(cx)), None);
    assert!(!previewed(cx));

    let ix = frames.iter().position(is_log).unwrap();
    decoded.update(cx, |p, cx| p.select_row(ix, cx));
    cx.run_until_parked();
    let snapshot = view.read_with(cx, |v, _| v.snapshot().clone());
    let bytes = raw_bytes(&snapshot, &frames[ix]);
    let hex = decoded
        .read_with(cx, |p, cx| p.selection_preview(cx))
        .expect("the selected frame's bytes");
    assert!(
        hex.starts_with(&serialist_core::codec::encode_hex(&bytes[..8], " ")),
        "{hex}"
    );
    assert!(previewed(cx));

    // A filter rebuilds the rows and drops the selection, and the preview with it.
    decoded.update(cx, |p, cx| p.set_query("heartbeat", cx));
    cx.run_until_parked();
    assert_eq!(decoded.read_with(cx, |p, cx| p.selection_preview(cx)), None);
    assert!(!previewed(cx));
}

#[gpui_test]
fn decoded_frames_export_to_csv_and_json_as_the_snapshot_has_them(cx: &mut TestAppContext) {
    let dir = config_dir("decoded-export", &race_profile("airoha-race"));
    let Opened {
        workspace,
        view,
        _world,
        ..
    } = open(cx, &dir, false);
    wait_frames(cx, &workspace, "a banner and three logs", |frames| {
        frames.iter().filter(|f| is_log(f)).count() >= 3
    });

    let csv_path = dir.join("frames.csv");
    let job = view.read_with(cx, |v, cx| v.export_job(ExportFormat::Csv, cx));
    let ExportJob::Frames { frames, raw, .. } = &job else {
        panic!("a frames job");
    };
    let (frames, raw) = (frames.clone(), raw.clone());
    let count = frames.count();
    assert_eq!(
        job.run(&csv_path),
        Ok(format!("Exported {count} frames to frames.csv"))
    );
    let rows = parse_csv(&fs::read_to_string(&csv_path).unwrap());
    assert_eq!(rows.len(), count + 1, "a header and a row per frame");
    let column = |name: &str| rows[0].iter().position(|h| h == name).unwrap();
    for (row, (_, frame)) in rows[1..].iter().zip(frames.frames()) {
        assert_eq!(row[column("kind")], frame.kind.as_str());
        assert_eq!(row[column("summary")], frame.summary);
        assert_eq!(row[column("direction")], "RX");
    }
    let (_, log) = frames.frames().find(|(_, f)| is_log(f)).unwrap();
    let log_row = &rows[1 + frames.frames().position(|(_, f)| is_log(f)).unwrap()];
    assert_eq!(log_row[column("cmd_id")], "3904");
    assert_eq!(
        log_row[column("raw")],
        serialist_core::codec::encode_hex(&raw_bytes(&raw, log), " ")
    );

    let json_path = dir.join("frames.json");
    view.update(cx, |v, cx| {
        v.export_to(json_path.clone(), ExportFormat::Json, cx)
    })
    .detach();
    run_until(cx, "the JSON export", |cx| {
        notice(cx, &view).is_some_and(|text| text.ends_with("frames to frames.json"))
    });
    let parsed: JsonValue = serde_json::from_str(&fs::read_to_string(&json_path).unwrap()).unwrap();
    let array = parsed.as_array().unwrap();
    assert!(
        array.len() >= count,
        "at least what the earlier snapshot had"
    );
    let first_log = array.iter().find(|frame| frame["kind"] == "log").unwrap();
    assert_eq!(first_log["fields"]["cmd_id"], 0x0F40);
    let id = first_log["id"].as_u64().unwrap();
    let snapshot = view.read_with(cx, |v, _| v.snapshot().clone());
    let frame = view
        .read_with(cx, |v, _| v.frame_reader().snapshot())
        .get(serialist_core::FrameId(id))
        .cloned()
        .unwrap();
    assert_eq!(
        first_log["raw"],
        serialist_core::codec::encode_hex(&raw_bytes(&snapshot, &frame), " ")
    );
    assert_eq!(first_log["raw_start"], frame.raw.start);
}

#[gpui_test]
fn a_device_profile_selects_the_codec_on_connect_and_the_devices_panel_names_it(
    cx: &mut TestAppContext,
) {
    let dir = config_dir("decoded-profile", &race_profile("airoha-race"));
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir, false);
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        Some("airoha-race".into())
    );
    wait_frames(cx, &workspace, "the banner frame", |frames| {
        frames.iter().any(|frame| frame.kind == "text")
    });
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.codec.as_deref(), Some("Codec: airoha-race"));

    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let (plugin, name, ix) = devices.read_with(cx, |devices, cx| {
        let list = devices.list();
        let ix = list
            .entries()
            .iter()
            .position(|entry| entry.info.id == PortId::new("virtual:race"))
            .expect("listed");
        let info = list.entries()[ix].info.clone();
        (
            devices.plugin_for(&info, cx),
            devices.display_name(&info, cx),
            ix,
        )
    });
    assert_eq!(plugin.as_deref(), Some("airoha-race"));
    assert_eq!(name, "RACE board");
    // The badge is drawn on the port's row.
    let badge = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.try_find(("device-plugin", ix)).is_some()
        })
        .unwrap();
    assert!(badge, "the plugin badge");
    // Other ports have no profile and no badge.
    devices.read_with(cx, |devices, cx| {
        let echo = devices
            .list()
            .entries()
            .iter()
            .find(|entry| entry.info.id == PortId::new("virtual:echo"))
            .unwrap()
            .info
            .clone();
        assert_eq!(devices.plugin_for(&echo, cx), None);
    });
    // Picking none turns decoding off; the frames so far stay.
    let kept = view.read_with(cx, |v, _| v.frames().count());
    view.update(cx, |v, cx| v.set_codec(None, cx));
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        None
    );
    assert!(view.read_with(cx, |v, _| v.frames().count()) >= kept);
}
