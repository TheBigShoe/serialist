//! `port:on_frame([filter,] fn)`: a callback for each frame the session's codec decodes.
//!
//! The same shape as `on_line`: a task on the run's executor keeps a position in the
//! session's [`FrameStore`](serialist_core::FrameStore) (a frame id), sleeps on the line
//! bell, and on each ring takes a snapshot and calls the function with each new frame, at
//! most [`ON_FRAME_BATCH`] from one snapshot, one call at a time. Nothing is queued: the
//! frames stay in the store, which is bounded (100 000 frames within 64 MiB by default),
//! so a handler that falls further behind than that skips the frames evicted meanwhile,
//! as `on_line` skips evicted lines.

use std::rc::Rc;
use std::sync::Arc;

use mlua::{Function, IntoLua, Lua, Value as LuaValue};
use serde_json::{Map, Value as JsonValue};
use serialist_core::codec::frame_matches;
use serialist_core::store::Snapshot;
use serialist_core::{Frame, FrameId, FrameStoreReader, Value};

use crate::callback::Slot;
use crate::race::race;
use crate::services::ScriptSession;
use crate::vm::RunCtx;

/// Most frames an `on_frame` task takes from one snapshot before looking again.
const ON_FRAME_BATCH: usize = 256;

/// A frame as the callback gets it: `kind`, `fields`, `severity`, `summary` and `bytes`.
pub(crate) struct LuaFrame {
    frame: Frame,
    /// The frame's bytes from the scrollback, if they are still there.
    bytes: Option<Vec<u8>>,
}

fn value(lua: &Lua, value: &Value) -> mlua::Result<LuaValue> {
    Ok(match value {
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Int(n) => LuaValue::Integer(*n),
        Value::UInt(n) => match i64::try_from(*n) {
            Ok(n) => LuaValue::Integer(n),
            Err(_) => LuaValue::Number(*n as f64),
        },
        Value::Float(x) => LuaValue::Number(*x),
        Value::Str(s) => LuaValue::String(lua.create_string(s)?),
        Value::Bytes(b) => LuaValue::String(lua.create_string(b)?),
        Value::List(items) => {
            let list = lua.create_table_with_capacity(items.len(), 0)?;
            for item in items {
                list.raw_push(self::value(lua, item)?)?;
            }
            LuaValue::Table(list)
        }
    })
}

impl IntoLua for LuaFrame {
    fn into_lua(self, lua: &Lua) -> mlua::Result<LuaValue> {
        let frame = self.frame;
        let table = lua.create_table()?;
        table.raw_set("kind", frame.kind.as_str())?;
        table.raw_set("severity", frame.severity.name())?;
        table.raw_set("summary", frame.summary)?;
        let fields = lua.create_table_with_capacity(0, frame.fields.len())?;
        for (name, field) in &frame.fields {
            // The first field of a name wins, as `Frame::field` reads it.
            if fields.raw_get::<LuaValue>(name.as_str())?.is_nil() {
                fields.raw_set(name.as_str(), value(lua, field)?)?;
            }
        }
        table.raw_set("fields", fields)?;
        if let Some(bytes) = self.bytes {
            table.raw_set("bytes", lua.create_string(&bytes)?)?;
        }
        Ok(LuaValue::Table(table))
    }
}

/// The bytes of `frame` from `snap`, unless some have been evicted.
fn bytes_of(snap: &Snapshot, frame: &Frame) -> Option<Vec<u8>> {
    let retained = snap.raw_range();
    if frame.raw.start < retained.start || frame.raw.end > retained.end {
        return None;
    }
    let mut bytes = Vec::with_capacity(frame.raw_len() as usize);
    for piece in snap.raw(frame.raw.clone()) {
        bytes.extend_from_slice(piece);
    }
    Some(bytes)
}

/// Calls `callback` with each frame from `next` on that `filter` matches, one at a time
/// and in order, until cancelled, the session closes, or a callback fails (which ends
/// the run).
pub(crate) async fn on_frame_task(
    ctx: Rc<RunCtx>,
    session: Arc<dyn ScriptSession>,
    frames: FrameStoreReader,
    callback: Function,
    filter: Option<Map<String, JsonValue>>,
    slot: Rc<Slot>,
    mut next: FrameId,
) {
    let mut listener = session.bell().listen();
    let mut batch = Vec::with_capacity(ON_FRAME_BATCH);
    loop {
        if !slot.is_active() {
            return;
        }
        let closed = listener.is_closed();
        let snap = frames.snapshot();
        let store = session.store().snapshot();
        for (id, frame) in snap.iter(next..snap.end()) {
            if batch.len() == ON_FRAME_BATCH {
                break;
            }
            next = id.next();
            if filter
                .as_ref()
                .is_none_or(|filter| frame_matches(filter, frame))
            {
                batch.push(LuaFrame {
                    bytes: bytes_of(&store, frame),
                    frame: frame.clone(),
                });
            }
        }
        let full = batch.len() == ON_FRAME_BATCH;
        if !full {
            next = next.max(snap.end());
        }
        drop(snap);
        drop(store);
        for frame in batch.drain(..) {
            if !slot.is_active() {
                return;
            }
            if let Err(err) = callback.call_async::<()>(frame).await {
                slot.release();
                ctx.fail(err);
                return;
            }
        }
        if full {
            continue;
        }
        if closed {
            slot.release();
            return;
        }
        race(listener.rung(), slot.wake.notified()).await;
    }
}
