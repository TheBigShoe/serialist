//! The configuration applied to the running app: real loaders from `serialist-core`
//! over a temporary config directory, installed into headless windows.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, PortInfo, PortKind, UsbInfo};
use serialist_sim::{EchoDevice, LinkConfig, SimWorld};

use crate::actions::{OpenSettings, ReloadConfig};
use crate::config::{self, Config, ConfigPiece, Opener, hsla};
use crate::prelude::*;
use crate::session_view::FRAME;
use crate::status::Notice;
use crate::terminal::TerminalView;
use crate::terminal::double::MemoryLines;
use crate::test_support::{
    FakeOpener, FakePortSource, TestDir, allow_engine_threads, displayed, open_test_window,
    open_workspace, port, run_until, wait_connected,
};
use crate::workspace::Workspace;

// --- Fixtures ----------------------------------------------------------------------

/// Every key of the plan's theme-key table, in a fixed order.
const THEME_KEYS: &[&str] = &[
    "terminal.background",
    "terminal.foreground",
    "terminal.bright_foreground",
    "terminal.dim_foreground",
    "terminal.ansi.black",
    "terminal.ansi.red",
    "terminal.ansi.green",
    "terminal.ansi.yellow",
    "terminal.ansi.blue",
    "terminal.ansi.magenta",
    "terminal.ansi.cyan",
    "terminal.ansi.white",
    "terminal.ansi.bright_black",
    "terminal.ansi.bright_red",
    "terminal.ansi.bright_green",
    "terminal.ansi.bright_yellow",
    "terminal.ansi.bright_blue",
    "terminal.ansi.bright_magenta",
    "terminal.ansi.bright_cyan",
    "terminal.ansi.bright_white",
    "background",
    "surface.background",
    "elevated_surface.background",
    "panel.background",
    "title_bar.background",
    "status_bar.background",
    "tab_bar.background",
    "tab.active_background",
    "tab.inactive_background",
    "toolbar.background",
    "element.background",
    "element.hover",
    "element.active",
    "element.selected",
    "ghost_element.background",
    "ghost_element.hover",
    "ghost_element.active",
    "ghost_element.selected",
    "border",
    "border.variant",
    "border.focused",
    "border.selected",
    "text",
    "text.muted",
    "text.accent",
    "icon",
    "icon.muted",
    "icon.accent",
    "scrollbar.thumb.background",
    "scrollbar.thumb.hover_background",
    "scrollbar.track.background",
    "search.match_background",
    "search.active_match_background",
    "success",
    "success.background",
    "warning",
    "warning.background",
    "error",
    "error.background",
    "info",
    "info.background",
    "hint",
    "hint.background",
    "editor.background",
    "editor.foreground",
];

/// A distinct opaque color for key `index` of a theme whose colors start at `seed`, so
/// a key mapped to the wrong slot shows up as a wrong color.
fn fixture_color(seed: u8, index: usize) -> String {
    format!(
        "#{seed:02x}{:02x}{:02x}ff",
        (index * 3) % 256,
        (index * 7 + 11) % 256
    )
}

/// One theme of the fixture, as the Zed schema writes it: flat style keys plus
/// `players` and `syntax` inside `style`.
fn fixture_theme_json(name: &str, appearance: &str, seed: u8) -> String {
    let mut style: Vec<String> = THEME_KEYS
        .iter()
        .enumerate()
        .map(|(ix, key)| format!("        \"{key}\": \"{}\"", fixture_color(seed, ix)))
        .collect();
    let player = THEME_KEYS.len();
    style.push(format!(
        "        \"players\": [{{ \"cursor\": \"{}\", \"selection\": \"{}\", \"background\": \"{}\" }}]",
        fixture_color(seed, player),
        fixture_color(seed, player + 1),
        fixture_color(seed, player + 2),
    ));
    style.push(format!(
        "        \"syntax\": {{ \"number\": {{ \"color\": \"{}\" }}, \"comment\": {{ \"color\": \"{}\", \"font_style\": \"italic\" }} }}",
        fixture_color(seed, player + 3),
        fixture_color(seed, player + 4),
    ));
    format!(
        "    {{\n      \"name\": \"{name}\",\n      \"appearance\": \"{appearance}\",\n      \"style\": {{\n{}\n      }}\n    }}",
        style.join(",\n")
    )
}

