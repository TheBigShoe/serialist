//! The matcher against a bare `Store`, with the ingest thread's calls made by hand so
//! every boundary is exact. The same flow over a real session is in `tests/matcher.rs`.

use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::store::Store;

/// A store and the matchers, driven the way the ingest thread drives them.
struct Rig {
    store: Store,
    handle: MatcherHandle,
}

impl Rig {
    fn new() -> Self {
        Self {
            store: Store::default(),
            handle: MatcherHandle::default(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.feed_at(bytes, Instant::now());
    }

    fn feed_at(&mut self, bytes: &[u8], at: Instant) {
        let report = self.store.append(bytes, at);
        self.handle.on_append(&self.store, &report, at);
    }

    fn local(&mut self, text: &str, direction: Direction) {
        self.store.append_local(text, direction);
        self.handle.advance(self.store.end(), Instant::now());
    }

    fn expect(&self, pattern: &str) -> Expectation {
        self.handle
            .expect(pattern, Duration::from_secs(60))
            .unwrap()
    }
}

fn matched(result: Option<ExpectResult>) -> (LineId, String, Vec<Option<String>>) {
    match result {
        Some(ExpectResult::Matched {
            line,
            text,
            captures,
            ..
        }) => (line, text, captures),
        other => panic!("expected a match, got {other:?}"),
    }
}

#[test]
fn a_finished_line_matches_and_reports_its_captures() {
    let mut rig = Rig::new();
    let expectation = rig.expect(r"^VAL=(\d+)(x)?");
    rig.feed(b"junk\r\nVAL=42\r\nmore\r\n");
    let (line, text, captures) = matched(expectation.try_wait());
    assert_eq!(line, LineId(1));
    assert_eq!(text, "VAL=42");
    // The whole match first, then each group; a group that sat out is `None`.
    assert_eq!(
        captures,
        [Some("VAL=42".to_owned()), Some("42".to_owned()), None]
    );
    assert_eq!(rig.handle.pending(), 0);
}

#[test]
fn the_pattern_is_searched_anywhere_in_the_line() {
    let mut rig = Rig::new();
    let anywhere = rig.expect("temp");
    let anchored = rig.expect("^temp$");
    rig.feed(b"cpu temp 41\r\n");
    assert_eq!(matched(anywhere.try_wait()).1, "cpu temp 41");
    assert_eq!(anchored.try_wait(), None);
    rig.feed(b"temp\r\n");
    assert_eq!(matched(anchored.try_wait()).0, LineId(1));
}

#[test]
fn a_line_waits_for_its_line_feed() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^OK$");
    rig.feed(b"O");
    rig.feed(b"K");
    assert_eq!(expectation.try_wait(), None, "the line has not ended");
    rig.feed(b"\r");
    assert_eq!(expectation.try_wait(), None, "a CR alone does not end it");
    rig.feed(b"\n");
    assert_eq!(matched(expectation.try_wait()).1, "OK");
}

#[test]
fn only_lines_after_registration_count() {
    let mut rig = Rig::new();
    rig.feed(b"OK\r\nOK\r\n");
    let expectation = rig.expect("^OK");
    assert_eq!(expectation.try_wait(), None, "old lines do not count");
    rig.feed(b"ERROR\r\n");
    assert_eq!(expectation.try_wait(), None);
    rig.feed(b"OK\r\n");
    assert_eq!(matched(expectation.try_wait()).0, LineId(3));
}

#[test]
fn a_line_that_began_before_registration_never_counts() {
    let mut rig = Rig::new();
    rig.feed(b"prompt> ");
    let expectation = rig.expect("OK");
    // The rest of that line arrives after the registration, but it began before.
    rig.feed(b"OK\r\n");
    assert_eq!(expectation.try_wait(), None);
    rig.feed(b"OK\r\n");
    assert_eq!(matched(expectation.try_wait()).0, LineId(1));
}

#[test]
fn one_line_satisfies_several_expectations() {
    let mut rig = Rig::new();
    let exact = rig.expect("^OK$");
    let loose = rig.expect("ok");
    let capture = rig.expect("(O)(K)");
    let other = rig.expect("^ERROR");
    assert_eq!(rig.handle.pending(), 4);
    rig.feed(b"OK\r\n");
    let (a, _, _) = matched(exact.try_wait());
    let (b, _, _) = matched(loose.try_wait());
    let (c, _, captures) = matched(capture.try_wait());
    assert_eq!((a, b, c), (LineId(0), LineId(0), LineId(0)));
    assert_eq!(captures.len(), 3);
    assert_eq!(other.try_wait(), None);
    assert_eq!(rig.handle.pending(), 1);
    rig.feed(b"ERROR 5\r\n");
    assert_eq!(matched(other.try_wait()).0, LineId(1));
}

#[test]
fn each_expectation_takes_the_first_line_that_matches() {
    let mut rig = Rig::new();
    let first = rig.expect("^n");
    rig.feed(b"n1\r\nn2\r\n");
    rig.feed(b"n3\r\n");
    assert_eq!(matched(first.try_wait()).1, "n1");
    // Asking again gives the same answer.
    assert_eq!(matched(first.try_wait()).1, "n1");
    assert_eq!(matched(Some(first.wait())).1, "n1");
}

#[test]
fn matching_is_smart_case() {
    let mut rig = Rig::new();
    let lower = rig.expect("error");
    let upper = rig.expect("Error");
    rig.feed(b"ERROR: bad\r\n");
    assert_eq!(matched(lower.try_wait()).1, "ERROR: bad");
    assert_eq!(upper.try_wait(), None, "a capital makes it case-sensitive");
    rig.feed(b"Error: bad\r\n");
    assert_eq!(matched(upper.try_wait()).0, LineId(1));
    // An inline flag wins either way.
    let forced = rig.expect("(?i)Error");
    rig.feed(b"eRRor\r\n");
    assert_eq!(matched(forced.try_wait()).0, LineId(2));
}

#[test]
fn sent_echoes_and_notices_are_never_matched() {
    let mut rig = Rig::new();
    let expectation = rig.expect("AT");
    rig.local("AT", Direction::Tx);
    rig.local("Connected to AT modem", Direction::Notice);
    assert_eq!(expectation.try_wait(), None);
    rig.feed(b"AT\r\n");
    // The received line has the next id, after the two local ones.
    assert_eq!(matched(expectation.try_wait()).0, LineId(2));
}

#[test]
fn local_lines_move_the_start_of_later_expectations() {
    let mut rig = Rig::new();
    rig.feed(b"pending");
    // A local line ends the line in progress, and registration starts after both.
    rig.local("AT", Direction::Tx);
    let expectation = rig.expect(".");
    rig.feed(b"\r\nreply\r\n");
    // `pending` (id 0) and the echo (id 1) are before it; the received `\r\n` after the
    // broken line is an empty line that `.` does not match.
    assert_eq!(matched(expectation.try_wait()).1, "reply");
}

#[test]
fn a_timeout_resolves_on_the_next_check_after_the_deadline() {
    let rig = Rig::new();
    let expectation = rig
        .handle
        .expect("never", Duration::from_millis(100))
        .unwrap();
    // Before the deadline nothing happens, however often it is checked.
    rig.handle.idle(Instant::now());
    assert_eq!(expectation.try_wait(), None);
    assert_eq!(rig.handle.pending(), 1);
    // After it, the check resolves it, with the time it took to notice.
    rig.handle.idle(Instant::now() + Duration::from_secs(1));
    match expectation.try_wait() {
        Some(ExpectResult::TimedOut { elapsed }) => {
            assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
        }
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert_eq!(rig.handle.pending(), 0);
}

#[test]
fn a_chunk_is_a_check_too() {
    let mut rig = Rig::new();
    let expectation = rig
        .handle
        .expect("never", Duration::from_millis(20))
        .unwrap();
    thread::sleep(Duration::from_millis(40));
    // Unrelated data arriving after the deadline expires it without an idle tick.
    rig.feed(b"something else\r\n");
    assert!(matches!(
        expectation.try_wait(),
        Some(ExpectResult::TimedOut { .. })
    ));
}

#[test]
fn a_chunk_received_after_the_deadline_does_not_count() {
    let mut rig = Rig::new();
    let expectation = rig.handle.expect("^OK", Duration::from_millis(10)).unwrap();
    // The chunk's own timestamp is past the deadline, though the ingest thread is
    // handling it early: too late, whatever the line says.
    rig.feed_at(b"OK\r\n", Instant::now() + Duration::from_secs(1));
    assert!(
        !matches!(expectation.try_wait(), Some(ExpectResult::Matched { .. })),
        "{:?}",
        expectation.try_wait()
    );
    rig.handle.idle(Instant::now() + Duration::from_secs(2));
    assert!(matches!(
        expectation.try_wait(),
        Some(ExpectResult::TimedOut { .. })
    ));
}

#[test]
fn a_chunk_received_before_the_deadline_counts_even_if_handled_late() {
    let mut rig = Rig::new();
    let expectation = rig
        .handle
        .expect("^OK", Duration::from_millis(200))
        .unwrap();
    let at = Instant::now();
    // The ingest thread gets to it after the deadline, but it was received in time.
    thread::sleep(Duration::from_millis(250));
    rig.feed_at(b"OK\r\n", at);
    let (line, ..) = matched(expectation.try_wait());
    assert_eq!(line, LineId(0));
}

#[test]
fn elapsed_runs_to_the_chunk_not_to_the_ingest_thread() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^OK");
    let at = Instant::now() + Duration::from_millis(40);
    rig.feed_at(b"OK\r\n", at);
    let Some(ExpectResult::Matched { elapsed, .. }) = expectation.try_wait() else {
        panic!("a match");
    };
    assert!(elapsed >= Duration::from_millis(40), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}

#[test]
fn an_absurd_timeout_means_never() {
    let rig = Rig::new();
    let expectation = rig.handle.expect("x", Duration::MAX).unwrap();
    rig.handle.idle(Instant::now() + Duration::from_secs(3600));
    assert_eq!(expectation.try_wait(), None);
}

#[test]
fn cancel_gives_up_and_wakes_the_waiter() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^OK");
    let other = rig.expect("^OK");
    let waiter = {
        let expectation = expectation.clone();
        thread::spawn(move || expectation.wait())
    };
    thread::sleep(Duration::from_millis(30));
    expectation.cancel();
    assert_eq!(waiter.join().unwrap(), ExpectResult::Closed);
    assert_eq!(rig.handle.pending(), 1, "only the other one is left");
    rig.feed(b"OK\r\n");
    assert_eq!(expectation.try_wait(), Some(ExpectResult::Closed));
    assert!(matches!(
        other.try_wait(),
        Some(ExpectResult::Matched { .. })
    ));
}

#[test]
fn cancel_after_a_result_changes_nothing() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^OK");
    rig.feed(b"OK\r\n");
    let before = expectation.try_wait();
    expectation.cancel();
    assert_eq!(expectation.try_wait(), before);
    assert!(matches!(before, Some(ExpectResult::Matched { .. })));
}

