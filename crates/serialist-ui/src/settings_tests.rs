//! The Settings screen, headless, over a temporary config directory: the real loaders,
//! the real watcher and the core's comment-preserving editors, driven through the
//! screen's controls.

use std::path::Path;

use serde_json::{Value, json};
use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, SettingsEditor, ThemeMode};
use serialist_sim::{EchoDevice, LinkConfig, SimWorld};

use crate::actions::keys;
use crate::config::{self, Config};
use crate::prelude::*;
use crate::session_view::FRAME;
use crate::settings_io::Origin;
use crate::settings_view::{DEBOUNCE, Field, MatchKey, ProfileEditor, Section, SettingsView};
use crate::test_support::{TestDir, draw, open_workspace, run_until, usb_port, wait_connected};
use crate::workspace::Workspace;

/// The chord the rebind test moves `tabs::NewTab` to.
#[cfg(target_os = "macos")]
const NEW_TAB_ELSEWHERE: &str = "cmd-alt-t";
#[cfg(not(target_os = "macos"))]
const NEW_TAB_ELSEWHERE: &str = "ctrl-alt-t";

/// A config directory whose `settings.json` is the commented template, as the app
/// writes it the first time settings are opened.
fn template_dir(name: &str) -> (TestDir, ConfigPaths) {
    let dir = TestDir::new(name);
    let paths = ConfigPaths::new(dir.path());
    paths.ensure_settings_file().expect("write the template");
    (dir, paths)
}

/// Load `paths` with the real loaders and watch it, as the app does at startup.
fn start(cx: &mut TestAppContext, paths: &ConfigPaths) {
    let paths = paths.clone();
    cx.update(|cx| config::start(paths, cx));
    cx.run_until_parked();
}

/// The value the file holds at `pointer` now.
fn file_value(path: &Path, pointer: &str) -> Option<Value> {
    SettingsEditor::open(path)
        .expect("the file parses")
        .get(pointer)
}

/// Press `keys::OPEN_SETTINGS_UI` and return the Settings screen it opened.
fn open_settings(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Entity<SettingsView> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press(keys::OPEN_SETTINGS_UI, cx);
    })
    .unwrap();
    cx.run_until_parked();
    workspace
        .read_with(cx, |w, _| w.settings_view().cloned())
        .expect("the settings tab")
}

