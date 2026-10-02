//! The virtual link on its own, driven through the raw transport halves.
//!
//! Timing is asserted exactly, on a [`ManualClock`]; `common` describes the pattern.
//! Tests with no timing assertions run in real time. Four smoke tests of the real-time
//! path (`system_clock_*`) keep loose wall-clock bounds so a regression there still shows
//! locally; they skip themselves under CI, where shared runners cannot keep time.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use proptest::prelude::*;
use serialist_core::{ControlLine, Transport, TransportError};
use serialist_sim::{
    EchoDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseVerifier,
    HOST_BACKLOG_LIMIT, LinkConfig, LinkHandle, ManualClock, SimDevice, VirtualLink,
};

use common::{
    BurstDevice, MS, PanicDevice, RecorderDevice, Recording, UnplugOnPanic, advance_to, committed,
    drain, read_exactly, read_to_disconnect, released, released_switching, serial, wait_for,
};

const NS: Duration = Duration::from_nanos(1);

/// Headroom for the wall-clock smoke tests: generous enough for a loaded machine.
const SMOKE_SLACK: Duration = Duration::from_millis(200);

fn paced(baud: u32) -> LinkConfig {
    LinkConfig {
        serial: serial(baud),
        ..LinkConfig::default()
    }
}

/// A 256-byte ramp repeated, so any position's expected value is known.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| i as u8).collect()
}

fn firehose(max_batch: usize) -> Box<FirehoseDevice> {
    Box::new(FirehoseDevice::new(FirehoseConfig {
        max_batch,
        ..FirehoseConfig::new(FirehoseContent::Text)
    }))
}

/// Connect `device` on a new manual clock and wait for its thread to go to sleep. The
/// clock has not moved since the link came up, so `clock.now()` is when it started.
fn on_manual_clock(
    device: Box<dyn SimDevice>,
    cfg: LinkConfig,
) -> (Arc<ManualClock>, Transport, LinkHandle) {
    let clock = Arc::new(ManualClock::new());
    let (t, link) = VirtualLink::connect_with_clock(device, cfg, clock.clone());
    clock.settle(1);
    (clock, t, link)
}

fn within(got: f64, want: f64, tolerance: f64) -> bool {
    (got - want).abs() / want <= tolerance
}

#[test]
fn echo_round_trip() {
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    assert_eq!(t.description, "virtual:echo @ 115200 8N1");
    t.writer.write_all(b"hello, device\r\n").unwrap();
    let back = read_exactly(&mut *t.reader, 15, 4096, Duration::from_secs(2));
    assert_eq!(back, b"hello, device\r\n");
    let stats = link.stats();
    assert_eq!(stats.host_to_device_bytes, 15);
    assert_eq!(stats.device_to_host_bytes, 15);
}

#[test]
fn idle_read_times_out_exactly_at_the_timeout() {
    let (clock, mut t, link) = on_manual_clock(Box::new(EchoDevice::new()), paced(115_200));
    for timeout in [10 * MS, 50 * MS] {
        let before = link.stats().host_read_calls;
        let started = clock.now();
        thread::scope(|s| {
            let _unplug = UnplugOnPanic(link.clone());
            // The read blocks until the clock passes its timeout, so it runs on a helper
            // thread while this one moves the clock.
            let read = s.spawn(|| {
                let mut buf = [0u8; 64];
                let n = t.reader.read(&mut buf, timeout).unwrap();
                (n, clock.now())
            });
            clock.settle(2);
            clock.advance(timeout - NS);
            // Woken by the move, the reader re-checked its deadline and went back to sleep.
            clock.settle(2);
            assert!(!read.is_finished(), "{timeout:?} read returned early");
            clock.advance(NS);
            assert_eq!(read.join().unwrap(), (0, started + timeout));
        });
        assert_eq!(link.stats().host_read_calls, before + 1);
    }
}

