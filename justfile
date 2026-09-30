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
