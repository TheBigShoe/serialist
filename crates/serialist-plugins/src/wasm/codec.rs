//! [`WasmCodec`] and [`WasmCodecFactory`]: a component behind the [`Codec`] trait.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serialist_core::Severity;
use serialist_core::codec::{Codec, CodecError, CodecFactory, CodecInfo, EncodeRequest, Frame};
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{ResourceLimiter, Store, Trap};

use super::convert::{self, Schema};
use super::engine::{WasmEngine, deadline_ticks};
use super::manifest::PluginManifest;
use super::wit;
use crate::PLUGIN_ERROR_KIND;
use crate::dir::WASM_ENTRY;

/// What a WebAssembly plugin may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WasmLimits {
    /// Most linear memory an instance may grow to, in bytes. Growing past it traps.
    pub memory_bytes: usize,
    /// Wall time one call may run (instantiation, `describe`, one `decode`, `encode` or
    /// `reset`) before it is interrupted. Enforced to within [`TICK`](super::TICK).
    pub time_per_call: Duration,
    /// Most bytes `decode` may hold back; a plugin that holds back more gets a
    /// [`PLUGIN_ERROR_KIND`] frame for them instead.
    pub max_held_back: usize,
    /// Log lines one call may write; later ones are counted and dropped.
    pub log_lines_per_call: u32,
}

impl WasmLimits {
    pub const DEFAULT_MEMORY_BYTES: usize = 64 * 1024 * 1024;
    /// Generous: the reference RACE plugin decodes a 64 KiB chunk in well under 1 ms.
    pub const DEFAULT_TIME_PER_CALL: Duration = Duration::from_millis(50);
    pub const DEFAULT_MAX_HELD_BACK: usize = 1024 * 1024;
    pub const DEFAULT_LOG_LINES_PER_CALL: u32 = 64;
}

impl Default for WasmLimits {
    fn default() -> Self {
        Self {
            memory_bytes: Self::DEFAULT_MEMORY_BYTES,
            time_per_call: Self::DEFAULT_TIME_PER_CALL,
            max_held_back: Self::DEFAULT_MAX_HELD_BACK,
            log_lines_per_call: Self::DEFAULT_LOG_LINES_PER_CALL,
        }
    }
}

/// Longest log line kept, in bytes; the rest is cut.
const MAX_LOG_LINE: usize = 1024;

/// A store's data: its limiter and the log of the call running.
struct HostState {
    limiter: Limiter,
    log: CallLog,
}

impl HostState {
    fn new(plugin: &Arc<str>, limits: &WasmLimits) -> Self {
        Self {
            limiter: Limiter {
                memory_bytes: limits.memory_bytes,
                denied: None,
            },
            log: CallLog {
                plugin: Arc::clone(plugin),
                max_lines: limits.log_lines_per_call,
                lines: 0,
                dropped: 0,
                last_error: None,
            },
        }
    }
}

/// Caps linear memory, and remembers a refused growth so the trap can say why.
struct Limiter {
    memory_bytes: usize,
    denied: Option<usize>,
}

impl ResourceLimiter for Limiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if desired > self.memory_bytes || maximum.is_some_and(|max| desired > max) {
            self.denied = Some(desired);
            wasmtime::bail!("memory would grow to {desired} bytes");
        }
        Ok(true)
    }

    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        Ok(desired <= 1_000_000 && maximum.is_none_or(|max| desired <= max))
    }
}

/// `log` lines of the call running.
struct CallLog {
    plugin: Arc<str>,
    max_lines: u32,
    lines: u32,
    dropped: u32,
    /// The last `error` line, shown if the call traps.
    last_error: Option<String>,
}

impl CallLog {
    fn start(&mut self) {
        self.lines = 0;
        self.dropped = 0;
        self.last_error = None;
    }

    fn finish(&mut self) {
        if self.dropped > 0 {
            tracing::warn!(
                plugin = %self.plugin,
                dropped = self.dropped,
                "a WebAssembly plugin wrote more log lines than one call may"
            );
        }
    }
}

fn truncate(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut cut = max;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("...");
    }
    text
}

impl wit::PluginImports for HostState {
    fn log(&mut self, level: wit::LogLevel, message: String) {
        let log = &mut self.log;
        let message = truncate(message, MAX_LOG_LINE);
        if level == wit::LogLevel::Error {
            log.last_error = Some(message.clone());
        }
        if log.lines >= log.max_lines {
            log.dropped += 1;
            return;
        }
        log.lines += 1;
        let plugin = &*log.plugin;
        match level {
            wit::LogLevel::Debug => tracing::debug!(plugin, "{message}"),
            wit::LogLevel::Info => tracing::info!(plugin, "{message}"),
            wit::LogLevel::Warn => tracing::warn!(plugin, "{message}"),
            wit::LogLevel::Error => tracing::error!(plugin, "{message}"),
        }
    }
}