const DARK_SEED: u8 = 0x21;
const LIGHT_SEED: u8 = 0xd4;

/// A Zed theme family file (schema v0.2.0) with a dark and a light theme.
fn fixture_family_json() -> String {
    format!(
        "{{\n  \"$schema\": \"https://zed.dev/schema/themes/v0.2.0.json\",\n  \"name\": \"Fixture\",\n  \"author\": \"Serialist tests\",\n  \"themes\": [\n{},\n{}\n  ]\n}}\n",
        fixture_theme_json("Fixture Dark", "dark", DARK_SEED),
        fixture_theme_json("Fixture Light", "light", LIGHT_SEED),
    )
}

/// The fixture's color for `key` in the theme seeded `seed`.
fn fixture(seed: u8, key: &str) -> Hsla {
    let index = THEME_KEYS
        .iter()
        .position(|k| *k == key)
        .unwrap_or_else(|| panic!("{key} is not a fixture key"));
    let hex = fixture_color(seed, index);
    let rgba = serialist_core::Rgba::parse_hex(&hex).expect("fixture color");
    hsla(rgba)
}

/// A config directory with `settings` (JSONC) and, optionally, the fixture theme.
struct ConfigDir {
    dir: TestDir,
}

impl ConfigDir {
    fn new(name: &str) -> Self {
        Self {
            dir: TestDir::new(name),
        }
    }

    fn with_settings(self, settings: &str) -> Self {
        self.write_settings(settings);
        self
    }

    fn with_themes(self) -> Self {
        let themes = self.dir.join("themes");
        std::fs::create_dir_all(&themes).unwrap();
        std::fs::write(themes.join("fixture.json"), fixture_family_json()).unwrap();
        self
    }

    fn with_keymap(self, keymap: &str) -> Self {
        std::fs::write(self.dir.join("keymap.json"), keymap).unwrap();
        self
    }

    fn write_settings(&self, settings: &str) {
        std::fs::write(self.dir.join("settings.json"), settings).unwrap();
    }

    fn paths(&self) -> ConfigPaths {
        ConfigPaths::new(self.dir.path())
    }
}

/// Load everything under `dir` with the real loaders and install it, as a reload does.
fn load(cx: &mut TestAppContext, dir: &ConfigDir) {
    let paths = dir.paths();
    cx.update(|cx| config::install(Config::load(paths, false), cx));
    cx.run_until_parked();
}

fn config(cx: &mut TestAppContext) -> Config {
    cx.update(|cx| cx.global::<Config>().clone())
}

/// Channels as bytes, for comparing colors that took different routes to `Hsla`.
fn bytes(color: Hsla) -> [u8; 4] {
    let rgba = color.to_rgb();
    [rgba.r, rgba.g, rgba.b, rgba.a].map(|v| (v * 255.0).round() as u8)
}

#[track_caller]
fn assert_color(actual: Hsla, expected: Hsla, what: &str) {
    let (a, e) = (bytes(actual), bytes(expected));
    let close = a.iter().zip(e).all(|(a, e)| a.abs_diff(e) <= 1);
    assert!(close, "{what}: got {a:02x?}, expected {e:02x?}");
}

fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
}

fn open_terminal(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<TerminalView>) {
    let lines = Arc::new(MemoryLines::new());
    for i in 0..50 {
        lines.push(&format!("line {i}"));
    }
    let (window, view) =
        open_test_window(cx, move |window, cx| TerminalView::new(lines, window, cx));
    draw(cx, window);
    (window, view)
}

// --- Fonts -------------------------------------------------------------------------

