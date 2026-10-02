# Scripting

Serialist runs Lua 5.4 scripts against the open session. A script can write to the port,
wait for a reply, react to received lines, ask the user a question and send saved
commands. It runs on its own thread, so a script that waits for the device never blocks
the window, and it reads the same scrollback the terminal shows.

This page is the reference. The API table and the sandbox rules are copied from the crate
docs of `serialist-script` (`crates/serialist-script/src/lib.rs`); if they ever disagree,
the crate docs are the source. The README has the short version.

## A first script

`version_probe.lua`, one of the two examples that ship in
`crates/serialist-core/assets/scripts/`:

```lua
-- version_probe.lua: check that an AT device answers, then ask for its version.
--
-- Run it from the Script console, a key binding or a saved command against the
-- simulator's AT modem (`serialist --virtual at`), or headless:
-- `serialist --port virtual:at --script version_probe.lua`.

local port = assert(serial.current())  -- or serial.open{ match = { product = "Airoha" }, baud = 921600 }
port:write("AT\r\n")
local ok = port:expect("^OK$", { timeout_ms = 1000 })
assert(ok, "no OK from the device")
log.info("matched", ok[1])
for i = 1, 3 do
  port:write("AT+VER?\r\n")
  local m = port:expect([[^\+VER: (\S+)]], { timeout_ms = 500 })
  log.info("value", i, m and m[2] or "timeout")
  sleep(100)
end
```

The other, `firehose_stats.lua`, counts received lines for two seconds with an `on_line`
callback (`serialist --virtual firehose` gives it something to count).

## Where scripts live

- Scripts are `*.lua` files under `scripts/` in the config directory (`~/.config/serialist`
  on macOS and Linux, `%APPDATA%\Serialist` on Windows, or the `--config-dir` directory).
  The Script console lists them at any depth up to eight folders, skipping hidden files and
  folders and symlinked folders. The folder is watched, so the list follows your edits.
- The Scripts menu's Open Scripts Folder creates the folder and, if it holds no `*.lua`
  file directly inside it, writes the two example scripts there. A folder with scripts of
  your own is left alone.
- A path that a key binding, a saved command or a device profile gives is relative to the
  scripts folder. An absolute path is used as it is. A relative path that is not in the
  scripts folder but is in the config directory (spelled `scripts/init.lua`, say) is found
  there too.
- Every run starts a fresh Lua VM. A session runs one script at a time; one started while
  another runs waits for it.
- With several ports open, each tab's session has its own script thread. A script runs on
  the session of the tab that was in front when it started (the console's Run buttons and
  REPL, a key binding, a saved command) or of the port that connected (`on_connect`), and
  stays there: `serial.current()` is that session, `commands.send` sends on it, and a
  saved command's `expect` is watched on the session it was sent on, whichever tab is in
  front meanwhile. A script in a background tab keeps running; its output goes to that
  tab's Script console output, which the console shows when the tab is in front again.
  Closing a tab, or disconnecting its port, stops its script.

## Triggers

