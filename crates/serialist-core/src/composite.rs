//! Several backends presented as one: [`MergedPortSource`] shows the ports of many
//! sources as a single list, and [`RoutingTransportFactory`] sends each `open` to the
//! factory that owns the port id. Together they let the app list real and simulated
//! ports side by side without either backend knowing about the other.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Select, Sender, unbounded};

use crate::config::SerialConfig;
use crate::discovery::diff_ports;
use crate::port::{PortEvent, PortId, PortInfo, PortSource};
use crate::transport::{Transport, TransportError, TransportFactory};

/// The scheme of simulated ports: `virtual:<device>`.
pub const VIRTUAL_SCHEME: &str = "virtual";

/// How long `subscribe` waits for a child's opening `Snapshot`. Every source in this
/// workspace sends it before `subscribe` returns; the bound only keeps a misbehaving
/// source from hanging the caller.
const FIRST_SNAPSHOT_WAIT: Duration = Duration::from_secs(1);

/// The ports of several sources as one list, in source order.
///
/// A subscription gets one merged `Snapshot`, then every child's `Added` and `Removed`.
/// A child that sends a later `Snapshot` has it translated into the `Added`/`Removed`
/// events that turn its old list into the new one, because forwarding it as is would
/// replace every other source's ports too.
///
/// Each subscription runs a small forwarding thread. It ends when every child has closed
/// its channel, or at the first child event after the subscriber dropped its receiver.
pub struct MergedPortSource {
    sources: Vec<Arc<dyn PortSource>>,
}

impl MergedPortSource {
    pub fn new(sources: Vec<Arc<dyn PortSource>>) -> Self {
        Self { sources }
    }
}

impl PortSource for MergedPortSource {
    fn snapshot(&self) -> Vec<PortInfo> {
        self.sources.iter().flat_map(|s| s.snapshot()).collect()
    }

    fn subscribe(&self) -> Receiver<PortEvent> {
        let mut children = Vec::with_capacity(self.sources.len());
        let mut merged = Vec::new();
        for source in &self.sources {
            let rx = source.subscribe();
            let first = match rx.recv_timeout(FIRST_SNAPSHOT_WAIT) {
                Ok(PortEvent::Snapshot(ports)) => ports,
                // Tolerate a source that skips its snapshot: an early `Added` still
                // belongs in the opening list.
                Ok(PortEvent::Added(info)) => vec![info],
                Ok(PortEvent::Removed(_)) | Err(_) => Vec::new(),
            };
            merged.extend(first.iter().cloned());
            children.push(Child { rx, current: first });
        }

        let (tx, rx) = unbounded();
        // The receiver is alive and the channel unbounded, so this cannot fail.
        let _ = tx.send(PortEvent::Snapshot(merged));
        let spawned = thread::Builder::new()
            .name("serialist-port-merge".to_owned())
            .spawn(move || forward(children, &tx));
        if let Err(err) = spawned {
            tracing::warn!(error = %err, "could not start the port merge thread; the list will not update");
        }
        rx
    }
}

struct Child {
    rx: Receiver<PortEvent>,
    /// The child's list as last reported, to diff a late `Snapshot` against.
    current: Vec<PortInfo>,
}

fn forward(mut children: Vec<Child>, tx: &Sender<PortEvent>) {
    while !children.is_empty() {
        let mut select = Select::new();
        for child in &children {
            select.recv(&child.rx);
        }
        let op = select.select();
        let ix = op.index();
        let Ok(event) = op.recv(&children[ix].rx) else {
            // That source is gone; the others carry on.
            children.swap_remove(ix);
            continue;
        };
        let child = &mut children[ix];
        let events = match event {
            PortEvent::Snapshot(ports) => {
                let events = diff_ports(&child.current, &ports);
                child.current = ports;
                events
            }
            PortEvent::Added(info) => {
                child.current.retain(|p| p.id != info.id);
                child.current.push(info.clone());
                vec![PortEvent::Added(info)]
            }
            PortEvent::Removed(id) => {
                child.current.retain(|p| p.id != id);
                vec![PortEvent::Removed(id)]
            }
        };
        for event in events {
            if tx.send(event).is_err() {
                return;
            }
        }
    }
}

/// Sends each `open` to a factory chosen by the port id's scheme.
///
/// An id of the form `<scheme>:<rest>` (for example `virtual:echo`) goes to the factory
/// registered for that scheme, and an unregistered scheme fails with `NotFound`. Ids
/// without a scheme (`/dev/cu.usbserial-1420`, `COM3`) go to the default factory,
/// normally the serial one.
pub struct RoutingTransportFactory {
    default: Arc<dyn TransportFactory>,
    schemes: Vec<(String, Arc<dyn TransportFactory>)>,
}

