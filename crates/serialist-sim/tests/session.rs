//! `serialist_core::Session` running against simulated devices over virtual links.
//!
//! Timing is asserted exactly, on a world built with `SimWorld::with_clock` and a
//! [`ManualClock`]; `common` describes the pattern. The session's reader thread blocks in
//! `read` with its timeout, which the link measures on the manual clock, so these tests
//! settle two threads per link: the device thread and the session's reader. The reader
//! stamps `received_at` from real time, so rates come from byte counts at known clock
//! times instead.

mod common;

use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use parking_lot::Mutex;
use serialist_core::{ControlLine, PortId, Session, SessionConfig, SessionEvent, TransportError};
use serialist_sim::{
    AtDevice, DeviceOutput, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseGenerator,
    FirehoseReport, FirehoseVerifier, LinkConfig, ManualClock, SimDevice, SimWorld,
    virtual_port_id,
};

use common::{
    MS, UnplugOnPanic, advance_to, close_on, collect_until, committed, next_event, released,
    released_switching, serial, take_data, wait_for,
};

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

/// Collect events until `Disconnected`, returning the data bytes and the error.
fn drain_to_disconnect(
    events: &Receiver<SessionEvent>,
    verifier: Option<&mut FirehoseVerifier>,
    limit: Duration,
) -> (u64, Option<TransportError>) {
    let deadline = std::time::Instant::now() + limit;
    let mut bytes = 0u64;
    let mut verifier = verifier;
    loop {
        let timeout = deadline.saturating_duration_since(std::time::Instant::now());
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
            SessionEvent::Disconnected { error } => return (bytes, error),
            other => panic!("unexpected {other:?}"),
        }
    }
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
    assert!(events.try_recv().is_err(), "nothing after Disconnected");
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

/// Two seconds of mixed firehose traffic through a session at `baud`, in 100 ms windows
/// moved in 20 ms steps. After every window the session has delivered exactly what the
/// paced wire has released, and the link has dropped and damaged nothing. Returns the
/// bytes received and the verifier's report on them.
fn firehose_through_a_session(baud: u32) -> (usize, FirehoseReport) {
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::empty_with_clock(clock.clone());
    let id = world.add_virtual("hose", "Firehose", LinkConfig::default(), || {
        Box::new(FirehoseDevice::new(FirehoseConfig::new(
            FirehoseContent::Mixed,
        )))
    });
    let session = open(&world, &id, baud);
    let link = world.link(&id).expect("link is up");
    let _unplug = UnplugOnPanic(link.clone());
    let events = session.events();
    expect_connected(&events);
    clock.settle(2);
    let t0 = clock.now();

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Mixed);
    let mut received = 0;
    for window in 1..=20u32 {
        let elapsed = 100 * MS * window;
        advance_to(&clock, t0 + elapsed, 20 * MS, 2);
        let data = take_data(&events);
        verifier.feed(&data);
        received += data.len();
        assert_eq!(
            received,
            released(baud, elapsed),
            "{baud} baud after {elapsed:?}"
        );
        assert_eq!(session.stats().rx_bytes, received as u64);
    }
    let stats = link.stats();
    assert_eq!(stats.dropped_bytes + stats.corrupted_bytes, 0, "{stats:?}");
    close_on(&clock, session, MS);
    (received, verifier.into_report())
}

#[test]
fn firehose_at_3_mbaud_is_complete_and_on_rate() {
    // 3 Mbaud 8N1 is 300 000 bytes/s in 300-byte packets: 30 000 bytes a window, less
    // one packet for the 1 ms latency in the first.
    let (received, report) = firehose_through_a_session(3_000_000);
    assert_eq!(received, 599_700);
    assert!(report.is_clean(), "{report:?}");
    // Mixed records average about 650 bytes, so 2 s at 300 kB/s is roughly 900 of them.
    assert!(report.records > 500, "{report:?}");
}

#[test]
fn firehose_at_12_mbaud_is_complete_and_on_rate() {
    // 12 Mbaud 8N1, an FT232H or FT2232H at full speed, is 1 200 000 bytes/s in
    // 1 200-byte packets: 120 000 bytes a window, less one packet in the first.
    let (received, report) = firehose_through_a_session(12_000_000);
    assert_eq!(received, 2_398_800);
    assert!(report.is_clean(), "{report:?}");
    // Roughly 3 700 records of about 650 bytes.
    assert!(report.records > 2_000, "{report:?}");
}

/// Feeds everything the host writes into a verifier the test reads.
struct VerifyingSink(Arc<Mutex<FirehoseVerifier>>);

impl SimDevice for VerifyingSink {
    fn name(&self) -> &str {
        "sink"
    }

    fn on_receive(&mut self, bytes: &[u8], _out: &mut dyn DeviceOutput) {
        self.0.lock().feed(bytes);
    }
}