| Trigger | How |
|---|---|
| Script console | The right-dock panel lists the scripts, each with a Run button. Its input line runs Lua as a script (`=expr` prints `expr`). Its header has Stop, Clear and Folder buttons. |
| Scripts menu | One `Run <path>` entry per script (the first 40), then Stop Script, Clear Script Console and Open Scripts Folder. |
| Key binding | The action `scripts::Run` with a path, in `keymap.json`: `{ "context": "Workspace", "bindings": { "cmd-alt-1": ["scripts::Run", { "path": "version_probe.lua" }] } }` (`ctrl-alt-1` on Linux and Windows). The bundled keymaps bind no script. `scripts::Stop`, `scripts::ClearConsole`, `scripts::RunInline` and `scripts::OpenScriptsFolder` can be bound the same way. |
| Saved command | A command whose payload is `{ "script": "version_probe.lua" }` runs the script when sent, in place of sending bytes. |
| Device profile | `on_connect` in a `devices` profile in `settings.json` names a script that runs once when a matching port has connected (after the opener has set DTR and RTS). |
| Headless | `serialist --port <PATH> --script <FILE>` runs one script with no window and exits. See [Headless runs](#headless-runs-with---script). |

## The Lua API

"Waits" means the call suspends the script (never the thread) and ends early with an
error if the run is stopped. Timeouts are `opts.timeout_ms` (or `opts` itself as a
number), default 1000; `0` looks once without waiting and `math.huge` waits (up to 30
days).

| Call | Arguments | Returns | Waits |
|---|---|---|---|
| `serial.current()` | | the run's port, or `nil, err` without a session | no |
| `serial.ports()` | | list of `{id, display_name, kind, vid?, pid?, product?, manufacturer?, serial_number?}` | no |
| `serial.open{port=, match=, baud=}` | `port` id, or `match` (`vid`, `pid`, `product`, `manufacturer`, `serial_number`, `path`), optional `baud` | a port, or `nil, err` | yes (worker thread) |
| `port:write(data)` | string, or table of byte values | nothing; error if the session closed | no (queued) |
| `port:write_hex(text)` | `"05 5A 00"`, `"055A00"`, `"0x05,0x5A"` | nothing | no (queued) |
| `port:read(n, opts)` | byte count, `{timeout_ms}` | up to `n` raw bytes (fewer at timeout), or `nil, "timeout" \| "closed"` | yes, until `n` bytes |
| `port:read_line(opts)` | `{timeout_ms}` | next received line (text only), or `nil, "timeout" \| "closed"` | yes |
| `port:expect(pattern, opts)` | regex (Rust syntax, smart case), `{timeout_ms}` | `captures, elapsed_ms` (`captures[1]` the match, `[2..]` groups, `false` for a group that did not take part, `.line` the whole line), or `nil, elapsed_ms, "timeout" \| "closed"` | yes |
| `port:on_line(fn)` | `fn(line)` | handle with `cancel()`, `active()`, and `<close>` support | no; `fn` runs for each later line, may wait |
| `port:discard()` | | nothing: skips everything unread | no |
| `port:set{dtr=, rts=, baud=}` | booleans, positive integer | nothing | no (queued) |
| `port:close()` | | nothing; closes ports from `serial.open`, only detaches `serial.current()` | yes (worker thread) |
| `port:description()`, `port:id()` | | strings | no |
| `sleep(ms)` | milliseconds | nothing | yes |
| `print(...)` | values | one `ScriptEvent::Output`, tab-joined | no |
| `log.debug/info/warn/error(...)` | values | `ScriptUi::log` and an `Output` line `"[info] ..."`, space-joined | no |
| `ui.prompt(label, default)` | strings | the answer, or `nil` if dismissed; sends `ScriptEvent::Prompt` first | yes |
| `ui.notify(text)` | string | nothing | no |
| `commands.send(name, params)` | name, `{param = value}` | `true`, or `nil, err` | no |
| `hex.encode(bytes, sep)` | string or byte table, separator (default `" "`) | `"05 5A 00"` | no |
| `hex.decode(text)` | hex text | byte string; error if malformed | no |
| `bytes.from_table(t)`, `bytes.to_table(s)` | byte table / string | string / byte table | no |
| `require(name)`, `dofile(path)` | `"lib.util"` / `"lib/util.lua"` in the scripts directory | the module's value / the file's results | yes (worker thread) |

`string.pack` and `string.unpack` are there for binary frames.

**Reading is a stream.** Each port object has one read position that `read`,
`read_line` and `expect` consume from, like a socket, so nothing is lost between
calls: two lines that arrive in one chunk come back from two `read_line` calls,
`expect("A")` then `expect("B")` finds `B` even when both arrived together, and a
reply that lands between `write` and `expect` is still found. The position starts
where the stream stood when the run began (`serial.current()`), or at the start of a
session `serial.open` made, so a script never sees what came before it. `read(n)`
takes raw bytes from the position; `read_line()` takes the next complete received
line (or, after a `read` took part of a line, the rest of that line);
`expect(pattern)` moves past the first matching line (and leaves the position alone
on a timeout); `discard()` skips to the end of what has arrived. `on_line` does not
touch the position: it sees every line from the call on. A line counts once a line
feed ends it or another line starts after it.

**Patterns are regexes**, not Lua patterns, with the same smart case as saved
commands: `port:expect([[^VAL=(\d+)]])`. Only received lines count, never echoes of
what was sent or app notices.

**Ending.** A run ends when its main chunk returns; `on_line` handlers still active
then are cancelled. An error in an `on_line` callback ends the run with that error.

In the table, `ScriptEvent::Output` is one line of script output (a line in the Script
console, or on stdout when headless), `ScriptEvent::Prompt` is a prompt asking for an
answer, and `ScriptUi::log` is the app's log.

A few details of the app's side of `commands.send(name, params)`:

- `name` is `collection/group/name`, `collection/name`, or a bare name. A bare name is the
  first command called that, exactly and then ignoring case, in the order the Commands
  panel lists them. `params` values are turned into text.
- It queues the send and returns at once. The command goes out from the main thread as if
  you had clicked it in the Commands panel: echo, expected reply and all.
- A bad name, a command whose payload is a script ("a script cannot start another"), and a
  command whose bytes cannot be built from `params` all return `nil` and a message.
- Headless, there are no saved commands: the call raises "saved commands are not available
  in this host".

`ui.notify(text)` prints `notice: <text>` in the Script console and shows a notice; headless
it goes to stderr as `notice: <text>`.

The plan (`docs/plan.md`) lists `port:on_frame`, timers and `codecs.<name>.encode` in the
scripting API. The table above has none of them.

## Sandbox and limits

- Libraries: `string`, `table`, `math`, `utf8`, `coroutine`, and `os.time`,
  `os.clock`, `os.date` only. No `io`, `debug`, `package`, `loadfile`,
  `string.dump`; `load` takes text chunks only.
- `require` and `dofile` read only inside `HostServices::scripts_dir` and reject
  `..`, absolute paths and symlinks that lead out.
- Memory: `Limits::memory_bytes` (64 MiB) caps the VM; going over raises a Lua
  memory error, which ends the script with a message, never the process.
- Stop: an instruction hook checks the stop flag every
  `Limits::instruction_check_every` (10 000) instructions, so `while true do end`
  stops too. A pending wait raises `script stopped` at once, so `<close>` handlers
  run, but every later wait fails the same way and the executor drops the script at
  its next suspension. `pcall`, `xpcall` and `coroutine.resume` cannot swallow a stop.
- Waiting calls work in the main body, required modules and `on_line` callbacks, not
  inside coroutines the script creates with `coroutine.create` or `coroutine.wrap`
  (they fail with a message saying so).

## Headless runs with `--script`

```sh
serialist --port virtual:at --script version_probe.lua
serialist --port /dev/cu.usbserial-1420 --baud 921600 --script probe.lua
```

`--script` needs `--port` (a real port path, `virtual:<NAME>` for a simulated device,
which turns the simulator on as it does for the window, `tcp:<HOST>:<PORT>` for a raw TCP
stream, or `replay:<FILE>[?speed=…&end=…]` for a recorded capture, which plays at the speed
and end of the `replay` setting unless its id says otherwise). The port is opened with the line
settings the window would use: the device profile that matches the port, over
`default_baud`, with `--baud` over both. The script runs with that session as
`serial.current()`, and the port closes when it ends, letting queued writes go out first.

How the API behaves without the app around it:

- `print` and `log.*` write to stdout, one line each; stdout carries the script's output
  and nothing else. Notices and errors go to stderr, and so does the app's log, where
  `log.*` lines also arrive at their level; it shows `warn` and above unless `RUST_LOG`
  says otherwise.
- `ui.prompt(label, default)` prints the label (and the default in brackets) to stderr and
  reads one line from stdin. An empty line takes the default; end of input answers `nil`.
- `ui.notify(text)` writes `notice: <text>` to stderr.
- `serial.open{ port = "<id>" }` works, and takes any id `--port` does, `tcp:` and `replay:`
  ones included; `match = {...}` does not, because there is no port list to match against
  (it returns `nil` and a message). `serial.ports()` returns an empty list. (In the window
  scripts get no `serial.open`: it returns `nil` and a message, and a script uses
  `serial.current()`, which works on a `tcp:` or `replay:` tab as on any port.)
- `commands.send` raises an error (no saved commands).
- `require` and `dofile` read from the script file's own folder.

### Exit codes

| Code | Meaning |
|---|---|
| 0 | The script's main chunk returned. (`--help` and `--version` also exit 0.) |
| 1 | The script ended with an error: a syntax error, an uncaught runtime error (the message and the Lua traceback go to stderr), a memory error, an error in an `on_line` callback, or the port could not be opened. |
| 2 | The script was stopped, or nothing ran: a bad flag, `--script` without `--port`, a script file that cannot be read, or a `--virtual` device that does not exist. |
