//! Decoders are plugins the user installs; the app ships with none active. Against the
//! real engine and the real config loaders and watcher: a fresh configuration has no
//! codec, no codec menu and no Decoded rail icon; the command palette installs the
//! bundled example and the watcher brings it in; a device profile naming a plugin that
//! is not installed connects without a codec and offers to install it; and a saved
//! command that needs a plugin that is not installed is not sent and says why.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{CommandRef, Direction, PortId};
use serialist_sim::SimWorld;

use crate::actions::keys;
use crate::actions::plugins::OpenPluginsFolder;
use crate::config::{self, Config, Opener};
use crate::docks::DockPanel;
use crate::plugin_files;
use crate::prelude::*;
use crate::session_view::SessionView;
use crate::status::NoticeAction;
use crate::test_support::{
    TestDir, allow_engine_threads, displayed, draw, open_test_window, run_until, wait_connected,
};
use crate::toolbar::ToolbarItem;
use crate::workspace::{AppOptions, Workspace};

/// The label of the palette's entry that installs the RACE example.
const INSTALL_RACE: &str = "Install example plugin: Airoha RACE";

/// What [`open`] opens.
struct Opened {
    window: AnyWindowHandle,
    workspace: Entity<Workspace>,
    view: Entity<SessionView>,
    _world: SimWorld,
}

/// A config directory with `settings` as its settings file and no plugins.
fn config_dir(name: &str, settings: &str) -> TestDir {
    let dir = TestDir::new(name);
    fs::write(dir.join("settings.json"), settings).unwrap();
    dir
}

/// A device profile for the simulated RACE device that names the `airoha-race` plugin.
const RACE_PROFILE: &str = r#"{ "devices": [ { "name": "RACE board",
    "match": { "path": "virtual:race" }, "plugin": "airoha-race" } ] }"#;

/// A workspace over the simulator with the configuration under `dir` loaded and watched,
/// as the app starts, opening `virtual:race`.
fn open(cx: &mut TestAppContext, dir: &TestDir) -> Opened {
    allow_engine_threads(cx);
    let world = SimWorld::new();
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: None,
        open_ports: vec![PortId::new("virtual:race")],
        store: None,
    };
    let paths = ConfigPaths::new(dir.path());
    let (window, workspace) = open_test_window(cx, move |window, cx| {
        config::start(paths, cx);
        Workspace::new(options, window, cx)
    });
    let view = wait_connected(cx, &workspace);
    Opened {
        window,
        workspace,
        view,
        _world: world,
    }
}

fn has_codec(cx: &mut TestAppContext, name: &str) -> bool {
    cx.update(|cx| cx.global::<Config>().codec_registry().contains(name))
}