/// What every codec of one plugin shares: the compiled, linked component and what it
/// described.
struct Shared {
    engine: WasmEngine,
    pre: wit::PluginPre<HostState>,
    info: CodecInfo,
    schema: Schema,
    limits: WasmLimits,
    /// The plugin's file, or the name it was given.
    origin: String,
    path: Option<PathBuf>,
    manifest: Option<PluginManifest>,
}

/// One instance of the component in its own store.
struct Instance {
    store: Store<HostState>,
    plugin: wit::Plugin,
}

/// Why a call failed: what the trap or the limit was, after the plugin's last error line.
fn failure(err: &wasmtime::Error, state: &mut HostState, limits: &WasmLimits) -> String {
    let what = if let Some(bytes) = state.limiter.denied.take() {
        format!(
            "the plugin ran out of memory: it tried to grow to {} KiB, past its limit of {} KiB",
            bytes / 1024,
            limits.memory_bytes / 1024
        )
    } else {
        match err.downcast_ref::<Trap>() {
            Some(Trap::Interrupt) => format!(
                "the plugin ran longer than the {} ms a call may take",
                limits.time_per_call.as_millis()
            ),
            Some(Trap::StackOverflow) => "the plugin overflowed its stack".to_owned(),
            Some(trap) => format!("the plugin trapped: {trap}"),
            None => format!("{err:#}"),
        }
    };
    match state.log.last_error.take() {
        Some(message) => format!("{message}; {what}"),
        None => what,
    }
}

impl Shared {
    /// A fresh instance, under the time and memory limits like any call.
    fn instantiate(&self) -> Result<Instance, String> {
        let plugin_name: Arc<str> = Arc::from(self.info.name.as_str());
        let mut store = Store::new(
            self.engine.engine(),
            HostState::new(&plugin_name, &self.limits),
        );
        store.limiter(|state| &mut state.limiter);
        store.set_epoch_deadline(deadline_ticks(self.limits.time_per_call));
        let result = {
            let _busy = self.engine.busy();
            self.pre.instantiate(&mut store)
        };
        match result {
            Ok(plugin) => Ok(Instance { store, plugin }),
            Err(err) => Err(failure(&err, store.data_mut(), &self.limits)),
        }
    }
}

impl Instance {
    /// One call into the plugin under the limits.
    fn call<R>(
        &mut self,
        shared: &Shared,
        f: impl FnOnce(&wit::Plugin, &mut Store<HostState>) -> wasmtime::Result<R>,
    ) -> Result<R, String> {
        self.store
            .set_epoch_deadline(deadline_ticks(shared.limits.time_per_call));
        self.store.data_mut().log.start();
        let result = {
            let _busy = shared.engine.busy();
            f(&self.plugin, &mut self.store)
        };
        let state = self.store.data_mut();
        state.log.finish();
        result.map_err(|err| failure(&err, state, &shared.limits))
    }
}

/// A codec implemented by a WebAssembly component. Each has its own store and instance;
/// nothing is shared with other codecs of the same plugin but the compiled code.
///
/// A call that traps, runs out of memory or runs past its time does not stall ingest: the
/// bytes it was given become one [`PLUGIN_ERROR_KIND`] frame (severity error), and the
/// codec starts over from a fresh instance with nothing held back.
pub struct WasmCodec {
    shared: Arc<Shared>,
    /// `None` after a failure until the next call makes a new one.
    instance: Option<Instance>,
    /// Bytes `decode` held back, presented again before the next chunk.
    held: Vec<u8>,
    /// Scratch for joining `held` and a chunk.
    input: Vec<u8>,
}

impl fmt::Debug for WasmCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmCodec")
            .field("plugin", &self.shared.origin)
            .field("name", &self.shared.info.name)
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}

impl WasmCodec {
    fn new(shared: Arc<Shared>) -> Result<Self, CodecError> {
        let instance = shared
            .instantiate()
            .map_err(|err| CodecError::Internal(format!("{}: {err}", shared.origin)))?;
        Ok(Self {
            shared,
            instance: Some(instance),
            held: Vec::new(),
            input: Vec::new(),
        })
    }

    /// What the plugin described when it was loaded.
    pub fn info(&self) -> &CodecInfo {
        &self.shared.info
    }

    pub fn limits(&self) -> WasmLimits {
        self.shared.limits
    }

    /// Bytes held back for the next chunk.
    pub fn held_back(&self) -> usize {
        self.held.len()
    }

