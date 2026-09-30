//! Settings, theme, fonts and keymap as one GPUI global, loaded from the config
//! directory and applied to the running app.
//!
//! # Flow
//!
//! [`start`] loads everything from [`ConfigPaths`] (the `--config-dir` flag, else
//! `SERIALIST_CONFIG_DIR`, else the platform directory) and installs it with
//! [`install`]: gpui-kit's theme and fonts are replaced through the theme bridge, the
//! key bindings are rebuilt, and the [`Config`] global is set, which notifies every view
//! that called `cx.observe_global::<Config>`. Views read what they need from the global
//! when built and again when notified: the terminal its font and palette, the session
//! view its display defaults, the Devices panel its profiles, the workspace the notice.
//!
//! A [`ConfigWatcher`] reports saves; a foreground task drains its channel, reloads the
//! piece that changed (settings, keymap or themes) and installs the result.
//!
//! # Failures
//!
//! Nothing here is fatal. A settings file that does not parse keeps the last good
//! settings (the bundled defaults at startup); a bad keymap keeps the last good
//! bindings; a bad theme file is skipped. Each problem is logged and shown in the status
//! line through [`Config::notice`] until the next load that succeeds.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use serialist_core::config_watch::{ConfigEvent, ConfigWatcher};
use serialist_core::settings::ConfigPaths;
use serialist_core::{
    Appearance, FontSpec, Keymap, PortInfo, Rgba as CoreRgba, SerialConfig, Settings,
    SettingsError, Theme as ZedTheme, ThemeRegistry, load_keymap, load_settings,
};

use crate::fonts::{
    FontParts, FontRole, TerminalFont, UiFont, build_font, clamp_font_size, clamp_line_height,
    installed_families, substitute_missing_family,
};
use crate::keymap;
use crate::prelude::*;
use crate::status::Notice;
use crate::terminal::TerminalPalette;
use crate::theme_bridge::{self, ZedColors};

/// A core color as a GPUI one.
pub fn hsla(color: CoreRgba) -> Hsla {
    Hsla::from(Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    })
}

/// The terminal font a font spec asks for.
pub fn terminal_font(spec: &FontSpec) -> TerminalFont {
    TerminalFont {
        font: build_font(&font_parts(spec), FontRole::Mono),
        size: clamp_font_size(spec.size),
        line_height: clamp_line_height(spec.line_height),
    }
}

/// The UI font a font spec asks for.
pub fn ui_font(spec: &FontSpec) -> UiFont {
    UiFont {
        font: build_font(&font_parts(spec), FontRole::Ui),
        size: clamp_font_size(spec.size),
    }
}

fn font_parts(spec: &FontSpec) -> FontParts<'_> {
    FontParts {
        family: spec.family.as_deref(),
        weight: Some(spec.weight),
        features: spec
            .features
            .iter()
            .map(|(tag, value)| (tag.to_owned(), value))
            .collect(),
        fallbacks: &spec.fallbacks,
    }
}

/// Which part of the configuration a load or a problem is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfigPiece {
    /// `settings.json` and the project settings.
    Settings,
    /// `keymap.json`.
    Keymap,
    /// The theme files in `themes/`.
    Themes,
    /// The theme the settings select, which may name one no file provides.
    Theme,
    /// Keymap entries GPUI could not bind: an unknown action, a bad keystroke.
    Bindings,
    /// A font family the settings name that is not installed.
    Fonts,
}

impl fmt::Display for ConfigPiece {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConfigPiece::Settings => "settings",
            ConfigPiece::Keymap => "keymap",
            ConfigPiece::Themes => "themes",
            ConfigPiece::Theme => "theme",
            ConfigPiece::Bindings => "bindings",
            ConfigPiece::Fonts => "fonts",
        })
    }
}

/// A problem from the last load of one piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigProblem {
    pub piece: ConfigPiece,
    /// The load failed and something older (or the default) is in use.
    pub is_error: bool,
    pub message: String,
}

