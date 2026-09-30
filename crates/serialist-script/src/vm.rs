//! One run: a fresh sandboxed Lua VM, driven on the script thread's executor.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::Sender;
use mlua::chunk::ChunkMode;
use mlua::{Error as LuaError, Function, HookTriggers, Lua, LuaOptions, StdLib, Table, VmState};
use tokio::runtime::Runtime;
use tokio::sync::Notify;
use tokio::task::LocalSet;

use crate::host::{HostServices, Job, RunControl, RunId, ScriptEvent, ScriptOutcome};
use crate::port::PortState;
use crate::race::{Either, race};

/// The error a wait raises when the run is stopped.
pub(crate) const STOPPED: &str = "script stopped";

pub(crate) fn runtime_error(message: impl std::fmt::Display) -> LuaError {
    LuaError::runtime(message)
}

/// Wraps the `pcall`, `xpcall`, `coroutine.*` and `load` built-ins. Runs once per VM,
/// before the script, and returns the table of coroutines the script creates.
///
/// - A stopped script cannot swallow its stop: an error caught by `pcall`, `xpcall` or
///   `coroutine.resume` is raised again while the stop flag is set.
/// - `coroutine.create` and `coroutine.wrap` record their coroutines (weakly), so a
///   waiting call made inside one fails with a clear message instead of yielding to the
///   script's own `resume`.
/// - `load` only takes text chunks.
const PRELUDE: &str = r##"
local raw_pcall, raw_xpcall, raw_resume, raw_create, raw_wrap, raw_running, raw_load, stopping = ...
local user_threads = setmetatable({}, { __mode = "k" })

local function recheck(ok, ...)
  if not ok and stopping() then
    error((...), 0)
  end
  return ok, ...
end

pcall = function(f, ...) return recheck(raw_pcall(f, ...)) end
xpcall = function(f, handler, ...) return recheck(raw_xpcall(f, handler, ...)) end
coroutine.resume = function(co, ...) return recheck(raw_resume(co, ...)) end

local function mark(f)
  if type(f) ~= "function" then
    return f
  end
  return function(...)
    user_threads[raw_running()] = true
    return f(...)
  end
end
coroutine.create = function(f) return raw_create(mark(f)) end
coroutine.wrap = function(f) return raw_wrap(mark(f)) end

load = function(chunk, name, mode, ...)
  if select("#", ...) > 0 then
    return raw_load(chunk, name, "t", ...)
  end
  return raw_load(chunk, name, "t")
end

return user_threads
"##;

/// State shared by everything one run registers with Lua. Lives on the script thread.
pub(crate) struct RunCtx {
    pub(crate) id: RunId,
    pub(crate) control: Arc<RunControl>,
    events: Sender<ScriptEvent>,
    pub(crate) services: HostServices,
    prompts: Cell<u64>,
    /// The first error from an `on_line` callback; it ends the run.
    failure: RefCell<Option<LuaError>>,
    failed: Notify,
    /// Ports the script opened; closed when it ends.
    owned: RefCell<Vec<Rc<PortState>>>,
    /// What `serial.current()` returns, positioned when the run starts.
    pub(crate) current: Option<Rc<PortState>>,
    /// Coroutines the script created itself, as weak keys.
    user_threads: RefCell<Option<Table>>,
}

impl RunCtx {
    fn new(job: &Job) -> Self {
        let current = job
            .services
            .session
            .as_ref()
            .map(|session| Rc::new(PortState::new(Arc::clone(session), false)));
        Self {
            id: job.id,
            control: Arc::clone(&job.control),
            events: job.events.clone(),
            services: job.services.clone(),
            prompts: Cell::new(0),
            failure: RefCell::new(None),
            failed: Notify::new(),
            owned: RefCell::new(Vec::new()),
            current,
            user_threads: RefCell::new(None),
        }
    }

    pub(crate) fn emit(&self, event: ScriptEvent) {
        // Nobody listening is fine: the run still has an outcome.
        let _ = self.events.send(event);
    }

    pub(crate) fn next_prompt_id(&self) -> u64 {
        let id = self.prompts.get() + 1;
        self.prompts.set(id);
        id
    }

    /// End the run with `err` (the first one wins).
    pub(crate) fn fail(&self, err: LuaError) {
        let mut failure = self.failure.borrow_mut();
        if failure.is_none() {
            *failure = Some(err);
            self.failed.notify_one();
        }
    }

    async fn failure(&self) -> LuaError {
        loop {
            if let Some(err) = self.failure.borrow_mut().take() {
                return err;
            }
            self.failed.notified().await;
        }
    }

    /// Runs `wait` unless the run is stopped first, in which case the wait is dropped
    /// and the script gets an error.
    pub(crate) async fn or_stop<F: Future>(&self, wait: F) -> mlua::Result<F::Output> {
        match race(wait, self.control.stopped()).await {
            Either::Left(value) => Ok(value),
            Either::Right(()) => Err(runtime_error(STOPPED)),
        }
    }

    /// Waiting calls suspend the coroutine the executor drives; inside a coroutine the
    /// script made, the suspension would land in the script's own `resume` instead.
    pub(crate) fn check_waitable(&self, lua: &Lua, what: &str) -> mlua::Result<()> {
        let user_threads = self.user_threads.borrow();
        if let Some(threads) = &*user_threads
            && threads.raw_get::<bool>(lua.current_thread())?
        {
            return Err(runtime_error(format!(
                "{what} cannot wait inside a coroutine the script created \
                 (coroutine.create or coroutine.wrap); call it from the script body, \
                 a required module or an on_line callback"
            )));
        }
        Ok(())
    }