/// Whether the drawn window has an element with this id.
fn drawn(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> bool {
    draw(cx, window);
    cx.update_window(window, |_, window, _| window.try_find(id).is_some())
        .unwrap()
}

/// The palette's labels, opened with its key and closed again.
fn palette_labels(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    workspace: &Entity<Workspace>,
) -> Vec<String> {
    press(cx, window, keys::COMMAND_PALETTE);
    let palette = workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .expect("the palette is open");
    let labels = palette.read_with(cx, |p, _| p.labels());
    press(cx, window, "escape");
    labels
}

fn press(cx: &mut TestAppContext, window: AnyWindowHandle, keys: &str) {
    cx.update_window(window, |_, window, cx| window.press(keys, cx))
        .unwrap();
    cx.run_until_parked();
}

fn notice(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Option<crate::status::Notice> {
    view.read_with(cx, |v, _| v.notice().cloned())
}

#[gpui_test]
fn a_fresh_configuration_has_no_codec_and_shows_nothing_about_codecs(cx: &mut TestAppContext) {
    let dir = config_dir("plugins-fresh", "{}");
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir);
    // Nothing is built in and nothing is installed.
    let (empty, choices) = cx.update(|cx| {
        let codecs = cx.global::<Config>().codecs();
        (codecs.is_empty(), codecs.choices())
    });
    assert!(empty);
    assert_eq!(choices, ["none"]);
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        None
    );

    // No codec menu in the toolbar or its overflow, and no Decoded rail icon.
    draw(cx, window);
    let layout = view.read_with(cx, |v, _| v.toolbar_layout().clone());
    assert!(!layout.shows(ToolbarItem::Codec), "{layout:?}");
    assert!(!layout.overflow.contains(&ToolbarItem::Codec), "{layout:?}");
    assert!(!drawn(cx, window, "codec-picker"));
    assert!(!drawn(cx, window, DockPanel::Decoded.rail_id()));
    assert!(drawn(cx, window, DockPanel::Scripts.rail_id()));
    assert!(!workspace.read_with(cx, |w, cx| w.has_decoded_rail(cx)));

    // The palette offers the example and the plugins folder.
    let labels = palette_labels(cx, window, &workspace);
    assert!(labels.contains(&INSTALL_RACE.to_owned()), "{labels:?}");
    assert!(
        labels.contains(&"Plugins: Open plugins folder".to_owned()),
        "{labels:?}"
    );

    // Opening the plugins folder puts the examples beside the plugins, not into them.
    let opened: Arc<Mutex<Vec<std::path::PathBuf>>> = Arc::default();
    let record = opened.clone();
    cx.update(|cx| {
        cx.set_global(Opener(Arc::new(move |path: &Path| {
            record.lock().push(path.to_path_buf());
            Ok(())
        })));
    });
    cx.dispatch_action(window, OpenPluginsFolder);
    let paths = ConfigPaths::new(dir.path());
    assert_eq!(opened.lock().as_slice(), [paths.plugins_dir()]);
    assert!(
        paths
            .example_plugins_dir()
            .join("airoha-race/plugin.lua")
            .is_file()
    );
    cx.update(|cx| config::reload(config::ConfigPiece::Plugins, cx));
    assert!(cx.update(|cx| cx.global::<Config>().codecs().is_empty()));
}

#[gpui_test]
fn installing_the_example_from_the_palette_brings_its_codec_in_through_the_watcher(
    cx: &mut TestAppContext,
) {
    let dir = config_dir("plugins-install", "{}");
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir);
    assert!(!has_codec(cx, "airoha-race"));

    // Typed into the palette, as a user would, then Enter.
    press(cx, window, keys::COMMAND_PALETTE);
    cx.update_window(window, |_, window, cx| {
        window.input("install example race", cx)
    })
    .unwrap();
    cx.run_until_parked();
    let palette = workspace
        .read_with(cx, |w, _| w.palette().cloned())
        .expect("the palette is open");
    let selected = palette.read_with(cx, |p, _| p.selected().map(|e| e.label.clone()));
    assert_eq!(selected.as_deref(), Some(INSTALL_RACE));
    press(cx, window, "enter");

    // The folder is written at once; the codec arrives when the watcher reloads.
    let folder = ConfigPaths::new(dir.path())
        .plugins_dir()
        .join("airoha-race");
    run_until(cx, "the example's folder", |_| {
        folder.join("plugin.lua").is_file()
    });
    run_until(cx, "the watcher to load the plugin", |cx| {
        has_codec(cx, "airoha-race")
    });
    let text = notice(cx, &view)
        .map(|notice| notice.text)
        .unwrap_or_default();
    assert!(
        text.starts_with("Installed the Airoha RACE example plugin in "),
        "{text}"
    );

    // Now the toolbar has a codec menu, the right rail a Decoded icon, and the palette
    // no longer offers what is installed.
    run_until(cx, "the codec menu", |cx| drawn(cx, window, "codec-picker"));
    assert!(drawn(cx, window, DockPanel::Decoded.rail_id()));
    let labels = palette_labels(cx, window, &workspace);
    assert!(!labels.contains(&INSTALL_RACE.to_owned()), "{labels:?}");

    // Picked from the menu's list, it decodes.
    assert!(view.update(cx, |v, cx| v.set_codec(Some("airoha-race"), cx)));
    run_until(cx, "a decoded log frame", |cx| {
        view.read_with(cx, |v, _| {
            v.frame_reader()
                .snapshot()
                .frames()
                .any(|(_, frame)| frame.kind == "log")
        })
    });
}

