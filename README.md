# Serialist

A native, GPU-rendered serial terminal built on [GPUI](https://gpui.rs), the UI framework
behind the Zed editor. macOS first, Linux and Windows from the same code.

- Devices appear the moment they are plugged in.
- Any baud rate, full framing and flow control settings, per-device profiles.
- Two modes: inline interactive, and saved commands you can send with a click or a key.
- Lua scripting and protocol plugins (Airoha RACE ships as the first one).
- Fonts and themes configured the way Zed does it; Zed theme files load unchanged.
- Fast: no dropped bytes at 3 Mbaud, frames under 8 ms at a million lines of scrollback.
- Every test runs without hardware.

## Status

Milestones 0 to 5 of `docs/plan.md` have landed: the session engine and GPUI shell, the
terminal element over a page-store scrollback, Zed-format settings, themes and keymaps that
apply live, inline interactive and saved-command modes, Lua scripting with a headless
`--script` mode, and protocol plugins in Rust, Lua and WebAssembly. Milestone 6 has
started: CI on macOS, Linux and Windows and release packaging are in; tabs for several
sessions and full terminal emulation are pending (the terminal is monitor mode, an ANSI
parser over the received text). The plan's "Status as built" section lists where the build
differs from the plan, and `CHANGELOG.md` says what each milestone shipped.

## Quick start

Build from a checkout. The Rust toolchain is pinned in `rust-toolchain.toml` and
`CONTRIBUTING.md` lists the platform prerequisites (full Xcode on macOS, a few system
packages on Linux). The first build compiles GPUI and takes several minutes.

```sh
cargo run --release -p serialist -- --virtual at
```

`--virtual at` lists Serialist's simulated devices next to your real ports and opens the
simulated AT modem, so everything works with no hardware attached (it answers `AT` with
`OK`). The built-in simulated devices are `echo`, `echo-lines`, `at`, `firehose`,
`firehose-ansi` and `race`. To open a real port, pick it in the Devices panel, or start
with `--port <PATH> --baud <N>`.

| Flag | What it does |
|---|---|
| `--port <PATH>` | Open this port at startup (`virtual:<NAME>` for a simulated one) |
| `--baud <N>` | Baud rate for `--port` and the Connect field, any positive integer (default 115200) |
| `--virtual [NAME]` | List the simulated devices next to the real ports; with a NAME, also open `virtual:<NAME>` at startup (repeatable; the first is opened unless `--port` is given) |
| `--config-dir <DIR>` | Read and keep the configuration in DIR instead of the user config directory (also `SERIALIST_CONFIG_DIR`) |
| `--script <PATH>` | Run this Lua script against `--port` with no window, then exit: 0 if it ends well, 1 on an error, 2 if it is stopped |
| `--terminal-demo` | Open only the milestone 1 terminal element, fed by an in-memory stream |
| `-h`, `--help`, `-V`, `--version` | Print the help or the version |

Logging follows `RUST_LOG`, for example `RUST_LOG=serialist=debug`.

### The config directory

Settings, key bindings, themes, saved commands, scripts, plugins and history live in one
directory: `~/.config/serialist` on macOS and Linux (where Zed users expect it),
`%APPDATA%\Serialist` on Windows. `--config-dir` or `SERIALIST_CONFIG_DIR` overrides it.
The app watches it and applies changes as you save; there is no restart.

| Path | What it holds |
|---|---|
| `settings.json` | Settings in JSON with comments and Zed's key names. The Serialist menu's Open Settings writes a commented template of every key if the file is missing. |
| `keymap.json` | Key bindings in Zed's keymap format, applied after the defaults. Open Keymap in the same menu writes a commented template. |
| `themes/` | Zed theme files (schema v0.2.0), one `*.json` each. Serialist Dark and Serialist Light are built in. |
| `commands/` | Saved-command collections, one `*.json` each. |
| `scripts/` | Lua scripts, `*.lua` at any depth. The Scripts menu's Open Scripts Folder creates it with two example scripts if it holds none. |
| `plugins/` | Codec plugins, one folder each: `plugin.lua`, or `plugin.wasm` with `plugin.toml`. See [`docs/plugins.md`](docs/plugins.md). |
| `history.jsonl` | The compose bar's history, one JSON string per line. |

A project can also check in `.serialist/settings.json` and `.serialist/commands.json`; the
nearest one found searching up from the working directory is used. Defaults for every
setting, with a comment each, are in
`crates/serialist-core/assets/default_settings.jsonc`.

## Keyboard

These are the bindings the app ships with, read from
`crates/serialist-core/assets/keymaps/{macos,linux,windows}.json`. Your `keymap.json` is
applied after them, so its bindings win, and a key bound to `null` is unbound. A dash means
that section has no entry for the action. Linux and Windows use the same bindings.

| Where | Action | What it does | macOS | Linux and Windows |
|---|---|---|---|---|
| Anywhere | `serialist::Quit` | Quit | `cmd-q` | `ctrl-q` |
| Workspace | `terminal::Clear` | Clear the terminal | `cmd-k` | `ctrl-shift-k` |
| Workspace | `serial::Disconnect` | Disconnect the port | `cmd-w` | `ctrl-shift-w` |
| Workspace | `terminal::Pause` | Pause or resume the view (capture continues) | `cmd-p` | `ctrl-p` |
| Workspace | `terminal::Export` | Export the scrollback, the paused view or the selection | `cmd-s` | `ctrl-shift-s` |
| Workspace | `terminal::ToggleRecord` | Start or stop raw recording to a file | `cmd-shift-r` | `ctrl-shift-r` |
| Workspace | `terminal::ToggleInline` | Switch between inline mode and command mode | `cmd-i` | `ctrl-i` |
| Devices panel | `devices::SelectNext` | Select the next device | `down` | `down` |
| Devices panel | `devices::SelectPrevious` | Select the previous device | `up` | `up` |
| Devices panel | `serial::Connect` | Connect the selected device | `enter` | `enter` |
| Commands panel | `commands::SelectNext` | Select the next command | `down` | `down` |
| Commands panel | `commands::SelectPrevious` | Select the previous command | `up` | `up` |
| Commands panel | `commands::SendSelected` | Send the selected command | `enter` | `enter` |
| Commands panel filter | `commands::SelectPrevious` | Select the previous command | `up` | `up` |
| Commands panel filter | `commands::SelectNext` | Select the next command | `down` | `down` |
| Compose bar input | `compose::HistoryPrevious` | Previous compose-bar history entry | `up` | `up` |
| Compose bar input | `compose::HistoryNext` | Next compose-bar history entry | `down` | `down` |
| Compose bar | `compose::CycleLineEnding` | Cycle the line ending (None, CR, LF, CRLF) | `cmd-e` | `ctrl-shift-e` |
| Compose bar | `compose::SaveAsCommand` | Open the command editor with the compose bar's text | `cmd-alt-s` | `ctrl-alt-s` |
| Terminal | `terminal::Copy` | Copy the selection | `cmd-c` | `ctrl-shift-c` |
| Terminal | `terminal::SelectAll` | Select every retained line | `cmd-a` | `ctrl-shift-a` |
| Terminal | `terminal::Search` | Open the search bar | `cmd-f` | `ctrl-shift-f` |
| Terminal | `terminal::ToggleWrap` | Toggle line wrapping | `alt-z` | `alt-z` |
| Terminal | `terminal::CycleTimestamps` | Cycle the timestamp gutter (off, absolute, relative, delta) | `alt-t` | `alt-t` |
| Terminal | `terminal::ToggleHexView` | Toggle the hex view | `alt-h` | `alt-h` |
| Terminal | `terminal::ToggleFrameStats` | Toggle the frame-time overlay | `cmd-alt-i` | `ctrl-alt-i` |
| Terminal | `terminal::PageUp` | Page up | `pageup` | `pageup` |
| Terminal | `terminal::PageDown` | Page down | `pagedown` | `pagedown` |
| Terminal | `terminal::ScrollToTop` | Scroll to the top | `home`, `cmd-up` | `home`, `ctrl-home` |
| Terminal | `terminal::JumpToBottom` | Jump to the live tail | `end`, `cmd-down` | `end`, `ctrl-end` |
| Terminal, inline mode | `terminal::Paste` | Send the clipboard to the port in paced chunks | `cmd-v` | `ctrl-shift-v` |
| Terminal, inline mode | `terminal::Copy` | Copy the selection | `cmd-c` | `ctrl-shift-c` |
| Terminal, inline mode | `terminal::SelectAll` | Select every retained line | `cmd-a` | `ctrl-shift-a` |
| Terminal, inline mode | `terminal::Search` | Open the search bar | `cmd-f` | `ctrl-shift-f` |
| Terminal, inline mode | `terminal::ToggleFrameStats` | Toggle the frame-time overlay | `cmd-alt-i` | `ctrl-alt-i` |
| Terminal, inline mode | `terminal::PageUp` | Page up | `shift-pageup` | `shift-pageup` |
| Terminal, inline mode | `terminal::PageDown` | Page down | `shift-pagedown` | `shift-pagedown` |
| Terminal, inline mode | `terminal::ScrollToTop` | Scroll to the top | `cmd-up` | — |
| Terminal, inline mode | `terminal::JumpToBottom` | Jump to the live tail | `cmd-down` | — |
| Terminal, inline mode | `terminal::ToggleInline` | Switch between inline mode and command mode | — | `ctrl-i` |
| Terminal, inline mode | `terminal::Clear` | Clear the terminal | — | `ctrl-shift-k` |
| Terminal, inline mode | `serial::Disconnect` | Disconnect the port | — | `ctrl-shift-w` |
| Terminal, inline mode | `terminal::Pause` | Pause or resume the view (capture continues) | — | `ctrl-shift-p` |
| Terminal, inline mode | `terminal::Export` | Export the scrollback, the paused view or the selection | — | `ctrl-shift-s` |
| Terminal, inline mode | `terminal::ToggleRecord` | Start or stop raw recording to a file | — | `ctrl-shift-r` |
| Search bar | `terminal::DismissSearch` | Close the search bar | `escape` | `escape` |

In inline mode the terminal sends every key it can encode to the port, plain ctrl chords
included; the entries in the "Terminal, inline mode" section are the exceptions, which run
as actions instead of being sent. Plain ctrl chords belong to the device there (ctrl-s is
XOFF), which is why Linux and Windows mostly use shifted chords. Pause is the exception at
plain `ctrl-p`, which inline mode sends to the device; in inline mode it is `ctrl-shift-p`.
The chord that leaves inline mode is a setting, `inline.escape_chord` (default `ctrl-]`),
not a keymap entry.

## Scripting

Lua 5.4 scripts run on their own thread against the open session, so a script that waits
for the device never blocks the window:

```lua
local port = assert(serial.current())
port:write("AT\r\n")
print(port:expect("^OK$", { timeout_ms = 1000 }) and "device is up" or "no answer")
```

Start a script from the Script console or the Scripts menu, from a key binding
(`["scripts::Run", { "path": "version_probe.lua" }]` in `keymap.json`), from a saved command
whose payload is `{ "script": "version_probe.lua" }`, from a device profile's
`on_connect`, or with no window: `serialist --port virtual:at --script version_probe.lua`.

[`docs/scripting.md`](docs/scripting.md) has the API table (it is copied from the crate
docs in `crates/serialist-script/src/lib.rs`), the sandbox rules, the triggers and the exit
codes. Two example scripts ship in `crates/serialist-core/assets/scripts/`:
`version_probe.lua` (checks that an AT device answers, then asks for its version) and
`firehose_stats.lua` (counts received lines for two seconds with `on_line`; try it against
`serialist --virtual firehose`).

## Plugins

A plugin is a codec: it frames the received bytes into structured frames, and encodes
structured commands into bytes to send. There are three ways to write one, all behind one
Rust trait (`serialist_core::Codec`):

| Tier | What it is | Where it lives |
|---|---|---|
| Built-in Rust | `text-lines` (one frame per line) and `airoha-race` | compiled into `serialist-plugins` |
| Lua | A folder with `plugin.lua`, which returns `describe`, `decode` and `encode` | `plugins/<name>/plugin.lua` |
| WebAssembly | A folder with `plugin.wasm` (a component) and `plugin.toml`, written against `serialist-plugin-sdk` or any language that can build the WIT world | `plugins/<name>/plugin.wasm` and `plugin.toml` |

The WebAssembly tier sits behind the `wasm` Cargo feature of `serialist-plugins`, which is
off by default: wasmtime and Cranelift make up most of a clean build and add minutes to it.
Without the feature, a `plugin.wasm` folder is found but reported as needing it. The Lua
tier needs nothing extra. Airoha RACE exists in all three tiers, and the tests hold them to
byte-identical output. How to write a Lua or WebAssembly plugin, and how the Decoded panel
and codec payloads in saved commands use plugins, is in [`docs/plugins.md`](docs/plugins.md).

## Installing a release

Release builds are attached to
[GitHub Releases](https://github.com/TheBigShoe/serialist/releases): a `.dmg` for macOS
(Apple Silicon), a `.tar.gz` and a `.deb` for Linux (x86_64), and an `.msi` for Windows
(x86_64). `SHA256SUMS` lists the checksums. **None of them are code-signed yet**, so each
OS warns on first launch:

- **macOS.** Open the `.dmg` and drag Serialist to Applications. The app is signed ad hoc
  but not with an Apple Developer ID and not notarized, so Gatekeeper refuses it at first
  ("Apple could not verify..."). Either Control-click Serialist in Applications, choose
  Open, and confirm (macOS 14 and earlier); or try to open it once, then go to System
  Settings, Privacy & Security, and press Open Anyway next to the Serialist notice
  (macOS 15 and later); or clear the download flag in Terminal:
  `xattr -dr com.apple.quarantine /Applications/Serialist.app`.
- **Windows.** SmartScreen shows "Windows protected your PC". Choose More info, then Run
  anyway. Installing from the `.msi` puts Serialist in Program Files and the Start menu;
  the Customize page has an option to add it to `PATH`.
- **Linux.** `sudo apt install ./serialist_*.deb`, or unpack the `.tar.gz` and run
  `./install.sh` (installs to `~/.local`). The `.deb` pulls in what it needs; for the
  tarball install the Vulkan loader and a Vulkan-capable GPU driver, xkbcommon
  (`libxkbcommon`, `libxkbcommon-x11`), fontconfig, libudev, and the Wayland or X11 client
  libraries (Debian and Ubuntu: `libvulkan1 libxkbcommon0 libxkbcommon-x11-0 libfontconfig1
  libudev1 libwayland-client0 libx11-xcb1`). To open serial ports, join the group that owns
  them (`dialout` on Debian and Ubuntu).

To build the packages yourself, see `docs/releasing.md`.

## License

Licensed under either of Apache License, Version 2.0 (`LICENSE-APACHE`) or MIT license
(`LICENSE-MIT`) at your option. Contributions are accepted under the same terms.