/// The loaded configuration and everything derived from it.
#[derive(Clone)]
pub struct Config {
    paths: ConfigPaths,
    settings: Arc<Settings>,
    /// The bundled default bindings followed by the user's.
    keymap: Arc<Keymap>,
    themes: Arc<ThemeRegistry>,
    theme: Arc<ZedTheme>,
    /// The window appearance `"mode": "system"` follows.
    system_dark: bool,
    terminal_font: TerminalFont,
    ui_font: UiFont,
    palette: TerminalPalette,
    problems: Vec<ConfigProblem>,
    /// Bumped by every install, so a view can tell a reload from a repeat.
    generation: u64,
}

impl Global for Config {}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("dir", &self.paths.dir)
            .field("theme", &self.theme.name)
            .field("problems", &self.problems)
            .finish_non_exhaustive()
    }
}

impl Config {
    /// The bundled defaults, with nothing read from disk. Paths point at `paths`.
    pub fn bundled(paths: ConfigPaths) -> Self {
        let settings = Arc::new(Settings::default());
        let themes = Arc::new(ThemeRegistry::bundled());
        let mut config = Self {
            paths,
            theme: Arc::new(themes.default_for(Appearance::Dark).clone()),
            settings,
            keymap: Arc::new(Keymap::bundled_default()),
            themes,
            system_dark: true,
            terminal_font: TerminalFont::default(),
            ui_font: UiFont::default(),
            palette: TerminalPalette::default(),
            problems: Vec::new(),
            generation: 0,
        };
        config.derive();
        config
    }

