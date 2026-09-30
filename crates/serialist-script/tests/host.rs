//! The script host without a device: output, errors, limits, stop, the UI and command
//! services, and the sandbox.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serialist_core::ParamValues;
use serialist_script::{
    HostServices, Limits, LogLevel, ScriptEvent, ScriptHost, ScriptOutcome, ScriptSource,
};

use common::{Answer, TempDir, TestCommands, TestUi, collect, run, services, wait_started};

fn plain_host() -> ScriptHost {
    ScriptHost::new(services(None, TestUi::new()))
}

#[test]
fn host_and_run_handles_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ScriptHost>();
    assert_send_sync::<serialist_script::ScriptRun>();
    assert_send_sync::<HostServices>();
}

#[test]
fn print_and_log_arrive_as_output_in_order() {
    let ui = TestUi::new();
    let host = ScriptHost::new(services(None, Arc::clone(&ui)));
    let finished = run(
        &host,
        "out.lua",
        r#"
        print("one", 2, 3.5, nil, true)
        log.info("two", 2)
        log.warn("three")
        print("four")
        log.debug({} == nil)
        log.error("five", "\xff")
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "one\t2\t3.5\tnil\ttrue",
            "[info] two 2",
            "[warn] three",
            "four",
            "[debug] false",
            "[error] five \u{FFFD}",
        ]
    );
    assert_eq!(finished.events.first(), Some(&ScriptEvent::Started));
    assert_eq!(
        finished.events.last(),
        Some(&ScriptEvent::Finished(ScriptOutcome::Ok))
    );
    assert_eq!(
        *ui.logs.lock(),
        [
            (LogLevel::Info, "two 2".to_owned()),
            (LogLevel::Warn, "three".to_owned()),
            (LogLevel::Debug, "false".to_owned()),
            (LogLevel::Error, "five \u{FFFD}".to_owned()),
        ]
    );
}

#[test]
fn errors_carry_a_traceback_with_the_script_name_and_line() {
    let host = plain_host();
    let finished = run(
        &host,
        "probe.lua",
        "local function inner()\n  error(\"boom\")\nend\ninner()\n",
    );
    let message = finished.error();
    assert!(message.contains("probe.lua:2: boom"), "{message}");
    assert!(message.contains("stack traceback"), "{message}");
    assert!(message.contains("probe.lua:4"), "{message}");

    // An error raised by the API (in Rust) still points at the script's line.
    let finished = run(&host, "broken.lua", "local x = 1\nhex.decode(\"zz\")\n");
    let message = finished.error();
    assert!(message.contains("hex.decode"), "{message}");
    assert!(message.contains("broken.lua:2"), "{message}");

    let finished = run(&host, "syntax.lua", "local x = \n");
    let message = finished.error();
    assert!(message.contains("syntax.lua:"), "{message}");
}

#[test]
fn the_memory_cap_is_an_error_not_an_abort() {
    let host = plain_host();
    let finished = run(
        &host,
        "hog.lua",
        r#"local t = {} for i = 1, 1e9 do t[i] = ("x"):rep(1000) end"#,
    );
    let message = finished.error();
    assert!(message.contains("memory"), "{message}");
    // The host is fine afterwards, with a fresh VM.
    run(
        &host,
        "after.lua",
        "local t = {} for i = 1, 1000 do t[i] = i end",
    )
    .assert_ok();

    let mut services = services(None, TestUi::new());
    services.limits = Limits {
        memory_bytes: 4 * 1024 * 1024,
        ..Limits::default()
    };
    let small = ScriptHost::new(services);
    let finished = run(
        &small,
        "small.lua",
        r#"local s = ("x"):rep(8 * 1024 * 1024)"#,
    );
    assert!(finished.error().contains("memory"), "{}", finished.error());
}

#[test]
fn stop_ends_a_busy_loop_within_a_second() {
    let host = plain_host();
    for code in [
        "while true do end",
        // A stop cannot be swallowed by pcall or a coroutine.
        "while true do pcall(function() while true do end end) end",
        "while true do coroutine.resume(coroutine.create(function() while true do end end)) end",
        // Code that cannot yield (a comparator) is stopped by the error path.
        "local t = {3, 2, 1} table.sort(t, function(a, b) while true do end end)",
    ] {
        let run = host.run(ScriptSource::new("spin.lua", code));
        wait_started(&run);
        std::thread::sleep(Duration::from_millis(100));
        let asked = Instant::now();
        run.stop();
        let outcome = run.wait_timeout(Duration::from_secs(1));
        assert_eq!(outcome, Some(ScriptOutcome::Stopped), "{code}");
        assert!(asked.elapsed() < Duration::from_secs(1), "{code}");
    }
}

