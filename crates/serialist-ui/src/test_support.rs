//! Test doubles for UI tests: a port source a test can plug and unplug, and a session
//! whose events a test feeds by hand. The simulator crate will provide richer ones; these
//! keep the UI crate's tests independent of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serialist_core::settings::ConfigPaths;
use serialist_core::{
    Direction, LineSource, PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig,
    SessionClosed, SessionEvent, SessionStats, StoreConfig, StyledLine, TransportError, UsbInfo,
};
use serialist_sim::SimWorld;

use crate::prelude::*;
use crate::session_handle::{SessionHandle, SessionOpener};
use crate::session_view::{FRAME, SessionView};
use crate::workspace::{AppOptions, Workspace};

pub(crate) fn port(id: &str) -> PortInfo {
    PortInfo {
        id: PortId::new(id),
        kind: PortKind::Unknown,
        display_name: id.to_owned(),
    }
}

pub(crate) fn usb_port(id: &str, product: &str, vid: u16, pid: u16) -> PortInfo {
    PortInfo {
        id: PortId::new(id),
        kind: PortKind::Usb(UsbInfo {
            vid,
            pid,
            serial_number: None,
            manufacturer: None,
            product: Some(product.to_owned()),
        }),
        display_name: product.to_owned(),
    }
}

#[derive(Default)]
struct PortSourceState {
    ports: Vec<PortInfo>,
    subscribers: Vec<Sender<PortEvent>>,
}

/// An in-memory [`PortSource`].
#[derive(Default)]
pub(crate) struct FakePortSource {
    state: Mutex<PortSourceState>,
}

impl FakePortSource {
    pub fn new(ports: impl IntoIterator<Item = PortInfo>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(PortSourceState {
                ports: ports.into_iter().collect(),
                subscribers: Vec::new(),
            }),
        })
    }

    pub fn plug(&self, info: PortInfo) {
        let mut state = self.state.lock();
        state.ports.retain(|p| p.id != info.id);
        state.ports.push(info.clone());
        state
            .subscribers
            .retain(|tx| tx.send(PortEvent::Added(info.clone())).is_ok());
    }

    pub fn unplug(&self, id: &PortId) {
        let mut state = self.state.lock();
        state.ports.retain(|p| &p.id != id);
        state
            .subscribers
            .retain(|tx| tx.send(PortEvent::Removed(id.clone())).is_ok());
    }
}

impl PortSource for FakePortSource {
    fn snapshot(&self) -> Vec<PortInfo> {
        self.state.lock().ports.clone()
    }

    fn subscribe(&self) -> Receiver<PortEvent> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut state = self.state.lock();
        tx.send(PortEvent::Snapshot(state.ports.clone())).ok();
        state.subscribers.push(tx);
        rx
    }
}

#[derive(Default)]
struct FakeSessionShared {
    written: Mutex<Vec<Vec<u8>>>,
    stats: Mutex<SessionStats>,
    closed: AtomicBool,
    /// A `Disconnected` went out; like `Session`, nothing is emitted after it.
    ended: AtomicBool,
}

/// The session half handed to the view.
struct FakeSession {
    events: Receiver<SessionEvent>,
    feed: Sender<SessionEvent>,
    shared: Arc<FakeSessionShared>,
}

/// The test's half: push events in, inspect what the view wrote.
#[derive(Clone)]
pub(crate) struct FakeFeed {
    tx: Sender<SessionEvent>,
    shared: Arc<FakeSessionShared>,
}

pub(crate) fn fake_session() -> (Box<dyn SessionHandle>, FakeFeed) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let shared = Arc::new(FakeSessionShared::default());
    let session = FakeSession {
        events: rx,
        feed: tx.clone(),
        shared: shared.clone(),
    };
    (Box::new(session), FakeFeed { tx, shared })
}

impl FakeFeed {
    pub fn connected(&self, description: &str) {
        self.send(SessionEvent::Connected {
            description: description.to_owned(),
        });
    }