#[gpui_test]
fn buffer_font_size_sets_the_terminal_grid(cx: &mut TestAppContext) {
    let (window, view) = open_terminal(cx);
    // The test text system's glyphs are 0.6 em wide, its ascent plus descent 1.3 em.
    let metrics = view.read_with(cx, |v, _| v.cell_metrics().expect("a frame"));
    assert_eq!(metrics.cell_width, px(15. * 0.6), "the bundled 15 px");
    assert!(view.read_with(cx, |v, _| v.shaped_lines_cached()) > 0);

    let dir = ConfigDir::new("font-size").with_settings(r#"{ "buffer_font_size": 20 }"#);
    load(cx, &dir);
    assert_eq!(view.read_with(cx, |v, _| v.font().size), px(20.));
    draw(cx, window);
    let metrics = view.read_with(cx, |v, _| v.cell_metrics().expect("a frame"));
    assert_eq!(metrics.cell_width, px(12.));
    assert_eq!(metrics.row_height, px(26.));

    // A terminal override and a line height reach the grid too.
    dir.write_settings(
        r#"{ "buffer_font_size": 20, "buffer_line_height": "comfortable",
             "terminal": { "font_size": 10 } }"#,
    );
    load(cx, &dir);
    draw(cx, window);
    let metrics = view.read_with(cx, |v, _| v.cell_metrics().expect("a frame"));
    assert_eq!(metrics.cell_width, px(6.));
    assert_eq!(metrics.row_height, px((10. * 1.618_f32).ceil()));
}

#[gpui_test]
fn font_features_weight_and_fallbacks_reach_the_font(cx: &mut TestAppContext) {
    let (window, view) = open_terminal(cx);
    let dir = ConfigDir::new("font-features").with_settings(
        r#"{
            "buffer_font_family": "Berkeley Mono",
            "buffer_font_features": { "calt": false, "ss01": true },
            "buffer_font_weight": 500,
            "buffer_font_fallbacks": ["Menlo"],
            "ui_font_size": 18,
            "ui_font_features": { "tnum": true },
        }"#,
    );
    load(cx, &dir);
    let font = view.read_with(cx, |v, _| v.font().font.clone());
    assert_eq!(font.family.as_ref(), "Berkeley Mono");
    assert_eq!(font.features.is_calt_enabled(), Some(false));
    assert_eq!(
        font.features.tag_value_list(),
        [("calt".to_owned(), 0), ("ss01".to_owned(), 1)]
    );
    assert_eq!(font.weight, FontWeight(500.));
    assert_eq!(
        font.fallbacks.as_ref().map(|f| f.fallback_list().to_vec()),
        Some(vec!["Menlo".to_owned()])
    );

    // The UI font goes to gpui-kit (whose size is the window's rem size) and the
    // terminal family to its monospace font.
    let ui = config(cx).ui_font().clone();
    assert_eq!(ui.font.features.tag_value_list(), [("tnum".to_owned(), 1)]);
    cx.update(|cx| {
        let theme = cx.theme();
        assert_eq!(theme.font_size, px(18.));
        assert_eq!(theme.font_family, ui.font.family);
        assert_eq!(theme.mono_font_family.as_ref(), "Berkeley Mono");
    });
    draw(cx, window);
    let rem = cx
        .update_window(window, |_, window, _| window.rem_size())
        .unwrap();
    assert_eq!(
        rem,
        px(18.),
        "gpui-kit's root sets the rem size from the UI font"
    );
}

// --- Themes ------------------------------------------------------------------------

