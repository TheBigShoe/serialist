# Common Serialist commands. Run `just` to list them. Each mirrors a CI step in
# .github/workflows/ci.yml, so a green `just ci` predicts a green test job.

[private]
default:
    @just --list

# Type-check every crate and target without generating code.
check:
    cargo check --workspace --all-targets --locked

# Run all tests. No serial hardware is needed; hardware tests are #[ignore].
test:
    cargo test --workspace --locked

# Clippy with warnings as errors, as in CI.
lint:
    cargo clippy --workspace --all-targets --locked -- -D warnings

# Format the workspace in place.
fmt:
    cargo fmt --all

# Verify formatting without changing files, as in CI.
fmt-check:
    cargo fmt --all --check

# Licenses and bans must pass, advisories only warn, as in CI (needs cargo-deny).
deny:
    cargo deny --all-features check licenses bans
    -cargo deny --all-features check advisories

# The plugin crate with the WebAssembly tier (wasmtime; a long first build).
test-wasm:
    cargo test -p serialist-plugins --features wasm --locked

# Rebuild the WebAssembly plugins (examples/plugins, the adapter's test plugin) and
# refresh the copies under serialist-plugins/tests/fixtures that the tests load. Needs
# `rustup target add wasm32-wasip2`.
wasm-fixtures:
    SERIALIST_BLESS_WASM=1 cargo test -p serialist-plugins --features wasm --locked --test wasm_plugin the_committed_plugins_match_their_sources

# Everything the CI test job runs.
ci: fmt-check lint test

# Fuzzing. fuzz/ is a cargo-fuzz crate outside the workspace, with its own Cargo.lock (see
# fuzz/Cargo.toml for why): cargo-fuzz builds it with nightly, so `cargo test --workspace`
# and `just ci` never see it. These mirror the `fuzz` job in .github/workflows/ci.yml.
# `--no-cfg-fuzzing`: nusb 0.2 has `cfg(fuzzing)` code that does not compile. New inputs
# go to fuzz/corpus/<target> (not committed) and crashes to fuzz/artifacts/<target>; copy
# a crash into fuzz/seeds/<target>/ once fixed so `just fuzz-check` replays it from then
# on. fuzz/dicts/<target>.dict, when it exists, is passed as the libFuzzer dictionary.
# Running a target needs `cargo install cargo-fuzz --locked`, and `just fuzz` nightly too.
# race_wasm needs the fuzz crate's `wasm` feature (wasmtime, a long first build), which
# `fuzz` and `fuzz-stable` add for that target only. cargo-fuzz rebuilds when the features
# change, so the next target after it builds without wasmtime again.
fuzz_flags := "--fuzz-dir fuzz --no-cfg-fuzzing"
fuzz_limits := "-rss_limit_mb=1024 -malloc_limit_mb=256 -timeout=10"

# Format check, clippy and the seed replay for fuzz/, on the pinned stable toolchain. The
# tests run twice: as a plain build sees the crate, and with the `wasm` feature, which adds
# the race_wasm target and its seeds.
fuzz-check:
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo clippy --manifest-path fuzz/Cargo.toml --all-targets --all-features --locked -- -D warnings
    cargo test --manifest-path fuzz/Cargo.toml --locked
    cargo test --manifest-path fuzz/Cargo.toml --locked --features wasm

# Fuzz one target for `secs` seconds with nightly and AddressSanitizer, as CI does.
fuzz target secs="60": (_fuzz-run "+nightly" "address" target secs)

# Fuzz one target on the pinned stable toolchain: coverage-guided, but no AddressSanitizer.
fuzz-stable target secs="60": (_fuzz-run "" "none" target secs)

