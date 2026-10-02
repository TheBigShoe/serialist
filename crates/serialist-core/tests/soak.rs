//! The stress tier's soak run: a mixed firehose at 12 Mbaud (1.2 MB/s) through a
//! session, the ingest thread and the store for a minute, on the real clock, with the
//! test thread playing the UI at 60 frames a second. It checks that every byte arrived
//! in order and that the heap stays under the store's budget, and prints the rate, the
//! heap growth and how much of a core each thread used.
//!
//! Ignored by default: it takes a minute, and its numbers only mean something in a
//! release build on a quiet machine. It needs no hardware.
//!
//! ```text
//! cargo test --release -p serialist-core --test soak -- --ignored --nocapture
//! ```
//!
//! `SERIALIST_SOAK_SECS` (default 60), `SERIALIST_SOAK_BAUD` (default 12000000) and
//! `SERIALIST_SOAK_BUDGET_MB` (default 256, the app's default scrollback budget) change
//! the run; a small budget shows eviction holding the heap flat.
//!
//! Heap use is measured with a counting global allocator, which is why this lives alone
//! in its own binary. Thread CPU time comes from `clock_gettime` on macOS and 64-bit
//! Linux and is reported as unavailable elsewhere.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{RecvTimeoutError, Sender, unbounded};
use parking_lot::Mutex;
use serialist_core::{
    ChunkSink, Ingest, LineId, LineSource, SerialConfig, Session, SessionConfig, Store, StoreConfig,
};
use serialist_sim::{
    DeviceOutput, FirehoseConfig, FirehoseContent, FirehoseDevice, FirehoseVerifier, LinkConfig,
    SimDevice, SimWorld,
};

struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let now = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(now, Ordering::Relaxed);
}

// SAFETY: forwards to the system allocator and only adds bookkeeping.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as the caller's.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        new
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// CPU time used so far by the calling thread or by the whole process, where the
/// platform reports it.
mod cpu {
    use std::time::Duration;

    #[cfg(all(
        any(target_os = "macos", target_os = "linux"),
        target_pointer_width = "64"
    ))]
    fn read(clock: i32) -> Option<Duration> {
        // `struct timespec` on 64-bit macOS and Linux: two 64-bit fields.
        #[repr(C)]
        struct Timespec {
            tv_sec: i64,
            tv_nsec: i64,
        }
        unsafe extern "C" {
            fn clock_gettime(clock: i32, tp: *mut Timespec) -> i32;
        }
        let mut ts = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec with the platform's layout.
        let rc = unsafe { clock_gettime(clock, &mut ts) };
        let secs = u64::try_from(ts.tv_sec).ok()?;
        let nanos = u32::try_from(ts.tv_nsec).ok()?;
        (rc == 0).then(|| Duration::new(secs, nanos))
    }

    #[cfg(not(all(
        any(target_os = "macos", target_os = "linux"),
        target_pointer_width = "64"
    )))]
    fn read(_clock: i32) -> Option<Duration> {
        None
    }

    // CLOCK_PROCESS_CPUTIME_ID and CLOCK_THREAD_CPUTIME_ID.
    #[cfg(target_os = "macos")]
    const IDS: (i32, i32) = (12, 16);
    #[cfg(not(target_os = "macos"))]
    const IDS: (i32, i32) = (2, 3);

    pub fn process() -> Option<Duration> {
        read(IDS.0)
    }

    pub fn thread() -> Option<Duration> {
        read(IDS.1)
    }
}

/// The latest CPU time a thread reported for itself.
type CpuSlot = Arc<Mutex<Option<Duration>>>;

/// A firehose that reports its thread's CPU time (generator and link pump) after each tick.
struct Metered {
    inner: FirehoseDevice,
    cpu: CpuSlot,
}

impl SimDevice for Metered {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn on_receive(&mut self, bytes: &[u8], out: &mut dyn DeviceOutput) {
        self.inner.on_receive(bytes, out);
    }