#[gpui_test]
fn a_zed_theme_colors_the_terminal_and_the_chrome(cx: &mut TestAppContext) {
    let (_window, view) = open_terminal(cx);
    let dir = ConfigDir::new("theme")
        .with_themes()
        .with_settings(r#"{ "theme": "Fixture Dark" }"#);
    load(cx, &dir);
    assert!(
        config(cx).problems().is_empty(),
        "{:?}",
        config(cx).problems()
    );

    let seed = DARK_SEED;
    let palette = view.read_with(cx, |v, _| v.palette().clone());
    assert_color(
        palette.ansi[1],
        fixture(seed, "terminal.ansi.red"),
        "ANSI red",
    );
    assert_color(
        palette.ansi[12],
        fixture(seed, "terminal.ansi.bright_blue"),
        "ANSI 12",
    );
    assert_color(
        palette.background,
        fixture(seed, "terminal.background"),
        "terminal bg",
    );
    assert_color(
        palette.foreground,
        fixture(seed, "terminal.foreground"),
        "terminal fg",
    );
    assert_color(
        palette.dim_foreground,
        fixture(seed, "terminal.dim_foreground"),
        "dim",
    );
    assert_color(
        palette.search_match,
        fixture(seed, "search.match_background"),
        "search match",
    );
    assert_color(palette.tx, fixture(seed, "info"), "sent lines");
    let player = config(cx).theme().player(0).copied().expect("player 0");
    assert_color(palette.cursor, hsla(player.cursor.unwrap()), "cursor");

    cx.update(|cx| {
        let theme = cx.theme();
        assert!(theme.is_dark());
        assert_color(
            theme.background,
            fixture(seed, "background"),
            "workspace background",
        );
        assert_color(theme.foreground, fixture(seed, "text"), "text");
        assert_color(
            theme.muted_foreground,
            fixture(seed, "text.muted"),
            "muted text",
        );
        assert_color(
            theme.sidebar,
            fixture(seed, "panel.background"),
            "Devices panel",
        );
        assert_color(
            theme.status_bar,
            fixture(seed, "status_bar.background"),
            "status bar",
        );
        assert_color(
            theme.title_bar,
            fixture(seed, "title_bar.background"),
            "title bar",
        );
        assert_color(
            theme.tab_bar,
            fixture(seed, "tab_bar.background"),
            "tab bar",
        );
        assert_color(
            theme.tab_active,
            fixture(seed, "tab.active_background"),
            "tab",
        );
        assert_color(
            theme.popover,
            fixture(seed, "elevated_surface.background"),
            "popover",
        );
        assert_color(theme.border, fixture(seed, "border.variant"), "border");
        assert_color(theme.input, fixture(seed, "border"), "input border");
        assert_color(theme.ring, fixture(seed, "border.focused"), "focus ring");
        assert_color(theme.button, fixture(seed, "element.background"), "button");
        assert_color(
            theme.button_hover,
            fixture(seed, "element.hover"),
            "button hover",
        );
        assert_color(
            theme.list_hover,
            fixture(seed, "ghost_element.hover"),
            "list hover",
        );
        assert_color(theme.primary, fixture(seed, "text.accent"), "primary");
        assert_color(
            theme.scrollbar_thumb,
            fixture(seed, "scrollbar.thumb.background"),
            "scrollbar thumb",
        );
        assert_color(theme.danger, fixture(seed, "error"), "danger");
        assert_color(theme.success, fixture(seed, "success"), "success");
        assert_color(theme.warning, fixture(seed, "warning"), "warning");
        assert_color(theme.caret, hsla(player.cursor.unwrap()), "caret");
        // No gpui-kit slot: the compose and search bars paint it themselves.
        let toolbar = Config::toolbar_background(cx).expect("a toolbar color");
        assert_color(toolbar, fixture(seed, "toolbar.background"), "toolbar");
    });
}

#[gpui_test]
fn theme_mode_picks_the_light_or_dark_theme_and_follows_the_system(cx: &mut TestAppContext) {
    let source = FakePortSource::new([port("/dev/a")]);
    let opener = Arc::new(FakeOpener::default());
    let (_window, _workspace) = open_test_window(cx, move |window, cx| {
        Workspace::with_opener(source, opener, None, window, cx)
    });
    let background = |cx: &mut TestAppContext| cx.update(|cx| cx.theme().background);
    let is_dark = |cx: &mut TestAppContext| cx.update(|cx| cx.theme().is_dark());

    let dir = ConfigDir::new("theme-mode").with_themes().with_settings(
        r#"{ "theme": { "mode": "light", "light": "Fixture Light", "dark": "Fixture Dark" } }"#,
    );
    load(cx, &dir);
    assert!(!is_dark(cx));
    assert_color(
        background(cx),
        fixture(LIGHT_SEED, "background"),
        "light mode",
    );

    dir.write_settings(
        r#"{ "theme": { "mode": "dark", "light": "Fixture Light", "dark": "Fixture Dark" } }"#,
    );
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert!(is_dark(cx));
    assert_color(
        background(cx),
        fixture(DARK_SEED, "background"),
        "dark mode",
    );
    assert_eq!(config(cx).theme().name, "Fixture Dark");

    // "system" follows the window: the test window is light, then turns dark.
    dir.write_settings(
        r#"{ "theme": { "mode": "system", "light": "Fixture Light", "dark": "Fixture Dark" } }"#,
    );
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert_color(
        background(cx),
        fixture(LIGHT_SEED, "background"),
        "system, light",
    );
    cx.update(|cx| config::set_appearance(WindowAppearance::Dark, cx));
    assert!(is_dark(cx));
    assert_color(
        background(cx),
        fixture(DARK_SEED, "background"),
        "system, dark",
    );
    let palette = config(cx).palette().clone();
    assert_color(
        palette.ansi[1],
        fixture(DARK_SEED, "terminal.ansi.red"),
        "dark palette",
    );
    cx.update(|cx| config::set_appearance(WindowAppearance::VibrantLight, cx));
    assert!(!is_dark(cx));
}

#[gpui_test]
fn a_missing_theme_falls_back_and_says_so(cx: &mut TestAppContext) {
    let (_window, _view) = open_terminal(cx);
    let dir = ConfigDir::new("theme-missing").with_settings(r#"{ "theme": "No Such Theme" }"#);
    load(cx, &dir);
    let config = config(cx);
    let notice = config.notice().expect("a notice");
    assert!(!notice.is_error, "the app still runs: {notice:?}");
    assert!(notice.text.contains("No Such Theme"), "{notice:?}");
    assert!(config.theme().name.starts_with("Serialist"));
}

// --- Failures ----------------------------------------------------------------------

#[gpui_test]
fn a_broken_settings_file_keeps_the_last_good_settings_and_shows_why(cx: &mut TestAppContext) {
    let source = FakePortSource::new([port("/dev/a")]);
    let opener = Arc::new(FakeOpener::default());
    let (_window, workspace) = open_test_window(cx, move |window, cx| {
        Workspace::with_opener(source, opener, None, window, cx)
    });
    let dir = ConfigDir::new("broken").with_settings(r#"{ "buffer_font_size": 19 }"#);
    load(cx, &dir);
    assert_eq!(config(cx).terminal_font().size, px(19.));
    assert_eq!(workspace.read_with(cx, |w, cx| w.config_notice(cx)), None);

    dir.write_settings("{ \"buffer_font_size\": 21,, oops");
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert_eq!(
        config(cx).terminal_font().size,
        px(19.),
        "the last good settings"
    );
    let notice = workspace
        .read_with(cx, |w, cx| w.config_notice(cx))
        .expect("the status line says why");
    assert!(notice.is_error);
    assert!(
        notice.text.starts_with("Settings not applied: ")
            && notice.text.contains("settings.json:1:"),
        "{notice:?}"
    );

    // An unknown key is a warning, not an error.
    dir.write_settings(r#"{ "buffer_font_size": 21, "bufer_font_size": 3 }"#);
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert_eq!(config(cx).terminal_font().size, px(21.));
    let notice = workspace.read_with(cx, |w, cx| w.config_notice(cx));
    assert!(
        notice
            .as_ref()
            .is_some_and(|n| !n.is_error && n.text.contains("bufer_font_size")),
        "{notice:?}"
    );

    dir.write_settings(r#"{ "buffer_font_size": 21 }"#);
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert_eq!(workspace.read_with(cx, |w, cx| w.config_notice(cx)), None);
}

// --- Keymap ------------------------------------------------------------------------

#[gpui_test]
fn a_keymap_file_rebinds_clear_and_reloads_without_duplicates(cx: &mut TestAppContext) {
    allow_engine_threads(cx);
    let source = FakePortSource::new([port("/dev/a")]);
    let opener = Arc::new(FakeOpener::default());
    let for_window = opener.clone();
    let (window, workspace) = open_test_window(cx, move |window, cx| {
        Workspace::with_opener(source, for_window, None, window, cx)
    });
    cx.run_until_parked();
    let binding_count =
        |cx: &mut TestAppContext| cx.update(|cx| cx.key_bindings().borrow().bindings().len());
    let bundled_count = binding_count(cx);

    let (old, new) = if cfg!(target_os = "macos") {
        ("cmd-k", "cmd-shift-l")
    } else {
        ("ctrl-shift-k", "ctrl-alt-l")
    };
    let keymap = format!(
        r#"// Clear moves to another chord.
        [ {{ "context": "Workspace",
             "bindings": {{ "{old}": null, "{new}": "terminal::Clear" }} }} ]"#
    );
    let dir = ConfigDir::new("keymap").with_keymap(&keymap);
    load(cx, &dir);
    assert!(
        config(cx).problems().is_empty(),
        "{:?}",
        config(cx).problems()
    );
    assert_eq!(binding_count(cx), bundled_count + 2);

    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    devices.update(cx, |devices, cx| assert!(devices.connect_selected(cx)));
    cx.run_until_parked();
    let feed = opener.opened()[0].2.clone();
    feed.connected("dev");
    feed.data(b"one\ntwo\n");
    let session = workspace.read_with(cx, |w, _| w.session().unwrap().clone());
    run_until(cx, "two lines", |cx| displayed(cx, &session).len() == 3);

    let press = |cx: &mut TestAppContext, keys: &str| {
        cx.update_window(window, |_, window, cx| window.press(keys, cx))
            .unwrap();
    };
    press(cx, old);
    assert_eq!(displayed(cx, &session).len(), 3, "{old} is unbound");
    press(cx, new);
    assert!(displayed(cx, &session).is_empty(), "{new} clears");

    // Reloading the same file, or all of the config, binds nothing twice.
    cx.update(|cx| config::reload(ConfigPiece::Keymap, cx));
    cx.dispatch_action(window, ReloadConfig);
    assert_eq!(binding_count(cx), bundled_count + 2);
    feed.data(b"three\n");
    run_until(cx, "the third line", |cx| {
        displayed(cx, &session).len() == 1
    });
    press(cx, new);
    assert!(
        displayed(cx, &session).is_empty(),
        "still bound once reloaded"
    );
}

#[gpui_test]
fn keymap_entries_that_cannot_bind_are_reported_and_the_rest_bind(cx: &mut TestAppContext) {
    let (_window, _view) = open_terminal(cx);
    let dir = ConfigDir::new("keymap-bad").with_keymap(
        r#"[ { "context": "Terminal",
               "bindings": { "alt-q": "terminal::NoSuchAction", "alt-w": "terminal::ToggleWrap" } } ]"#,
    );
    load(cx, &dir);
    let problems = config(cx).problems().to_vec();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].piece, ConfigPiece::Bindings);
    assert!(problems[0].message.contains("terminal::NoSuchAction"));
    let bound = cx.update(|cx| {
        cx.key_bindings().borrow().bindings().any(|b| {
            b.action().name() == "terminal::ToggleWrap"
                && b.keystrokes().len() == 1
                && b.keystrokes()[0].inner().unparse() == "alt-w"
        })
    });
    assert!(bound);

    // A file that does not parse keeps the bindings in use.
    std::fs::write(dir.dir.join("keymap.json"), "[ { \"bindings\": ").unwrap();
    cx.update(|cx| config::reload(ConfigPiece::Keymap, cx));
    let notice = config(cx).notice().expect("a notice");
    assert!(
        notice.is_error && notice.text.starts_with("Keymap not applied"),
        "{notice:?}"
    );
    let still = cx.update(|cx| {
        cx.key_bindings()
            .borrow()
            .bindings()
            .any(|b| b.action().name() == "terminal::ToggleWrap")
    });
    assert!(still);
}

// --- Settings in sessions ----------------------------------------------------------

const EARBUDS: &str = "/dev/cu.usbmodem-EARBUDS";

fn earbuds() -> PortInfo {
    PortInfo {
        id: PortId::new(EARBUDS),
        kind: PortKind::Usb(UsbInfo {
            vid: 0x0e8d,
            pid: 0x2000,
            serial_number: Some("L1".into()),
            manufacturer: Some("Airoha".into()),
            product: Some("Airoha BT UART".into()),
        }),
        display_name: "Airoha BT UART".into(),
    }
}

#[gpui_test]
fn a_device_profile_sets_the_baud_line_ending_and_name(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    world.add_device(earbuds(), LinkConfig::unpaced(), || {
        Box::new(EchoDevice::new())
    });
    let (window, workspace) = open_workspace(cx, &world, None);
    let dir = ConfigDir::new("profile").with_settings(
        r#"{
            "default_baud": 57600,
            "devices": [
                { "name": "Left earbud", "match": { "vid": "0x0e8d", "product": "airoha" },
                  "baud": 921600, "parity": "even", "eol": "cr" }
            ]
        }"#,
    );
    load(cx, &dir);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let id = PortId::new(EARBUDS);
    run_until(cx, "the earbuds to be listed", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&id).is_some_and(|e| e.present))
    });
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| d.select_port(id.clone(), window, cx));
    })
    .unwrap();
    devices.read_with(cx, |d, cx| {
        let info = &d.list().get(&id).unwrap().info;
        assert_eq!(d.display_name(info, cx), "Left earbud");
        assert!(d.has_profile(info, cx));
        assert_eq!(
            d.baud_text(cx),
            "921600",
            "the field shows the profile's rate"
        );
    });

    devices.update(cx, |d, cx| assert!(d.connect_selected(cx)));
    let view = wait_connected(cx, &workspace);
    view.read_with(cx, |v, cx| {
        assert_eq!(v.serial().baud, 921_600);
        assert_eq!(v.serial().summary(), "921600 8E1");
        assert_eq!(
            v.title(),
            format!("{EARBUDS} @ 921600 8E1"),
            "the link's own account"
        );
        assert_eq!(
            v.compose().read(cx).line_ending(),
            serialist_core::LineEnding::Cr
        );
    });
}

