//! Escape-sequence fixtures: what each one leaves on the screen, and that any split of
//! its bytes into chunks leaves exactly the same thing.

mod common;

use proptest::prelude::*;
use serialist_core::{Color, LineId, LineSource, StyleFlags};
use serialist_vt::{CursorShape, VtEvent, VtScreen};

use common::{flags, in_pieces, one_shot, plain, run, screen_text, scrollback_text, t0};

struct Fixture {
    name: &'static str,
    columns: usize,
    rows: usize,
    bytes: Vec<u8>,
}

fn fixture(name: &'static str, columns: usize, rows: usize, bytes: impl Into<Vec<u8>>) -> Fixture {
    Fixture {
        name,
        columns,
        rows,
        bytes: bytes.into(),
    }
}

/// One redraw of a U-Boot style boot menu with item `selected` highlighted: cursor home,
/// then every line rewritten in place with an erase to the end of the line.
fn menu_frame(selected: usize, redraw: u32) -> String {
    let mut out = String::from("\x1b[H");
    out.push_str("\x1b[1;1H  *** U-Boot Boot Menu ***\x1b[K");
    for (i, item) in ["Boot 1", "Boot 2", "U-Boot console"].iter().enumerate() {
        out.push_str(&format!("\x1b[{};1H     ", i + 3));
        if i == selected {
            out.push_str("\x1b[7m");
        }
        out.push_str(item);
        out.push_str("\x1b[0m\x1b[K");
    }
    out.push_str(&format!(
        "\x1b[7;1H  Press UP/DOWN to move, ENTER to select\x1b[K\x1b[8;1H  redraw {redraw}\x1b[K"
    ));
    out
}

fn menu_bytes() -> Vec<u8> {
    let mut out = String::from("\x1b[?25l\x1b[2J");
    for (n, selected) in [0, 1, 2, 1].into_iter().enumerate() {
        out.push_str(&menu_frame(selected, n as u32));
    }
    out.into_bytes()
}

/// A vttest-style drawing: a border of `*` made with absolute and relative cursor moves
/// (CUP, CUB, CUD, CUU), then letters placed with CUF, DECSC/DECRC, RI, IND and CUB.
fn vttest_bytes() -> Vec<u8> {
    let mut out = String::from("\x1b[2J\x1b[H************");
    out.push_str("\x1b[6;1H************");
    out.push_str("\x1b[2;1H*\x1b[D\x1b[B*\x1b[D\x1b[B*\x1b[D\x1b[B*");
    out.push_str("\x1b[5;12H*\x1b[A*\x1b[A*\x1b[A*");
    out.push_str("\x1b[3;3H+\x1b[C+\x1b[C+\x1b7");
    out.push_str("\x1b[4;6HQ\x1b8R");
    out.push_str("\x1b[5;3H\x1bM\x1bMS");
    out.push_str("\x1b[3;3H\x1bDT");
    out.push_str("\x1b[2;9H\x1b[2BU");
    out.push_str("\x1b[5;10H\x1b[3DV\x1b[1;1H");
    out.into_bytes()
}

