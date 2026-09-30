//! The wasmtime engine plugins compile against, and the ticker that bounds how long a
//! call may run.
//!
//! Epoch interruption: compiled code checks the engine's epoch at loop headers and
//! function entries and traps once it passes the store's deadline. One thread per engine
//! advances the epoch every [`TICK`] while any call is running and parks otherwise, so
//! an idle app does not wake up a hundred times a second for nothing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};
use std::time::Duration;

use serialist_core::codec::CodecError;
use wasmtime::{Config, Engine, EngineWeak, OptLevel};

/// How often the epoch advances while a call runs: the granularity of the time limit.
pub const TICK: Duration = Duration::from_millis(10);

/// Most wasm stack a call may use; deeper recursion traps.
const MAX_WASM_STACK: usize = 512 * 1024;

/// A wasmtime engine set up for plugins. Cheap to clone; clones share compiled code's
/// engine and its ticker thread. Components compiled with one engine run only with it.
#[derive(Clone)]
pub struct WasmEngine {
    inner: Arc<Inner>,
}

struct Inner {
    engine: Engine,
    ticker: Ticker,
}

impl std::fmt::Debug for WasmEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmEngine").finish_non_exhaustive()
    }
}

impl WasmEngine {
    /// A new engine with its own ticker thread.
    pub fn new() -> Result<Self, CodecError> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .epoch_interruption(true)
            .max_wasm_stack(MAX_WASM_STACK)
            .cranelift_opt_level(OptLevel::Speed);
        let engine = Engine::new(&config).map_err(|err| {
            CodecError::Internal(format!("the WebAssembly engine did not start: {err}"))
        })?;
        let ticker = Ticker::spawn(engine.weak())?;
        Ok(Self {
            inner: Arc::new(Inner { engine, ticker }),
        })
    }

    /// The engine [`load_plugins`](crate::load_plugins) and
    /// [`WasmCodecFactory::load_dir`](super::WasmCodecFactory::load_dir) use: made once
    /// per process, on first use.
    pub fn shared() -> Result<Self, CodecError> {
        static SHARED: OnceLock<Result<WasmEngine, CodecError>> = OnceLock::new();
        SHARED.get_or_init(WasmEngine::new).clone()
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// Mark a call as running until the guard drops, so the epoch advances meanwhile.
    pub(crate) fn busy(&self) -> Busy<'_> {
        self.inner.ticker.busy()
    }
}

/// The deadline, in ticks beyond the current epoch, for a call of at most `limit`: it
/// traps between `limit` and `limit + TICK` after it starts.
pub(crate) fn deadline_ticks(limit: Duration) -> u64 {
    let ticks = limit.as_nanos().div_ceil(TICK.as_nanos()).max(1);
    u64::try_from(ticks).unwrap_or(u64::MAX - 1) + 1
}

struct TickerState {
    /// Calls running now.
    active: AtomicUsize,
    /// The engine is gone: the thread exits.
    shutdown: AtomicBool,
}

struct Ticker {
    state: Arc<TickerState>,
    thread: Thread,
}

impl Ticker {
    fn spawn(engine: EngineWeak) -> Result<Self, CodecError> {
        let state = Arc::new(TickerState {
            active: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
        });
        let shared = Arc::clone(&state);
        let handle = thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || tick(&shared, &engine))
            .map_err(|err| {
                CodecError::Internal(format!("the WebAssembly ticker did not start: {err}"))
            })?;
        Ok(Self {
            state,
            thread: handle.thread().clone(),
        })
    }

    fn busy(&self) -> Busy<'_> {
        if self.state.active.fetch_add(1, Ordering::AcqRel) == 0 {
            self.thread.unpark();
        }
        Busy(&self.state)
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.state.shutdown.store(true, Ordering::Release);
        self.thread.unpark();
    }
}

/// Advance the epoch every tick while a call runs; park while none does. Park and unpark
/// pair up through the thread's token, so a call that starts between the check and the
/// park is never missed.
fn tick(state: &TickerState, engine: &EngineWeak) {
    loop {
        if state.shutdown.load(Ordering::Acquire) {
            return;
        }
        if state.active.load(Ordering::Acquire) == 0 {
            thread::park();
            continue;
        }
        thread::sleep(TICK);
        match engine.upgrade() {
            Some(engine) => engine.increment_epoch(),
            None => return,
        }
    }
}

/// A running call; see [`WasmEngine::busy`].
pub(crate) struct Busy<'a>(&'a TickerState);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_covers_the_limit_and_at_most_one_tick_more() {
        assert_eq!(deadline_ticks(Duration::from_millis(50)), 6);
        assert_eq!(deadline_ticks(Duration::from_millis(45)), 6);
        assert_eq!(deadline_ticks(Duration::ZERO), 2);
        assert_eq!(deadline_ticks(Duration::MAX), u64::MAX);
    }

    #[test]
    fn the_ticker_does_not_keep_the_engine_alive() {
        let engine = WasmEngine::new().unwrap();
        let weak = engine.engine().weak();
        {
            let _busy = engine.busy();
            thread::sleep(TICK * 5);
        }
        drop(engine);
        // The ticker held only a weak reference, so the engine is gone.
        assert!(weak.upgrade().is_none());
    }
}
