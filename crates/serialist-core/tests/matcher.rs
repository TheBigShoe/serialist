//! Line matchers and saved commands over a real `Session` and a simulated AT modem: no
//! hardware, no fixed sleeps beyond the timeouts under test.

use std::time::Duration;

use serialist_core::commands::CommandCollection;
use serialist_core::ingest::IDLE_INTERVAL;
use serialist_core::{
    Direction, ExpectResult, Expectation, Ingest, IngestHandle, LineEnding, LineId, LineSource,
    ParamValues, SerialConfig, Session, SessionConfig, Store,
};
use serialist_sim::{SimWorld, virtual_port_id};

fn serial(baud: u32) -> SerialConfig {
    SerialConfig {
        baud,
        ..SerialConfig::default()
    }
}

/// A session on the simulated modem, with its ingest thread.
struct Rig {
    world: SimWorld,
    session: Session,
    ingest: IngestHandle,
}

fn rig() -> Rig {
    let world = SimWorld::new();
    let id = virtual_port_id(SimWorld::AT);
    let session =
        Session::open(world.factory(), SessionConfig::new(id, serial(115_200))).expect("open");
    let ingest = Ingest::spawn(
        session.events(),
        Store::default(),
        Vec::new(),
        Box::new(|| {}),
    );
    Rig {
        world,
        session,
        ingest,
    }
}

impl Rig {
    fn send(&self, text: &str) {
        self.session
            .write(text.as_bytes().to_vec())
            .expect("the session is open");
    }

    fn expect(&self, pattern: &str, timeout: Duration) -> Expectation {
        self.ingest
            .matchers()
            .expect(pattern, timeout)
            .expect("a valid pattern")
    }
}

/// The result, failing the test instead of hanging if it never comes.
fn resolve(expectation: &Expectation) -> ExpectResult {
    expectation
        .wait_timeout(Duration::from_secs(10))
        .unwrap_or_else(|| panic!("{expectation:?} never resolved"))
}

fn matched(result: ExpectResult) -> (LineId, String, Vec<Option<String>>, Duration) {
    match result {
        ExpectResult::Matched {
            line,
            text,
            captures,
            elapsed,
            ..
        } => (line, text, captures, elapsed),
        other => panic!("expected a match, got {other:?}"),
    }
}

#[test]
fn at_gets_ok() {
    let rig = rig();
    // Register first, then send: the reply cannot beat the listener.
    let expectation = rig.expect("^OK", Duration::from_secs(2));
    rig.send("AT\r\n");
    let (line, text, captures, elapsed) = matched(resolve(&expectation));
    assert_eq!(text, "OK");
    assert_eq!(captures, [Some("OK".to_owned())]);
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
    // The matched line is the one in the scrollback.
    let stored = rig.ingest.snapshot();
    let mut lines = Vec::new();
    stored.lines(line..line.next(), &mut lines);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        (lines[0].direction, lines[0].text.as_str()),
        (Direction::Rx, "OK")
    );
    assert_eq!(rig.ingest.matchers().pending(), 0);
}

#[test]
fn a_match_carries_its_ranges_within_the_stored_line() {
    let rig = rig();
    let expectation = rig.expect(r"(O)(K)$", Duration::from_secs(2));
    rig.send("AT\r\n");
    let ExpectResult::Matched {
        line,
        text,
        range,
        capture_ranges,
        ..
    } = resolve(&expectation)
    else {
        panic!("expected a match");
    };
    assert_eq!(range, 0..2);
    assert_eq!(capture_ranges, [Some(0..2), Some(0..1), Some(1..2)]);
    // The ranges are into the text of the line as the scrollback holds it.
    let stored = rig.ingest.snapshot();
    let mut lines = Vec::new();
    stored.lines(line..line.next(), &mut lines);
    assert_eq!(lines[0].text, text);
    assert_eq!(&lines[0].text[range], "OK");
}

