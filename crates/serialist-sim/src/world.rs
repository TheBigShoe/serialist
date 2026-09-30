//! A port source and a transport factory wired together, so plugging a device makes it
//! both visible and openable, and unplugging it does both the other way.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use parking_lot::Mutex;
use serialist_core::{PortId, PortInfo, PortSource, TransportFactory};

use crate::source::virtual_port;
use crate::{
    AtDevice, EchoDevice, FirehoseConfig, FirehoseContent, FirehoseDevice, LinkConfig, LinkHandle,
    SimDevice, SimPortSource, SimTransportFactory,
};

/// A simulated set of devices for tests and the app's developer mode. Cheap to clone;
/// clones share the same devices.
///
/// [`SimWorld::new`] plugs in the built-ins, each on a default (paced) [`LinkConfig`]:
///
/// | Id                       | Device                                        |
/// |--------------------------|-----------------------------------------------|
/// | `virtual:echo`           | [`EchoDevice::new`]                           |
/// | `virtual:echo-lines`     | [`EchoDevice::lines`]                         |
/// | `virtual:at`             | [`AtDevice::new`]                             |
/// | `virtual:firehose`       | [`FirehoseDevice`], text, as fast as the baud |
/// | `virtual:firehose-ansi`  | [`FirehoseDevice`], ANSI, as fast as the baud |
#[derive(Clone, Default)]
pub struct SimWorld {
    source: SimPortSource,
    factory: SimTransportFactory,
    /// Everything ever added, plugged or not, so `plug` can bring a device back.
    infos: Arc<Mutex<BTreeMap<PortId, PortInfo>>>,
}

impl SimWorld {
    pub const ECHO: &'static str = "echo";
    pub const ECHO_LINES: &'static str = "echo-lines";
    pub const AT: &'static str = "at";
    pub const FIREHOSE: &'static str = "firehose";
    pub const FIREHOSE_ANSI: &'static str = "firehose-ansi";

    /// A world with the built-in devices plugged in.
    pub fn new() -> Self {
        let world = Self::empty();
        let link = LinkConfig::default();
        world.add_virtual(Self::ECHO, "Echo (virtual)", link.clone(), || {
            Box::new(EchoDevice::new())
        });
        world.add_virtual(
            Self::ECHO_LINES,
            "Line echo (virtual)",
            link.clone(),
            || Box::new(EchoDevice::lines()),
        );
        world.add_virtual(Self::AT, "AT modem (virtual)", link.clone(), || {
            Box::new(AtDevice::new())
        });
        world.add_virtual(Self::FIREHOSE, "Firehose (virtual)", link.clone(), || {
            Box::new(FirehoseDevice::new(FirehoseConfig::new(
                FirehoseContent::Text,
            )))
        });
        world.add_virtual(Self::FIREHOSE_ANSI, "ANSI firehose (virtual)", link, || {
            Box::new(
                FirehoseDevice::new(FirehoseConfig::new(FirehoseContent::Ansi))
                    .with_name(Self::FIREHOSE_ANSI),
            )
        });
        world
    }

    /// A world with no devices. `SimWorld::default()` is the same; only `new` adds the built-ins.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Register a device under `virtual:<name>` and plug it in.
    pub fn add_virtual<F>(
        &self,
        name: &str,
        display_name: &str,
        link: LinkConfig,
        constructor: F,
    ) -> PortId
    where
        F: Fn() -> Box<dyn SimDevice> + Send + Sync + 'static,
    {
        self.add_device(virtual_port(name, display_name), link, constructor)
    }

    /// Register a device under any port info (a fake USB adapter, say) and plug it in.
    pub fn add_device<F>(&self, info: PortInfo, link: LinkConfig, constructor: F) -> PortId
    where
        F: Fn() -> Box<dyn SimDevice> + Send + Sync + 'static,
    {
        let id = info.id.clone();
        self.factory
            .register_port(id.clone(), link, Arc::new(constructor));
        self.infos.lock().insert(id.clone(), info.clone());
        self.source.plug(info);
        id
    }

    /// Plug a previously added device back in. Returns whether it is known.
    pub fn plug(&self, id: &PortId) -> bool {
        let Some(info) = self.infos.lock().get(id).cloned() else {
            return false;
        };
        self.factory.set_present(id, true);
        self.source.plug(info);
        true
    }

    /// Pull a device: it leaves the port list, its open links report `Disconnected`,
    /// and opening it fails with `NotFound` until it is plugged again.
    pub fn unplug(&self, id: &PortId) -> bool {
        let known = self.factory.set_present(id, false);
        self.source.unplug(id);
        known
    }

    pub fn source(&self) -> &SimPortSource {
        &self.source
    }

    pub fn factory(&self) -> &SimTransportFactory {
        &self.factory
    }

    /// The source as the trait object the app's device list takes.
    pub fn port_source(&self) -> Arc<dyn PortSource> {
        Arc::new(self.source.clone())
    }

    /// The factory as the trait object a session takes.
    pub fn transport_factory(&self) -> Arc<dyn TransportFactory> {
        Arc::new(self.factory.clone())
    }

    /// The most recently opened link to `id` that is still up.
    pub fn link(&self, id: &PortId) -> Option<LinkHandle> {
        self.factory.link(id)
    }
}

impl fmt::Debug for SimWorld {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimWorld")
            .field("source", &self.source)
            .field("factory", &self.factory)
            .finish()
    }
}
