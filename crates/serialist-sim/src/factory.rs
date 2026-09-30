//! A `TransportFactory` that opens virtual links to registered simulated devices.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;
use serialist_core::{PortId, SerialConfig, Transport, TransportError, TransportFactory};

use crate::source::virtual_port_id;
use crate::{LinkConfig, LinkHandle, SimDevice, VirtualLink};

/// Builds a fresh device for each open, so every connection starts from power-on state.
pub type DeviceConstructor = Arc<dyn Fn() -> Box<dyn SimDevice> + Send + Sync>;

struct Entry {
    constructor: DeviceConstructor,
    link: LinkConfig,
    present: bool,
    /// Links opened for this port that are still up, oldest first.
    links: Vec<LinkHandle>,
}

impl Entry {
    fn unplug_links(&mut self) {
        for link in self.links.drain(..) {
            link.unplug();
        }
    }
}

/// Opens `virtual:<name>` ports. Cheap to clone; clones share one registry.
///
/// `open` uses the registered [`LinkConfig`] with its `serial` replaced by the config the
/// host asked for, so a session opened at 3 Mbaud is paced at 3 Mbaud.
#[derive(Clone, Default)]
pub struct SimTransportFactory {
    inner: Arc<Mutex<BTreeMap<PortId, Entry>>>,
}

impl SimTransportFactory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a device under `virtual:<name>`, present and ready to open.
    pub fn register<F>(&self, name: &str, link: LinkConfig, constructor: F) -> PortId
    where
        F: Fn() -> Box<dyn SimDevice> + Send + Sync + 'static,
    {
        let id = virtual_port_id(name);
        self.register_port(id.clone(), link, Arc::new(constructor));
        id
    }

    /// Register a device under any id, for tests that impersonate a real port such as
    /// `/dev/cu.usbserial-1420`. Replaces an existing registration; its open links stay up.
    pub fn register_port(&self, id: PortId, link: LinkConfig, constructor: DeviceConstructor) {
        let mut entries = self.inner.lock();
        let links = entries.remove(&id).map(|e| e.links).unwrap_or_default();
        entries.insert(
            id,
            Entry {
                constructor,
                link,
                present: true,
                links,
            },
        );
    }

    /// Forget a port and unplug its open links. Returns whether it was registered.
    pub fn unregister(&self, id: &PortId) -> bool {
        match self.inner.lock().remove(id) {
            Some(mut entry) => {
                entry.unplug_links();
                true
            }
            None => false,
        }
    }

    /// Mark a registered port present or absent. Going absent unplugs its open links and
    /// makes `open` fail with `NotFound` until it is present again. Returns whether the
    /// port is registered.
    pub fn set_present(&self, id: &PortId, present: bool) -> bool {
        let mut entries = self.inner.lock();
        let Some(entry) = entries.get_mut(id) else {
            return false;
        };
        entry.present = present;
        if !present {
            entry.unplug_links();
        }
        true
    }

    pub fn is_present(&self, id: &PortId) -> bool {
        self.inner.lock().get(id).is_some_and(|e| e.present)
    }

    /// Registered port ids, sorted.
    pub fn ports(&self) -> Vec<PortId> {
        self.inner.lock().keys().cloned().collect()
    }

    /// The handle of the most recently opened link for `id` that is still up.
    pub fn link(&self, id: &PortId) -> Option<LinkHandle> {
        let mut entries = self.inner.lock();
        let entry = entries.get_mut(id)?;
        entry.links.retain(|l| !l.is_unplugged());
        entry.links.last().cloned()
    }

    pub fn link_config(&self, id: &PortId) -> Option<LinkConfig> {
        self.inner.lock().get(id).map(|e| e.link.clone())
    }

    /// Change the link behaviour for future opens of `id`. Returns whether it is registered.
    pub fn set_link_config(&self, id: &PortId, link: LinkConfig) -> bool {
        match self.inner.lock().get_mut(id) {
            Some(entry) => {
                entry.link = link;
                true
            }
            None => false,
        }
    }
}

impl TransportFactory for SimTransportFactory {
    fn open(&self, port: &PortId, config: &SerialConfig) -> Result<Transport, TransportError> {
        if config.baud == 0 {
            return Err(TransportError::Config(
                "baud must be greater than zero".into(),
            ));
        }
        let (constructor, mut link_cfg) = {
            let entries = self.inner.lock();
            match entries.get(port) {
                Some(entry) if entry.present => {
                    (Arc::clone(&entry.constructor), entry.link.clone())
                }
                _ => return Err(TransportError::NotFound(port.clone())),
            }
        };
        link_cfg.serial = config.clone();
        // Device code runs outside the registry lock.
        let device = constructor();
        let (mut transport, handle) = VirtualLink::connect(device, link_cfg);
        transport.description = format!("{port} @ {}", config.summary());

        let mut entries = self.inner.lock();
        match entries.get_mut(port) {
            Some(entry) if entry.present => {
                entry.links.retain(|l| !l.is_unplugged());
                entry.links.push(handle);
                Ok(transport)
            }
            // Unplugged while the device was being built.
            _ => {
                handle.unplug();
                Err(TransportError::NotFound(port.clone()))
            }
        }
    }
}

impl fmt::Debug for SimTransportFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let entries = self.inner.lock();
        let mut map = f.debug_map();
        for (id, entry) in entries.iter() {
            map.entry(&id.as_str(), &(entry.present, entry.links.len()));
        }
        map.finish()
    }
}