fn show(cx: &mut TestAppContext, window: AnyWindowHandle, section: Section) {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(section.nav_id(), cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

/// Type `text` into a field as a user does: focus it, clear it, type, and let the
/// debounce run out.
fn type_into(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SettingsView>,
    field: Field,
    text: &str,
) {
    let input = view.read_with(cx, |v, _| v.input(field).clone());
    cx.update_window(window, |_, window, cx| {
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.set_value("", window, cx);
        });
        window.input(text, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.executor().advance_clock(DEBOUNCE);
    cx.run_until_parked();
}

fn activate_tab(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
    index: usize,
) {
    cx.update_window(window, |_, window, cx| {
        workspace.update(cx, |w, cx| w.activate_index(index, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

fn update_view(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<SettingsView>,
    f: impl FnOnce(&mut SettingsView, &mut Window, &mut Context<SettingsView>),
) {
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| f(view, window, cx));
    })
    .unwrap();
    cx.run_until_parked();
}

#[gpui_test]
fn the_key_opens_one_settings_tab_and_a_font_size_reaches_the_file_and_the_terminal(
    cx: &mut TestAppContext,
) {
    let world = SimWorld::new();
    let (window, workspace) = open_workspace(cx, &world, Some("virtual:echo"));
    let (_dir, paths) = template_dir("settings-font-size");
    start(cx, &paths);
    let session = wait_connected(cx, &workspace);

    let view = open_settings(cx, window, &workspace);
    workspace.read_with(cx, |w, cx| {
        assert_eq!(w.tab_count(), 2, "the session and Settings");
        assert!(w.shows_tab_bar());
        let labels = w.tab_labels(cx);
        assert_eq!(labels[1].title, "Settings");
        assert!(labels[1].active);
        assert_eq!(w.window_title(), "Settings \u{2014} Serialist");
    });
    // Only one at a time: the key goes back to it.
    open_settings(cx, window, &workspace);
    assert_eq!(workspace.read_with(cx, |w, _| w.tab_count()), 2);

    show(cx, window, Section::TerminalFont);
    assert_eq!(
        view.read_with(cx, |v, _| v.section()),
        Section::TerminalFont
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.origin("/buffer_font_size")),
        Origin::Default
    );
    type_into(cx, window, &view, Field::BufferFontSize, "18");

    let text = std::fs::read_to_string(&paths.settings).unwrap();
    assert!(text.contains("\"buffer_font_size\": 18"), "{text}");
    assert!(
        text.starts_with("// Serialist settings. Everything below is commented out"),
        "the template's header stays"
    );
    assert!(text.contains("  // Size in points.\n  \"buffer_font_size\": 18"));
    assert!(
        text.contains("// \"buffer_font_weight\": 400,"),
        "others stay commented"
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.origin("/buffer_font_size")),
        Origin::User
    );

    // The watcher reloads the file and the terminal takes the new size.
    let terminal = session.read_with(cx, |s, _| s.terminal().clone());
    run_until(cx, "the terminal to take 18 pt", |cx| {
        terminal.read_with(cx, |t, _| t.font().size) == px(18.)
    });
    activate_tab(cx, window, &workspace, 0);
    draw(cx, window);
    let metrics = terminal.read_with(cx, |t, _| t.cell_metrics().expect("a frame"));
    assert_eq!(metrics.cell_width, px(18. * 0.6));

    // cmd-w closes the Settings tab like any other.
    activate_tab(cx, window, &workspace, 1);
    cx.update_window(window, |_, window, cx| window.press(keys::CLOSE_TAB, cx))
        .unwrap();
    cx.run_until_parked();
    workspace.read_with(cx, |w, _| {
        assert_eq!(w.tab_count(), 1);
        assert!(w.settings_view().is_none());
    });
}

#[gpui_test]
fn ligatures_theme_and_default_write_and_remove_their_keys(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-keys");
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);

    show(cx, window, Section::TerminalFont);
    cx.update_window(window, |_, window, cx| {
        window.click("settings-ligatures", cx)
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        file_value(&paths.settings, "/buffer_font_features/calt"),
        Some(json!(false))
    );
    run_until(cx, "ligatures off in the terminal font", |cx| {
        cx.update(|cx| {
            !cx.global::<Config>()
                .settings()
                .buffer_font_features
                .ligatures_enabled()
        })
    });

    show(cx, window, Section::Appearance);
    update_view(cx, window, &view, |v, window, cx| {
        v.set_theme_mode(ThemeMode::Dark, window, cx);
        v.set_theme_name(false, "Serialist Dark", window, cx);
    });
    assert_eq!(
        file_value(&paths.settings, "/theme"),
        Some(json!({ "mode": "dark", "light": "Serialist Dark", "dark": "Serialist Dark" }))
    );
    let text = std::fs::read_to_string(&paths.settings).unwrap();
    assert!(
        text.contains("// A theme name, or { \"mode\""),
        "the comment above the theme stays"
    );

    // "Default" (the row's reset button) removes the key.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("reset/theme", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(file_value(&paths.settings, "/theme"), None);
    assert_eq!(
        view.read_with(cx, |v, _| v.origin("/theme")),
        Origin::Default
    );
    update_view(cx, window, &view, |v, window, cx| {
        v.reset("/buffer_font_features/calt", window, cx);
    });
    assert_eq!(file_value(&paths.settings, "/buffer_font_features"), None);
}

#[gpui_test]
fn a_bad_value_is_refused_beside_its_control_and_the_file_is_untouched(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-refused");
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);
    let before = std::fs::read_to_string(&paths.settings).unwrap();

    show(cx, window, Section::Display);
    type_into(cx, window, &view, Field::HexBytesPerRow, "0");
    type_into(cx, window, &view, Field::TimestampFormat, "%Q");
    view.read_with(cx, |v, _| {
        assert!(
            v.error("/display/hex_bytes_per_row")
                .is_some_and(|e| e.contains("1 to 256"))
        );
        assert!(
            v.error("/display/timestamp_format")
                .is_some_and(|e| e.contains("strftime"))
        );
    });
    assert_eq!(std::fs::read_to_string(&paths.settings).unwrap(), before);

    // The escape chord is recorded by pressing it, and checked as the loader checks it.
    show(cx, window, Section::Session);
    cx.update_window(window, |_, window, cx| {
        window.click("settings-escape-chord", cx);
        window.press("ctrl-alt-x", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(
        file_value(&paths.settings, "/inline/escape_chord"),
        Some(json!("ctrl-alt-x"))
    );
    update_view(cx, window, &view, |v, window, cx| {
        v.set_escape_chord("x", window, cx);
    });
    assert!(
        view.read_with(cx, |v, _| v
            .error("/inline/escape_chord")
            .map(str::to_owned))
            .is_some_and(|e| e.contains("types a character"))
    );
    assert_eq!(
        file_value(&paths.settings, "/inline/escape_chord"),
        Some(json!("ctrl-alt-x"))
    );
}

#[gpui_test]
fn a_profile_added_in_the_form_is_written_and_used_on_the_next_connect(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let board = usb_port(
        "/dev/cu.usbserial-BENCH",
        "CH340 bench board",
        0x1a86,
        0x7523,
    );
    world.add_device(board.clone(), LinkConfig::unpaced(), || {
        Box::new(EchoDevice::new())
    });
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-profile");
    start(cx, &paths);
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let id = PortId::new("/dev/cu.usbserial-BENCH");
    run_until(cx, "the board to be listed", |cx| {
        devices.read_with(cx, |d, _| d.list().get(&id).is_some_and(|e| e.present))
    });

    let view = open_settings(cx, window, &workspace);
    show(cx, window, Section::Devices);
    update_view(cx, window, &view, |v, window, cx| {
        v.open_profile_editor(None, window, cx);
    });
    let editor: Entity<ProfileEditor> = view
        .read_with(cx, |v, _| v.profile_editor().cloned())
        .expect("the profile form");
    cx.update_window(window, |_, window, cx| {
        editor.update(cx, |editor, cx| {
            // The listed port fills the match keys and the name.
            editor.fill_from_port("CH340 bench board (/dev/cu.usbserial-BENCH)", window, cx);
            let form = editor.port_form().clone();
            form.update(cx, |form, cx| form.enter_baud("921600", window, cx));
            let product = editor.match_input(MatchKey::Product).clone();
            ProfileEditor::set_text(&product, "CH340", window, cx);
        });
    })
    .unwrap();
    cx.run_until_parked();
    let saved = cx
        .update_window(window, |_, window, cx| {
            view.update(cx, |v, cx| v.save_profile(window, cx))
        })
        .unwrap();
    assert!(
        saved,
        "{:?}",
        editor.read_with(cx, |e, _| e.error().map(str::to_owned))
    );
    assert_eq!(
        file_value(&paths.settings, "/devices/0"),
        Some(json!({
            "name": "CH340 bench board",
            "match": { "vid": "0x1a86", "pid": "0x7523", "product": "CH340" },
            "baud": 921600
        }))
    );

    // Once the watcher has it, connecting the board uses the profile.
    run_until(cx, "the profile to load", |cx| {
        cx.update(|cx| cx.global::<Config>().settings().devices.len() == 1)
    });
    cx.update_window(window, |_, window, cx| {
        devices.update(cx, |d, cx| d.select_port(id.clone(), window, cx));
    })
    .unwrap();
    devices.update(cx, |d, cx| assert!(d.connect_selected(cx)));
    let session = wait_connected(cx, &workspace);
    assert_eq!(session.read_with(cx, |s, _| s.serial().baud), 921_600);

    // Removing it writes the list without it.
    let view = workspace
        .read_with(cx, |w, _| w.settings_view().cloned())
        .unwrap();
    update_view(cx, window, &view, |v, window, cx| {
        v.remove_profile(0, window, cx)
    });
    assert_eq!(file_value(&paths.settings, "/devices"), None);
}

#[gpui_test]
fn rebinding_a_key_writes_the_user_keymap_and_the_new_chord_works(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-rebind");
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);
    show(cx, window, Section::Keymap);
    let rows = view.read_with(cx, |v, cx| v.binding_rows_now(cx));
    assert!(rows.iter().any(|row| row.keystrokes == keys::NEW_TAB
        && row.action.name == "tabs::NewTab"
        && row.context.as_deref() == Some("Workspace")));

    update_view(cx, window, &view, |v, window, cx| {
        v.select_binding(Some("Workspace"), keys::NEW_TAB, cx);
        v.start_rebind(window, cx);
    });
    assert!(view.read_with(cx, |v, cx| v.rebind_recorder().read(cx).is_recording()));
    let tabs_before = workspace.read_with(cx, |w, _| w.tab_count());
    cx.update_window(window, |_, window, cx| window.press(NEW_TAB_ELSEWHERE, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |v, _| v.keymap_error().map(str::to_owned)),
        None
    );
    assert_eq!(
        workspace.read_with(cx, |w, _| w.tab_count()),
        tabs_before,
        "the recorder took the chord; nothing ran"
    );
    let keymap = std::fs::read_to_string(&paths.keymap).unwrap();
    assert!(
        keymap.contains(&format!("\"{NEW_TAB_ELSEWHERE}\": \"tabs::NewTab\"")),
        "{keymap}"
    );
    assert!(
        keymap.contains(&format!("\"{}\": null", keys::NEW_TAB)),
        "{keymap}"
    );
    assert!(
        keymap.starts_with("// Serialist key bindings"),
        "the template stays"
    );

    run_until(cx, "the keymap to reload", |cx| {
        cx.update(|cx| {
            cx.global::<Config>()
                .keymap()
                .resolved()
                .iter()
                .any(|entry| entry.keystrokes == NEW_TAB_ELSEWHERE)
        })
    });
    let rows = view.read_with(cx, |v, cx| v.binding_rows_now(cx));
    assert!(rows.iter().any(|row| row.keystrokes == NEW_TAB_ELSEWHERE
        && row.source == crate::settings_view::BindingSource::User));
    cx.update_window(window, |_, window, cx| window.press(NEW_TAB_ELSEWHERE, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(cx, |w, _| w.tab_count()),
        tabs_before + 1
    );
    cx.update_window(window, |_, window, cx| window.press(keys::NEW_TAB, cx))
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        workspace.read_with(cx, |w, _| w.tab_count()),
        tabs_before + 1,
        "the old chord is unbound"
    );
}

#[gpui_test]
fn a_broken_file_shows_the_error_until_it_loads_again(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-broken");
    std::fs::write(&paths.settings, "{ \"buffer_font_size\": 18,, }").unwrap();
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);
    let broken = view.read_with(cx, |v, _| v.broken().map(str::to_owned));
    assert!(
        broken
            .as_deref()
            .is_some_and(|message| message.contains("settings.json:1:")),
        "{broken:?}"
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("settings-broken-open").is_some());
        assert!(window.try_find("settings-nav").is_none(), "no form");
    })
    .unwrap();

    std::fs::write(&paths.settings, "{ \"buffer_font_size\": 18 }").unwrap();
    run_until(cx, "the form to come back", |cx| {
        view.read_with(cx, |v, _| v.broken().is_none())
    });
    assert_eq!(
        view.read_with(cx, |v, cx| v.field_text(Field::BufferFontSize, cx)),
        "18"
    );
}

#[gpui_test]
fn an_edit_made_elsewhere_shows_in_the_open_screen(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (_dir, paths) = template_dir("settings-external");
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);
    show(cx, window, Section::TerminalFont);
    assert_eq!(
        view.read_with(cx, |v, cx| v.field_text(Field::BufferFontSize, cx)),
        "15"
    );

    SettingsEditor::set_in_file(&paths.settings, "/buffer_font_size", 20).unwrap();
    SettingsEditor::set_in_file(&paths.settings, "/display/wrap", true).unwrap();
    run_until(cx, "the field to show 20", |cx| {
        view.read_with(cx, |v, cx| v.field_text(Field::BufferFontSize, cx)) == "20"
    });
    view.read_with(cx, |v, _| {
        assert!(v.settings().display.wrap);
        assert_eq!(v.origin("/display/wrap"), Origin::User);
    });
}