#[test]
fn idle_reader_loop_reads_once_per_timeout() {
    let (clock, mut t, link) = on_manual_clock(Box::new(EchoDevice::new()), paced(115_200));
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let _unplug = UnplugOnPanic(link.clone());
        s.spawn(|| {
            let mut buf = [0u8; 64];
            while !stop.load(Ordering::Acquire) {
                assert_eq!(t.reader.read(&mut buf, 10 * MS).unwrap(), 0);
            }
        });
        clock.settle(2);
        let before = link.stats().host_read_calls;
        // One second in 5 ms steps: each read times out at 10 ms and the next starts at
        // once. Every step wakes the reader, so one that spun, or returned before its
        // timeout, would show 200 calls or more.
        advance_to(&clock, clock.now() + Duration::from_secs(1), 5 * MS, 2);
        assert_eq!(link.stats().host_read_calls - before, 100);
        stop.store(true, Ordering::Release);
        clock.advance(10 * MS);
    });
}

#[test]
fn read_returns_as_soon_as_a_byte_arrives() {
    let cfg = LinkConfig {
        latency: Duration::ZERO,
        ..LinkConfig::unpaced()
    };
    let (clock, t, link) = on_manual_clock(Box::new(EchoDevice::new()), cfg);
    let Transport {
        mut reader,
        mut writer,
        ..
    } = t;
    let t0 = clock.now();
    thread::scope(|s| {
        let _unplug = UnplugOnPanic(link.clone());
        let read = s.spawn(|| {
            let mut buf = [0u8; 16];
            let n = reader.read(&mut buf, Duration::from_secs(5)).unwrap();
            (buf[..n].to_vec(), clock.now())
        });
        clock.settle(2);
        writer.write_all(b"x").unwrap();
        // Echoed and read with the clock standing still: the read did not wait for its
        // timeout.
        assert_eq!(read.join().unwrap(), (b"x".to_vec(), t0));
    });
}

#[test]
fn reads_never_exceed_max_chunk() {
    let data = pattern(10_000);
    let cfg = LinkConfig {
        max_chunk: 64,
        ..LinkConfig::unpaced()
    };
    let (mut t, _link) = VirtualLink::connect(Box::new(BurstDevice::new(data.clone())), cfg);
    let got = read_exactly(&mut *t.reader, data.len(), 64, Duration::from_secs(5));
    assert_eq!(got, data);
}

#[test]
fn paced_rate_matches_the_schedule_in_every_window() {
    // Steps of different lengths, all within the look-ahead's refill margin: when the
    // device thread happens to wake must not change what the host receives.
    for (baud, step) in [
        (115_200, 4 * MS),
        (9_600, 10 * MS),
        (1_000_000, 7 * MS),
        (3_000_000, 24 * MS),
        (12_000_000, 24 * MS),
        (12_000_000, 3 * MS),
    ] {
        let (clock, mut t, _link) = on_manual_clock(firehose(16 * 1024), paced(baud));
        let t0 = clock.now();
        let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
        let mut total = 0;
        for window in 1..=5u32 {
            let elapsed = 100 * MS * window;
            advance_to(&clock, t0 + elapsed, step, 1);
            let (got, disconnected) = drain(&mut *t.reader);
            assert!(!disconnected);
            verifier.feed(&got);
            total += got.len();
            assert_eq!(
                total,
                released(baud, elapsed),
                "{baud} baud after {elapsed:?}"
            );
            if baud == 115_200 {
                // 11 520 bytes/s in 12-byte packets: exactly 1 152 bytes per 100 ms,
                // less one packet for the 1 ms latency in the first window.
                assert_eq!(got.len(), if window == 1 { 1_140 } else { 1_152 });
            }
            if baud == 12_000_000 {
                // 1 200 000 bytes/s in 1 200-byte packets: exactly 120 000 bytes per
                // 100 ms, less one packet in the first window.
                assert_eq!(got.len(), if window == 1 { 118_800 } else { 120_000 });
            }
        }
        assert!(verifier.report().is_clean(), "{:?}", verifier.report());
    }
}

