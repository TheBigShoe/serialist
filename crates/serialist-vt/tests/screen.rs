//! The screen's behavior over time: ids as rows scroll out, resizing, damage and
//! generations, synchronized updates, resets.

mod common;

use std::time::{Duration, Instant};

use serialist_core::{LineId, LineSource};
use serialist_vt::{MAX_ZERO_WIDTH, MouseEncoding, MouseMode, MouseReporting, VtEvent, VtScreen};

use common::{in_pieces, one_shot, screen_text, scrollback_text, t0};

fn numbered(from: usize, to: usize) -> Vec<u8> {
    (from..to)
        .map(|n| format!("line {n}\r\n"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn scrollback_ids_are_stable_as_lines_scroll_out() {
    let mut screen = VtScreen::new(20, 5, 50);
    screen.feed(&numbered(0, 30));
    let snap = screen.snapshot();
    // 30 lines and the cursor on a blank sixth row: 26 rows went off the top.
    assert_eq!(snap.first_visible(), LineId(26));
    for n in 0..26u64 {
        let line = snap.line(LineId(n)).expect("retained");
        assert_eq!(line.text, format!("line {n}"));
        assert!(line.complete, "scrollback rows are final");
    }
    let row = snap.line(LineId(26)).unwrap();
    assert_eq!(row.text, "line 26");
    assert!(!row.complete, "screen rows can still change");

    // More lines: the same ids still name the same text, until they are evicted.
    screen.feed(&numbered(30, 100));
    let later = screen.snapshot();
    assert_eq!(later.first_visible(), LineId(96));
    assert_eq!(later.scrollback_lines(), 50);
    assert_eq!(later.first_line(), LineId(46));
    assert!(later.line(LineId(45)).is_none(), "evicted");
    for n in 46..96u64 {
        assert_eq!(later.line(LineId(n)).unwrap().text, format!("line {n}"));
    }
    // The earlier snapshot is untouched.
    assert_eq!(snap.line(LineId(3)).unwrap().text, "line 3");
    assert_eq!(snap.first_visible(), LineId(26));
}

#[test]
fn a_row_keeps_its_id_and_time_while_it_scrolls_up_and_out() {
    let mut screen = VtScreen::new(20, 3, 100);
    let first = Instant::now();
    screen.feed_at(b"alpha\r\n", first);
    let snap = screen.snapshot();
    let alpha = snap.line(LineId(0)).unwrap();
    assert_eq!((alpha.text.as_str(), alpha.received_at), ("alpha", first));

    let second = first + Duration::from_millis(5);
    screen.feed_at(b"beta\r\n", second);
    let snap = screen.snapshot();
    assert_eq!(snap.line(LineId(0)).unwrap().received_at, first);
    assert_eq!(snap.line(LineId(1)).unwrap().received_at, second);

    let third = second + Duration::from_millis(5);
    screen.feed_at(b"gamma\r\ndelta", third);
    let snap = screen.snapshot();
    assert_eq!(scrollback_text(&snap), ["alpha"]);
    assert_eq!(screen_text(&snap), ["beta", "gamma", "delta"]);
    // Alpha scrolled out and beta moved to the top row; both kept id and time.
    assert_eq!(snap.line(LineId(0)).unwrap().received_at, first);
    assert_eq!(snap.line(LineId(1)).unwrap().text, "beta");
    assert_eq!(snap.line(LineId(1)).unwrap().received_at, second);
    assert_eq!(snap.line(LineId(2)).unwrap().text, "gamma");
    assert_eq!(snap.line(LineId(2)).unwrap().received_at, third);
}

#[test]
fn a_rewritten_row_keeps_its_id_but_not_its_text() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"menu item\r\n");
    let before = screen.snapshot();
    screen.feed(b"\x1b[1;1Hother item\x1b[K");
    let after = screen.snapshot();
    assert_eq!(before.line(LineId(0)).unwrap().text, "menu item");
    assert_eq!(after.line(LineId(0)).unwrap().text, "other item");
    assert_eq!(after.changed_since(&before), LineId(0)..LineId(1));
}

#[test]
fn changed_since_covers_new_and_rewritten_rows() {
    let mut screen = VtScreen::new(20, 4, 100);
    screen.feed(b"a\r\nb\r\nc");
    let s1 = screen.snapshot();
    let same = screen.snapshot();
    assert_eq!(same.generation(), s1.generation(), "nothing changed");
    assert_eq!(same.changed_since(&s1), s1.end()..s1.end());

    // A cursor move changes no row, but it is a new snapshot.
    screen.feed(b"\x1b[1;1H");
    let s2 = screen.snapshot();
    assert!(s2.generation() > s1.generation());
    assert_eq!(s2.changed_since(&s1), s2.end()..s2.end());

    // Rewriting the third row.
    screen.feed(b"\x1b[3;1Hz");
    let s3 = screen.snapshot();
    assert_eq!(s3.changed_since(&s2), LineId(2)..LineId(3));

    // Scrolling two rows off: the rows that were on screen are the same lines (same
    // ids, same text), only the new bottom rows are new.
    screen.feed(b"\x1b[4;1H\n\nnew");
    let s4 = screen.snapshot();
    assert_eq!(s4.first_visible(), LineId(2));
    assert_eq!(s4.changed_since(&s3), LineId(4)..LineId(6));
    assert_eq!(screen_text(&s4), ["z", "", "", "new"]);
}

#[test]
fn feeding_nothing_new_keeps_the_generation() {
    let mut screen = VtScreen::new(20, 4, 100);
    screen.feed(b"hello");
    let s1 = screen.snapshot();
    // A query changes no row, cursor, mode or title.
    screen.feed(b"\x1b[c");
    let s2 = screen.snapshot();
    assert_eq!(s2.generation(), s1.generation());
    assert_eq!(
        screen.take_events(),
        [VtEvent::Respond(b"\x1b[?6c".to_vec())]
    );
}

#[test]
fn resize_keeps_content() {
    let mut screen = VtScreen::new(20, 6, 100);
    screen.feed(b"one\r\ntwo\r\nthree\r\nfour");
    let before = screen.snapshot();
    assert_eq!(
        screen_text(&before),
        ["one", "two", "three", "four", "", ""]
    );

    // Wider and taller: rows stay put, blank rows below.
    screen.resize(30, 8);
    let wider = screen.snapshot();
    assert_eq!((wider.columns(), wider.viewport_rows()), (30, 8));
    assert_eq!(
        screen_text(&wider),
        ["one", "two", "three", "four", "", "", "", ""]
    );
    assert_eq!(wider.first_visible(), LineId(0));

    // Shorter than the cursor row: the top rows scroll into the scrollback.
    screen.resize(30, 2);
    let short = screen.snapshot();
    assert_eq!(scrollback_text(&short), ["one", "two"]);
    assert_eq!(screen_text(&short), ["three", "four"]);
    let cursor = short.cursor().unwrap();
    assert_eq!((cursor.line, cursor.column), (LineId(3), 4));

    // Narrower than a line: it rewraps.
    screen.feed(b"\r\nabcdefghij");
    screen.resize(5, 3);
    let narrow = screen.snapshot();
    let all: Vec<String> = {
        let mut lines = Vec::new();
        narrow.lines(narrow.first_line()..narrow.end(), &mut lines);
        lines.into_iter().map(|l| l.text).collect()
    };
    let joined = all.concat();
    assert!(joined.contains("onetwothreefour"), "{all:?}");
    assert!(joined.contains("abcdefghij"), "{all:?}");
    // The scrollback keeps its ids through every resize.
    assert_eq!(narrow.line(LineId(0)).unwrap().text, "one");
    assert_eq!(narrow.line(LineId(1)).unwrap().text, "two");

    // Clamped to the minimum.
    screen.resize(0, 0);
    let tiny = screen.snapshot();
    assert_eq!((tiny.columns(), tiny.viewport_rows()), (2, 1));
}

#[test]
fn rows_pushed_off_during_a_resize_on_the_alternate_screen_arrive_on_return() {
    let mut screen = VtScreen::new(20, 4, 100);
    screen.feed(b"p1\r\np2\r\np3\r\np4\x1b[?1049h");
    screen.feed(b"full screen app");
    screen.resize(20, 2);
    let on_alt = screen.snapshot();
    assert!(on_alt.modes().alternate_screen);
    assert_eq!(on_alt.scrollback_lines(), 0);
    screen.feed(b"\x1b[?1049l");
    let back = screen.snapshot();
    assert!(!back.modes().alternate_screen);
    assert_eq!(scrollback_text(&back), ["p1", "p2"]);
    assert_eq!(screen_text(&back), ["p3", "p4"]);
}

#[test]
fn reset_forgets_screen_and_scrollback_but_not_ids() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"a\r\nb\r\nc\r\nd\r\ne\x1b]0;title\x07\x1b[31");
    let before = screen.snapshot();
    assert_eq!(before.first_visible(), LineId(2));
    screen.reset();
    screen.feed(b"m");
    let after = screen.snapshot();
    assert_eq!(after.scrollback_lines(), 0);
    assert_eq!(after.first_line(), LineId(2));
    assert_eq!(after.title(), None);
    // The half-received SGR was dropped, so `m` is text.
    assert_eq!(screen_text(&after), ["m", "", ""]);
    assert_eq!(
        screen.take_events(),
        [VtEvent::Title("title".into()), VtEvent::ResetTitle]
    );
}

