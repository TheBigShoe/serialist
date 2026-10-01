# Serialist

A native, GPU-rendered serial terminal built on [GPUI](https://gpui.rs), the UI framework
behind the Zed editor. macOS first, Linux and Windows from the same code.

- Devices appear the moment they are plugged in.
- Any baud rate, full framing and flow control settings, per-device profiles.
- Two modes: inline interactive, and saved commands you can send with a click or a key.
- Lua scripting and protocol plugins you install (an Airoha RACE example ships with the app).
- Fonts and themes configured the way Zed does it; Zed theme files load unchanged.
- Fast: no dropped bytes at 3 Mbaud, frames under 8 ms at a million lines of scrollback.
- Every test runs without hardware.

## Status

Milestones 0 to 5 of `docs/plan.md` have landed: the session engine and GPUI shell, the
terminal element over a page-store scrollback, Zed-format settings, themes and keymaps that
apply live, inline interactive and saved-command modes, Lua scripting with a headless
`--script` mode, and protocol plugins in Lua and WebAssembly. Milestone 6 has
started: CI on macOS, Linux and Windows, release packaging, tabs for several sessions
(with a port settings popover and session restore) and full terminal emulation (VT mode,
for U-Boot menus and Linux consoles, next to the monitor log) are in. The plan's "Status as built" section lists where the build
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
`firehose-ansi`, `race` and `menu` (a U-Boot style boot menu, for VT mode). To open a real
port, pick it in the Devices panel, or start with `--port <PATH> --baud <N>`.

| Flag | What it does |
|---|---|
| `--port <PATH>` | Open this port at startup in a tab of its own (`virtual:<NAME>` for a simulated one). Repeatable |
| `--baud <N>` | Baud rate for `--port` and the Connect field, any positive integer (default 115200) |
| `--virtual [NAME]` | List the simulated devices next to the real ports; with a NAME, also open `virtual:<NAME>` at startup in a tab of its own. Repeatable |
| `--config-dir <DIR>` | Read and keep the configuration in DIR instead of the user config directory (also `SERIALIST_CONFIG_DIR`) |
| `--script <PATH>` | Run this Lua script against `--port` with no window, then exit: 0 if it ends well, 1 on an error, 2 if it is stopped |
| `--terminal-demo` | Open only the milestone 1 terminal element, fed by an in-memory stream |
| `-h`, `--help`, `-V`, `--version` | Print the help or the version |

Logging follows `RUST_LOG`, for example `RUST_LOG=serialist=debug`.

### The window

Devices and Commands sit in the left dock, the session in the center, the Decoded panel and
the Script console in the right dock, and the status bar along the bottom. Each dock has a
rail of icons that opens and closes its panels. The Decoded panel (whose icon shows once a
codec plugin is installed) opens when a session decodes with a codec and the Script console
when a script prints; drag a dock's edge to
resize it (the widths are kept in `state.json`). Below 1100 px of window width the right dock
folds to its rail, and below 900 px the left one does; a panel opened from the rail stays open.

The session toolbar is one row of icons, each with its shortcut in its tooltip: the port and
its settings, Connect or Disconnect, Command / Inline, Pause, Record and Clear, Search, Hex,
VT mode, Timestamps and Wrap, an Export menu (text, the VT screen, raw bytes, decoded
frames), and, once a codec
plugin is installed, the codec menu (which also turns summaries and hidden frames on and
off, installs the bundled examples and opens the plugins folder). Whatever does not fit the width
goes to the `…` menu at its right end. The status bar shows the port and its settings (click
for the settings), the newest notice, then the mode (click to switch), RX and TX with their
rates, and chips for the codec, VT mode, a pause, a recording and a running script.

`cmd-shift-p` (`ctrl-shift-p` on Linux and Windows) opens the command palette: every action,
saved command and script, filtered as you type, with its key binding; Enter runs it.

### VT mode

By default a session is in monitor mode: a log of every line received, with colors and
other escapes applied line by line. VT mode shows a terminal screen instead, which the
device draws on with cursor addressing, so a U-Boot `bootmenu`, a Linux console, `top` or
an editor look as they would in a terminal. `alt-v` in the terminal (or the toolbar's
`>_` button) switches a session; `terminal.emulation` (`"monitor"` or `"vt"`) sets the
default, and a device profile's `"emulation"` sets it per device. Try it with
`serialist --virtual menu`: the `>_` button, then `cmd-i` (inline mode) and the arrow keys.

In VT mode the screen is sized to the terminal pane (a device that asks with `CSI 18 t`
learns the new size), the cursor is drawn where the device puts it, the arrows follow
the device's cursor key mode, pastes are bracketed when it asks, its window title shows
on the tab and its bell flashes the status dot. Queries such as device attributes and
the cursor position are answered at once. The log keeps recording underneath: search
reads it (the search bar says so), raw and text export and recording are the same as in
monitor mode, and Export adds the screen's rows as text. Switching to VT mode starts a
blank screen that the next bytes draw on; what arrived before is not replayed into it.
Pause holds the screen as it was; Clear hides the log's lines and leaves the screen, which
is the device's, alone.

### Tabs

Every open port has a tab: `serialist --virtual at --virtual race` opens two, the first in
front. Connect in the Devices panel opens a new tab (or goes to the port's tab if it has
one), `cmd-t` opens an empty tab to pick a port for, `cmd-w` closes a tab (asking first while
it records or runs a script), `cmd-1` to `cmd-9` and `cmd-shift-]`/`[` switch, and tabs
reorder by dragging. The tab bar shows once two tabs are open. The status line, the window
title, the Decoded panel and the Script console follow the tab in front; the others keep
receiving, recording and running their scripts without drawing, and their label counts the
bytes that arrived meanwhile. When the window closes the tabs are written to `state.json`
and reopen at the next start (the `restore_session` setting; ports named on the command line
open instead).

The button at the left of a session's toolbar (`115200 8N1`), and the port in the status
bar, open its port settings: baud
(any integer, or one from the list), data bits, parity, stop bits, flow control, line ending,
local echo, live DTR and RTS switches and Send break, applied to the open port at once. The
gear on a Devices row (hover the row) sets the same things for the next connect to that port.
Disconnect in the toolbar closes the port and keeps the scrollback; Connect in its place opens
it again, with the same settings, into the same scrollback.

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
| `plugins/` | Codec plugins, one folder each: `plugin.lua`, or `plugin.wasm` with `plugin.toml`. None is installed at first; `plugins/examples/` holds copies of the bundled examples, which decode nothing until installed. See [Plugins](#plugins). |
| `history.jsonl` | The compose bar's history, one JSON string per line. |
| `state.json` | The tabs open when the window last closed (ports, line settings, codec, input mode), reopened at the next start. Written by the app. |

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
| Workspace | `serial::Disconnect` | Disconnect the port (asking first while it records or runs a script) | `cmd-shift-w` | `ctrl-alt-w` |
| Workspace | `tabs::NewTab` | Open an empty tab and focus the Devices panel | `cmd-t` | `ctrl-shift-t` |
| Workspace | `tabs::CloseTab` | Close the tab: disconnect, stop its script and recording (asking first while either runs) | `cmd-w` | `ctrl-shift-w` |
| Workspace | `tabs::NextTab` | Go to the next tab | `cmd-shift-]` | `ctrl-shift-]`, `ctrl-pagedown` |
| Workspace | `tabs::PreviousTab` | Go to the previous tab | `cmd-shift-[` | `ctrl-shift-[`, `ctrl-pageup` |
| Workspace | `tabs::ActivateTab1` to `9` | Go to tab 1 to 9 | `cmd-1` to `cmd-9` | `ctrl-1` to `ctrl-9` |
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
| Terminal | `terminal::ToggleEmulation` | Switch between monitor mode and VT mode | `alt-v` | `alt-v` |
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
| Terminal, inline mode | `serial::Disconnect` | Disconnect the port | — | `ctrl-alt-w` |
| Terminal, inline mode | `tabs::NewTab` | Open an empty tab | — | `ctrl-shift-t` |
| Terminal, inline mode | `tabs::CloseTab` | Close the tab | — | `ctrl-shift-w` |
| Terminal, inline mode | `tabs::NextTab` | Go to the next tab | — | `ctrl-shift-]` |
| Terminal, inline mode | `tabs::PreviousTab` | Go to the previous tab | — | `ctrl-shift-[` |
| Terminal, inline mode | `terminal::Pause` | Pause or resume the view (capture continues) | — | `ctrl-shift-p` |
| Terminal, inline mode | `terminal::Export` | Export the scrollback, the paused view or the selection | — | `ctrl-shift-s` |
| Terminal, inline mode | `terminal::ToggleRecord` | Start or stop raw recording to a file | — | `ctrl-shift-r` |
| Search bar | `terminal::DismissSearch` | Close the search bar | `escape` | `escape` |

In inline mode the terminal sends every key it can encode to the port, plain ctrl chords
included; the entries in the "Terminal, inline mode" section are the exceptions, which run
as actions instead of being sent. Plain ctrl chords belong to the device there (ctrl-s is
XOFF), which is why Linux and Windows mostly use shifted chords. Pause is the exception at
plain `ctrl-p`, which inline mode sends to the device; in inline mode it is `ctrl-shift-p`.
So are the tab numbers `ctrl-1` to `ctrl-9` and `ctrl-pageup`/`ctrl-pagedown`: in inline
mode, `ctrl-shift-[` and `ctrl-shift-]` switch tabs. On macOS no `cmd` chord is sent to the
device, so every Workspace binding works in inline mode too.
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
structured commands into bytes to send. Decoders are plugins you install and enable: the
app has none built in and none is active in a fresh config directory, so until one is
installed the toolbar shows no codec menu and the right dock no Decoded panel icon.

Plugins live in the config directory's `plugins/` folder
(`~/.config/serialist/plugins` on macOS and Linux, `%APPDATA%\Serialist\plugins` on
Windows), one folder each, and each registers under its folder's name. Copying a plugin's
folder in enables it at once; deleting it removes it.

| Kind | What it is | Where it lives |
|---|---|---|
| Lua | A folder with `plugin.lua`, which returns `describe`, `decode` and `encode` | `plugins/<name>/plugin.lua` |
| WebAssembly | A folder with `plugin.wasm` (a component) and `plugin.toml`, written against `serialist-plugin-sdk` or any language that can build the WIT world | `plugins/<name>/plugin.wasm` and `plugin.toml` |

**The bundled example.** The app ships an Airoha RACE plugin (in Lua, and also as
WebAssembly in a build with the `wasm` feature) but does not enable it. To install it, open
the command palette (`cmd-shift-p`) and run "Install example plugin: Airoha RACE", or use
"Install example plugin…" in the codec menu once another plugin is installed. That copies
the example into `plugins/airoha-race/`, and the codec appears. Then try it:
`serialist --virtual race`, pick `airoha-race` in the codec menu, and send "RACE version"
from the Commands panel. "Open plugins folder" (palette, codec menu, or the Serialist menu)
also writes read-only copies of the examples into `plugins/examples/`, which do not load.

A device profile's `"plugin"` selects a codec only when that plugin is installed; otherwise
the port connects without one, the status line names the missing plugin with an Install
button, and the Devices row shows the plugin's name greyed. A saved command whose payload
names a codec that is not installed is not sent ("Install the airoha-race plugin to send
this command").

The WebAssembly kind sits behind the `wasm` Cargo feature of `serialist-plugins` (the app
forwards it: `cargo build -p serialist --features wasm`), which is
off by default: wasmtime and Cranelift make up most of a clean build and add minutes to it.
Without the feature, a `plugin.wasm` folder is found but reported as needing it. The Lua
kind needs nothing extra. The Airoha RACE plugin exists in Lua and WebAssembly and as a Rust
reference codec that the tests hold both to, byte for byte; the Rust one is never
registered in the app. How to write a Lua or WebAssembly plugin, and how the Decoded panel
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

