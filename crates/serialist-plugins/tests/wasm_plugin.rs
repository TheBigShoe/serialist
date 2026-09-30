//! The WebAssembly adapter: the plugin contract, the sandbox, the limits, discovery, and
//! the committed plugin builds. Needs the `wasm` feature:
//!
//! ```text
//! cargo test -p serialist-plugins --features wasm
//! ```
//!
//! The plugins run from `tests/fixtures`, committed builds of
//! `examples/plugins/airoha-race-wasm` and `tests/plugins/lines-wasm`, so no wasm
//! toolchain is needed. With the `wasm32-wasip2` target installed,
//! [`the_committed_plugins_match_their_sources`] rebuilds them and checks the copies are
//! current; `just wasm-fixtures` refreshes them.

#![cfg(feature = "wasm")]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::json;
use serialist_core::{Codec, CodecError, CodecFactory, EncodeRequest, Frame, Severity, Value};
use serialist_plugins::race::race_info;
use serialist_plugins::wasm::PluginManifest;
use serialist_plugins::{
    LuaLimits, PLUGIN_ERROR_KIND, PluginKind, WasmCodec, WasmCodecFactory, WasmEngine, WasmLimits,
    find_plugins, load_plugins,
};

use common::{TempDir, decode_chunks, fixtures, wasm_race, wasm_race_factory};

fn limits() -> WasmLimits {
    WasmLimits {
        memory_bytes: 16 * 1024 * 1024,
        ..WasmLimits::default()
    }
}

fn lines_dir() -> PathBuf {
    fixtures().join("wasm/lines-wasm")
}

fn lines_factory() -> WasmCodecFactory {
    static FACTORY: OnceLock<WasmCodecFactory> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            WasmCodecFactory::load_dir_with(lines_dir(), &WasmEngine::shared().unwrap(), limits())
                .expect("the test plugin loads")
        })
        .clone()
}

fn lines() -> WasmCodec {
    lines_factory()
        .create_wasm()
        .expect("the test plugin starts")
}

fn spans(frames: &[Frame]) -> Vec<(&str, std::ops::Range<u64>)> {
    frames
        .iter()
        .map(|f| (f.kind.as_str(), f.raw.clone()))
        .collect()
}