#[test]
fn close_resolves_everything_and_everything_registered_later() {
    let rig = Rig::new();
    let a = rig.expect("a");
    let b = rig.expect("b");
    rig.handle.close();
    assert_eq!(a.wait(), ExpectResult::Closed);
    assert_eq!(b.wait(), ExpectResult::Closed);
    assert_eq!(rig.handle.pending(), 0);
    let late = rig.expect("c");
    assert_eq!(late.try_wait(), Some(ExpectResult::Closed));
}

#[test]
fn an_abandoned_expectation_is_forgotten() {
    let mut rig = Rig::new();
    let kept = rig.expect("keep");
    drop(rig.expect("drop"));
    let clone = rig.expect("clone");
    drop(clone.clone());
    assert_eq!(rig.handle.pending(), 2, "a live clone still holds it");
    rig.feed(b"nothing\r\n");
    assert_eq!(rig.handle.pending(), 2);
    drop(kept);
    drop(clone);
    assert_eq!(rig.handle.pending(), 0);
    // The registry sweeps them on the next chunk.
    rig.feed(b"x\r\n");
    assert_eq!(rig.handle.core.registry.lock().entries.len(), 0);
}

#[test]
fn an_invalid_pattern_is_the_regex_error() {
    let rig = Rig::new();
    let err = rig
        .handle
        .expect("(unclosed", Duration::from_secs(1))
        .expect_err("not a regex");
    assert!(err.to_string().contains("unclosed"), "{err}");
    assert_eq!(rig.handle.pending(), 0);
}