#[test]
fn zero_scrollback_still_counts_ids() {
    let mut screen = VtScreen::new(20, 2, 0);
    screen.feed(&numbered(0, 10));
    let snap = screen.snapshot();
    assert_eq!(snap.scrollback_lines(), 0);
    assert_eq!(snap.first_visible(), LineId(9));
    assert_eq!(snap.first_line(), LineId(9));
    assert_eq!(screen_text(&snap), ["line 9", ""]);
}

#[test]
fn a_synchronized_update_shows_at_its_end() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"old");
    screen.feed(b"\x1b[?2026h\x1b[1;1Hnew");
    assert!(screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "old", "held back");
    screen.feed(b"!\x1b[?2026l");
    assert!(!screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "new!");
}

#[test]
fn an_abandoned_synchronized_update_is_flushed() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"\x1b[?2026hstuck");
    assert!(!screen.flush_sync(Instant::now()), "not timed out yet");
    assert_eq!(screen_text(&screen.snapshot())[0], "");
    assert!(screen.flush_sync(Instant::now() + Duration::from_secs(1)));
    assert_eq!(screen_text(&screen.snapshot())[0], "stuck");

    screen.feed(b"\x1b[?2026h more");
    assert!(screen.end_sync());
    assert!(!screen.end_sync());
    assert_eq!(screen_text(&screen.snapshot())[0], "stuck more");
}

