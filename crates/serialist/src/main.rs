//! Serialist: a native, GPU-rendered serial terminal.
//!
//! The binary parses flags, sets up logging, picks the port source and transport
//! factory, and hands them to the UI. Everything visual lives in `serialist-ui`.
//! `--script` runs a Lua script against `--port` with no window instead (see
//! [`headless`]).

mod cli;
mod headless;
mod wiring;

use std::path::Path;
use std::sync::Arc;

use serialist_script::{ScriptOutcome, StdioUi};
use serialist_sim::SimWorld;
use serialist_ui::config;
use serialist_ui::prelude::application;
use tracing_subscriber::EnvFilter;

use crate::cli::{Args, Command};

fn main() {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(Command::Run(args)) => args,
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            return;
        }
        Ok(Command::Version) => {
            println!("serialist {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(error) => {
            eprintln!("serialist: {error:#}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    if let Some(script) = args.script.clone() {
        std::process::exit(run_script(&args, &script));
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // --config-dir, else SERIALIST_CONFIG_DIR, else the platform's config directory.
    let config_paths = config::paths_for(args.config_dir.clone());

    if args.terminal_demo {
        application().run(move |cx| {
            serialist_ui::init(cx);
            config::start(config_paths, cx);
            if let Err(error) = serialist_ui::terminal::open_terminal_demo(cx) {
                tracing::error!("could not open the terminal demo: {error:#}");
                cx.quit();
            }
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
        });
        return;
    }

    let options = match wiring::app_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("serialist: {error:#}");
            std::process::exit(2);
        }
    };
    application().run(move |cx| {
        serialist_ui::init(cx);
        // Settings, keymap and themes: loaded before the window opens so it starts in
        // the user's theme and fonts, then watched for changes.
        config::start(config_paths, cx);
        if let Err(error) = serialist_ui::open_main_window(options, cx) {
            tracing::error!("could not open the main window: {error:#}");
            cx.quit();
        }
    });
}

/// `--script`: run the script headless and return the exit status. The script's output
/// is stdout's alone: logs go to stderr, warnings and errors only unless `RUST_LOG`
/// says otherwise.
fn run_script(args: &Args, script: &Path) -> i32 {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();
    let paths = config::paths_for(args.config_dir.clone());
    let outcome = headless::run(
        args,
        script,
        &paths,
        wiring::Backend::real(),
        SimWorld::new(),
        Arc::new(StdioUi::new()),
    );
    match outcome {
        Ok(outcome) => {
            match &outcome {
                ScriptOutcome::Ok => {}
                ScriptOutcome::Error(message) => {
                    eprintln!("serialist: the script failed: {message}")
                }
                ScriptOutcome::Stopped => eprintln!("serialist: the script was stopped"),
            }
            headless::exit_code(&outcome)
        }
        Err(error) => {
            eprintln!("serialist: {error:#}");
            2
        }
    }
}
