//! The virtual link on its own, driven through the raw transport halves.

mod common;

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use proptest::prelude::*;
use serialist_core::{ControlLine, TransportError};
use serialist_sim::{
    EchoDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseVerifier, LinkConfig,
    VirtualLink,
};

use common::{
    BurstDevice, PanicDevice, RecorderDevice, Recording, read_exactly, read_to_disconnect, serial,
};

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
fn idle_read_times_out_after_about_the_timeout() {
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    let mut buf = [0u8; 64];
    for timeout_ms in [10u64, 50] {
        let timeout = Duration::from_millis(timeout_ms);
        let started = Instant::now();
        let before = link.stats().host_read_calls;
        assert_eq!(t.reader.read(&mut buf, timeout).unwrap(), 0);
        let took = started.elapsed();
        assert!(
            took >= timeout.mul_f64(0.5) && took <= timeout.mul_f64(1.5),
            "{timeout:?} timeout returned after {took:?}"
        );
        assert_eq!(link.stats().host_read_calls, before + 1);
    }
}

#[test]
fn idle_reader_loop_does_not_spin() {
    let (mut t, link) = VirtualLink::connect(Box::new(EchoDevice::new()), paced(115_200));
    let mut buf = [0u8; 64];
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(1) {
        assert_eq!(
            t.reader.read(&mut buf, Duration::from_millis(10)).unwrap(),
            0
        );
    }
    let calls = link.stats().host_read_calls;
    assert!(
        calls < 200,
        "{calls} read calls in one second at a 10 ms timeout"
    );
    assert!(calls >= 50, "{calls} read calls: timeouts are too long");
}

#[test]
fn read_returns_as_soon_as_a_byte_arrives() {
    let cfg = LinkConfig {
        latency: Duration::ZERO,
        ..LinkConfig::unpaced()
    };
    let (mut t, _link) = VirtualLink::connect(Box::new(EchoDevice::new()), cfg);
    let mut writer = t.writer;
    let sender = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        writer.write_all(b"x").unwrap();
        writer
    });
    let mut buf = [0u8; 16];
    let started = Instant::now();
    let n = t.reader.read(&mut buf, Duration::from_secs(5)).unwrap();
    let took = started.elapsed();
    assert_eq!(&buf[..n], b"x");
    assert!(took < Duration::from_millis(500), "read took {took:?}");
    drop(sender.join().unwrap());
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

/// Bytes read per 100 ms window, from a reader thread running for `run`.
fn windows(reader: &mut dyn serialist_core::TransportReader, run: Duration) -> Vec<usize> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut arrivals = Vec::new();
    let started = Instant::now();
    while started.elapsed() < run {
        let n = reader.read(&mut buf, Duration::from_millis(10)).unwrap();
        if n > 0 {
            arrivals.push((Instant::now(), n));
        }
    }
    let t0 = arrivals[0].0;
    let window = Duration::from_millis(100);
    let count = ((arrivals.last().unwrap().0 - t0).as_millis() / 100) as usize;
    let mut totals = vec![0usize; count];
    for (at, n) in arrivals {
        let k = ((at - t0).as_nanos() / window.as_nanos()) as usize;
        if k < count {
            totals[k] += n;
        }
    }
    totals
}

#[test]
fn paced_rate_is_accurate_over_100ms_windows() {
    // 1 Mbaud 8N1 = 100_000 bytes/s = 10_000 bytes per 100 ms window.
    let device = FirehoseDevice::new(FirehoseConfig::new(FirehoseContent::Text));
    let (mut t, _link) = VirtualLink::connect(Box::new(device), paced(1_000_000));
    let totals = windows(&mut *t.reader, Duration::from_millis(1250));
    assert!(totals.len() >= 10, "{totals:?}");
    // Skip the first window, which includes start-up.
    for (i, &bytes) in totals.iter().enumerate().skip(1) {
        let err = (bytes as f64 - 10_000.0).abs() / 10_000.0;
        assert!(
            err <= 0.05,
            "window {i}: {bytes} bytes, {:.1}% off; all: {totals:?}",
            err * 100.0
        );
    }
}