#[gpui_test]
fn a_key_a_project_file_sets_is_read_only_here(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    let (dir, mut paths) = template_dir("settings-project");
    let project = dir.join("project/.serialist/settings.json");
    std::fs::create_dir_all(project.parent().unwrap()).unwrap();
    std::fs::write(&project, "{ \"display\": { \"wrap\": true } }").unwrap();
    paths.project_settings = Some(project.clone());
    start(cx, &paths);
    let view = open_settings(cx, window, &workspace);
    assert_eq!(
        view.read_with(cx, |v, _| v.origin("/display/wrap")),
        Origin::Project(project.clone())
    );
    update_view(cx, window, &view, |v, window, cx| {
        v.set_switch("/display/wrap", false, window, cx);
    });
    assert_eq!(file_value(&paths.settings, "/display/wrap"), None);
    assert!(view.read_with(cx, |v, _| v.settings().display.wrap));
    // Other keys are still the user's to set.
    assert_eq!(
        view.read_with(cx, |v, _| v.origin("/display/timestamps")),
        Origin::Default
    );
    cx.executor().advance_clock(FRAME);
}

#[gpui_test]
fn without_a_config_directory_nothing_is_read_or_written(cx: &mut TestAppContext) {
    let world = SimWorld::empty();
    let (window, workspace) = open_workspace(cx, &world, None);
    // The bundled configuration only: no `config::start`.
    let view = open_settings(cx, window, &workspace);
    view.read_with(cx, |v, _| {
        assert!(v.broken().is_none());
        assert_eq!(v.origin("/display/wrap"), Origin::Default);
    });
    update_view(cx, window, &view, |v, window, cx| {
        v.set_switch("/display/wrap", true, window, cx);
    });
    view.read_with(cx, |v, _| {
        assert!(
            v.error("/display/wrap")
                .is_some_and(|e| e.contains("not read from a directory"))
        );
        assert!(!v.settings().display.wrap);
    });
}