#[test]
fn reconfigure_changes_the_rate_on_the_next_advance() {
    let (clock, mut t, link) = on_manual_clock(firehose(16 * 1024), paced(1_000_000));
    let t0 = clock.now();
    advance_to(&clock, t0 + 100 * MS, 4 * MS, 1);
    let mut total = drain(&mut *t.reader).0.len();
    assert_eq!(total, released(1_000_000, 100 * MS));
    // At 1 Mbaud (100 bytes/ms) the wire is committed one look-ahead past now.
    let before = link.stats().device_to_host_bytes as usize;
    assert_eq!(before, committed(1_000_000, 100 * MS));
    assert_eq!(before, 13_200);

    t.writer.reconfigure(&serial(500_000)).unwrap();
    clock.settle(1);
    assert_eq!(link.stats().device_to_host_bytes as usize, before);
    // On the next advance the device thread commits at the new rate: 200 bytes for
    // 4 ms at 50 bytes/ms, where the old rate would have committed 400.
    advance_to(&clock, t0 + 104 * MS, 4 * MS, 1);
    assert_eq!(link.stats().device_to_host_bytes as usize - before, 200);

    // Bytes already on the wire keep the old rate; the new one follows them back to back.
    let mut per_window = Vec::new();
    for window in 2..=4u32 {
        let elapsed = 100 * MS * window;
        advance_to(&clock, t0 + elapsed, 4 * MS, 1);
        let got = drain(&mut *t.reader).0.len();
        total += got;
        per_window.push(got);
        let want = released_switching(before, 1_000_000, 500_000, elapsed);
        assert_eq!(total, want, "after {elapsed:?}");
    }
    // 10 000 bytes per 100 ms before; 5 000 once the old look-ahead is out.
    assert_eq!(per_window, [6_650, 5_000, 5_000]);

    assert!(matches!(
        t.writer.reconfigure(&serial(0)),
        Err(TransportError::Config(_))
    ));
}

#[test]
fn one_callback_commits_at_most_one_lookahead_to_the_wire() {
    // 9600 baud 8N1 = 960 bytes/s. The firehose hands the link 16 KiB (17 s of data) in
    // one tick; only one look-ahead of it may be on the wire at a time.
    let (clock, mut t, link) = on_manual_clock(firehose(16 * 1024), paced(9_600));
    let t0 = clock.now();
    // 32 ms at 960 bytes/s is 30.72 bytes; the rest waits in the device's FIFO, which
    // still counts as queued.
    assert_eq!(link.stats().device_to_host_bytes, 30);
    assert_eq!(link.queued_to_host(), 16 * 1024);
    let mut read = 0;
    for ms in 1..=100 {
        let elapsed = MS * ms;
        advance_to(&clock, t0 + elapsed, MS, 1);
        read += drain(&mut *t.reader).0.len();
        let stats = link.stats();
        assert_eq!(
            stats.device_to_host_bytes as usize,
            committed(9_600, elapsed),
            "after {elapsed:?}"
        );
        assert_eq!(read, released(9_600, elapsed), "after {elapsed:?}");
        assert_eq!(link.queued_to_host(), 16 * 1024 - read);
    }
    assert_eq!(read, 95);
}

#[test]
fn queued_to_host_stays_bounded_when_the_host_stops_reading() {
    let batch = 64 * 1024;
    // Unpaced and with no latency, the device runs without the clock moving, until the
    // backlog limit holds it back.
    let (clock, mut t, link) = on_manual_clock(firehose(batch), LinkConfig::unpaced());
    assert_eq!(link.queued_to_host(), HOST_BACKLOG_LIMIT);
    assert_eq!(link.stats().device_to_host_bytes, HOST_BACKLOG_LIMIT as u64);

    // Reading it below half the limit lets the device run again. It tops the wire back
    // up to the limit; the rest of its last batch waits in its FIFO.
    let mut read = 0;
    let mut buf = vec![0u8; 64 * 1024];
    while read <= HOST_BACKLOG_LIMIT / 2 {
        read += t.reader.read(&mut buf, Duration::ZERO).unwrap();
    }
    clock.settle(1);
    let sent = read.div_ceil(batch) * batch;
    assert_eq!(
        link.stats().device_to_host_bytes,
        (HOST_BACKLOG_LIMIT + read) as u64
    );
    assert_eq!(link.queued_to_host(), HOST_BACKLOG_LIMIT - read + sent);
}