#[gpui_test]
fn a_plugin_of_ones_own_brings_the_codec_menu_which_offers_the_examples(cx: &mut TestAppContext) {
    let dir = config_dir("plugins-own", "{}");
    let mine = dir.join("plugins").join("my-race");
    fs::create_dir_all(&mine).unwrap();
    fs::write(mine.join("plugin.lua"), serialist_plugins::AIROHA_RACE_LUA).unwrap();
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir);
    let choices = cx.update(|cx| cx.global::<Config>().codecs().choices());
    assert_eq!(choices, ["none", "my-race"]);
    assert!(drawn(cx, window, "codec-picker"));
    assert!(drawn(cx, window, DockPanel::Decoded.rail_id()));

    // The menu opens (with the examples not installed in a submenu); the palette offers
    // them too.
    let examples = cx.update(|cx| {
        plugin_files::examples_to_install(cx)
            .iter()
            .map(|example| example.name)
            .collect::<Vec<_>>()
    });
    assert_eq!(examples.first(), Some(&"airoha-race"));
    cx.update_window(window, |_, window, cx| window.click("codec-picker", cx))
        .unwrap();
    cx.run_until_parked();
    draw(cx, window);
    press(cx, window, "escape");
    let labels = palette_labels(cx, window, &workspace);
    assert!(labels.contains(&INSTALL_RACE.to_owned()), "{labels:?}");

    // The menu's item installs through the session view; the watcher brings it in.
    view.update(cx, |v, cx| v.install_example_plugin("airoha-race", cx));
    run_until(cx, "the watcher to load the plugin", |cx| {
        has_codec(cx, "airoha-race")
    });
    let choices = cx.update(|cx| cx.global::<Config>().codecs().choices());
    assert_eq!(choices, ["none", "airoha-race", "my-race"]);
    // Installed already: nothing is touched, and the notice says why.
    view.update(cx, |v, cx| v.install_example_plugin("airoha-race", cx));
    let shown = notice(cx, &view).expect("a notice");
    assert!(shown.is_error);
    assert!(
        shown.text.starts_with("Not installed: airoha-race: ")
            && shown.text.ends_with("already exists"),
        "{}",
        shown.text
    );
}

#[gpui_test]
fn a_profile_naming_a_missing_plugin_connects_without_a_codec_and_offers_to_install_it(
    cx: &mut TestAppContext,
) {
    let dir = config_dir("plugins-missing", RACE_PROFILE);
    let Opened {
        window,
        workspace,
        view,
        _world,
    } = open(cx, &dir);
    // Connected, and not decoding: the profile's plugin is not installed.
    assert_eq!(
        view.read_with(cx, |v, _| v.codec_name().map(str::to_owned)),
        None
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.wanted_codec().map(str::to_owned)),
        Some("airoha-race".to_owned())
    );
    let shown = notice(cx, &view).expect("a notice");
    assert_eq!(
        shown.text,
        "The airoha-race plugin is not installed; connected without a codec"
    );
    assert!(shown.is_error);
    assert_eq!(
        shown.action,
        Some(NoticeAction::InstallExamplePlugin("airoha-race".into()))
    );
    let status = workspace.read_with(cx, |w, cx| w.status_line(cx)).unwrap();
    assert_eq!(status.notice.as_ref(), Some(&shown));
    assert_eq!(status.codec, None);
    assert!(!workspace.read_with(cx, |w, _| w.is_panel_shown(DockPanel::Decoded)));

    // A plugin the app has no example of offers the plugins folder instead.
    assert_eq!(
        plugin_files::action_for_missing("my-proto"),
        NoticeAction::OpenPluginsFolder
    );

    // The Devices row still names the profile's plugin, greyed: it is not installed.
    let devices = workspace.read_with(cx, |w, _| w.devices().clone());
    let race_row = |cx: &mut TestAppContext| {
        devices.read_with(cx, |devices, cx| {
            let entries = devices.list().entries();
            let ix = entries
                .iter()
                .position(|entry| entry.info.id == PortId::new("virtual:race"))
                .expect("listed");
            let plugin = devices.plugin_for(&entries[ix].info, cx);
            let installed = devices.plugin_installed("airoha-race", cx);
            (ix, plugin, installed)
        })
    };
    let (ix, plugin, installed) = race_row(cx);
    assert_eq!(plugin.as_deref(), Some("airoha-race"));
    assert!(!installed, "shown greyed, with a tooltip");
    draw(cx, window);
    let chip = cx
        .update_window(window, |_, window, _| {
            window.try_find(("device-plugin", ix)).is_some()
        })
        .unwrap();
    assert!(chip, "the plugin's badge is on the row");

    // The notice's Install button installs it; the watcher loads it, and the session
    // decodes with it from then on.
    assert!(drawn(cx, window, "status-notice-action"));
    cx.update_window(window, |_, window, cx| {
        window.click("status-notice-action", cx)
    })
    .unwrap();
    run_until(
        cx,
        "the session to decode with the installed plugin",
        |cx| view.read_with(cx, |v, _| v.codec_name() == Some("airoha-race")),
    );
    assert_eq!(
        view.read_with(cx, |v, _| v.wanted_codec().map(str::to_owned)),
        None
    );
    run_until(cx, "a decoded log frame", |cx| {
        view.read_with(cx, |v, _| {
            v.frame_reader()
                .snapshot()
                .frames()
                .any(|(_, frame)| frame.kind == "log")
        })
    });
    assert!(workspace.read_with(cx, |w, _| w.is_panel_shown(DockPanel::Decoded)));
    assert!(race_row(cx).2, "the badge is no longer greyed");
}

