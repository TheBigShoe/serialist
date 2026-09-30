//! The ingest thread end to end: a `Session` over `SimWorld`, no hardware.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, unbounded};
use serialist_core::{
    ChunkSink, ConnectionInfo, Direction, ExpectResult, Ingest, IngestHandle, LineId, LineSource,
    LinkState, SerialConfig, Session, SessionConfig, Store, StoreConfig, StyledLine,
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

/// Remembers what `on_connect` was told.
struct ConnectSink(Arc<Mutex<Vec<String>>>);

impl ChunkSink for ConnectSink {
    fn on_chunk(&mut self, _bytes: &[u8], _at: Instant) {}

    fn on_connect(&mut self, description: &str) {
        self.0.lock().unwrap().push(description.to_owned());
    }

    fn on_disconnect(&mut self) {}
}

/// Poll `handle.connection()` until `done` accepts it.
fn wait_for_connection(
    handle: &IngestHandle,
    what: &str,
    done: impl Fn(&ConnectionInfo) -> bool,
) -> ConnectionInfo {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let info = handle.connection();
        if done(&info) {
            return info;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {info:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn connection_and_on_connect_over_a_session() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO_LINES);
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(115_200))).expect("open");
    let heard = Arc::new(Mutex::new(Vec::new()));
    let handle = Ingest::spawn(
        session.events(),
        Store::default(),
        vec![Box::new(ConnectSink(Arc::clone(&heard)))],
        Box::new(|| {}),
    );

    let info = wait_for_connection(&handle, "connected", |info| {
        matches!(info.state, LinkState::Connected { .. })
    });
    let LinkState::Connected { description } = &info.state else {
        unreachable!("checked above")
    };
    assert!(
        description.starts_with("virtual:echo-lines"),
        "{description}"
    );
    assert_eq!(info.description.as_ref(), Some(description));
    // The state is set before the notice is stored, and the sinks are told after it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while heard.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "the sink never heard on_connect");
        thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(*heard.lock().unwrap(), std::slice::from_ref(description));
    assert_eq!(
        handle.snapshot().line(LineId(0)).unwrap().text,
        format!("Connected to {description}")
    );

    session.close();
    let info = wait_for_connection(&handle, "disconnected", |info| {
        matches!(info.state, LinkState::Disconnected { .. })
    });
    assert_eq!(info.state, LinkState::Disconnected { error: None });
    assert_eq!(info.description.as_ref(), Some(description), "kept");
    let store = handle.join().expect("the ingest thread ran cleanly");
    assert_eq!(heard.lock().unwrap().len(), 1, "on_connect is called once");
    let last = lines(&store).pop().expect("lines");
    assert_eq!(last.text, "Disconnected");
}

#[test]
fn an_unplugged_device_reports_its_error() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session = Session::open(
        world.factory(),
        SessionConfig::new(id.clone(), serial(115_200)),
    )
    .expect("open");
    let handle = Ingest::spawn(
        session.events(),
        Store::default(),
        Vec::new(),
        Box::new(|| {}),
    );
    wait_for_connection(&handle, "connected", |info| {
        matches!(info.state, LinkState::Connected { .. })
    });
    assert!(world.unplug(&id));
    let info = wait_for_connection(&handle, "disconnected", |info| {
        matches!(info.state, LinkState::Disconnected { .. })
    });
    let LinkState::Disconnected { error: Some(error) } = info.state else {
        panic!("an unplug is an error, not an orderly close: {info:?}");
    };
    let store = handle.join().expect("the ingest thread ran cleanly");
    let last = lines(&store).pop().expect("lines");
    assert_eq!(last.direction, Direction::Notice);
    assert_eq!(last.text, format!("Disconnected: {error}"));
    drop(session);
}