#[test]
fn a_large_write_at_12_mbaud_reaches_the_device_whole_and_on_rate() {
    const BAUD: u32 = 12_000_000;
    // Two seconds of wire time at 1 200 000 bytes/s.
    const TOTAL: usize = 2_400_000;
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::empty_with_clock(clock.clone());
    let verifier = Arc::new(Mutex::new(FirehoseVerifier::new(FirehoseContent::Binary)));
    let device_verifier = Arc::clone(&verifier);
    let id = world.add_virtual("sink", "Byte sink", LinkConfig::default(), move || {
        Box::new(VerifyingSink(Arc::clone(&device_verifier)))
    });
    let session = open(&world, &id, BAUD);
    let link = world.link(&id).expect("link is up");
    let _unplug = UnplugOnPanic(link.clone());
    let events = session.events();
    expect_connected(&events);
    clock.settle(2);
    let t0 = clock.now();

    // Binary frames carry every byte value. They are queued all at once in 64 KiB
    // writes, as a file send would; the link takes each write like a large OS buffer, so
    // with the clock standing still they all start on the wire at `t0`, back to back.
    let mut payload = Vec::with_capacity(TOTAL);
    FirehoseGenerator::new(FirehoseContent::Binary, 7).fill(&mut payload, TOTAL);
    for piece in payload.chunks(64 * 1024) {
        session.write(piece.to_vec()).unwrap();
    }
    wait_for(
        Duration::from_secs(5),
        "the writer to hand over every write",
        || link.stats().host_to_device_bytes == TOTAL as u64,
    );
    assert_eq!(session.stats().tx_bytes, TOTAL as u64);
    clock.settle(2);

    // The device sees the host's bytes on the schedule the host sees the device's.
    for window in 1..=20u32 {
        let elapsed = 100 * MS * window;
        advance_to(&clock, t0 + elapsed, 20 * MS, 2);
        let got = verifier.lock().report().bytes;
        assert_eq!(got as usize, released(BAUD, elapsed), "after {elapsed:?}");
    }
    // The last byte finishes at 2 000 ms and lands one latency later.
    advance_to(&clock, t0 + 2_001 * MS, MS, 2);
    let report = verifier.lock().report().clone();
    assert_eq!(report.bytes, TOTAL as u64);
    assert!(report.is_clean(), "{report:?}");
    // Frames average about 140 bytes, so roughly 17 000 of them.
    assert!(report.records > 10_000, "{report:?}");
    let stats = link.stats();
    assert_eq!(stats.host_to_device_bytes, TOTAL as u64);
    assert_eq!(stats.dropped_bytes + stats.corrupted_bytes, 0, "{stats:?}");
    assert!(take_data(&events).is_empty(), "the sink sends nothing back");
    close_on(&clock, session, MS);
}

#[test]
fn firehose_32_mib_unpaced_loses_nothing() {
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
    let (bytes, error) =
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
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::with_clock(clock.clone());
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 1_000_000);
    let link = world.link(&id).expect("link is up");
    let _unplug = UnplugOnPanic(link.clone());
    let events = session.events();
    expect_connected(&events);
    clock.settle(2);
    let t0 = clock.now();

    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    advance_to(&clock, t0 + 60 * MS, 4 * MS, 2);
    let head = take_data(&events);
    assert_eq!(head.len(), released(1_000_000, 60 * MS));
    verifier.feed(&head);
    assert!(world.unplug(&id));
    // Disconnected arrives with the clock standing still. Everything released had
    // already been read; the look-ahead in flight is lost.
    let (tail, error) = drain_to_disconnect(&events, Some(&mut verifier), Duration::from_secs(5));
    assert!(
        matches!(error, Some(TransportError::Disconnected)),
        "{error:?}"
    );
    assert_eq!(tail, 0);
    assert_eq!(clock.now(), t0 + 60 * MS);

    // What arrived is gap-free, and every byte on the wire is accounted for.
    let stats = link.stats();
    assert_eq!(
        stats.device_to_host_bytes as usize,
        committed(1_000_000, 60 * MS)
    );
    assert_eq!(
        stats.lost_on_unplug as usize,
        committed(1_000_000, 60 * MS) - head.len()
    );
    assert_eq!(session.stats().rx_bytes, head.len() as u64);
    assert!(verifier.report().is_clean(), "{:?}", verifier.report());

    assert!(!session.is_connected());
    assert!(session.write(b"AT\r".to_vec()).is_err());
    assert!(session.set_control(ControlLine::Dtr, true).is_err());
    // Its reader has already stopped, so closing needs no time to pass.
    session.close();
    assert!(events.try_recv().is_err(), "nothing after Disconnected");

    // Gone from the factory until plugged back in; then it starts over from record 0.
    let cfg = SessionConfig::new(id.clone(), serial(1_000_000));
    assert!(matches!(
        Session::open(world.factory(), cfg.clone()),
        Err(TransportError::NotFound(_))
    ));
    assert!(world.plug(&id));
    let again = Session::open(world.factory(), cfg).unwrap();
    let _unplug_again = UnplugOnPanic(world.link(&id).expect("link is up again"));
    let events = again.events();
    expect_connected(&events);
    clock.settle(2);
    let t1 = clock.now();
    advance_to(&clock, t1 + 30 * MS, 4 * MS, 2);
    let fresh_data = take_data(&events);
    assert_eq!(fresh_data.len(), released(1_000_000, 30 * MS));
    let mut fresh = FirehoseVerifier::new(FirehoseContent::Text);
    fresh.feed(&fresh_data);
    assert!(fresh.report().is_clean() && fresh.report().records > 0);
    close_on(&clock, again, MS);
}

