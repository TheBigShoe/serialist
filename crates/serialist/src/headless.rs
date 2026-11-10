//! `--script`: run one Lua script against a port with no window, then exit.
//!
//! The port comes from `--port` (a real path, `virtual:<NAME>` for a simulated device,
//! which turns the simulator on as it does for the window, `tcp:<HOST>:<PORT>` or
//! `replay:<FILE>`; the `replay` setting applies as in the window). Its line settings
//! are the GUI's: the device profile that matches the port over `default_baud`, with
//! `--baud` over both. The script runs through
//! [`run_headless`](serialist_script::run_headless) with standard streams for its
//! output and prompts; `require` reads from the script's own folder. The plugins in the
//! config directory's `plugins/` load as they do for the window, for
//! `codecs.<name>.encode`, and the port decodes with the plugin its device profile names
//! (`plugin`), for `port:on_frame`. `main` exits with [`exit_code`] of the outcome.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use serialist_core::settings::ConfigPaths;
use serialist_core::{PortId, PortInfo, PortKind, Settings, load_settings};
use serialist_script::{HeadlessCodecs, ScriptOutcome, ScriptSource, ScriptUi, run_headless_with};
use serialist_sim::SimWorld;
use serialist_ui::AppOptions;
use serialist_ui::codecs::CodecSet;

use crate::cli::Args;
use crate::wiring::{self, Backend};

/// The process's exit status for a run that ended with `outcome`: 0 when the script
/// returned, 1 on an error, 130 if it was stopped. 2 is left for "nothing ran" (a bad
/// flag, a missing `--port`, an unreadable script), and 130 is what a shell reports for
/// a process ended by Ctrl-C, so a clean stop on Ctrl-C would report the same.
pub fn exit_code(outcome: &ScriptOutcome) -> i32 {
    match outcome {
        ScriptOutcome::Ok => 0,
        ScriptOutcome::Error(_) => 1,
        ScriptOutcome::Stopped => 130,
    }
}