impl RoutingTransportFactory {
    pub fn new(default: Arc<dyn TransportFactory>) -> Self {
        Self {
            default,
            schemes: Vec::new(),
        }
    }

    /// Route `<scheme>:` ids to `factory`. A later registration of the same scheme wins.
    pub fn with_scheme(mut self, scheme: &str, factory: Arc<dyn TransportFactory>) -> Self {
        self.schemes.retain(|(s, _)| s != scheme);
        self.schemes.push((scheme.to_owned(), factory));
        self
    }

    /// The factory that would open `port`, or `None` for an unregistered scheme.
    pub fn route(&self, port: &PortId) -> Option<&Arc<dyn TransportFactory>> {
        match scheme_of(port) {
            None => Some(&self.default),
            Some(scheme) => self
                .schemes
                .iter()
                .find(|(s, _)| s == scheme)
                .map(|(_, factory)| factory),
        }
    }
}

impl TransportFactory for RoutingTransportFactory {
    fn open(&self, port: &PortId, config: &SerialConfig) -> Result<Transport, TransportError> {
        match self.route(port) {
            Some(factory) => factory.open(port, config),
            None => Err(TransportError::NotFound(port.clone())),
        }
    }
}

/// The `<scheme>` of a `<scheme>:<rest>` id. A scheme is at least two characters, starts
/// with a letter and holds only letters, digits, `+`, `-` and `.`, so device paths that
/// contain colons (`/dev/serial/by-path/pci-0000:00:14.0-usb-0:2:1.0-port0`) and drive
/// letters are never mistaken for one.
pub fn scheme_of(port: &PortId) -> Option<&str> {
    let (scheme, _) = port.as_str().split_once(':')?;
    let mut chars = scheme.chars();
    let starts_with_letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    let valid = starts_with_letter
        && scheme.len() >= 2
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;
    use crate::port::PortKind;
    use crate::transport::{ControlLine, TransportReader, TransportWriter};

    fn port(id: &str) -> PortInfo {
        PortInfo {
            id: PortId::new(id),
            kind: PortKind::Unknown,
            display_name: id.to_owned(),
        }
    }

    /// A source a test can push events through by hand.
    #[derive(Default)]
    struct HandSource {
        ports: Vec<PortInfo>,
        senders: Mutex<Vec<Sender<PortEvent>>>,
    }

    impl HandSource {
        fn with(ports: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                ports: ports.iter().map(|id| port(id)).collect(),
                senders: Mutex::default(),
            })
        }

        fn send(&self, event: PortEvent) {
            for tx in self.senders.lock().iter() {
                tx.send(event.clone()).unwrap();
            }
        }

        fn close(&self) {
            self.senders.lock().clear();
        }
    }

    impl PortSource for HandSource {
        fn snapshot(&self) -> Vec<PortInfo> {
            self.ports.clone()
        }

        fn subscribe(&self) -> Receiver<PortEvent> {
            let (tx, rx) = unbounded();
            tx.send(PortEvent::Snapshot(self.ports.clone())).unwrap();
            self.senders.lock().push(tx);
            rx
        }
    }

    fn recv(rx: &Receiver<PortEvent>) -> PortEvent {
        rx.recv_timeout(Duration::from_secs(1)).expect("an event")
    }

    #[test]
    fn snapshot_and_subscription_merge_in_source_order() {
        let real = HandSource::with(&["/dev/cu.a", "/dev/cu.b"]);
        let sim = HandSource::with(&["virtual:echo"]);
        let merged = MergedPortSource::new(vec![real.clone(), sim.clone()]);

        let ids = |ports: Vec<PortInfo>| ports.into_iter().map(|p| p.id.0).collect::<Vec<_>>();
        assert_eq!(
            ids(merged.snapshot()),
            ["/dev/cu.a", "/dev/cu.b", "virtual:echo"]
        );

        let rx = merged.subscribe();
        match recv(&rx) {
            PortEvent::Snapshot(ports) => {
                assert_eq!(ids(ports), ["/dev/cu.a", "/dev/cu.b", "virtual:echo"]);
            }
            other => panic!("expected the merged snapshot first, got {other:?}"),
        }

        sim.send(PortEvent::Added(port("virtual:at")));
        assert_eq!(recv(&rx), PortEvent::Added(port("virtual:at")));
        real.send(PortEvent::Removed(PortId::new("/dev/cu.a")));
        assert_eq!(recv(&rx), PortEvent::Removed(PortId::new("/dev/cu.a")));
    }

    #[test]
    fn a_late_child_snapshot_becomes_a_diff() {
        let real = HandSource::with(&["/dev/cu.a", "/dev/cu.b"]);
        let sim = HandSource::with(&["virtual:echo"]);
        let merged = MergedPortSource::new(vec![real.clone(), sim.clone()]);
        let rx = merged.subscribe();
        let _ = recv(&rx);

        real.send(PortEvent::Snapshot(vec![
            port("/dev/cu.b"),
            port("/dev/cu.c"),
        ]));
        assert_eq!(recv(&rx), PortEvent::Removed(PortId::new("/dev/cu.a")));
        assert_eq!(recv(&rx), PortEvent::Added(port("/dev/cu.c")));
        assert!(
            rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "the simulator's port is untouched"
        );
    }

    #[test]
    fn the_merged_channel_closes_after_every_child_does() {
        let a = HandSource::with(&["/dev/cu.a"]);
        let b = HandSource::with(&[]);
        let merged = MergedPortSource::new(vec![a.clone(), b.clone()]);
        let rx = merged.subscribe();
        let _ = recv(&rx);

        a.close();
        b.send(PortEvent::Added(port("/dev/cu.z")));
        assert_eq!(recv(&rx), PortEvent::Added(port("/dev/cu.z")));
        b.close();
        assert!(
            matches!(
                rx.recv_timeout(Duration::from_secs(1)),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            ),
            "no children left, so the forwarder exits and drops its sender"
        );
    }

    struct Nothing;

    impl TransportReader for Nothing {
        fn read(&mut self, _: &mut [u8], _: Duration) -> Result<usize, TransportError> {
            Ok(0)
        }
    }

    impl TransportWriter for Nothing {
        fn write_all(&mut self, _: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }

        fn set_control(&mut self, _: ControlLine, _: bool) -> Result<(), TransportError> {
            Ok(())
        }

        fn reconfigure(&mut self, _: &SerialConfig) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// Records the ids it was asked to open and labels the transport with its name.
    struct Recorder {
        name: &'static str,
        opened: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn new(name: &'static str) -> Arc<Self> {
            Arc::new(Self {
                name,
                opened: Mutex::default(),
            })
        }
    }

    impl TransportFactory for Recorder {
        fn open(&self, port: &PortId, _: &SerialConfig) -> Result<Transport, TransportError> {
            self.opened.lock().push(port.0.clone());
            Ok(Transport {
                reader: Box::new(Nothing),
                writer: Box::new(Nothing),
                description: self.name.to_owned(),
            })
        }
    }

    #[test]
    fn routes_by_scheme_and_rejects_unknown_ones() {
        let serial = Recorder::new("serial");
        let sim = Recorder::new("sim");
        let router =
            RoutingTransportFactory::new(serial.clone()).with_scheme(VIRTUAL_SCHEME, sim.clone());
        let open = |id: &str| {
            router
                .open(&PortId::new(id), &SerialConfig::default())
                .map(|t| t.description)
        };

        assert_eq!(open("virtual:echo").unwrap(), "sim");
        assert_eq!(open("/dev/cu.usbserial-1420").unwrap(), "serial");
        assert_eq!(open("COM3").unwrap(), "serial");
        assert_eq!(
            open("/dev/serial/by-path/pci-0000:00:14.0-usb-0:2:1.0-port0").unwrap(),
            "serial"
        );
        assert!(matches!(
            open("tcp:192.168.1.5:4000"),
            Err(TransportError::NotFound(id)) if id.as_str() == "tcp:192.168.1.5:4000"
        ));
        assert_eq!(*sim.opened.lock(), ["virtual:echo"]);
        assert_eq!(serial.opened.lock().len(), 3);
    }

    #[test]
    fn schemes() {
        let scheme = |id: &str| scheme_of(&PortId::new(id)).map(str::to_owned);
        assert_eq!(scheme("virtual:echo").as_deref(), Some("virtual"));
        assert_eq!(scheme("rfc2217+tcp:host:1").as_deref(), Some("rfc2217+tcp"));
        assert_eq!(scheme("/dev/ttyUSB0"), None);
        assert_eq!(scheme("COM10"), None);
        assert_eq!(
            scheme(r"C:\ports\x"),
            None,
            "a drive letter is not a scheme"
        );
        assert_eq!(scheme(":x"), None);
        assert_eq!(scheme("9p:x"), None);
    }
}
