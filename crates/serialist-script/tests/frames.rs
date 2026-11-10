//! `port:on_frame`: callbacks for the frames a session's codec decodes, against the
//! simulated RACE device.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serialist_core::{CodecFactory, CodecRegistry, PortId, SerialConfig};
use serialist_script::{
    CodecSource, HeadlessSession, Limits, ScriptHost, ScriptOutcome, ScriptSession, ScriptSource,
};
use serialist_sim::{RaceDevice, SimWorld};

use common::{FAST_ECHO, TestUi, collect, open, race, run, services, wait_started, world};

/// `virtual:race`, decoded with the test RACE codec.
fn decoding(world: &SimWorld) -> Arc<HeadlessSession> {
    Arc::new(
        HeadlessSession::open_decoding(
            world.factory(),
            PortId::new("virtual:race"),
            SerialConfig::default(),
            Some(race::Factory::new() as Arc<dyn CodecFactory>),
        )
        .expect("the simulated port opens"),
    )
}

/// A host on `session` that can also encode with the test codec.
fn host_with_codec(session: &Arc<HeadlessSession>, limits: Limits) -> ScriptHost {
    let mut services = services(Some(Arc::clone(session)), TestUi::new());
    let mut registry = CodecRegistry::new();
    registry.register(race::Factory::new() as Arc<dyn CodecFactory>);
    services.codecs = Some(Arc::new(registry) as Arc<dyn CodecSource>);
    services.limits = limits;
    ScriptHost::new(services)
}

#[test]
fn on_frame_sees_the_response_to_an_encoded_command() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let finished = run(
        &host,
        "version.lua",
        r#"
        local port = serial.current()
        local response, logs = nil, 0
        local h <close> = port:on_frame({ kind = "response", cmd_id = 0x0F15 }, function(frame)
          response = frame
        end)
        local all <close> = port:on_frame(function(frame)
          if frame.kind == "log" then logs = logs + 1 end
        end)
        port:write(codecs.race.encode{ cmd_id = 0x0F15 })
        for _ = 1, 200 do
          if response and logs > 0 then break end
          sleep(10)
        end
        assert(response, "no response frame")
        print(response.kind, string.format("0x%04X", response.fields.cmd_id), response.severity)
        print(response.summary)
        -- Fields that are bytes come as byte strings; `bytes` is the frame from the
        -- scrollback.
        print(hex.encode(response.fields.payload:sub(1, 1)), response.fields.payload:sub(2, 15))
        print(hex.encode(response.bytes:sub(1, 2)), #response.bytes == 4 + 2 + #response.fields.payload)
        print(logs > 0)
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "response\t0x0F15\tinfo",
            "response 0x0F15",
            &format!("00\t{}", RaceDevice::DEFAULT_VERSION),
            "05 5B\ttrue",
            "true",
        ]
    );
}

#[test]
fn a_filter_compares_the_way_saved_command_predicates_do() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let finished = run(
        &host,
        "filters.lua",
        r#"
        local port = serial.current()
        local seen = {}
        local function count(name)
          seen[name] = 0
          return function() seen[name] = seen[name] + 1 end
        end
        local a <close> = port:on_frame({ cmd_id = "0x0F15" }, count("hex"))
        local b <close> = port:on_frame({ kind = "response", cmd_id = 3861 }, count("number"))
        local c <close> = port:on_frame({ kind = "response", cmd_id = 1 }, count("other id"))
        local d <close> = port:on_frame({ kind = "nope" }, count("other kind"))
        local e <close> = port:on_frame({ missing = 1 }, count("missing field"))
        port:write(codecs.race.encode{ cmd_id = 0x0F15 })
        for _ = 1, 200 do
          if seen.hex > 0 and seen.number > 0 then break end
          sleep(10)
        end
        sleep(50)
        print(seen.hex, seen.number, seen["other id"], seen["other kind"], seen["missing field"])
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["1\t1\t0\t0\t0"]);
}

