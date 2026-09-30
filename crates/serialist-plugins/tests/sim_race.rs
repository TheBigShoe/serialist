//! The RACE codecs end to end: a real `Session` and `Ingest` over the simulated RACE
//! device, with the codec made on the ingest thread from a factory
//! (`Ingest::spawn_with` and `CodecSink::from_factory`). No hardware.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use serialist_core::{
    ChunkSink, Codec, CodecFactory, CodecSink, EncodeRequest, FrameSnapshot, FrameStore,
    FrameStoreReader, Ingest, IngestHandle, SerialConfig, Session, SessionConfig, Store, Value,
};
use serialist_plugins::race::AirohaRace;
use serialist_plugins::{LuaCodecFactory, LuaLimits, bundled_race_lua};
use serialist_sim::{LinkConfig, RaceDevice, SimWorld, virtual_port_id};

use common::lua_race;

fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

/// Start ingest for `session` with a codec `factory` makes on the ingest thread. Returns
/// the handle, the decoded frames' reader and the channel its waker rings.
fn spawn_decoding(
    session: &Session,
    factory: Arc<dyn CodecFactory>,
) -> (IngestHandle, FrameStoreReader, Receiver<()>) {
    let frames = FrameStore::default();
    let reader = frames.reader();
    let (wake_tx, wake_rx) = unbounded();
    let ingest = Ingest::spawn_with(
        session.events(),
        Store::default(),
        Box::new(move || -> Vec<Box<dyn ChunkSink>> {
            let waker = move || {
                let _ = wake_tx.send(());
            };
            vec![Box::new(CodecSink::from_factory(
                factory,
                frames,
                Some(Box::new(waker)),
            ))]
        }),
        Box::new(|| {}),
    );
    (ingest, reader, wake_rx)
}

/// Play the Decoded panel until `enough` holds: on each wake, acknowledge, then look.
fn wait_for(
    reader: &FrameStoreReader,
    wakes: &Receiver<()>,
    enough: impl Fn(&FrameSnapshot) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        reader.acknowledge();
        if enough(&reader.snapshot()) {
            return;
        }
        assert!(Instant::now() < deadline, "{:?}", reader.snapshot());
        let _ = wakes.recv_timeout(Duration::from_millis(100));
    }
}

/// Talk to a fast-logging RACE device through a codec from `factory`, sending requests
/// `encoder` builds, and check what comes back.
fn talk_to_the_device(factory: Arc<dyn CodecFactory>, mut encoder: Box<dyn Codec>) {
    let world = SimWorld::empty();
    let id = world.add_virtual("race", "Airoha RACE", LinkConfig::default(), || {
        Box::new(
            RaceDevice::new()
                .with_log_interval(Some(Duration::from_millis(15)))
                .with_text_every(3),
        )
    });
    let session = Session::open(world.factory(), SessionConfig::new(id, serial(921_600)))
        .expect("the virtual port opens");
    let (ingest, reader, wakes) = spawn_decoding(&session, factory);

    let version = encoder
        .encode(&EncodeRequest::new("race_version"))
        .expect("the version query encodes");
    assert_eq!(version, [0x05, 0x5A, 0x02, 0x00, 0x15, 0x0F]);
    session.write(version).unwrap();
    let unknown = encoder
        .encode(
            &EncodeRequest::new("race")
                .with("cmd_id", "0x1234")
                .with("payload", "AA BB"),
        )
        .unwrap();
    session.write(unknown).unwrap();

    wait_for(&reader, &wakes, |snap| {
        snap.filter("response").count() >= 2
            && snap.filter("log").count() >= 6
            && snap.filter("text").count() >= 2
    });
    session.close();
    let store = ingest.join().expect("the ingest thread ran cleanly");

    let decoded = reader.snapshot();
    let responses: Vec<_> = decoded.filter("response").map(|(_, f)| f).collect();
    assert_eq!(responses[0].field("cmd_id"), Some(&Value::UInt(0x0F15)));
    assert_eq!(
        responses[0].field("cmd_id_hex"),
        Some(&Value::Str("0x0F15".into()))
    );
    assert_eq!(
        responses[0].field("payload"),
        Some(&Value::Bytes(RaceDevice::new().version_payload()))
    );
    assert_eq!(responses[1].field("cmd_id"), Some(&Value::UInt(0x1234)));
    assert_eq!(
        responses[1].field("payload"),
        Some(&Value::Bytes(vec![RaceDevice::STATUS_UNSUPPORTED]))
    );
    let logs: Vec<_> = decoded.filter("log").map(|(_, f)| f).collect();
    for (n, log) in logs.iter().enumerate() {
        assert_eq!(log.field("cmd_id"), Some(&Value::UInt(0x0F40)));
        assert_eq!(
            log.field("payload"),
            Some(&Value::Bytes(
                RaceDevice::log_text(n as u64 + 1).into_bytes()
            ))
        );
    }
    let texts: Vec<_> = decoded
        .filter("text")
        .filter_map(|(_, f)| f.field("text")?.as_str())
        .collect();
    assert_eq!(texts[0], "Airoha RACE simulator SIM-RACE 1.4.2");
    assert_eq!(texts[1], "sim: heartbeat 3");

    // Every frame's raw range names exactly its bytes in the scrollback: read them back
    // from a store snapshot and decode them alone to the same frame. Frames tile the
    // stream from offset zero, the ingest start.
    let raw = store.snapshot();
    let mut next = 0;
    for (_, frame) in decoded.frames() {
        assert_eq!(frame.raw.start, next, "frames tile the stream");
        next = frame.raw.end;
        let bytes: Vec<u8> = raw.raw(frame.raw.clone()).flatten().copied().collect();
        assert_eq!(bytes.len() as u64, frame.raw_len(), "{frame:?}");
        let mut alone = Vec::new();
        AirohaRace::new().decode(&bytes, frame.at, frame.raw.start, &mut alone);
        assert_eq!(alone, std::slice::from_ref(frame));
    }
    assert!(next <= raw.raw_range().end);
}