#[test]
fn a_pattern_that_never_comes_times_out() {
    let rig = rig();
    let timeout = Duration::from_millis(300);
    let expectation = rig.expect("^NEVER", timeout);
    rig.send("AT\r\n");
    match resolve(&expectation) {
        ExpectResult::TimedOut { elapsed } => {
            assert!(elapsed >= timeout, "never early: {elapsed:?}");
            // Noticed on the ingest thread's next tick at the latest.
            assert!(
                elapsed <= timeout + IDLE_INTERVAL + Duration::from_millis(500),
                "{elapsed:?}"
            );
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(rig.ingest.matchers().pending(), 0);
}

#[test]
fn a_timeout_resolves_while_other_data_keeps_arriving() {
    let rig = rig();
    let expectation = rig.expect("^NEVER", Duration::from_millis(200));
    // Chunks every 20 ms keep the thread from ever going idle, so its idle tick never
    // fires; each chunk is a check by itself.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let result = loop {
        rig.send("AT\r\n");
        if let Some(result) = expectation.wait_timeout(Duration::from_millis(20)) {
            break result;
        }
        assert!(std::time::Instant::now() < deadline, "no timeout");
    };
    assert!(
        matches!(result, ExpectResult::TimedOut { .. }),
        "{result:?}"
    );
}

#[test]
fn two_expectations_are_answered_by_one_exchange() {
    let rig = rig();
    let version = rig.expect(r"^\+VER: (\S+)", Duration::from_secs(2));
    let ok = rig.expect("^OK$", Duration::from_secs(2));
    let also_ok = rig.expect("ok", Duration::from_secs(2));
    rig.send("AT+VER?\r\n");

    let (version_line, text, captures, _) = matched(resolve(&version));
    assert_eq!(text, "+VER: 1.0.0");
    assert_eq!(captures[1].as_deref(), Some("1.0.0"));
    let (ok_line, ..) = matched(resolve(&ok));
    let (also_line, ..) = matched(resolve(&also_ok));
    // `OK` follows the version line, and one line answered two expectations.
    assert!(ok_line > version_line);
    assert_eq!(ok_line, also_line);
}

#[test]
fn an_exchange_can_be_repeated_with_fresh_expectations() {
    let rig = rig();
    let mut lines = Vec::new();
    for _ in 0..3 {
        let expectation = rig.expect("^OK", Duration::from_secs(2));
        rig.send("AT\r\n");
        let (line, ..) = matched(resolve(&expectation));
        lines.push(line);
    }
    // Each round matched its own reply.
    assert!(lines.windows(2).all(|pair| pair[0] < pair[1]), "{lines:?}");
}

#[test]
fn cancel_gives_up_without_disturbing_the_others() {
    let rig = rig();
    let cancelled = rig.expect("^OK", Duration::from_secs(30));
    let kept = rig.expect("^OK", Duration::from_secs(30));
    let waiter = {
        let cancelled = cancelled.clone();
        std::thread::spawn(move || cancelled.wait())
    };
    std::thread::sleep(Duration::from_millis(50));
    cancelled.cancel();
    assert_eq!(waiter.join().unwrap(), ExpectResult::Closed);
    assert_eq!(rig.ingest.matchers().pending(), 1);

    rig.send("AT\r\n");
    assert!(matches!(resolve(&kept), ExpectResult::Matched { .. }));
    // The cancelled one stays cancelled.
    assert_eq!(cancelled.try_wait(), Some(ExpectResult::Closed));
}

#[test]
fn only_lines_after_registration_count() {
    let rig = rig();
    let first = rig.expect("^OK", Duration::from_secs(2));
    rig.send("AT\r\n");
    let (first_line, ..) = matched(resolve(&first));

    // Registered after the reply arrived: it must not match the old OK.
    let late = rig.expect("^OK", Duration::from_millis(300));
    assert!(matches!(resolve(&late), ExpectResult::TimedOut { .. }));

    // But it does match the next one.
    let next = rig.expect("^OK", Duration::from_secs(2));
    rig.send("AT\r\n");
    let (next_line, ..) = matched(resolve(&next));
    assert!(next_line > first_line);
}

#[test]
fn a_sent_echo_is_not_a_reply() {
    let rig = rig();
    let expectation = rig.expect("^AT$", Duration::from_millis(300));
    // What the UI stores when it sends: a local Tx line with the text sent.
    rig.ingest
        .append_local("AT", Direction::Tx)
        .expect("ingest running");
    rig.send("AT\r\n");
    // The modem answers OK, never AT (its echo is off), so nothing matches.
    assert!(matches!(
        resolve(&expectation),
        ExpectResult::TimedOut { .. }
    ));
}

#[test]
fn closing_the_session_resolves_closed() {
    let rig = rig();
    let waiting = rig.expect("^NEVER", Duration::from_secs(30));
    let matchers = rig.ingest.matchers();
    rig.session.close();
    assert_eq!(resolve(&waiting), ExpectResult::Closed);
    // Once closed, later registrations are closed at once.
    let late = matchers.expect("^OK", Duration::from_secs(30)).unwrap();
    assert_eq!(late.try_wait(), Some(ExpectResult::Closed));
    assert_eq!(matchers.pending(), 0);
    let _ = rig.ingest.join();
}

#[test]
fn dropping_the_session_resolves_closed_too() {
    let Rig {
        world,
        session,
        ingest,
    } = rig();
    let waiting = ingest
        .matchers()
        .expect("^NEVER", Duration::from_secs(30))
        .unwrap();
    drop(session);
    assert_eq!(resolve(&waiting), ExpectResult::Closed);
    drop(world);
}

#[test]
fn an_unplugged_device_resolves_closed() {
    let rig = rig();
    let waiting = rig.expect("^NEVER", Duration::from_secs(30));
    assert!(rig.world.unplug(&virtual_port_id(SimWorld::AT)));
    assert_eq!(resolve(&waiting), ExpectResult::Closed);
}

#[test]
fn a_reply_that_beats_the_disconnect_still_matches() {
    let rig = rig();
    let reply = rig.expect("^OK", Duration::from_secs(30));
    rig.send("AT\r\n");
    // `close` lets the queued write and the reply through before it disconnects.
    let (line, ..) = matched(resolve(&reply));
    rig.session.close();
    assert!(line > LineId(0));
}

#[test]
fn stopping_ingest_resolves_closed() {
    let rig = rig();
    let waiting = rig.expect("^NEVER", Duration::from_secs(30));
    let Rig {
        ingest, session, ..
    } = rig;
    ingest.stop().expect("the ingest thread ran cleanly");
    assert_eq!(resolve(&waiting), ExpectResult::Closed);
    drop(session);
}

/// Every bundled AT command, encoded from its defaults, sent, and answered as its own
/// `expect` says.
#[test]
fn the_bundled_commands_work_against_the_simulated_modem() {
    let rig = rig();
    let examples = CommandCollection::bundled_examples();
    let mut sent = 0;
    for (group, command) in examples.commands() {
        let Some(expect) = &command.expect else {
            continue;
        };
        let bytes = command
            .encode(&ParamValues::new(), LineEnding::Crlf)
            .unwrap_or_else(|err| panic!("{}: {err}", command.name));
        let expectation = rig
            .ingest
            .matchers()
            .expect(&expect.pattern, expect.timeout())
            .expect("the example patterns are regexes");
        rig.session.write(bytes).expect("session open");
        let (_, text, captures, elapsed) = matched(resolve(&expectation));
        assert!(
            elapsed < expect.timeout(),
            "{} in {group:?}: {elapsed:?}",
            command.name
        );
        if command.name == "AT+VER?" {
            assert_eq!(text, "+VER: 1.0.0");
            assert_eq!(captures[1].as_deref(), Some("1.0.0"));
        }
        sent += 1;
    }
    assert_eq!(sent, 4, "AT, ATI, AT+VER? and Echo");
}