#[gpui_test]
fn display_settings_start_each_session_and_reloads_change_only_what_changed(
    cx: &mut TestAppContext,
) {
    let world = SimWorld::empty();
    world.add_device(earbuds(), LinkConfig::unpaced(), || {
        Box::new(EchoDevice::new())
    });
    let (_window, workspace) = open_workspace(cx, &world, None);
    let dir = ConfigDir::new("display").with_settings(
        r#"{ "local_echo": true, "line_ending": "lf",
             "display": { "wrap": true, "timestamps": "delta", "view": "hex_ascii",
                          "hex_bytes_per_row": 8 } }"#,
    );
    load(cx, &dir);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let id = PortId::new(EARBUDS);
    run_until(cx, "the port", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&id).is_some())
    });
    workspace.update(cx, |w, cx| {
        let serial = w.serial_for(&id, cx);
        assert_eq!(serial.baud, 115_200, "no profile: default_baud");
    });
    devices.update(cx, |d, cx| assert!(d.connect_selected(cx)));
    let view = wait_connected(cx, &workspace);
    let terminal = view.read_with(cx, |v, _| v.terminal().clone());
    terminal.read_with(cx, |t, _| {
        assert!(t.wrap());
        assert_eq!(t.timestamps(), serialist_core::TimestampMode::Delta);
        assert_eq!(t.display_mode(), crate::terminal::DisplayMode::Hex);
    });
    view.read_with(cx, |v, cx| {
        assert_eq!(v.hex_bytes_per_row(), 8);
        assert!(v.compose().read(cx).local_echo());
        assert_eq!(
            v.compose().read(cx).line_ending(),
            serialist_core::LineEnding::Lf
        );
    });

    // The user turns wrap off; a reload that changes only the row width keeps that.
    terminal.update(cx, |t, cx| t.set_wrap(false, cx));
    dir.write_settings(
        r#"{ "local_echo": true, "line_ending": "lf",
             "display": { "wrap": true, "timestamps": "delta", "view": "hex_ascii",
                          "hex_bytes_per_row": 32 } }"#,
    );
    cx.update(|cx| config::reload(ConfigPiece::Settings, cx));
    assert_eq!(view.read_with(cx, |v, _| v.hex_bytes_per_row()), 32);
    assert!(
        !terminal.read_with(cx, |t, _| t.wrap()),
        "the user's toggle stands"
    );
}

