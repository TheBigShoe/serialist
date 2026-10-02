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
fuzz_flags := "--fuzz-dir fuzz --no-cfg-fuzzing"
fuzz_limits := "-rss_limit_mb=1024 -malloc_limit_mb=256 -timeout=10"

# Format check, clippy and the seed replay for fuzz/, on the pinned stable toolchain.
fuzz-check:
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo clippy --manifest-path fuzz/Cargo.toml --all-targets --locked -- -D warnings
    cargo test --manifest-path fuzz/Cargo.toml --locked

# Fuzz one target for `secs` seconds with nightly and AddressSanitizer, as CI does.
fuzz target secs="60": (_fuzz-run "+nightly" "address" target secs)

# Fuzz one target on the pinned stable toolchain: coverage-guided, but no AddressSanitizer.
fuzz-stable target secs="60": (_fuzz-run "" "none" target secs)

[private]
_fuzz-run toolchain sanitizer target secs:
    mkdir -p fuzz/corpus/{{target}}
    cargo {{toolchain}} fuzz run --sanitizer {{sanitizer}} {{fuzz_flags}} {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- -max_total_time={{secs}} {{fuzz_limits}} $(test -f fuzz/dicts/{{target}}.dict && echo -dict=fuzz/dicts/{{target}}.dict)

# List the fuzz targets.
fuzz-list:
    cargo fuzz list --fuzz-dir fuzz

# Everything but serialist-core, for its coverage floor.
not_core := "crates/serialist-(sim|script|plugins|plugin-sdk|ui|vt)/|crates/serialist/|examples/"

# The store gate is skipped: its timing budget is for uninstrumented builds. Needs `cargo
# install cargo-llvm-cov --locked` and `rustup component add llvm-tools-preview`. For a
# browsable report afterwards: `cargo llvm-cov report --html --open`.
# Line coverage held to the CI floors: 87% of the workspace, 90% of serialist-core.
coverage:
    cargo llvm-cov --workspace --locked --no-report -- --skip one_million_lines_gate
    cargo llvm-cov report --summary-only --fail-under-lines 87
    cargo llvm-cov report --summary-only --fail-under-lines 90 --ignore-filename-regex '{{not_core}}'

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

# Render the real workspace offscreen with Metal in twenty states (macOS only) and write
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