#[test]
fn color_queries_are_answered_by_the_app() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"\x1b]11;?\x07");
    let events = screen.take_events();
    let [VtEvent::ColorRequest(request)] = &events[..] else {
        panic!("{events:?}");
    };
    assert_eq!(request.index, 257, "the default background");
    assert_eq!(
        request.answer(0x12, 0x34, 0x56),
        b"\x1b]11;rgb:1212/3434/5656\x07"
    );
}

#[test]
fn clipboard_writes_are_refused() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07ok");
    assert!(screen.take_events().is_empty());
    assert_eq!(screen_text(&screen.snapshot())[0], "ok");
}

#[test]
fn modes_the_ui_acts_on() {
    let mut screen = VtScreen::new(20, 3, 100);
    let modes = screen.snapshot().modes();
    assert!(!modes.app_cursor_keys && !modes.bracketed_paste && modes.mouse.is_none());
    screen.feed(b"\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b=\x1b[?25l\x1b[5 q");
    let snap = screen.snapshot();
    let modes = snap.modes();
    assert!(modes.app_cursor_keys);
    assert!(modes.bracketed_paste);
    assert_eq!(
        modes.mouse,
        Some(MouseReporting {
            mode: MouseMode::Click,
            encoding: MouseEncoding::X10
        })
    );
    assert!(modes.app_keypad);
    let cursor = snap.cursor().unwrap();
    assert!(!cursor.visible);
    assert_eq!(cursor.shape, serialist_vt::CursorShape::Beam);
    assert!(cursor.blinking);
}

