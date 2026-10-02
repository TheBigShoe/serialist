//! The opt-in hardware tier: checks only a real adapter can answer, such as whether the
//! OS driver really runs it at 12 Mbaud. Every test here is `#[ignore]`d, needs a serial
//! device named by an environment variable, and never runs in CI or gates a merge.
//! Without the variable an ignored run prints "skipped" and passes.
//!
//! `loopback_at_12_mbaud` needs an adapter that reaches 12 Mbaud (an FTDI FT232H or
//! FT2232H) with its TX pin wired to its RX pin, and nothing else on the line:
//!
//! ```text
//! SERIALIST_HW_PORT=/dev/cu.usbserial-FT1234 \
//!     cargo test --release -p serialist-core --test hardware -- --ignored --nocapture
//! ```
//!
//! `SERIALIST_HW_PORT` is the port's path (`/dev/cu.usbserial-…` on macOS,
//! `/dev/ttyUSB0` on Linux, `COM7` on Windows). `SERIALIST_HW_BAUD` (default 12000000)
//! and `SERIALIST_HW_BYTES` (default 4 MiB) change the rate and the payload, and
//! `SERIALIST_HW_FLOW=hardware` turns on RTS/CTS, for which RTS must be wired to CTS too.

use std::time::{Duration, Instant};

use serialist_core::{
    FlowControl, PortId, SerialConfig, SerialportFactory, Session, SessionConfig, SessionEvent,
};
use serialist_sim::{FirehoseContent, FirehoseGenerator, FirehoseVerifier};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().replace('_', "").parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "hardware: needs SERIALIST_HW_PORT naming a 12 Mbaud adapter with TX wired to RX"]
fn loopback_at_12_mbaud() {
    let Some(port) = std::env::var("SERIALIST_HW_PORT")
        .ok()
        .filter(|p| !p.is_empty())
    else {
        eprintln!("skipped: set SERIALIST_HW_PORT to an adapter with TX wired to RX");
        return;
    };
    let baud = u32::try_from(env_u64("SERIALIST_HW_BAUD", 12_000_000)).expect("baud fits u32");
    let total = usize::try_from(env_u64("SERIALIST_HW_BYTES", 4 << 20)).expect("size fits");
    let flow_control = match std::env::var("SERIALIST_HW_FLOW").as_deref() {
        Ok("hardware") => FlowControl::Hardware,
        _ => FlowControl::None,
    };
    let serial = SerialConfig {
        baud,
        flow_control,
        ..SerialConfig::default()
    };
    let nominal = serial.bytes_per_second();
    eprintln!(
        "loopback: {total} bytes on {port} at {} ({nominal:.0} B/s), flow {flow_control:?}",
        serial.summary()
    );

    let session = Session::open(
        &SerialportFactory::new(),
        SessionConfig::new(PortId::new(port.clone()), serial),
    )
    .unwrap_or_else(|err| panic!("open {port}: {err}"));
    let events = session.events();
    match events.recv_timeout(Duration::from_secs(5)) {
        Ok(SessionEvent::Connected { description }) => eprintln!("connected: {description}"),
        other => panic!("expected Connected, got {other:?}"),
    }

    // Binary frames carry every byte value and a sequence number, so a lost, damaged or
    // reordered byte shows as a gap or a bad checksum.
    let mut payload = Vec::with_capacity(total);
    FirehoseGenerator::new(FirehoseContent::Binary, 12).fill(&mut payload, total);
    let started = Instant::now();
    for piece in payload.chunks(64 * 1024) {
        session.write(piece.to_vec()).expect("the session is open");
    }

    // Generous: four times the wire time plus a few seconds of driver latency.
    let wire_time = Duration::from_secs_f64(total as f64 / nominal);
    let deadline = started + wire_time * 4 + Duration::from_secs(5);
    let mut verifier = FirehoseVerifier::new(FirehoseContent::Binary);
    let mut chunks = 0u64;
    let mut first_byte = None;
    while verifier.report().bytes < total as u64 {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(SessionEvent::Data { bytes, .. }) => {
                first_byte.get_or_insert_with(Instant::now);
                verifier.feed(&bytes);
                chunks += 1;
            }
            Ok(SessionEvent::WriteFailed(err)) => panic!("write failed: {err}"),
            Ok(SessionEvent::Disconnected { error }) => panic!("disconnected: {error:?}"),
            Ok(SessionEvent::Connected { .. }) => {}
            Err(_) => break,
        }
    }
    let elapsed = started.elapsed();
    let report = verifier.report().clone();
    let stats = session.stats();
    session.close();

    let rate = report.bytes as f64 / elapsed.as_secs_f64();
    eprintln!(
        "received {} of {total} bytes in {:.3} s ({rate:.0} B/s, {:.1}% of nominal), \
         first byte after {:?}, {chunks} chunks (mean {:.0} bytes), sent {}",
        report.bytes,
        elapsed.as_secs_f64(),
        100.0 * rate / nominal,
        first_byte.map(|t| t - started),
        report.bytes as f64 / chunks.max(1) as f64,
        stats.tx_bytes,
    );
    eprintln!(
        "records {}, missing {}, corrupt {}, out of order {}",
        report.records, report.missing_records, report.corrupt_records, report.out_of_order
    );
    assert_eq!(stats.tx_bytes, total as u64, "every byte was written");
    assert_eq!(
        report.bytes, total as u64,
        "every byte came back in time (is TX wired to RX?)"
    );
    assert!(report.is_clean(), "{report:?}");
}
