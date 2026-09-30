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

A change is ready when `just ci` and `just deny` pass. CI runs the same gates on macOS,
Linux and Windows, and a pull request needs all three green.

## Tests never need hardware

CI has no serial devices, so every test, benchmark and stress run must work without one.
Code talks to ports through the `Transport` trait; tests use the virtual transport, the
fake port source and the simulated devices in `serialist-sim`. A test that needs a real
adapter is marked `#[ignore]` with a comment saying what hardware it needs, and it never
gates a merge. A bug first seen on hardware gets a recorded capture and a regression
test that reproduces it without hardware. See "Testing strategy" in `docs/plan.md`.

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
