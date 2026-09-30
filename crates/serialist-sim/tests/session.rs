//! `serialist_core::Session` running against simulated devices over virtual links.
//!
//! Rate and latency assertions use the reader thread's `received_at` stamps, never the
//! test thread's clock, so a stalled test thread cannot skew them. The tests that
//! measure time or burn CPU hold [`HEAVY`], so on a small CI runner the 32 MiB firehose
//! never competes with a rate measurement.

mod common;

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serialist_core::{ControlLine, PortId, Session, SessionConfig, SessionEvent, TransportError};
use serialist_sim::{
    AtDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseVerifier, LinkConfig,
    PACED_LOOKAHEAD, SimWorld, virtual_port_id,
};

use common::{collect_until, next_event, serial};

static HEAVY: Mutex<()> = Mutex::new(());

/// Serialise the timing-sensitive and CPU-heavy tests in this binary.
fn heavy() -> MutexGuard<'static, ()> {
    HEAVY.lock().unwrap_or_else(PoisonError::into_inner)
}

fn open(world: &SimWorld, id: &PortId, baud: u32) -> Session {
    let cfg = SessionConfig::new(id.clone(), serial(baud));
    Session::open(world.factory(), cfg).expect("open session")
}

fn expect_connected(events: &Receiver<SessionEvent>) -> String {
    match next_event(events) {
        SessionEvent::Connected { description } => description,
        other => panic!("expected Connected first, got {other:?}"),
    }
}

/// Collect events until `Disconnected`, returning the data bytes, the error, and when
/// the event arrived.
fn drain_to_disconnect(
    events: &Receiver<SessionEvent>,
    verifier: Option<&mut FirehoseVerifier>,
    limit: Duration,
) -> (u64, Option<TransportError>, Instant) {
    let deadline = Instant::now() + limit;
    let mut bytes = 0u64;
    let mut verifier = verifier;
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        match events
            .recv_timeout(timeout)
            .expect("no Disconnected in time")
        {
            SessionEvent::Data { bytes: chunk, .. } => {
                bytes += chunk.len() as u64;
                if let Some(v) = verifier.as_deref_mut() {
                    v.feed(&chunk);
                }
            }
            SessionEvent::Disconnected { error } => return (bytes, error, Instant::now()),
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// The first `Data` event's reader timestamp.
fn first_data(events: &Receiver<SessionEvent>) -> Instant {
    match next_event(events) {
        SessionEvent::Data { received_at, .. } => received_at,
        other => panic!("expected Data, got {other:?}"),
    }
}

/// Bytes per second the reader received in `from..to`, by its own timestamps. Consumes
/// events up to the first one stamped at or after `to`.
fn data_rate(events: &Receiver<SessionEvent>, from: Instant, to: Instant) -> f64 {
    let mut bytes = 0u64;
    loop {
        let SessionEvent::Data {
            bytes: chunk,
            received_at,
        } = next_event(events)
        else {
            panic!("unexpected event");
        };
        if received_at >= to {
            return bytes as f64 / (to - from).as_secs_f64();
        }
        if received_at >= from {
            bytes += chunk.len() as u64;
        }
    }
}

fn within(got: f64, want: f64, tolerance: f64) -> bool {
    (got - want).abs() / want <= tolerance
}

#[test]
fn echo_round_trip() {
    let world = SimWorld::new();
    let session = open(&world, &virtual_port_id(SimWorld::ECHO), 115_200);
    let events = session.events();
    assert_eq!(expect_connected(&events), "virtual:echo @ 115200 8N1");
    session.write(b"hello\r\n".to_vec()).unwrap();
    session.write(b"world\r\n".to_vec()).unwrap();
    let got = collect_until(&events, |r| r.len() >= 14);
    assert_eq!(got, b"hello\r\nworld\r\n");
    assert_eq!(session.stats().tx_bytes, 14);
    assert_eq!(session.stats().rx_bytes, 14);
    session.close();
    assert!(matches!(
        next_event(&events),
        SessionEvent::Disconnected { error: None }
    ));
    assert!(events.recv_timeout(Duration::from_millis(200)).is_err());
}

#[test]
fn close_sends_the_last_command_before_disconnecting() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session = open(&world, &id, 115_200);
    let link = world.link(&id).unwrap();
    // "Send reboot, then disconnect": the write must reach the device.
    session.write(b"reboot\r\n".to_vec()).unwrap();
    session.close();
    assert_eq!(link.stats().host_to_device_bytes, 8);
}

#[test]
fn at_conversation() {
    let world = SimWorld::new();
    let session = open(&world, &virtual_port_id(SimWorld::AT), 115_200);
    let events = session.events();
    expect_connected(&events);

    let ask = |command: &str| -> String {
        session.write(command.as_bytes().to_vec()).unwrap();
        let reply = collect_until(&events, |r| {
            r.ends_with(b"OK\r\n") || r.ends_with(b"ERROR\r\n")
        });
        String::from_utf8(reply).unwrap()
    };
    assert_eq!(ask("AT\r"), "\r\nOK\r\n");
    assert_eq!(
        ask("ATI\r\n"),
        format!("\r\n{}\r\n\r\nOK\r\n", AtDevice::DEFAULT_IDENTITY)
    );
    assert_eq!(
        ask("at+ver?\n"),
        format!("\r\n+VER: {}\r\n\r\nOK\r\n", AtDevice::DEFAULT_VERSION)
    );
    assert_eq!(ask("AT+NOPE\r"), "\r\nERROR\r\n");
    assert_eq!(ask("ATE1\r"), "\r\nOK\r\n");
    assert_eq!(ask("AT\r"), "AT\r\r\nOK\r\n");
}

#[test]
fn firehose_at_3_mbaud_is_complete_and_on_rate() {
    let _heavy = heavy();
    let world = SimWorld::empty();
    let id = world.add_virtual("hose", "Firehose", LinkConfig::default(), || {
        Box::new(FirehoseDevice::new(FirehoseConfig::new(
            FirehoseContent::Mixed,
        )))
    });
    let session = open(&world, &id, 3_000_000);
    let events = session.events();
    expect_connected(&events);

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Mixed);
    let measure = Duration::from_secs(2);
    let mut first: Option<Instant> = None;
    let mut in_window = 0u64;
    loop {
        let SessionEvent::Data { bytes, received_at } = next_event(&events) else {
            panic!("unexpected event");
        };
        verifier.feed(&bytes);
        let t0 = *first.get_or_insert(received_at);
        let since = received_at - t0;
        if since >= measure {
            break;
        }
        // The first chunk marks t0; count what arrived after it.
        if since > Duration::ZERO {
            in_window += bytes.len() as u64;
        }
    }
    session.close();

    let rate = in_window as f64 / measure.as_secs_f64();
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    // Mixed records average about 650 bytes, so 2 s at 300 kB/s is roughly 900 of them.
    assert!(report.records > 500, "{report:?}");
    // 3 Mbaud 8N1 is 300_000 bytes/s.
    assert!(
        within(rate, 300_000.0, 0.10),
        "achieved {rate:.0} B/s, want 300000 +/- 10%"
    );
}

