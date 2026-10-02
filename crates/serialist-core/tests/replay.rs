//! The file-replay transport against captures on disk, paced on a `ManualClock` so every
//! instant is exact, plus one `Session` run end to end and one real-time idle check.
//!
//! Single-threaded tests read with a zero timeout, a non-blocking probe the `ManualClock`
//! docs allow, and move the clock between reads. The session test lets the session's
//! reader thread do the reading and settles it with `ManualClock::settle`. Nothing here
//! needs hardware: the captures are temp files.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serialist_core::{
    ControlLine, PortId, REPLAY_SCHEME, ReplayEnd, ReplayOptions, ReplaySpeed,
    ReplayTransportFactory, RoutingTransportFactory, SerialConfig, SerialportFactory, Session,
    SessionConfig, SessionEvent, Timing, TimingWriter, Transport, TransportError, TransportFactory,
    timing_path,
};
use serialist_sim::ManualClock;

const MS: Duration = Duration::from_millis(1);
const NS: Duration = Duration::from_nanos(1);

/// The capture most tests play: three chunks, 10 ms and 50 ms after the first.
const CHUNKS: [(Duration, &[u8]); 3] = [
    (Duration::ZERO, b"abc"),
    (Duration::from_millis(10), b"defg"),
    (Duration::from_millis(50), b"hijkl"),
];

/// A directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "serialist-test-{label}-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, bytes).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The text of a sidecar with an `rx` record for each `(arrival, length)`, arrivals
/// counted from the recording's origin.
fn sidecar(chunks: &[(Duration, usize)]) -> Vec<u8> {
    let origin = Instant::now();
    let mut writer = TimingWriter::new(Vec::new(), origin).expect("write the header");
    for (at, len) in chunks {
        writer.rx(origin + *at, *len).expect("write a record");
    }
    writer.into_inner()
}

/// A capture on disk: the raw file and, for [`Capture::timed`], its sidecar.
struct Capture {
    _dir: TempDir,
    path: PathBuf,
    /// What the raw file holds.
    raw: Vec<u8>,
}

impl Capture {
    /// `name` holding the chunks back to back, with a sidecar that times each one.
    fn timed(name: &str, chunks: &[(Duration, &[u8])]) -> Self {
        let raw: Vec<u8> = chunks
            .iter()
            .flat_map(|(_, bytes)| bytes.to_vec())
            .collect();
        let lens: Vec<_> = chunks
            .iter()
            .map(|(at, bytes)| (*at, bytes.len()))
            .collect();
        let capture = Self::untimed(name, &raw);
        fs::write(timing_path(&capture.path), sidecar(&lens)).expect("write the sidecar");
        capture
    }

    /// `name` holding `raw`, with no sidecar.
    fn untimed(name: &str, raw: &[u8]) -> Self {
        let dir = TempDir::new("replay");
        let path = dir.write(name, raw);
        Self {
            _dir: dir,
            path,
            raw: raw.to_vec(),
        }
    }

    /// `cap.bin` with the three standard chunks and a sidecar.
    fn standard() -> Self {
        Self::timed("cap.bin", &CHUNKS)
    }

    /// The id that replays this capture, with `query` (`"?speed=4x"`, or nothing) after it.
    fn id(&self, query: &str) -> PortId {
        PortId::new(format!("replay:{}{query}", self.path.display()))
    }
}

/// A replay factory on a manual clock.
struct Rig {
    clock: Arc<ManualClock>,
    factory: ReplayTransportFactory,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new());
        let factory = ReplayTransportFactory::with_clock(Arc::clone(&clock) as _);
        Self { clock, factory }
    }

    fn open(&self, id: &PortId) -> Transport {
        self.factory
            .open(id, &SerialConfig::default())
            .unwrap_or_else(|err| panic!("{id} should open: {err}"))
    }

    fn open_err(&self, id: &PortId) -> TransportError {
        match self.factory.open(id, &SerialConfig::default()) {
            Err(err) => err,
            Ok(transport) => panic!("{id} should not open, but did as {}", transport.description),
        }
    }
}

