//! Runs the built `serialist` binary on the paths that end before any window opens: the
//! informational flags, bad usage, and `--script` runs against a simulated device. None
//! of them touches a display or a serial port, and each child gets its own config
//! directory so the developer's own settings are never read.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a child may run before the test fails instead of hanging.
const TIMEOUT: Duration = Duration::from_secs(60);

/// A scratch directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "serialist-cli-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
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

/// What a run produced.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs the binary with `args` and a config directory of its own, killing it if it is
/// still going after [`TIMEOUT`].
fn serialist(dir: &TempDir, args: &[&str]) -> Run {
    let config = dir.path().join("config");
    let mut child = Command::new(env!("CARGO_BIN_EXE_serialist"))
        .arg("--config-dir")
        .arg(&config)
        .args(args)
        .env_remove("SERIALIST_CONFIG_DIR")
        .env_remove("RUST_LOG")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary starts");
    // Read the pipes on threads so a chatty child cannot fill one and stall.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut out_pipe, &mut bytes).unwrap();
        bytes
    });
    let err = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut err_pipe, &mut bytes).unwrap();
        bytes
    });
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if start.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("serialist {args:?} did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = Output {
        status,
        stdout: out.join().unwrap(),
        stderr: err.join().unwrap(),
    };
    Run {
        code: output
            .status
            .code()
            .expect("exited, not killed by a signal"),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

#[test]
fn version_prints_the_name_and_number() {
    let dir = TempDir::new("version");
    let run = serialist(&dir, &["--version"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(
        run.stdout,
        format!("serialist {}\n", env!("CARGO_PKG_VERSION"))
    );
    let short = serialist(&dir, &["-V"]);
    assert_eq!(short.stdout, run.stdout);
}

#[test]
fn help_prints_the_usage_on_stdout() {
    let dir = TempDir::new("help");
    let run = serialist(&dir, &["--help"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert!(run.stdout.starts_with("Usage: serialist"), "{}", run.stdout);
    assert!(run.stdout.contains("--script <PATH>"), "{}", run.stdout);
    assert!(run.stderr.is_empty(), "{}", run.stderr);
}

#[test]
fn an_unknown_flag_exits_two_with_the_usage_on_stderr() {
    let dir = TempDir::new("unknown-flag");
    let run = serialist(&dir, &["--no-such-flag"]);
    assert_eq!(run.code, 2);
    assert!(run.stderr.starts_with("serialist: "), "{}", run.stderr);
    assert!(run.stderr.contains("Usage: serialist"), "{}", run.stderr);
    assert!(run.stdout.is_empty(), "{}", run.stdout);
}

#[test]
fn a_script_that_cannot_run_exits_two() {
    let dir = TempDir::new("no-run");
    let script = dir.write("ok.lua", "print('hi')\n");
    let script = script.to_str().unwrap();
    let missing = dir.path().join("missing.lua");
    let missing = missing.to_str().unwrap();

    let cases: [(&str, Vec<&str>); 4] = [
        ("no --port", vec!["--script", script]),
        (
            "two --port",
            vec![
                "--port",
                "virtual:at",
                "--port",
                "virtual:echo",
                "--script",
                script,
            ],
        ),
        (
            "an unreadable script",
            vec!["--port", "virtual:at", "--script", missing],
        ),
        (
            "an unknown simulated device",
            vec!["--port", "virtual:no-such-device", "--script", script],
        ),
    ];
    for (what, args) in cases {
        let run = serialist(&dir, &args);
        assert_eq!(run.code, 2, "{what}: {}", run.stderr);
        assert!(
            run.stderr.starts_with("serialist: "),
            "{what}: {}",
            run.stderr
        );
        assert!(run.stdout.is_empty(), "{what}: {}", run.stdout);
    }
}

#[test]
fn a_script_against_a_simulated_device_prints_only_its_output() {
    let dir = TempDir::new("script-ok");
    let script = dir.write(
        "probe.lua",
        "local port = assert(serial.current())\n\
         port:write('AT\\r\\n')\n\
         assert(port:expect('^OK$'), 'no OK')\n\
         print('got OK')\n",
    );
    let run = serialist(
        &dir,
        &["--port", "virtual:at", "--script", script.to_str().unwrap()],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    assert_eq!(run.stdout, "got OK\n");
}

#[test]
fn a_script_that_raises_an_error_exits_one_and_says_so_on_stderr() {
    let dir = TempDir::new("script-error");
    let script = dir.write("boom.lua", "print('before')\nerror('boom')\n");
    let run = serialist(
        &dir,
        &["--port", "virtual:at", "--script", script.to_str().unwrap()],
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert_eq!(run.stdout, "before\n");
    assert!(run.stderr.contains("boom.lua:2: boom"), "{}", run.stderr);
}
