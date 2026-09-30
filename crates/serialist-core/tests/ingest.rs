//! The ingest thread end to end: a `Session` over `SimWorld`, no hardware.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, unbounded};
use serialist_core::{
    ChunkSink, Direction, Ingest, LineId, LineSource, SerialConfig, Session, SessionConfig, Store,
    StoreConfig, StyledLine,
};
use serialist_sim::{
    FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseVerifier, LinkConfig, SimWorld,
    virtual_port_id,
};

/// The rate test must not compete with the other test in this binary for CPU.
static HEAVY: Mutex<()> = Mutex::new(());

fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

fn lines(store: &Store) -> Vec<StyledLine> {
    let snap = store.snapshot();
    let mut out = Vec::new();
    snap.lines(snap.first_line()..snap.end(), &mut out);
    out
}

/// Counts what a sink sees.
struct CountingSink(Arc<AtomicU64>, Arc<AtomicU64>);

impl ChunkSink for CountingSink {
    fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
        self.0.fetch_add(bytes.len() as u64, Ordering::Relaxed);
    }

    fn on_disconnect(&mut self) {
        self.1.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn firehose_at_3_mbaud_is_stored_whole() {
    let _heavy = heavy();
    let world = SimWorld::empty();
    let id = world.add_virtual("hose", "Firehose", LinkConfig::default(), || {
        Box::new(FirehoseDevice::new(FirehoseConfig::new(
            FirehoseContent::Text,
        )))
    });
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(3_000_000))).expect("open");
    let sink_bytes = Arc::new(AtomicU64::new(0));
    let sink_disconnects = Arc::new(AtomicU64::new(0));
    let (wake_tx, wake_rx) = unbounded();
    let handle = Ingest::spawn(
        session.events(),
        Store::new(StoreConfig::default()),
        vec![Box::new(CountingSink(
            Arc::clone(&sink_bytes),
            Arc::clone(&sink_disconnects),
        ))],
        Box::new(move || {
            let _ = wake_tx.send(());
        }),
    );

    // Play the UI for two seconds: on each wake, check no second wake is queued,
    // acknowledge, take the frame's snapshot, then "render" for a frame.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut acks = 0u64;
    let mut last_end = LineId(0);
    while Instant::now() < deadline {
        match wake_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => {
                assert!(
                    wake_rx.try_recv().is_err(),
                    "woken twice without an acknowledge"
                );
                handle.acknowledge();
                acks += 1;
                let snap = handle.snapshot();
                assert!(snap.end() >= last_end);
                last_end = snap.end();
                thread::sleep(Duration::from_millis(8));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("waker dropped early"),
        }
    }
    let wakes = handle.stats().wakes;
    session.close();
    let store = handle.join().expect("the ingest thread ran cleanly");

    assert!(acks > 50, "only {acks} frames in 2 s");
    assert!(wakes <= acks + 2, "{wakes} wakes for {acks} acknowledges");

    // Every byte, in order, with nothing damaged.
    let snap = store.snapshot();
    let stats = snap.stats();
    assert_eq!(stats.raw_start, 0);
    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    for slice in snap.raw(0..stats.raw_len) {
        verifier.feed(slice);
    }
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    // 2 s at 300 kB/s; allow for a slow CI runner.
    assert!(report.bytes > 400_000, "{report:?}");
    assert_eq!(sink_bytes.load(Ordering::Relaxed), stats.raw_len);
    assert_eq!(sink_disconnects.load(Ordering::Relaxed), 1);

    // The line index agrees: the complete received lines are the records.
    let all = lines(&store);
    let mut by_line = FirehoseVerifier::new(FirehoseContent::Text);
    let mut complete = 0u64;
    for line in all
        .iter()
        .filter(|l| l.direction == Direction::Rx && l.complete)
    {
        by_line.feed(line.text.as_bytes());
        by_line.feed(b"\n");
        complete += 1;
    }
    assert!(by_line.report().is_clean(), "{:?}", by_line.report());
    assert_eq!(by_line.report().records, complete);
    assert!(complete + 1 >= report.records);
    assert_eq!(all.first().map(|l| l.direction), Some(Direction::Notice));
    let last = all.last().expect("lines");
    assert_eq!(
        (last.direction, last.text.as_str()),
        (Direction::Notice, "Disconnected")
    );
}

#[test]
fn tx_echo_and_notices_keep_their_order() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO_LINES);
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(115_200))).expect("open");
    let handle = Ingest::spawn(
        session.events(),
        Store::default(),
        Vec::new(),
        Box::new(|| {}),
    );
    let wait_for = |what: &str, pred: &dyn Fn(&[StyledLine]) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snap = handle.snapshot();
            let mut lines = Vec::new();
            snap.lines(snap.first_line()..snap.end(), &mut lines);
            if pred(&lines) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {lines:?}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    };
    wait_for("the connect notice", &|l| !l.is_empty());
    for word in ["hello", "again"] {
        handle
            .append_local(word, Direction::Tx)
            .expect("ingest running");
        session
            .write(format!("{word}\r\n").into_bytes())
            .expect("session open");
        wait_for(word, &|l| {
            l.iter()
                .any(|x| x.direction == Direction::Rx && x.complete && x.text == word)
        });
    }
    session.close();
    let store = handle.join().expect("the ingest thread ran cleanly");
    let got: Vec<_> = lines(&store)
        .into_iter()
        .map(|l| (l.direction, l.text))
        .collect();
    assert_eq!(got.len(), 6, "{got:?}");
    assert_eq!(got[0].0, Direction::Notice);
    assert!(
        got[0].1.starts_with("Connected to virtual:echo-lines"),
        "{got:?}"
    );
    assert_eq!(
        &got[1..],
        &[
            (Direction::Tx, "hello".to_owned()),
            (Direction::Rx, "hello".to_owned()),
            (Direction::Tx, "again".to_owned()),
            (Direction::Rx, "again".to_owned()),
            (Direction::Notice, "Disconnected".to_owned()),
        ]
    );
}