#[test]
fn mouse_modes_and_encodings() {
    let mut screen = VtScreen::new(20, 3, 100);
    let mut mouse = |bytes: &[u8]| {
        screen.feed(bytes);
        screen.snapshot().modes().mouse
    };
    let on = |mode, encoding| Some(MouseReporting { mode, encoding });
    assert_eq!(
        mouse(b"\x1b[?1002h"),
        on(MouseMode::Drag, MouseEncoding::X10)
    );
    assert_eq!(
        mouse(b"\x1b[?1006h"),
        on(MouseMode::Drag, MouseEncoding::Sgr)
    );
    // The last mode set wins, and the encoding stays.
    assert_eq!(
        mouse(b"\x1b[?1003h"),
        on(MouseMode::Motion, MouseEncoding::Sgr)
    );
    assert_eq!(
        mouse(b"\x1b[?1005h"),
        on(MouseMode::Motion, MouseEncoding::Utf8)
    );
    assert_eq!(mouse(b"\x1b[?1003l"), None);
    // An encoding alone asks for nothing.
    assert_eq!(mouse(b"\x1b[?1006h"), None);
    assert_eq!(
        mouse(b"\x1b[?1000h"),
        on(MouseMode::Click, MouseEncoding::Sgr)
    );
    // A full reset turns it off.
    assert_eq!(mouse(b"\x1bc"), None);
}

/// Alacritty 0.26 erases above the cursor (`CSI 1 J`) only when the cursor is below the
/// second row; on the second row the first row survives. Recorded here so a change in
/// that behavior is noticed.
#[test]
fn erase_above_from_the_second_row_leaves_the_first() {
    let mut screen = VtScreen::new(10, 4, 100);
    screen.feed_at(b"aaaa\r\nbbbb\r\ncccc\x1b[2;2H\x1b[1J", t0());
    assert_eq!(
        screen_text(&screen.snapshot()),
        ["aaaa", "  bb", "cccc", ""]
    );
}

#[test]
fn the_event_queue_is_bounded() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(&b"\x1b[5n".repeat(serialist_vt::MAX_EVENTS + 10));
    assert_eq!(screen.take_events().len(), serialist_vt::MAX_EVENTS);
}

/// vte 0.15.0 skips the bytes after a character that the next chunk completes when they
/// hold another whole character and then an invalid or cut-off one: its buffer of up to
/// four bytes yields the first character but consumes everything valid in it. The screen
/// holds back a cut-off character instead, so vte never sees one split across calls.
#[test]
fn a_character_cut_by_a_chunk_boundary_does_not_eat_the_text_after_it() {
    let cut_off_next: [&[u8]; 3] = [b"caf\xc3", b"\xa9 \xe2\x82", b"\xac end"];
    let invalid_next: [&[u8]; 2] = [b"caf\xc3", b"\xa9 \xff end"];
    for (chunks, expected) in [
        (&cut_off_next[..], "caf\u{e9} \u{20ac} end"),
        (&invalid_next[..], "caf\u{e9} \u{fffd} end"),
    ] {
        let mut screen = VtScreen::new(20, 3, 100);
        for chunk in chunks {
            screen.feed_at(chunk, t0());
        }
        assert_eq!(screen_text(&screen.snapshot())[0], expected);
        assert_eq!(
            screen.bytes_fed(),
            chunks.iter().map(|c| c.len()).sum::<usize>() as u64
        );
    }
}

