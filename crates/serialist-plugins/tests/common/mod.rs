//! Helpers shared by the plugin integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serialist_core::{Codec, Frame};
use serialist_plugins::{LuaCodec, LuaLimits, bundled_race_lua};

/// Feed `chunks` to `codec` as one stream starting at offset 0. Chunk `i` arrives at
/// `t0 + i ms`, so two codecs fed the same chunks see the same times.
pub fn decode_chunks(codec: &mut dyn Codec, chunks: &[&[u8]], t0: Instant) -> Vec<Frame> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    for (i, chunk) in chunks.iter().enumerate() {
        codec.decode(
            chunk,
            t0 + Duration::from_millis(i as u64),
            offset,
            &mut out,
        );
        offset += chunk.len() as u64;
    }
    out
}

/// The bundled Lua RACE plugin, loaded fresh.
pub fn lua_race() -> LuaCodec {
    bundled_race_lua(LuaLimits::default())
        .expect("the bundled plugin loads")
        .create_lua()
        .expect("the bundled plugin loads")
}

/// The committed build of the WebAssembly RACE plugin
/// (`examples/plugins/airoha-race-wasm`), compiled once per test binary.
#[cfg(feature = "wasm")]
pub fn wasm_race_factory() -> serialist_plugins::WasmCodecFactory {
    use std::sync::OnceLock;
    static FACTORY: OnceLock<serialist_plugins::WasmCodecFactory> = OnceLock::new();
    FACTORY
        .get_or_init(|| {
            serialist_plugins::WasmCodecFactory::load_dir(
                fixtures().join("plugins/airoha-race-wasm"),
            )
            .expect("the committed RACE plugin loads")
        })
        .clone()
}

/// The WebAssembly RACE plugin, instantiated fresh.
#[cfg(feature = "wasm")]
pub fn wasm_race() -> serialist_plugins::WasmCodec {
    wasm_race_factory()
        .create_wasm()
        .expect("the committed RACE plugin instantiates")
}

/// `tests/fixtures`.
pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Frames with every arrival time set to `t0`, for comparing differently cut streams.
pub fn timeless(frames: &[Frame], t0: Instant) -> Vec<Frame> {
    frames
        .iter()
        .cloned()
        .map(|mut f| {
            f.at = t0;
            f
        })
        .collect()
}

/// A directory under the system temp dir, removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "serialist-plugins-{label}-{}-{}-{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Writes `text` to `relative` under the directory, creating parents.
    pub fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(&path, text).expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