    /// A port the script opened, to be closed when the run ends.
    pub(crate) fn adopt(&self, port: &Rc<PortState>) {
        self.owned.borrow_mut().push(Rc::clone(port));
    }

    fn close_owned(&self) {
        for port in self.owned.borrow_mut().drain(..) {
            port.close_now();
        }
    }
}

/// Runs one job to its outcome on the calling (script) thread.
pub(crate) fn run(runtime: &Runtime, job: &Job) -> ScriptOutcome {
    let ctx = Rc::new(RunCtx::new(job));
    let lua = match build(&ctx) {
        Ok(lua) => lua,
        Err(err) => return ScriptOutcome::Error(format!("could not start Lua: {err}")),
    };
    ctx.emit(ScriptEvent::Started);
    tracing::debug!(run = %ctx.id, name = %job.source.name, "script started");
    let local = LocalSet::new();
    let outcome = local.block_on(runtime, drive(&lua, &ctx, job));
    // Cancels the on_line tasks while the VM is still alive.
    drop(local);
    ctx.close_owned();
    drop(lua);
    outcome
}

fn build(ctx: &Rc<RunCtx>) -> mlua::Result<Lua> {
    let libs = StdLib::COROUTINE
        | StdLib::TABLE
        | StdLib::STRING
        | StdLib::UTF8
        | StdLib::MATH
        | StdLib::OS;
    let lua = Lua::new_with(libs, LuaOptions::default())?;
    let limits = ctx.services.limits;
    if limits.memory_bytes > 0 {
        lua.set_memory_limit(limits.memory_bytes)?;
    }
    install_hook(&lua, ctx, limits.instruction_check_every.max(1))?;
    let user_threads = sandbox(&lua, ctx)?;
    *ctx.user_threads.borrow_mut() = Some(user_threads);
    crate::api::install(&lua, ctx)?;
    Ok(lua)
}

/// Every `every` instructions, in every coroutine: once the run is stopped, the first
/// check yields to the executor (which then drops the script), and any later check
/// raises an error, for code that cannot yield (a `table.sort` comparator, say).
fn install_hook(lua: &Lua, ctx: &RunCtx, every: u32) -> mlua::Result<()> {
    let control = Arc::clone(&ctx.control);
    let checks_after_stop = Cell::new(0u32);
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(every),
        move |_lua, _debug| {
            if !control.is_stopped() {
                return Ok(VmState::Continue);
            }
            let n = checks_after_stop.get();
            checks_after_stop.set(n.saturating_add(1));
            if n == 0 {
                Ok(VmState::Yield)
            } else {
                Err(runtime_error(STOPPED))
            }
        },
    )
}

/// Strips the standard library down to what scripts may use, then runs the prelude.
fn sandbox(lua: &Lua, ctx: &RunCtx) -> mlua::Result<Table> {
    let globals = lua.globals();
    let os: Table = globals.get("os")?;
    let safe_os = lua.create_table()?;
    for name in ["time", "clock", "date"] {
        safe_os.set(name, os.get::<mlua::Value>(name)?)?;
    }
    globals.set("os", safe_os)?;
    // `io`, `package` and `debug` are never loaded; `require` and `dofile` come back
    // as sandboxed versions in `api::install`.
    for name in ["io", "package", "debug", "loadfile", "dofile", "require"] {
        globals.set(name, mlua::Nil)?;
    }
    let string: Table = globals.get("string")?;
    string.set("dump", mlua::Nil)?;

    let coroutine: Table = globals.get("coroutine")?;
    let control = Arc::clone(&ctx.control);
    let stopping = lua.create_function(move |_, ()| Ok(control.is_stopped()))?;
    lua.load(PRELUDE).set_name("=sandbox").call((
        globals.get::<Function>("pcall")?,
        globals.get::<Function>("xpcall")?,
        coroutine.get::<Function>("resume")?,
        coroutine.get::<Function>("create")?,
        coroutine.get::<Function>("wrap")?,
        coroutine.get::<Function>("running")?,
        globals.get::<Function>("load")?,
        stopping,
    ))
}

async fn drive(lua: &Lua, ctx: &Rc<RunCtx>, job: &Job) -> ScriptOutcome {
    let main = match lua
        .load(job.source.code.as_str())
        .set_name(format!("@{}", job.source.name))
        .set_mode(ChunkMode::Text)
        .into_function()
    {
        Ok(main) => main,
        Err(err) => return ScriptOutcome::Error(describe(&err)),
    };
    let ended = race(
        main.call_async::<()>(()),
        race(ctx.control.stopped(), ctx.failure()),
    )
    .await;
    match ended {
        Either::Left(Ok(())) => ScriptOutcome::Ok,
        Either::Left(Err(_)) if ctx.control.is_stopped() => ScriptOutcome::Stopped,
        Either::Left(Err(err)) => ScriptOutcome::Error(describe(&err)),
        Either::Right(Either::Left(())) => ScriptOutcome::Stopped,
        Either::Right(Either::Right(err)) => ScriptOutcome::Error(describe(&err)),
    }
}

/// The message for an error that ended a run: mlua's text, which carries the Lua
/// traceback, without its trailing newline.
fn describe(err: &LuaError) -> String {
    match err {
        LuaError::MemoryError(message) => format!("memory error: {message}"),
        other => other.to_string().trim_end().to_owned(),
    }
}