#[test]
fn text_is_the_same_cut_anywhere_twice_including_inside_characters() {
    let mut bytes = "ab\u{e9} \u{20ac}\u{1F600} \u{65e5}x \u{ff}\u{fe}\r\nfin"
        .as_bytes()
        .to_vec();
    bytes.extend_from_slice(b"\xff\x9b\xc3 \xe2\x82!");
    bytes.extend_from_slice("\u{1F600}\u{e9}".as_bytes());
    let whole = one_shot(10, 3, &bytes);
    for first in 1..bytes.len() {
        for second in first..bytes.len() {
            assert_eq!(
                in_pieces(10, 3, &bytes, &[first, second]),
                whole,
                "cut at {first} and {second}"
            );
        }
    }
}

#[test]
fn a_character_cut_off_at_the_end_waits_for_the_rest_and_a_reset_forgets_it() {
    let mut screen = VtScreen::new(10, 2, 100);
    screen.feed_at(b"a\xe2\x82", t0());
    assert_eq!(screen_text(&screen.snapshot())[0], "a");
    screen.feed_at(b"\xacb", t0());
    assert_eq!(screen_text(&screen.snapshot())[0], "a\u{20ac}b");

    screen.feed_at(b"\r\n\xf0\x9f", t0());
    screen.reset();
    screen.feed_at(b"\x98\x80z", t0());
    // The reset dropped the cut-off emoji, so its tail is two stray C1 controls, which
    // draw nothing (had the screen kept the bytes, it would show the emoji).
    assert_eq!(screen_text(&screen.snapshot())[0], "z");
}

#[test]
fn a_cell_keeps_at_most_max_zero_width_marks() {
    let mut screen = VtScreen::new(20, 2, 10);
    let mut bytes = Vec::new();
    // Far more marks than the cap on a narrow character, on a wide one (they join its
    // first cell), and then a fresh cell, which is not full.
    bytes.extend_from_slice("a".as_bytes());
    bytes.extend_from_slice("\u{301}".repeat(MAX_ZERO_WIDTH + 50).as_bytes());
    bytes.extend_from_slice("\u{65e5}".as_bytes());
    bytes.extend_from_slice("\u{302}".repeat(MAX_ZERO_WIDTH + 1).as_bytes());
    bytes.extend_from_slice("b\u{303}".as_bytes());
    screen.feed(&bytes);
    let snap = screen.snapshot();
    assert_eq!(
        screen_text(&snap)[0],
        format!(
            "a{}\u{65e5}{}b\u{303}",
            "\u{301}".repeat(MAX_ZERO_WIDTH),
            "\u{302}".repeat(MAX_ZERO_WIDTH)
        )
    );
    // The cursor did not move for any of the dropped marks: a, the two cells of the
    // wide character, b.
    assert_eq!(snap.cursor().expect("a cursor").column, 4);
}

#[test]
fn a_mark_on_the_last_column_joins_that_cell_up_to_the_cap() {
    // After the last column is written the cursor stays on it, waiting to wrap, and a
    // mark joins that cell rather than the one before.
    let mut screen = VtScreen::new(4, 2, 10);
    let mut bytes = b"abcd".to_vec();
    bytes.extend_from_slice("\u{301}".repeat(MAX_ZERO_WIDTH + 5).as_bytes());
    bytes.extend_from_slice(b"e");
    screen.feed(&bytes);
    let snap = screen.snapshot();
    assert_eq!(
        screen_text(&snap),
        [
            format!("abcd{}", "\u{301}".repeat(MAX_ZERO_WIDTH)),
            "e".to_string()
        ]
    );
}

