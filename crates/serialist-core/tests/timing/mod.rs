//! The rule for a wall-clock threshold in a test, shared by the test binaries that have
//! one (`gate`, `concurrency`, `ingest`). The serialist-sim tests have the same rule in
//! `serialist-sim/tests/common/mod.rs`, which this one follows.

/// Whether this run holds the tests' time and rate thresholds to account.
///
/// A threshold in wall-clock time (a budget for a million appends, reads completed in a
/// second, bytes delivered in two) is a target for a quiet machine and a build with no
/// instrumentation. It does not hold under a coverage build (`cargo llvm-cov` passes
/// `--cfg coverage`, and the counters it adds make code several times slower), on a loaded
/// developer machine, or on a shared CI runner, where a thread can wait tens of
/// milliseconds for a core. So a threshold is checked unless the build is instrumented,
/// and unless `CI` is set without `SERIALIST_TIMING_TESTS`. Set that variable to hold a CI
/// runner to its thresholds anyway.
///
/// Only thresholds follow this. What a test must also get right whatever the speed
/// (every byte arrived, in order; memory within its budget; every line reads back) is
/// checked on every run, and the work itself still runs, so a skipped run exercises the
/// same code.
pub fn wall_clock_timing_enabled() -> bool {
    !cfg!(coverage)
        && (std::env::var_os("CI").is_none()
            || std::env::var_os("SERIALIST_TIMING_TESTS").is_some())
}
