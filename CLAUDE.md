# Serialist — agent instructions

Read this whole file first, then `docs/plan.md` (the build plan: goals, architecture,
crate layout, testing strategy, milestones). The plan is the source of truth for scope.

## Identity
- Rust workspace, edition 2024, toolchain pinned in `rust-toolchain.toml` (1.98.1).
- License MIT OR Apache-2.0. Never copy code from GPL projects (Zed's `terminal`,
  `settings`, `theme`, `ui` crates; Baudrun). Copying *patterns* is fine.
- GPUI via the pinned community snapshot `gpui-pre =0.3.7` and `gpui-kit =0.7.0`.
  Every GPUI/gpui-kit import goes through `serialist_ui::prelude`.

## Hard rules
1. Tests run without hardware. Never add a test that needs a serial device attached.
   Hardware-dependent checks are `#[ignore]` and documented as such.
2. Dependency direction: `serialist` -> `serialist-ui` -> (`serialist-script`,
   `serialist-plugins`, `serialist-sim`) -> `serialist-core`. `serialist-core` and
   `serialist-sim` never depend on GPUI.
3. Bytes never touch the main thread. Reads happen on a reader thread with a 10–50 ms
   timeout, never a zero timeout. Work is handed over in chunks, never per byte.
4. Keep the contracts in `serialist-core/src/{transport,port,config}.rs` and
   `serialist-sim/src/lib.rs` stable. If a contract must change, change it minimally,
   update every implementor, and say why in the commit message.
5. `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` must pass
   before you report a task done. Report failures verbatim; never claim green without running it.

## Conventions
- Errors: `thiserror` in library crates, `anyhow` only in the binary.
- Logging: `tracing`. No `println!` outside the binary.
- Tests live next to the code (`#[cfg(test)]`) or in `tests/` for integration; property
  tests use `proptest`; snapshot tests use `insta`.
- Commit messages: imperative subject under 72 chars, body explains why.