#[test]
fn repeating_a_mark_does_not_grow_its_cell() {
    // `CSI 65535 b` repeats the last character that many times: eleven bytes that would
    // add 65535 marks to one cell, and a snapshot would copy them all.
    let mut screen = VtScreen::new(20, 2, 10);
    let mut bytes = "e\u{301}".as_bytes().to_vec();
    for _ in 0..200 {
        bytes.extend_from_slice(b"\x1b[65535b");
    }
    bytes.extend_from_slice(b"x");
    screen.feed(&bytes);
    let snap = screen.snapshot();
    assert_eq!(
        screen_text(&snap)[0],
        format!("e{}x", "\u{301}".repeat(MAX_ZERO_WIDTH))
    );
}

#[test]
fn insert_mode_moves_the_rest_of_the_row_right() {
    // `CSI 4 h`: a character pushes what is under and after the cursor along, and what
    // reaches the end of the row falls off.
    let mut screen = VtScreen::new(8, 2, 10);
    screen.feed(b"abcdef\x1b[1;2H\x1b[4hXY");
    assert_eq!(screen_text(&screen.snapshot())[0], "aXYbcdef");
    screen.feed(b"Z");
    let snap = screen.snapshot();
    assert_eq!(screen_text(&snap)[0], "aXYZbcde");
    assert_eq!(snap.cursor().expect("a cursor").column, 4);
    // Back in replace mode a character overwrites.
    screen.feed(b"\x1b[4l12");
    assert_eq!(screen_text(&screen.snapshot())[0], "aXYZ12de");
}

#[test]
fn a_repeat_in_insert_mode_fills_rows_and_pushes_text_off() {
    // The repeat goes through blank cells (nothing to move), wraps, and then pushes
    // `tail` off the end of the second row a cell at a time.
    let mut screen = VtScreen::new(10, 3, 10);
    screen.feed(b"\x1b[2;4Htail\x1b[1;1H\x1b[4hx\x1b[12b");
    assert_eq!(
        screen_text(&screen.snapshot()),
        ["xxxxxxxxxx", "xxx   tail", ""]
    );
    screen.feed(b"\x1b[3b");
    assert_eq!(
        screen_text(&screen.snapshot()),
        ["xxxxxxxxxx", "xxxxxx   t", ""]
    );
    // A wide character takes two cells and moves the rest by two.
    screen.feed(b"\x1b[3;1Hab\x1b[3;1H\xe6\x97\xa5");
    assert_eq!(screen_text(&screen.snapshot())[2], "\u{65e5}ab");
}

const BEGIN: &[u8] = b"\x1b[?2026h";
const END: &[u8] = b"\x1b[?2026l";

fn bytes(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn what_arrives_ahead_of_an_update_in_the_same_chunk_shows_at_once() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(&bytes(&[b"before", BEGIN, b"\x1b[2;1Hduring"]));
    assert!(screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot()), ["before", "", ""]);
    screen.feed(END);
    assert_eq!(screen_text(&screen.snapshot()), ["before", "during", ""]);
}

#[test]
fn one_update_after_another_shows_each_when_the_next_begins() {
    // A device that redraws without pause: every chunk ends inside an update, so the
    // screen is never seen between two. Each frame shows when the next one begins.
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(&bytes(&[BEGIN, b"\x1b[1;1Hframe"]));
    assert_eq!(screen_text(&screen.snapshot())[0], "");
    screen.feed(&bytes(&[b" one", END, BEGIN, b"\x1b[1;1Hframe"]));
    assert!(screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "frame one");
    screen.feed(&bytes(&[b" two", END, BEGIN, b"\x1b[1;1Hframe thr"]));
    assert_eq!(screen_text(&screen.snapshot())[0], "frame two");
    screen.feed(&bytes(&[b"ee", END]));
    assert_eq!(screen_text(&screen.snapshot())[0], "frame three");
}

#[test]
fn a_chunk_of_many_updates_takes_one_snapshot_for_them() {
    let mut screen = VtScreen::new(20, 3, 100);
    let before = screen.snapshot().generation();
    let mut chunk = Vec::new();
    for n in 0..500 {
        chunk.extend_from_slice(&bytes(&[BEGIN, format!("\x1b[1;1H{n}").as_bytes(), END]));
    }
    screen.feed(&chunk);
    let snap = screen.snapshot();
    assert_eq!(screen_text(&snap)[0], "499");
    assert!(
        snap.generation() <= before + 2,
        "{} snapshots for one chunk",
        snap.generation() - before
    );
}

