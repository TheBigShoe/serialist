//! Port objects: `serial.current()` and `serial.open{...}` return one of these.
//!
//! # Reading is a stream
//!
//! A port object reads what the device sent as a stream with one read position, like a
//! socket: `read`, `read_line` and `expect` each consume from it, and nothing is lost
//! between calls, so two lines that arrive in one chunk come back from two `read_line`
//! calls, and `expect("A")` then `expect("B")` finds `B` even when both arrived together.
//! The position starts where the stream stood when the port object was made (the run's
//! start for `serial.current()`), so a script never sees what came before it, and a
//! reply that arrives between `write` and `expect` is never missed.
//!
//! - `read(n)` takes raw bytes from the position.
//! - `read_line()` takes the next complete received line after it, or the rest of the
//!   line a `read` took the start of.
//! - `expect(pattern)` looks at complete received lines from the position on and moves
//!   past the first that matches; on a timeout the position stays put.
//! - `discard()` skips everything received so far.
//! - `on_line(fn)` does not touch the position: it sees every line from the call on.
//!
//! Only received lines count: echoes of what was sent and app notices are skipped, as
//! for saved commands' matchers. A line counts once it is complete (ended by a line
//! feed) or another line has started after it.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use mlua::{
    Function, IntoLuaMulti, Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value,
};
use serialist_core::matcher::compile_pattern;
use serialist_core::store::Snapshot;
use serialist_core::text::{Direction, LineId, LineSource, StyledLine};
use serialist_core::{ControlLine, SerialConfig};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::hex::decode_hex;
use crate::race::{Either, race};
use crate::services::ScriptSession;
use crate::vm::{RunCtx, runtime_error};

/// Used when a wait's `timeout_ms` is not given.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);
/// Longest wait accepted; `math.huge` means this.
const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 3600);
/// Most lines an `on_line` task takes from one snapshot before looking again.
const ON_LINE_BATCH: usize = 256;

/// A position in the received stream.
#[derive(Clone, Copy, Debug)]
struct Cursor {
    /// Stream offset of the next unread byte.
    pos: u64,
    /// No line before this one can start at or after `pos`; scans begin here.
    line: LineId,
}

impl Cursor {
    /// The end of everything received so far. The line in progress is behind it.
    fn at_end(snap: &Snapshot) -> Self {
        Self {
            pos: snap.raw_range().end,
            line: snap.end(),
        }
    }

    /// The next complete received line with bytes at or after `pos`, moving `line`
    /// past everything examined. Stops at the line in progress. A line whose start a
    /// `read` already took comes back as the rest of its raw bytes, without the line
    /// ending.
    fn next_line(&mut self, snap: &Snapshot) -> Option<StyledLine> {
        let end = snap.end();
        let mut id = self.line.max(snap.first_line());
        let mut found = None;
        while id < end {
            let Some(mut line) = snap.line(id) else {
                break;
            };
            if !line.complete && id.next() >= end {
                break;
            }
            id = id.next();
            if line.direction == Direction::Rx && line.raw.end > self.pos {
                if line.raw.start < self.pos {
                    let mut rest = Vec::new();
                    for piece in snap.raw(self.pos..line.raw.end) {
                        rest.extend_from_slice(piece);
                    }
                    while matches!(rest.last(), Some(b'\n' | b'\r')) {
                        rest.pop();
                    }
                    line.text = String::from_utf8_lossy(&rest).into_owned();
                }
                found = Some(line);
                break;
            }
        }
        self.line = id;
        found
    }

    /// Consume through `line`.
    fn consume(&mut self, line: &StyledLine) {
        self.pos = self.pos.max(line.raw.end);
        self.line = self.line.max(line.id.next());
    }
}

/// How a wait ended without its result.
enum Missed {
    TimedOut,
    Closed,
}

impl Missed {
    fn reason(&self) -> &'static str {
        match self {
            Missed::TimedOut => "timeout",
            Missed::Closed => "closed",
        }
    }

    /// `nil, "timeout"` or `nil, "closed"`.
    fn into_lua_multi(self, lua: &Lua) -> mlua::Result<MultiValue> {
        (Value::Nil, self.reason()).into_lua_multi(lua)
    }
}

