//! Which ports the app lists and how it opens them.
//!
//! Real ports always: `RealPortSource` for the list and `SerialportFactory` to open
//! them. `tcp:<host>:<port>` ids always open through `TcpTransportFactory` and
//! `replay:<path>` ids through `ReplayTransportFactory`; neither kind is listed, since
//! nothing discovers them (see `serialist_core::address` for the grammar). With
//! `--virtual` (or a `virtual:` `--port`), the simulator's devices are listed
//! alongside the real ports and `virtual:` ids open through the simulator. Every port
//! named with `--port ID` or `--virtual NAME` opens at startup in a tab of its own, in
//! the order given (so `--virtual firehose` streams right away). A
//! `RoutingTransportFactory` picks the backend from the id, so the UI, the headless
//! `--script` runner and its scripts' `serial.open` never need to know which one a port
//! belongs to.

use std::sync::Arc;

use anyhow::bail;
use serialist_core::composite::scheme_of;
use serialist_core::{
    MergedPortSource, PortId, PortSource, REPLAY_SCHEME, RealPortSource, ReplayTransportFactory,
    RoutingTransportFactory, SerialportFactory, TCP_SCHEME, TcpTransportFactory, TransportFactory,
    VIRTUAL_SCHEME,
};
use serialist_sim::SimWorld;
use serialist_ui::AppOptions;

use crate::cli::Args;

/// One place ports come from and the factory that opens them.
pub struct Backend {
    pub port_source: Arc<dyn PortSource>,
    pub transport_factory: Arc<dyn TransportFactory>,
}

impl Backend {
    /// The host's serial ports.
    pub fn real() -> Self {
        Self {
            port_source: Arc::new(RealPortSource::new()),
            transport_factory: Arc::new(SerialportFactory::new()),
        }
    }
}

/// The app's options for these flags, over the real serial layer and the built-in
/// simulated devices.
pub fn app_options(args: &Args) -> anyhow::Result<AppOptions> {
    build(args, Backend::real(), SimWorld::new())
}

/// [`app_options`] with the backends supplied, so tests can stand in for real hardware.
pub fn build(args: &Args, real: Backend, world: SimWorld) -> anyhow::Result<AppOptions> {
    let known = virtual_names(&world);
    let open: Vec<PortId> = args.open.iter().cloned().map(PortId::new).collect();
    let is_virtual = |port: &&PortId| scheme_of(port) == Some(VIRTUAL_SCHEME);
    for port in open.iter().filter(is_virtual) {
        let name = port.as_str().trim_start_matches("virtual:");
        if !known.iter().any(|k| k == name) {
            bail!(
                "unknown virtual device {name:?}; known devices: {}",
                known.join(", ")
            );
        }
    }

    let simulate = args.simulator || open.iter().any(|port| is_virtual(&port));
    let router = RoutingTransportFactory::new(real.transport_factory)
        .with_scheme(TCP_SCHEME, Arc::new(TcpTransportFactory::new()))
        .with_scheme(REPLAY_SCHEME, Arc::new(ReplayTransportFactory::new()));
    let (port_source, transport_factory) = if simulate {
        let source = MergedPortSource::new(vec![real.port_source, world.port_source()]);
        let router = router.with_scheme(VIRTUAL_SCHEME, world.transport_factory());
        (Arc::new(source) as Arc<dyn PortSource>, router)
    } else {
        // Without the simulator a `virtual:` id has no route and fails with NotFound.
        (real.port_source, router)
    };

    Ok(AppOptions {
        port_source,
        transport_factory: Arc::new(transport_factory),
        // Without --baud the rate comes from the settings: a device profile, else
        // `default_baud`.
        baud: args.baud,
        select_port: open.first().cloned(),
        // A tab each, in the order given; with none, the last session's tabs reopen.
        open_ports: open,
        // Sized by the `scrollback_budget_bytes` setting.
        store: None,
    })
}

