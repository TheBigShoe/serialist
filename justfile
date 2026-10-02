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
# `--no-cfg-fuzzing`: nusb 0.2 has `cfg(fuzzing)` code that does not compile.
fuzz_flags := "--fuzz-dir fuzz --no-cfg-fuzzing"
fuzz_limits := "-rss_limit_mb=1024 -malloc_limit_mb=256 -timeout=10"

# Format check, clippy and the seed replay for fuzz/, on the pinned stable toolchain.
fuzz-check:
    cargo fmt --manifest-path fuzz/Cargo.toml --check
    cargo clippy --manifest-path fuzz/Cargo.toml --all-targets --locked -- -D warnings
    cargo test --manifest-path fuzz/Cargo.toml --locked

# Fuzz one target for `secs` seconds; `just fuzz-list` names them. New inputs go to
# fuzz/corpus/<target> (not committed), crashes to fuzz/artifacts/<target>. Copy a crash
# into fuzz/seeds/<target>/ once fixed so `just fuzz-check` replays it from then on.
# Needs `rustup toolchain install nightly` and `cargo install cargo-fuzz --locked`.
fuzz target secs="60":
    mkdir -p fuzz/corpus/{{target}}
    cargo +nightly fuzz run {{fuzz_flags}} {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- -max_total_time={{secs}} {{fuzz_limits}}

# The same run without nightly: the pinned stable toolchain with coverage guidance but no
# AddressSanitizer. Good for a quick local look; CI fuzzes with nightly and ASan.
fuzz-stable target secs="60":
    mkdir -p fuzz/corpus/{{target}}
    cargo fuzz run --sanitizer none {{fuzz_flags}} {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- -max_total_time={{secs}} {{fuzz_limits}}

# List the fuzz targets.
fuzz-list:
    cargo fuzz list --fuzz-dir fuzz

# Render the real workspace offscreen with Metal in nineteen states (macOS only) and write
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
