//! The fake port source, the transport factory, and the world that ties them together.

mod common;

use std::sync::Arc;
use std::time::Duration;

use serialist_core::{
    PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig, TransportError,
    TransportFactory, UsbInfo,
};
use serialist_sim::{
    EchoDevice, LinkConfig, ManualClock, SimPortSource, SimTransportFactory, SimWorld,
    virtual_port, virtual_port_id,
};

use common::{MS, advance_to, drain, read_exactly, read_to_disconnect};

fn recv(rx: &crossbeam_channel::Receiver<PortEvent>) -> PortEvent {
    rx.recv_timeout(Duration::from_secs(1))
        .expect("no port event")
}

fn usb_port(path: &str, serial_number: &str) -> PortInfo {
    PortInfo {
        id: PortId::new(path),
        kind: PortKind::Usb(UsbInfo {
            vid: 0x0403,
            pid: 0x6001,
            serial_number: Some(serial_number.into()),
            manufacturer: Some("FTDI".into()),
            product: Some("FT232R USB UART".into()),
        }),
        display_name: "FT232R USB UART".into(),
    }
}

#[test]
fn subscription_starts_with_a_snapshot_then_follows_changes() {
    let source = SimPortSource::new();
    let echo = virtual_port("echo", "Echo");
    source.plug(echo.clone());
    assert_eq!(echo.id.as_str(), "virtual:echo");
    assert_eq!(echo.kind, PortKind::Virtual);

    let rx = source.subscribe();
    assert_eq!(recv(&rx), PortEvent::Snapshot(vec![echo.clone()]));

    let usb = usb_port("/dev/cu.usbserial-A1", "A1");
    source.plug(usb.clone());
    assert_eq!(recv(&rx), PortEvent::Added(usb.clone()));
    assert!(source.unplug(&echo.id));
    assert_eq!(recv(&rx), PortEvent::Removed(echo.id.clone()));
    assert!(!source.unplug(&echo.id), "already gone");
    assert!(rx.try_recv().is_err(), "no event for a no-op unplug");
    assert_eq!(source.snapshot(), vec![usb]);
}

#[test]
fn late_subscribers_see_the_current_list() {
    let source = SimPortSource::new();
    source.plug(virtual_port("a", "A"));
    source.plug(virtual_port("b", "B"));
    source.unplug(&virtual_port_id("a"));
    let rx = source.subscribe();
    assert_eq!(recv(&rx), PortEvent::Snapshot(vec![virtual_port("b", "B")]));
    assert!(rx.try_recv().is_err());
}

#[test]
fn replugging_changed_info_is_removed_then_added() {
    let source = SimPortSource::new();
    source.plug(virtual_port("x", "Old name"));
    let rx = source.subscribe();
    let _ = recv(&rx);
    source.plug(virtual_port("x", "Old name"));
    assert!(rx.try_recv().is_err(), "identical plug is a no-op");
    source.plug(virtual_port("x", "New name"));
    assert_eq!(recv(&rx), PortEvent::Removed(virtual_port_id("x")));
    assert_eq!(recv(&rx), PortEvent::Added(virtual_port("x", "New name")));
    assert_eq!(source.snapshot().len(), 1);
}

#[test]
fn many_subscribers_and_dropped_ones_are_pruned() {
    let source = SimPortSource::new();
    let keep: Vec<_> = (0..3).map(|_| source.subscribe()).collect();
    let gone = source.subscribe();
    assert_eq!(source.subscriber_count(), 4);
    drop(gone);
    source.plug(virtual_port("p", "P"));
    assert_eq!(source.subscriber_count(), 3);
    for rx in &keep {
        assert_eq!(recv(rx), PortEvent::Snapshot(vec![]));
        assert_eq!(recv(rx), PortEvent::Added(virtual_port("p", "P")));
    }
    drop(keep);
    source.unplug(&virtual_port_id("p"));
    assert_eq!(source.subscriber_count(), 0);
}

#[test]
fn factory_opens_registered_ports_only() {
    let factory = SimTransportFactory::new();
    let id = factory.register(
        "echo",
        LinkConfig::unpaced(),
        || Box::new(EchoDevice::new()),
    );
    assert_eq!(id, virtual_port_id("echo"));
    assert_eq!(factory.ports(), vec![id.clone()]);

    let cfg = SerialConfig::default();
    let unknown = virtual_port_id("nope");
    assert!(matches!(
        factory.open(&unknown, &cfg),
        Err(TransportError::NotFound(p)) if p == unknown
    ));

    let mut t = factory.open(&id, &cfg).unwrap();
    assert_eq!(t.description, "virtual:echo @ 115200 8N1");
    t.writer.write_all(b"ping").unwrap();
    assert_eq!(
        read_exactly(&mut *t.reader, 4, 4096, Duration::from_secs(2)),
        b"ping"
    );

    let link = factory.link(&id).expect("handle for the open link");
    assert_eq!(link.device_name(), "echo");
    // The host's serial settings replace the registered ones.
    assert_eq!(link.link_config().serial, cfg);

    assert!(factory.set_present(&id, false));
    assert!(link.is_unplugged());
    assert!(read_to_disconnect(&mut *t.reader, Duration::from_secs(2)).is_empty());
    assert!(matches!(
        factory.open(&id, &cfg),
        Err(TransportError::NotFound(_))
    ));
    assert!(factory.link(&id).is_none());

    assert!(factory.set_present(&id, true));
    assert!(factory.open(&id, &cfg).is_ok());
    assert!(factory.unregister(&id));
    assert!(matches!(
        factory.open(&id, &cfg),
        Err(TransportError::NotFound(_))
    ));
    assert!(matches!(
        factory.open(&unknown, &SerialConfig { baud: 0, ..cfg }),
        Err(TransportError::Config(_))
    ));
}