/// A non-blocking probe: what is due right now, in a buffer of `len` bytes.
fn try_read(transport: &mut Transport, len: usize) -> Result<Vec<u8>, TransportError> {
    let mut buf = vec![0u8; len];
    let n = transport.reader.read(&mut buf, Duration::ZERO)?;
    buf.truncate(n);
    Ok(buf)
}

/// What is due right now, which must be a success.
fn read(transport: &mut Transport, len: usize) -> Vec<u8> {
    try_read(transport, len).expect("a probe succeeds")
}

fn assert_disconnected(result: Result<Vec<u8>, TransportError>) {
    assert!(
        matches!(result, Err(TransportError::Disconnected)),
        "expected Disconnected, got {result:?}"
    );
}

/// Plays `id` and checks that each of [`CHUNKS`] arrives, whole and alone, at exactly
/// `due` after the open, and not a nanosecond before.
fn assert_delivers_at(rig: &Rig, id: &PortId, due: [Duration; 3]) {
    let mut transport = rig.open(id);
    let opened = rig.clock.now();
    let mut elapsed = Duration::ZERO;
    for (due, (_, expected)) in due.into_iter().zip(CHUNKS) {
        if due > elapsed {
            rig.clock.advance(due - elapsed - NS);
            assert_eq!(read(&mut transport, 64), b"", "nothing before {due:?}");
            rig.clock.advance(NS);
            elapsed = due;
        }
        assert_eq!(rig.clock.now() - opened, due);
        assert_eq!(
            read(&mut transport, 64),
            expected,
            "the chunk due at {due:?}"
        );
    }
    assert_disconnected(try_read(&mut transport, 64));
}

#[test]
fn sidecar_pacing_at_1x_is_exact() {
    let cap = Capture::standard();
    let rig = Rig::new();
    let mut transport = rig.open(&cap.id(""));

    assert_eq!(read(&mut transport, 64), b"abc");
    assert_eq!(read(&mut transport, 64), b"");

    rig.clock.advance(10 * MS - NS);
    assert_eq!(read(&mut transport, 64), b"");
    rig.clock.advance(NS);
    assert_eq!(read(&mut transport, 64), b"defg");
    assert_eq!(read(&mut transport, 64), b"");

    rig.clock.advance(40 * MS);
    assert_eq!(read(&mut transport, 64), b"hijkl");
    assert_disconnected(try_read(&mut transport, 64));
}

#[test]
fn speed_scales_the_schedule() {
    let cap = Capture::standard();
    let rig = Rig::new();
    assert_delivers_at(
        &rig,
        &cap.id("?speed=4x"),
        [
            Duration::ZERO,
            Duration::from_micros(2_500),
            Duration::from_micros(12_500),
        ],
    );
    assert_delivers_at(
        &rig,
        &cap.id("?speed=0.5x"),
        [Duration::ZERO, 20 * MS, 100 * MS],
    );
}

#[test]
fn max_speed_ignores_time() {
    let cap = Capture::standard();
    let rig = Rig::new();
    let opened = rig.clock.now();
    let mut transport = rig.open(&cap.id("?speed=max"));

    for (_, expected) in CHUNKS {
        assert_eq!(read(&mut transport, 64), expected, "one chunk per read");
    }
    assert_disconnected(try_read(&mut transport, 64));
    assert_eq!(rig.clock.now(), opened, "the clock never moved");
}

#[test]
fn factory_defaults_apply_when_the_id_has_none() {
    let cap = Capture::standard();
    let rig = Rig::new();
    rig.factory.set_defaults(ReplayOptions {
        speed: ReplaySpeed::Max,
        ..ReplayOptions::default()
    });

    // A bare id plays at the default, which here is `max`.
    let mut transport = rig.open(&cap.id(""));
    assert_eq!(transport.description, "replay:cap.bin (max)");
    for (_, expected) in CHUNKS {
        assert_eq!(read(&mut transport, 64), expected);
    }
    assert_disconnected(try_read(&mut transport, 64));

    // The id's own speed wins: the second chunk now waits its 10 ms.
    let mut transport = rig.open(&cap.id("?speed=1x"));
    assert_eq!(transport.description, "replay:cap.bin (1x)");
    assert_eq!(read(&mut transport, 64), b"abc");
    assert_eq!(read(&mut transport, 64), b"");
    rig.clock.advance(10 * MS);
    assert_eq!(read(&mut transport, 64), b"defg");

    // A default for the end applies the same way.
    rig.factory.set_defaults(ReplayOptions {
        speed: ReplaySpeed::Max,
        end: ReplayEnd::Hold,
    });
    let mut transport = rig.open(&cap.id(""));
    for _ in CHUNKS {
        read(&mut transport, 64);
    }
    assert_eq!(read(&mut transport, 64), b"", "held open, not disconnected");
}

