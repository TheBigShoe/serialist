# Using Serialist

The reference behind the README: the command line, the window, VT mode, tabs, the four
kinds of port id, recording and replay, the Settings screen, the config directory and
every key binding. Scripts are in [`scripting.md`](scripting.md), plugins in
[`plugins.md`](plugins.md), and building the packages in [`releasing.md`](releasing.md).

## Command line

`--virtual at` lists Serialist's simulated devices next to your real ports and opens the
simulated AT modem, so everything works with no hardware attached (it answers `AT` with
`OK`). The built-in simulated devices are `echo`, `echo-lines`, `at`, `firehose`,
`firehose-ansi`, `race` and `menu` (a U-Boot style boot menu, for VT mode). To open a real
port, pick it in the Devices panel, or start with `--port <PATH> --baud <N>`. A TCP stream or
a recorded capture opens from the Serial menu or with `--port` too; see [Ports](#ports).

| Flag | What it does |
|---|---|
| `--port <ID>` | Open this port at startup in a tab of its own: a path (`/dev/cu.usbserial-1420`, `COM3`), `virtual:<NAME>` for a simulated device, `tcp:<HOST>:<PORT>` for a raw TCP stream or `replay:<FILE>` for a recorded capture (see [Ports](#ports)). Repeatable |
| `--baud <N>` | Baud rate for `--port` and the Connect field, any positive integer (default 115200) |
| `--virtual [NAME]` | List the simulated devices next to the real ports; with a NAME, also open `virtual:<NAME>` at startup in a tab of its own. Repeatable |
| `--config-dir <DIR>` | Read and keep the configuration in DIR instead of the user config directory (also `SERIALIST_CONFIG_DIR`) |
| `--script <PATH>` | Run this Lua script against `--port` with no window, then exit: 0 if it ends well, 1 on an error, 2 if it is stopped |
| `--terminal-demo` | Open only the milestone 1 terminal element, fed by an in-memory stream |
| `-h`, `--help`, `-V`, `--version` | Print the help or the version |

Logging follows `RUST_LOG`, for example `RUST_LOG=serialist=debug`.

## The window

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

## VT mode

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

## Tabs

Every open port has a tab: `serialist --virtual at --virtual race` opens two, the first in
front. Connect in the Devices panel opens a new tab (or goes to the port's tab if it has
one), `cmd-t` opens an empty tab to pick a port for, `cmd-w` closes a tab (asking first while
it records or runs a script), `cmd-1` to `cmd-9` and `cmd-shift-]`/`[` switch, and tabs
reorder by dragging. The tab bar shows once two tabs are open. The status line, the window
title, the Decoded panel and the Script console follow the tab in front (with Settings in
front the status line keeps showing the session you came from, or the config folder when
there is none); the others keep
receiving, recording and running their scripts without drawing, and their label counts the
bytes that arrived meanwhile. When the window closes the tabs are written to `state.json`
and reopen at the next start (the `restore_session` setting; ports named on the command line
open instead; [Ports](#ports) says what happens to TCP and replay tabs).

The button at the left of a session's toolbar (`115200 8N1`), and the port in the status
bar, open its port settings: baud
(any integer, or one from the list), data bits, parity, stop bits, flow control, line ending,
local echo, live DTR and RTS switches and Send break, applied to the open port at once (on a TCP tab
the button reads `TCP` and keeps only the line ending and local echo, and on a replay it
shows the speed; see [Ports](#ports)). The
gear on a Devices row (hover the row) sets the same things for the next connect to that port.
Disconnect in the toolbar closes the port and keeps the scrollback; Connect in its place opens
it again, with the same settings, into the same scrollback.

## Ports

Everything that opens is a port id: what follows `--port`, what a tab's entry in `state.json`
holds, and what a device profile's `match.path` is a prefix of. There are four forms. A
`tcp:` or `replay:` port is never discovered, so it does not appear in the Devices panel.

| Id | Opens |
|---|---|
| `/dev/cu.usbserial-1420`, `COM3` | An OS serial port. |
| `virtual:<name>` | A simulated device (the Devices panel's Simulated group, or `--virtual`). |
| `tcp:<host>:<port>` | A raw TCP byte stream, for serial-to-network bridges such as ser2net in raw mode, ESP-Link or a terminal server's raw port. `host` is a name, an IPv4 address or a bracketed IPv6 address (`tcp:[::1]:4000`) and `port` is 1 to 65535. Raw only: no Telnet negotiation and no RFC 2217. |
| `replay:<file>[?speed=…&end=…]` | A recorded capture played into the session as if a device were sending it. `speed` is `1x`, `4x`, `0.5x` and so on, or `max` (as fast as the session reads); `end` is `disconnect` (the default: the session ends after the last byte) or `hold` (it stays open). |

Two actions open them from the window, in the Serial menu (beside Disconnect) and in the
command palette. Neither has a default key.

- **Connect to TCP…** (`serial::ConnectTcp`, "Serial: Connect TCP" in the palette) asks for
  `host:port` in a small dialog. A pasted `tcp:host:port` works, and an entry that does not
  parse says why under the field and keeps the dialog open.
- **Open Capture…** (`serial::OpenCapture`, "Serial: Open capture") picks a file with the
  platform's open dialog.

`--port` takes the same ids, and so does `--script` (see [`scripting.md`](scripting.md#headless-runs-with---script)).
Quote a replay id in a shell, since `?` is a glob character: `--port 'replay:boot.bin?speed=4x'`.
A device profile applies to these ports as to any other, with `match.path` a prefix of the id:
`{ "match": { "path": "tcp:10.0.0.5:4000" }, "plugin": "airoha-race" }` gives that endpoint a
codec, and `"path": "replay:"` matches every replay.

What the tab shows:

- A TCP tab is titled `host:port` and a replay tab with the capture's file name; the window
  title follows.
- The status line shows the transport's own description, `tcp:bridge.local:4000 (192.168.1.50:4000)`
  or `replay:boot.bin (4x)`, and no line settings, which a stream does not have. A replay with
  no timing file is the exception: its baud rate paces it, so the settings stay
  (`replay:dump.bin (4x, no timing)`, then `115200 8N1`).
- The toolbar's settings button reads `TCP`, and its popover keeps the line ending and local
  echo only: no baud, framing, flow control, DTR, RTS or Send break, because a socket has none
  (a bridge's own serial side is set on the bridge). On a replay the button shows the speed and
  opens a menu (0.25x, 0.5x, 1x, 2x, 4x, 10x, 100x, max); choosing one plays the capture again
  from its first byte at that speed, in the same tab, after the scrollback already there.
- A peer that closes a TCP connection ends the session as "Connection lost", and Connect opens a
  new one. A replay that plays its last byte under `end = disconnect` ends quietly as
  "Disconnected", and Connect plays it again. Whatever is typed or sent to a replay is discarded,
  so inline mode and the compose bar do no harm there.

The `replay` setting gives the speed and the end of a replay whose id does not say:

```jsonc
"replay": { "speed": "1x", "end": "disconnect" }
```

`speed` is `"1x"`, `"4x"`, `"0.5x"`, `"max"` or a bare number, and `end` is `"disconnect"` or
`"hold"`. An id's own `?speed=` and `?end=` win, and a change reaches the next replay that
opens, not one already playing. The Settings screen has no control for it; edit `settings.json`.

Restored tabs: with `restore_session` on, a `tcp:` tab connects again at the next start, as a
serial tab does when its device is plugged in (if the endpoint does not answer, the tab says why
and waits with a Connect button). A `replay:` tab waits with a Connect button instead, because
playing a file the moment the app opens would be a surprise.

## Recording and replay

Record (the toolbar, `cmd-shift-r`, `ctrl-shift-r` on Linux and Windows) asks for a file name
and appends every chunk the port delivers to it, byte for byte, as it always has. It also
writes `<file>.timing` beside it (`boot.bin` gets `boot.bin.timing`): a small text file with one
line per chunk saying when it arrived and where its bytes sit in the raw file, plus a line each
for the link coming up and going down. A replay reads it to play the capture at its original
pace and in its original chunks. The format is written out in the module docs of
`serialist_core::capture` (`crates/serialist-core/src/capture.rs`). The two files are flushed
together, a recording starts only if both can be created, and stopping says
`Recorded 12 KiB to boot.bin (timing in boot.bin.timing)`. Only Record writes a sidecar;
Export's raw bytes are the raw file alone. A replay looks for `<name>.timing` in the capture's
folder, so keep the pair together.

The sidecar is optional. A capture without one, such as a recording made before sidecars existed
or a dump from another tool, still replays: at the baud rate the tab opens with (`default_baud`,
a device profile that matches the id, or `--baud`) times the speed, or as fast as the session
reads at `max`. A `.timing` file that does not parse makes the open fail with a message naming
the port, rather than play at the wrong pace.

## Settings

`cmd-,` (`ctrl-,` on Linux and Windows), the Serialist menu's Settings… or the palette
opens the Settings screen in a tab beside the sessions: Appearance (theme mode, the light
and dark themes, the UI font), Terminal font (family, size, weight, line height, ligatures,
fallbacks, and the `terminal.*` overrides), Display, Session (default baud, line ending,
echo, restore, and inline mode's Backspace, escape chord and paste pacing), Devices (the
device profiles: add, edit with the port settings form, remove, drag to reorder), Keymap
(every binding in effect, yours marked, with the saved commands' keys listed as "command" rows,
with Rebind… on a selected row) and Plugins. It is
a front end to `settings.json` and `keymap.json`, which stay the source of truth: each
change writes one key into the file at once (text fields after a 300 ms pause), keeping
your comments and uncommenting the template's line for the key, and the watcher applies it
like any save. A value nobody set is marked "default"; the arrow beside one you set removes
it again; one a project's `.serialist/settings.json` sets is read-only there. A value the
loader would reject is never written and the reason shows under it, and a `settings.json`
that does not load shows its error and an Open settings.json button instead.

## The config directory

Settings, key bindings, themes, saved commands, scripts, plugins and history live in one
directory: `~/.config/serialist` on macOS and Linux (where Zed users expect it),
`%APPDATA%\Serialist` on Windows. `--config-dir` or `SERIALIST_CONFIG_DIR` overrides it.
The directory is Serialist's own: it never reads Zed's configuration, and only the settings
key names and the theme file format match Zed's, so a Zed theme file can be copied into
`themes/` as it is.
The app watches it and applies changes as you save; there is no restart.

| Path | What it holds |
|---|---|
| `settings.json` | Settings in JSON with comments and Zed's key names, edited by hand or from the Settings screen. The Serialist menu's Open settings.json writes a commented template of every key if the file is missing. |
| `keymap.json` | Key bindings in Zed's keymap format, applied after the defaults. Open Keymap in the same menu writes a commented template. |
| `themes/` | Zed theme files (schema v0.2.0), one `*.json` each. Eight are built in: Serialist Dark and Serialist Light (the defaults), Serialist Ember (warm charcoal, amber accent), Serialist Phosphor (green CRT), Serialist Paper (warm off-white, teal accent), Serialist Contrast (black and white, every text color it sets at 7:1 or more), and Fadetouched and Fadetouched Blur (dark teal-green, by Arishawke, MIT; see `THIRD_PARTY_LICENSES.md`; Serialist does not blur windows, so Fadetouched Blur differs from Fadetouched only in its translucent surface colors over an opaque window). |
| `commands/` | Saved-command collections, one `*.json` each. |
| `scripts/` | Lua scripts, `*.lua` at any depth. The Scripts menu's Open Scripts Folder creates it with two example scripts if it holds none. |
| `plugins/` | Codec plugins, one folder each: `plugin.lua`, or `plugin.wasm` with `plugin.toml`. None is installed at first; `plugins/examples/` holds copies of the bundled examples, which decode nothing until installed. See [`plugins.md`](plugins.md). |
| `history.jsonl` | The compose bar's history, one JSON string per line. |
| `state.json` | The tabs open when the window last closed (ports, line settings, codec, input mode, monitor or VT), reopened at the next start. Written by the app. |

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
| Workspace | `serialist::OpenSettingsUi` | Open the Settings screen in a tab | `cmd-,` | `ctrl-,` |
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