#[test]
fn wait_blocks_until_the_line_arrives() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^go");
    let waiter = {
        let expectation = expectation.clone();
        thread::spawn(move || expectation.wait())
    };
    thread::sleep(Duration::from_millis(30));
    assert!(!waiter.is_finished());
    rig.feed(b"go!\r\n");
    let result = waiter.join().unwrap();
    assert!(matches!(result, ExpectResult::Matched { .. }), "{result:?}");
}

#[test]
fn wait_timeout_returns_none_while_pending() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^go");
    let started = Instant::now();
    assert_eq!(expectation.wait_timeout(Duration::from_millis(50)), None);
    assert!(started.elapsed() >= Duration::from_millis(50));
    // It stays registered, and can still resolve.
    assert_eq!(rig.handle.pending(), 1);
    rig.feed(b"go\r\n");
    assert!(matches!(
        expectation.wait_timeout(Duration::from_secs(5)),
        Some(ExpectResult::Matched { .. })
    ));
}

#[test]
fn handles_and_expectations_cross_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<MatcherHandle>();
    assert_send_sync::<Expectation>();
    assert_send_sync::<ExpectResult>();

    // Several threads register while another feeds; nothing deadlocks or is lost.
    let mut rig = Rig::new();
    let handle = rig.handle.clone();
    let registrars: Vec<_> = (0..4)
        .map(|_| {
            let handle = handle.clone();
            thread::spawn(move || {
                let mut resolved = 0;
                for _ in 0..50 {
                    let expectation = handle.expect("^tick", Duration::from_secs(10)).unwrap();
                    if matches!(
                        expectation.wait_timeout(Duration::from_secs(10)),
                        Some(ExpectResult::Matched { .. })
                    ) {
                        resolved += 1;
                    }
                }
                resolved
            })
        })
        .collect();
    let deadline = Instant::now() + Duration::from_secs(20);
    while registrars.iter().any(|thread| !thread.is_finished()) {
        assert!(Instant::now() < deadline, "the registrars never finished");
        rig.feed(b"tick\r\n");
        thread::yield_now();
    }
    for registrar in registrars {
        assert_eq!(registrar.join().unwrap(), 50);
    }
}

