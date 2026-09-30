//! Command-line flags. Hand-rolled: three flags do not justify an argument-parser
//! dependency, and keeping `main` free of one keeps cold start small.

use anyhow::{Context as _, bail};

pub const USAGE: &str = "\
Usage: serialist [OPTIONS]

Options:
  --port <PATH>      Open this serial port at startup
  --baud <N>         Baud rate, any positive integer (default 115200)
  --virtual <NAME>   List a simulated device as virtual:<NAME> and select it
                     (repeatable)
  -h, --help         Print this help
  -V, --version      Print the version

Logging follows RUST_LOG, for example RUST_LOG=serialist=debug.";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    pub port: Option<String>,
    pub baud: Option<u32>,
    pub virtual_devices: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Run(Args),
    Help,
    Version,
}

pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Command> {
    let mut parsed = Args::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
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
            "--port" => parsed.port = Some(value("--port")?),
            "--baud" => {
                let text = value("--baud")?;
                let baud = serialist_ui::parse_baud(&text)
                    .with_context(|| format!("invalid --baud {text:?}"))?;
                parsed.baud = Some(baud);
            }
            "--virtual" => parsed.virtual_devices.push(value("--virtual")?),
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
        ]);
        let joined = run(&[
            "--port=/dev/cu.usbserial-1420",
            "--baud=921600",
            "--virtual=echo",
            "--virtual=at",
        ]);
        assert_eq!(spaced.unwrap(), expected);
        assert_eq!(joined.unwrap(), expected);
    }

    #[test]
    fn custom_baud_rates_are_accepted() {
        let Command::Run(args) = run(&["--baud", "250000"]).unwrap() else {
            panic!("expected run");
        };
        assert_eq!(args.baud, Some(250_000));
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
        assert_eq!(message(&["--baud="]), "--baud needs a value");
        assert!(message(&["--baud", "fast"]).starts_with("invalid --baud \"fast\""));
        assert!(message(&["--baud", "0"]).contains("above zero"));
        assert_eq!(message(&["--bogus"]), "unknown argument \"--bogus\"");
    }
}
