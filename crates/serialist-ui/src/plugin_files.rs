//! Plugins from the UI's side: the bundled example plugins that the codec menu and the
//! command palette offer to install, installing one, opening the plugins folder, and the
//! notice for a plugin that a device profile or a saved command names but that is not
//! installed.
//!
//! Decoders are plugins the user installs and enables; the app ships with none active.
//! Installing an example copies its folder into the config directory's `plugins/`
//! ([`ConfigPaths::install_example_plugin`](serialist_core::settings::ConfigPaths::install_example_plugin)).
//! Nothing reloads here: the configuration's watcher sees the new folder and loads it like
//! any plugin (see [`config`]), the session toolbar's codec menu appears, and a session
//! whose device profile named the plugin starts decoding with it.

use std::io;

use serialist_plugins::{EXAMPLE_PLUGINS, ExamplePlugin, example_plugin};

use crate::config::{self, Config};
use crate::prelude::*;
use crate::status::{Notice, NoticeAction};

/// The bundled example plugins that are not installed, in the order menus list them.
pub fn examples_to_install(cx: &App) -> Vec<&'static ExamplePlugin> {
    match cx.try_global::<Config>() {
        Some(config) => config.codecs().examples_to_install(),
        None => EXAMPLE_PLUGINS.iter().collect(),
    }
}

/// Whether there is a codec to decode with: a plugin is installed and loaded.
pub fn has_codecs(cx: &App) -> bool {
    cx.try_global::<Config>()
        .is_some_and(|config| !config.codecs().is_empty())
}

/// What a notice about the missing plugin `name` offers: installing the example of that
/// name when the app bundles one, else the plugins folder to put it in.
pub fn action_for_missing(name: &str) -> NoticeAction {
    if example_plugin(name).is_some() {
        NoticeAction::InstallExamplePlugin(name.to_owned())
    } else {
        NoticeAction::OpenPluginsFolder
    }
}

/// An error notice saying `text` about the missing plugin `name`, with the way to get it.
pub fn missing_plugin_notice(text: impl Into<String>, name: &str) -> Notice {
    Notice::error(text).with_action(action_for_missing(name))
}

/// Install the bundled example plugin `name`: copy its folder into the plugins folder,
/// where the watcher finds and loads it. Returns what the status line says.
pub fn install_example(name: &str, cx: &mut App) -> Notice {
    let Some(example) = example_plugin(name) else {
        return Notice::error(format!("No example plugin is named {name}"));
    };
    let Some(config) = cx.try_global::<Config>() else {
        return Notice::error(format!(
            "Not installed: {name}: no configuration directory to install it in"
        ));
    };
    if !config.is_loaded() {
        return Notice::error(format!(
            "Not installed: {name}: the configuration was not read from a directory"
        ));
    }
    let paths = config.paths().clone();
    match paths.install_example_plugin(example) {
        Ok(folder) => {
            tracing::info!(plugin = name, folder = %folder.display(), "installed an example plugin");
            Notice::info(format!(
                "Installed the {} example plugin in {}",
                example.title,
                folder.display()
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Notice::error(format!("Not installed: {name}: {error}"))
        }
        Err(error) => {
            tracing::error!(plugin = name, %error, "could not install an example plugin");
            Notice::error(format!("Could not install the {name} plugin: {error}"))
        }
    }
}

/// Create the plugins folder if needed, with copies of the example plugins in its
/// `examples/` folder (where they decode nothing), then open it. Only a configuration
/// read from its directory writes there.
pub fn open_plugins_folder(cx: &mut App) {
    let Some(paths) = cx
        .try_global::<Config>()
        .filter(|config| config.is_loaded())
        .map(|config| config.paths().clone())
    else {
        tracing::warn!("no configuration directory to open the plugins folder in");
        return;
    };
    match paths.ensure_example_plugins(EXAMPLE_PLUGINS) {
        Ok(written) => {
            if !written.is_empty() {
                tracing::info!(count = written.len(), "wrote the example plugins");
            }
            config::open_path(&paths.plugins_dir(), cx);
        }
        Err(error) => tracing::error!(%error, "could not prepare the plugins folder"),
    }
}

/// Do what a notice's button says.
pub fn run_notice_action(action: &NoticeAction, cx: &mut App) -> Option<Notice> {
    match action {
        NoticeAction::InstallExamplePlugin(name) => Some(install_example(name, cx)),
        NoticeAction::OpenPluginsFolder => {
            open_plugins_folder(cx);
            None
        }
    }
}