#[test]
fn reconfigure_changes_the_rate_live() {
    let device = FirehoseDevice::new(FirehoseConfig::new(FirehoseContent::Text));
    let (mut t, _link) = VirtualLink::connect(Box::new(device), paced(1_000_000));
    let fast: usize = windows(&mut *t.reader, Duration::from_millis(450))[1..]
        .iter()
        .sum();
    t.writer.reconfigure(&serial(500_000)).unwrap();
    // Let the 20 ms of look-ahead already scheduled at the old rate drain.
    let _ = windows(&mut *t.reader, Duration::from_millis(150));
    let slow: usize = windows(&mut *t.reader, Duration::from_millis(450))[1..]
        .iter()
        .sum();
    // Three 100 ms windows each: 30_000 bytes, then 15_000.
    let within = |got: usize, want: f64| (got as f64 - want).abs() / want <= 0.1;
    assert!(within(fast, 30_000.0), "fast phase: {fast}");
    assert!(within(slow, 15_000.0), "slow phase: {slow}");
    assert!(matches!(
        t.writer.reconfigure(&serial(0)),
        Err(TransportError::Config(_))
    ));
}

#[test]
fn host_to_device_is_paced_too() {
    let recording = Arc::new(Mutex::new(Recording::default()));
    let device = RecorderDevice(Arc::clone(&recording));
    // 100 kbaud 8N1 = 10_000 bytes/s, so 2_000 bytes take 200 ms on the wire.
    let (mut t, _link) = VirtualLink::connect(Box::new(device), paced(100_000));
    let sent_at = Instant::now();
    t.writer.write_all(&pattern(2_000)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while recording
        .lock()
        .chunks
        .iter()
        .map(|c| c.1.len())
        .sum::<usize>()
        < 2_000
    {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let rec = recording.lock();
    let spread = rec.chunks.last().unwrap().0 - sent_at;
    assert!(
        spread >= Duration::from_millis(190) && spread < Duration::from_millis(400),
        "2000 bytes at 10 kB/s arrived over {spread:?}"
    );
    // Delivered in wire packets (about 1 ms each), not one lump.
    assert!(rec.chunks.len() >= 100, "{} chunks", rec.chunks.len());
    let joined: Vec<u8> = rec.chunks.iter().flat_map(|c| c.1.clone()).collect();
    assert_eq!(joined, pattern(2_000));
}

#[test]
fn unplug_delivers_queued_bytes_then_disconnects() {
    let data = pattern(10_000);
    // 100 ms of data at 1 Mbaud, unplugged a fifth of the way through.
    let (mut t, link) =
        VirtualLink::connect(Box::new(BurstDevice::new(data.clone())), paced(1_000_000));
    thread::sleep(Duration::from_millis(20));
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
    let got = read_to_disconnect(&mut *t.reader, Duration::from_secs(5));
    assert_eq!(
        got, data,
        "every byte the device sent before the unplug arrives"
    );
    let mut buf = [0u8; 8];
    assert!(matches!(
        t.reader.read(&mut buf, Duration::from_millis(10)),
        Err(TransportError::Disconnected)
    ));
    assert!(link.wait_for_device_exit(Duration::from_secs(1)));
}

#[test]
fn device_disconnect_behaves_like_unplug() {
    let data = pattern(3_000);
    let device = BurstDevice::new(data.clone()).then_disconnect();
    let (mut t, link) = VirtualLink::connect(Box::new(device), paced(1_000_000));
    let got = read_to_disconnect(&mut *t.reader, Duration::from_secs(5));
    assert_eq!(got, data);
    assert!(link.is_unplugged());
    assert!(matches!(
        t.writer.write_all(b"x"),
        Err(TransportError::Disconnected)
    ));
    assert!(link.wait_for_device_exit(Duration::from_secs(1)));
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
        Box::new(RecorderDevice(Arc::clone(&recording))),
        paced(115_200),
    );
    assert!(link.control_line(ControlLine::Dtr));
    assert!(link.control_line(ControlLine::Rts));
    t.writer.set_control(ControlLine::Dtr, false).unwrap();
    t.writer.set_control(ControlLine::Rts, false).unwrap();
    t.writer.set_control(ControlLine::Rts, true).unwrap();
    assert!(!link.control_line(ControlLine::Dtr));
    assert!(link.control_line(ControlLine::Rts));
    let deadline = Instant::now() + Duration::from_secs(2);
    while recording.lock().controls.len() < 3 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
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
        max_chunk in 1usize..5000,
        latency_us in 0u64..3_000,
        jitter_us in 0u64..3_000,
        seed in any::<u64>(),
    ) {
        let cfg = LinkConfig {
            serial: serial(3_000_000),
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