#[gpui_test]
fn a_codec_command_without_its_plugin_is_not_sent_and_says_what_to_install(
    cx: &mut TestAppContext,
) {
    let dir = config_dir("plugins-command", "{}");
    fs::create_dir_all(dir.join("commands")).unwrap();
    fs::write(
        dir.join("commands").join("mine.json"),
        r#"{ "name": "Mine", "groups": [ { "name": "G", "commands": [
            { "name": "Hex query", "payload": { "hex": "05 5A 02 00 15 0F" },
              "expect": { "frame": { "kind": "response" }, "timeout_ms": 300 } } ] } ] }"#,
    )
    .unwrap();
    let Opened {
        workspace,
        view,
        _world,
        ..
    } = open(cx, &dir);
    let commands = workspace.read_with(cx, |w, _| w.commands().clone());
    let sent = |cx: &mut TestAppContext| {
        displayed(cx, &view)
            .iter()
            .any(|line| line.direction == Direction::Tx)
    };

    // The bundled RACE version command: a codec payload and a frame predicate.
    commands.update(cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "RACE", "RACE version"), cx);
    });
    cx.run_until_parked();
    let shown = notice(cx, &view).expect("a notice");
    assert_eq!(
        shown.text,
        "RACE version: Install the airoha-race plugin to send this command"
    );
    assert!(shown.is_error);
    assert_eq!(
        shown.action,
        Some(NoticeAction::InstallExamplePlugin("airoha-race".into()))
    );
    // The one with a parameter says so before asking for it.
    commands.update(cx, |panel, cx| {
        panel.send(CommandRef::new("AT basics", "RACE", "RACE command"), cx);
    });
    cx.run_until_parked();
    assert!(workspace.read_with(cx, |w, _| w.param_prompt().is_none()));
    assert_eq!(
        notice(cx, &view).map(|notice| notice.text).as_deref(),
        Some("RACE command: Install the airoha-race plugin to send this command")
    );
    assert!(!sent(cx), "nothing was written");
    assert_eq!(view.read_with(cx, |v, _| v.stats().tx_bytes), 0);

    // Bytes of its own are sent, but a decoded reply cannot be waited for.
    commands.update(cx, |panel, cx| {
        panel.send(CommandRef::new("Mine", "G", "Hex query"), cx);
    });
    run_until(cx, "the hex command's echo", sent);
    assert_eq!(
        notice(cx, &view).map(|notice| notice.text).as_deref(),
        Some(
            "Sent Hex query; its reply is a decoded frame, and no codec plugin is installed \
             to decode it"
        )
    );
}