#[test]
fn each_open_gets_a_fresh_device() {
    let factory = SimTransportFactory::new();
    let id = factory.register("lines", LinkConfig::unpaced(), || {
        Box::new(EchoDevice::lines())
    });
    let cfg = SerialConfig::default();
    let mut first = factory.open(&id, &cfg).unwrap();
    first.writer.write_all(b"half a line").unwrap();
    drop(first);
    // A new device has an empty line buffer.
    let mut second = factory.open(&id, &cfg).unwrap();
    second.writer.write_all(b"ok\n").unwrap();
    assert_eq!(
        read_exactly(&mut *second.reader, 3, 4096, Duration::from_secs(2)),
        b"ok\n"
    );
}

#[test]
fn world_lists_and_opens_the_builtins() {
    let world = SimWorld::new();
    let ports = world.port_source().snapshot();
    let ids: Vec<_> = ports.iter().map(|p| p.id.as_str().to_owned()).collect();
    assert_eq!(
        ids,
        [
            "virtual:echo",
            "virtual:echo-lines",
            "virtual:at",
            "virtual:firehose",
            "virtual:firehose-ansi"
        ]
    );
    assert!(ports.iter().all(|p| p.kind == PortKind::Virtual));
    let factory = world.transport_factory();
    for port in &ports {
        let t = factory.open(&port.id, &SerialConfig::default());
        assert!(t.is_ok(), "{} failed to open", port.id);
    }
}

#[test]
fn world_unplug_and_replug_update_both_sides() {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::ECHO);
    let rx = world.source().subscribe();
    let _ = recv(&rx);
    let cfg = SerialConfig::default();
    let mut t = world.factory().open(&id, &cfg).unwrap();

    assert!(world.unplug(&id));
    assert_eq!(recv(&rx), PortEvent::Removed(id.clone()));
    assert!(!world.source().contains(&id));
    assert!(read_to_disconnect(&mut *t.reader, Duration::from_secs(2)).is_empty());
    assert!(matches!(
        world.factory().open(&id, &cfg),
        Err(TransportError::NotFound(_))
    ));

    assert!(world.plug(&id));
    assert!(matches!(recv(&rx), PortEvent::Added(info) if info.id == id));
    let mut again = world.factory().open(&id, &cfg).unwrap();
    again.writer.write_all(b"back").unwrap();
    assert_eq!(
        read_exactly(&mut *again.reader, 4, 4096, Duration::from_secs(2)),
        b"back"
    );

    assert!(!world.unplug(&virtual_port_id("never-added")));
    assert!(!world.plug(&virtual_port_id("never-added")));
}

#[test]
fn world_can_impersonate_a_usb_adapter() {
    let world = SimWorld::empty();
    let info = usb_port("/dev/cu.usbserial-A9", "A9");
    let id = world.add_device(info.clone(), LinkConfig::unpaced(), || {
        Box::new(EchoDevice::new())
    });
    assert_eq!(world.source().snapshot(), vec![info]);
    let mut t = world.factory().open(&id, &SerialConfig::default()).unwrap();
    assert_eq!(t.description, "/dev/cu.usbserial-A9 @ 115200 8N1");
    t.writer.write_all(b"usb").unwrap();
    assert_eq!(
        read_exactly(&mut *t.reader, 3, 4096, Duration::from_secs(2)),
        b"usb"
    );
}

#[test]
fn a_world_on_a_manual_clock_runs_its_links_on_it() {
    let clock = Arc::new(ManualClock::new());
    let world = SimWorld::with_clock(clock.clone());
    let factory = SimTransportFactory::with_clock(clock.clone());
    let id = factory.register(
        "echo",
        LinkConfig::default(),
        || Box::new(EchoDevice::new()),
    );
    for factory in [world.factory(), &factory] {
        // 115 200 baud 8N1 with 1 ms of latency.
        let mut t = factory.open(&id, &SerialConfig::default()).unwrap();
        clock.settle(1);
        let t0 = clock.now();
        t.writer.write_all(b"ping").unwrap();
        clock.settle(1);
        // Nothing moves until the clock does.
        assert_eq!(drain(&mut *t.reader), (Vec::new(), false));
        // "ping" takes 347 us on the wire and is released to the device at 1.35 ms; in
        // 1 ms steps the device sees it at 2 ms and echoes it at once, and the echo is
        // released to the host at 3.35 ms.
        advance_to(&clock, t0 + 3 * MS, MS, 1);
        assert_eq!(drain(&mut *t.reader).0, b"");
        advance_to(&clock, t0 + 4 * MS, MS, 1);
        assert_eq!(drain(&mut *t.reader).0, b"ping");
    }
}