fn fixtures() -> Vec<Fixture> {
    vec![
        fixture("clear", 20, 5, &b"hello\r\nworld\x1b[2J"[..]),
        fixture("clear-saved", 20, 5, &b"hello\r\nworld\x1b[2J\x1b[3Jafter"[..]),
        fixture(
            "cursor-addressing",
            20,
            6,
            &b"\x1b[3;5HX\x1b[1;1HA\x1b[6;20HZ\x1b[4;4fY\x1b[2;2H"[..],
        ),
        fixture(
            "scroll-region",
            10,
            6,
            &b"1\r\n2\r\n3\r\n4\r\n5\r\n6\x1b[2;4r\x1b[4;1H\nX\x1b[r\x1b[6;1H\n"[..],
        ),
        fixture(
            "erase-line",
            10,
            4,
            &b"abcdefgh\x1b[4G\x1b[K\r\n12345678\x1b[4G\x1b[1K\r\nxxxxxxxx\x1b[2K\r\nqrstuvwx\x1b[3G\x1b[2X"[..],
        ),
        fixture(
            "erase-display",
            10,
            4,
            &b"aaaa\r\nbbbb\r\ncccc\r\ndddd\x1b[3;3H\x1b[1J\x1b[4;3H\x1b[J"[..],
        ),
        fixture(
            "sgr",
            40,
            3,
            &b"\x1b[31mred\x1b[0m \x1b[1;4;38;5;200mX\x1b[0m \x1b[48;2;1;2;3mY\x1b[0m \x1b[92mG\x1b[0m \x1b[2;3;7;9;8mZ\x1b[m \x1b[38;5;3mI\x1b[0m\r\n\x1b[38:2::10:20:30mT\x1b[4:3mU"[..],
        ),
        fixture(
            "alternate-screen",
            10,
            3,
            &b"primary\x1b[?1049halt\r\nscreen\r\nmore\r\nlines\x1b[?1049l"[..],
        ),
        fixture("uboot-menu", 44, 8, menu_bytes()),
        fixture("vttest-cursor", 12, 6, vttest_bytes()),
        fixture(
            "device-attributes",
            20,
            5,
            &b"\x1b[c\x1b[>c\x1b[5n\x1b[3;4H\x1b[6n\x1b]0;router\x07\x07\x07"[..],
        ),
        fixture("wide-and-combining", 10, 2, "日本語\r\ne\u{301}x".as_bytes()),
        fixture(
            "scrolling",
            12,
            4,
            (0..30)
                .map(|n| format!("line {n}\r\n"))
                .collect::<String>()
                .into_bytes(),
        ),
        fixture("autowrap", 5, 3, &b"abcdefghijklmnopqrstu"[..]),
        fixture("reset", 10, 3, &b"a\r\nb\r\nc\r\nd\x1b]2;t\x07\x1bcfresh"[..]),
    ]
}

fn get(name: &str) -> Fixture {
    fixtures()
        .into_iter()
        .find(|f| f.name == name)
        .expect("a fixture by that name")
}

fn run_fixture(name: &str) -> (VtScreen, serialist_vt::VtSnapshot) {
    let f = get(name);
    let mut screen = VtScreen::new(f.columns, f.rows, 1000);
    screen.feed_at(&f.bytes, t0());
    let snapshot = screen.snapshot();
    (screen, snapshot)
}

#[test]
fn clear_screen_scrolls_the_screen_into_the_scrollback() {
    let (_, snap) = run_fixture("clear");
    assert_eq!(scrollback_text(&snap), ["hello", "world"]);
    assert!(screen_text(&snap).iter().all(String::is_empty));
    assert_eq!(snap.first_visible(), LineId(2));
    // The cursor stays where it was.
    let cursor = snap.cursor().unwrap();
    assert_eq!((cursor.line, cursor.column), (LineId(3), 5));

    let (_, snap) = run_fixture("clear-saved");
    assert_eq!(scrollback_text(&snap), Vec::<String>::new());
    assert_eq!(snap.first_line(), LineId(2), "erased ids are not reused");
    assert_eq!(snap.visible_line(1).unwrap().text, "     after");
}

#[test]
fn cursor_addressing_places_text() {
    let (_, snap) = run_fixture("cursor-addressing");
    assert_eq!(
        screen_text(&snap),
        ["A", "", "    X", "   Y", "", "                   Z"]
    );
    let cursor = snap.cursor().unwrap();
    assert_eq!((cursor.line, cursor.column), (LineId(1), 1));
    assert!(cursor.visible);
    assert_eq!(cursor.shape, CursorShape::Block);
}

#[test]
fn a_scroll_region_scrolls_only_its_rows() {
    let (_, snap) = run_fixture("scroll-region");
    // Scrolling inside the region (rows 2 to 4) sent nothing to the scrollback; the
    // full-screen line feed at the end sent the top row.
    assert_eq!(scrollback_text(&snap), ["1"]);
    assert_eq!(screen_text(&snap), ["3", "4", "X", "5", "6", ""]);
}