    /// One call into the plugin, making an instance first if the last one failed. After
    /// a failure the instance is dropped, since a trapped component may not be entered
    /// again.
    fn call<R>(
        &mut self,
        f: impl FnOnce(&wit::Plugin, &mut Store<HostState>) -> wasmtime::Result<R>,
    ) -> Result<R, String> {
        let instance = match &mut self.instance {
            Some(instance) => instance,
            slot => slot.insert(
                self.shared
                    .instantiate()
                    .map_err(|err| format!("the plugin could not start again: {err}"))?,
            ),
        };
        let result = instance.call(&self.shared, f);
        if result.is_err() {
            self.instance = None;
            self.held.clear();
        }
        result
    }

    fn fail(&self, raw: std::ops::Range<u64>, at: Instant, message: &str, out: &mut Vec<Frame>) {
        tracing::warn!(plugin = %self.shared.info.name, %message, "a WebAssembly codec failed");
        let first_line = message.lines().next().unwrap_or_default();
        out.push(
            Frame::new(PLUGIN_ERROR_KIND, raw, at)
                .with_severity(Severity::Error)
                .with_summary(format!("plugin error: {first_line}"))
                .with_field("error", message),
        );
    }

    /// One call to the plugin's `decode` over `input`, which starts at stream offset `base`.
    fn decode_input(&mut self, input: &[u8], base: u64, at: Instant, out: &mut Vec<Frame>) {
        let end = base + input.len() as u64;
        let result = match self.call(|plugin, store| plugin.call_decode(store, input)) {
            Ok(result) => result,
            Err(message) => {
                self.fail(base..end, at, &message, out);
                return;
            }
        };
        for (i, frame) in result.frames.iter().enumerate() {
            let (start, stop) = match convert::frame_span(frame, input.len()) {
                Ok(span) => span,
                Err(err) => {
                    // A frame outside the input spoils the rest of the call.
                    self.fail(base..end, at, &format!("frame {}: {err}", i + 1), out);
                    return;
                }
            };
            let raw = base + start..base + stop;
            match convert::frame_from_wit(frame, &self.shared.schema, raw.clone(), at) {
                Ok(frame) => out.push(frame),
                Err(err) => self.fail(raw, at, &format!("frame {}: {err}", i + 1), out),
            }
        }
        let held = result.held as usize;
        if held > input.len() {
            let message = format!(
                "decode held back {held} bytes of the {} it was given",
                input.len()
            );
            self.fail(base..end, at, &message, out);
        } else if held > self.shared.limits.max_held_back {
            let message = format!(
                "decode held back {held} bytes, more than the limit of {}",
                self.shared.limits.max_held_back
            );
            self.fail(end - held as u64..end, at, &message, out);
        } else {
            self.held.extend_from_slice(&input[input.len() - held..]);
        }
    }
}

impl Codec for WasmCodec {
    fn describe(&self) -> CodecInfo {
        self.shared.info.clone()
    }

    fn decode(&mut self, chunk: &[u8], at: Instant, raw_offset: u64, out: &mut Vec<Frame>) {
        if chunk.is_empty() {
            return;
        }
        let base = raw_offset.saturating_sub(self.held.len() as u64);
        let mut input = std::mem::take(&mut self.input);
        input.clear();
        input.append(&mut self.held);
        input.extend_from_slice(chunk);
        self.decode_input(&input, base, at, out);
        self.input = input;
    }

    fn encode(&mut self, request: &EncodeRequest) -> Result<Vec<u8>, CodecError> {
        let request = convert::request_to_wit(&request.command, &request.fields)
            .map_err(CodecError::Internal)?;
        match self.call(|plugin, store| plugin.call_encode(store, &request)) {
            Ok(result) => result.map_err(convert::error_from_wit),
            Err(message) => Err(CodecError::Internal(message)),
        }
    }

    fn reset(&mut self) {
        self.held.clear();
        if self.instance.is_some()
            && let Err(message) = self.call(|plugin, store| plugin.call_reset(store))
        {
            tracing::warn!(plugin = %self.shared.info.name, %message, "a WebAssembly codec failed to reset");
        }
    }
}

/// Makes [`WasmCodec`]s from one plugin. Compiles the component once, links it, and runs
/// it once to learn what it describes (and to report a broken plugin early); each
/// [`create`](CodecFactory::create) instantiates the compiled code in a fresh store, which
/// takes microseconds rather than a compile.
#[derive(Clone)]
pub struct WasmCodecFactory {
    shared: Arc<Shared>,
}

impl fmt::Debug for WasmCodecFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmCodecFactory")
            .field("plugin", &self.shared.origin)
            .field("name", &self.shared.info.name)
            .finish_non_exhaustive()
    }
}

impl WasmCodecFactory {
    /// Load the plugin folder `dir` (`plugin.toml` and `plugin.wasm`) with the shared
    /// engine and the default limits.
    pub fn load_dir(dir: impl AsRef<Path>) -> Result<Self, CodecError> {
        Self::load_dir_with(dir, &WasmEngine::shared()?, WasmLimits::default())
    }

