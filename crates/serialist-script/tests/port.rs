//! Scripts driving real `Session`s and `Ingest` threads against simulated devices.

mod common;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialist_core::{ControlLine, PortId, SerialConfig};
use serialist_script::{
    HeadlessOpener, ScriptHost, ScriptOutcome, ScriptSession, ScriptSource, StdioUi, run_headless,
};
use serialist_sim::SimWorld;

use common::{FAST_ECHO, SharedBuf, TestUi, collect, host_on, open, run, services, world};

/// The plan's `version_probe.lua`, adapted to what the AT modem answers: its version
/// query stands in for `AT+READ=<i>`.
const VERSION_PROBE: &str = r#"
local port = serial.current()               -- or serial.open{ port = "virtual:at" }
port:write("AT\r\n")
local ok = port:expect("^OK$", { timeout_ms = 1000 })
assert(ok, "no OK from the modem")
log.info("matched", ok[1])
for i = 1, 3 do
  port:write("AT+VER?\r\n")
  local m = port:expect([[^\+VER: (\S+)]], { timeout_ms = 500 })
  log.info("value", i, m and m[2] or "timeout")
  sleep(100)
end
"#;

#[test]
fn version_probe_against_the_at_modem() {
    let world = world();
    let session = open(&world, "virtual:at");
    let host = host_on(&session);
    let finished = run(&host, "version_probe.lua", VERSION_PROBE);
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "[info] matched OK",
            "[info] value 1 1.0.0",
            "[info] value 2 1.0.0",
            "[info] value 3 1.0.0",
        ]
    );
}

#[test]
fn an_expect_that_never_matches_times_out_with_nil_and_elapsed() {
    let world = world();
    let session = open(&world, "virtual:at");
    let host = host_on(&session);
    let started = Instant::now();
    let finished = run(
        &host,
        "never.lua",
        r#"
        local port = serial.current()
        port:write("AT\r\n")
        local m, elapsed, why = port:expect("^NEVER", { timeout_ms = 300 })
        assert(m == nil)
        print(math.type(elapsed), elapsed, why)
        -- The OK it skipped over was not consumed.
        assert(port:expect("^OK$", { timeout_ms = 0 }))
        "#,
    );
    finished.assert_ok();
    let fields: Vec<&str> = finished.output[0].split('\t').collect();
    assert_eq!(fields[0], "integer");
    let elapsed: u64 = fields[1].parse().unwrap();
    assert!((300..3000).contains(&elapsed), "elapsed {elapsed} ms");
    assert_eq!(fields[2], "timeout");
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[test]
fn expect_captures_and_reads_the_stream_in_order() {
    let world = world();
    let session = open(&world, "virtual:at");
    let host = host_on(&session);
    let finished = run(
        &host,
        "stream.lua",
        r#"
        local port = serial.current()
        -- ATI answers "\r\n<identity>\r\n\r\nOK\r\n". Once all of it is stored, both
        -- expects still find their line: the stream keeps what was not read yet.
        port:write("ATI\r\n")
        sleep(200)
        local m, elapsed = port:expect("(Virtual) (?P<what>\\w+)", { timeout_ms = 0 })
        print(m[1], m[2], m[3], m.what, m.line, math.type(elapsed))
        assert(port:expect("^OK$", { timeout_ms = 0 }))
        -- A group that does not take part is false, keeping positions.
        port:write("AT\r\n")
        m = port:expect("^(X)?(OK)$")
        print(m[1], m[2], m[3])
        -- Smart case: lower-case patterns ignore case.
        port:write("AT+VER?\r\n")
        assert(port:expect("^\\+ver: 1"))
        sleep(200)
        port:discard()
        print(port:read_line({ timeout_ms = 0 }))
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "Virtual Modem\tVirtual\tModem\tModem\tSerialist Virtual Modem\tinteger",
            "OK\tfalse\tOK",
            "nil\ttimeout",
        ]
    );
}

#[test]
fn read_line_and_read_with_timeouts() {
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = host_on(&session);
    let finished = run(
        &host,
        "reads.lua",
        r#"
        local port = serial.current()
        port:write("first\r\nsecond\r\n")
        print(port:read_line())
        print(port:read_line())
        print(port:read_line(100))
        port:write("abcdef")
        print(port:read(4))
        -- Only two bytes are left: they come back when the timeout ends.
        print(port:read(4, { timeout_ms = 100 }))
        print(port:read(1, { timeout_ms = 50 }))
        port:write({0x41, 0x42, 10})
        port:write_hex("43 44 0A")
        print(port:read_line(), port:read_line())
        assert(not pcall(port.read, port, 0))
        assert(not pcall(port.read_line, port, { timeout_ms = -1 }))
        assert(not pcall(port.read_line, port, "soon"))
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "first",
            "second",
            "nil\ttimeout",
            "abcd",
            "ef",
            "nil\ttimeout",
            "AB\tCD",
        ]
    );
}