#[test]
fn host_to_device_is_paced_too() {
    let clock = Arc::new(ManualClock::new());
    let recording = Arc::new(Mutex::new(Recording::default()));
    let device = RecorderDevice::on_clock(Arc::clone(&recording), clock.clone());
    // 100 kbaud 8N1 = 10 000 bytes/s: a 10-byte packet every millisecond.
    let (mut t, _link) =
        VirtualLink::connect_with_clock(Box::new(device), paced(100_000), clock.clone());
    clock.settle(1);
    let t0 = clock.now();
    t.writer.write_all(&pattern(2_000)).unwrap();
    clock.settle(1);
    advance_to(&clock, t0 + 250 * MS, MS, 1);

    let rec = recording.lock();
    assert_eq!(rec.chunks.len(), 200);
    for (i, (at, chunk)) in rec.chunks.iter().enumerate() {
        // Packet i finishes at i + 1 ms and reaches the device 1 ms later.
        assert_eq!(*at - t0, MS * (i as u32 + 2), "packet {i}");
        assert_eq!(chunk.len(), 10, "packet {i}");
    }
    let joined: Vec<u8> = rec.chunks.iter().flat_map(|c| c.1.clone()).collect();
    assert_eq!(joined, pattern(2_000));
}

#[test]
fn unplug_keeps_released_bytes_and_drops_the_rest() {
    let data = pattern(10_000);
    // 100 ms of data at 1 Mbaud, unplugged a fifth of the way through.
    let (clock, mut t, link) =
        on_manual_clock(Box::new(BurstDevice::new(data.clone())), paced(1_000_000));
    let t0 = clock.now();
    advance_to(&clock, t0 + 20 * MS, 4 * MS, 1);
    link.unplug();
    assert!(link.is_unplugged());
    assert!(matches!(
        t.writer.write_all(b"x"),
        Err(TransportError::Disconnected)
    ));
    assert!(matches!(
        t.writer.set_control(ControlLine::Dtr, false),
        Err(TransportError::Disconnected)
    ));

    // With the clock standing still: what was released, then Disconnected.
    let (got, disconnected) = drain(&mut *t.reader);
    assert!(disconnected);
    assert_eq!(got.len(), released(1_000_000, 20 * MS));
    assert_eq!(got, data[..got.len()]);
    // One look-ahead was on the wire; what had not been released is lost. What was
    // still in the device's FIFO is not counted anywhere.
    let stats = link.stats();
    assert_eq!(
        stats.device_to_host_bytes as usize,
        committed(1_000_000, 20 * MS)
    );
    assert_eq!(stats.lost_on_unplug, 5_200 - 1_900);
    assert_eq!(
        got.len() as u64,
        stats.device_to_host_bytes - stats.lost_on_unplug
    );
    let mut buf = [0u8; 8];
    assert!(matches!(
        t.reader.read(&mut buf, 10 * MS),
        Err(TransportError::Disconnected)
    ));
    wait_for(Duration::from_secs(5), "device thread exit", || {
        !link.is_device_running()
    });
}

#[test]
fn unplug_loses_the_lookahead_in_flight_and_disconnects_at_once() {
    for baud in [9_600, 115_200] {
        let (clock, mut t, link) = on_manual_clock(firehose(16 * 1024), paced(baud));
        let t0 = clock.now();
        advance_to(&clock, t0 + 200 * MS, 4 * MS, 1);
        let committed_bytes = link.stats().device_to_host_bytes as usize;
        assert_eq!(committed_bytes, committed(baud, 200 * MS));
        // At 9600 baud one 16 KiB batch is 17 s of data. Pulling the cable does not wait
        // for it: the host gets what was released, then Disconnected, with no more time
        // passing.
        link.unplug();
        let (got, disconnected) = drain(&mut *t.reader);
        assert!(disconnected, "{baud} baud");
        assert_eq!(got.len(), released(baud, 200 * MS), "{baud} baud");
        assert_eq!(
            link.stats().lost_on_unplug as usize,
            committed_bytes - got.len(),
            "{baud} baud"
        );
        let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
        verifier.feed(&got);
        assert!(verifier.report().is_clean(), "{:?}", verifier.report());
    }
}

