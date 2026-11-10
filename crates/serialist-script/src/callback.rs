//! Counted callbacks: what `timer.after`, `timer.every` and `port:on_frame` return.
//!
//! Each active one takes a slot from the run's [`Limits::max_callbacks`](crate::Limits)
//! and gives it back when it is cancelled, fires for the last time, or fails, so a script
//! that starts them in a loop gets an error instead of a heap that grows without bound.
//! (`port:on_line` predates the limit and does not count, so no script that ran before
//! changes meaning.)

use std::cell::Cell;
use std::rc::Rc;

use mlua::{MetaMethod, UserData, UserDataMethods};
use tokio::sync::Notify;

use crate::vm::{RunCtx, runtime_error};

/// One active callback's state, shared by its task and its Lua handle.
pub(crate) struct Slot {
    active: Cell<bool>,
    /// Woken when the callback is cancelled, so its task ends at once.
    pub(crate) wake: Notify,
    ctx: Rc<RunCtx>,
}

impl Slot {
    /// Take one of the run's callback slots, or fail with a message naming `what`.
    pub(crate) fn claim(ctx: &Rc<RunCtx>, what: &str) -> mlua::Result<Rc<Self>> {
        let limit = ctx.services.limits.max_callbacks;
        let active = ctx.callbacks.get();
        if active >= limit {
            return Err(runtime_error(format!(
                "{what}: this run already has {active} timers and frame handlers active \
                 (the most a run may have); cancel one first"
            )));
        }
        ctx.callbacks.set(active + 1);
        Ok(Rc::new(Self {
            active: Cell::new(true),
            wake: Notify::new(),
            ctx: Rc::clone(ctx),
        }))
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.get()
    }

    /// Give the slot back and wake the task; later calls do nothing.
    pub(crate) fn release(&self) {
        if self.active.replace(false) {
            self.ctx
                .callbacks
                .set(self.ctx.callbacks.get().saturating_sub(1));
            self.wake.notify_one();
        }
    }
}

/// What a timer or a frame handler returns: `cancel()` stops it, `active()` says whether
/// it will run again, and leaving the scope of a `local h <close> = ...` cancels it.
/// Losing the handle does not.
pub(crate) struct CallbackHandle(pub(crate) Rc<Slot>);

impl UserData for CallbackHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("cancel", |_, this, ()| {
            this.0.release();
            Ok(())
        });
        methods.add_method("active", |_, this, ()| Ok(this.0.is_active()));
        methods.add_meta_method(MetaMethod::Close, |_, this, _: mlua::MultiValue| {
            this.0.release();
            Ok(())
        });
    }
}