    /// Load the plugin folder `dir`: read and check its manifest, compile its component,
    /// and check that it describes itself with the manifest's name and version.
    pub fn load_dir_with(
        dir: impl AsRef<Path>,
        engine: &WasmEngine,
        limits: WasmLimits,
    ) -> Result<Self, CodecError> {
        let dir = dir.as_ref();
        let path = dir.join(WASM_ENTRY);
        let fail = |message: String| {
            CodecError::Internal(format!("WebAssembly plugin {}: {message}", dir.display()))
        };
        let manifest = PluginManifest::load(dir).map_err(fail)?;
        let bytes =
            std::fs::read(&path).map_err(|err| fail(format!("{}: {err}", path.display())))?;
        let shared = Shared::load(
            path.display().to_string(),
            Some(path),
            Some(manifest),
            &bytes,
            engine,
            limits,
        )
        .map_err(fail)?;
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    /// A plugin from component bytes, with no manifest; `name` appears in error messages.
    pub fn from_bytes(
        name: &str,
        wasm: &[u8],
        engine: &WasmEngine,
        limits: WasmLimits,
    ) -> Result<Self, CodecError> {
        let shared = Shared::load(name.to_owned(), None, None, wasm, engine, limits)
            .map_err(|err| CodecError::Internal(format!("WebAssembly plugin {name}: {err}")))?;
        Ok(Self {
            shared: Arc::new(shared),
        })
    }

    /// The plugin's manifest, if it was loaded from a folder.
    pub fn manifest(&self) -> Option<&PluginManifest> {
        self.shared.manifest.as_ref()
    }

    /// The plugin's `plugin.wasm`, if it was loaded from a folder.
    pub fn path(&self) -> Option<&Path> {
        self.shared.path.as_deref()
    }

    pub fn limits(&self) -> WasmLimits {
        self.shared.limits
    }

    /// A codec that is a [`WasmCodec`] rather than a trait object.
    pub fn create_wasm(&self) -> Result<WasmCodec, CodecError> {
        WasmCodec::new(Arc::clone(&self.shared))
    }
}

impl CodecFactory for WasmCodecFactory {
    fn info(&self) -> CodecInfo {
        self.shared.info.clone()
    }

    fn create(&self) -> Result<Box<dyn Codec>, CodecError> {
        Ok(Box::new(self.create_wasm()?))
    }
}

/// The imports a plugin may have: the world's one function.
const ALLOWED_IMPORTS: [&str; 1] = ["log"];

impl Shared {
    fn load(
        origin: String,
        path: Option<PathBuf>,
        manifest: Option<PluginManifest>,
        bytes: &[u8],
        engine: &WasmEngine,
        limits: WasmLimits,
    ) -> Result<Self, String> {
        let component = Component::new(engine.engine(), bytes)
            .map_err(|err| format!("not a WebAssembly component: {err:#}"))?;
        let ty = component.component_type();
        let foreign: Vec<&str> = ty
            .imports(engine.engine())
            .map(|(name, _)| name)
            .filter(|name| !ALLOWED_IMPORTS.contains(name))
            .collect();
        if !foreign.is_empty() {
            return Err(format!(
                "it imports {}, but a plugin gets only `log` (no WASI: build a Rust plugin \
                 as no_std with serialist-plugin-sdk's `rt` feature)",
                foreign.join(", ")
            ));
        }
        let mut linker = Linker::<HostState>::new(engine.engine());
        wit::Plugin::add_to_linker::<HostState, HasSelf<HostState>>(&mut linker, |state| state)
            .map_err(|err| format!("{err:#}"))?;
        let pre = linker
            .instantiate_pre(&component)
            .and_then(wit::PluginPre::new)
            .map_err(|err| format!("does not export the serialist:codec/plugin world: {err:#}"))?;
        let mut shared = Shared {
            engine: engine.clone(),
            pre,
            info: CodecInfo {
                name: manifest
                    .as_ref()
                    .map_or_else(|| origin.clone(), |m| m.name.clone()),
                ..CodecInfo::default()
            },
            schema: Schema::new(),
            limits,
            origin,
            path,
            manifest,
        };
        let mut instance = shared.instantiate()?;
        let info = instance
            .call(&shared, |plugin, store| plugin.call_describe(store))
            .map_err(|err| format!("describe() failed: {err}"))?;
        let info = convert::info_from_wit(info)?;
        if let Some(manifest) = &shared.manifest
            && (manifest.name != info.name || manifest.version != info.version)
        {
            return Err(format!(
                "plugin.toml says {} {} but the plugin describes itself as {} {}",
                manifest.name, manifest.version, info.name, info.version
            ));
        }
        shared.schema = convert::schema(&info);
        shared.info = info;
        Ok(shared)
    }
}