#[test]
fn device_disconnect_drains_then_disconnects() {
    let data = pattern(3_000);
    let device = BurstDevice::new(data.clone()).then_disconnect();
    let clock = Arc::new(ManualClock::new());
    let (mut t, link) =
        VirtualLink::connect_with_clock(Box::new(device), paced(1_000_000), clock.clone());
    let t0 = clock.now();
    // 3 000 bytes fit in one look-ahead, so the device's FIFO empties at once and its
    // thread finishes hanging up without the clock moving.
    wait_for(Duration::from_secs(5), "device thread exit", || {
        !link.is_device_running()
    });
    // The device is going away: host writes fail at once...
    assert!(matches!(
        t.writer.write_all(b"x"),
        Err(TransportError::Disconnected)
    ));
    // ...but everything it sent still arrives at wire pace. The last packet finishes at
    // 30 ms and is released at 31 ms; Disconnected comes right after it.
    clock.set(t0 + 31 * MS - NS);
    let (got, disconnected) = drain(&mut *t.reader);
    assert!(!disconnected);
    assert_eq!(got, data[..2_900]);
    clock.advance(NS);
    let (rest, disconnected) = drain(&mut *t.reader);
    assert!(disconnected);
    assert_eq!(rest, data[2_900..]);
    assert!(link.is_unplugged());
    assert_eq!(link.stats().lost_on_unplug, 0);
}

#[test]
fn device_thread_exits_when_the_host_drops_both_halves() {
    let (t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    assert!(link.is_device_running());
    let serialist_core::Transport { reader, writer, .. } = t;
    drop(reader);
    assert!(!link.wait_for_device_exit(Duration::from_millis(50)));
    drop(writer);
    assert!(link.wait_for_device_exit(Duration::from_secs(1)));
}

#[test]
fn device_panic_unplugs_the_link() {
    let (mut t, link) = VirtualLink::connect(Box::new(PanicDevice), LinkConfig::unpaced());
    t.writer.write_all(b"boom").unwrap();
    let got = read_to_disconnect(&mut *t.reader, Duration::from_secs(5));
    assert!(got.is_empty());
    assert!(link.wait_for_device_exit(Duration::from_secs(1)));
}

#[test]
fn control_lines_are_tracked_and_reach_the_device() {
    let recording = Arc::new(Mutex::new(Recording::default()));
    let (mut t, link) = VirtualLink::connect(
        Box::new(RecorderDevice::new(Arc::clone(&recording))),
        paced(115_200),
    );
    assert!(link.control_line(ControlLine::Dtr));
    assert!(link.control_line(ControlLine::Rts));
    t.writer.set_control(ControlLine::Dtr, false).unwrap();
    t.writer.set_control(ControlLine::Rts, false).unwrap();
    t.writer.set_control(ControlLine::Rts, true).unwrap();
    assert!(!link.control_line(ControlLine::Dtr));
    assert!(link.control_line(ControlLine::Rts));
    wait_for(Duration::from_secs(2), "control changes", || {
        recording.lock().controls.len() >= 3
    });
    assert_eq!(
        recording.lock().controls,
        [
            (ControlLine::Dtr, false),
            (ControlLine::Rts, false),
            (ControlLine::Rts, true)
        ]
    );
}

#[test]
fn set_link_config_applies_live() {
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), LinkConfig::unpaced());
    link.set_link_config(LinkConfig {
        max_chunk: 3,
        ..LinkConfig::unpaced()
    });
    assert_eq!(link.link_config().max_chunk, 3);
    t.writer.write_all(b"abcdefgh").unwrap();
    let got = read_exactly(&mut *t.reader, 8, 3, Duration::from_secs(2));
    assert_eq!(got, b"abcdefgh");
}