    fn on_tick(&mut self, now: Instant, out: &mut dyn DeviceOutput) -> Option<Instant> {
        let next = self.inner.on_tick(now, out);
        *self.cpu.lock() = cpu::thread();
        next
    }
}

/// Copies every chunk to the verifier thread and reports the ingest thread's CPU time.
struct Tap {
    tx: Sender<Vec<u8>>,
    cpu: CpuSlot,
}

impl ChunkSink for Tap {
    fn on_chunk(&mut self, bytes: &[u8], _at: Instant) {
        let _ = self.tx.send(bytes.to_vec());
        *self.cpu.lock() = cpu::thread();
    }

    fn on_disconnect(&mut self) {}
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

const MB: f64 = 1024.0 * 1024.0;

/// What every thread had used, and what had arrived, at one moment.
#[derive(Clone, Copy)]
struct Mark {
    at: Instant,
    rx_bytes: u64,
    process: Option<Duration>,
    ingest: Option<Duration>,
    device: Option<Duration>,
    ui: Option<Duration>,
}

/// `later - earlier` of a CPU time as a share of `wall`, in percent of one core.
fn share(earlier: Option<Duration>, later: Option<Duration>, wall: Duration) -> String {
    match (earlier, later) {
        (Some(a), Some(b)) => format!(
            "{:.2}%",
            100.0 * b.saturating_sub(a).as_secs_f64() / wall.as_secs_f64()
        ),
        _ => "n/a".to_owned(),
    }
}

#[test]
#[ignore = "a 60 s soak: use --release --ignored --nocapture"]
fn soak_at_12_mbaud() {
    let secs = env_u64("SERIALIST_SOAK_SECS", 60).max(2);
    let baud = u32::try_from(env_u64("SERIALIST_SOAK_BAUD", 12_000_000)).expect("baud fits u32");
    let budget_mb = env_u64("SERIALIST_SOAK_BUDGET_MB", 256);
    let budget = usize::try_from(budget_mb).expect("budget fits usize") * 1024 * 1024;
    let content = FirehoseContent::Mixed;
    let serial = SerialConfig {
        baud,
        ..SerialConfig::default()
    };
    let nominal = serial.bytes_per_second();
    eprintln!(
        "soak: {secs} s of {content:?} at {baud} baud ({nominal:.0} B/s), budget {budget_mb} MiB"
    );

    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);

    let device_cpu = CpuSlot::default();
    let ingest_cpu = CpuSlot::default();
    let world = SimWorld::empty();
    let slot = Arc::clone(&device_cpu);
    let id = world.add_virtual("hose", "Firehose", LinkConfig::default(), move || {
        Box::new(Metered {
            inner: FirehoseDevice::new(FirehoseConfig::new(content)),
            cpu: Arc::clone(&slot),
        })
    });
    let session =
        Session::open(world.factory(), SessionConfig::new(id.clone(), serial)).expect("open");
    let link = world.link(&id).expect("link is up");

    let (tap_tx, tap_rx) = unbounded::<Vec<u8>>();
    let verifier = thread::spawn(move || {
        let mut verifier = FirehoseVerifier::new(content);
        for chunk in tap_rx {
            verifier.feed(&chunk);
        }
        verifier.into_report()
    });
    let (wake_tx, wake_rx) = unbounded();
    let handle = Ingest::spawn(
        session.events(),
        Store::new(StoreConfig::with_budget(budget)),
        vec![Box::new(Tap {
            tx: tap_tx,
            cpu: Arc::clone(&ingest_cpu),
        })],
        Box::new(move || {
            let _ = wake_tx.send(());
        }),
    );

    let mark = || Mark {
        at: Instant::now(),
        rx_bytes: session.stats().rx_bytes,
        process: cpu::process(),
        ingest: *ingest_cpu.lock(),
        device: *device_cpu.lock(),
        ui: cpu::thread(),
    };