/// Poll the newest snapshot until `done` accepts its lines.
fn wait_for_lines(
    handle: &IngestHandle,
    what: &str,
    done: impl Fn(&[(Direction, String, bool)]) -> bool,
) -> Vec<(Direction, String, bool)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snap = handle.snapshot();
        let mut all = Vec::new();
        snap.lines(snap.first_line()..snap.end(), &mut all);
        let shown: Vec<_> = all
            .into_iter()
            .map(|l| (l.direction, l.text, l.complete))
            .collect();
        if done(&shown) {
            return shown;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {shown:?}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn line(direction: Direction, text: &str, complete: bool) -> (Direction, String, bool) {
    (direction, text.to_owned(), complete)
}

#[test]
fn typed_text_is_echoed_key_by_key_around_the_devices_own_echo() {
    // The raw echo device sends back exactly what it is sent, so the received line "hi"
    // grows as the keys are typed, and the local echo is a line of its own beside it.
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(115_200))).expect("open");
    let handle = Ingest::spawn(
        session.events(),
        Store::default(),
        Vec::new(),
        Box::new(|| {}),
    );
    wait_for_lines(&handle, "the connect notice", |l| l.len() == 1);
    let expectation = handle
        .matchers()
        .expect("^hi$", Duration::from_secs(5))
        .expect("a valid pattern");

    // Typed and written, like the UI: the echo first, then the byte.
    let type_key = |key: &str| {
        handle
            .append_local_inline(key, Direction::Tx)
            .expect("ingest running");
        session
            .write(key.as_bytes().to_vec())
            .expect("session open");
    };
    type_key("h");
    let shown = wait_for_lines(&handle, "the device's echo of h", |l| l.len() == 3);
    assert_eq!(
        shown[1..],
        [
            line(Direction::Tx, "h", false),
            line(Direction::Rx, "h", false)
        ],
        "the typed line was ended by the received byte that came after it"
    );

    // A received line in progress is not ended by typing: the typed text follows it.
    type_key("i");
    let shown = wait_for_lines(&handle, "the device's echo of i", |l| {
        l.len() == 4 && l[2].1 == "hi"
    });
    assert_eq!(
        shown[1..],
        [
            line(Direction::Tx, "h", false),
            line(Direction::Rx, "hi", false),
            line(Direction::Tx, "i", false),
        ]
    );

    // Enter closes the typed line, and the device's CRLF ends the received one.
    handle
        .append_local_inline("\n", Direction::Tx)
        .expect("ingest running");
    session.write(b"\r\n".to_vec()).expect("session open");
    let shown = wait_for_lines(&handle, "the line to end", |l| l[2].2);
    assert_eq!(
        shown[1..],
        [
            line(Direction::Tx, "h", false),
            line(Direction::Rx, "hi", true),
            line(Direction::Tx, "i", true),
        ]
    );
    // The received line was matched whole, though typing went on beside it.
    let ExpectResult::Matched {
        line: matched,
        text,
        range,
        ..
    } = expectation
        .wait_timeout(Duration::from_secs(5))
        .expect("matched")
    else {
        panic!("expected a match");
    };
    assert_eq!((matched, text.as_str(), range), (LineId(2), "hi", 0..2));

    session.close();
    let store = handle.join().expect("the ingest thread ran cleanly");
    let last = lines(&store).pop().expect("lines");
    assert_eq!(last.text, "Disconnected");
}

#[test]
fn taking_typed_text_back_leaves_the_line_ids_expectations_wait_on_intact() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(115_200))).expect("open");
    let handle = Ingest::spawn(
        session.events(),
        Store::default(),
        Vec::new(),
        Box::new(|| {}),
    );
    wait_for_lines(&handle, "the connect notice", |l| l.len() == 1);

    handle
        .append_local_inline("ab", Direction::Tx)
        .expect("ingest running");
    wait_for_lines(&handle, "the typed line", |l| l.len() == 2);
    // Registered while the typed line exists, so it starts after it...
    let expectation = handle
        .matchers()
        .expect("^OK$", Duration::from_secs(5))
        .expect("a valid pattern");
    // ...which is then taken back, so the reply takes its id, and still counts.
    handle.truncate_local_line(5).expect("ingest running");
    wait_for_lines(&handle, "the line to go", |l| l.len() == 1);
    session.write(b"OK\r\n".to_vec()).expect("session open");
    let ExpectResult::Matched {
        line: matched,
        text,
        ..
    } = expectation
        .wait_timeout(Duration::from_secs(5))
        .expect("matched")
    else {
        panic!("expected a match");
    };
    assert_eq!((matched, text.as_str()), (LineId(1), "OK"));

    session.close();
    handle.join().expect("the ingest thread ran cleanly");
}