#[test]
fn a_begin_spelled_another_way_opens_an_update_in_any_chunks() {
    // `CSI ? 2026 ; 1 h` sets two modes. vte, holding the bytes of an update back itself,
    // found the end and the next begin by searching for their exact bytes, and so opened
    // an update for this one or not depending on where the chunks were cut.
    let stream = bytes(&[
        b"q",
        BEGIN,
        b"held",
        END,
        b"\x1b[?2026;1h\x1b[2;1Hztail\r\nend",
    ]);
    let whole = {
        let mut screen = VtScreen::new(20, 4, 100);
        screen.feed_at(&stream, t0());
        assert!(screen.sync_pending());
        assert!(screen.end_sync());
        common::print(&screen.snapshot(), screen.take_events())
    };
    assert_eq!(whole.lines[0].1, "qheld");
    assert_eq!(whole.lines[1].1, "ztail");
    for cut in 0..=stream.len() {
        let mut screen = VtScreen::new(20, 4, 100);
        screen.feed_at(&stream[..cut], t0());
        let _ = screen.snapshot();
        screen.feed_at(&stream[cut..], t0());
        assert!(screen.sync_pending(), "cut at {cut}");
        assert!(screen.end_sync());
        let split = common::print(&screen.snapshot(), screen.take_events());
        assert_eq!(split, whole, "cut at {cut}");
    }
}

#[test]
fn queries_inside_an_update_are_answered_at_once() {
    let mut screen = VtScreen::new(20, 3, 100);
    // Where is the cursor, and is mode 2026 set?
    screen.feed(&bytes(&[BEGIN, b"ab\x1b[6n\x1b[?2026$p"]));
    assert!(screen.sync_pending());
    assert_eq!(
        screen.take_events(),
        [
            VtEvent::Respond(b"\x1b[1;3R".to_vec()),
            VtEvent::Respond(b"\x1b[?2026;1$y".to_vec())
        ]
    );
    assert_eq!(screen_text(&screen.snapshot())[0], "", "the view is held");
    screen.feed(&bytes(&[END, b"\x1b[?2026$p"]));
    assert_eq!(
        screen.take_events(),
        [VtEvent::Respond(b"\x1b[?2026;2$y".to_vec())]
    );
}

#[test]
fn a_second_begin_does_not_extend_an_update() {
    let mut screen = VtScreen::new(20, 3, 100);
    let began = Instant::now();
    screen.feed_at(&bytes(&[BEGIN, b"a"]), began);
    screen.feed_at(&bytes(&[BEGIN, b"b"]), began + Duration::from_millis(100));
    assert!(!screen.flush_sync(began + Duration::from_millis(140)));
    assert_eq!(screen_text(&screen.snapshot())[0], "");
    // 150 ms after the first begin, whatever the second said. Feeding checks the clock
    // too, before it parses.
    screen.feed_at(b"c", began + Duration::from_millis(160));
    assert!(!screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "abc");
}

#[test]
fn a_resize_and_a_reset_end_an_update() {
    let mut screen = VtScreen::new(20, 3, 100);
    screen.feed(&bytes(&[BEGIN, b"x"]));
    screen.resize(30, 4);
    assert!(!screen.sync_pending());
    let snap = screen.snapshot();
    assert_eq!((snap.columns(), snap.viewport_rows()), (30, 4));
    assert_eq!(screen_text(&snap)[0], "x");

    // RIS from the device, and the app's own reset.
    screen.feed(&bytes(&[BEGIN, b"y\x1bc"]));
    assert!(!screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "");
    screen.feed(&bytes(&[BEGIN, b"z"]));
    screen.reset();
    assert!(!screen.sync_pending());
    assert_eq!(screen_text(&screen.snapshot())[0], "");
}