#[test]
fn stop_ends_a_wait_at_once() {
    let ui = TestUi::answering([Answer::Never]);
    let host = ScriptHost::new(services(None, ui));
    for code in ["sleep(60000)", "ui.prompt('never answered')"] {
        let run = host.run(ScriptSource::new("wait.lua", code));
        wait_started(&run);
        std::thread::sleep(Duration::from_millis(50));
        let asked = Instant::now();
        run.stop();
        assert_eq!(
            run.wait_timeout(Duration::from_secs(1)),
            Some(ScriptOutcome::Stopped),
            "{code}"
        );
        assert!(asked.elapsed() < Duration::from_millis(500), "{code}");
    }
}

#[test]
fn runs_queue_in_order_and_a_queued_run_can_be_stopped() {
    let host = plain_host();
    let first = host.run(ScriptSource::new("a.lua", "sleep(200) print('a')"));
    let second = host.run(ScriptSource::new("b.lua", "print('b')"));
    let third = host.run(ScriptSource::new("c.lua", "print('c')"));
    third.stop();
    assert_ne!(first.id(), second.id());
    let a = collect(&first, Duration::from_secs(10));
    let b = collect(&second, Duration::from_secs(10));
    let c = collect(&third, Duration::from_secs(10));
    assert_eq!(a.output, ["a"]);
    assert_eq!(b.output, ["b"]);
    assert_eq!(c.events, [ScriptEvent::Finished(ScriptOutcome::Stopped)]);
    assert_eq!(first.outcome(), Some(ScriptOutcome::Ok));
}

#[test]
fn dropping_the_host_stops_its_script() {
    let host = plain_host();
    let run = host.run(ScriptSource::new("spin.lua", "while true do end"));
    let queued = host.run(ScriptSource::new("never.lua", "print('never')"));
    wait_started(&run);
    let started = Instant::now();
    drop(host);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(run.outcome(), Some(ScriptOutcome::Stopped));
    assert_eq!(queued.outcome(), Some(ScriptOutcome::Stopped));
}

#[test]
fn ui_prompt_and_notify_round_trip() {
    let ui = TestUi::answering([Answer::Now(Some("42".into())), Answer::Now(None)]);
    let host = ScriptHost::new(services(None, Arc::clone(&ui)));
    let finished = run(
        &host,
        "ask.lua",
        r#"
        local a = ui.prompt("Device id", "7")
        local b = ui.prompt("Skip?")
        print(a, b)
        ui.notify("done")
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["42\tnil"]);
    let prompts: Vec<_> = finished
        .events
        .iter()
        .filter(|event| matches!(event, ScriptEvent::Prompt { .. }))
        .cloned()
        .collect();
    assert_eq!(
        prompts,
        [
            ScriptEvent::Prompt {
                id: 1,
                label: "Device id".into(),
                default: Some("7".into())
            },
            ScriptEvent::Prompt {
                id: 2,
                label: "Skip?".into(),
                default: None
            },
        ]
    );
    assert_eq!(
        *ui.prompts.lock(),
        [
            ("Device id".to_owned(), Some("7".to_owned())),
            ("Skip?".to_owned(), None)
        ]
    );
    assert_eq!(*ui.notes.lock(), ["done"]);
}

