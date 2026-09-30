# Changelog

All notable changes to Serialist are recorded here, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/) (before 1.0, a minor version may break things).

To cut a release, rename "Unreleased" to the version and date and start a new empty
"Unreleased" above it. `docs/releasing.md` has the steps, and the release workflow
publishes the version's section as the draft release's notes.

## [Unreleased]

Nothing has been released yet. What exists so far, one line per milestone of `docs/plan.md`:

- Milestone 0, toolchain and shell: the workspace, CI and license policy, the session engine with reader and writer threads, real serial transport and port discovery, a simulated-device world (`--virtual`), and a GPUI shell that shows a device's output.
- Milestone 1, performance core: a page-store scrollback with O(1) append, the monitor-mode ANSI parser, an ingest thread, a GPU terminal element with search, selection, hex view, pause, export and raw recording, plus criterion benches and frame-cost gates.
- Milestone 2, configuration: JSONC settings, Zed theme and keymap loading, font settings with fallback, and a debounced watcher that applies changes live.
- Milestone 3, interaction modes: inline interactive typing, saved commands with a Commands panel and persistent history, line matchers, local-time timestamps and dim control-character glyphs.
- Milestone 4, scripting: a Lua 5.4 script host on its own thread, a Script console, a watched scripts folder, and a headless `--script` run.
- Milestone 5, plugins: the `Codec` trait and frame store, Airoha RACE and text codecs in Rust and Lua, and a simulated RACE device.
- Milestone 6, started: CI on macOS, Linux and Windows, and release packaging: a macOS `.app` in a `.dmg`, a Linux `.tar.gz` and `.deb`, a Windows `.msi`, and a workflow that turns a `v*` tag into a draft GitHub Release. Release builds are unsigned.
