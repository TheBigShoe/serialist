//! `timer.after` and `timer.every`: callbacks beside the script's own waits.

mod common;

use std::time::Duration;

use serialist_script::{Limits, ScriptHost, ScriptOutcome, ScriptSource};

use common::{FAST_ECHO, TestUi, collect, host_on, open, run, services, wait_started, world};

fn host() -> ScriptHost {
    ScriptHost::new(services(None, TestUi::new()))
}

#[test]
fn after_and_every_run_while_the_script_sleeps() {
    let finished = run(
        &host(),
        "timers.lua",
        r#"
        local fired, ticks = false, 0
        local once = timer.after(50, function() fired = true end)
        local tick = timer.every(20, function() ticks = ticks + 1 end)
        assert(once:active() and tick:active())
        sleep(300)
        assert(fired, "after did not fire")
        assert(not once:active(), "a fired after is no longer active")
        assert(tick:active())
        tick:cancel()
        assert(not tick:active())
        local stopped_at = ticks
        sleep(100)
        print(ticks >= 5, ticks <= 16, ticks == stopped_at)
        -- `after(0)` runs at the next wait.
        local now = false
        timer.after(0, function() now = true end)
        assert(not now)
        sleep(0)
        sleep(10)
        print(now)
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["true\ttrue\ttrue", "true"]);
}

#[test]
fn a_timer_does_not_block_the_script_s_other_waits() {
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = host_on(&session);
    // A timer writes through the echo device while the body waits for the echoes.
    let finished = run(
        &host,
        "keepalive.lua",
        r#"
        local port = serial.current()
        local n = 0
        local t <close> = timer.every(20, function()
          n = n + 1
          port:write("tick " .. n .. "\n")
        end)
        for i = 1, 3 do
          local m = assert(port:expect("^tick (\\d+)$", { timeout_ms = 2000 }))
          print(m[2])
        end
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["1", "2", "3"]);
}

#[test]
fn callbacks_may_wait_and_never_overlap() {
    let finished = run(
        &host(),
        "slow.lua",
        r#"
        local running, overlapped, calls = false, false, 0
        local t = timer.every(5, function()
          if running then overlapped = true end
          running = true
          calls = calls + 1
          sleep(30)
          running = false
        end)
        sleep(300)
        t:cancel()
        -- Each call takes 30 ms, so a 5 ms timer runs at most once per 30 ms or so:
        -- the ticks that fall during a call are skipped, not queued.
        print(overlapped, calls >= 3, calls <= 11)
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["false\ttrue\ttrue"]);
}

#[test]
fn the_end_of_the_run_cancels_its_timers() {
    let finished = run(
        &host(),
        "end.lua",
        r#"
        timer.after(50, function() print("late") end)
        timer.every(10, function() print("tick") end)
        "#,
    );
    finished.assert_ok();
    assert!(finished.output.is_empty(), "{:?}", finished.output);
    // A scope with `<close>` cancels too.
    let finished = run(
        &host(),
        "close.lua",
        r#"
        do
          local t <close> = timer.after(20, function() print("late") end)
        end
        sleep(60)
        "#,
    );
    finished.assert_ok();
    assert!(finished.output.is_empty(), "{:?}", finished.output);
}

#[test]
fn an_error_in_a_callback_ends_the_run() {
    let finished = run(
        &host(),
        "fails.lua",
        r#"
        timer.after(10, function() error("timer callback failed") end)
        sleep(5000)
        print("not reached")
        "#,
    );
    assert!(
        finished.error().contains("timer callback failed"),
        "{}",
        finished.error()
    );
    assert!(finished.output.is_empty());
}

#[test]
fn bad_arguments_are_errors() {
    for (code, expected) in [
        (
            "timer.every(0, function() end)",
            "timer.every: the interval must be at least 1 ms",
        ),
        (
            "timer.every(0.5, function() end)",
            "timer.every: the interval must be at least 1 ms",
        ),
        (
            "timer.after(-1, function() end)",
            "timer.after: a time in milliseconds must be 0 or more",
        ),
        ("timer.after(10)", "bad argument"),
        ("timer.after('soon', function() end)", "bad argument"),
    ] {
        let finished = run(&host(), "bad.lua", code);
        assert!(
            finished.error().contains(expected),
            "{code}: {}",
            finished.error()
        );
    }
}

#[test]
fn a_timer_flood_hits_the_limit_and_cancelling_frees_room() {
    let finished = run(
        &host(),
        "flood.lua",
        r#"
        local handles = {}
        local ok, err = pcall(function()
          for i = 1, 100000 do handles[i] = timer.every(1000, function() end) end
        end)
        print(#handles, tostring(err):match("timer%.every: [^\n]*"))
        handles[1]:cancel()
        handles[1] = timer.after(1000, function() end)
        -- Fired timers give their slot back too.
        for i = 1, 10 do handles[i]:cancel() end
        for i = 1, 10 do handles[i] = timer.after(0, function() end) end
        sleep(10)
        for i = 1, 10 do handles[i] = timer.every(1000, function() end) end
        print("room again")
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "1024\ttimer.every: this run already has 1024 timers and frame handlers active \
             (the most a run may have); cancel one first",
            "room again"
        ]
    );

    let mut services = services(None, TestUi::new());
    services.limits = Limits {
        max_callbacks: 2,
        ..Limits::default()
    };
    let small = ScriptHost::new(services);
    let finished = run(
        &small,
        "two.lua",
        "timer.after(1000, print) timer.after(1000, print) timer.after(1000, print)",
    );
    assert!(
        finished.error().contains("already has 2 timers"),
        "{}",
        finished.error()
    );
}

#[test]
fn stopping_a_run_with_timers_stops_it() {
    let host = host();
    let run = host.run(ScriptSource::new(
        "forever.lua",
        "timer.every(1, function() end) sleep(math.huge)",
    ));
    wait_started(&run);
    std::thread::sleep(Duration::from_millis(50));
    run.stop();
    let finished = collect(&run, Duration::from_secs(10));
    assert_eq!(finished.outcome, ScriptOutcome::Stopped);
}