#[test]
fn debug_output_names_the_state() {
    let rig = Rig::new();
    let expectation = rig.expect("^OK");
    assert!(format!("{:?}", rig.handle).contains("pending: 1"));
    let shown = format!("{expectation:?}");
    assert!(shown.contains("^OK") && shown.contains("None"), "{shown}");
    assert_eq!(expectation.pattern(), "^OK");
    assert_eq!(expectation.timeout(), Duration::from_secs(60));
}

#[test]
fn a_match_reports_where_it_sits_in_the_line() {
    let mut rig = Rig::new();
    let expectation = rig.expect(r"(\w+)=(\d+)(x)?");
    // The arrow is three bytes, so byte ranges and character counts differ.
    rig.feed("\u{2192} temp=41 ok\r\n".as_bytes());
    let Some(ExpectResult::Matched {
        text,
        captures,
        range,
        capture_ranges,
        ..
    }) = expectation.try_wait()
    else {
        panic!("expected a match");
    };
    assert_eq!(text, "\u{2192} temp=41 ok");
    assert_eq!(range, 4..11);
    assert_eq!(&text[range], "temp=41");
    assert_eq!(
        capture_ranges,
        [Some(4..11), Some(4..8), Some(9..11), None],
        "the whole match, each group, and the group that sat out"
    );
    for (captured, range) in captures.iter().zip(&capture_ranges) {
        assert_eq!(
            captured.as_deref(),
            range.clone().map(|range| &text[range]),
            "each range slices out its capture"
        );
    }
}