/// Look at the store until `look` finds something, `deadline` passes, the session can
/// deliver no more, or the run stops (an error).
async fn wait_for<T>(
    ctx: &RunCtx,
    session: &dyn ScriptSession,
    deadline: Instant,
    mut look: impl FnMut(&Snapshot) -> Option<T>,
) -> mlua::Result<Result<T, Missed>> {
    let mut listener = session.bell().listen();
    loop {
        // Read before the snapshot: once closed, the snapshot holds everything.
        let closed = listener.is_closed();
        let snap = session.store().snapshot();
        if let Some(found) = look(&snap) {
            return Ok(Ok(found));
        }
        if closed {
            return Ok(Err(Missed::Closed));
        }
        drop(snap);
        let timeout = tokio::time::sleep_until(deadline);
        match race(listener.rung(), race(timeout, ctx.control.stopped())).await {
            Either::Left(()) => {}
            Either::Right(Either::Left(())) => return Ok(Err(Missed::TimedOut)),
            Either::Right(Either::Right(())) => return Err(runtime_error(crate::vm::STOPPED)),
        }
    }
}

/// A wait's options as a timeout: `{ timeout_ms = n }`, or just `n`, or nothing for
/// [`DEFAULT_TIMEOUT`].
fn timeout_of(opts: &Value, what: &str) -> mlua::Result<Duration> {
    let ms = match opts {
        Value::Nil => None,
        Value::Integer(ms) => Some(*ms as f64),
        Value::Number(ms) => Some(*ms),
        Value::Table(opts) => opts.get::<Option<f64>>("timeout_ms")?,
        other => {
            return Err(runtime_error(format!(
                "{what}: options must be a table such as {{ timeout_ms = 500 }}, got {}",
                other.type_name()
            )));
        }
    };
    ms.map_or(Ok(DEFAULT_TIMEOUT), |ms| duration_ms(ms, what))
}

/// A millisecond count from Lua as a duration: not negative, capped at 30 days.
pub(crate) fn duration_ms(ms: f64, what: &str) -> mlua::Result<Duration> {
    if ms.is_nan() || ms < 0.0 {
        return Err(runtime_error(format!(
            "{what}: a time in milliseconds must be 0 or more, got {ms}"
        )));
    }
    Ok(Duration::try_from_secs_f64(ms / 1000.0)
        .unwrap_or(MAX_TIMEOUT)
        .min(MAX_TIMEOUT))
}