    pub fn data(&self, bytes: &[u8]) {
        {
            let mut stats = self.shared.stats.lock();
            stats.rx_bytes += bytes.len() as u64;
            stats.rx_chunks += 1;
        }
        self.send(SessionEvent::Data {
            bytes: Arc::from(bytes),
            received_at: Instant::now(),
        });
    }

    /// The link reports its end. As with `Session`, the session reads as disconnected
    /// before the event is visible, and a later `close` emits nothing more.
    pub fn disconnected(&self, error: Option<TransportError>) {
        if !self.shared.ended.swap(true, Ordering::SeqCst) {
            self.send(SessionEvent::Disconnected { error });
        }
    }

    pub fn send(&self, event: SessionEvent) {
        self.tx.send(event).expect("the view keeps a receiver");
    }

    pub fn written(&self) -> Vec<Vec<u8>> {
        self.shared.written.lock().clone()
    }

    pub fn was_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }
}

impl SessionHandle for FakeSession {
    fn events(&self) -> Receiver<SessionEvent> {
        self.events.clone()
    }

    fn write(&self, bytes: Vec<u8>) -> Result<(), SessionClosed> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(SessionClosed);
        }
        self.shared.stats.lock().tx_bytes += bytes.len() as u64;
        self.shared.written.lock().push(bytes);
        Ok(())
    }

    fn stats(&self) -> SessionStats {
        *self.shared.stats.lock()
    }

    fn is_connected(&self) -> bool {
        !self.shared.closed.load(Ordering::SeqCst) && !self.shared.ended.load(Ordering::SeqCst)
    }

    fn close(self: Box<Self>) {
        // Same contract as `Session::close`: an orderly close reports itself, unless the
        // link already reported its own end.
        self.shared.closed.store(true, Ordering::SeqCst);
        if !self.shared.ended.swap(true, Ordering::SeqCst) {
            self.feed
                .send(SessionEvent::Disconnected { error: None })
                .ok();
        }
    }
}

/// Hands out fake sessions and keeps their feeds for the test.
#[derive(Default)]
pub(crate) struct FakeOpener {
    opened: Mutex<Vec<(PortId, SerialConfig, FakeFeed)>>,
    fail_with: Mutex<Option<&'static str>>,
}

impl FakeOpener {
    pub fn fail_next(&self, reason: &'static str) {
        *self.fail_with.lock() = Some(reason);
    }

    pub fn opened(&self) -> Vec<(PortId, SerialConfig, FakeFeed)> {
        self.opened.lock().clone()
    }
}

impl SessionOpener for FakeOpener {
    fn open(
        &self,
        port: &PortId,
        serial: &SerialConfig,
    ) -> Result<Box<dyn SessionHandle>, TransportError> {
        if let Some(reason) = self.fail_with.lock().take() {
            return Err(TransportError::Unsupported(reason));
        }
        let (session, feed) = fake_session();
        self.opened
            .lock()
            .push((port.clone(), serial.clone(), feed));
        Ok(session)
    }
}

/// The size of a test window: a laptop's, wide enough for both docks and the center.
pub(crate) const TEST_WINDOW: (f32, f32) = (1280., 800.);

/// Open a headless test window whose content is built by `build`, with the app
/// initialised the way `main` does it.
pub(crate) fn open_test_window<V: Render>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) -> (AnyWindowHandle, Entity<V>) {
    open_test_window_sized(cx, TEST_WINDOW, build)
}

/// [`open_test_window`] at `width` by `height`.
pub(crate) fn open_test_window_sized<V: Render>(
    cx: &mut TestAppContext,
    (width, height): (f32, f32),
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) -> (AnyWindowHandle, Entity<V>) {
    cx.update(|cx| {
        // Animations (a dialog's 250 ms slide and fade) run on the wall clock, not on the
        // test clock. A click aimed at a button in a dialog that is still sliding in lands
        // where the button was some frames ago: close enough on a fast machine, a miss on
        // a loaded one. With reduced motion they show their end state from the first frame,
        // as in the screenshot harness.
        cx.set_reduce_motion(true);
        crate::workspace::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(width), px(height)),
        };
        kit_open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| build(window, cx)),
        )
        .expect("open test window")
    })
}

