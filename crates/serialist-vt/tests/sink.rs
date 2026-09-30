//! The sink on its own, and end to end: a `Session` over `SimWorld`, `Ingest::spawn_with`
//! with a `VtSink` next to the store, the way the UI will run VT mode. No hardware.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, unbounded};
use parking_lot::Mutex;
use serialist_core::{
    ChunkSink, Ingest, LineId, SerialConfig, Session, SessionConfig, Store, StyleFlags,
};
use serialist_sim::{DeviceOutput, LinkConfig, MenuDevice, SimDevice, SimWorld};
use serialist_vt::{VtEvent, VtHandle, VtScreen, VtSink, VtSnapshot};

use common::screen_text;

fn counting_sink() -> (VtSink, VtHandle, Arc<AtomicUsize>) {
    let wakes = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&wakes);
    let sink = VtSink::new(
        VtScreen::new(20, 4, 100),
        Some(Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let handle = sink.handle();
    (sink, handle, wakes)
}

#[test]
fn the_sink_publishes_each_chunk_and_wakes_once_per_acknowledge() {
    let (mut sink, screen, wakes) = counting_sink();
    let now = Instant::now();
    assert!(screen_text(&screen.snapshot()).iter().all(String::is_empty));

    sink.on_chunk(b"hel", now);
    assert_eq!(screen_text(&screen.snapshot())[0], "hel");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    sink.on_chunk(b"lo", now);
    assert_eq!(screen_text(&screen.snapshot())[0], "hello");
    assert_eq!(wakes.load(Ordering::SeqCst), 1, "a wake is still pending");

    screen.acknowledge();
    let generation = screen.snapshot().generation();
    // A chunk that changes nothing on screen publishes nothing and wakes nobody...
    sink.on_chunk(b"\x1b[0m", now);
    assert_eq!(screen.snapshot().generation(), generation);
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    // ...but a query queues an event, which wakes.
    sink.on_chunk(b"\x1b[5n", now);
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
    assert_eq!(
        screen.take_events(),
        [VtEvent::Respond(b"\x1b[0n".to_vec())]
    );
}

#[test]
fn a_responder_answers_on_the_ingest_thread_instead_of_queueing() {
    let answered = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&answered);
    let mut sink = VtSink::new(VtScreen::new(20, 4, 100), None)
        .with_responder(move |bytes: &[u8]| log.lock().push(bytes.to_vec()));
    let screen = sink.handle();
    sink.on_chunk(b"\x1b[c\x1b]2;box\x07\x1b[2;3H\x1b[6n", Instant::now());
    assert_eq!(
        *answered.lock(),
        [b"\x1b[?6c".to_vec(), b"\x1b[2;3R".to_vec()]
    );
    assert_eq!(screen.take_events(), [VtEvent::Title("box".into())]);
    assert_eq!(screen.snapshot().title(), Some("box"));
}

#[test]
fn the_handle_resizes_and_resets() {
    let (mut sink, screen, _) = counting_sink();
    sink.on_chunk(b"a\r\nb\r\nc\r\nd", Instant::now());
    assert_eq!(screen.size(), (20, 4));
    screen.resize(30, 2);
    let snap = screen.snapshot();
    assert_eq!((snap.columns(), snap.viewport_rows()), (30, 2));
    assert_eq!(screen_text(&snap), ["c", "d"]);
    assert_eq!(snap.scrollback_lines(), 2);
    screen.reset();
    let snap = screen.snapshot();
    assert_eq!(snap.scrollback_lines(), 0);
    assert_eq!(screen_text(&snap), ["", ""]);
}

#[test]
fn idle_and_disconnect_end_an_abandoned_synchronized_update() {
    let (mut sink, screen, wakes) = counting_sink();
    sink.on_chunk(b"\x1b[?2026hheld", Instant::now());
    assert_eq!(screen_text(&screen.snapshot())[0], "");
    assert_eq!(wakes.load(Ordering::SeqCst), 0, "nothing new to show");
    sink.on_idle(Instant::now());
    assert_eq!(screen_text(&screen.snapshot())[0], "", "not timed out yet");
    sink.on_idle(Instant::now() + Duration::from_secs(1));
    assert_eq!(screen_text(&screen.snapshot())[0], "held");
    assert_eq!(wakes.load(Ordering::SeqCst), 1);

    screen.acknowledge();
    sink.on_chunk(b"\x1b[?2026h more", Instant::now());
    sink.on_disconnect();
    assert_eq!(screen_text(&screen.snapshot())[0], "held more");
    assert_eq!(wakes.load(Ordering::SeqCst), 2);
}

/// Play the UI: on each wake, acknowledge, then look, until `enough` holds.
fn wait_for(
    screen: &VtHandle,
    wakes: &Receiver<()>,
    what: &str,
    enough: impl Fn(&VtSnapshot) -> bool,
) -> Arc<VtSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        screen.acknowledge();
        let snapshot = screen.snapshot();
        if enough(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < deadline, "{what}: {snapshot:?}");
        let _ = wakes.recv_timeout(Duration::from_millis(100));
    }
}

/// Screen rows drawn in reverse video.
fn highlighted(snapshot: &VtSnapshot) -> Vec<usize> {
    (0..snapshot.viewport_rows())
        .filter(|&row| {
            snapshot.visible_line(row).is_some_and(|line| {
                line.runs
                    .iter()
                    .any(|run| run.style.flags.contains(StyleFlags::INVERSE))
            })
        })
        .collect()
}