/// Send `n` known bytes through a faulty link and return what arrived plus the counters.
fn faulty_run(
    drop_p: f64,
    corrupt_p: f64,
    seed: u64,
    n: usize,
) -> (Vec<u8>, serialist_sim::LinkStats) {
    let cfg = LinkConfig {
        drop_probability: drop_p,
        corrupt_probability: corrupt_p,
        seed,
        ..LinkConfig::unpaced()
    };
    let device = BurstDevice::new(pattern(n)).then_disconnect();
    let (mut t, link) = VirtualLink::connect(Box::new(device), cfg);
    let got = read_to_disconnect(&mut *t.reader, Duration::from_secs(10));
    (got, link.stats())
}

#[test]
fn drop_counter_matches_observed_loss() {
    let n = 100_000;
    let (got, stats) = faulty_run(0.02, 0.0, 1, n);
    assert_eq!(stats.device_to_host_bytes, n as u64);
    assert_eq!(got.len() as u64, n as u64 - stats.dropped_bytes);
    assert_eq!(stats.corrupted_bytes, 0);
    // 2% of 100_000 is 2_000; allow generous sampling noise.
    assert!((1_500..2_500).contains(&stats.dropped_bytes), "{stats:?}");
}

#[test]
fn corrupt_counter_matches_observed_damage() {
    let n = 100_000;
    let (got, stats) = faulty_run(0.0, 0.02, 2, n);
    assert_eq!(got.len(), n);
    let differing = got
        .iter()
        .zip(pattern(n))
        .filter(|(a, b)| **a != *b)
        .count();
    assert_eq!(differing as u64, stats.corrupted_bytes);
    assert_eq!(stats.dropped_bytes, 0);
    assert!((1_500..2_500).contains(&stats.corrupted_bytes), "{stats:?}");
}

#[test]
fn faults_are_deterministic_per_seed() {
    let a = faulty_run(0.01, 0.01, 7, 20_000);
    let b = faulty_run(0.01, 0.01, 7, 20_000);
    let c = faulty_run(0.01, 0.01, 8, 20_000);
    assert_eq!(a, b);
    assert_ne!(a.0, c.0);
    assert_eq!(a.0.len() as u64, 20_000 - a.1.dropped_bytes);
}

#[test]
fn firehose_through_a_raw_link_verifies() {
    let device = FirehoseDevice::new(
        FirehoseConfig::new(FirehoseContent::Binary).with_total(2 * 1024 * 1024),
    );
    let (mut t, _link) = VirtualLink::connect(Box::new(device), LinkConfig::unpaced());
    let got = read_exactly(
        &mut *t.reader,
        2 * 1024 * 1024,
        4096,
        Duration::from_secs(20),
    );
    let mut verifier = FirehoseVerifier::new(FirehoseContent::Binary);
    verifier.feed(&got);
    assert!(verifier.report().is_clean(), "{:?}", verifier.report());
}

// Smoke tests of the real-time path: `SystemClock` waits and real pacing. Loose bounds,
// skipped under CI.

#[test]
fn system_clock_read_timeout_smoke() {
    skip_unless_wall_clock_timing!();
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    let mut buf = [0u8; 64];
    for timeout in [10 * MS, 50 * MS] {
        let started = Instant::now();
        assert_eq!(t.reader.read(&mut buf, timeout).unwrap(), 0);
        let took = started.elapsed();
        assert!(
            took >= timeout && took <= timeout + SMOKE_SLACK,
            "{timeout:?} timeout returned after {took:?}"
        );
    }
    assert_eq!(link.stats().host_read_calls, 2);
}

#[test]
fn system_clock_idle_reader_smoke() {
    skip_unless_wall_clock_timing!();
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    let mut buf = [0u8; 64];
    let started = Instant::now();
    while started.elapsed() < Duration::from_millis(500) {
        assert_eq!(t.reader.read(&mut buf, 10 * MS).unwrap(), 0);
    }
    // About 50 at a 10 ms timeout. A spinning reader makes thousands.
    let calls = link.stats().host_read_calls;
    assert!((3..=100).contains(&calls), "{calls} reads in 500 ms");
}