#[test]
fn commands_send_goes_through_the_sender() {
    let commands = Arc::new(TestCommands::default());
    let mut services = services(None, TestUi::new());
    services.commands = Some(Arc::clone(&commands) as Arc<dyn serialist_script::CommandSender>);
    let host = ScriptHost::new(services);
    let finished = run(
        &host,
        "cmd.lua",
        r#"
        assert(commands.send("Version", { id = 0x0F15, label = "boot", ratio = 1.5 }))
        assert(commands.send("Reset") == true)
        print(commands.send("missing"))
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["nil\tno command named missing"]);
    assert_eq!(
        *commands.sent.lock(),
        [
            (
                "Version".to_owned(),
                ParamValues::new()
                    .with("id", "3861")
                    .with("label", "boot")
                    .with("ratio", "1.5")
            ),
            ("Reset".to_owned(), ParamValues::new()),
        ]
    );

    // Without a sender the call says so.
    let finished = run(&plain_host(), "none.lua", r#"commands.send("Version")"#);
    assert!(
        finished.error().contains("not available"),
        "{}",
        finished.error()
    );
}

#[test]
fn the_sandbox_has_no_os_io_or_escape_hatches() {
    let dir = TempDir::new();
    dir.write("lib/util.lua", "return { answer = 42 }");
    dir.write("init_twice.lua", "loads = (loads or 0) + 1 return loads");
    dir.write("tools/init.lua", "return 'tools'");
    dir.write("waits.lua", "sleep(1) return 'waited'");
    dir.write("values.lua", "return 1, 2");
    let mut services = services(None, TestUi::new());
    services.scripts_dir = Some(dir.0.clone());
    let host = ScriptHost::new(services);
    let finished = run(
        &host,
        "sandbox.lua",
        r#"
        assert(os.execute == nil and os.getenv == nil and os.remove == nil and os.exit == nil)
        assert(io == nil, "io")
        assert(type(os.time()) == "number" and type(os.clock()) == "number")
        assert(type(os.date("%Y")) == "string")
        assert(debug == nil and package == nil and loadfile == nil and string.dump == nil)
        assert(string.pack and string.unpack and utf8 and math and table and coroutine)

        local ok, err = pcall(require, "../x")
        assert(not ok) print(err)
        ok, err = pcall(dofile, "../x.lua")
        assert(not ok) print(err)
        ok, err = pcall(dofile, "/etc/passwd")
        assert(not ok)
        ok, err = pcall(require, "not.there")
        assert(not ok) print(err)

        assert(require("lib.util").answer == 42)
        assert(require("lib.util") == require("lib.util"))
        assert(require("init_twice") == 1 and require("init_twice") == 1)
        assert(require("tools") == "tools")
        assert(require("waits") == "waited")
        local a, b = dofile("values.lua")
        assert(a == 1 and b == 2)

        local f, why = load(string.char(27) .. "Lua", "bin")
        assert(f == nil) print(why)
        assert(load("return 1 + 1")() == 2)
        local env = { x = 5 }
        assert(load("return x", "env", "b", env)() == 5)
        "#,
    );
    finished.assert_ok();
    assert!(
        finished.output[0].contains("not a module name"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[1].contains("relative path"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[2].contains("not found"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[3].contains("binary"),
        "{:?}",
        finished.output
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_scripts_directory_is_refused() {
    let outside = TempDir::new();
    outside.write("secret.lua", "return 'secret'");
    let dir = TempDir::new();
    std::os::unix::fs::symlink(outside.0.join("secret.lua"), dir.0.join("leak.lua")).unwrap();
    let mut services = services(None, TestUi::new());
    services.scripts_dir = Some(dir.0.clone());
    let host = ScriptHost::new(services);
    let finished = run(&host, "leak.lua", "print(pcall(require, 'leak'))");
    finished.assert_ok();
    assert!(
        finished.output[0].starts_with("false") && finished.output[0].contains("outside"),
        "{:?}",
        finished.output
    );
}

#[test]
fn require_without_a_scripts_directory_says_so() {
    let finished = run(&plain_host(), "req.lua", "require('lib.util')");
    assert!(
        finished.error().contains("no scripts directory"),
        "{}",
        finished.error()
    );
}

#[test]
fn hex_and_bytes_helpers() {
    let finished = run(
        &plain_host(),
        "hex.lua",
        r#"
        assert(hex.encode("\5\90\0") == "05 5A 00")
        assert(hex.encode({5, 90}, "") == "055A")
        assert(hex.decode("05 5a") == "\5\90")
        assert(hex.decode("0x05,0x5A") == "\5\90")
        local t = bytes.to_table("\1\2\255")
        assert(#t == 3 and t[1] == 1 and t[3] == 255)
        assert(bytes.from_table({65, 66}) == "AB")
        assert(not pcall(bytes.from_table, {256}))
        assert(not pcall(hex.decode, "5"))
        assert(string.unpack("<I2", "\21\15") == 0x0F15)
        "#,
    );
    finished.assert_ok();
}

#[test]
fn waiting_inside_a_coroutine_the_script_made_fails_clearly() {
    let finished = run(
        &plain_host(),
        "co.lua",
        r#"
        local co = coroutine.wrap(function() sleep(1) end)
        print(pcall(co))
        local c2 = coroutine.create(function() return ui.prompt("x") end)
        print(coroutine.resume(c2))
        -- Plain coroutines still work.
        local gen = coroutine.wrap(function() for i = 1, 3 do coroutine.yield(i) end end)
        print(gen(), gen(), gen())
        "#,
    );
    finished.assert_ok();
    assert!(
        finished.output[0].starts_with("false"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[0].contains("cannot wait inside a coroutine"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[1].starts_with("false"),
        "{:?}",
        finished.output
    );
    assert_eq!(finished.output[2], "1\t2\t3");
}

#[test]
fn a_script_without_a_session_is_told_so() {
    let finished = run(&plain_host(), "none.lua", "print(serial.current())");
    finished.assert_ok();
    assert_eq!(
        finished.output,
        ["nil\tno session is attached to this script"]
    );
    let finished = run(
        &plain_host(),
        "open.lua",
        "print(serial.open{ port = 'x' })",
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["nil\tserial.open is not supported here"]);
}