#[test]
fn firehose_32_mib_unpaced_loses_nothing() {
    let _heavy = heavy();
    const TOTAL: u64 = 32 * 1024 * 1024;
    let world = SimWorld::empty();
    let id = world.add_virtual("hose-fast", "Fast firehose", LinkConfig::unpaced(), || {
        let cfg = FirehoseConfig {
            max_batch: 64 * 1024,
            disconnect_when_done: true,
            ..FirehoseConfig::new(FirehoseContent::Mixed).with_total(TOTAL)
        };
        Box::new(FirehoseDevice::new(cfg))
    });
    let session = open(&world, &id, 115_200);
    let link = world.link(&id).expect("link is up");
    let events = session.events();
    expect_connected(&events);

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Mixed);
    let (bytes, error, _) =
        drain_to_disconnect(&events, Some(&mut verifier), Duration::from_secs(120));
    assert!(
        matches!(error, Some(TransportError::Disconnected)),
        "{error:?}"
    );
    assert_eq!(bytes, TOTAL);
    assert_eq!(session.stats().rx_bytes, TOTAL);
    let stats = link.stats();
    assert_eq!(stats.device_to_host_bytes, TOTAL);
    assert_eq!(stats.dropped_bytes, 0);
    assert_eq!(stats.lost_on_unplug, 0);
    let report = verifier.report();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.bytes, TOTAL);
    assert!(report.records > 50_000, "{report:?}");
}

#[test]
fn unplug_mid_stream_disconnects_promptly_without_gaps() {
    let _heavy = heavy();
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 1_000_000);
    let link = world.link(&id).expect("link is up");
    let events = session.events();
    expect_connected(&events);

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    let head = collect_until(&events, |r| r.len() >= 5_000);
    verifier.feed(&head);
    let unplugged_at = Instant::now();
    assert!(world.unplug(&id));
    let (tail, error, disconnected_at) =
        drain_to_disconnect(&events, Some(&mut verifier), Duration::from_secs(5));
    assert!(
        matches!(error, Some(TransportError::Disconnected)),
        "{error:?}"
    );
    let latency = disconnected_at - unplugged_at;
    assert!(
        latency < Duration::from_millis(100),
        "Disconnected {latency:?} after unplug"
    );

    // What arrived is gap-free, and every byte on the wire is accounted for.
    let total = head.len() as u64 + tail;
    let stats = link.stats();
    assert_eq!(total, stats.device_to_host_bytes - stats.lost_on_unplug);
    assert_eq!(session.stats().rx_bytes, total);
    assert!(verifier.report().is_clean(), "{:?}", verifier.report());

    assert!(!session.is_connected());
    assert!(session.write(b"AT\r".to_vec()).is_err());
    assert!(session.set_control(ControlLine::Dtr, true).is_err());
    session.close();
    assert!(
        events.recv_timeout(Duration::from_millis(200)).is_err(),
        "nothing after Disconnected"
    );

    // Gone from the factory until plugged back in; then it starts over from record 0.
    let cfg = SessionConfig::new(id.clone(), serial(1_000_000));
    assert!(matches!(
        Session::open(world.factory(), cfg.clone()),
        Err(TransportError::NotFound(_))
    ));
    assert!(world.plug(&id));
    let again = Session::open(world.factory(), cfg).unwrap();
    let events = again.events();
    expect_connected(&events);
    let mut fresh = FirehoseVerifier::new(FirehoseContent::Text);
    fresh.feed(&collect_until(&events, |r| r.len() >= 2_000));
    assert!(fresh.report().is_clean() && fresh.report().records > 0);
}

