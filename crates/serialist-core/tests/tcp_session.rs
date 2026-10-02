//! The raw TCP transport end to end: a real `Session` through the router, against a
//! `TcpListener` on 127.0.0.1 inside the test. Loopback only, so no hardware and no
//! network are involved.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use serialist_core::{
    ControlLine, PortId, RoutingTransportFactory, SerialConfig, SerialportFactory, Session,
    SessionConfig, SessionEvent, TCP_SCHEME, TcpTransportFactory, TransportError,
};

const WAIT: Duration = Duration::from_secs(5);

/// The app's routing for `tcp:` ids. The serial default is never reached here.
fn router() -> RoutingTransportFactory {
    RoutingTransportFactory::new(Arc::new(SerialportFactory::new()))
        .with_scheme(TCP_SCHEME, Arc::new(TcpTransportFactory::new()))
}

/// A listener on a free loopback port and the `tcp:` id that reaches it.
fn listen() -> (TcpListener, PortId) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, PortId::new(format!("tcp:127.0.0.1:{port}")))
}

fn open(router: &RoutingTransportFactory, id: &PortId) -> Session {
    let mut config = SessionConfig::new(id.clone(), SerialConfig::default());
    config.read_timeout = Duration::from_millis(10);
    Session::open(router, config).expect("the listener accepts")
}

fn accept(listener: &TcpListener) -> TcpStream {
    let (peer, _) = listener.accept().unwrap();
    peer.set_read_timeout(Some(WAIT)).unwrap();
    peer
}

fn next(events: &Receiver<SessionEvent>) -> SessionEvent {
    events.recv_timeout(WAIT).expect("an event in time")
}

/// Received bytes until `len` have arrived, across however many chunks.
fn receive(events: &Receiver<SessionEvent>, len: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < len {
        match next(events) {
            SessionEvent::Data { bytes, .. } => got.extend_from_slice(&bytes),
            other => panic!("expected data, got {other:?}"),
        }
    }
    got
}

#[test]
fn connect_read_write_then_the_peer_hangs_up() {
    let (listener, id) = listen();
    let router = router();
    let session = open(&router, &id);
    let events = session.events();
    let mut peer = accept(&listener);

    match next(&events) {
        SessionEvent::Connected { description } => assert_eq!(description, id.as_str()),
        other => panic!("expected Connected first, got {other:?}"),
    }

    // Read: what the peer sends arrives unchanged.
    let banner = b"U-Boot 2026.07\r\n=> ";
    peer.write_all(banner).unwrap();
    assert_eq!(receive(&events, banner.len()), banner);

    // Write: what the session sends reaches the peer.
    session.write(b"printenv\r\n".to_vec()).unwrap();
    let mut sent = [0u8; 10];
    peer.read_exact(&mut sent).unwrap();
    assert_eq!(&sent, b"printenv\r\n");

    // Control lines and line settings are accepted no-ops; break is unsupported.
    let slow = SerialConfig {
        baud: 9600,
        ..SerialConfig::default()
    };
    session.set_control(ControlLine::Dtr, true).unwrap();
    session.set_control(ControlLine::Rts, false).unwrap();
    session.reconfigure(slow.clone()).unwrap();
    session.send_break(Duration::from_millis(5)).unwrap();
    match next(&events) {
        SessionEvent::WriteFailed(TransportError::Unsupported(what)) => assert_eq!(what, "break"),
        other => panic!("only the break fails, got {other:?}"),
    }
    assert_eq!(session.serial_config(), slow, "the settings are kept");
    assert!(session.is_connected());

    // The peer hangs up: the session ends with Disconnected, and nothing follows.
    drop(peer);
    match next(&events) {
        SessionEvent::Disconnected {
            error: Some(TransportError::Disconnected),
        } => {}
        other => panic!("expected Disconnected, got {other:?}"),
    }
    assert!(!session.is_connected());
    assert!(session.write(b"late".to_vec()).is_err());
    assert_eq!(session.stats().rx_bytes, banner.len() as u64);
    assert_eq!(session.stats().tx_bytes, 10);
    session.close();
    assert!(events.recv_timeout(Duration::from_millis(100)).is_err());
}

#[test]
fn closing_the_session_hangs_up_on_the_peer() {
    let (listener, id) = listen();
    let router = router();
    let session = open(&router, &id);
    let events = session.events();
    let mut peer = accept(&listener);

    // "Send reboot, then disconnect" must not lose the reboot.
    session.write(b"reset\r\n".to_vec()).unwrap();
    session.close();

    let mut got = Vec::new();
    peer.read_to_end(&mut got)
        .expect("the peer sees an orderly end of stream");
    assert_eq!(got, b"reset\r\n");

    let all: Vec<_> = events.try_iter().collect();
    assert!(
        matches!(all.first(), Some(SessionEvent::Connected { .. })),
        "{all:?}"
    );
    assert!(
        matches!(all.last(), Some(SessionEvent::Disconnected { error: None })),
        "an orderly close: {all:?}"
    );
}

#[test]
fn a_megabyte_arrives_whole_and_in_order() {
    let (listener, id) = listen();
    let router = router();
    let session = open(&router, &id);
    let events = session.events();
    let mut peer = accept(&listener);
    let _connected = next(&events);

    let sent: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
    let writer = {
        let sent = sent.clone();
        std::thread::spawn(move || peer.write_all(&sent).map(|()| peer))
    };
    assert_eq!(receive(&events, sent.len()), sent);
    let _peer = writer.join().unwrap().unwrap();
    assert!(session.stats().rx_chunks >= 1);
}

#[test]
fn reconnect_is_a_new_connection_to_the_same_endpoint() {
    let (listener, id) = listen();
    let router = router();

    let first = open(&router, &id);
    let first_events = first.events();
    let peer = accept(&listener);
    let _connected = next(&first_events);
    drop(peer);
    assert!(matches!(
        next(&first_events),
        SessionEvent::Disconnected { error: Some(_) }
    ));

    // What the UI's reconnect does: open the same id again.
    let second = open(&router, &id);
    let events = second.events();
    let mut peer = accept(&listener);
    assert!(matches!(next(&events), SessionEvent::Connected { .. }));
    peer.write_all(b"again").unwrap();
    assert_eq!(receive(&events, 5), b"again");
}

#[test]
fn nothing_listening_is_an_io_error_not_not_found() {
    let (listener, id) = listen();
    drop(listener);
    let router = router();
    let mut config = SessionConfig::new(id.clone(), SerialConfig::default());
    config.read_timeout = Duration::from_millis(10);
    match Session::open(&router, config) {
        Err(TransportError::Io(err)) => {
            assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err}");
        }
        other => panic!("expected a refused connection, got {other:?}"),
    }
}