#[test]
fn cancel_stops_it_and_a_closed_session_ends_it() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let closer = Arc::clone(&session);
    let finished = run(
        &host,
        "cancel.lua",
        r#"
        local port = serial.current()
        local n = 0
        local h = port:on_frame(function() n = n + 1 end)
        -- Logs come every 200 ms.
        for _ = 1, 100 do
          if n > 0 then break end
          sleep(10)
        end
        assert(n > 0, "no log frame")
        h:cancel()
        assert(not h:active())
        local at_cancel = n
        sleep(500)
        print(n == at_cancel)
        local later = port:on_frame(function() end)
        print(later:active())
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["true", "true"]);

    // A handler whose session closes stops being active.
    let run = host.run(ScriptSource::new(
        "closes.lua",
        r#"
        local h = serial.current():on_frame(function() end)
        while h:active() do sleep(10) end
        print("ended")
        "#,
    ));
    wait_started(&run);
    std::thread::sleep(Duration::from_millis(100));
    closer.close_now();
    let finished = collect(&run, Duration::from_secs(10));
    finished.assert_ok();
    assert_eq!(finished.output, ["ended"]);
    // A closed port refuses new handlers.
    let finished = common::run(
        &host,
        "closed.lua",
        "local p = serial.current() p:close() p:on_frame(function() end)",
    );
    assert!(
        finished.error().contains("is closed"),
        "{}",
        finished.error()
    );
}

#[test]
fn a_session_without_a_codec_says_so() {
    let world = world();
    let session = open(&world, FAST_ECHO);
    let host = common::host_on(&session);
    let finished = run(
        &host,
        "none.lua",
        "serial.current():on_frame(function() end)",
    );
    assert!(
        finished
            .error()
            .contains("port:on_frame: virtual:echo-fast decodes no frames here"),
        "{}",
        finished.error()
    );
    // The trait's default is no frames.
    assert!(ScriptSession::frames(session.as_ref()).is_none());
}

#[test]
fn errors_in_callbacks_and_arguments() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let finished = run(
        &host,
        "fails.lua",
        r#"
        serial.current():on_frame(function(frame) error("frame handler failed on " .. frame.kind) end)
        sleep(5000)
        "#,
    );
    assert!(
        finished.error().contains("frame handler failed on log"),
        "{}",
        finished.error()
    );
    for (code, expected) in [
        (
            "p:on_frame()",
            "give a function, or a filter table and a function, got nil and nil",
        ),
        ("p:on_frame({ kind = 'log' })", "got table and nil"),
        ("p:on_frame(function() end, {})", "got function and table"),
        (
            "p:on_frame({ [1] = 2 }, function() end)",
            "port:on_frame: filter: field names must be strings",
        ),
    ] {
        let finished = run(
            &host,
            "bad.lua",
            &format!("local p = serial.current()\n{code}"),
        );
        assert!(
            finished.error().contains(expected),
            "{code}: {}",
            finished.error()
        );
    }
}

#[test]
fn handlers_count_against_the_callback_limit() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(
        &session,
        Limits {
            max_callbacks: 3,
            ..Limits::default()
        },
    );
    let finished = run(
        &host,
        "limit.lua",
        r#"
        local p = serial.current()
        local a = p:on_frame(function() end)
        local b = timer.every(1000, function() end)
        local c = p:on_frame(function() end)
        local ok, err = pcall(p.on_frame, p, function() end)
        print(tostring(err):match("port:on_frame: [^\n]*"))
        a:cancel()
        local d = p:on_frame(function() end)
        print(d:active())
        "#,
    );
    finished.assert_ok();
    assert_eq!(
        finished.output,
        [
            "port:on_frame: this run already has 3 timers and frame handlers active \
             (the most a run may have); cancel one first",
            "true"
        ]
    );
}