#[test]
fn erase_in_line_and_display() {
    let (_, snap) = run_fixture("erase-line");
    assert_eq!(screen_text(&snap), ["abc", "    5678", "", "qr  uvwx"]);
    let (_, snap) = run_fixture("erase-display");
    assert_eq!(screen_text(&snap), ["", "", "   c", "dd"]);
    assert_eq!(snap.scrollback_lines(), 0);
}

#[test]
fn sgr_colors_and_attributes_become_runs() {
    let (_, snap) = run_fixture("sgr");
    let d = Color::Default;
    let line = snap.visible_line(0).unwrap();
    assert_eq!(line.text, "red X Y G Z I");
    assert_eq!(
        line.runs,
        [
            run(3, Color::Ansi(1), d, StyleFlags::NONE),
            plain(1),
            run(
                1,
                Color::Indexed(200),
                d,
                flags(&[StyleFlags::BOLD, StyleFlags::UNDERLINE])
            ),
            plain(1),
            run(1, d, Color::Rgb(1, 2, 3), StyleFlags::NONE),
            plain(1),
            run(1, Color::Ansi(10), d, StyleFlags::NONE),
            plain(1),
            run(
                1,
                d,
                d,
                flags(&[
                    StyleFlags::DIM,
                    StyleFlags::ITALIC,
                    StyleFlags::INVERSE,
                    StyleFlags::STRIKETHROUGH,
                    StyleFlags::HIDDEN
                ])
            ),
            plain(1),
            run(1, Color::Ansi(3), d, StyleFlags::NONE),
        ]
    );
    let line = snap.visible_line(1).unwrap();
    assert_eq!(line.text, "TU");
    assert_eq!(
        line.runs,
        [
            run(1, Color::Rgb(10, 20, 30), d, StyleFlags::NONE),
            run(1, Color::Rgb(10, 20, 30), d, StyleFlags::UNDERLINE),
        ]
    );
}

#[test]
fn the_alternate_screen_comes_and_goes() {
    let f = get("alternate-screen");
    let mut screen = VtScreen::new(f.columns, f.rows, 1000);
    let enter = b"primary\x1b[?1049h";
    screen.feed_at(enter, t0());
    let on_alt = screen.snapshot();
    assert!(on_alt.modes().alternate_screen);
    assert!(screen_text(&on_alt).iter().all(String::is_empty));

    // Lines scrolled off the alternate screen are gone, not scrollback.
    screen.feed_at(&f.bytes[enter.len()..f.bytes.len() - 8], t0());
    let snap = screen.snapshot();
    assert_eq!(screen_text(&snap), ["screen", "more", "lines"]);
    assert_eq!(snap.scrollback_lines(), 0);
    assert_eq!(snap.first_visible(), LineId(0));

    screen.feed_at(b"\x1b[?1049l", t0());
    let back = screen.snapshot();
    assert!(!back.modes().alternate_screen);
    assert_eq!(screen_text(&back), ["primary", "", ""]);
    let cursor = back.cursor().unwrap();
    assert_eq!((cursor.line, cursor.column), (LineId(0), 7));
}

#[test]
fn a_menu_redraw_rewrites_rows_in_place() {
    let (_, snap) = run_fixture("uboot-menu");
    assert_eq!(
        screen_text(&snap),
        [
            "  *** U-Boot Boot Menu ***",
            "",
            "     Boot 1",
            "     Boot 2",
            "     U-Boot console",
            "",
            "  Press UP/DOWN to move, ENTER to select",
            "  redraw 3",
        ]
    );
    // Four redraws scrolled nothing: the menu rows keep ids 0 to 7.
    assert_eq!(snap.scrollback_lines(), 0);
    assert_eq!(snap.first_visible(), LineId(0));
    let highlighted: Vec<usize> = (0..snap.viewport_rows())
        .filter(|&row| {
            snap.visible_line(row)
                .unwrap()
                .runs
                .iter()
                .any(|r| r.style.flags.contains(StyleFlags::INVERSE))
        })
        .collect();
    assert_eq!(highlighted, [3], "the last frame highlights Boot 2");
    assert!(!snap.cursor().unwrap().visible, "the menu hid the cursor");
}

