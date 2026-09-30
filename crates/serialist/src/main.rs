//! Serialist: a native, GPU-rendered serial terminal.
//!
//! The binary parses flags, sets up logging, picks the port source and transport
//! factory, and hands them to the UI. Everything visual lives in `serialist-ui`.

mod cli;
mod wiring;

use serialist_ui::prelude::application;
use tracing_subscriber::EnvFilter;

use crate::cli::Command;

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

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let options = match wiring::app_options(&args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("serialist: {error:#}");
            std::process::exit(2);
        }
    };
    application().run(move |cx| {
        serialist_ui::init(cx);
        if let Err(error) = serialist_ui::open_main_window(options, cx) {
            tracing::error!("could not open the main window: {error:#}");
            cx.quit();
        }
    });
}