    // Play the UI: on each wake acknowledge, snapshot, copy the last screenful of lines,
    // then wait out a 60 fps frame. Measure from the second second on, past start-up.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(secs);
    let mut warm = None;
    let mut next_report = started + Duration::from_secs(1);
    let mut frames = 0u64;
    let mut visible = Vec::new();
    eprintln!("     t   received   rx MiB   store MiB   heap MiB");
    while Instant::now() < deadline {
        match wake_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => {
                handle.acknowledge();
                let snap = handle.snapshot();
                let end = snap.end();
                let first = LineId(end.0.saturating_sub(60).max(snap.first_line().0));
                visible.clear();
                snap.lines(first..end, &mut visible);
                frames += 1;
                thread::sleep(Duration::from_micros(16_667));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => panic!("the ingest thread ended early"),
        }
        let now = Instant::now();
        if now >= next_report {
            let elapsed = now - started;
            if warm.is_none() {
                warm = Some(mark());
            }
            if elapsed.as_secs() % 10 == 0 || elapsed.as_secs() <= 1 {
                let store = handle.stats().store;
                eprintln!(
                    "{:>5.1}s {:>10} {:>8.1} {:>11.1} {:>10.1}",
                    elapsed.as_secs_f64(),
                    session.stats().rx_bytes,
                    store.raw_len as f64 / MB,
                    store.memory as f64 / MB,
                    CURRENT.load(Ordering::Relaxed).saturating_sub(base) as f64 / MB,
                );
            }
            next_report += Duration::from_secs(1);
        }
    }
    let end = mark();
    let warm = warm.expect("ran past the first second");
    let heap_end = CURRENT.load(Ordering::Relaxed).saturating_sub(base);
    let rx_chunks = session.stats().rx_chunks;
    let ingest_stats = handle.stats();

    session.close();
    let store = handle.join().expect("the ingest thread ran cleanly");
    let report = verifier.join().expect("the verifier ran cleanly");
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(base);
    let stats = store.stats();
    let link_stats = link.stats();

    let wall = end.at - warm.at;
    let rate = (end.rx_bytes - warm.rx_bytes) as f64 / wall.as_secs_f64();
    eprintln!(
        "rate: {rate:.0} B/s over {:.1} s, {:+.3}% of nominal; {} chunks, mean {:.0} bytes",
        wall.as_secs_f64(),
        100.0 * (rate - nominal) / nominal,
        rx_chunks,
        stats.raw_len as f64 / rx_chunks.max(1) as f64,
    );
    eprintln!(
        "stored: {} bytes, {} records, clean {}, missing {}, corrupt {}; link dropped {}",
        stats.raw_len,
        report.records,
        report.is_clean(),
        report.missing_records,
        report.corrupt_records,
        link_stats.dropped_bytes,
    );
    eprintln!(
        "heap: {:.1} MiB at the end, {:.1} MiB peak, store {:.1} MiB for {:.1} MiB retained \
         ({} lines, {} evicted)",
        heap_end as f64 / MB,
        peak as f64 / MB,
        stats.memory as f64 / MB,
        stats.retained_bytes() as f64 / MB,
        stats.lines(),
        stats.evicted_lines,
    );
    eprintln!(
        "cpu (one core = 100%): process {}, ingest {}, device+link {}, ui {}",
        share(warm.process, end.process, wall),
        share(warm.ingest, end.ingest, wall),
        share(warm.device, end.device, wall),
        share(warm.ui, end.ui, wall),
    );
    eprintln!(
        "ui: {frames} frames, {} wakes, {} chunks ingested",
        ingest_stats.wakes, ingest_stats.chunks
    );

    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.bytes, stats.raw_len);
    assert_eq!(link_stats.dropped_bytes, 0);
    assert!(rate > 0.9 * nominal, "{rate:.0} B/s against {nominal:.0}");
    // The store keeps to its budget; the rest is the link, the channels and the copy
    // the verifier is fed, a few MiB at most.
    assert!(
        peak <= budget + 16 * 1024 * 1024,
        "peak heap {peak} over the budget {budget}"
    );
}
