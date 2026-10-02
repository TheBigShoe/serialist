//! The milestone 1 performance gate for the store, on one million short lines.
//!
//! Thresholds are release-mode targets; a debug build relaxes the timing ones five
//! times (memory is the same in both). Timings take the best of a few rounds so a busy
//! machine does not fail the gate on one unlucky sample. Heap use is measured with a
//! counting global allocator, which is why this test lives alone in its own binary.
//!
//! The timing thresholds are checked only where wall-clock timing holds (see
//! `timing::wall_clock_timing_enabled`: not under coverage, not on CI unless asked). The
//! work is done and the timings are printed either way, and every memory and correctness
//! check runs everywhere.

mod timing;

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serialist_core::{LineId, LineSource, Searcher, Snapshot, Store, StoreConfig};
use serialist_sim::{FirehoseContent, FirehoseGenerator};

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

const LINES: u64 = 1_000_000;

/// Heap measurements need the process to themselves: tests here run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Timing thresholds are for release builds; debug builds get five times as long.
fn relaxed(d: Duration) -> Duration {
    if cfg!(debug_assertions) { d * 5 } else { d }
}

/// Hold `took` to `limit` (relaxed for a debug build) where wall-clock timing holds.
/// Anywhere else say what was measured and let it pass.
fn within(what: &str, took: Duration, limit: Duration) {
    if timing::wall_clock_timing_enabled() {
        assert!(took < relaxed(limit), "{what} took {took:?}");
    } else {
        eprintln!(
            "{what} took {took:?}: the {:?} limit is not checked here (coverage build, or CI \
             without SERIALIST_TIMING_TESTS)",
            relaxed(limit)
        );
    }
}

/// Best of `rounds` runs of `f`, each timed as a whole.
fn best_of(rounds: usize, mut f: impl FnMut()) -> Duration {
    (0..rounds)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .min()
        .expect("at least one round")
}

/// Append `count` short log lines in 4 KiB chunks, the last one a needle.
fn fill(store: &mut Store, count: u64, buf: &mut Vec<u8>) {
    use std::io::Write as _;
    let now = Instant::now();
    for i in 0..count {
        let kind = if i + 1 == count {
            "needle"
        } else {
            "status ok"
        };
        write!(buf, "{i:08} sensor temp={} {kind}\r\n", i % 90).expect("write to vec");
        if buf.len() >= 4096 || i + 1 == count {
            store.append(buf, now);
            buf.clear();
        }
    }
}

/// Mean time of one `line()` over `ids`, best of a few rounds.
fn lookup_time(snap: &Snapshot, ids: &[LineId]) -> Duration {
    best_of(5, || {
        for &id in ids {
            black_box(snap.line(id).expect("line"));
        }
    }) / ids.len() as u32
}

#[test]
fn one_million_lines_gate() {
    let _serial = serial();
    // Memory: the store's peak heap while appending, against the bytes appended.
    let mut buf = Vec::with_capacity(8192);
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let mut store = Store::new(StoreConfig::default());
    let append_start = Instant::now();
    fill(&mut store, LINES, &mut buf);
    let append_time = append_start.elapsed();
    let peak = PEAK.load(Ordering::Relaxed) - base;
    let stats = store.stats();
    assert_eq!(stats.end_line, LineId(LINES));
    assert_eq!(stats.evicted_lines, 0);
    let raw = stats.raw_len as usize;
    assert!(
        peak * 2 < raw * 3,
        "peak heap {peak} bytes for {raw} raw bytes ({:.2}x, limit 1.5x)",
        peak as f64 / raw as f64
    );
    assert!(
        stats.memory * 2 < raw * 3,
        "accounted {} for {raw} raw bytes",
        stats.memory
    );
    // Accounting tracks the real heap closely.
    let live = CURRENT.load(Ordering::Relaxed) - base - buf.capacity();
    let (lo, hi) = (live.min(stats.memory), live.max(stats.memory));
    assert!(
        hi - lo < hi / 20,
        "accounted {} vs heap {live}",
        stats.memory
    );
    within(
        &format!("appending {LINES} lines"),
        append_time,
        Duration::from_secs(2),
    );

    let reader = store.reader();
    let snap = reader.snapshot();

    // snapshot() is well under 10 µs, whatever the store's size.
    let rounds = 10_000u32;
    let snapshot_time = best_of(5, || {
        for _ in 0..rounds {
            black_box(reader.snapshot());
        }
    }) / rounds;
    within("snapshot", snapshot_time, Duration::from_micros(10));

    // line() costs the same at either end of a million lines and in a 100-line store.
    let first: Vec<LineId> = (0..10_000).map(LineId).collect();
    let last: Vec<LineId> = (LINES - 10_000..LINES).map(LineId).collect();
    let t_first = lookup_time(&snap, &first);
    let t_last = lookup_time(&snap, &last);
    let mut small = Store::default();
    fill(&mut small, 100, &mut buf);
    let small_ids: Vec<LineId> = (0..100).cycle().take(10_000).map(LineId).collect();
    let t_small = lookup_time(&small.snapshot(), &small_ids);
    for (name, t) in [("first", t_first), ("last", t_last), ("small", t_small)] {
        within(&format!("line() near {name}"), t, Duration::from_micros(2));
    }
    let (fast, slow) = (
        t_first.min(t_last).min(t_small),
        t_first.max(t_last).max(t_small),
    );
    // A ratio of two timings is as much a wall-clock check as a limit is.
    if timing::wall_clock_timing_enabled() {
        assert!(
            slow < fast * 3 + Duration::from_nanos(100),
            "lookups not constant: first {t_first:?}, last {t_last:?}, 100-line store {t_small:?}"
        );
    }

    // Search: the only match is the last line, so this is a full scan either way.
    let cancel = AtomicBool::new(false);
    let mut hits = Vec::new();
    let forward = best_of(3, || {
        hits = snap
            .search("needle", LineId(0), false, 1, &cancel)
            .expect("pattern");
    });
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].line, LineId(LINES - 1));
    let backward = best_of(3, || {
        hits = snap
            .search("^00000000 ", LineId(LINES), true, 1, &cancel)
            .expect("pattern");
    });
    assert_eq!(hits.first().map(|m| m.line), Some(LineId(0)));
    within("forward search", forward, Duration::from_millis(50));
    within("backward search", backward, Duration::from_millis(50));
}

#[test]
fn eviction_keeps_the_real_heap_within_budget() {
    let _serial = serial();
    let budget = 8 * 1024 * 1024;
    let mut chunk = Vec::with_capacity(8192);
    let mut generator = FirehoseGenerator::new(FirehoseContent::Mixed, 11);
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let mut store = Store::new(StoreConfig::with_budget(budget));
    let mut appended = 0usize;
    let mut worst = 0usize;
    while appended < budget * 5 / 2 {
        chunk.clear();
        generator.fill(&mut chunk, 4096);
        store.append(&chunk, Instant::now());
        appended += chunk.len();
        let live = CURRENT.load(Ordering::Relaxed) - base;
        worst = worst.max(live);
        assert!(
            live <= budget + budget / 100,
            "heap {live} over budget {budget}"
        );
    }
    let stats = store.stats();
    assert!(stats.evicted_lines > 0 && stats.raw_start > 0, "{stats:?}");
    // Between appends the heap stays within budget; inside one it may briefly hold a
    // new page and a sealed block's copy before evicting.
    let peak = PEAK.load(Ordering::Relaxed) - base;
    assert!(
        peak <= budget + 512 * 1024,
        "peak {peak} for budget {budget}"
    );
    assert!(
        worst > budget / 2,
        "the store should fill its budget: {worst}"
    );
}