#[test]
fn unplug_at_9600_baud_disconnects_within_100ms() {
    let _heavy = heavy();
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 9_600);
    let events = session.events();
    expect_connected(&events);
    // At 960 bytes/s the firehose's 16 KiB batches are 17 s of data each.
    collect_until(&events, |r| r.len() >= 100);
    let unplugged_at = Instant::now();
    assert!(world.unplug(&id));
    let (_, error, disconnected_at) = drain_to_disconnect(&events, None, Duration::from_secs(5));
    assert!(matches!(error, Some(TransportError::Disconnected)));
    let latency = disconnected_at - unplugged_at;
    assert!(
        latency < Duration::from_millis(100),
        "Disconnected {latency:?} after unplug at 9600 baud"
    );
}

#[test]
fn idle_session_reader_does_not_spin() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let mut cfg = SessionConfig::new(id.clone(), serial(115_200));
    cfg.read_timeout = Duration::from_millis(10);
    let session = Session::open(world.factory(), cfg).unwrap();
    let link = world.link(&id).unwrap();
    let before = link.stats().host_read_calls;
    let started = Instant::now();
    thread::sleep(Duration::from_secs(1));
    let calls = link.stats().host_read_calls - before;
    let per_second = calls as f64 / started.elapsed().as_secs_f64();
    assert!(
        per_second < 200.0,
        "{per_second:.0} reads/s at a 10 ms timeout"
    );
    // About 100/s with 1 ms timers, 64/s on Windows' 15.6 ms tick.
    assert!(
        per_second > 20.0,
        "{per_second:.0} reads/s: reader is stalling"
    );
    assert_eq!(session.stats().rx_chunks, 0);

    // Nothing queued, so close is bounded by about one read timeout.
    let started = Instant::now();
    session.close();
    assert!(
        started.elapsed() < Duration::from_millis(200),
        "close took {:?}",
        started.elapsed()
    );
}

#[test]
fn reconfigure_through_the_session_changes_the_link_rate() {
    let _heavy = heavy();
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 1_000_000);
    let link = world.link(&id).unwrap();
    let events = session.events();
    expect_connected(&events);

    // 1 Mbaud: skip 50 ms of start-up, then measure 500 ms.
    let t0 = first_data(&events);
    let from = t0 + Duration::from_millis(50);
    let fast = data_rate(&events, from, from + Duration::from_millis(500));

    // A stalled test thread must not matter: rates come from the reader's timestamps,
    // and the slow window is placed after the change was observed to take effect.
    thread::sleep(Duration::from_millis(100));
    session.reconfigure(serial(500_000)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while link.link_config().serial.baud != 500_000 {
        assert!(
            Instant::now() < deadline,
            "reconfigure never reached the link"
        );
        thread::sleep(Duration::from_millis(1));
    }
    let applied = Instant::now();
    // Bytes already on the wire (at most one look-ahead) still go at the old rate.
    let from = applied + PACED_LOOKAHEAD + Duration::from_millis(20);
    let slow = data_rate(&events, from, from + Duration::from_millis(500));

    assert!(within(fast, 100_000.0, 0.1), "before: {fast:.0} B/s");
    assert!(within(slow, 50_000.0, 0.1), "after: {slow:.0} B/s");
    assert_eq!(session.serial_config().baud, 500_000);
    assert_eq!(session.config().serial.baud, 1_000_000);
    assert_eq!(session.port(), &id);
}

#[test]
fn control_lines_reach_the_link() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session = open(&world, &id, 115_200);
    let link = world.link(&id).unwrap();
    session.set_control(ControlLine::Dtr, false).unwrap();
    session.set_control(ControlLine::Rts, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while link.control_line(ControlLine::Dtr) || link.control_line(ControlLine::Rts) {
        assert!(Instant::now() < deadline, "control lines never changed");
        thread::sleep(Duration::from_millis(1));
    }
}