/// A scratch directory under the system temp dir, removed on drop.
pub(crate) struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("serialist-test-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create a test directory");
        Self(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Install the bundled example plugin `name` into the config directory `paths` names,
/// as the "Install example plugin" action does. The app ships with no codec active, so a
/// test that decodes installs one first.
pub(crate) fn install_example_plugin(paths: &ConfigPaths, name: &str) -> PathBuf {
    let example = serialist_plugins::example_plugin(name)
        .unwrap_or_else(|| panic!("no example plugin named {name}"));
    paths
        .install_example_plugin(example)
        .expect("install the example plugin")
}

/// A config directory for a test that decodes: an empty `settings.json` and the
/// example Airoha RACE plugin installed.
pub(crate) fn race_config_dir(name: &str) -> TestDir {
    let dir = TestDir::new(name);
    std::fs::write(dir.join("settings.json"), "{}").expect("write settings.json");
    install_example_plugin(&ConfigPaths::new(dir.path()), "airoha-race");
    dir
}

/// The rows of a CSV file as an export writes it, quotes undone.
pub(crate) fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            ('"', true) => quoted = false,
            ('"', false) if cell.is_empty() => quoted = true,
            (',', false) => row.push(std::mem::take(&mut cell)),
            ('\n', false) => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            (c, _) => cell.push(c),
        }
    }
    rows
}

// Driving the real engine (Session and ingest threads over SimWorld links) from a
// headless workspace. The engine runs on real threads, so waits are bounded in real
// time. Each step runs whatever the ingest thread's doorbell woke, then advances the
// test clock by one frame, which fires the session view's pacing timer so the next
// ring is answered; nothing sleeps longer than the engine takes.

/// Generous failure bound for engine-driven waits; tests finish far sooner.
pub(crate) const ENGINE_WAIT: Duration = Duration::from_secs(10);

/// Let real threads wake the test's tasks. A session view's ingest thread rings the
/// view's doorbell from its own thread, which GPUI's test scheduler otherwise records
/// as nondeterminism and fails the test for. Call it before opening a session view.
pub(crate) fn allow_engine_threads(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
}

pub(crate) fn run_until(
    cx: &mut TestAppContext,
    what: &str,
    mut done: impl FnMut(&mut TestAppContext) -> bool,
) {
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        if done(cx) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ENGINE_WAIT:?} waiting for {what}"
        );
        cx.executor().advance_clock(FRAME);
    }
}

/// Advance `frames` frames, running whatever they wake.
pub(crate) fn step(cx: &mut TestAppContext, frames: usize) {
    for _ in 0..frames {
        cx.executor().advance_clock(FRAME);
        cx.run_until_parked();
    }
}

/// A workspace over `world`, optionally opening `connect_to` at startup as `--port` does.
pub(crate) fn open_workspace(
    cx: &mut TestAppContext,
    world: &SimWorld,
    connect_to: Option<&str>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    open_workspace_options(cx, world, connect_to, None)
}

/// [`open_workspace`] with sessions stored as `store` says.
pub(crate) fn open_workspace_with(
    cx: &mut TestAppContext,
    world: &SimWorld,
    connect_to: Option<&str>,
    store: StoreConfig,
) -> (AnyWindowHandle, Entity<Workspace>) {
    open_workspace_options(cx, world, connect_to, Some(store))
}

fn open_workspace_options(
    cx: &mut TestAppContext,
    world: &SimWorld,
    connect_to: Option<&str>,
    store: Option<StoreConfig>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    allow_engine_threads(cx);
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        baud: None,
        select_port: None,
        open_ports: connect_to.map(PortId::new).into_iter().collect(),
        store,
    };
    open_test_window(cx, move |window, cx| Workspace::new(options, window, cx))
}

pub(crate) fn session_of(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |w, _| w.session().cloned())
}

/// Wait for the workspace's session view to show ingest's `Connected to …` notice.
pub(crate) fn wait_connected(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
) -> Entity<SessionView> {
    run_until(cx, "the session to connect", |cx| {
        session_of(cx, workspace).is_some_and(|view| {
            view.read_with(cx, |v, _| {
                v.snapshot()
                    .line(serialist_core::LineId::ZERO)
                    .is_some_and(|line| line.text.starts_with("Connected to "))
            })
        })
    });
    session_of(cx, workspace).expect("session view")
}