/// Every `x` a snapshot holds, scrollback and screen.
fn count_x(screen: &mut VtScreen) -> usize {
    let snap = screen.snapshot();
    scrollback_text(&snap)
        .iter()
        .chain(&screen_text(&snap))
        .map(|row| row.matches('x').count())
        .sum()
}

#[test]
fn a_repeat_writes_a_screenful_at_most() {
    // `CSI n b` repeats the last character n times. Up to a screenful it is exact.
    let mut screen = VtScreen::new(10, 3, 100);
    screen.feed(b"ab\x1b[12bc");
    assert_eq!(screen_text(&screen.snapshot()), ["abbbbbbbbb", "bbbbc", ""]);

    // Past it the rest is dropped: thirty of the 65535, after the one that was typed.
    let mut screen = VtScreen::new(10, 3, 100);
    screen.feed(b"x\x1b[65535b");
    let snap = screen.snapshot();
    assert_eq!(scrollback_text(&snap), ["xxxxxxxxxx"]);
    assert_eq!(screen_text(&snap), ["xxxxxxxxxx", "xxxxxxxxxx", "x"]);
    // Each repeat has its own screenful, and text goes on after it.
    screen.feed(b"\x1b[65535b\x1b[65535by");
    let snap = screen.snapshot();
    assert_eq!(snap.first_visible(), LineId(7));
    assert_eq!(screen_text(&snap), ["xxxxxxxxxx", "xxxxxxxxxx", "xy"]);
}

#[test]
fn a_repeat_is_capped_however_it_is_spelled_and_wherever_it_is_cut() {
    // Parameters the parser reads as a count of 65535 or so, with what it lets stand
    // inside a sequence: more parameters, a control character (which it acts on), DEL and
    // bytes above ASCII (which it skips).
    let spellings: [&[u8]; 6] = [
        b"\x1b[65535b",
        b"\x1b[65535;7b",
        b"\x1b[99999999b",
        b"\x1b[655\r35b",
        b"\x1b[655\x7f35b",
        b"\x1b[655\xc3\xa935b",
    ];
    for spelling in spellings {
        let stream = [b"x", spelling, b"\r\ny"].concat();
        let mut whole = VtScreen::new(10, 3, 100);
        whole.feed_at(&stream, t0());
        let count = count_x(&mut whole);
        assert!(
            (20..=31).contains(&count),
            "{count} characters from {spelling:?}"
        );
        let whole = common::print(&whole.snapshot(), whole.take_events());
        for cut in 0..=stream.len() {
            let split = in_pieces(10, 3, &stream, &[cut]);
            // `in_pieces` keeps 1000 rows of scrollback; nothing here reaches 100.
            assert_eq!(split.lines, whole.lines, "{spelling:?} cut at {cut}");
        }
    }
}

#[test]
fn a_repeat_inside_a_synchronized_update_is_capped_too() {
    let mut screen = VtScreen::new(10, 3, 100);
    screen.feed(&bytes(&[BEGIN, b"x\x1b[65535b\x1b[65535b", END]));
    assert_eq!(count_x(&mut screen), 61);
}

#[test]
fn the_letter_b_in_text_is_only_a_letter() {
    // After a digit, a blank, a control character or a bracket a `b` could end a
    // sequence, and is parsed on its own; it is still the same text.
    let text = "0b1010 [b] (b) b\tb\r\nbob abba \u{e9}b 3b\x1b[1mb\x1b[0mb\x1b]0;b\x07b";
    let whole = one_shot(40, 3, text.as_bytes());
    // The tab goes to column 24.
    assert_eq!(whole.lines[0].1, "0b1010 [b] (b) b        b");
    assert_eq!(whole.lines[1].1, "bob abba \u{e9}b 3bbbb");
    assert_eq!(whole.title.as_deref(), Some("b"));
    for cut in 0..=text.len() {
        assert_eq!(in_pieces(40, 3, text.as_bytes(), &[cut]).lines, whole.lines);
    }
}