#[test]
fn the_rust_codec_talks_to_the_simulated_device() {
    talk_to_the_device(AirohaRace::factory(), Box::new(AirohaRace::new()));
}

#[test]
fn the_lua_codec_talks_to_the_simulated_device() {
    let factory = Arc::new(bundled_race_lua(LuaLimits::default()).unwrap());
    talk_to_the_device(factory, Box::new(lua_race()));
}

#[cfg(feature = "wasm")]
#[test]
fn the_wasm_codec_talks_to_the_simulated_device() {
    let factory = Arc::new(common::wasm_race_factory());
    talk_to_the_device(factory, Box::new(common::wasm_race()));
}

/// The plugin file on disk, the built-in `virtual:race` device, and a codec made on the
/// ingest thread by a `LuaCodecFactory`: the path the app takes.
#[test]
fn a_lua_plugin_file_decodes_the_built_in_race_device() {
    let plugin =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/plugins/airoha-race/plugin.lua");
    let factory = LuaCodecFactory::load(&plugin).expect("the plugin file loads");
    let mut encoder = factory.create().unwrap();
    let world = SimWorld::new();
    let session = Session::open(
        world.factory(),
        SessionConfig::new(virtual_port_id(SimWorld::RACE), serial(921_600)),
    )
    .expect("virtual:race opens");
    let (ingest, reader, wakes) = spawn_decoding(&session, Arc::new(factory));
    session
        .write(encoder.encode(&EncodeRequest::new("race_version")).unwrap())
        .unwrap();
    wait_for(&reader, &wakes, |snap| {
        snap.filter("response").count() >= 1 && snap.filter("log").count() >= 1
    });
    session.close();
    ingest.join().expect("the ingest thread ran cleanly");

    let decoded = reader.snapshot();
    let (_, response) = decoded.filter("response").next().unwrap();
    assert_eq!(response.field("cmd_id"), Some(&Value::UInt(0x0F15)));
    assert_eq!(
        response.field("payload"),
        Some(&Value::Bytes(RaceDevice::new().version_payload()))
    );
    let (_, log) = decoded.filter("log").next().unwrap();
    assert_eq!(
        log.field("payload"),
        Some(&Value::Bytes(RaceDevice::log_text(1).into_bytes()))
    );
    assert_eq!(
        decoded.filter("text").next().unwrap().1.field("text"),
        Some(&Value::Str("Airoha RACE simulator SIM-RACE 1.4.2".into()))
    );
    assert!(decoded.filter("codec_error").next().is_none());
}

#[test]
fn the_built_in_world_has_a_race_device() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::RACE);
    assert!(world.source().contains(&id));
}