[private]
_fuzz-run toolchain sanitizer target secs:
    mkdir -p fuzz/corpus/{{target}}
    cargo {{toolchain}} fuzz run --sanitizer {{sanitizer}} {{fuzz_flags}} {{ if target == "race_wasm" { "--features wasm" } else { "" } }} {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- -max_total_time={{secs}} {{fuzz_limits}} $(test -f fuzz/dicts/{{target}}.dict && echo -dict=fuzz/dicts/{{target}}.dict)

# List the fuzz targets.
fuzz-list:
    cargo fuzz list --fuzz-dir fuzz

# Code no hardware-free native test can run, left out of every coverage report: the
# binary's entry point (it installs the global log subscriber, exits the process and starts
# the GPUI event loop). Not for code that is merely untested. The same string is
# COVERAGE_IGNORE in the coverage job of .github/workflows/ci.yml; change both together.
coverage_ignore := 'crates/serialist/src/main\.rs'

# One run of every test under cargo-llvm-cov, nothing skipped: a test that holds a
# wall-clock threshold skips just that threshold in a coverage build, and keeps its memory
# and correctness checks. Then the JSON summary and the lcov file, and a floor for each
# crate and the workspace: .github/scripts/coverage_floors.py holds the floors and prints
# the table. The reports are in target/coverage/; for a browsable one afterwards, repeat the
# --ignore-filename-regex of the report lines below: `cargo llvm-cov report --html --open`.
# Needs `cargo install cargo-llvm-cov --locked` and `rustup component add llvm-tools-preview`.
# Line coverage of every test, each crate held to its floor, as the CI coverage job does.
coverage:
    cargo llvm-cov --workspace --locked --no-report --no-fail-fast
    mkdir -p target/coverage
    cargo llvm-cov report --json --summary-only --output-path target/coverage/summary.json --ignore-filename-regex '{{coverage_ignore}}'
    cargo llvm-cov report --lcov --output-path target/coverage/lcov.info --ignore-filename-regex '{{coverage_ignore}}'
    python3 .github/scripts/coverage_floors.py target/coverage/summary.json

# `--bench '*'` picks the [[bench]] targets only: a library's libtest harness rejects
# criterion's flags. `--quick` proves they build and run; for real numbers pass other
# criterion arguments, such as a filter: `just bench parse`.
# Every criterion bench, as the CI bench job runs them.
bench *args="--quick":
    cargo bench --workspace --locked --bench '*' -- {{args}}

# What the CI bench job does on a pull request: the base first, then the head (this
# working tree, uncommitted changes included), one after the other with fuller sampling
# than --quick (the job's flags), then bench_regressions.py's table of changes. It fails
# when a bench is confidently more than 30% slower (BENCH_REGRESSION_THRESHOLD changes
# that). Unlike CI it does not measure a regressed bench a second time, so repeat one
# yourself: `just bench-compare main --exact parse/overwrite_non_ascii`. Extra arguments go
# to criterion on both runs; a filter keeps it short: `just bench-compare main parse`. The
# base is a worktree in target/bench-base (reused, and moved to the commit asked for) that
# builds into its own target/ directory: sharing the head's makes cargo run the head with
# the base's binaries (see the job comment in ci.yml). Criterion's results are in
# target/bench-criterion (emptied first).
# Benchmark this working tree against another commit and fail on a regression.
bench-compare base="main" *args="":
    #!/usr/bin/env bash
    set -euo pipefail
    git rev-parse --verify --quiet "{{base}}" > /dev/null || { echo "bench-compare: no such revision: {{base}}" >&2; exit 1; }
    export CRITERION_HOME="$PWD/target/bench-criterion"
    worktree=target/bench-base
    if [ -d "$worktree" ]; then
        git -C "$worktree" checkout --quiet --force --detach "{{base}}"
    else
        git worktree add --force --detach "$worktree" "{{base}}"
    fi
    rm -rf "$CRITERION_HOME"
    # `cargo bench --bench '*'` is an error when nothing matches, as at a base that
    # predates the benches.
    metadata=$(cd "$worktree" && cargo metadata --no-deps --format-version 1 --locked)
    if ! grep -q '"kind":\["bench"\]' <<< "$metadata"; then
        echo "bench-compare: {{base}} has no bench targets, so there is nothing to compare with"
        exit 0
    fi
    times="--warm-up-time 1 --measurement-time 3"
    (cd "$worktree" && CARGO_TARGET_DIR="$PWD/target" cargo bench --workspace --locked --bench '*' -- --save-baseline base $times {{args}})
    cargo bench --workspace --locked --bench '*' -- --baseline-lenient base $times {{args}}
    python3 .github/scripts/bench_regressions.py "$CRITERION_HOME"

# Render the real workspace offscreen with Metal in twenty-two states (macOS only) and write
# PNGs to target/screenshots/. Names pick shots by file name: `just screenshots 03 light`.
screenshots *names:
    cargo run -p serialist-ui --example screenshots --locked -- --out target/screenshots {{names}}
    @ls -lh target/screenshots/*.png

# Release packaging. These run packaging/package.sh (bash; Git Bash on Windows), the same
# script .github/workflows/release.yml calls. Each builds the release binary first and
# writes to dist/. None of them cross-compile: run the one for the machine you are on.
# See docs/releasing.md.

# Package for this machine's OS.
release:
    bash packaging/package.sh

# macOS: target/bundle/Serialist.app and dist/Serialist-<version>-macos-<arch>.dmg.
release-macos:
    bash packaging/package.sh macos

# Linux: dist/*.tar.gz and dist/*.deb (needs cargo-deb).
release-linux:
    bash packaging/package.sh linux

# Windows: dist/*.msi (needs cargo-wix and the WiX Toolset 3).
release-windows:
    bash packaging/package.sh windows

# Regenerate the .icns, .ico and PNG icons from packaging/icons/serialist.svg and serialist-small.svg (macOS only).
icons:
    packaging/icons/render.sh