#[test]
fn on_line_runs_while_the_script_sleeps_and_stops_after_cancel() {
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = host_on(&session);
    let finished = run(
        &host,
        "watch.lua",
        r#"
        local port = serial.current()
        local seen = {}
        local during_sleep = 0
        local sleeping = false
        local handle = port:on_line(function(line)
          seen[#seen + 1] = line
          if sleeping then during_sleep = during_sleep + 1 end
        end)
        for i = 1, 5 do port:write("line " .. i .. "\n") end
        -- The callbacks run while the main body sleeps: sleep never blocks the thread.
        sleeping = true
        for _ = 1, 200 do
          if #seen == 5 then break end
          sleep(10)
        end
        sleeping = false
        handle:cancel()
        port:write("late 1\nlate 2\n")
        sleep(300)
        print(#seen, table.concat(seen, ","))
        print(during_sleep, handle:active())
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        ["5\tline 1,line 2,line 3,line 4,line 5", "5\tfalse"]
    );
}

#[test]
fn on_line_callbacks_may_wait_and_their_errors_end_the_run() {
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = host_on(&session);
    let finished = run(
        &host,
        "responder.lua",
        r#"
        local port = serial.current()
        -- An auto-responder: waits inside the callback, then answers.
        do
          local h <close> = port:on_line(function(line)
            if line == "ping" then
              sleep(10)
              port:write("pong\n")
            end
          end)
          port:write("ping\n")
          assert(port:expect("^pong$", { timeout_ms = 2000 }))
        end
        print("closed")
        port:on_line(function(line) error("callback failed on " .. line) end)
        port:write("boom\n")
        sleep(5000)
        print("not reached")
        "#,
    );
    let message = finished.error();
    assert!(message.contains("callback failed on boom"), "{message}");
    assert_eq!(finished.output, ["closed"]);
}

#[test]
fn a_disconnect_ends_waits_with_closed() {
    let world = world();
    let session = open(&world, "virtual:at");
    let host = host_on(&session);
    let run = host.run(ScriptSource::new(
        "unplug.lua",
        r#"
        local port = serial.current()
        print(port:read_line({ timeout_ms = 10000 }))
        print(port:expect("x", { timeout_ms = 10000 }))
        print(pcall(port.write, port, "AT\r\n"))
        "#,
    ));
    common::wait_started(&run);
    std::thread::sleep(Duration::from_millis(100));
    world.unplug(&PortId::new("virtual:at"));
    let finished = collect(&run, Duration::from_secs(5));
    finished.assert_ok();
    assert_eq!(finished.output[0], "nil\tclosed");
    assert!(
        finished.output[1].starts_with("nil\t"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[1].ends_with("\tclosed"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[2].starts_with("false"),
        "{:?}",
        finished.output
    );
}

#[test]
fn set_drives_control_lines_and_baud() {
    let world = world();
    let id = PortId::new("virtual:echo");
    let session = open(&world, "virtual:echo");
    let host = host_on(&session);
    let finished = run(
        &host,
        "lines.lua",
        r#"
        local port = serial.current()
        port:set{ dtr = false, rts = true, baud = 9600 }
        print(port:id(), port:description(), tostring(port))
        assert(not pcall(port.set, port, { parity = "even" }))
        assert(not pcall(port.set, port, { baud = "fast" }))
        "#,
    );
    finished.assert_ok();
    assert!(
        finished.output[0].starts_with("virtual:echo\t"),
        "{:?}",
        finished.output
    );
    assert!(finished.output[0].ends_with("\tserial port virtual:echo"));
    let link = world.link(&id).expect("the link is up");
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.serial_config().baud != 9600 || link.control_line(ControlLine::Dtr) {
        assert!(Instant::now() < deadline, "the settings never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(link.control_line(ControlLine::Rts));
}

#[test]
fn set_takes_12_mbaud_to_the_link() {
    let world = world();
    let id = PortId::new("virtual:echo");
    let session = open(&world, "virtual:echo");
    let host = host_on(&session);
    let finished = run(
        &host,
        "fast.lua",
        r#"
        serial.current():set{ baud = 12000000 }
        "#,
    );
    finished.assert_ok();
    let link = world.link(&id).expect("the link is up");
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.serial_config().baud != 12_000_000 {
        assert!(Instant::now() < deadline, "the rate never arrived");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(link.link_config().serial.baud, 12_000_000);
}

#[test]
fn serial_open_and_ports_through_the_headless_opener() {
    let world = world();
    let mut services = services(None, TestUi::new());
    services.ports = Some(world.port_source());
    services.opener = Some(Arc::new(
        HeadlessOpener::new(world.transport_factory()).with_ports(world.port_source()),
    ));
    let host = ScriptHost::new(services);
    let finished = run(
        &host,
        "open.lua",
        r#"
        local ids = {}
        for _, p in ipairs(serial.ports()) do ids[#ids + 1] = p.id .. "=" .. p.kind end
        print(table.concat(ids, ","))
        local port = assert(serial.open{ port = "virtual:at", baud = 9600 })
        port:write("ATI\r\n")
        local m = assert(port:expect("Modem"))
        print(m[1])
        port:close()
        print(pcall(port.write, port, "x"))
        print(serial.open{ match = { product = "nothing" } })
        print(serial.open{ port = "virtual:nope" })
        -- Not closed by the script: closed when it ends.
        assert(serial.open{ port = "virtual:echo" })
        "#,
    );
    finished.assert_ok();
    assert!(
        finished.output[0].contains("virtual:at=virtual")
            && finished.output[0].contains("virtual:echo-fast=virtual"),
        "{:?}",
        finished.output
    );
    assert_eq!(finished.output[1], "Modem");
    assert!(
        finished.output[2].starts_with("false") && finished.output[2].contains("closed"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[3].starts_with("nil\tno port matches"),
        "{:?}",
        finished.output
    );
    assert!(
        finished.output[4].starts_with("nil\tcould not open virtual:nope"),
        "{:?}",
        finished.output
    );
    // The script's own port was closed with it, so the device can be opened again.
    let deadline = Instant::now() + Duration::from_secs(5);
    while world.link(&PortId::new("virtual:echo")).is_some() {
        assert!(Instant::now() < deadline, "the opened port stayed open");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn run_headless_end_to_end_against_virtual_at() {
    let world = SimWorld::new();
    let out = SharedBuf::default();
    let err = SharedBuf::default();
    let ui = Arc::new(StdioUi::with_io(
        Box::new(io::Cursor::new(b"Ada\n\n".to_vec())),
        Box::new(out.clone()),
        Box::new(err.clone()),
    ));
    let script = format!(
        "{VERSION_PROBE}\nprint(ui.prompt('Name', 'x'))\nprint(ui.prompt('Again', 'fallback'))\nui.notify('bye')\n"
    );
    let outcome = run_headless(
        PortId::new("virtual:at"),
        SerialConfig::default(),
        world.transport_factory(),
        ScriptSource::new("version_probe.lua", script),
        ui,
    );
    assert_eq!(outcome, ScriptOutcome::Ok, "stdout: {}", out.text());
    assert_eq!(
        out.text(),
        "[info] matched OK\n[info] value 1 1.0.0\n[info] value 2 1.0.0\n\
         [info] value 3 1.0.0\nAda\nfallback\n"
    );
    assert_eq!(err.text(), "Name [x]: Again [fallback]: notice: bye\n");

    let outcome = run_headless(
        PortId::new("virtual:missing"),
        SerialConfig::default(),
        world.transport_factory(),
        ScriptSource::new("x.lua", ""),
        Arc::new(StdioUi::with_io(
            Box::new(io::empty()),
            Box::new(io::sink()),
            Box::new(io::sink()),
        )),
    );
    assert!(
        matches!(&outcome, ScriptOutcome::Error(message) if message.contains("could not open")),
        "{outcome:?}"
    );
}

/// Documents the per-call cost of the async bridge: 10 000 write/expect round trips
/// through the real session, ingest thread, store and bell to an echo device.
#[test]
fn ten_thousand_round_trips_stay_well_under_ten_seconds() {
    const ROUND_TRIPS: u32 = 10_000;
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = host_on(&session);
    let code = format!(
        r#"
        local port = serial.current()
        for i = 1, {ROUND_TRIPS} do
          port:write("ping " .. i .. "\n")
          if not port:expect("^ping " .. i .. "$", {{ timeout_ms = 2000 }}) then
            error("lost round trip " .. i)
          end
        end
        "#
    );
    let started = Instant::now();
    let finished = collect(
        &host.run(ScriptSource::new("bench.lua", code)),
        Duration::from_secs(60),
    );
    let took = started.elapsed();
    finished.assert_ok();
    eprintln!(
        "{ROUND_TRIPS} write/expect round trips in {took:?}: {:.1} us each",
        took.as_secs_f64() * 1e6 / f64::from(ROUND_TRIPS)
    );
    assert!(took < Duration::from_secs(10), "took {took:?}");
}