#[test]
fn unplug_at_9600_baud_disconnects_without_the_clock_moving() {
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::with_clock(clock.clone());
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 9_600);
    let link = world.link(&id).expect("link is up");
    let _unplug = UnplugOnPanic(link.clone());
    let events = session.events();
    expect_connected(&events);
    clock.settle(2);
    let t0 = clock.now();

    // At 960 bytes/s the firehose's 16 KiB batches are 17 s of data each; only one
    // look-ahead of it is on the wire at a time.
    advance_to(&clock, t0 + 200 * MS, 4 * MS, 2);
    let head = take_data(&events);
    assert_eq!(head.len(), released(9_600, 200 * MS));
    assert!(world.unplug(&id));
    let (tail, error) = drain_to_disconnect(&events, None, Duration::from_secs(5));
    assert!(matches!(error, Some(TransportError::Disconnected)));
    assert_eq!(tail, 0);
    assert_eq!(clock.now(), t0 + 200 * MS);
    assert_eq!(
        link.stats().lost_on_unplug as usize,
        committed(9_600, 200 * MS) - head.len()
    );
    session.close();
}

#[test]
fn idle_session_reader_reads_once_per_timeout() {
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::with_clock(clock.clone());
    let id = virtual_port_id(SimWorld::ECHO);
    let mut cfg = SessionConfig::new(id.clone(), serial(115_200));
    cfg.read_timeout = 10 * MS;
    let session = Session::open(world.factory(), cfg).unwrap();
    let link = world.link(&id).unwrap();
    let _unplug = UnplugOnPanic(link.clone());
    clock.settle(2);
    let before = link.stats().host_read_calls;

    // One second in 5 ms steps: the reader's read times out every 10 ms and it reads
    // again at once. Every step wakes the reader, so one that spun, or returned before
    // its timeout, would show 200 calls or more.
    advance_to(&clock, clock.now() + Duration::from_secs(1), 5 * MS, 2);
    assert_eq!(link.stats().host_read_calls - before, 100);
    assert_eq!(session.stats().rx_chunks, 0);

    // Nothing queued, so close only waits for the reader's current read to time out.
    close_on(&clock, session, 10 * MS);
}

#[test]
fn reconfigure_through_the_session_changes_the_link_rate() {
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::with_clock(clock.clone());
    let id = virtual_port_id(SimWorld::FIREHOSE);
    let session = open(&world, &id, 1_000_000);
    let link = world.link(&id).unwrap();
    let _unplug = UnplugOnPanic(link.clone());
    let events = session.events();
    expect_connected(&events);
    clock.settle(2);
    let t0 = clock.now();

    advance_to(&clock, t0 + 100 * MS, 4 * MS, 2);
    let mut received = take_data(&events).len();
    assert_eq!(received, released(1_000_000, 100 * MS));

    session.reconfigure(serial(500_000)).unwrap();
    // The writer thread applies it; the link sees it before the session does.
    wait_for(Duration::from_secs(5), "reconfigure to apply", || {
        session.serial_config().baud == 500_000
    });
    assert_eq!(link.link_config().serial.baud, 500_000);
    clock.settle(2);
    // Bytes already on the wire (one look-ahead at 1 Mbaud) keep the old rate; the new
    // rate follows them back to back.
    let before = link.stats().device_to_host_bytes as usize;
    assert_eq!(before, committed(1_000_000, 100 * MS));

    let mut per_window = Vec::new();
    for window in 2..=4u32 {
        let elapsed = 100 * MS * window;
        advance_to(&clock, t0 + elapsed, 4 * MS, 2);
        let got = take_data(&events).len();
        received += got;
        per_window.push(got);
        let want = released_switching(before, 1_000_000, 500_000, elapsed);
        assert_eq!(received, want, "after {elapsed:?}");
    }
    // 10 000 bytes per 100 ms before; 5 000 once the old look-ahead is out.
    assert_eq!(per_window, [6_650, 5_000, 5_000]);
    assert_eq!(session.config().serial.baud, 1_000_000);
    assert_eq!(session.port(), &id);
    close_on(&clock, session, MS);
}

#[test]
fn control_lines_reach_the_link() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let session = open(&world, &id, 115_200);
    let link = world.link(&id).unwrap();
    session.set_control(ControlLine::Dtr, false).unwrap();
    session.set_control(ControlLine::Rts, false).unwrap();
    wait_for(Duration::from_secs(2), "control lines to change", || {
        !link.control_line(ControlLine::Dtr) && !link.control_line(ControlLine::Rts)
    });
}
