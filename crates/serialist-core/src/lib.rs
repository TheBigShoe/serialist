//! Serialist core: everything that does not need a window.
//!
//! Dependency rule: this crate never depends on GPUI. It is built and tested headless,
//! and every abstraction here has an in-process implementation in `serialist-sim`
//! so the whole engine can be exercised without hardware.

pub mod ansi;
pub mod codec;
pub mod commands;
pub mod composite;
pub mod config;
pub mod config_watch;
pub mod discovery;
pub mod frames;
pub mod history;
pub mod ingest;
pub mod keymap;
pub mod matcher;
pub mod port;
pub mod serial;
pub mod session;
pub mod settings;
pub mod store;
pub mod text;
pub mod theme;
pub mod transport;

#[cfg(test)]
mod test_util;

pub use ansi::{AnsiParser, OwnedLine, ParsedLine};
pub use codec::{
    Codec, CodecError, CodecFactory, CodecInfo, CodecRegistry, CommandInfo, EncodeRequest,
    FieldInfo, FieldType, FnCodecFactory, Frame, FrameKindInfo, Severity, SmolStr, Value,
};
pub use commands::{
    CollectionSource, Command, CommandCollection, CommandGroup, CommandRef, CommandStore,
    CommandWarning, DEFAULT_EXPECT_TIMEOUT_MS, EditError, Expect, Param, ParamKind, ParamValues,
    Payload, PayloadError,
};
pub use composite::{MergedPortSource, RoutingTransportFactory, VIRTUAL_SCHEME};
pub use config::{DataBits, FlowControl, Parity, SerialConfig, StopBits};
pub use config_watch::{ConfigEvent, ConfigWatcher};
pub use discovery::RealPortSource;
pub use frames::{
    CodecSink, FrameId, FrameSnapshot, FrameStats, FrameStore, FrameStoreConfig, FrameStoreReader,
};
pub use history::History;
pub use ingest::{
    ChunkSink, ConnectionInfo, Ingest, IngestHandle, IngestPanicked, IngestStats, IngestStopped,
    LinkState,
};
pub use keymap::{ActionRef, KeyBinding, Keymap, KeymapError, load_keymap};
pub use matcher::{ExpectResult, Expectation, MatcherHandle};
pub use port::{PortEvent, PortId, PortInfo, PortKind, PortSource, UsbInfo};
pub use serial::SerialportFactory;
pub use session::{Session, SessionClosed, SessionConfig, SessionEvent, SessionStats};
pub use settings::EditError as SettingsEditError;
pub use settings::{
    BackspaceKey, ConfigPaths, DeviceMatch, DeviceProfile, DisplaySettings, DisplayView,
    FontFeatures, FontSpec, InlineSettings, KeyChange, KeymapEditor, LineEnding, LineHeight,
    Platform, Settings, SettingsEditor, SettingsError, SettingsWarning, TerminalSettings,
    ThemeMode, ThemeSelection, TimestampMode, UsbId, load_settings,
};
pub use store::{
    AppendReport, HexStyles, HexView, Snapshot, Store, StoreConfig, StoreReader, StoreStats,
    TextExportReport, TextOptions, Timestamps,
};
pub use text::{
    Color, Direction, Epoch, LineId, LineSource, SearchMatch, Searcher, Style, StyleFlags,
    StyleRun, StyledLine,
};
pub use theme::{
    Appearance, PlayerColors, Rgba, SyntaxStyle, Theme, ThemeFamily, ThemeRegistry, ThemeWarning,
};
pub use transport::{
    ControlLine, Transport, TransportError, TransportFactory, TransportReader, TransportWriter,
};
