//! `--script`: run one Lua script against a port with no window, then exit.
//!
//! The port comes from `--port` (a real path, or `virtual:<NAME>` for a simulated
//! device, which turns the simulator on as it does for the window). Its line settings
//! are the GUI's: the device profile that matches the port over `default_baud`, with
//! `--baud` over both. The script runs through
//! [`run_headless`](serialist_script::run_headless) with standard streams for its
//! output and prompts; `require` reads from the script's own folder. `main` exits with
//! [`exit_code`] of the outcome.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, PortInfo, PortKind, Settings, load_settings};
use serialist_script::{ScriptOutcome, ScriptSource, ScriptUi, run_headless};
use serialist_sim::SimWorld;

use crate::cli::Args;
use crate::wiring::{self, Backend};

/// The process's exit status for a run that ended with `outcome`: 0 when the script
/// returned, 1 on an error, 2 if it was stopped.
pub fn exit_code(outcome: &ScriptOutcome) -> i32 {
    match outcome {
        ScriptOutcome::Ok => 0,
        ScriptOutcome::Error(_) => 1,
        ScriptOutcome::Stopped => 2,
    }
}

/// Run `script` against `--port`, with the settings under `paths`, over the ports
/// `real` and `world` provide (the host's and the simulator's, or stand-ins in tests).
/// Fails before running anything without `--port`, with a script that cannot be read,
/// or with flags that name an unknown simulated device.
pub fn run(
    args: &Args,
    script: &Path,
    paths: &ConfigPaths,
    real: Backend,
    world: SimWorld,
    ui: Arc<dyn ScriptUi>,
) -> anyhow::Result<ScriptOutcome> {
    let port = match args.ports.as_slice() {
        [port] => port.as_str(),
        [] => bail!("--script needs --port <PATH> (virtual:<NAME> for a simulated device)"),
        _ => bail!("--script runs against one --port, not {}", args.ports.len()),
    };
    let source = ScriptSource::from_file(script)
        .with_context(|| format!("could not read the script {}", script.display()))?;
    let options = wiring::build(args, real, world)?;
    let port = PortId::new(port);
    let info = options
        .port_source
        .snapshot()
        .into_iter()
        .find(|info| info.id == port)
        .unwrap_or_else(|| PortInfo {
            id: port.clone(),
            kind: PortKind::Unknown,
            display_name: port.to_string(),
        });
    let settings = match load_settings(Some(&paths.settings), paths.project_settings.as_deref()) {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(%error, "settings not applied; using the defaults");
            Settings::default()
        }
    };
    let mut serial = settings.serial_config_for(&info);
    if let Some(baud) = args.baud {
        serial.baud = baud;
    }
    tracing::info!(%port, serial = %serial.summary(), script = %source.name, "running a script");
    Ok(run_headless(
        port,
        serial,
        options.transport_factory,
        source,
        ui,
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serialist_script::StdioUi;

    use super::*;
    use crate::cli::{self, Command};

    /// A scratch directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "serialist-headless-{name}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// No real ports: the tests never touch hardware.
    fn no_real_ports() -> Backend {
        let world = SimWorld::empty();
        Backend {
            port_source: world.port_source(),
            transport_factory: world.transport_factory(),
        }
    }

    fn args(flags: &[&str]) -> Args {
        match cli::parse(flags.iter().map(|flag| flag.to_string())).unwrap() {
            Command::Run(args) => args,
            other => panic!("{other:?}"),
        }
    }

    /// Run `code` from a file the way `main` does, with stdout captured. Returns the
    /// outcome and what the script printed.
    fn run_script(dir: &TempDir, flags: &[&str], code: &str) -> (ScriptOutcome, String) {
        let script = dir.write("probe.lua", code);
        let script_flag = script.display().to_string();
        let mut all = flags.to_vec();
        all.extend(["--script", &script_flag]);
        let args = args(&all);
        assert_eq!(args.script.as_deref(), Some(script.as_path()));
        let out = SharedBuf::default();
        let ui = Arc::new(StdioUi::with_io(
            Box::new(io::Cursor::new(Vec::new())),
            Box::new(out.clone()),
            Box::new(io::sink()),
        ));
        let paths = ConfigPaths::new(dir.0.join("config"));
        let outcome = run(&args, &script, &paths, no_real_ports(), SimWorld::new(), ui).unwrap();
        (outcome, out.text())
    }

    #[test]
    fn a_script_expects_ok_from_virtual_at_and_exits_zero() {
        let dir = TempDir::new("ok");
        let (outcome, out) = run_script(
            &dir,
            &["--port", "virtual:at"],
            "local port = assert(serial.current())\nport:write('AT\\r\\n')\n\
             assert(port:expect('^OK$'), 'no OK')\nprint('got OK from', port:description())\n",
        );
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(exit_code(&outcome), 0);
        assert_eq!(out, "got OK from\tvirtual:at @ 115200 8N1\n");
    }

    #[test]
    fn an_error_exits_one_and_names_the_line() {
        let dir = TempDir::new("error");
        let (outcome, _) = run_script(
            &dir,
            &["--port", "virtual:at"],
            "local x = 1\nerror('boom')\n",
        );
        let ScriptOutcome::Error(message) = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(message.contains("probe.lua:2: boom"), "{message}");
        assert_eq!(exit_code(&outcome), 1);
        assert_eq!(exit_code(&ScriptOutcome::Stopped), 2);
    }

    #[test]
    fn the_device_profile_and_baud_set_the_line() {
        let dir = TempDir::new("profile");
        std::fs::create_dir_all(dir.0.join("config")).unwrap();
        dir.write(
            "config/settings.json",
            r#"{ "devices": [ { "match": { "path": "virtual:at" }, "baud": 9600 } ] }"#,
        );
        let code = "print(serial.current():description())\n";
        let (outcome, out) = run_script(&dir, &["--port", "virtual:at"], code);
        assert_eq!(outcome, ScriptOutcome::Ok);
        assert_eq!(out, "virtual:at @ 9600 8N1\n", "the profile's baud");
        let (_, out) = run_script(&dir, &["--port", "virtual:at", "--baud", "57600"], code);
        assert_eq!(out, "virtual:at @ 57600 8N1\n", "--baud wins");
    }

    #[test]
    fn without_a_port_or_a_readable_script_nothing_runs() {
        let dir = TempDir::new("no-port");
        let script = dir.write("probe.lua", "print(1)");
        let paths = ConfigPaths::new(dir.0.join("config"));
        let ui = || {
            Arc::new(StdioUi::with_io(
                Box::new(io::empty()),
                Box::new(io::sink()),
                Box::new(io::sink()),
            ))
        };
        let error = run(
            &args(&["--virtual", "at"]),
            &script,
            &paths,
            no_real_ports(),
            SimWorld::new(),
            ui(),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "--script needs --port <PATH> (virtual:<NAME> for a simulated device)"
        );
        let missing = dir.0.join("missing.lua");
        let error = run(
            &args(&["--port", "virtual:at"]),
            &missing,
            &paths,
            no_real_ports(),
            SimWorld::new(),
            ui(),
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").starts_with("could not read the script "),
            "{error:#}"
        );
    }
}