#[test]
fn system_clock_paced_rate_smoke() {
    skip_unless_wall_clock_timing!();
    // 1 Mbaud 8N1 is 100 000 bytes/s.
    let (mut t, _link) = VirtualLink::connect(firehose(16 * 1024), paced(1_000_000));
    let mut buf = vec![0u8; 64 * 1024];
    let mut bytes = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        bytes += t.reader.read(&mut buf, 10 * MS).unwrap();
    }
    let rate = bytes as f64 / started.elapsed().as_secs_f64();
    assert!(within(rate, 100_000.0, 0.25), "{rate:.0} B/s over 2 s");
}

#[test]
fn system_clock_paced_rate_at_12_mbaud_smoke() {
    skip_unless_wall_clock_timing!();
    // 12 Mbaud 8N1 is 1 200 000 bytes/s: the device thread has to wake on time on a
    // real clock to keep the wire busy, and the reader to keep up, with every byte
    // checked on the way.
    let (mut t, link) = VirtualLink::connect(firehose(16 * 1024), paced(12_000_000));
    let mut verifier = FirehoseVerifier::new(FirehoseContent::Text);
    let mut buf = vec![0u8; 64 * 1024];
    let mut bytes = 0;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(2) {
        let n = t.reader.read(&mut buf, 10 * MS).unwrap();
        verifier.feed(&buf[..n]);
        bytes += n;
    }
    let rate = bytes as f64 / started.elapsed().as_secs_f64();
    assert!(within(rate, 1_200_000.0, 0.25), "{rate:.0} B/s over 2 s");
    assert!(verifier.report().is_clean(), "{:?}", verifier.report());
    assert_eq!(link.stats().dropped_bytes, 0);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// With no faults, any bytes written in any chunking come back identical and in
    /// order, whatever the chunk limit, latency, jitter and pacing.
    #[test]
    fn echo_preserves_bytes_and_order(
        writes in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..600), 1..8),
        max_chunk in 1usize..700,
        latency_us in 0u64..3_000,
        jitter_us in 0u64..3_000,
        pace in any::<bool>(),
        seed in any::<u64>(),
    ) {
        let cfg = LinkConfig {
            serial: serial(2_000_000),
            pace_to_baud: pace,
            max_chunk,
            latency: Duration::from_micros(latency_us),
            jitter: Duration::from_micros(jitter_us),
            drop_probability: 0.0,
            corrupt_probability: 0.0,
            seed,
        };
        let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), cfg);
        for w in &writes {
            t.writer.write_all(w).unwrap();
        }
        let expected: Vec<u8> = writes.concat();
        let got = read_exactly(&mut *t.reader, expected.len(), max_chunk, Duration::from_secs(10));
        prop_assert_eq!(got, expected);
        let stats = link.stats();
        prop_assert_eq!(stats.dropped_bytes + stats.corrupted_bytes, 0);
    }

    /// The same for bytes the device originates, which take the device-to-host path only.
    #[test]
    fn device_output_preserves_bytes_and_order(
        data in prop::collection::vec(any::<u8>(), 0..4000),
        baud in prop::sample::select(vec![3_000_000u32, 12_000_000]),
        max_chunk in 1usize..5000,
        latency_us in 0u64..3_000,
        jitter_us in 0u64..3_000,
        seed in any::<u64>(),
    ) {
        let cfg = LinkConfig {
            serial: serial(baud),
            max_chunk,
            latency: Duration::from_micros(latency_us),
            jitter: Duration::from_micros(jitter_us),
            seed,
            ..LinkConfig::default()
        };
        let device = BurstDevice::new(data.clone()).then_disconnect();
        let (mut t, _link) = VirtualLink::connect(Box::new(device), cfg);
        let got = read_to_disconnect(&mut *t.reader, Duration::from_secs(10));
        prop_assert_eq!(got, data);
    }
}