/// What a headless run opens its port through: the options for `args` over `real` and
/// `world` (see [`wiring::build`]) and the settings under `paths`, which are the defaults
/// when a file does not parse; a key whose value this build cannot read is left out with
/// a logged warning and the rest of the file applies. The `replay` setting is applied here, so a `replay:` port or a
/// script's `serial.open` plays at the speed and end the settings name (an id's own
/// `?speed=` and `?end=` still win), as it does in the window.
pub(crate) fn configured_options(
    args: &Args,
    paths: &ConfigPaths,
    real: Backend,
    world: SimWorld,
) -> anyhow::Result<(AppOptions, Settings)> {
    let options = wiring::build(args, real, world)?;
    let settings = match load_settings(Some(&paths.settings), paths.project_settings.as_deref()) {
        Ok(settings) => {
            // An unknown key, or a value this build cannot read: that key alone is left
            // out and the rest of the file applies, as in the window.
            for warning in &settings.warnings {
                tracing::warn!("{warning}");
            }
            settings
        }
        Err(error) => {
            tracing::warn!(%error, "settings not applied; using the defaults");
            Settings::default()
        }
    };
    if let Some(replay) = &options.replay {
        replay.set_defaults(settings.replay.options());
    }
    Ok((options, settings))
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
        [] => bail!(
            "--script needs --port <ID> (a device path, virtual:<NAME> for a simulated device, \
             tcp:<HOST>:<PORT> or replay:<FILE>)"
        ),
        _ => bail!("--script runs against one --port, not {}", args.ports.len()),
    };
    let source = ScriptSource::from_file(script)
        .with_context(|| format!("could not read the script {}", script.display()))?;
    let (options, settings) = configured_options(args, paths, real, world)?;
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
    let plugin = settings
        .profile_for(&info)
        .and_then(|profile| profile.plugin.clone());
    let mut serial = settings.serial_config_for(&info);
    if let Some(baud) = args.baud {
        serial.baud = baud;
    }
    let (codecs, problems) = CodecSet::load(&paths.plugins_dir());
    for problem in problems {
        tracing::warn!("{problem}");
    }
    let registry = codecs.registry().clone();
    let decode = plugin.and_then(|name| {
        let factory = registry.get(&name);
        if factory.is_none() {
            tracing::warn!(
                "the device profile names the plugin {name}, which is not installed; \
                 nothing is decoded"
            );
        }
        factory
    });
    tracing::info!(%port, serial = %serial.summary(), script = %source.name, "running a script");
    Ok(run_headless_with(
        port,
        serial,
        options.transport_factory,
        source,
        ui,
        HeadlessCodecs {
            source: Some(registry),
            decode,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    use serialist_core::{ReplayEnd, ReplayOptions, ReplaySpeed};
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

    /// Serves one TCP connection on 127.0.0.1: answers `AT\r\n` with `OK\r\n`, then
    /// waits for the client to hang up. Returns the `tcp:` port id and the server thread.
    fn at_over_tcp() -> (String, std::thread::JoinHandle<()>) {
        use std::io::Read;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let id = format!("tcp:127.0.0.1:{}", listener.local_addr().unwrap().port());
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while !line.ends_with(b"\r\n") {
                stream.read_exact(&mut byte).unwrap();
                line.push(byte[0]);
            }
            assert_eq!(line, b"AT\r\n");
            stream.write_all(b"OK\r\n").unwrap();
            // Until the script's session closes the connection.
            let _ = stream.read(&mut byte);
        });
        (id, server)
    }

    #[test]
    fn a_script_runs_against_a_tcp_port() {
        let (id, server) = at_over_tcp();
        let dir = TempDir::new("tcp");
        let (outcome, out) = run_script(
            &dir,
            &["--port", &id],
            "local port = assert(serial.current())\nport:write('AT\\r\\n')\n\
             assert(port:expect('^OK$'), 'no OK')\nprint(port:description())\n",
        );
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(out, format!("{id}\n"));
        server.join().unwrap();
    }

    #[test]
    fn serial_open_reaches_a_tcp_port_with_no_scripting_changes() {
        let (id, server) = at_over_tcp();
        let dir = TempDir::new("tcp-open");
        let code = format!(
            "local port = assert(serial.open{{ port = '{id}' }})\n\
             port:write('AT\\r\\n')\n\
             assert(port:expect('^OK$'), 'no OK')\nport:close()\nprint('ok')\n"
        );
        let (outcome, out) = run_script(&dir, &["--port", "virtual:at"], &code);
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(out, "ok\n");
        server.join().unwrap();
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
        assert_eq!(exit_code(&ScriptOutcome::Stopped), 130);
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
        let (_, out) = run_script(&dir, &["--port", "virtual:at", "--baud", "12000000"], code);
        assert_eq!(out, "virtual:at @ 12000000 8N1\n", "the top of the list");
    }

    /// Install the bundled Airoha RACE Lua plugin into `dir`'s config directory.
    fn install_race_plugin(dir: &TempDir) {
        let paths = ConfigPaths::new(dir.0.join("config"));
        let example = CodecSet::default()
            .examples_to_install()
            .into_iter()
            .find(|example| example.name == "airoha-race")
            .expect("the bundled RACE example");
        paths.install_example_plugin(example).unwrap();
    }

    #[test]
    fn a_script_encodes_with_an_installed_plugin() {
        let dir = TempDir::new("encode");
        let code = "print(codecs['airoha-race'] ~= nil)\n";
        let (outcome, out) = run_script(&dir, &["--port", "virtual:race"], code);
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(out, "false\n", "nothing installed yet");

        install_race_plugin(&dir);
        let code = "local race = assert(codecs['airoha-race'])\n\
                    print(hex.encode(race.encode{ cmd_id = '0x0F15' }))\n\
                    local ok, err = pcall(race.encode, {})\n\
                    print(tostring(err):match('codecs%[\"airoha%-race\"%]%.encode: [^\\n]*'))\n";
        let (outcome, out) = run_script(&dir, &["--port", "virtual:race"], code);
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(
            out,
            "05 5A 02 00 15 0F\ncodecs[\"airoha-race\"].encode: missing field `cmd_id`\n"
        );
    }

    #[test]
    fn the_profile_s_plugin_decodes_for_on_frame() {
        let dir = TempDir::new("frames");
        let code = "local p = serial.current()\n\
                    local got\n\
                    local h <close> = p:on_frame({ kind = 'response' }, function(f) got = f end)\n\
                    p:write(codecs['airoha-race'].encode{ cmd_id = 0x0F15 })\n\
                    for _ = 1, 200 do if got then break end sleep(10) end\n\
                    print(got and string.format('%s 0x%04X', got.kind, got.fields.cmd_id))\n";
        // Installed but not named by a profile: nothing decodes.
        install_race_plugin(&dir);
        let (outcome, _) = run_script(&dir, &["--port", "virtual:race"], code);
        let ScriptOutcome::Error(message) = &outcome else {
            panic!("{outcome:?}");
        };
        assert!(message.contains("decodes no frames here"), "{message}");

        dir.write(
            "config/settings.json",
            r#"{ "devices": [ { "match": { "path": "virtual:race" }, "plugin": "airoha-race" } ] }"#,
        );
        let (outcome, out) = run_script(&dir, &["--port", "virtual:race"], code);
        assert_eq!(outcome, ScriptOutcome::Ok, "{out}");
        assert_eq!(out, "response 0x0F15\n");
    }

    /// What `run` opens its port through, for `settings` in a config directory.
    fn configured(dir: &TempDir, settings: Option<&str>) -> (AppOptions, Settings) {
        std::fs::create_dir_all(dir.0.join("config")).unwrap();
        if let Some(settings) = settings {
            dir.write("config/settings.json", settings);
        }
        let paths = ConfigPaths::new(dir.0.join("config"));
        let args = args(&["--port", "tcp:127.0.0.1:9"]);
        configured_options(&args, &paths, no_real_ports(), SimWorld::new()).unwrap()
    }

    fn replay_defaults(options: &AppOptions) -> ReplayOptions {
        options
            .replay
            .as_ref()
            .expect("the replay factory")
            .defaults()
    }

    #[test]
    fn the_replay_setting_reaches_the_factory_a_run_opens_through() {
        let dir = TempDir::new("replay-setting");
        let (options, settings) = configured(
            &dir,
            Some(r#"{ "replay": { "speed": "max", "end": "hold" } }"#),
        );
        let chosen = ReplayOptions {
            speed: ReplaySpeed::Max,
            end: ReplayEnd::Hold,
        };
        assert_eq!(settings.replay.options(), chosen);
        assert_eq!(replay_defaults(&options), chosen);

        // One key of the object leaves the other at the bundled default.
        let dir = TempDir::new("replay-one-key");
        let (options, _) = configured(&dir, Some(r#"{ "replay": { "speed": 4 } }"#));
        assert_eq!(
            replay_defaults(&options),
            ReplayOptions {
                speed: ReplaySpeed::Times(4.0),
                end: ReplayEnd::Disconnect,
            }
        );

        // No settings file, and one that does not parse, both play at 1x and disconnect.
        let dir = TempDir::new("replay-no-settings");
        let (options, _) = configured(&dir, None);
        assert_eq!(replay_defaults(&options), ReplayOptions::default());
        let dir = TempDir::new("replay-broken-settings");
        let (options, settings) = configured(&dir, Some(r#"{ "replay": { "speed": "#));
        assert_eq!(replay_defaults(&options), ReplayOptions::default());
        assert_eq!(settings, Settings::default(), "the defaults stand in");
    }

    #[test]
    fn one_bad_value_leaves_the_rest_of_the_file_in_force() {
        let dir = TempDir::new("one-bad-value");
        let (options, settings) = configured(
            &dir,
            Some(
                r#"{ "replay": { "speed": "warp", "end": "hold" },
                     "line_ending": "crcrlf", "default_baud": 9600,
                     "devices": [ { "match": { "path": "tcp:" }, "parity": "sometimes",
                                    "baud": 57600 } ] }"#,
            ),
        );
        assert_eq!(
            replay_defaults(&options),
            ReplayOptions {
                speed: ReplayOptions::default().speed,
                end: ReplayEnd::Hold,
            },
            "the speed this build cannot read is left out; the end applies"
        );
        assert_eq!(settings.line_ending, Settings::default().line_ending);
        assert_eq!(settings.default_baud, 9600);
        assert_eq!(settings.devices.len(), 1);
        assert_eq!(settings.devices[0].baud, Some(57600));
        assert_eq!(settings.devices[0].parity, None);
        let mut keys: Vec<&str> = settings.warnings.iter().map(|w| w.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["devices[0].parity", "line_ending", "replay.speed"]);
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
            "--script needs --port <ID> (a device path, virtual:<NAME> for a simulated device, \
             tcp:<HOST>:<PORT> or replay:<FILE>)"
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