#[test]
fn chunk_boundaries_survive_a_small_buffer() {
    // Both chunks are due at once, so only the chunk boundary keeps them apart.
    let cap = Capture::timed(
        "cap.bin",
        &[(Duration::ZERO, b"0123456789"), (Duration::ZERO, b"xyz")],
    );
    let rig = Rig::new();
    let mut transport = rig.open(&cap.id(""));

    assert_eq!(read(&mut transport, 4), b"0123");
    assert_eq!(read(&mut transport, 4), b"4567");
    assert_eq!(
        read(&mut transport, 4),
        b"89",
        "the rest of the chunk, alone"
    );
    assert_eq!(read(&mut transport, 4), b"xyz");
    assert_disconnected(try_read(&mut transport, 4));
}

#[test]
fn no_sidecar_paces_at_the_baud() {
    let raw: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let cap = Capture::untimed("dump.bin", &raw);
    let rig = Rig::new();
    let serial = SerialConfig {
        baud: 9600,
        ..SerialConfig::default()
    };
    // 9600 8N1 is 960 bytes a second; a batch is what 10 ms earns, 9 bytes.
    let mut transport = rig
        .factory
        .open(&cap.id(""), &serial)
        .expect("a capture without a sidecar opens");
    assert_eq!(transport.description, "replay:dump.bin (1x, no timing)");
    assert_eq!(
        read(&mut transport, 40),
        b"",
        "nothing is earned at the open"
    );

    rig.clock.advance(100 * MS);
    let mut reads = Vec::new();
    loop {
        let chunk = read(&mut transport, 40);
        if chunk.is_empty() {
            break;
        }
        reads.push(chunk);
    }
    let lens: Vec<usize> = reads.iter().map(Vec::len).collect();
    assert_eq!(lens.iter().sum::<usize>(), 96, "exactly what 100 ms earns");
    let (last, others) = lens.split_last().expect("some reads");
    assert!(
        others.iter().all(|&n| n >= 9),
        "full batches only: {lens:?}"
    );
    assert!(*last >= 9, "{lens:?}");
    assert_eq!(
        lens,
        [40, 40, 16],
        "never more than the buffer or the budget"
    );
    assert_eq!(reads.concat(), raw[..96]);

    // Doubling the baud at 100 ms keeps the 96 bytes earned so far and earns the next
    // 100 ms at twice the rate. The reader only looks after the 100 ms have passed, so
    // this proves the change counts from the writer's instant, not the reader's.
    transport
        .writer
        .reconfigure(&SerialConfig {
            baud: 19_200,
            ..serial
        })
        .unwrap();
    rig.clock.advance(100 * MS);
    let next = read(&mut transport, 4096);
    assert_eq!(next.len(), 192, "100 ms at 1920 bytes a second");
    assert_eq!(next, raw[96..288]);
    assert_eq!(read(&mut transport, 4096), b"", "and no more than that");
}

