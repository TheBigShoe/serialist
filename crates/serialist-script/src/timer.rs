//! `timer.after(ms, fn)` and `timer.every(ms, fn)`: callbacks on the script thread's
//! executor, beside the script's own waits.
//!
//! A timer is a task on the run's executor that sleeps until it is due and then calls its
//! function, as an `on_line` callback is called: one call at a time, the function may
//! wait, and an error in it ends the run. A timer never queues calls. `every` schedules
//! its next call one interval after the last was due; when a call ran so long that the
//! next one is already late, it is skipped and the timer is due one interval after the
//! call returned. So a timer costs one task whatever its interval and however slow its
//! function, and the number of active timers is capped by
//! [`Limits::max_callbacks`](crate::Limits::max_callbacks).

use std::rc::Rc;
use std::time::Duration;

use mlua::{Function, Lua, Table};
use tokio::time::Instant;

use crate::callback::{CallbackHandle, Slot};
use crate::port::duration_ms;
use crate::race::race;
use crate::vm::{RunCtx, runtime_error};

/// The shortest interval `timer.every` takes.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

pub(crate) fn timer_table(lua: &Lua, ctx: &Rc<RunCtx>) -> mlua::Result<Table> {
    let timer = lua.create_table()?;
    for (name, repeat) in [("after", false), ("every", true)] {
        let ctx = Rc::clone(ctx);
        let what = format!("timer.{name}");
        timer.set(
            name,
            lua.create_function(move |_, (ms, callback): (f64, Function)| {
                let delay = duration_ms(ms, &what)?;
                if repeat && delay < MIN_INTERVAL {
                    return Err(runtime_error(format!(
                        "{what}: the interval must be at least 1 ms, got {ms}"
                    )));
                }
                let slot = Slot::claim(&ctx, &what)?;
                tokio::task::spawn_local(timer_task(
                    Rc::clone(&ctx),
                    callback,
                    Rc::clone(&slot),
                    delay,
                    repeat,
                ));
                Ok(CallbackHandle(slot))
            })?,
        )?;
    }
    Ok(timer)
}

/// Calls `callback` after `delay`, and again every `delay` if `repeat`, until cancelled
/// or a call fails (which ends the run).
async fn timer_task(
    ctx: Rc<RunCtx>,
    callback: Function,
    slot: Rc<Slot>,
    delay: Duration,
    repeat: bool,
) {
    let mut due = Instant::now() + delay;
    loop {
        race(tokio::time::sleep_until(due), slot.wake.notified()).await;
        if !slot.is_active() {
            return;
        }
        if Instant::now() < due {
            continue;
        }
        if let Err(err) = callback.call_async::<()>(()).await {
            slot.release();
            ctx.fail(err);
            return;
        }
        if !repeat {
            slot.release();
            return;
        }
        due += delay;
        let now = Instant::now();
        if due <= now {
            due = now + delay;
        }
    }
}
