//! The RACE codecs end to end: a real `Session` and `Ingest` over the simulated RACE
//! device, with the codec fed as a `CodecSink`. No hardware.

mod common;

use std::time::{Duration, Instant};

use crossbeam_channel::unbounded;
use serialist_core::{
    Codec, CodecSink, EncodeRequest, FrameSnapshot, FrameStore, Ingest, SerialConfig, Session,
    SessionConfig, Store, Value,
};
use serialist_plugins::race::AirohaRace;
use serialist_sim::{LinkConfig, RaceDevice, SimWorld};

use common::lua_race;

fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

/// Talk to a fast-logging RACE device through `codec`, sending requests `encoder` builds,
/// and check what comes back.
fn talk_to_the_device(codec: Box<dyn Codec>, mut encoder: Box<dyn Codec>) {
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
    let frames = FrameStore::default();
    let reader = frames.reader();
    let (wake_tx, wake_rx) = unbounded();
    let sink = CodecSink::new(
        codec,
        frames,
        Some(Box::new(move || {
            let _ = wake_tx.send(());
        })),
    );
    let ingest = Ingest::spawn(
        session.events(),
        Store::default(),
        vec![Box::new(sink)],
        Box::new(|| {}),
    );

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

    // Play the Decoded panel: on each wake, acknowledge, then look.
    let deadline = Instant::now() + Duration::from_secs(10);
    let enough = |snap: &FrameSnapshot| {
        snap.filter("response").count() >= 2
            && snap.filter("log").count() >= 6
            && snap.filter("text").count() >= 2
    };
    loop {
        reader.acknowledge();
        if enough(&reader.snapshot()) {
            break;
        }
        assert!(Instant::now() < deadline, "{:?}", reader.snapshot());
        let _ = wake_rx.recv_timeout(Duration::from_millis(100));
    }
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
    talk_to_the_device(Box::new(AirohaRace::new()), Box::new(AirohaRace::new()));
}

#[test]
fn the_lua_codec_talks_to_the_simulated_device() {
    talk_to_the_device(Box::new(lua_race()), Box::new(lua_race()));
}

#[test]
fn the_built_in_world_has_a_race_device() {
    let world = SimWorld::new();
    let id = serialist_sim::virtual_port_id(SimWorld::RACE);
    assert!(world.source().contains(&id));
}