#[test]
fn no_sidecar_never_runs_ahead_of_the_line() {
    let raw: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
    let cap = Capture::untimed("dump.bin", &raw);
    let rig = Rig::new();
    let serial = SerialConfig {
        baud: 9600,
        ..SerialConfig::default()
    };
    let mut transport = rig.factory.open(&cap.id(""), &serial).unwrap();

    // A reader that looks every millisecond gets a batch of 9 whenever one is earned.
    let mut got = Vec::new();
    for ms in 1..=100u64 {
        rig.clock.advance(MS);
        let chunk = read(&mut transport, 4096);
        got.extend_from_slice(&chunk);
        assert!(
            chunk.is_empty() || chunk.len() >= 9,
            "{ms} ms: a read of {} bytes is less than a batch",
            chunk.len()
        );
        let earned = ms * 96 / 100;
        assert!(
            got.len() as u64 <= earned,
            "{ms} ms: {} bytes read, only {earned} earned",
            got.len()
        );
    }
    assert_eq!(got, raw[..got.len()]);
    assert!(got.len() >= 96 - 9, "{} bytes in 100 ms", got.len());
}

#[test]
fn no_sidecar_at_max_fills_the_buffer() {
    let raw: Vec<u8> = (0..100 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let cap = Capture::untimed("big.bin", &raw);
    let rig = Rig::new();
    let mut transport = rig.open(&cap.id("?speed=max"));

    let mut got = Vec::new();
    let mut lens = Vec::new();
    loop {
        match try_read(&mut transport, 10_000) {
            Ok(chunk) => {
                lens.push(chunk.len());
                got.extend(chunk);
            }
            Err(TransportError::Disconnected) => break,
            Err(other) => panic!("{other}"),
        }
    }
    assert_eq!(got, raw);
    assert_eq!(lens.len(), 11);
    assert!(lens[..10].iter().all(|&n| n == 10_000), "{lens:?}");
    assert_eq!(lens[10], 102_400 - 100_000);
}

#[test]
fn the_end_disconnects_or_holds() {
    let cap = Capture::standard();
    let rig = Rig::new();

    let mut transport = rig.open(&cap.id("?speed=max&end=disconnect"));
    for (_, expected) in CHUNKS {
        assert_eq!(read(&mut transport, 64), expected);
    }
    assert_disconnected(try_read(&mut transport, 64));
    assert_disconnected(try_read(&mut transport, 64));

    let mut transport = rig.open(&cap.id("?speed=max&end=hold"));
    for (_, expected) in CHUNKS {
        assert_eq!(read(&mut transport, 64), expected);
    }
    for _ in 0..3 {
        assert_eq!(read(&mut transport, 64), b"", "silent, not disconnected");
    }

    // A read that is allowed to wait waits out its timeout on the clock, and no longer.
    let Transport { mut reader, .. } = transport;
    let waiting = thread::spawn(move || {
        let mut buf = [0u8; 64];
        let result = reader.read(&mut buf, 20 * MS);
        (reader, result)
    });
    rig.clock.settle(1);
    assert!(!waiting.is_finished());
    rig.clock.advance(19 * MS);
    rig.clock.settle(1);
    assert!(!waiting.is_finished(), "19 ms of a 20 ms timeout");
    rig.clock.advance(MS);
    let (_reader, result) = waiting.join().unwrap();
    assert!(matches!(result, Ok(0)), "{result:?}");
}

#[test]
fn open_errors() {
    let rig = Rig::new();
    let dir = TempDir::new("replay-open-errors");

    // No raw file: NotFound, naming the id.
    let missing = PortId::new(format!("replay:{}", dir.path().join("nope.bin").display()));
    match rig.open_err(&missing) {
        TransportError::NotFound(id) => assert_eq!(id, missing),
        other => panic!("expected NotFound, got {other:?}"),
    }

    // A sidecar that is not one: Config, naming the id.
    let raw = dir.write("boot.bin", b"abc");
    let bad = PortId::new(format!("replay:{}", raw.display()));
    let sidecar_path = timing_path(&raw);
    for text in [
        "hello\n",
        "serialist-timing 2\n",
        "serialist-timing 1\nrx 0 5 1\n",
    ] {
        fs::write(&sidecar_path, text).unwrap();
        match rig.open_err(&bad) {
            TransportError::Config(message) => {
                assert!(message.contains(bad.as_str()), "{message}");
            }
            other => panic!("{text:?}: expected Config, got {other:?}"),
        }
    }

    // A malformed id is Config too, before the file is looked for.
    for id in [
        "replay:/c/boot.bin?speed=warp",
        "replay:/c/boot.bin?end=later",
        "replay:/c/boot.bin?volume=11",
        "replay:",
    ] {
        match rig.open_err(&PortId::new(id)) {
            TransportError::Config(message) => assert!(message.contains(id), "{message}"),
            other => panic!("{id}: expected Config, got {other:?}"),
        }
    }

    // No sidecar at all is fine, and the description says so.
    let plain = dir.write("plain.bin", b"xyz");
    let transport = rig.open(&PortId::new(format!("replay:{}", plain.display())));
    assert_eq!(transport.description, "replay:plain.bin (1x, no timing)");
}

#[test]
fn the_raw_file_and_sidecar_may_disagree() {
    let rig = Rig::new();

    // The sidecar describes 12 bytes but the raw file was flushed only to 9: the last
    // record is cut back to what exists, and nothing past it plays.
    let cap = Capture::standard();
    fs::write(&cap.path, &cap.raw[..9]).unwrap();
    let mut transport = rig.open(&cap.id(""));
    assert_eq!(read(&mut transport, 64), b"abc");
    rig.clock.advance(10 * MS);
    assert_eq!(read(&mut transport, 64), b"defg");
    rig.clock.advance(40 * MS);
    assert_eq!(read(&mut transport, 64), b"hi");
    assert_disconnected(try_read(&mut transport, 64));

    // The raw file is longer than the sidecar knows: the tail arrives right after the
    // last record, as a chunk of its own.
    let cap = Capture::standard();
    let mut longer = cap.raw.clone();
    longer.extend_from_slice(b"MNO");
    fs::write(&cap.path, longer).unwrap();
    let mut transport = rig.open(&cap.id(""));
    assert_eq!(read(&mut transport, 64), b"abc");
    rig.clock.advance(10 * MS);
    assert_eq!(read(&mut transport, 64), b"defg");
    rig.clock.advance(39 * MS);
    assert_eq!(read(&mut transport, 64), b"");
    rig.clock.advance(MS);
    assert_eq!(read(&mut transport, 64), b"hijkl");
    assert_eq!(read(&mut transport, 64), b"MNO", "due with the last record");
    assert_disconnected(try_read(&mut transport, 64));
}

#[test]
fn a_late_reader_gets_the_overdue_chunks_back_to_back() {
    let cap = Capture::standard();
    let rig = Rig::new();
    let mut transport = rig.open(&cap.id(""));

    // The reader sleeps through the whole capture and then catches up in one go.
    rig.clock.advance(Duration::from_secs(5));
    for (_, expected) in CHUNKS {
        assert_eq!(read(&mut transport, 64), expected, "one chunk per read");
    }
    assert_disconnected(try_read(&mut transport, 64));
}

#[test]
fn writes_and_controls_are_harmless() {
    let cap = Capture::standard();
    let rig = Rig::new();
    let mut transport = rig.open(&cap.id(""));

    transport.writer.write_all(b"printenv\r\n").unwrap();
    transport.writer.write_all(b"").unwrap();
    transport
        .writer
        .set_control(ControlLine::Dtr, true)
        .unwrap();
    transport
        .writer
        .set_control(ControlLine::Rts, false)
        .unwrap();
    transport
        .writer
        .reconfigure(&SerialConfig {
            baud: 9600,
            ..SerialConfig::default()
        })
        .unwrap();
    match transport.writer.send_break(10 * MS) {
        Err(TransportError::Unsupported(what)) => assert_eq!(what, "break"),
        other => panic!("break is unsupported, got {other:?}"),
    }

    // None of it reached the capture, and a sidecar's schedule ignores the baud.
    assert_eq!(read(&mut transport, 64), b"abc");
    assert_eq!(read(&mut transport, 64), b"");
    rig.clock.advance(10 * MS);
    assert_eq!(read(&mut transport, 64), b"defg");
}

/// Lets the session's reader thread, the only thread on the clock, finish what the last
/// `advance` woke it for. A reader that has reached the end of the capture exits instead
/// of sleeping, which `settle` cannot see, so that counts as settled too.
fn settle_or_end(clock: &ManualClock, session: &Session) {
    let limit = Instant::now() + Duration::from_secs(10);
    while clock.sleepers() < 1 && session.is_connected() {
        assert!(
            Instant::now() < limit,
            "the reader neither went back to sleep nor stopped"
        );
        thread::sleep(MS);
    }
}

#[test]
fn a_session_replays_the_capture_whole() {
    let cap = Capture::timed(
        "cap.bin",
        &[
            (Duration::ZERO, b"boot: start\r\n"),
            (10 * MS, &[0x00, 0xff, 0x7f, 0x80, 0x0d, 0x0a]),
            (35 * MS, b"ready\r\n"),
            (60 * MS, &[b'#'; 100]),
        ],
    );
    let clock = Arc::new(ManualClock::new());
    let router = RoutingTransportFactory::new(Arc::new(SerialportFactory::new())).with_scheme(
        REPLAY_SCHEME,
        Arc::new(ReplayTransportFactory::with_clock(Arc::clone(&clock) as _)),
    );
    let mut config = SessionConfig::new(cap.id(""), SerialConfig::default());
    config.read_timeout = Duration::from_millis(10);
    let session = Session::open(&router, config).expect("the capture opens");
    let events = session.events();

    // Let the reader take the chunk that is due at once and go to sleep, then play the
    // rest in 5 ms steps until the session ends.
    clock.settle(1);
    let mut steps = 0;
    while session.is_connected() {
        steps += 1;
        assert!(steps <= 100, "the session never ended");
        clock.advance(5 * MS);
        settle_or_end(&clock, &session);
    }

    let all: Vec<SessionEvent> = events.try_iter().collect();
    match all.first() {
        Some(SessionEvent::Connected { description }) => {
            assert_eq!(description, "replay:cap.bin (1x)");
        }
        other => panic!("expected Connected first, got {other:?}"),
    }
    match all.last() {
        Some(SessionEvent::Disconnected {
            error: Some(TransportError::Disconnected),
        }) => {}
        other => panic!("expected the capture to end the session, got {other:?}"),
    }
    let mut bytes = Vec::new();
    let mut lens = Vec::new();
    for event in &all[1..all.len() - 1] {
        match event {
            SessionEvent::Data { bytes: chunk, .. } => {
                bytes.extend_from_slice(chunk);
                lens.push(chunk.len() as u64);
            }
            other => panic!("only data between the ends, got {other:?}"),
        }
    }
    assert_eq!(bytes, cap.raw, "the whole capture, byte for byte");
    let recorded = Timing::read_file(&timing_path(&cap.path)).unwrap();
    let recorded: Vec<u64> = recorded.rx().map(|(_, _, len)| len).collect();
    assert_eq!(lens, recorded, "chunk for chunk, as it was recorded");
    assert_eq!(session.stats().rx_chunks, 4);
}

#[test]
fn an_idle_replay_does_not_spin() {
    let cap = Capture::standard();
    // Real time: this is the one test that does not use the manual clock.
    let factory = ReplayTransportFactory::new();
    let mut transport = factory
        .open(&cap.id("?speed=max&end=hold"), &SerialConfig::default())
        .unwrap();
    let mut buf = [0u8; 64];
    for (_, expected) in CHUNKS {
        let n = transport.reader.read(&mut buf, 10 * MS).unwrap();
        assert_eq!(&buf[..n], expected);
    }

    let started = Instant::now();
    let mut calls = 0u32;
    while started.elapsed() < Duration::from_millis(300) {
        let n = transport.reader.read(&mut buf, 10 * MS).unwrap();
        assert_eq!(n, 0, "held open and silent");
        calls += 1;
    }
    let per_second = f64::from(calls) / started.elapsed().as_secs_f64();
    // A 10 ms timeout is about 100 reads a second, 64 on Windows' 15.6 ms timer tick, and
    // a loaded CI runner has been seen at 16. The point is the upper bound; the lower
    // bound only proves the loop ran.
    assert!(calls >= 2, "only {calls} reads in 300 ms");
    assert!(
        per_second < 200.0,
        "{per_second:.0} reads/s looks like a spin"
    );
}
