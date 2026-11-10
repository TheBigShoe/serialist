//! `codecs.<name>.encode`: building commands with an installed codec, against the
//! simulated RACE device.

mod common;

use std::sync::Arc;

use serialist_core::{CodecFactory, CodecRegistry};
use serialist_script::{CodecSource, ScriptHost};
use serialist_sim::RaceDevice;

use common::{TestUi, open, race, run, services, world};

/// A registry holding `factory` under its name.
fn registry(factory: Arc<race::Factory>) -> Arc<dyn CodecSource> {
    let mut registry = CodecRegistry::new();
    registry.register(factory as Arc<dyn CodecFactory>);
    Arc::new(registry)
}

#[test]
fn encode_builds_a_command_the_device_answers() {
    let world = world();
    let session = open(&world, "virtual:race");
    let factory = race::Factory::new();
    let mut services = services(Some(session), TestUi::new());
    services.codecs = Some(registry(Arc::clone(&factory)));
    let host = ScriptHost::new(services);
    let finished = run(
        &host,
        "encode.lua",
        r#"
        local race = assert(codecs.race)
        assert(race.name == "race" and tostring(race) == "codec race")
        local version = race.encode{ cmd_id = 0x0F15 }
        print(hex.encode(version))
        -- The saved-command spellings: hex text, a `command` key, the method call.
        assert(race.encode{ cmd_id = "0x0F15" } == version)
        assert(codecs.race.encode{ command = "race", cmd_id = 3861 } == version)
        assert(codecs.race:encode{ cmd_id = "0f15" } == version)
        print(hex.encode(race.encode{ cmd_id = 1, payload = "AA BB" }))
        print(hex.encode(race.encode{ cmd_id = 1, payload = { 0xAA, 0xBB } }))
        print(hex.encode(race.encode{ cmd_id = 1, payload = {} }))
        local port = serial.current()
        port:write(version)
        -- The response, past the banner and any log frame: sync, type, length, command
        -- id, status 0, then the version.
        local seen = ""
        while not seen:find("\x05\x5B", 1, true) do
          seen = seen .. assert(port:read(1, 2000))
        end
        print(hex.encode("\x05\x5B" .. assert(port:read(5, 2000))))
        "#,
    );
    finished.assert_ok();
    assert_eq!(finished.output[0], "05 5A 02 00 15 0F");
    assert_eq!(finished.output[1], "05 5A 04 00 01 00 AA BB");
    assert_eq!(finished.output[2], "05 5A 04 00 01 00 AA BB");
    assert_eq!(finished.output[3], "05 5A 02 00 01 00");
    let expected_len =
        2 + 1 + RaceDevice::DEFAULT_VERSION.len() + 1 + RaceDevice::DEFAULT_BUILD_TIME.len();
    let len = expected_len.to_le_bytes();
    assert_eq!(
        finished.output[4],
        format!("05 5B {:02X} {:02X} 15 0F 00", len[0], len[1])
    );
    // One codec for the run, however many calls.
    assert_eq!(factory.made(), 1);
}

#[test]
fn a_codec_that_is_not_installed_reads_as_nil() {
    // No codec source at all.
    let host = ScriptHost::new(services(None, TestUi::new()));
    run(
        &host,
        "none.lua",
        r#"
        assert(codecs.race == nil)
        assert(codecs["airoha-race"] == nil)
        assert(codecs[1] == nil)
        "#,
    )
    .assert_ok();

    // A source without that name.
    let mut services = services(None, TestUi::new());
    services.codecs = Some(registry(race::Factory::new()));
    let host = ScriptHost::new(services);
    run(
        &host,
        "other.lua",
        r#"
        assert(codecs.race)
        assert(codecs.other == nil)
        "#,
    )
    .assert_ok();
    let finished = run(&host, "index.lua", "codecs.other.encode{}");
    assert!(
        finished.error().contains("attempt to index a nil value"),
        "{}",
        finished.error()
    );
}

#[test]
fn bad_fields_raise_errors_that_name_the_codec_and_field() {
    let factory = race::Factory::new();
    let mut services = services(None, TestUi::new());
    services.codecs = Some(registry(Arc::clone(&factory)));
    let host = ScriptHost::new(services);
    let errors = [
        (
            "race.encode{}",
            "codecs.race.encode: missing field `cmd_id`",
        ),
        (
            "race.encode{ cmd_id = 0x10000 }",
            "codecs.race.encode: field `cmd_id`: 0x10000 is larger than 0xFFFF",
        ),
        (
            "race.encode{ cmd_id = 1, colour = 'red' }",
            "codecs.race.encode: field `colour`: not a field of `race`",
        ),
        (
            "race.encode{ command = 'reboot' }",
            "codecs.race.encode: unknown command `reboot`",
        ),
        (
            "race.encode('x')",
            "codecs.race.encode: fields must be a table",
        ),
        (
            "race.encode{ cmd_id = 1, payload = '\\xff' }",
            "codecs.race.encode: field `payload` is not UTF-8 text",
        ),
        (
            "race.encode{ [1] = 2 }",
            "codecs.race.encode: field names must be strings",
        ),
        (
            "race.encode{ cmd_id = print }",
            "codecs.race.encode: field `cmd_id` cannot be a function",
        ),
        (
            "local t = {} t.t = t race.encode{ cmd_id = t }",
            "nests tables more than 16 deep",
        ),
        ("race.encode{ cmd_id = 0/0 }", "is not a finite number"),
        (
            "race.encode{ command = 'boom' }",
            "codecs.race.encode: the codec panicked",
        ),
    ];
    for (code, expected) in errors {
        let finished = run(
            &host,
            "bad.lua",
            &format!("local race = codecs.race\n{code}"),
        );
        assert!(
            finished.error().contains(expected),
            "{code}: {}",
            finished.error()
        );
    }
    // pcall catches it like any error, and a codec that panicked is made again.
    run(
        &host,
        "pcall.lua",
        r#"
        local ok, err = pcall(codecs.race.encode, { command = "boom" })
        assert(not ok and tostring(err):find("panicked"), tostring(err))
        assert(codecs.race.encode{ cmd_id = 1 })
        "#,
    )
    .assert_ok();
}

#[test]
fn a_reload_that_replaces_the_factory_gets_a_fresh_codec() {
    use parking_lot::Mutex;

    /// A source whose factory a test swaps, as a plugin reload does in the app.
    struct Swappable(Mutex<Arc<race::Factory>>);

    impl CodecSource for Swappable {
        fn codec(&self, name: &str) -> Option<Arc<dyn CodecFactory>> {
            (name == race::NAME).then(|| Arc::clone(&self.0.lock()) as Arc<dyn CodecFactory>)
        }
    }

    let first = race::Factory::new();
    let second = race::Factory::new();
    let source = Arc::new(Swappable(Mutex::new(Arc::clone(&first))));
    let mut services = services(None, TestUi::new());
    services.codecs = Some(Arc::clone(&source) as Arc<dyn CodecSource>);
    let host = ScriptHost::new(services);
    let swap_to = Arc::clone(&second);
    let swapper = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        *source.0.lock() = swap_to;
    });
    run(
        &host,
        "reload.lua",
        r#"
        for i = 1, 5 do assert(codecs.race.encode{ cmd_id = i }) end
        sleep(600)
        for i = 1, 5 do assert(codecs.race.encode{ cmd_id = i }) end
        "#,
    )
    .assert_ok();
    swapper.join().unwrap();
    assert_eq!((first.made(), second.made()), (1, 1));
}