// --- Actions -----------------------------------------------------------------------

#[gpui_test]
fn open_settings_writes_the_template_and_opens_it(cx: &mut TestAppContext) {
    let (window, _view) = open_terminal(cx);
    let dir = ConfigDir::new("open-settings");
    load(cx, &dir);
    let opened: Arc<Mutex<Vec<std::path::PathBuf>>> = Arc::default();
    let record = opened.clone();
    cx.update(|cx| {
        cx.set_global(Opener(Arc::new(move |path: &std::path::Path| {
            record.lock().push(path.to_path_buf());
            Ok(())
        })));
    });

    cx.dispatch_action(window, OpenSettings);
    let settings = dir.dir.join("settings.json");
    assert_eq!(opened.lock().as_slice(), [settings.clone()]);
    let template = std::fs::read_to_string(&settings).unwrap();
    assert!(
        template.contains("// \"buffer_font_size\""),
        "commented template"
    );
    // The template is valid and changes nothing.
    load(cx, &dir);
    assert!(
        config(cx).problems().is_empty(),
        "{:?}",
        config(cx).problems()
    );

    cx.dispatch_action(window, crate::actions::OpenKeymap);
    cx.dispatch_action(window, crate::actions::OpenThemesFolder);
    let opened = opened.lock().clone();
    assert_eq!(opened[1], dir.dir.join("keymap.json"));
    assert_eq!(opened[2], dir.dir.join("themes"));
    assert!(dir.dir.join("themes").is_dir());
    // The keymap template binds nothing and loads cleanly.
    load(cx, &dir);
    assert!(
        config(cx).problems().is_empty(),
        "{:?}",
        config(cx).problems()
    );
}

// --- Hot reload --------------------------------------------------------------------

#[gpui_test]
fn saving_settings_changes_the_running_terminal_within_two_seconds(cx: &mut TestAppContext) {
    allow_engine_threads(cx);
    let (window, view) = open_terminal(cx);
    let dir = ConfigDir::new("watch").with_settings(r#"{ "buffer_font_size": 15 }"#);
    let paths = dir.paths();
    cx.update(|cx| config::start(paths, cx));
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |v, _| v.font().size), px(15.));

    dir.write_settings(r#"{ "buffer_font_size": 20 }"#);
    let started = Instant::now();
    loop {
        cx.run_until_parked();
        if view.read_with(cx, |v, _| v.font().size) == px(20.) {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the font did not change within 2 s"
        );
        cx.executor().advance_clock(FRAME);
    }
    draw(cx, window);
    let metrics = view.read_with(cx, |v, _| v.cell_metrics().expect("a frame"));
    assert_eq!(metrics.cell_width, px(12.));
    assert_eq!(
        config(cx).notice(),
        None::<Notice>,
        "a clean reload leaves no notice"
    );
}
