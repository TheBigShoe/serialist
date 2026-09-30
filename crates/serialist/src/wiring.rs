//! Which port source and transport factory the app runs with.
//!
//! Milestone 0 stand-ins: neither the real serialport-backed source and factory nor
//! `serialist_sim`'s `SimPortSource` and `SimTransportFactory` exist yet. Once they do,
//! `app_options` swaps these stubs for them and nothing in the UI changes.

use std::sync::Arc;

use serialist_core::{
    PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig, Transport, TransportError,
    TransportFactory,
};
use serialist_ui::AppOptions;

use crate::cli::Args;

/// A fixed list of ports: whatever the flags named.
pub struct StaticPortSource {
    ports: Vec<PortInfo>,
}

impl StaticPortSource {
    pub fn new(ports: Vec<PortInfo>) -> Self {
        Self { ports }
    }
}

impl PortSource for StaticPortSource {
    fn snapshot(&self) -> Vec<PortInfo> {
        self.ports.clone()
    }

    fn subscribe(&self) -> crossbeam_channel::Receiver<PortEvent> {
        // The list never changes, so the sender is dropped after the snapshot and the
        // panel's drain loop ends instead of polling forever.
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(PortEvent::Snapshot(self.ports.clone())).ok();
        rx
    }
}

/// Refuses every open with a clear reason until a real backend is wired in.
pub struct UnavailableTransportFactory;

impl TransportFactory for UnavailableTransportFactory {
    fn open(&self, port: &PortId, _config: &SerialConfig) -> Result<Transport, TransportError> {
        tracing::warn!(%port, "no transport backend is wired into this build yet");
        Err(TransportError::Unsupported(
            "no transport backend in this build yet",
        ))
    }
}

pub fn virtual_port(name: &str) -> PortInfo {
    PortInfo {
        id: PortId::new(format!("virtual:{name}")),
        kind: PortKind::Virtual,
        display_name: format!("{name} (simulated)"),
    }
}

pub fn app_options(args: &Args) -> AppOptions {
    let mut ports = Vec::new();
    let mut select_port = None;

    for name in &args.virtual_devices {
        // TODO(milestone 0 integration): register `name` with serialist_sim's device
        // registry and use SimPortSource/SimTransportFactory instead of these stubs.
        tracing::info!(
            device = %name,
            "--virtual: serialist-sim is not wired in yet; listing a placeholder port"
        );
        let port = virtual_port(name);
        select_port.get_or_insert_with(|| port.id.clone());
        ports.push(port);
    }

    let mut connect_on_start = false;
    if let Some(path) = &args.port {
        let id = PortId::new(path.clone());
        if !ports.iter().any(|p| p.id == id) {
            ports.push(PortInfo {
                id: id.clone(),
                kind: PortKind::Unknown,
                display_name: path.clone(),
            });
        }
        select_port = Some(id);
        connect_on_start = true;
    }

    let serial = SerialConfig {
        baud: args.baud.unwrap_or(SerialConfig::default().baud),
        ..SerialConfig::default()
    };

    AppOptions {
        port_source: Arc::new(StaticPortSource::new(ports)),
        transport_factory: Arc::new(UnavailableTransportFactory),
        serial,
        select_port,
        connect_on_start,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_become_ports_and_a_selection() {
        let args = Args {
            port: Some("/dev/cu.usbserial-1".into()),
            baud: Some(921_600),
            virtual_devices: vec!["echo".into()],
        };
        let options = app_options(&args);
        let ids: Vec<String> = options
            .port_source
            .snapshot()
            .into_iter()
            .map(|p| p.id.0)
            .collect();
        assert_eq!(ids, ["virtual:echo", "/dev/cu.usbserial-1"]);
        assert_eq!(
            options.select_port,
            Some(PortId::new("/dev/cu.usbserial-1"))
        );
        assert!(options.connect_on_start);
        assert_eq!(options.serial.baud, 921_600);
    }

    #[test]
    fn virtual_only_selects_without_connecting() {
        let args = Args {
            virtual_devices: vec!["echo".into()],
            ..Args::default()
        };
        let options = app_options(&args);
        assert_eq!(options.select_port, Some(PortId::new("virtual:echo")));
        assert!(!options.connect_on_start);
        assert_eq!(options.serial, SerialConfig::default());
    }

    #[test]
    fn static_source_sends_one_snapshot_then_closes() {
        let source = StaticPortSource::new(vec![virtual_port("echo")]);
        let rx = source.subscribe();
        assert_eq!(
            rx.recv().unwrap(),
            PortEvent::Snapshot(vec![virtual_port("echo")])
        );
        assert!(rx.recv().is_err());
    }
}
