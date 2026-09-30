//! A `PortSource` whose device list a test controls.

use std::fmt;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use parking_lot::Mutex;
use serialist_core::{PortEvent, PortId, PortInfo, PortKind, PortSource};

/// The id of the virtual port named `name`: `virtual:<name>`.
pub fn virtual_port_id(name: &str) -> PortId {
    PortId::new(format!("virtual:{name}"))
}

/// Port info for a virtual device: id `virtual:<name>`, kind `Virtual`.
pub fn virtual_port(name: &str, display_name: impl Into<String>) -> PortInfo {
    PortInfo {
        id: virtual_port_id(name),
        kind: PortKind::Virtual,
        display_name: display_name.into(),
    }
}

#[derive(Default)]
struct Inner {
    /// In plug order, which is the order `snapshot` reports.
    ports: Vec<PortInfo>,
    subscribers: Vec<Sender<PortEvent>>,
}

impl Inner {
    /// Send to every live subscriber and forget the ones whose receiver is gone.
    fn broadcast(&mut self, event: &PortEvent) {
        self.subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

/// A controllable device list. Events reach subscribers synchronously, well inside the
/// 250 ms hotplug budget. Cheap to clone; clones share the same list.
#[derive(Clone, Default)]
pub struct SimPortSource {
    inner: Arc<Mutex<Inner>>,
}

impl SimPortSource {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make a port appear. Plugging an id that is already present with different info
    /// reports `Removed` then `Added`; plugging identical info again does nothing.
    pub fn plug(&self, info: PortInfo) {
        let mut inner = self.inner.lock();
        if let Some(pos) = inner.ports.iter().position(|p| p.id == info.id) {
            if inner.ports[pos] == info {
                return;
            }
            let old = inner.ports.remove(pos);
            inner.broadcast(&PortEvent::Removed(old.id));
        }
        inner.ports.push(info.clone());
        inner.broadcast(&PortEvent::Added(info));
    }

    /// Make a port disappear. Returns whether it was present.
    pub fn unplug(&self, id: &PortId) -> bool {
        let mut inner = self.inner.lock();
        let Some(pos) = inner.ports.iter().position(|p| &p.id == id) else {
            return false;
        };
        inner.ports.remove(pos);
        inner.broadcast(&PortEvent::Removed(id.clone()));
        true
    }

    pub fn contains(&self, id: &PortId) -> bool {
        self.inner.lock().ports.iter().any(|p| &p.id == id)
    }

    /// Subscribers still registered. A dropped receiver is pruned at the next event.
    pub fn subscriber_count(&self) -> usize {
        self.inner.lock().subscribers.len()
    }
}

impl PortSource for SimPortSource {
    fn snapshot(&self) -> Vec<PortInfo> {
        self.inner.lock().ports.clone()
    }

    fn subscribe(&self) -> Receiver<PortEvent> {
        let (tx, rx) = unbounded();
        let mut inner = self.inner.lock();
        // Under the lock, so no plug or unplug can slip between the snapshot and the
        // registration.
        let _ = tx.send(PortEvent::Snapshot(inner.ports.clone()));
        inner.subscribers.push(tx);
        rx
    }
}

impl fmt::Debug for SimPortSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.inner.lock();
        f.debug_struct("SimPortSource")
            .field("ports", &inner.ports)
            .field("subscribers", &inner.subscribers.len())
            .finish()
    }
}