    /// Everything under `paths`, with bundled defaults for whatever fails to load.
    pub fn load(paths: ConfigPaths, system_dark: bool) -> Self {
        let mut config = Self::bundled(paths);
        config.system_dark = system_dark;
        // Themes first, so the settings resolve their theme against the user's files
        // rather than warning that it is missing from the bundled ones.
        config.reload_themes();
        config.reload_keymap();
        config.reload_settings();
        config
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn settings(&self) -> &Arc<Settings> {
        &self.settings
    }

    pub fn keymap(&self) -> &Arc<Keymap> {
        &self.keymap
    }

    /// The resolved Zed theme for the current appearance.
    pub fn theme(&self) -> &Arc<ZedTheme> {
        &self.theme
    }

    pub fn themes(&self) -> &Arc<ThemeRegistry> {
        &self.themes
    }

    pub fn terminal_font(&self) -> &TerminalFont {
        &self.terminal_font
    }

    pub fn ui_font(&self) -> &UiFont {
        &self.ui_font
    }

    pub fn palette(&self) -> &TerminalPalette {
        &self.palette
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn is_dark(&self) -> bool {
        self.theme.is_dark()
    }

    /// A Zed theme key with no gpui-kit counterpart, such as `toolbar.background`,
    /// for the views that paint it themselves.
    pub fn color(&self, key: &str) -> Option<Hsla> {
        self.theme.color(key).map(hsla)
    }

    /// The `toolbar.background` of the compose bar and the search bar, if the theme
    /// sets one.
    pub fn toolbar_background(cx: &App) -> Option<Hsla> {
        cx.try_global::<Config>()?.color("toolbar.background")
    }

    pub fn problems(&self) -> &[ConfigProblem] {
        &self.problems
    }

    /// What the status line says about the configuration: the first problem, with a
    /// count of the others. An error (a file that did not load) outranks a warning.
    pub fn notice(&self) -> Option<Notice> {
        let first = self
            .problems
            .iter()
            .find(|p| p.is_error)
            .or_else(|| self.problems.first())?;
        let more = self.problems.len() - 1;
        let text = if more == 0 {
            first.message.clone()
        } else {
            format!("{} (+{more} more)", first.message)
        };
        Some(if first.is_error {
            Notice::error(text)
        } else {
            Notice::info(text)
        })
    }

    /// The line configuration and settings to open `port` with: the first matching
    /// device profile over `default_baud` and 8N1.
    pub fn serial_config_for(&self, port: &PortInfo) -> SerialConfig {
        self.settings.serial_config_for(port)
    }

    fn set_problems(&mut self, piece: ConfigPiece, problems: Vec<ConfigProblem>) {
        self.problems.retain(|p| p.piece != piece);
        for problem in &problems {
            if problem.is_error {
                tracing::error!(%piece, "{}", problem.message);
            } else {
                tracing::warn!(%piece, "{}", problem.message);
            }
        }
        self.problems.extend(problems);
    }

    /// Read the settings files again. On failure the settings in use stay.
    pub fn reload_settings(&mut self) {
        let loaded = load_settings(
            Some(&self.paths.settings),
            self.paths.project_settings.as_deref(),
        );
        let problems = match loaded {
            Ok(settings) => {
                let warnings = settings
                    .warnings
                    .iter()
                    .map(|warning| ConfigProblem {
                        piece: ConfigPiece::Settings,
                        is_error: false,
                        message: warning.to_string(),
                    })
                    .collect();
                self.settings = Arc::new(settings);
                warnings
            }
            Err(error) => vec![settings_error(&error)],
        };
        self.set_problems(ConfigPiece::Settings, problems);
        self.derive();
    }

    /// Read the keymap file again, over the bundled defaults. On failure the bindings
    /// in use stay.
    pub fn reload_keymap(&mut self) {
        let problems = match load_keymap(Some(&self.paths.keymap)) {
            Ok(keymap) => {
                self.keymap = Arc::new(keymap);
                Vec::new()
            }
            Err(error) => vec![ConfigProblem {
                piece: ConfigPiece::Keymap,
                is_error: true,
                message: format!("Keymap not applied: {error}"),
            }],
        };
        self.set_problems(ConfigPiece::Keymap, problems);
    }

    /// Read the themes folder again.
    pub fn reload_themes(&mut self) {
        self.themes = Arc::new(ThemeRegistry::load_from(&self.paths));
        let problems = self
            .themes
            .take_warnings()
            .into_iter()
            .map(|warning| ConfigProblem {
                piece: ConfigPiece::Themes,
                is_error: false,
                message: warning.to_string(),
            })
            .collect();
        self.set_problems(ConfigPiece::Themes, problems);
        self.derive();
    }

    /// Follow a new window appearance. Returns whether the theme changed.
    pub fn set_system_dark(&mut self, dark: bool) -> bool {
        if self.system_dark == dark {
            return false;
        }
        self.system_dark = dark;
        let before = self.theme.name.clone();
        self.derive();
        self.theme.name != before
    }

    /// Recompute the theme, fonts and palette from the settings and the registry.
    fn derive(&mut self) {
        let theme = self
            .themes
            .resolve(&self.settings.theme, self.system_dark)
            .clone();
        // A theme the settings name but no file provides comes back as a warning.
        let missing = self
            .themes
            .take_warnings()
            .into_iter()
            .map(|warning| ConfigProblem {
                piece: ConfigPiece::Theme,
                is_error: false,
                message: warning.to_string(),
            })
            .collect();
        self.set_problems(ConfigPiece::Theme, missing);
        self.palette = TerminalPalette::from_lookup(|key| theme.color(key).map(hsla));
        self.theme = Arc::new(theme);
        self.terminal_font = terminal_font(&self.settings.resolved_terminal_font());
        self.ui_font = ui_font(&self.settings.resolved_ui_font());
    }

    /// Swap a font family the settings name but the machine lacks for an installed one
    /// (see [`substitute_missing_family`]), and say so. Fonts are only listed when a
    /// family is named, so the defaults cost nothing at startup.
    fn check_fonts(&mut self, cx: &App) {
        let named = self.settings.resolved_terminal_font().family.is_some()
            || self.settings.resolved_ui_font().family.is_some();
        let mut problems = Vec::new();
        if named {
            let installed = installed_families(cx);
            let fonts = [
                (&mut self.terminal_font.font, FontRole::Mono, "the terminal"),
                (&mut self.ui_font.font, FontRole::Ui, "the UI"),
            ];
            for (font, role, user) in fonts {
                if let Some(missing) = substitute_missing_family(font, role, installed) {
                    problems.push(ConfigProblem {
                        piece: ConfigPiece::Fonts,
                        is_error: false,
                        message: format!(
                            "Font {missing:?} is not installed; {user} uses {:?}",
                            font.family
                        ),
                    });
                }
            }
        }
        self.set_problems(ConfigPiece::Fonts, problems);
    }

    /// gpui-kit's theme for this configuration.
    pub fn kit_theme(&self) -> ThemeConfig {
        let theme = self.theme.clone();
        let lookup = move |key: &str| theme.color(key).map(hsla);
        theme_bridge::kit_theme_config(
            &ZedColors {
                name: &self.theme.name,
                dark: self.theme.is_dark(),
                lookup: &lookup,
            },
            &self.ui_font,
            &self.terminal_font,
        )
    }
}

fn settings_error(error: &SettingsError) -> ConfigProblem {
    ConfigProblem {
        piece: ConfigPiece::Settings,
        is_error: true,
        message: format!("Settings not applied: {error}"),
    }
}

/// Opens a file or folder with the platform's default application.
pub type OpenPath = dyn Fn(&Path) -> io::Result<()> + Send + Sync;

/// How [`OpenSettings`](crate::actions::OpenSettings) and friends open files: the
/// platform opener by default, a recorder in tests.
#[derive(Clone)]
pub struct Opener(pub Arc<OpenPath>);

impl Global for Opener {}

impl Default for Opener {
    fn default() -> Self {
        Self(Arc::new(open_with_platform))
    }
}

/// `open` on macOS, `start` on Windows, `xdg-open` elsewhere. The child is not waited
/// for; the opener returns once it has started.
pub fn open_with_platform(path: &Path) -> io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        // `start` treats a first quoted argument as the window title.
        command.args(["/C", "start", ""]);
        command
    } else {
        std::process::Command::new("xdg-open")
    };
    command.arg(path);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command.spawn()?;
    // Reap the child off the main thread so it does not linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Open `path` with the installed [`Opener`], reporting failure in the log.
pub fn open_path(path: &Path, cx: &mut App) {
    let opener = cx.try_global::<Opener>().cloned().unwrap_or_default();
    if let Err(error) = (opener.0)(path) {
        tracing::error!(path = %path.display(), %error, "could not open");
    }
}

/// Install `config`: gpui-kit's theme and fonts, the key bindings (only when the
/// keymap changed), then the global, which notifies every observer.
pub fn install(mut config: Config, cx: &mut App) {
    let previous = cx.try_global::<Config>();
    config.generation = previous.map_or(1, |previous| previous.generation + 1);
    let rebind = previous.is_none_or(|previous| !Arc::ptr_eq(&previous.keymap, &config.keymap));
    config.check_fonts(cx);
    theme_bridge::apply_kit_theme(config.kit_theme(), cx);
    if rebind {
        let problems = keymap::apply(&config.keymap, cx)
            .into_iter()
            .map(|message| ConfigProblem {
                piece: ConfigPiece::Bindings,
                is_error: false,
                message,
            })
            .collect();
        config.set_problems(ConfigPiece::Bindings, problems);
    }
    cx.set_global(config);
}

/// Change the installed configuration with `edit`, then install the result.
pub fn update(cx: &mut App, edit: impl FnOnce(&mut Config)) {
    let mut config = cx
        .try_global::<Config>()
        .cloned()
        .unwrap_or_else(|| Config::bundled(ConfigPaths::default_for_platform()));
    edit(&mut config);
    install(config, cx);
}

/// Reload one piece from disk and install the result.
pub fn reload(piece: ConfigPiece, cx: &mut App) {
    tracing::info!(%piece, "reloading configuration");
    update(cx, |config| match piece {
        ConfigPiece::Settings | ConfigPiece::Fonts => config.reload_settings(),
        ConfigPiece::Keymap | ConfigPiece::Bindings => config.reload_keymap(),
        ConfigPiece::Themes | ConfigPiece::Theme => config.reload_themes(),
    });
}

/// Reload everything from disk.
pub fn reload_all(cx: &mut App) {
    update(cx, |config| {
        config.reload_themes();
        config.reload_keymap();
        config.reload_settings();
    });
}

/// Follow a window appearance change; re-resolves a `"mode": "system"` theme.
pub fn set_appearance(appearance: WindowAppearance, cx: &mut App) {
    let dark = matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    let changed = cx
        .try_global::<Config>()
        .is_some_and(|config| config.system_dark != dark);
    if changed {
        update(cx, |config| {
            config.set_system_dark(dark);
        });
    }
}

/// Where the config lives: `--config-dir`, else `SERIALIST_CONFIG_DIR`, else the
/// platform directory. A project `.serialist/settings.json` above the working
/// directory joins it.
pub fn paths_for(config_dir: Option<PathBuf>) -> ConfigPaths {
    let paths = match config_dir {
        Some(dir) => ConfigPaths::new(dir),
        None => ConfigPaths::default_for_platform(),
    };
    match std::env::current_dir() {
        Ok(cwd) => paths.with_project_from(&cwd),
        Err(_) => paths,
    }
}

/// Longest the watcher task blocks on the channel (on the background executor) before
/// looking again, so it notices the app going away.
const WATCH_POLL: Duration = Duration::from_millis(50);

/// Pause between batches of watcher events.
const WATCH_FRAME: Duration = Duration::from_millis(8);

/// The piece a watcher event asks to reload.
fn piece_of(event: ConfigEvent) -> ConfigPiece {
    match event {
        ConfigEvent::Settings => ConfigPiece::Settings,
        ConfigEvent::Keymap => ConfigPiece::Keymap,
        ConfigEvent::Themes => ConfigPiece::Themes,
    }
}

/// The running watcher and the task draining it, kept alive as a global.
struct Watching {
    _watcher: ConfigWatcher,
    _task: Task<()>,
}

impl Global for Watching {}

/// Reload whatever the files under `paths` change, until the app quits or [`start`]
/// replaces the watch. The watcher's thread only sends events; the loads and installs
/// run on the main thread, one per piece that changed in each batch.
pub fn watch(paths: &ConfigPaths, cx: &mut App) {
    let (tx, rx) = crossbeam_channel::unbounded::<ConfigEvent>();
    let watcher = ConfigWatcher::spawn(paths, tx);
    let task = cx.spawn(async move |cx| {
        loop {
            let events = rx.clone();
            let (batch, closed) = cx
                .background_spawn(async move {
                    match events.recv_timeout(WATCH_POLL) {
                        Ok(first) => {
                            let mut batch = vec![first];
                            batch.extend(events.try_iter());
                            (batch, false)
                        }
                        Err(RecvTimeoutError::Timeout) => (Vec::new(), false),
                        Err(RecvTimeoutError::Disconnected) => (Vec::new(), true),
                    }
                })
                .await;
            let mut pieces: Vec<ConfigPiece> = Vec::new();
            for piece in batch.into_iter().map(piece_of) {
                if !pieces.contains(&piece) {
                    pieces.push(piece);
                }
            }
            if !pieces.is_empty() {
                cx.update(|cx| {
                    for piece in pieces {
                        reload(piece, cx);
                    }
                });
            }
            if closed {
                break;
            }
            cx.background_executor().timer(WATCH_FRAME).await;
        }
    });
    cx.set_global(Watching {
        _watcher: watcher,
        _task: task,
    });
}

/// Load and install the configuration under `paths`, then watch it for changes.
pub fn start(paths: ConfigPaths, cx: &mut App) {
    let dark = matches!(
        cx.window_appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    tracing::info!(dir = %paths.dir.display(), "loading configuration");
    install(Config::load(paths.clone(), dark), cx);
    watch(&paths, cx);
}