/// Names `--virtual` accepts: the world's `virtual:<name>` ports, sorted.
fn virtual_names(world: &SimWorld) -> Vec<String> {
    world
        .factory()
        .ports()
        .into_iter()
        .filter_map(|id| id.as_str().strip_prefix("virtual:").map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use serialist_core::{PortInfo, PortKind, SerialConfig, TransportError, UsbInfo};
    use serialist_sim::{EchoDevice, LinkConfig};

    use super::*;

    const FAKE_ADAPTER: &str = "/dev/cu.usbserial-FAKE";

    /// A stand-in for the host's serial layer: a simulated device under a real-looking
    /// path, so tests never touch hardware.
    fn fake_real() -> Backend {
        let world = SimWorld::empty();
        world.add_device(
            PortInfo {
                id: PortId::new(FAKE_ADAPTER),
                kind: PortKind::Usb(UsbInfo {
                    vid: 0x0403,
                    pid: 0x6001,
                    serial_number: Some("FAKE".into()),
                    manufacturer: Some("FTDI".into()),
                    product: Some("FT232R USB UART".into()),
                }),
                display_name: "FT232R USB UART (FAKE)".into(),
            },
            LinkConfig::unpaced(),
            || Box::new(EchoDevice::new()),
        );
        Backend {
            port_source: world.port_source(),
            transport_factory: world.transport_factory(),
        }
    }

    /// The options for these flags, parsed as `main` parses them.
    fn options(flags: &[&str]) -> anyhow::Result<AppOptions> {
        let args = match crate::cli::parse(flags.iter().map(|flag| flag.to_string()))? {
            crate::cli::Command::Run(args) => args,
            other => panic!("{other:?}"),
        };
        build(&args, fake_real(), SimWorld::new())
    }

    fn listed(options: &AppOptions) -> Vec<String> {
        options
            .port_source
            .snapshot()
            .into_iter()
            .map(|p| p.id.0)
            .collect()
    }

    fn opens(options: &AppOptions, id: &str) -> Result<String, TransportError> {
        options
            .transport_factory
            .open(&PortId::new(id), &SerialConfig::default())
            .map(|transport| transport.description)
    }

    #[test]
    fn named_virtual_device_is_listed_with_real_ports_and_opened() {
        let options = options(&["--virtual", "echo"]).unwrap();
        assert_eq!(
            listed(&options),
            [
                FAKE_ADAPTER,
                "virtual:echo",
                "virtual:echo-lines",
                "virtual:at",
                "virtual:firehose",
                "virtual:firehose-ansi",
                "virtual:race",
                "virtual:menu",
            ]
        );
        assert_eq!(options.select_port, Some(PortId::new("virtual:echo")));
        assert_eq!(
            options.open_ports,
            [PortId::new("virtual:echo")],
            "a named device opens at startup"
        );

        assert_eq!(
            opens(&options, "virtual:echo").unwrap(),
            "virtual:echo @ 115200 8N1"
        );
        assert_eq!(
            opens(&options, FAKE_ADAPTER).unwrap(),
            format!("{FAKE_ADAPTER} @ 115200 8N1"),
            "paths go to the serial backend"
        );
        assert!(matches!(
            opens(&options, "rfc2217:localhost:4000"),
            Err(TransportError::NotFound(_))
        ));
    }

    #[test]
    fn tcp_and_replay_ids_route_with_or_without_the_simulator() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tcp = format!("tcp:127.0.0.1:{}", listener.local_addr().unwrap().port());
        for flags in [&[][..], &["--virtual"][..]] {
            let options = options(flags).unwrap();
            assert_eq!(opens(&options, &tcp).unwrap(), tcp, "{flags:?}");
            // The replay factory parses the id before anything else, so a bad option
            // proves the route; an unregistered scheme would be NotFound instead.
            assert!(
                matches!(
                    opens(&options, "replay:/captures/boot.bin?speed=warp"),
                    Err(TransportError::Config(message)) if message.contains("speed")
                ),
                "{flags:?}: routed to the replay factory"
            );
            assert!(
                matches!(opens(&options, "tcp:nope"), Err(TransportError::Config(_))),
                "{flags:?}: a malformed id is a config error"
            );
            assert!(
                !listed(&options).iter().any(|id| id.starts_with("tcp:")),
                "nothing lists tcp ports"
            );
        }
    }

    #[test]
    fn a_tcp_port_opens_at_startup_like_any_other() {
        let options = options(&["--port", "tcp:10.0.0.5:4000"]).unwrap();
        assert_eq!(options.open_ports, [PortId::new("tcp:10.0.0.5:4000")]);
        assert_eq!(options.select_port, Some(PortId::new("tcp:10.0.0.5:4000")));
        assert_eq!(listed(&options), [FAKE_ADAPTER], "the simulator stays off");
    }

    #[test]
    fn several_ports_open_a_tab_each_in_the_order_given() {
        let options = options(&[
            "--virtual",
            "at",
            "--port",
            FAKE_ADAPTER,
            "--virtual",
            "race",
        ])
        .unwrap();
        assert_eq!(
            options.open_ports,
            [
                PortId::new("virtual:at"),
                PortId::new(FAKE_ADAPTER),
                PortId::new("virtual:race"),
            ]
        );
        assert_eq!(options.select_port, Some(PortId::new("virtual:at")));
    }

    #[test]
    fn bare_virtual_lists_the_built_ins_without_selecting() {
        let options = options(&["--virtual"]).unwrap();
        assert_eq!(listed(&options).len(), 8);
        assert_eq!(options.select_port, None);
        assert!(
            options.open_ports.is_empty(),
            "the last session's tabs reopen"
        );
    }

    #[test]
    fn without_virtual_only_real_ports_exist() {
        let options = options(&[]).unwrap();
        assert_eq!(listed(&options), [FAKE_ADAPTER]);
        assert!(matches!(
            opens(&options, "virtual:echo"),
            Err(TransportError::NotFound(_))
        ));
    }

    #[test]
    fn unknown_virtual_devices_are_rejected_with_the_list() {
        let error = options(&["--virtual", "at", "--virtual", "toaster"])
            .err()
            .expect("an error");
        assert_eq!(
            error.to_string(),
            "unknown virtual device \"toaster\"; known devices: at, echo, echo-lines, firehose, firehose-ansi, menu, race"
        );
        assert!(options(&["--port", "virtual:toaster"]).is_err());
    }

    #[test]
    fn port_preselects_and_connects_at_the_given_baud() {
        let options = options(&["--port", FAKE_ADAPTER, "--baud", "921600"]).unwrap();
        assert_eq!(options.select_port, Some(PortId::new(FAKE_ADAPTER)));
        assert_eq!(options.open_ports, [PortId::new(FAKE_ADAPTER)]);
        assert_eq!(options.baud, Some(921_600), "--baud wins over profiles");
    }

    #[test]
    fn a_virtual_port_turns_the_simulator_on() {
        let options = options(&["--port", "virtual:at"]).unwrap();
        assert!(listed(&options).contains(&"virtual:at".to_owned()));
        assert_eq!(options.open_ports, [PortId::new("virtual:at")]);
        assert!(opens(&options, "virtual:at").is_ok());
    }
}
