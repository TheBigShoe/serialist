//! Lua scripting for Serialist: scripts that drive a session without ever blocking the UI.
//!
//! # Threads
//!
//! A [`ScriptHost`] owns one thread, `serialist-script`, which runs queued scripts one at
//! a time. Each run gets a fresh Lua 5.4 VM (mlua, vendored) and is driven by a
//! current-thread tokio executor on that thread. Every Lua call that waits (`sleep`,
//! `port:expect`, `port:read`, `port:read_line`, `ui.prompt`, `serial.open`,
//! `port:close`, `require`, `dofile`) is a Rust `async fn`, so a waiting script suspends
//! its coroutine and the thread goes back to the executor: `on_line` callbacks run while
//! the main body sleeps, and the thread never blocks on I/O. File reads and port opens go
//! to a small pool of `serialist-script-io` workers.
//!
//! Reads never touch the transport: a script reads the session's scrollback
//! ([`ScriptSession::store`]) and sleeps on a [`LineBell`] the ingest thread rings after
//! each stored chunk, so a script sees exactly what the terminal shows.
//!
//! # Host boundary
//!
//! The app implements [`ScriptSession`] (the port behind `serial.current()`),
//! [`ScriptUi`] (prompts, notifications, log lines), [`CommandSender`] (saved commands)
//! and [`PortOpener`] (`serial.open`), and passes them in [`HostServices`].
//! [`run_headless`] does all of that for `--script`, with [`HeadlessSession`],
//! [`HeadlessOpener`] and [`StdioUi`].
//!
//! ```no_run
//! use std::sync::Arc;
//! use serialist_script::{HostServices, ScriptEvent, ScriptHost, ScriptSource, StdioUi};
//!
//! let host = ScriptHost::new(HostServices::new(Arc::new(StdioUi::new())));
//! let run = host.run(ScriptSource::new("hello.lua", r#"print("hello")"#));
//! for event in run.events() {
//!     match event {
//!         ScriptEvent::Output(line) => println!("{line}"),
//!         ScriptEvent::Finished(outcome) => { println!("{outcome:?}"); break }
//!         _ => {}
//!     }
//! }
//! ```
//!
//! # The Lua API
//!
//! "Waits" means the call suspends the script (never the thread) and ends early with an
//! error if the run is stopped. Timeouts are `opts.timeout_ms` (or `opts` itself as a
//! number), default 1000; `0` looks once without waiting and `math.huge` waits (up to 30
//! days).
//!
//! | Call | Arguments | Returns | Waits |
//! |---|---|---|---|
//! | `serial.current()` | | the run's port, or `nil, err` without a session | no |
//! | `serial.ports()` | | list of `{id, display_name, kind, vid?, pid?, product?, manufacturer?, serial_number?}` | no |
//! | `serial.open{port=, match=, baud=}` | `port` id, or `match` (`vid`, `pid`, `product`, `manufacturer`, `serial_number`, `path`), optional `baud` | a port, or `nil, err` | yes (worker thread) |
//! | `port:write(data)` | string, or table of byte values | nothing; error if the session closed | no (queued) |
//! | `port:write_hex(text)` | `"05 5A 00"`, `"055A00"`, `"0x05,0x5A"` | nothing | no (queued) |
//! | `port:read(n, opts)` | byte count, `{timeout_ms}` | up to `n` raw bytes (fewer at timeout), or `nil, "timeout" \| "closed"` | yes, until `n` bytes |
//! | `port:read_line(opts)` | `{timeout_ms}` | next received line (text only), or `nil, "timeout" \| "closed"` | yes |
//! | `port:expect(pattern, opts)` | regex (Rust syntax, smart case), `{timeout_ms}` | `captures, elapsed_ms` (`captures[1]` the match, `[2..]` groups, `false` for a group that did not take part, `.line` the whole line), or `nil, elapsed_ms, "timeout" \| "closed"` | yes |
//! | `port:on_line(fn)` | `fn(line)` | handle with `cancel()`, `active()`, and `<close>` support | no; `fn` runs for each later line, may wait |
//! | `port:discard()` | | nothing: skips everything unread | no |
//! | `port:set{dtr=, rts=, baud=}` | booleans, positive integer | nothing | no (queued) |
//! | `port:close()` | | nothing; closes ports from `serial.open`, only detaches `serial.current()` | yes (worker thread) |
//! | `port:description()`, `port:id()` | | strings | no |
//! | `sleep(ms)` | milliseconds | nothing | yes |
//! | `print(...)` | values | one [`ScriptEvent::Output`], tab-joined | no |
//! | `log.debug/info/warn/error(...)` | values | [`ScriptUi::log`] and an `Output` line `"[info] ..."`, space-joined | no |
//! | `ui.prompt(label, default)` | strings | the answer, or `nil` if dismissed; sends [`ScriptEvent::Prompt`] first | yes |
//! | `ui.notify(text)` | string | nothing | no |
//! | `commands.send(name, params)` | name, `{param = value}` | `true`, or `nil, err` | no |
//! | `hex.encode(bytes, sep)` | string or byte table, separator (default `" "`) | `"05 5A 00"` | no |
//! | `hex.decode(text)` | hex text | byte string; error if malformed | no |
//! | `bytes.from_table(t)`, `bytes.to_table(s)` | byte table / string | string / byte table | no |
//! | `require(name)`, `dofile(path)` | `"lib.util"` / `"lib/util.lua"` in the scripts directory | the module's value / the file's results | yes (worker thread) |
//!
//! `string.pack` and `string.unpack` are there for binary frames.
//!
//! **Reading is a stream.** Each port object has one read position that `read`,
//! `read_line` and `expect` consume from, like a socket, so nothing is lost between
//! calls: two lines that arrive in one chunk come back from two `read_line` calls,
//! `expect("A")` then `expect("B")` finds `B` even when both arrived together, and a
//! reply that lands between `write` and `expect` is still found. The position starts
//! where the stream stood when the run began (`serial.current()`), or at the start of a
//! session `serial.open` made, so a script never sees what came before it. `read(n)`
//! takes raw bytes from the position; `read_line()` takes the next complete received
//! line (or, after a `read` took part of a line, the rest of that line);
//! `expect(pattern)` moves past the first matching line (and leaves the position alone
//! on a timeout); `discard()` skips to the end of what has arrived. `on_line` does not
//! touch the position: it sees every line from the call on. A line counts once a line
//! feed ends it or another line starts after it.
//!
//! **Patterns are regexes**, not Lua patterns, with the same smart case as saved
//! commands: `port:expect([[^VAL=(\d+)]])`. Only received lines count, never echoes of
//! what was sent or app notices.
//!
//! **Ending.** A run ends when its main chunk returns; `on_line` handlers still active
//! then are cancelled. An error in an `on_line` callback ends the run with that error.
//!
//! # Sandbox and limits
//!
//! - Libraries: `string`, `table`, `math`, `utf8`, `coroutine`, and `os.time`,
//!   `os.clock`, `os.date` only. No `io`, `debug`, `package`, `loadfile`,
//!   `string.dump`; `load` takes text chunks only.
//! - `require` and `dofile` read only inside [`HostServices::scripts_dir`] and reject
//!   `..`, absolute paths and symlinks that lead out.
//! - Memory: [`Limits::memory_bytes`] (64 MiB) caps the VM; going over raises a Lua
//!   memory error, which ends the script with a message, never the process.
//! - Stop: an instruction hook checks the stop flag every
//!   [`Limits::instruction_check_every`] (10 000) instructions, so `while true do end`
//!   stops too. A pending wait raises `script stopped` at once, so `<close>` handlers
//!   run, but every later wait fails the same way and the executor drops the script at
//!   its next suspension. `pcall`, `xpcall` and `coroutine.resume` cannot swallow a stop.
//! - Waiting calls work in the main body, required modules and `on_line` callbacks, not
//!   inside coroutines the script creates with `coroutine.create` or `coroutine.wrap`
//!   (they fail with a message saying so).

mod api;
mod bell;
mod headless;
mod hex;
mod host;
mod port;
mod race;
mod services;
mod vm;

pub use bell::{BellSink, LineBell};
pub use headless::{HeadlessOpener, HeadlessSession, StdioUi, run_headless};
pub use hex::{HexError, decode_hex, encode_hex};
pub use host::{
    HostServices, Limits, RunId, SCRIPT_THREAD_NAME, SCRIPT_WORKER_THREAD_NAME, ScriptEvent,
    ScriptHost, ScriptOutcome, ScriptRun, ScriptSource,
};
pub use services::{
    CommandError, CommandSender, LogLevel, OpenError, OpenRequest, PortOpener, PromptFuture,
    ScriptSession, ScriptUi,
};