/// The lines the session's terminal displays now, read from the element's source.
pub(crate) fn displayed(cx: &mut TestAppContext, view: &Entity<SessionView>) -> Vec<StyledLine> {
    view.read_with(cx, |v, cx| {
        let terminal = v.terminal().read(cx);
        let mut out = Vec::new();
        terminal
            .source()
            .lines(terminal.displayed_span().range(), &mut out);
        out
    })
}

/// Wait until the lines the session's terminal displays satisfy `done`, and return them.
///
/// What the terminal shows is the view's snapshot, which follows the ingest thread's
/// publications on the next doorbell wake, a frame later at the earliest. A test that
/// acts (types, sends, feeds bytes) and then reads the lines must wait for the state it
/// is about to assert rather than assume the wake has happened: the wait here is on the
/// condition, never on time, so it holds however slowly the ingest thread runs.
pub(crate) fn wait_for_lines(
    cx: &mut TestAppContext,
    view: &Entity<SessionView>,
    what: &str,
    mut done: impl FnMut(&[StyledLine]) -> bool,
) -> Vec<StyledLine> {
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        let lines = displayed(cx, view);
        if done(&lines) {
            return lines;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ENGINE_WAIT:?} waiting for {what}; the terminal shows {:?}",
            lines
                .iter()
                .map(|line| (line.direction, line.text.as_str(), line.complete))
                .collect::<Vec<_>>()
        );
        cx.executor().advance_clock(FRAME);
    }
}

/// Wait until the view's snapshot holds at least `total` received bytes. For a line the
/// device echoes in pieces, this is the wait that means "all of it has arrived", where a
/// received line with the right text may still be missing its line ending.
pub(crate) fn wait_for_received(cx: &mut TestAppContext, view: &Entity<SessionView>, total: u64) {
    let mut seen = 0;
    let deadline = Instant::now() + ENGINE_WAIT;
    loop {
        cx.run_until_parked();
        seen = seen.max(view.read_with(cx, |v, _| v.snapshot().raw_range().end));
        if seen >= total {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {ENGINE_WAIT:?} waiting for {total} received bytes; the view has {seen}"
        );
        cx.executor().advance_clock(FRAME);
    }
}

/// A received line with exactly this text among the newest 1000 displayed.
pub(crate) fn has_rx_line(cx: &mut TestAppContext, view: &Entity<SessionView>, text: &str) -> bool {
    view.read_with(cx, |v, cx| {
        let terminal = v.terminal().read(cx);
        let span = terminal.displayed_span();
        let from = serialist_core::LineId(span.end.0.saturating_sub(1000)).max(span.first);
        let mut lines = Vec::new();
        terminal.source().lines(from..span.end, &mut lines);
        lines
            .iter()
            .any(|line| line.direction == Direction::Rx && line.text == text)
    })
}

/// Resize the window to `width` by `height` and draw a frame at the new size.
pub(crate) fn resize_window(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    (width, height): (f32, f32),
) {
    cx.simulate_window_resize(window, size(px(width), px(height)));
    cx.run_until_parked();
    draw(cx, window);
}

/// Draw the window twice, so a layout that depends on the last frame has settled.
pub(crate) fn draw(cx: &mut TestAppContext, window: AnyWindowHandle) {
    for _ in 0..2 {
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
    }
}

/// Turn on local echo in the session's compose bar, as its Echo button does. The
/// bundled settings leave it off.
pub(crate) fn enable_local_echo(cx: &mut TestAppContext, view: &Entity<SessionView>) {
    let compose = view.read_with(cx, |v, _| v.compose().clone());
    compose.update(cx, |compose, cx| compose.set_local_echo(true, cx));
}

/// Type into whatever has focus and press Enter, as a user would.
pub(crate) fn type_line(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    cx.update_window(window, |_, window, cx| {
        window.input(text, cx);
        window.press("enter", cx);
    })
    .unwrap();
}