fn error_of(frame: &Frame) -> &str {
    frame
        .field("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

#[test]
fn the_race_plugin_loads_from_its_folder_with_its_manifest() {
    let factory = wasm_race_factory();
    assert_eq!(factory.info(), race_info());
    let manifest = factory.manifest().expect("loaded from a folder");
    assert_eq!(
        (
            manifest.name.as_str(),
            manifest.version.as_str(),
            manifest.api.as_str()
        ),
        ("airoha-race", "1.0.0", "1")
    );
    assert!(
        factory
            .path()
            .unwrap()
            .ends_with("airoha-race-wasm/plugin.wasm")
    );
    let mut codec = factory.create().unwrap();
    let mut out = Vec::new();
    codec.decode(
        &[0x05, 0x5B, 0x02, 0x00, 0x15, 0x0F],
        Instant::now(),
        7,
        &mut out,
    );
    assert_eq!(spans(&out), [("response", 7..13)]);
    assert_eq!(out[0].summary, "response 0x0F15 len 0");
}

#[test]
fn held_back_bytes_are_prepended_and_offsets_stay_global() {
    let mut codec = lines();
    let frames = decode_chunks(&mut codec, &[b"ab", b"c\nde", b"f\ng"], Instant::now());
    assert_eq!(spans(&frames), [("line", 0..4), ("line", 4..8)]);
    assert_eq!(codec.held_back(), 1);
    let first = &frames[0];
    assert_eq!(first.summary, "abc");
    // Declared fields in declared order, then the others in the plugin's order.
    assert_eq!(
        first.fields,
        [
            ("text".into(), Value::Str("abc".into())),
            ("n".into(), Value::UInt(3)),
            ("calls".into(), Value::Int(2)),
        ]
    );
    assert_eq!(first.severity, Severity::Info);
}

#[test]
fn nested_lists_come_through_whole() {
    let frames = decode_chunks(&mut lines(), &[b"list\n"], Instant::now());
    assert_eq!(
        frames[0].field("nested"),
        Some(&Value::List(vec![
            Value::Int(1),
            Value::List(vec![Value::Str("a".into())]),
            Value::Bool(true),
        ]))
    );
}

#[test]
fn a_runaway_decode_becomes_a_plugin_error_and_decoding_goes_on() {
    for trap in ["loop", "panic", "grow", "deep"] {
        let mut codec = lines();
        let started = Instant::now();
        let input = format!("ok\n{trap}\n");
        let frames = decode_chunks(&mut codec, &[input.as_bytes(), b"after\n"], started);
        let end = input.len() as u64;
        assert_eq!(
            spans(&frames),
            [(PLUGIN_ERROR_KIND, 0..end), ("line", end..end + 6)],
            "{trap}"
        );
        let error = &frames[0];
        assert_eq!(error.severity, Severity::Error);
        let message = error_of(error);
        let expected = match trap {
            "loop" => "ran longer than the 50 ms a call may take",
            // The SDK's panic handler logs the message, and the host shows it.
            "panic" => "panicked: boom at offset 3 (crates/serialist-plugins/tests/plugins",
            "grow" => "ran out of memory: it tried to grow to",
            _ => "overflowed its stack",
        };
        assert!(message.contains(expected), "{trap}: {message}");
        assert_eq!(error.summary, format!("plugin error: {message}"), "{trap}");
        // A fresh instance: the plugin's state starts over.
        assert_eq!(frames[1].field("calls"), Some(&Value::Int(1)), "{trap}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{trap} stalled"
        );
    }
}

#[test]
fn the_time_limit_is_the_codecs() {
    let factory = WasmCodecFactory::load_dir_with(
        lines_dir(),
        &WasmEngine::new().unwrap(),
        WasmLimits {
            time_per_call: Duration::from_millis(20),
            ..limits()
        },
    )
    .unwrap();
    let mut codec = factory.create_wasm().unwrap();
    let started = Instant::now();
    let frames = decode_chunks(&mut codec, &[b"loop\n"], started);
    let took = started.elapsed();
    assert!(error_of(&frames[0]).contains("20 ms"), "{frames:?}");
    assert!(took >= Duration::from_millis(20), "{took:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
}

#[test]
fn frames_that_break_the_contract_are_reported_not_trusted() {
    let mut codec = lines();
    let frames = decode_chunks(
        &mut codec,
        &[b"badtype\nmissing\noutside\nwarn\n"],
        Instant::now(),
    );
    let got: Vec<_> = frames
        .iter()
        .map(|f| (f.kind.as_str(), f.raw.clone(), error_of(f).to_owned()))
        .collect();
    assert_eq!(got[0].0, PLUGIN_ERROR_KIND);
    assert_eq!(got[0].1, 0..8, "a bad frame's error covers that frame");
    assert!(
        got[0]
            .2
            .contains("field `n` is of type str but is declared uint"),
        "{got:?}"
    );
    assert_eq!(got[1].1, 8..16);
    assert!(
        got[1].2.contains("field `n` is declared but missing"),
        "{got:?}"
    );
    // A frame outside the input spoils the rest of the call.
    assert_eq!(got[2].1, 0..29);
    assert!(got[2].2.contains("fall outside"), "{got:?}");
    assert_eq!(got.len(), 3);

    // Holding back more than it was given.
    let mut codec = lines();
    let frames = decode_chunks(&mut codec, &[b"x\n#"], Instant::now());
    assert_eq!(spans(&frames), [("line", 0..2), (PLUGIN_ERROR_KIND, 0..3)]);
    assert_eq!(codec.held_back(), 0);

    let frames = decode_chunks(&mut lines(), &[b"warn\n", b"chatty\n"], Instant::now());
    assert_eq!(frames[0].severity, Severity::Warning);
    assert_eq!(
        frames[1].kind, "line",
        "a chatty plugin is capped, not failed"
    );
}

#[test]
fn holding_back_past_the_limit_is_an_error_for_those_bytes() {
    let factory = WasmCodecFactory::load_dir_with(
        lines_dir(),
        &WasmEngine::shared().unwrap(),
        WasmLimits {
            max_held_back: 4,
            ..limits()
        },
    )
    .unwrap();
    let mut codec = factory.create_wasm().unwrap();
    let frames = decode_chunks(&mut codec, &[b"a\nbcdefg"], Instant::now());
    assert_eq!(spans(&frames), [("line", 0..2), (PLUGIN_ERROR_KIND, 2..8)]);
    assert!(error_of(&frames[1]).contains("more than the limit of 4"));
    assert_eq!(codec.held_back(), 0);
}

#[test]
fn encode_maps_results_and_errors() {
    let mut codec = lines();
    let say = |fields: serde_json::Value| EncodeRequest {
        command: "say".into(),
        fields: fields.as_object().cloned().unwrap(),
    };
    assert_eq!(
        codec.encode(&say(json!({ "text": "hi" }))),
        Ok(b"hi\n".to_vec())
    );
    assert_eq!(
        codec.encode(&say(json!({}))),
        Err(CodecError::MissingField("text".into()))
    );
    assert_eq!(
        codec.encode(&say(json!({ "text": 1 }))),
        Err(CodecError::bad_field("text", "must be a string"))
    );
    assert_eq!(
        codec.encode(&EncodeRequest::new("nope")),
        Err(CodecError::UnknownCommand("nope".into()))
    );
    assert_eq!(
        codec.encode(&EncodeRequest::new("fail")),
        Err(CodecError::Internal("plain message".into()))
    );
    assert!(matches!(
        codec.encode(&EncodeRequest::new("panic")),
        Err(CodecError::Internal(message)) if message.contains("raised here")
    ));
    // The trap cost the instance; the next call gets a fresh one.
    assert_eq!(
        codec.encode(&say(json!({ "text": "again" }))),
        Ok(b"again\n".to_vec())
    );
    let types = EncodeRequest {
        command: "types".into(),
        fields: json!({
            "a": null, "b": [1, 2], "c": { "d": 1 }, "e": 1.5, "f": 2, "g": [],
            "h": u64::MAX, "i": -3, "j": "s", "k": true
        })
        .as_object()
        .cloned()
        .unwrap(),
    };
    assert_eq!(codec.encode(&types), Ok(b"typed".to_vec()));
}

#[test]
fn reset_forgets_held_bytes_and_state() {
    let mut codec = lines();
    let at = Instant::now();
    let mut out = Vec::new();
    codec.decode(b"a\nb", at, 0, &mut out);
    codec.reset();
    assert_eq!(codec.held_back(), 0);
    codec.decode(b"c\n", at, 3, &mut out);
    assert_eq!(spans(&out), [("line", 0..2), ("line", 3..5)]);
    assert_eq!(out[1].field("calls"), Some(&Value::Int(1)));
    assert_eq!(out[1].summary, "c");
}

#[test]
fn a_wasm_codec_moves_to_the_ingest_thread() {
    let mut codec = wasm_race_factory().create().unwrap();
    let frames = std::thread::spawn(move || {
        let mut out = Vec::new();
        codec.decode(b"hello\n", Instant::now(), 0, &mut out);
        out
    })
    .join()
    .unwrap();
    assert_eq!(frames[0].field("text"), Some(&Value::Str("hello".into())));
}

/// A plugin folder under `dir` holding the RACE component and `manifest`.
fn race_folder(dir: &TempDir, name: &str, manifest: Option<&str>) {
    let wasm = fs::read(fixtures().join("plugins/airoha-race-wasm/plugin.wasm")).unwrap();
    let folder = dir.path().join(name);
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("plugin.wasm"), wasm).unwrap();
    if let Some(manifest) = manifest {
        fs::write(folder.join("plugin.toml"), manifest).unwrap();
    }
}

#[test]
fn plugin_folders_load_and_bad_ones_say_why() {
    let dir = TempDir::new("wasm-plugins");
    let good = "name = \"airoha-race\"\nversion = \"1.0.0\"\napi = \"1\"\n";
    race_folder(&dir, "a-good", Some(good));
    race_folder(&dir, "b-future", Some(&good.replace("\"1\"\n", "\"2\"\n")));
    race_folder(
        &dir,
        "c-renamed",
        Some(&good.replace("airoha-race", "other")),
    );
    race_folder(&dir, "d-no-manifest", None);
    dir.write("e-not-a-component/plugin.wasm", "\0asm");
    dir.write("e-not-a-component/plugin.toml", good);
    let found = find_plugins(dir.path());
    assert!(found.iter().all(|p| p.kind == PluginKind::Wasm));

    let mut registry = serialist_core::CodecRegistry::new();
    let warnings = load_plugins(dir.path(), &mut registry, LuaLimits::default());
    let warned: Vec<_> = warnings
        .iter()
        .map(|w| {
            let folder = w.path.parent().unwrap().file_name().unwrap();
            (folder.to_str().unwrap().to_owned(), w.message.clone())
        })
        .collect();
    let expect = [
        ("b-future", "api = \"2\""),
        ("c-renamed", "plugin.toml says other 1.0.0"),
        ("d-no-manifest", "plugin.toml"),
        ("e-not-a-component", "not a WebAssembly component"),
    ];
    assert_eq!(warned.len(), expect.len(), "{warned:#?}");
    for ((folder, message), (want_folder, hint)) in warned.iter().zip(expect) {
        assert_eq!(folder, want_folder);
        assert!(message.contains(hint), "{folder}: {message}");
        assert!(
            message.contains("WebAssembly plugin"),
            "{folder}: {message}"
        );
    }
    assert_eq!(registry.names().collect::<Vec<_>>(), ["airoha-race"]);
    let mut codec = registry.create("airoha-race").unwrap();
    let mut out = Vec::new();
    codec.decode(b"hi\n", Instant::now(), 0, &mut out);
    assert_eq!(out[0].kind, "text");
}

#[test]
fn components_that_are_not_plugins_are_refused_at_load() {
    let engine = WasmEngine::shared().unwrap();
    let cases = [
        (
            r#"(component (import "wasi:cli/stderr@0.2.3" (instance)))"#,
            "imports wasi:cli/stderr@0.2.3, but a plugin gets only `log`",
        ),
        (
            "(component)",
            "does not export the serialist:codec/plugin world",
        ),
        ("(module)", "not a WebAssembly component"),
    ];
    for (wat, hint) in cases {
        let bytes = wat::parse_str(wat).unwrap();
        match WasmCodecFactory::from_bytes("bad", &bytes, &engine, limits()) {
            Err(CodecError::Internal(message)) => {
                assert!(message.contains(hint), "{wat}: {message}");
                assert!(message.starts_with("WebAssembly plugin bad: "), "{message}");
            }
            other => panic!("{wat}: {other:?}"),
        }
    }
}

#[test]
fn the_race_plugin_and_the_manifest_in_the_example_agree() {
    let example = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/plugins/airoha-race-wasm/plugin.toml");
    let manifest = PluginManifest::parse(&fs::read_to_string(example).unwrap()).unwrap();
    assert_eq!(Some(&manifest), wasm_race_factory().manifest());
    let _ = wasm_race();
}

// The committed builds --------------------------------------------------------------

/// A guest plugin crate and where its build is committed under `tests/fixtures`.
struct Guest {
    package: &'static str,
    /// The crate's folder, from the workspace root.
    source: &'static str,
    fixture: &'static str,
}

const GUESTS: [Guest; 2] = [
    Guest {
        package: "airoha-race-wasm",
        source: "examples/plugins/airoha-race-wasm",
        fixture: "plugins/airoha-race-wasm",
    },
    Guest {
        package: "lines-wasm",
        source: "crates/serialist-plugins/tests/plugins/lines-wasm",
        fixture: "wasm/lines-wasm",
    },
];

const TARGET: &str = "wasm32-wasip2";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

/// Build every guest for wasm32-wasip2 with the `wasm-plugin` profile, as
/// `just wasm-fixtures` does, into `target/wasm-plugins`. Paths are remapped so the
/// bytes do not depend on where the checkout or the cargo home is. `Err` says why it
/// could not (no wasm target installed, most often).
fn build_guests() -> Result<Vec<Vec<u8>>, String> {
    let root = workspace_root();
    let libdir = Command::new("rustc")
        .args(["--print", "target-libdir", "--target", TARGET])
        .current_dir(&root)
        .output()
        .map_err(|err| format!("rustc did not run: {err}"))?;
    let libdir = PathBuf::from(String::from_utf8_lossy(&libdir.stdout).trim());
    if !libdir.is_dir() {
        return Err(format!(
            "the {TARGET} target is not installed (rustup target add {TARGET})"
        ));
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
        .ok_or("no cargo home")?;
    let remap = [
        format!("--remap-path-prefix={}=/cargo", cargo_home.display()),
        format!("--remap-path-prefix={}=/serialist", root.display()),
    ]
    .join("\x1f");
    let target_dir = root.join("target/wasm-plugins");
    let mut cargo = Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()));
    cargo
        .current_dir(&root)
        .args([
            "build",
            "--locked",
            "--target",
            TARGET,
            "--profile",
            "wasm-plugin",
        ])
        .arg("--target-dir")
        .arg(&target_dir)
        .env("CARGO_ENCODED_RUSTFLAGS", remap)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_BUILD_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR");
    for guest in &GUESTS {
        cargo.args(["-p", guest.package]);
    }
    let output = cargo
        .output()
        .map_err(|err| format!("cargo did not run: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "the guest build failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    GUESTS
        .iter()
        .map(|guest| {
            let file = format!("{}.wasm", guest.package.replace('-', "_"));
            let path = target_dir.join(TARGET).join("wasm-plugin").join(file);
            fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))
        })
        .collect()
}

/// A short fingerprint for messages: length and FNV-1a hash.
fn fingerprint(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{} bytes, fnv {hash:016x}", bytes.len())
}

/// The fixtures are the plugins built from the current sources. Skipped (with a message)
/// without the wasm32-wasip2 target. With `SERIALIST_BLESS_WASM=1` it writes them instead.
#[test]
fn the_committed_plugins_match_their_sources() {
    let built = match build_guests() {
        Ok(built) => built,
        Err(why) => {
            eprintln!("skipped: the committed WebAssembly plugins were not rebuilt: {why}");
            return;
        }
    };
    let bless = std::env::var_os("SERIALIST_BLESS_WASM").is_some();
    let root = workspace_root();
    for (guest, wasm) in GUESTS.iter().zip(built) {
        let fixture = fixtures().join(guest.fixture);
        let manifest = fs::read(root.join(guest.source).join("plugin.toml")).unwrap();
        if bless {
            fs::create_dir_all(&fixture).unwrap();
            fs::write(fixture.join("plugin.wasm"), &wasm).unwrap();
            fs::write(fixture.join("plugin.toml"), &manifest).unwrap();
            continue;
        }
        let committed = fs::read(fixture.join("plugin.wasm")).unwrap_or_default();
        assert!(
            committed == wasm,
            "{}: tests/fixtures/{}/plugin.wasm ({}) is not what the source builds ({}); \
             run `just wasm-fixtures`",
            guest.package,
            guest.fixture,
            fingerprint(&committed),
            fingerprint(&wasm),
        );
        assert_eq!(
            fs::read(fixture.join("plugin.toml")).unwrap_or_default(),
            manifest,
            "{}: the committed plugin.toml differs from the source's; run `just wasm-fixtures`",
            guest.package
        );
    }
}

#[test]
fn factories_share_one_compile_and_codecs_share_nothing() {
    let factory = lines_factory();
    let (mut a, mut b) = (
        factory.create_wasm().unwrap(),
        factory.create_wasm().unwrap(),
    );
    let at = Instant::now();
    let mut out = Vec::new();
    a.decode(b"x", at, 0, &mut out);
    b.decode(b"y\n", at, 0, &mut out);
    assert_eq!(a.held_back(), 1);
    assert_eq!(out[0].summary, "y", "b never saw a's held byte");
    let registry = {
        let mut registry = serialist_core::CodecRegistry::new();
        registry.register(Arc::new(factory));
        registry
    };
    assert!(registry.create("lines").is_ok());
}
