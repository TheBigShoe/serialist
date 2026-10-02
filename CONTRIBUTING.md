# Contributing to Serialist

Read `CLAUDE.md` for the architecture rules and `docs/plan.md` for scope, crate layout
and milestones. The plan is the source of truth for what gets built.

## Toolchain

The Rust toolchain is pinned to an exact version in `rust-toolchain.toml` (edition 2024
workspace). With rustup installed, running `rustup toolchain install` in the repository
installs that version with clippy and rustfmt. Do not bump it in an unrelated change.

Platform prerequisites for building GPUI:

- macOS: full Xcode (not just the command line tools), for the Metal shader compiler.
- Linux: the system packages in the "Install Linux system packages" step of
  `.github/workflows/ci.yml` (xkbcommon, Wayland, X11, fontconfig, a Vulkan loader).
- Windows: nothing extra.

The first build compiles GPUI, wgpu and gpui-kit and takes several minutes.

## Running the checks

Install [just](https://github.com/casey/just), then:

| Command | What it runs |
| --- | --- |
| `just check` | `cargo check` over the whole workspace |
| `just test` | `cargo test --workspace` |
| `just lint` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `just fmt` | `cargo fmt --all` (`just fmt-check` only verifies) |
| `just ci` | format check, clippy and tests: what the CI test job runs |
| `just deny` | license, ban and advisory checks (needs `cargo install --locked cargo-deny`) |
| `just fuzz-check` | format check, clippy and the seed replay of the `fuzz/` crate, on the pinned toolchain; needs no nightly and no cargo-fuzz |
| `just fuzz <target> [secs]` | one fuzz target for `secs` seconds (default 60) with nightly and AddressSanitizer, as CI does; needs `cargo install cargo-fuzz --locked` and `rustup toolchain install nightly`. `just fuzz-list` names the targets |
| `just fuzz-stable <target> [secs]` | the same on the pinned toolchain: coverage-guided, but without AddressSanitizer |
| `just coverage` | every test once under cargo-llvm-cov, then each crate and the workspace held to its line-coverage floor; needs `cargo install cargo-llvm-cov --locked` and `rustup component add llvm-tools-preview` |
| `just bench [args]` | every criterion bench, with `--quick` unless you pass arguments: `just bench parse` runs the benches whose id contains `parse` with criterion's own sampling |
| `just bench-compare [base] [args]` | the benches at `base` (default `main`) and in this working tree, one after the other, then a table of the changes; fails if a bench is confidently more than 30% slower |

A change is ready when `just ci` and `just deny` pass, and `just fuzz-check` when it touches
`fuzz/`, which `just ci` never builds. CI runs the same gates on macOS, Linux and Windows,
and a pull request needs all three green.

`.github/workflows/ci.yml` has more jobs than that, all on Linux: `wasm` (clippy and tests of
`serialist-plugins` with its `wasm` feature; `just test-wasm` runs the tests), `licenses`
(`cargo deny`), `fuzz`, `coverage` and `bench`. The last three are described below, and
`fuzz-nightly.yml` runs the fuzz targets for longer once a day.

## Tests never need hardware

CI has no serial devices, so every test, benchmark and stress run must work without one.
Code talks to ports through the `Transport` trait; tests use the virtual transport, the
fake port source and the simulated devices in `serialist-sim`. A test that needs a real
adapter is marked `#[ignore]` with a comment saying what hardware it needs, and it never
gates a merge. A bug first seen on hardware gets a recorded capture and a regression
test that reproduces it without hardware. See "Testing strategy" in `docs/plan.md`.

Two opt-in runs sit outside `just ci`, both `#[ignore]`d:

- The soak: a 12 Mbaud firehose through a session, the ingest thread and the store for
  a minute, simulated, printing the rate, heap growth and each thread's share of a core.
  `cargo test --release -p serialist-core --test soak -- --ignored --nocapture`
  (`SERIALIST_SOAK_SECS`, `SERIALIST_SOAK_BAUD` and `SERIALIST_SOAK_BUDGET_MB` change it).
- The hardware tier: a loopback at 12 Mbaud on a real adapter (an FTDI FT232H or
  FT2232H) with TX wired to RX, named by `SERIALIST_HW_PORT`.
  `SERIALIST_HW_PORT=/dev/cu.usbserial-XXXX cargo test --release -p serialist-core --test hardware -- --ignored --nocapture`
  (`SERIALIST_HW_BAUD`, `SERIALIST_HW_BYTES` and `SERIALIST_HW_FLOW=hardware` change it).
  Without the variable it prints "skipped".

## Fuzzing

The fuzz targets in `fuzz/` (cargo-fuzz, libFuzzer) feed arbitrary bytes, cut into arbitrary
chunks, to code that reads untrusted input, and check what that code promises beyond not
panicking: chunking never changes the result, no byte is lost, memory stays within its
bound, a plugin agrees with its Rust reference. There are nine, listed by `just fuzz-list`:
`ansi_monitor` (the monitor-mode ANSI parser), `store_ingest` (the page store and its memory
accounting), `vt_screen` (the VT screen), `config_jsonc` (the settings, theme, keymap and
command loaders), `lua_values` (the Lua-to-frame conversion), `text_lines` and `race_rust`
(the Rust reference codecs), and `race_lua` and `race_wasm` (the Lua and WebAssembly RACE
plugins against the Rust one). `race_wasm` needs the fuzz crate's `wasm` feature (wasmtime),
which `just fuzz` and `just fuzz-stable` add for it. Each target's module in `fuzz/src/`
states its input format and its checks.

`fuzz/` is outside the workspace (the root `Cargo.toml` excludes it) and has its own
`Cargo.lock`, so `just ci` and `just deny` never see it; the header of `fuzz/Cargo.toml` has
the reasons. Seeds are committed in `fuzz/seeds/<target>/` as raw bytes (`.gitattributes`
marks them binary so CR bytes survive). `just fuzz` and `just fuzz-stable` start from them and
write what they find to `fuzz/corpus/<target>/`, and a crash to `fuzz/artifacts/<target>/`;
neither directory is committed. `just fuzz-check` replays every seed on the pinned toolchain,
so a seed is also a regression test.

The CI `fuzz` job, on every pull request and every push to main, replays the seeds on stable,
then builds the targets with nightly and AddressSanitizer (with `--features wasm`) and runs
each for 60 s from the seeds and the cached corpus. The corpus is restored from the newest
`fuzz-corpus-` entry in the Actions cache; a push to main minimizes it and saves it, and a
pull request only reads it. A crash fails the job and uploads `fuzz/artifacts/` as the
`fuzz-artifacts` artifact. `.github/workflows/fuzz-nightly.yml` repeats the loop at 03:17 UTC
every day with 600 s per target on the same cache, and can be started by hand with another
number of seconds.

To add a target, copy `fuzz/src/ansi_monitor.rs` and `fuzz/fuzz_targets/ansi_monitor.rs`:

1. `fuzz/src/<target>.rs`: module docs that state the input format and every check beyond
   "no panic"; a `pub fn run(data: &[u8])` that reads `Input::parse(data)` (a config byte,
   chunk lengths, then the stream; see `fuzz/src/lib.rs`) and panics when a check fails; and a
   `seeds_replay` test calling `crate::replay_seeds("<target>", super::run)`.
2. `pub mod <target>;` in `fuzz/src/lib.rs`, in alphabetical order.
3. `fuzz/fuzz_targets/<target>.rs`, the entry point: `#![no_main]` and
   `libfuzzer_sys::fuzz_target!(|data: &[u8]| serialist_fuzz::<target>::run(data));`.
4. A `[[bin]]` for it in `fuzz/Cargo.toml`, like the others (`test`, `doc` and `bench` off).
5. A few seeds in `fuzz/seeds/<target>/`, at least one for each config mode, and optionally a
   libFuzzer dictionary in `fuzz/dicts/<target>.dict`.

`just fuzz-check` fails for a target with no `[[bin]]` or no seeds, and `just fuzz-stable
<target> 60` tries it out. CI picks the target up by itself, since it runs whatever `cargo
fuzz list` prints. Every crate the targets use is declared up front in `fuzz/Cargo.toml`, so a new target
normally leaves `fuzz/Cargo.lock` alone; one that needs another crate changes it, and the
header of `fuzz/Cargo.toml` says how to update it.

When a target finds a crash, CI uploads the input in the `fuzz-artifacts` artifact, and a local
run leaves it in `fuzz/artifacts/<target>/`. Minimize it (`cargo +nightly fuzz tmin --fuzz-dir
fuzz --no-cfg-fuzzing <target> <file>`; on the pinned toolchain drop `+nightly` and add
`--sanitizer none`), fix the bug with a regression test next to the code, and commit the
minimized input as `fuzz/seeds/<target>/regress_<what>`. From then on `just fuzz-check`
replays it on every CI run and names the file if the bug comes back. The `regress_*` seeds
already there are what the targets have found so far.

Two findings are still open. Alacritty keeps every combining mark a device sends for a cell,
with no limit, so one cell can grow without bound; `vt_screen` does not bound it. And vte
0.15 handles a begin of a synchronized update that is not exactly `ESC [ ? 2026 h` (such as
`ESC [ ? 2026 ; 1 h`) differently depending on how the stream is chunked, which can delay the
screen by at most 150 ms; `vt_screen` ends any open update before it compares screens
(`regress_sync_begin_variant_at_resize`).

## Coverage floors

`just coverage` runs every test once under cargo-llvm-cov, with nothing skipped, writes
`target/coverage/summary.json` and `target/coverage/lcov.info`, and runs
`.github/scripts/coverage_floors.py`, which prints each crate's and the workspace's line
coverage next to its floor and exits 1 if one is under. The CI `coverage` job does the same
on Linux only (one instrumented GPUI build is enough) and keeps both files as an artifact.
Only `crates/serialist/src/main.rs`, the binary's entry point, is left out of the reports,
because no hardware-free native test can run it; the exclusion is not for code that is merely
untested. It is one regex, `coverage_ignore` in the `justfile` and `COVERAGE_IGNORE` in
`ci.yml`: change both together.

The floors are the `FLOORS` table at the top of `coverage_floors.py`, from 70%
(`serialist-plugin-sdk`) to 93% (the binary) for crates and 87% for the workspace. The
script's header gives the rule: a floor is 3 points under the lower of a macOS and a Linux
measurement, rounded down, and never above the measurement minus 2. To raise a floor, edit
its number in `FLOORS` and the measurements in the comment above it. Lower one only with a
reason in the commit message: a floor that follows the code down guards nothing. A crate
added to the workspace needs a line in `FLOORS`, because the script fails on a crate in the
report with no floor, and on a floor with no crate in the report.

## Benchmarks

The criterion benches (`crates/*/benches/`) cover store append, the ANSI parser, the line
index, search, codec decoding (the Rust and the Lua RACE codecs), VT feeding, the virtual
link and the firehose. A new one is a `[[bench]]` with `harness = false` in a workspace
crate, with criterion as a dev-dependency; `--bench '*'`, which `just bench` and CI use,
picks it up. `just bench` with its default `--quick` only proves they build and run.

The CI `bench` job first runs the regression script's self-test. On a push to main it then
runs every bench with `--quick` as a smoke test: hosted runners are too noisy for absolute
numbers, and `--quick` is too noisy for a comparison. On a pull request it benchmarks the base
commit and the head one after the other on the same runner (`--warm-up-time 1
--measurement-time 3`), and `.github/scripts/bench_regressions.py` reads the change criterion
computed for each bench. A bench has regressed when it is confidently more than 30% slower:
the lower bound of the confidence interval on the change in its median time is past 30%. One
that is slower on paper but whose interval reaches below 30% is shown as `noisy` and does not
count. Each bench that regressed is then measured again, alone and without the shortened
sampling, and the job fails only if it regresses in both rounds. A bench that is new in the
pull request is measured but not compared, and a slowdown under 30% passes.

`just bench-compare [base]` does the comparison locally: it benchmarks this working tree,
uncommitted changes included, against `base` (default `main`), which it checks out in a
worktree at `target/bench-base`. Unlike CI it measures once, so repeat a bench it flags on its
own: `just bench-compare main --exact parse/overwrite_non_ascii`. Extra arguments go to
criterion on both runs.

To change the threshold, set `BENCH_REGRESSION_THRESHOLD` (0.30 is 30%) or pass `--threshold`
to the script. The CI job sets neither, so for CI change `DEFAULT_THRESHOLD` in
`bench_regressions.py`, or set the variable in the job's `env`.

## Wall-clock thresholds in tests

A test that bounds how long something takes (a million appends, a firehose parsed, reads
completed in a second) holds a target for a quiet machine, not for an instrumented build or a
shared CI runner. Such thresholds are checked on a developer machine. They are skipped in a
coverage build (`cargo llvm-cov` passes `--cfg coverage`), and on CI (where `CI` is set) unless
`SERIALIST_TIMING_TESTS` is set, to any value. A test with a skipped threshold still does its
work and prints what it measured, and what it must get right at any speed (every byte
arrived, in order; memory within its budget; every line reads back) is checked on every run.
The virtual link's real-time smoke tests, which are about the real clock throughout, return
early with a notice instead.

The store gate, the concurrency test, the ingest and virtual link rate checks, the VT
firehose bounds and the config watcher's drop bound follow this rule. The terminal frame
budget (8 ms, 40 ms in a debug build) and the ANSI parser's overwrite bound are skipped under
coverage only. The helpers are `wall_clock_timing_enabled` in
`crates/serialist-core/tests/timing/mod.rs` (integration tests) and
`crates/serialist-core/src/test_util.rs` (unit tests), and `skip_unless_wall_clock_timing!` in
`crates/serialist-sim/tests/common/mod.rs`; `crates/serialist-vt/tests/firehose.rs` has its
own copy of the check. A crate whose tests use `cfg!(coverage)` declares the cfg under
`[lints.rust]` in its `Cargo.toml` (see `serialist-core`'s), so clippy accepts it.

## Licensing

Serialist is licensed under MIT OR Apache-2.0, and contributions are accepted under the
same terms. Do not copy code from GPL projects, including Zed's `terminal`, `settings`,
`theme` and `ui` crates and Baudrun. Copying patterns is fine; copying code is not.
`deny.toml` keeps GPL, LGPL and AGPL dependencies out of the tree, so do not add a
license to its allow list without checking it is compatible.

## Commit messages

Imperative subject under 72 characters, and a body that explains why the change was
made. If you change a contract in `serialist-core/src/{transport,port,config}.rs` or
`serialist-sim/src/lib.rs`, say why in the commit message.