#[test]
fn vttest_style_cursor_movement() {
    let (_, snap) = run_fixture("vttest-cursor");
    assert_eq!(
        screen_text(&snap),
        [
            "************",
            "*          *",
            "* S + +R   *",
            "* T  Q  U  *",
            "*     V    *",
            "************",
        ]
    );
    let cursor = snap.cursor().unwrap();
    assert_eq!((cursor.line, cursor.column), (LineId(0), 0));
}

#[test]
fn queries_are_answered_with_respond_events() {
    let (mut screen, snap) = run_fixture("device-attributes");
    let events = screen.take_events();
    assert_eq!(events.len(), 6, "{events:?}");
    assert_eq!(events[0], VtEvent::Respond(b"\x1b[?6c".to_vec()));
    let VtEvent::Respond(secondary) = &events[1] else {
        panic!("{events:?}");
    };
    assert!(secondary.starts_with(b"\x1b[>0;") && secondary.ends_with(b";1c"));
    assert_eq!(events[2], VtEvent::Respond(b"\x1b[0n".to_vec()));
    assert_eq!(events[3], VtEvent::Respond(b"\x1b[3;4R".to_vec()));
    assert_eq!(events[4], VtEvent::Title("router".into()));
    assert_eq!(
        events[5],
        VtEvent::Bell,
        "three bells in a row are one event"
    );
    assert_eq!(snap.title(), Some("router"));
    assert!(screen.take_events().is_empty());
}

#[test]
fn wide_characters_appear_once_and_combining_marks_stay_put() {
    let (_, snap) = run_fixture("wide-and-combining");
    assert_eq!(screen_text(&snap), ["日本語", "e\u{301}x"]);
    let cursor = snap.cursor().unwrap();
    // Three columns of text, but the cursor counts cells: e, x.
    assert_eq!((cursor.line, cursor.column), (LineId(1), 2));
}

#[test]
fn autowrap_continues_on_the_next_row() {
    let (_, snap) = run_fixture("autowrap");
    assert_eq!(scrollback_text(&snap), ["abcde", "fghij"]);
    assert_eq!(screen_text(&snap), ["klmno", "pqrst", "u"]);
}

#[test]
fn a_full_reset_clears_screen_scrollback_and_title() {
    let (mut screen, snap) = run_fixture("reset");
    assert_eq!(scrollback_text(&snap), Vec::<String>::new());
    assert_eq!(screen_text(&snap), ["fresh", "", ""]);
    assert_eq!(
        snap.first_line(),
        LineId(1),
        "the scrolled-off row's id is retired"
    );
    assert_eq!(snap.title(), None);
    assert_eq!(
        screen.take_events(),
        [VtEvent::Title("t".into()), VtEvent::ResetTitle]
    );
}

#[test]
fn every_fixture_is_the_same_split_anywhere_in_two() {
    for f in fixtures() {
        let whole = one_shot(f.columns, f.rows, &f.bytes);
        for cut in 1..f.bytes.len() {
            let split = in_pieces(f.columns, f.rows, &f.bytes, &[cut]);
            assert_eq!(split, whole, "fixture {} cut at {cut}", f.name);
        }
    }
}

#[test]
fn every_fixture_is_the_same_fed_a_byte_at_a_time() {
    for f in fixtures() {
        let whole = one_shot(f.columns, f.rows, &f.bytes);
        let cuts: Vec<usize> = (1..f.bytes.len()).collect();
        assert_eq!(
            in_pieces(f.columns, f.rows, &f.bytes, &cuts),
            whole,
            "fixture {}",
            f.name
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn every_fixture_is_the_same_in_random_chunks(
        pick in 0usize..15,
        mut cuts in proptest::collection::vec(0usize..600, 0..12),
    ) {
        let all = fixtures();
        let f = &all[pick % all.len()];
        cuts.sort_unstable();
        let whole = one_shot(f.columns, f.rows, &f.bytes);
        let split = in_pieces(f.columns, f.rows, &f.bytes, &cuts);
        prop_assert_eq!(split, whole, "fixture {} cuts {:?}", f.name, cuts);
    }
}