#[test]
fn a_slow_handler_takes_frames_in_order_without_a_backlog() {
    // A handler that waits in every call sees frames one at a time, in order; frames
    // that arrive meanwhile wait in the bounded frame store, not in a queue.
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let finished = run(
        &host,
        "slow.lua",
        r#"
        local p = serial.current()
        local busy, overlapped, ids = false, false, {}
        local h <close> = p:on_frame({ kind = "response" }, function(frame)
          if busy then overlapped = true end
          busy = true
          ids[#ids + 1] = frame.fields.cmd_id
          sleep(20)
          busy = false
        end)
        for id = 1, 5 do p:write(codecs.race.encode{ cmd_id = id }) end
        for _ = 1, 300 do
          if #ids == 5 then break end
          sleep(10)
        end
        print(overlapped, table.concat(ids, ","))
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output, ["false\t1,2,3,4,5"]);
}

#[test]
fn stopping_a_run_with_frame_handlers_stops_it() {
    let world = world();
    let session = decoding(&world);
    let host = host_with_codec(&session, Limits::default());
    let run = host.run(ScriptSource::new(
        "forever.lua",
        "serial.current():on_frame(function() sleep(1000) end) sleep(math.huge)",
    ));
    wait_started(&run);
    std::thread::sleep(Duration::from_millis(300));
    run.stop();
    assert_eq!(
        collect(&run, Duration::from_secs(10)).outcome,
        ScriptOutcome::Stopped
    );
}

/// A session whose frames the test writes itself, into a small frame store.
struct FedSession {
    store: serialist_core::Store,
    frames: serialist_core::FrameStoreReader,
    bell: serialist_script::LineBell,
}

impl ScriptSession for FedSession {
    fn port_id(&self) -> PortId {
        PortId::new("fed")
    }
    fn description(&self) -> String {
        "fed".into()
    }
    fn write(&self, _: Vec<u8>) -> Result<(), serialist_core::SessionClosed> {
        Ok(())
    }
    fn set_control(
        &self,
        _: serialist_core::ControlLine,
        _: bool,
    ) -> Result<(), serialist_core::SessionClosed> {
        Ok(())
    }
    fn reconfigure(&self, _: SerialConfig) -> Result<(), serialist_core::SessionClosed> {
        Ok(())
    }
    fn serial_config(&self) -> SerialConfig {
        SerialConfig::default()
    }
    fn store(&self) -> serialist_core::StoreReader {
        self.store.reader()
    }
    fn bell(&self) -> serialist_script::LineBell {
        self.bell.clone()
    }
    fn frames(&self) -> Option<serialist_core::FrameStoreReader> {
        Some(self.frames.clone())
    }
}

#[test]
fn a_handler_that_falls_behind_skips_evicted_frames_instead_of_queuing_them() {
    use serialist_core::{Frame, FrameStore, FrameStoreConfig, Store};

    let mut frames = FrameStore::new(FrameStoreConfig {
        capacity: 600,
        ..FrameStoreConfig::default()
    });
    let bell = serialist_script::LineBell::new();
    let session = Arc::new(FedSession {
        store: Store::default(),
        frames: frames.reader(),
        bell: bell.clone(),
    });
    let mut services = services(None, TestUi::new());
    services.session = Some(session as Arc<dyn ScriptSession>);
    let host = ScriptHost::new(services);
    let run = host.run(ScriptSource::new(
        "behind.lua",
        r#"
        local seen, last, ordered, bytes = 0, -1, true, nil
        local h <close> = serial.current():on_frame(function(frame)
          if frame.fields.n <= last then ordered = false end
          last = frame.fields.n
          bytes = frame.bytes
          seen = seen + 1
        end)
        print("ready")
        while last < 9999 do sleep(5) end
        print(seen <= 1024, ordered, bytes)
        "#,
    ));
    let events = run.events();
    // Started, then "ready" once the handler is registered.
    for _ in 0..2 {
        events.recv_timeout(Duration::from_secs(10)).unwrap();
    }
    // Ten thousand frames at once into a store that keeps 600: the handler gets what is
    // retained when it looks, never a queue of all of them.
    let at = std::time::Instant::now();
    frames.extend((0..10_000u64).map(|n| Frame::new("tick", n..n + 1, at).with_field("n", n)));
    bell.ring();
    let finished = collect(&run, Duration::from_secs(10));
    finished.assert_ok();
    // No scrollback holds the frames' bytes here, so `bytes` is nil.
    assert_eq!(finished.output, ["true\ttrue\tnil"]);
}