fn deadline_after(timeout: Duration) -> Instant {
    Instant::now() + timeout
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// A string, or a table of integers 0 to 255, as bytes.
pub(crate) fn bytes_arg(value: &Value, what: &str) -> mlua::Result<Vec<u8>> {
    match value {
        Value::String(s) => Ok(s.as_bytes().to_vec()),
        Value::Table(table) => {
            let mut out = Vec::with_capacity(table.raw_len());
            for (i, item) in table.sequence_values::<Value>().enumerate() {
                let byte = match item? {
                    Value::Integer(n) => u8::try_from(n).ok(),
                    Value::Number(n) if n.fract() == 0.0 && (0.0..=255.0).contains(&n) => {
                        Some(n as u8)
                    }
                    _ => None,
                };
                let Some(byte) = byte else {
                    return Err(runtime_error(format!(
                        "{what}: item {} is not a byte (an integer from 0 to 255)",
                        i + 1
                    )));
                };
                out.push(byte);
            }
            Ok(out)
        }
        other => Err(runtime_error(format!(
            "{what}: expected a string or a table of bytes, got {}",
            other.type_name()
        ))),
    }
}

/// One port object's state: the session, whether the script owns it, and the read
/// position. Shared by every Lua handle to the same port.
pub(crate) struct PortState {
    session: Arc<dyn ScriptSession>,
    /// Opened by the script, so closed by it.
    owned: bool,
    closed: Cell<bool>,
    cursor: Cell<Cursor>,
}

impl PortState {
    /// A port the script opened (`owned`) reads its session's whole stream; the run's
    /// own session is read from where it stands now.
    pub(crate) fn new(session: Arc<dyn ScriptSession>, owned: bool) -> Self {
        let cursor = if owned {
            Cursor {
                pos: 0,
                line: LineId::ZERO,
            }
        } else {
            Cursor::at_end(&session.store().snapshot())
        };
        Self {
            session,
            owned,
            closed: Cell::new(false),
            cursor: Cell::new(cursor),
        }
    }

    fn name(&self) -> String {
        self.session.port_id().to_string()
    }

    fn check_open(&self, what: &str) -> mlua::Result<()> {
        if self.closed.get() {
            return Err(runtime_error(format!(
                "port:{what}: {} is closed",
                self.name()
            )));
        }
        Ok(())
    }

    fn write(&self, bytes: Vec<u8>, what: &str) -> mlua::Result<()> {
        self.check_open(what)?;
        self.session.write(bytes).map_err(|_| {
            runtime_error(format!(
                "port:{what}: the session on {} has closed",
                self.name()
            ))
        })
    }

    /// Close a port the script opened, blocking; used when the run ends.
    pub(crate) fn close_now(&self) {
        if self.owned && !self.closed.replace(true) {
            self.session.close();
        }
    }
}

/// The Lua handle to a port.
#[derive(Clone)]
pub(crate) struct Port {
    pub(crate) state: Rc<PortState>,
    pub(crate) ctx: Rc<RunCtx>,
}

impl Port {
    async fn read(&self, n: usize, timeout: Duration) -> mlua::Result<Result<Vec<u8>, Missed>> {
        let state = &self.state;
        let start = state.cursor.get().pos;
        // Up to `n` bytes from the read position (or the oldest retained byte, if
        // eviction has passed it), and where they end.
        let take = |snap: &Snapshot| -> (u64, Vec<u8>) {
            let raw = snap.raw_range();
            let from = start.max(raw.start);
            let to = raw.end.min(from.saturating_add(n as u64));
            let mut out = Vec::with_capacity(to.saturating_sub(from) as usize);
            for piece in snap.raw(from..to) {
                out.extend_from_slice(piece);
            }
            (from + out.len() as u64, out)
        };
        let full = wait_for(
            &self.ctx,
            state.session.as_ref(),
            deadline_after(timeout),
            |snap| {
                let raw = snap.raw_range();
                let available = raw.end.saturating_sub(start.max(raw.start));
                (available >= n as u64).then(|| take(snap))
            },
        )
        .await?;
        let got = match full {
            Ok(taken) => taken,
            // Out of time or data: return what there is, if anything.
            Err(missed) => {
                let taken = take(&state.session.store().snapshot());
                if taken.1.is_empty() {
                    return Ok(Err(missed));
                }
                taken
            }
        };
        let (end, bytes) = got;
        let mut cursor = state.cursor.get();
        cursor.pos = cursor.pos.max(end);
        state.cursor.set(cursor);
        Ok(Ok(bytes))
    }

    async fn read_line(&self, timeout: Duration) -> mlua::Result<Result<String, Missed>> {
        let state = &self.state;
        let mut cursor = state.cursor.get();
        let found = wait_for(
            &self.ctx,
            state.session.as_ref(),
            deadline_after(timeout),
            |snap| cursor.next_line(snap),
        )
        .await?;
        // Lines skipped on the way (echoes, notices) never count, so keep the progress
        // even without a result.
        let mut latest = state.cursor.get();
        latest.line = latest.line.max(cursor.line);
        if let Ok(line) = &found {
            latest.consume(line);
        }
        state.cursor.set(latest);
        Ok(found.map(|line| line.text))
    }

    async fn expect(
        &self,
        lua: &Lua,
        pattern: &str,
        timeout: Duration,
    ) -> mlua::Result<MultiValue> {
        let regex = compile_pattern(pattern)
            .map_err(|err| runtime_error(format!("port:expect: bad pattern {pattern:?}: {err}")))?;
        let state = &self.state;
        let started = Instant::now();
        let mut scan = state.cursor.get();
        let found = wait_for(
            &self.ctx,
            state.session.as_ref(),
            deadline_after(timeout),
            |snap| {
                while let Some(line) = scan.next_line(snap) {
                    if let Some(caps) = regex.captures(&line.text) {
                        let groups: Vec<Option<String>> = caps
                            .iter()
                            .map(|group| group.map(|m| m.as_str().to_owned()))
                            .collect();
                        return Some((line, groups));
                    }
                }
                None
            },
        )
        .await?;
        let elapsed = millis(started.elapsed());
        match found {
            Ok((line, groups)) => {
                let mut cursor = state.cursor.get();
                cursor.consume(&line);
                state.cursor.set(cursor);
                let captures = lua.create_table()?;
                let value = |group: &Option<String>| match group {
                    Some(text) => lua.create_string(text).map(Value::String),
                    None => Ok(Value::Boolean(false)),
                };
                for (i, (group, name)) in groups.iter().zip(regex.capture_names()).enumerate() {
                    captures.raw_set(i + 1, value(group)?)?;
                    if let Some(name) = name {
                        captures.raw_set(name, value(group)?)?;
                    }
                }
                captures.raw_set("line", line.text)?;
                (captures, elapsed).into_lua_multi(lua)
            }
            Err(missed) => (Value::Nil, elapsed, missed.reason()).into_lua_multi(lua),
        }
    }

    fn on_line(&self, callback: Function) -> LineHandle {
        let sub = Rc::new(Subscription {
            cancelled: Cell::new(false),
            wake: Notify::new(),
        });
        let start = Cursor::at_end(&self.state.session.store().snapshot());
        tokio::task::spawn_local(on_line_task(
            Rc::clone(&self.ctx),
            Arc::clone(&self.state.session),
            callback,
            Rc::clone(&sub),
            start,
        ));
        LineHandle(sub)
    }

    fn set(&self, options: &Table) -> mlua::Result<()> {
        self.state.check_open("set")?;
        let mut dtr = None;
        let mut rts = None;
        let mut baud = None;
        for pair in options.pairs::<String, Value>() {
            let (key, value) = pair?;
            match (key.as_str(), value) {
                ("dtr", Value::Boolean(on)) => dtr = Some(on),
                ("rts", Value::Boolean(on)) => rts = Some(on),
                ("baud", Value::Integer(rate)) if rate > 0 => {
                    baud = Some(u32::try_from(rate).map_err(|_| {
                        runtime_error(format!("port:set: baud {rate} is too high"))
                    })?);
                }
                ("dtr" | "rts", other) => {
                    return Err(runtime_error(format!(
                        "port:set: {key} must be true or false, got {}",
                        other.type_name()
                    )));
                }
                ("baud", other) => {
                    return Err(runtime_error(format!(
                        "port:set: baud must be a positive integer, got {other:?}"
                    )));
                }
                (other, _) => {
                    return Err(runtime_error(format!(
                        "port:set: unknown setting {other:?} (dtr, rts and baud are known)"
                    )));
                }
            }
        }
        let session = &self.state.session;
        let closed = |_: serialist_core::SessionClosed| {
            runtime_error(format!(
                "port:set: the session on {} has closed",
                self.state.name()
            ))
        };
        if let Some(on) = dtr {
            session.set_control(ControlLine::Dtr, on).map_err(closed)?;
        }
        if let Some(on) = rts {
            session.set_control(ControlLine::Rts, on).map_err(closed)?;
        }
        if let Some(baud) = baud {
            let serial = SerialConfig {
                baud,
                ..session.serial_config()
            };
            session.reconfigure(serial).map_err(closed)?;
        }
        Ok(())
    }

    async fn close(&self) -> mlua::Result<()> {
        let state = &self.state;
        if state.closed.replace(true) || !state.owned {
            // `serial.current()` belongs to the user: closing it only detaches the script.
            return Ok(());
        }
        let session = Arc::clone(&state.session);
        tokio::task::spawn_blocking(move || session.close())
            .await
            .map_err(|err| runtime_error(format!("port:close: {err}")))
    }
}

impl UserData for Port {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("write", |_, this, data: Value| {
            this.state.write(bytes_arg(&data, "port:write")?, "write")
        });
        methods.add_method("write_hex", |_, this, text: String| {
            let bytes =
                decode_hex(&text).map_err(|err| runtime_error(format!("port:write_hex: {err}")))?;
            this.state.write(bytes, "write_hex")
        });
        methods.add_async_method("read", |lua, this, (n, opts): (i64, Value)| {
            let checked = this
                .ctx
                .check_waitable(&lua, "port:read")
                .and_then(|()| this.state.check_open("read"))
                .and_then(|()| {
                    let n = usize::try_from(n).ok().filter(|&n| n > 0).ok_or_else(|| {
                        runtime_error(format!("port:read: n must be 1 or more, got {n}"))
                    })?;
                    Ok((n, timeout_of(&opts, "port:read")?))
                });
            let port = Port::clone(&this);
            async move {
                let (n, timeout) = checked?;
                match port.read(n, timeout).await? {
                    Ok(bytes) => lua.create_string(&bytes)?.into_lua_multi(&lua),
                    Err(missed) => missed.into_lua_multi(&lua),
                }
            }
        });
        methods.add_async_method("read_line", |lua, this, opts: Value| {
            let checked = this
                .ctx
                .check_waitable(&lua, "port:read_line")
                .and_then(|()| this.state.check_open("read_line"))
                .and_then(|()| timeout_of(&opts, "port:read_line"));
            let port = Port::clone(&this);
            async move {
                let timeout = checked?;
                match port.read_line(timeout).await? {
                    Ok(text) => text.into_lua_multi(&lua),
                    Err(missed) => missed.into_lua_multi(&lua),
                }
            }
        });
        methods.add_async_method("expect", |lua, this, (pattern, opts): (String, Value)| {
            let checked = this
                .ctx
                .check_waitable(&lua, "port:expect")
                .and_then(|()| this.state.check_open("expect"))
                .and_then(|()| timeout_of(&opts, "port:expect"));
            let port = Port::clone(&this);
            async move {
                let timeout = checked?;
                port.expect(&lua, &pattern, timeout).await
            }
        });
        methods.add_method("on_line", |_, this, callback: Function| {
            this.state.check_open("on_line")?;
            Ok(this.on_line(callback))
        });
        methods.add_method("discard", |_, this, ()| {
            this.state.check_open("discard")?;
            let cursor = Cursor::at_end(&this.state.session.store().snapshot());
            this.state.cursor.set(cursor);
            Ok(())
        });
        methods.add_method("set", |_, this, options: Table| this.set(&options));
        methods.add_async_method("close", |lua, this, ()| {
            let checked = this.ctx.check_waitable(&lua, "port:close");
            let port = Port::clone(&this);
            async move {
                checked?;
                port.close().await
            }
        });
        methods.add_method("description", |_, this, ()| {
            Ok(this.state.session.description())
        });
        methods.add_method("id", |_, this, ()| Ok(this.state.name()));
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("serial port {}", this.state.name()))
        });
    }
}