#[test]
fn the_first_match_in_the_line_is_the_one_reported() {
    let mut rig = Rig::new();
    let expectation = rig.expect(r"\d+");
    rig.feed(b"a 12 b 345\r\n");
    let Some(ExpectResult::Matched {
        captures,
        range,
        capture_ranges,
        ..
    }) = expectation.try_wait()
    else {
        panic!("expected a match");
    };
    assert_eq!(range, 2..4);
    assert_eq!(captures, [Some("12".to_owned())]);
    assert_eq!(capture_ranges, [Some(2..4)]);
}

#[test]
fn a_pattern_that_matches_nothing_in_particular_has_an_empty_range() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^");
    rig.feed(b"anything\r\n");
    let Some(ExpectResult::Matched {
        text,
        captures,
        range,
        capture_ranges,
        ..
    }) = expectation.try_wait()
    else {
        panic!("expected a match");
    };
    assert_eq!(text, "anything");
    assert_eq!(range, 0..0);
    assert_eq!(captures, [Some(String::new())]);
    assert_eq!(capture_ranges, [Some(0..0)]);
}

#[test]
fn ranges_follow_the_smart_case_regex_and_the_decoded_text() {
    let mut rig = Rig::new();
    // No capital in the pattern, so it ignores case; the line's text is what the parser
    // made of it, with the escape sequences gone, not the raw bytes.
    let expectation = rig.expect("ready");
    rig.feed(b"\x1b[32mSystem READY\x1b[0m\r\n");
    let Some(ExpectResult::Matched {
        text,
        range,
        capture_ranges,
        ..
    }) = expectation.try_wait()
    else {
        panic!("expected a match");
    };
    assert_eq!(text, "System READY");
    assert_eq!(range, 7..12);
    assert_eq!(&text[range.clone()], "READY");
    assert_eq!(capture_ranges, [Some(range)]);
}

impl Rig {
    /// Text typed in line, as the ingest thread applies it.
    fn type_inline(&mut self, text: &str) {
        self.store.append_local_inline(text, Direction::Tx);
        self.handle.advance(self.store.end(), Instant::now());
    }

    /// Typed text taken back, as the ingest thread applies it.
    fn truncate(&mut self, chars: usize) {
        if self.store.truncate_local_line(chars) > 0 {
            self.handle.retreat(self.store.end(), Instant::now());
        }
    }
}

#[test]
fn typing_does_not_interrupt_a_line_the_matchers_are_waiting_for() {
    let mut rig = Rig::new();
    let expectation = rig.expect("^ok$");
    rig.feed(b"o");
    // A whole local line would end "o" here, unmatched. Typing leaves it alone.
    rig.type_inline("x");
    assert_eq!(expectation.try_wait(), None);
    rig.feed(b"k\r\n");
    let (line, text, _) = matched(expectation.try_wait());
    assert_eq!((line, text.as_str()), (LineId(0), "ok"));
}

#[test]
fn typed_lines_are_never_matched() {
    let mut rig = Rig::new();
    let expectation = rig.expect("ok");
    rig.type_inline("ok\nok");
    rig.feed(b"nothing\r\n");
    rig.feed(b"nope\r\n");
    assert_eq!(expectation.try_wait(), None);
    rig.feed(b"ok\r\n");
    // Only the received "ok" counts, though two typed lines say "ok" before it.
    let (line, text, _) = matched(expectation.try_wait());
    assert_eq!(text, "ok");
    assert_eq!(
        rig.store.snapshot().line(line).unwrap().direction,
        Direction::Rx
    );
}

#[test]
fn an_expectation_registered_over_typed_text_that_is_taken_back_still_sees_the_next_line() {
    let mut rig = Rig::new();
    rig.type_inline("ab");
    // Registered while the typed line has id 0, so it starts at line 1...
    let expectation = rig.expect("^OK$");
    // ...which is where the line that arrives next lands only if the typed one stays.
    rig.truncate(2);
    rig.feed(b"OK\r\n");
    let (line, text, _) = matched(expectation.try_wait());
    assert_eq!((line, text.as_str()), (LineId(0), "OK"));

    // With the typed line kept, the reply is line 1 and counts all the same.
    let mut rig = Rig::new();
    rig.type_inline("ab");
    let expectation = rig.expect("^OK$");
    rig.feed(b"OK\r\n");
    let (line, _, _) = matched(expectation.try_wait());
    assert_eq!(line, LineId(1));
}
