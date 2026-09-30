//! Command-line flags. Hand-rolled: three flags do not justify an argument-parser
//! dependency, and keeping `main` free of one keeps cold start small.

use std::path::PathBuf;

use anyhow::{Context as _, bail};

pub const USAGE: &str = "\
Usage: serialist [OPTIONS]

Options:
  --port <PATH>       Open this port at startup (virtual:<NAME> for a simulated one)
  --baud <N>          Baud rate for --port and the Connect field, any positive
                      integer (default 115200)
  --virtual [NAME]    List the simulated devices next to the real ports; with a
                      NAME, also open virtual:<NAME> at startup (repeatable; the
                      first is opened unless --port is given). Built-ins: echo,
                      echo-lines, at, firehose, firehose-ansi
  --config-dir <DIR>  Read settings.json, keymap.json and themes/ from DIR instead
                      of the user config directory (also SERIALIST_CONFIG_DIR)
  --terminal-demo     Open only the milestone 1 terminal element, fed by an
                      in-memory stream (200 000 lines, 2 000 more a second)
  -h, --help          Print this help
  -V, --version       Print the version

Logging follows RUST_LOG, for example RUST_LOG=serialist=debug.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub port: Option<String>,
    pub baud: Option<u32>,
    /// Simulated devices named with `--virtual NAME`, in order.
    pub virtual_devices: Vec<String>,
    /// `--virtual` was given at all, with or without a name.
    pub simulator: bool,
    /// `--terminal-demo`: the terminal element alone, over an in-memory stream.
    pub terminal_demo: bool,
    /// `--config-dir`: where settings, keymap and themes live, over the environment
    /// and the platform default.
    pub config_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Run(Args),
    Help,
    Version,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Command> {
    let mut parsed = Args::default();
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        if arg == "--virtual" {
            // The name is optional, so only a following non-flag argument is taken.
            parsed.simulator = true;
            if let Some(name) = args.next_if(|next| !next.starts_with('-')) {
                parsed.virtual_devices.push(name);
            }
            continue;
        }
        // Accept both `--flag value` and `--flag=value`.
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => {
                (flag.to_owned(), Some(value.to_owned()))
            }
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> anyhow::Result<String> {
            match inline.clone().or_else(|| args.next()) {
                Some(value) if !value.is_empty() => Ok(value),
                _ => bail!("{name} needs a value"),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "--terminal-demo" if inline.is_none() => parsed.terminal_demo = true,
            "--port" => parsed.port = Some(value("--port")?),
            "--config-dir" => parsed.config_dir = Some(PathBuf::from(value("--config-dir")?)),
            "--baud" => {
                let text = value("--baud")?;
                let baud = serialist_ui::parse_baud(&text)
                    .with_context(|| format!("invalid --baud {text:?}"))?;
                parsed.baud = Some(baud);
            }
            "--virtual" => {
                // Only the `--virtual=NAME` spelling reaches here.
                parsed.simulator = true;
                parsed
                    .virtual_devices
                    .extend(inline.filter(|name| !name.is_empty()));
            }
            other => bail!("unknown argument {other:?}"),
        }
    }
    Ok(Command::Run(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> anyhow::Result<Command> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_flags_is_a_plain_run() {
        assert_eq!(run(&[]).unwrap(), Command::Run(Args::default()));
    }

    #[test]
    fn all_flags_in_both_spellings() {
        let expected = Command::Run(Args {
            port: Some("/dev/cu.usbserial-1420".into()),
            baud: Some(921_600),
            virtual_devices: vec!["echo".into(), "at".into()],
            simulator: true,
            terminal_demo: false,
            config_dir: Some(PathBuf::from("/tmp/serialist config")),
        });
        let spaced = run(&[
            "--port",
            "/dev/cu.usbserial-1420",
            "--baud",
            "921600",
            "--virtual",
            "echo",
            "--virtual",
            "at",
            "--config-dir",
            "/tmp/serialist config",
        ]);
        let joined = run(&[
            "--port=/dev/cu.usbserial-1420",
            "--baud=921600",
            "--virtual=echo",
            "--virtual=at",
            "--config-dir=/tmp/serialist config",
        ]);
        assert_eq!(spaced.unwrap(), expected);
        assert_eq!(joined.unwrap(), expected);
    }

    #[test]
    fn virtual_without_a_name_just_enables_the_simulator() {
        let bare = Command::Run(Args {
            simulator: true,
            ..Args::default()
        });
        assert_eq!(run(&["--virtual"]).unwrap(), bare);
        assert_eq!(run(&["--virtual="]).unwrap(), bare);
        let Command::Run(args) = run(&["--virtual", "--baud", "9600"]).unwrap() else {
            panic!("expected run");
        };
        assert!(args.simulator);
        assert!(
            args.virtual_devices.is_empty(),
            "a flag is not a device name"
        );
        assert_eq!(args.baud, Some(9600));
    }

    #[test]
    fn custom_baud_rates_are_accepted() {
        let Command::Run(args) = run(&["--baud", "250000"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(args.baud, Some(250_000));
    }

    #[test]
    fn terminal_demo_flag() {
        let Command::Run(args) = run(&["--terminal-demo"]).unwrap() else {
            panic!("expected run");
        };
        assert!(args.terminal_demo);
        assert!(!args.simulator);
        assert!(run(&["--terminal-demo=yes"]).is_err(), "takes no value");
    }

    #[test]
    fn help_and_version() {
        assert_eq!(run(&["--help"]).unwrap(), Command::Help);
        assert_eq!(run(&["-V"]).unwrap(), Command::Version);
    }

    #[test]
    fn errors_name_the_problem() {
        let message = |args: &[&str]| format!("{:#}", run(args).unwrap_err());
        assert_eq!(message(&["--port"]), "--port needs a value");
        assert_eq!(message(&["--config-dir"]), "--config-dir needs a value");
        assert_eq!(message(&["--baud="]), "--baud needs a value");
        assert!(message(&["--baud", "fast"]).starts_with("invalid --baud \"fast\""));
        assert!(message(&["--baud", "0"]).contains("above zero"));
        assert_eq!(message(&["--bogus"]), "unknown argument \"--bogus\"");
    }
}