/// The redraw count on the menu's status row.
fn redraws(snapshot: &VtSnapshot) -> u64 {
    let status = &snapshot
        .visible_line(MenuDevice::STATUS_ROW - 1)
        .expect("a status row")
        .text;
    status
        .trim()
        .trim_start_matches("Redraw ")
        .split(',')
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Start a session on `id` with a VT screen fed next to the store. The responder writes
/// the device's answers straight back through the session.
fn open(
    world: &SimWorld,
    id: serialist_core::PortId,
) -> (
    Arc<Session>,
    serialist_core::IngestHandle,
    VtHandle,
    Receiver<()>,
) {
    let session = Arc::new(
        Session::open(
            world.factory(),
            SessionConfig::new(id, SerialConfig::default()),
        )
        .expect("the virtual port opens"),
    );
    let (wake_tx, wake_rx) = unbounded();
    let writer = Arc::clone(&session);
    let sink = VtSink::new(
        VtScreen::new(60, 10, 1000),
        Some(Box::new(move || {
            let _ = wake_tx.send(());
        })),
    )
    .with_responder(move |bytes: &[u8]| {
        let _ = writer.write(bytes.to_vec());
    });
    let screen = sink.handle();
    let ingest = Ingest::spawn_with(
        session.events(),
        Store::default(),
        Box::new(move || -> Vec<Box<dyn ChunkSink>> { vec![Box::new(sink)] }),
        Box::new(|| {}),
    );
    (session, ingest, screen, wake_rx)
}

fn close(session: Arc<Session>, ingest: serialist_core::IngestHandle) -> Store {
    // Stopping the ingest thread drops the sink, and with it the responder's session.
    let store = ingest.stop().expect("the ingest thread ran cleanly");
    Arc::try_unwrap(session)
        .expect("only the test holds the session now")
        .close();
    store
}

#[test]
fn a_boot_menu_over_a_session_draws_in_place_and_follows_the_arrows() {
    let world = SimWorld::empty();
    let id = world.add_virtual(SimWorld::MENU, "Boot menu", LinkConfig::unpaced(), || {
        Box::new(MenuDevice::new().with_redraw_interval(Some(Duration::from_millis(20))))
    });
    let (session, ingest, screen, wakes) = open(&world, id);

    let snap = wait_for(&screen, &wakes, "the menu", |snap| {
        highlighted(snap) == [2]
            && snap
                .visible_line(0)
                .is_some_and(|l| l.text.contains("Boot Menu"))
    });
    assert_eq!(
        screen_text(&snap)[..7],
        [
            "  *** Serialist Boot Menu ***",
            "",
            "     Boot from eMMC",
            "     Boot from network (TFTP)",
            "     U-Boot console",
            "",
            "  Press UP/DOWN to move, ENTER to select",
        ]
    );
    assert!(!snap.cursor().unwrap().visible, "the menu hides the cursor");

    session.write(b"\x1b[B".to_vec()).unwrap();
    wait_for(&screen, &wakes, "the second item", |snap| {
        highlighted(snap) == [3]
    });
    // Application cursor key form, then Enter.
    session.write(b"\x1bOB\r".to_vec()).unwrap();
    let chosen = wait_for(&screen, &wakes, "the selection", |snap| {
        highlighted(snap) == [4]
            && snap
                .visible_line(MenuDevice::STATUS_ROW - 1)
                .is_some_and(|l| l.text.ends_with("selected: U-Boot console"))
    });

    // The timer keeps redrawing, in place: nothing scrolls, so ids stay 0 to 9.
    let later = wait_for(&screen, &wakes, "more redraws", |snap| {
        redraws(snap) >= redraws(&chosen) + 5
    });
    assert_eq!(later.scrollback_lines(), 0);
    assert_eq!(later.first_visible(), LineId(0));
    assert_eq!(
        later.changed_since(&chosen),
        LineId(7)..LineId(8),
        "only the status row"
    );

    let store = close(session, ingest);
    assert!(store.stats().raw_len > 0, "the store saw the same bytes");
}

/// Asks the terminal who it is and where its cursor is, and keeps what comes back.
struct Asker(Arc<Mutex<Vec<u8>>>);

impl SimDevice for Asker {
    fn name(&self) -> &str {
        "asker"
    }

    fn on_connect(&mut self, out: &mut dyn DeviceOutput) {
        out.send(b"login: \x1b[c\x1b[6n");
    }

    fn on_receive(&mut self, bytes: &[u8], _: &mut dyn DeviceOutput) {
        self.0.lock().extend_from_slice(bytes);
    }
}

#[test]
fn a_device_query_is_answered_through_the_session() {
    let world = SimWorld::empty();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&heard);
    let id = world.add_virtual("asker", "Asker", LinkConfig::unpaced(), move || {
        Box::new(Asker(Arc::clone(&log)))
    });
    let (session, ingest, screen, wakes) = open(&world, id);
    wait_for(&screen, &wakes, "the prompt", |snap| {
        snap.visible_line(0).is_some_and(|l| l.text == "login:")
    });
    // Primary device attributes, then the cursor after "login: " (row 1, column 8).
    let expected = b"\x1b[?6c\x1b[1;8R";
    let deadline = Instant::now() + Duration::from_secs(10);
    while heard.lock().len() < expected.len() {
        assert!(Instant::now() < deadline, "{:?}", heard.lock());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*heard.lock(), expected);
    assert!(screen.take_events().is_empty(), "answers are not queued");
    close(session, ingest);
}
