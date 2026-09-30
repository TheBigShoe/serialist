//! Test doubles for UI tests: a port source a test can plug and unplug, and a session
//! whose events a test feeds by hand. The simulator crate will provide richer ones; these
//! keep the UI crate's tests independent of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serialist_core::{
    PortEvent, PortId, PortInfo, PortKind, PortSource, SerialConfig, SessionClosed, SessionEvent,
    SessionStats, TransportError, UsbInfo,
};
use serialist_sim::SimWorld;

use crate::drain::FRAME;
use crate::line_buffer::LineKind;
use crate::prelude::*;
use crate::session_handle::{SessionHandle, SessionOpener};
use crate::session_model::ConnectionState;
use crate::session_view::SessionView;
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

    pub fn disconnected(&self, error: Option<TransportError>) {
        self.send(SessionEvent::Disconnected { error });
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
        !self.shared.closed.load(Ordering::SeqCst)
    }

    fn close(self: Box<Self>) {
        // Same contract as `Session::close`: an orderly close reports itself.
        self.shared.closed.store(true, Ordering::SeqCst);
        self.feed
            .send(SessionEvent::Disconnected { error: None })
            .ok();
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

/// Open a headless test window whose content is built by `build`, with the app
/// initialised the way `main` does it.
pub(crate) fn open_test_window<V: Render>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) -> (AnyWindowHandle, Entity<V>) {
    cx.update(|cx| {
        crate::workspace::init(cx);
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(700.)),
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

// Driving the real engine (Session threads over SimWorld links) from a headless
// workspace. The engine runs on real threads, so waits are bounded in real time. Each
// step advances the test clock by one frame, which fires the drain loop's pacing timer;
// the drain worker then blocks for at most its idle wait and returns the moment data
// arrives, so nothing sleeps longer than the engine takes.

/// Generous failure bound for engine-driven waits; tests finish far sooner.
pub(crate) const ENGINE_WAIT: Duration = Duration::from_secs(10);

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

/// A workspace over `world`, optionally opening `connect_to` at startup as `--port` does.
pub(crate) fn open_workspace(
    cx: &mut TestAppContext,
    world: &SimWorld,
    connect_to: Option<&str>,
) -> (AnyWindowHandle, Entity<Workspace>) {
    let options = AppOptions {
        port_source: world.port_source(),
        transport_factory: world.transport_factory(),
        serial: SerialConfig::default(),
        select_port: connect_to.map(PortId::new),
        connect_on_start: connect_to.is_some(),
    };
    open_test_window(cx, move |window, cx| Workspace::new(options, window, cx))
}

pub(crate) fn session_of(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
) -> Option<Entity<SessionView>> {
    workspace.read_with(cx, |w, _| w.session().cloned())
}

pub(crate) fn wait_connected(
    cx: &mut TestAppContext,
    workspace: &Entity<Workspace>,
) -> Entity<SessionView> {
    run_until(cx, "the session to connect", |cx| {
        session_of(cx, workspace)
            .is_some_and(|s| s.read_with(cx, |v, _| v.model().state == ConnectionState::Connected))
    });
    session_of(cx, workspace).expect("session view")
}

pub(crate) fn has_rx_line(cx: &mut TestAppContext, view: &Entity<SessionView>, text: &str) -> bool {
    view.read_with(cx, |v, _| {
        v.model()
            .buffer
            .rows()
            .any(|r| r.kind == LineKind::Rx && r.text == text)
    })
}

/// Type into whatever has focus and press Enter, as a user would.
pub(crate) fn type_line(cx: &mut TestAppContext, window: AnyWindowHandle, text: &str) {
    cx.update_window(window, |_, window, cx| {
        window.input(text, cx);
        window.press("enter", cx);
    })
    .unwrap();
}