struct Subscription {
    cancelled: Cell<bool>,
    wake: Notify,
}

impl Subscription {
    fn cancel(&self) {
        if !self.cancelled.replace(true) {
            self.wake.notify_one();
        }
    }
}

/// What `port:on_line` returns: `cancel()` stops the callbacks, and so does leaving the
/// scope of a `local h <close> = port:on_line(...)`. Losing the handle does not.
pub(crate) struct LineHandle(Rc<Subscription>);

impl UserData for LineHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |_, this, ()| {
            this.0.cancel();
            Ok(())
        });
        methods.add_method("active", |_, this, ()| Ok(!this.0.cancelled.get()));
        methods.add_meta_method(MetaMethod::Close, |_, this, _: mlua::MultiValue| {
            this.0.cancel();
            Ok(())
        });
    }
}

/// Calls `callback` with each received line from `cursor` on, one at a time and in
/// order, until cancelled, the session closes, or a callback fails (which ends the run).
async fn on_line_task(
    ctx: Rc<RunCtx>,
    session: Arc<dyn ScriptSession>,
    callback: Function,
    sub: Rc<Subscription>,
    mut cursor: Cursor,
) {
    let mut listener = session.bell().listen();
    let mut batch = Vec::with_capacity(ON_LINE_BATCH);
    loop {
        if sub.cancelled.get() {
            return;
        }
        let closed = listener.is_closed();
        let snap = session.store().snapshot();
        while batch.len() < ON_LINE_BATCH {
            match cursor.next_line(&snap) {
                Some(line) => batch.push(line),
                None => break,
            }
        }
        drop(snap);
        let full = batch.len() == ON_LINE_BATCH;
        for line in batch.drain(..) {
            if sub.cancelled.get() {
                return;
            }
            if let Err(err) = callback.call_async::<()>(line.text).await {
                ctx.fail(err);
                return;
            }
        }
        if full {
            continue;
        }
        if closed {
            return;
        }
        race(listener.rung(), sub.wake.notified()).await;
    }
}
