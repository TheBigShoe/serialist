//! Real port discovery: `serialport` enumeration for the list, `nusb` hotplug events as
//! the trigger for refreshing it.
//!
//! [`dedupe_ports`] and [`diff_ports`] are pure and carry the interesting logic.
//! [`RealPortSource`] wires them to a background thread that
//!
//! 1. listens to `nusb::watch_devices()`,
//! 2. waits 150 ms after the first event of a burst (the tty node can appear a moment
//!    after the USB event), then re-enumerates and diffs,
//! 3. re-enumerates every 3 s regardless, for Bluetooth and PCI ports nusb cannot see.
//!
//! If nusb cannot start (no permission, unsupported platform) the source degrades to
//! polling alone and says so once through `tracing`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, select, unbounded};
use futures_core::Stream;
use parking_lot::Mutex;
use serialport::{SerialPortInfo, SerialPortType};

use crate::port::{PortEvent, PortId, PortInfo, PortKind, PortSource, UsbInfo};

/// Quiet period between the first hotplug event of a burst and the re-enumeration.
pub const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(150);

/// Fallback re-enumeration period, for ports that never produce a USB event.
pub const POLL_INTERVAL: Duration = Duration::from_secs(3);

/// macOS exposes every serial device twice: `/dev/cu.X` (call-out, opens immediately)
/// and `/dev/tty.X` (call-in, blocks waiting for carrier detect).
const MAC_CALLOUT_PREFIX: &str = "/dev/cu.";
const MAC_DIALIN_PREFIX: &str = "/dev/tty.";

// ---------------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------------

/// Converts raw `serialport` enumeration output into the list the UI shows.
///
/// * When both `/dev/cu.X` and `/dev/tty.X` are present, only `cu.X` is kept. The rule
///   is by name, not by platform, so it is testable everywhere; nothing outside macOS
///   creates `/dev/tty.<name>` nodes, and Linux's `/dev/ttyUSB0` has no dot after `tty`.
///   A `tty.X` with no `cu.X` twin is kept.
/// * Repeated paths collapse to the first.
/// * `SerialPortType` maps to [`PortKind`]; USB strings are trimmed and empty ones drop.
/// * `display_name` is `<product> (<serial number>)`, or just the product, or the path.
/// * The result is sorted naturally by path, so `COM2` precedes `COM10`.
pub fn dedupe_ports(ports: Vec<SerialPortInfo>) -> Vec<PortInfo> {
    let callout_suffixes: HashSet<String> = ports
        .iter()
        .filter_map(|p| p.port_name.strip_prefix(MAC_CALLOUT_PREFIX))
        .map(str::to_owned)
        .collect();

    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(ports.len());
    for port in ports {
        if port.port_name.trim().is_empty() {
            continue;
        }
        if let Some(suffix) = port.port_name.strip_prefix(MAC_DIALIN_PREFIX)
            && callout_suffixes.contains(suffix)
        {
            continue;
        }
        if !seen.insert(port.port_name.clone()) {
            continue;
        }
        out.push(to_port_info(port));
    }
    out.sort_by(|a, b| natural_cmp(a.id.as_str(), b.id.as_str()));
    out
}

fn to_port_info(port: SerialPortInfo) -> PortInfo {
    let kind = match port.port_type {
        SerialPortType::UsbPort(usb) => PortKind::Usb(UsbInfo {
            vid: usb.vid,
            pid: usb.pid,
            serial_number: clean(usb.serial_number),
            manufacturer: clean(usb.manufacturer),
            product: clean(usb.product),
        }),
        SerialPortType::PciPort => PortKind::Pci,
        SerialPortType::BluetoothPort => PortKind::Bluetooth,
        SerialPortType::Unknown => PortKind::Unknown,
    };
    let display_name = display_name(&port.port_name, &kind);
    PortInfo {
        id: PortId(port.port_name),
        kind,
        display_name,
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

fn display_name(path: &str, kind: &PortKind) -> String {
    match kind {
        PortKind::Usb(UsbInfo {
            product: Some(product),
            serial_number: Some(serial),
            ..
        }) => format!("{product} ({serial})"),
        PortKind::Usb(UsbInfo {
            product: Some(product),
            ..
        }) => product.clone(),
        _ => path.to_owned(),
    }
}

/// Orders strings with digit runs compared by value (`ttyUSB2` before `ttyUSB10`).
/// Ties fall back to plain byte order, which keeps the ordering total.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    fn digits_len(s: &[u8]) -> usize {
        s.iter().take_while(|c| c.is_ascii_digit()).count()
    }

    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => break,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(cx), Some(cy)) if cx.is_ascii_digit() && cy.is_ascii_digit() => {
                let (lx, ly) = (digits_len(x), digits_len(y));
                let (dx, dy) = (&x[..lx], &y[..ly]);
                let tx = &dx[dx.iter().take_while(|&&c| c == b'0').count()..];
                let ty = &dy[dy.iter().take_while(|&&c| c == b'0').count()..];
                let ord = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
                if ord != Ordering::Equal {
                    return ord;
                }
                x = &x[lx..];
                y = &y[ly..];
            }
            (Some(cx), Some(cy)) => {
                if cx != cy {
                    return cx.cmp(cy);
                }
                x = &x[1..];
                y = &y[1..];
            }
        }
    }
    a.cmp(b)
}

/// The events that turn `old` into `new`: `Removed` for ids that vanished, then `Added`
/// for ids that appeared, each in list order.
///
/// A port whose id is unchanged but whose details changed (for example a tty node that
/// was listed as `Unknown` before its USB metadata resolved) yields `Removed` followed
/// by `Added`. That keeps every consumer consistent whether it keys by id or just
/// appends, at the cost of one transient removal.
pub fn diff_ports(old: &[PortInfo], new: &[PortInfo]) -> Vec<PortEvent> {
    let old_by_id: HashMap<&PortId, &PortInfo> = old.iter().map(|p| (&p.id, p)).collect();
    let new_by_id: HashMap<&PortId, &PortInfo> = new.iter().map(|p| (&p.id, p)).collect();

    let removed = old
        .iter()
        .filter(|p| new_by_id.get(&p.id).is_none_or(|n| *n != *p))
        .map(|p| PortEvent::Removed(p.id.clone()));
    let added = new
        .iter()
        .filter(|p| old_by_id.get(&p.id).is_none_or(|o| *o != *p))
        .map(|p| PortEvent::Added(p.clone()));
    removed.chain(added).collect()
}

/// Enumerates the system's serial ports and dedupes them.
pub fn list_ports() -> Result<Vec<PortInfo>, serialport::Error> {
    serialport::available_ports().map(dedupe_ports)
}

// ---------------------------------------------------------------------------------
// RealPortSource
// ---------------------------------------------------------------------------------

type Enumerator = Box<dyn Fn() -> Result<Vec<PortInfo>, serialport::Error> + Send + Sync>;

struct State {
    /// What subscribers have been told so far.
    current: Vec<PortInfo>,
    subscribers: Vec<Sender<PortEvent>>,
}

struct Shared {
    /// Diffing and broadcasting happen under this one lock, and `subscribe` snapshots
    /// under it too, so a subscriber sees `Snapshot` and then exactly the later changes.
    state: Mutex<State>,
    /// Held for the whole of a refresh, enumeration included, so two refreshes can never
    /// overlap and a slow older result can never be applied over a newer one. It is a
    /// separate lock from `state` on purpose: `subscribe` and `snapshot` only take
    /// `state`, so they never wait behind a slow enumeration.
    refresh_lock: Mutex<()>,
    enumerate: Enumerator,
    /// Log the first failure of a run at `warn`, the repeats at `debug`.
    enumerate_failing: AtomicBool,
}

impl Shared {
    /// Enumerates, applies the diff to `state` and notifies subscribers. When
    /// enumeration fails the old list is kept, so an error never reads as "every port
    /// was unplugged".
    ///
    /// Refreshes are serialised: each one enumerates only after the previous one has
    /// been applied, so results are applied in the order they were taken.
    fn refresh(&self) {
        let _one_at_a_time = self.refresh_lock.lock();

        let fresh = match (self.enumerate)() {
            Ok(list) => {
                self.enumerate_failing.store(false, AtomicOrdering::Relaxed);
                list
            }
            Err(err) => {
                if self.enumerate_failing.swap(true, AtomicOrdering::Relaxed) {
                    tracing::debug!(error = %err, "port enumeration still failing");
                } else {
                    tracing::warn!(error = %err, "port enumeration failed; keeping the previous list");
                }
                return;
            }
        };

        let mut state = self.state.lock();
        let events = diff_ports(&state.current, &fresh);
        state.current = fresh;
        if !events.is_empty() {
            tracing::debug!(events = events.len(), "port list changed");
            // A send only fails when the receiver is gone, which is how dead
            // subscribers get pruned.
            state
                .subscribers
                .retain(|tx| events.iter().all(|e| tx.send(e.clone()).is_ok()));
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct MonitorOptions {
    debounce: Duration,
    poll_interval: Duration,
    watch_usb: bool,
}

impl Default for MonitorOptions {
    fn default() -> Self {
        Self {
            debounce: HOTPLUG_DEBOUNCE,
            poll_interval: POLL_INTERVAL,
            watch_usb: true,
        }
    }
}

/// The real [`PortSource`]. Dropping it stops and joins the monitor thread.
pub struct RealPortSource {
    shared: Arc<Shared>,
    kick_tx: Sender<()>,
    /// Dropping the sender is the stop signal; nothing is ever sent.
    stop_tx: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Default for RealPortSource {
    fn default() -> Self {
        Self::new()
    }
}

impl RealPortSource {
    /// Enumerates once on the calling thread, then starts the monitor thread. The first
    /// enumeration is what lets `snapshot` and `subscribe` be correct from the start;
    /// it costs a few milliseconds on macOS and Linux and can take a few hundred on
    /// Windows, so construct the source off the UI thread if that matters. Every later
    /// enumeration happens on the monitor thread.
    pub fn new() -> Self {
        Self::with_parts(Box::new(list_ports), MonitorOptions::default())
    }

    fn with_parts(enumerate: Enumerator, options: MonitorOptions) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                current: Vec::new(),
                subscribers: Vec::new(),
            }),
            refresh_lock: Mutex::new(()),
            enumerate,
            enumerate_failing: AtomicBool::new(false),
        });
        // Fill the cache before anyone can subscribe. Failure leaves it empty and the
        // monitor's first pass retries.
        shared.refresh();

        let (stop_tx, stop_rx) = bounded::<()>(0);
        let (kick_tx, kick_rx) = bounded::<()>(1);
        let thread = {
            let shared = Arc::clone(&shared);
            let kick_keepalive = kick_tx.clone();
            std::thread::Builder::new()
                .name("serialist-port-monitor".to_owned())
                .spawn(move || monitor_loop(&shared, options, &stop_rx, &kick_rx, &kick_keepalive))
        };
        let thread = match thread {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!(error = %err, "could not start the port monitor thread; ports will not update");
                None
            }
        };

        Self {
            shared,
            kick_tx,
            stop_tx: Some(stop_tx),
            thread,
        }
    }

    /// Ask the monitor to re-enumerate soon, as if a hotplug event had arrived. The usual
    /// debounce applies, and requests made while one is pending coalesce into it.
    pub fn request_refresh(&self) {
        // A full channel already holds a pending request.
        let _ = self.kick_tx.try_send(());
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.shared.state.lock().subscribers.len()
    }
}

impl PortSource for RealPortSource {
    /// The cached list, which the monitor thread keeps current: within about 150 ms of
    /// a USB hotplug event and within 3 s otherwise. This never enumerates on the
    /// caller's thread, since enumeration can take hundreds of milliseconds on Windows
    /// and the caller may be the UI thread. It also nudges the monitor to refresh (a
    /// channel send, coalesced with any pending request), so the next call is fresher
    /// and subscribers hear about any difference as events.
    fn snapshot(&self) -> Vec<PortInfo> {
        let cached = self.shared.state.lock().current.clone();
        self.request_refresh();
        cached
    }

    fn subscribe(&self) -> Receiver<PortEvent> {
        let (tx, rx) = unbounded();
        let mut state = self.shared.state.lock();
        // The receiver is alive and the channel unbounded, so this cannot fail.
        let _ = tx.send(PortEvent::Snapshot(state.current.clone()));
        state.subscribers.push(tx);
        rx
    }
}

impl Drop for RealPortSource {
    fn drop(&mut self) {
        drop(self.stop_tx.take());
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::warn!("port monitor thread panicked");
        }
    }
}

fn monitor_loop(
    shared: &Shared,
    options: MonitorOptions,
    stop_rx: &Receiver<()>,
    kick_rx: &Receiver<()>,
    // Keeps `kick_rx` connected for the life of the loop; only `stop_rx` may end it.
    _kick_keepalive: &Sender<()>,
) {
    // `wake_tx` stays alive for the whole loop, so `wake_rx` never disconnects (which
    // would make the select spin) even when the watcher is disabled.
    let (wake_tx, wake_rx) = bounded::<()>(1);
    let mut usb = if options.watch_usb {
        UsbWatcher::start(wake_tx.clone(), options.poll_interval)
    } else {
        UsbWatcher::disabled()
    };
    // Arm the waker, then enumerate: a device plugged in between `new()` and the watch
    // going live would otherwise wait for the first poll.
    usb.drain();
    shared.refresh();

    let mut next_poll = Instant::now() + options.poll_interval;
    let mut debounce_at: Option<Instant> = None;
    loop {
        let wait = debounce_at
            .map_or(next_poll, |d| d.min(next_poll))
            .saturating_duration_since(Instant::now());
        select! {
            recv(stop_rx) -> _ => return,
            recv(wake_rx) -> _ => {
                if usb.drain() {
                    debounce_at.get_or_insert(Instant::now() + options.debounce);
                }
            }
            recv(kick_rx) -> _ => {
                debounce_at.get_or_insert(Instant::now() + options.debounce);
            }
            default(wait) => {}
        }

        let now = Instant::now();
        if debounce_at.is_some_and(|d| now >= d) || now >= next_poll {
            debounce_at = None;
            shared.refresh();
            next_poll = Instant::now() + options.poll_interval;
        }
    }
}

/// Polls a `nusb` hotplug stream from the monitor thread, with a waker that pings the
/// monitor's channel. One thread, no executor.
struct UsbWatcher {
    watch: Option<Pin<Box<nusb::hotplug::HotplugWatch>>>,
    waker: Waker,
}

struct ChannelWaker(Sender<()>);

impl Wake for ChannelWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // A full channel already holds a pending wakeup.
        let _ = self.0.try_send(());
    }
}

impl UsbWatcher {
    fn start(wake_tx: Sender<()>, poll_interval: Duration) -> Self {
        let waker = Waker::from(Arc::new(ChannelWaker(wake_tx)));
        let watch = match open_hotplug_watch() {
            Ok(watch) => Some(Box::pin(watch)),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    poll_secs = poll_interval.as_secs_f32(),
                    "USB hotplug events unavailable; falling back to polling",
                );
                None
            }
        };
        Self { watch, waker }
    }

    fn disabled() -> Self {
        Self {
            watch: None,
            waker: Waker::noop().clone(),
        }
    }

    /// Consumes every queued event, re-arming the waker. True if there was any.
    fn drain(&mut self) -> bool {
        let Some(watch) = self.watch.as_mut() else {
            return false;
        };
        let mut cx = Context::from_waker(&self.waker);
        let mut saw_event = false;
        loop {
            match watch.as_mut().poll_next(&mut cx) {
                Poll::Ready(Some(event)) => {
                    tracing::trace!(?event, "USB hotplug event");
                    saw_event = true;
                }
                Poll::Ready(None) => {
                    tracing::warn!("USB hotplug stream ended; falling back to polling");
                    self.watch = None;
                    break;
                }
                Poll::Pending => break,
            }
        }
        saw_event
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
fn open_hotplug_watch() -> Result<nusb::hotplug::HotplugWatch, String> {
    nusb::watch_devices().map_err(|e| e.to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_hotplug_watch() -> Result<nusb::hotplug::HotplugWatch, String> {
    Err("USB hotplug is not supported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::PortEvent;
    use serialport::UsbPortInfo;
    use std::sync::atomic::AtomicUsize;

    // ---- fixtures ------------------------------------------------------------

    fn usb(
        name: &str,
        vid: u16,
        pid: u16,
        product: Option<&str>,
        serial: Option<&str>,
    ) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_owned(),
            port_type: SerialPortType::UsbPort(UsbPortInfo {
                vid,
                pid,
                serial_number: serial.map(str::to_owned),
                manufacturer: Some("FTDI".to_owned()),
                product: product.map(str::to_owned),
            }),
        }
    }

    fn plain(name: &str, port_type: SerialPortType) -> SerialPortInfo {
        SerialPortInfo {
            port_name: name.to_owned(),
            port_type,
        }
    }

    fn info(id: &str) -> PortInfo {
        PortInfo {
            id: PortId::new(id),
            kind: PortKind::Unknown,
            display_name: id.to_owned(),
        }
    }

    fn ids(list: &[PortInfo]) -> Vec<&str> {
        list.iter().map(|p| p.id.as_str()).collect()
    }

    // ---- dedupe_ports --------------------------------------------------------

    #[test]
    fn macos_keeps_cu_and_drops_tty_twin() {
        let ports = vec![
            usb(
                "/dev/tty.usbserial-1420",
                0x0403,
                0x6001,
                Some("FT232R USB UART"),
                Some("A50285BI"),
            ),
            usb(
                "/dev/cu.usbserial-1420",
                0x0403,
                0x6001,
                Some("FT232R USB UART"),
                Some("A50285BI"),
            ),
            plain(
                "/dev/tty.Bluetooth-Incoming-Port",
                SerialPortType::BluetoothPort,
            ),
            plain(
                "/dev/cu.Bluetooth-Incoming-Port",
                SerialPortType::BluetoothPort,
            ),
        ];
        let out = dedupe_ports(ports);
        assert_eq!(
            ids(&out),
            ["/dev/cu.Bluetooth-Incoming-Port", "/dev/cu.usbserial-1420"]
        );
        assert_eq!(out[0].kind, PortKind::Bluetooth);
        assert_eq!(out[0].display_name, "/dev/cu.Bluetooth-Incoming-Port");
    }

    #[test]
    fn macos_tty_without_a_cu_twin_is_kept() {
        let ports = vec![
            plain("/dev/tty.lonely", SerialPortType::Unknown),
            plain("/dev/cu.other", SerialPortType::Unknown),
            // A different suffix must not be mistaken for a twin.
            plain("/dev/tty.other2", SerialPortType::Unknown),
        ];
        assert_eq!(
            ids(&dedupe_ports(ports)),
            ["/dev/cu.other", "/dev/tty.lonely", "/dev/tty.other2"]
        );
    }

    #[test]
    fn macos_usb_metadata_survives_dedupe() {
        let out = dedupe_ports(vec![
            usb(
                "/dev/tty.usbmodem14201",
                0x2341,
                0x0043,
                Some("Arduino Uno"),
                Some("85735"),
            ),
            usb(
                "/dev/cu.usbmodem14201",
                0x2341,
                0x0043,
                Some("Arduino Uno"),
                Some("85735"),
            ),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id.as_str(), "/dev/cu.usbmodem14201");
        assert_eq!(out[0].display_name, "Arduino Uno (85735)");
        assert_eq!(
            out[0].kind,
            PortKind::Usb(UsbInfo {
                vid: 0x2341,
                pid: 0x0043,
                serial_number: Some("85735".to_owned()),
                manufacturer: Some("FTDI".to_owned()),
                product: Some("Arduino Uno".to_owned()),
            })
        );
    }

    #[test]
    fn linux_usb_and_acm_ports_are_never_treated_as_twins() {
        let out = dedupe_ports(vec![
            usb(
                "/dev/ttyUSB1",
                0x10c4,
                0xea60,
                Some("CP2102 USB to UART Bridge"),
                Some("0001"),
            ),
            usb(
                "/dev/ttyUSB0",
                0x0403,
                0x6001,
                Some("FT232R USB UART"),
                None,
            ),
            usb(
                "/dev/ttyACM0",
                0x2341,
                0x0043,
                Some("Arduino Uno"),
                Some("85735"),
            ),
            plain("/dev/ttyS0", SerialPortType::PciPort),
            usb("/dev/ttyUSB10", 0x0403, 0x6001, None, None),
        ]);
        // Natural order: ttyUSB2 style numbers compare by value, and every node survives.
        assert_eq!(
            ids(&out),
            [
                "/dev/ttyACM0",
                "/dev/ttyS0",
                "/dev/ttyUSB0",
                "/dev/ttyUSB1",
                "/dev/ttyUSB10"
            ]
        );
        assert_eq!(out[1].kind, PortKind::Pci);
        // Product and serial: both shown.
        assert_eq!(out[0].display_name, "Arduino Uno (85735)");
        // Product without a serial number: product alone.
        assert_eq!(out[2].display_name, "FT232R USB UART");
        // No product at all: the path.
        assert_eq!(out[4].display_name, "/dev/ttyUSB10");
    }

    #[test]
    fn windows_com_ports_sort_naturally() {
        let out = dedupe_ports(vec![
            plain("COM10", SerialPortType::Unknown),
            usb(
                "COM3",
                0x0403,
                0x6015,
                Some("USB Serial Port"),
                Some("DQ00A1B2"),
            ),
            plain("COM1", SerialPortType::PciPort),
            plain("COM2", SerialPortType::Unknown),
        ]);
        assert_eq!(ids(&out), ["COM1", "COM2", "COM3", "COM10"]);
        assert_eq!(out[2].display_name, "USB Serial Port (DQ00A1B2)");
        assert_eq!(out[0].kind, PortKind::Pci);
        assert_eq!(out[1].kind, PortKind::Unknown);
        assert_eq!(out[1].display_name, "COM2");
    }

    #[test]
    fn empty_and_blank_metadata_is_treated_as_missing() {
        let mut blank = usb("/dev/ttyUSB0", 1, 2, Some("  "), Some(""));
        if let SerialPortType::UsbPort(u) = &mut blank.port_type {
            u.manufacturer = Some("\t".to_owned());
        }
        let out = dedupe_ports(vec![blank]);
        assert_eq!(out[0].display_name, "/dev/ttyUSB0");
        assert_eq!(
            out[0].kind,
            PortKind::Usb(UsbInfo {
                vid: 1,
                pid: 2,
                serial_number: None,
                manufacturer: None,
                product: None,
            })
        );

        let padded = usb("/dev/ttyUSB1", 1, 2, Some(" Widget "), Some(" 42 "));
        assert_eq!(dedupe_ports(vec![padded])[0].display_name, "Widget (42)");
    }

    #[test]
    fn duplicate_paths_and_blank_names_collapse() {
        let out = dedupe_ports(vec![
            plain("/dev/ttyS1", SerialPortType::Unknown),
            plain("/dev/ttyS1", SerialPortType::PciPort),
            plain("", SerialPortType::Unknown),
            plain("  ", SerialPortType::Unknown),
        ]);
        assert_eq!(ids(&out), ["/dev/ttyS1"]);
        assert_eq!(out[0].kind, PortKind::Unknown, "first entry wins");
        assert!(dedupe_ports(Vec::new()).is_empty());
    }

    #[test]
    fn natural_order_is_total_and_numeric() {
        let mut names = vec![
            "COM10",
            "COM2",
            "COM02",
            "COM1",
            "COM",
            "COM1a",
            "ttyUSB9",
            "ttyUSB10",
            "ttyUSB010",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            [
                "COM",
                "COM1",
                "COM1a",
                "COM02",
                "COM2",
                "COM10",
                "ttyUSB9",
                "ttyUSB010",
                "ttyUSB10"
            ]
        );
        // Antisymmetric, including for numerically equal names.
        for a in &names {
            for b in &names {
                assert_eq!(natural_cmp(a, b), natural_cmp(b, a).reverse(), "{a} vs {b}");
            }
        }
    }

    // ---- diff_ports ----------------------------------------------------------

    #[test]
    fn diff_of_identical_lists_is_empty() {
        let a = vec![info("A"), info("B")];
        assert!(diff_ports(&a, &a).is_empty());
        assert!(diff_ports(&[], &[]).is_empty());
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let old = vec![info("A"), info("B")];
        let new = vec![info("B"), info("C"), info("D")];
        assert_eq!(
            diff_ports(&old, &new),
            vec![
                PortEvent::Removed(PortId::new("A")),
                PortEvent::Added(info("C")),
                PortEvent::Added(info("D")),
            ]
        );
    }

    #[test]
    fn diff_from_and_to_empty() {
        let list = vec![info("A"), info("B")];
        assert_eq!(
            diff_ports(&[], &list),
            vec![PortEvent::Added(info("A")), PortEvent::Added(info("B"))]
        );
        assert_eq!(
            diff_ports(&list, &[]),
            vec![
                PortEvent::Removed(PortId::new("A")),
                PortEvent::Removed(PortId::new("B"))
            ]
        );
    }

    #[test]
    fn diff_ignores_reordering() {
        let old = vec![info("A"), info("B"), info("C")];
        let new = vec![info("C"), info("A"), info("B")];
        assert!(diff_ports(&old, &new).is_empty());
    }

    #[test]
    fn diff_treats_changed_details_as_remove_then_add() {
        let before = info("/dev/cu.x");
        let mut after = before.clone();
        after.kind = PortKind::Bluetooth;
        after.display_name = "Speaker".to_owned();
        assert_eq!(
            diff_ports(&[before], &[after.clone()]),
            vec![
                PortEvent::Removed(PortId::new("/dev/cu.x")),
                PortEvent::Added(after)
            ]
        );
    }

    // ---- list_ports and the source with the real enumerator ---------------------

    #[test]
    fn real_enumeration_does_not_panic_and_is_well_formed() {
        // Possibly empty, possibly an Err (a sandbox without udev, say); never a panic.
        let Ok(list) = list_ports() else { return };
        let mut seen = HashSet::new();
        for port in &list {
            assert!(!port.id.as_str().trim().is_empty());
            assert!(!port.display_name.trim().is_empty());
            assert!(seen.insert(&port.id), "duplicate id {}", port.id);
            if let Some(suffix) = port.id.as_str().strip_prefix(MAC_DIALIN_PREFIX) {
                assert!(
                    !list
                        .iter()
                        .any(|p| p.id.as_str() == format!("{MAC_CALLOUT_PREFIX}{suffix}")),
                    "{} kept next to its cu twin",
                    port.id
                );
            }
        }
    }

    #[test]
    fn real_source_subscription_starts_with_a_snapshot() {
        let source = RealPortSource::new();
        let rx = source.subscribe();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(PortEvent::Snapshot(_)) => {}
            other => panic!("expected a Snapshot first, got {other:?}"),
        }
        // The plain call must also be safe on any machine.
        let _ = source.snapshot();
    }

    // ---- the source with an injected enumerator --------------------------------

    /// A controllable port list plus a call counter, standing in for the OS.
    #[derive(Clone)]
    struct FakeBus {
        ports: Arc<Mutex<Vec<PortInfo>>>,
        calls: Arc<AtomicUsize>,
        failing: Arc<AtomicBool>,
        /// How long each enumeration takes, like a slow SetupAPI scan.
        delay: Arc<Mutex<Duration>>,
        /// Dropped with the source's last reference to the enumerator.
        probe: Arc<()>,
    }

    impl FakeBus {
        fn new(initial: &[&str]) -> Self {
            Self {
                ports: Arc::new(Mutex::new(initial.iter().map(|n| info(n)).collect())),
                calls: Arc::new(AtomicUsize::new(0)),
                failing: Arc::new(AtomicBool::new(false)),
                delay: Arc::new(Mutex::new(Duration::ZERO)),
                probe: Arc::new(()),
            }
        }

        fn set(&self, names: &[&str]) {
            *self.ports.lock() = names.iter().map(|n| info(n)).collect();
        }

        fn calls(&self) -> usize {
            self.calls.load(AtomicOrdering::SeqCst)
        }

        /// Blocks until the monitor thread has finished its startup enumeration, so a
        /// test that changes the list next cannot have the change swallowed by it.
        fn wait_for_monitor_startup(&self) {
            let deadline = Instant::now() + PATIENCE;
            // One call from `RealPortSource::new`, one from the monitor's first pass.
            while self.calls() < 2 {
                assert!(Instant::now() < deadline, "monitor never started");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn enumerator(&self) -> Enumerator {
            let bus = self.clone();
            Box::new(move || {
                // Borrow the whole bus so the closure owns the probe clone too.
                let bus = &bus;
                bus.calls.fetch_add(1, AtomicOrdering::SeqCst);
                let delay = *bus.delay.lock();
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
                if bus.failing.load(AtomicOrdering::SeqCst) {
                    return Err(serialport::Error::new(
                        serialport::ErrorKind::Unknown,
                        "simulated enumeration failure",
                    ));
                }
                Ok(bus.ports.lock().clone())
            })
        }

        fn source(&self, debounce: Duration, poll_interval: Duration) -> RealPortSource {
            RealPortSource::with_parts(
                self.enumerator(),
                MonitorOptions {
                    debounce,
                    poll_interval,
                    watch_usb: false,
                },
            )
        }
    }

    const LONG: Duration = Duration::from_secs(60);
    const PATIENCE: Duration = Duration::from_secs(5);

    fn expect_snapshot(rx: &Receiver<PortEvent>) -> Vec<PortInfo> {
        match rx.recv_timeout(PATIENCE) {
            Ok(PortEvent::Snapshot(list)) => list,
            other => panic!("expected Snapshot, got {other:?}"),
        }
    }

    #[test]
    fn subscription_gets_snapshot_first_then_events() {
        let bus = FakeBus::new(&["A", "B"]);
        let source = bus.source(Duration::from_millis(20), LONG);
        let rx = source.subscribe();
        assert_eq!(ids(&expect_snapshot(&rx)), ["A", "B"]);
        assert!(rx.try_recv().is_err(), "nothing else queued yet");

        bus.set(&["B", "C"]);
        source.request_refresh();
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Removed(PortId::new("A"))
        );
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("C"))
        );
    }

    #[test]
    fn snapshot_returns_the_cache_without_enumerating_on_the_callers_thread() {
        let bus = FakeBus::new(&["A"]);
        let source = bus.source(Duration::from_millis(200), LONG);
        assert_eq!(ids(&source.snapshot()), ["A"]);
        bus.wait_for_monitor_startup();

        // A device appears, and every enumeration now takes 400 ms.
        bus.set(&["A", "B"]);
        *bus.delay.lock() = Duration::from_millis(400);
        let before = bus.calls();
        let started = Instant::now();
        let listed = source.snapshot();
        let took = started.elapsed();

        assert_eq!(ids(&listed), ["A"], "the cache, not a fresh scan");
        assert!(
            took < Duration::from_millis(150),
            "snapshot blocked for {took:?}"
        );
        assert_eq!(
            bus.calls(),
            before,
            "snapshot enumerated on the caller's thread"
        );
    }

    #[test]
    fn snapshot_requests_a_refresh_that_reaches_subscribers_and_later_snapshots() {
        let bus = FakeBus::new(&["A"]);
        let source = bus.source(Duration::from_millis(20), LONG);
        let rx = source.subscribe();
        expect_snapshot(&rx);
        bus.wait_for_monitor_startup();

        bus.set(&["A", "B"]);
        assert_eq!(ids(&source.snapshot()), ["A"], "stale for now");
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("B")),
            "the nudge made the monitor refresh"
        );
        assert_eq!(ids(&source.snapshot()), ["A", "B"]);
    }

    #[test]
    fn overlapping_refreshes_never_apply_a_stale_result() {
        // Enumeration #0 is slow and reports the old list (its picture was taken before
        // B appeared). Enumeration #1 is fast and reports the new one. Started from two
        // threads, an unserialised refresh applies #1 first and then #0 over it, which
        // shows up as a spurious `Removed(B)` and a stale cache until the next poll.
        let calls = Arc::new(AtomicUsize::new(0));
        let enumerate: Enumerator = {
            let calls = Arc::clone(&calls);
            Box::new(move || match calls.fetch_add(1, AtomicOrdering::SeqCst) {
                0 => {
                    let old = vec![info("A")];
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(old)
                }
                _ => Ok(vec![info("A"), info("B")]),
            })
        };
        let (tx, rx) = unbounded();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                current: vec![info("A")],
                subscribers: vec![tx],
            }),
            refresh_lock: Mutex::new(()),
            enumerate,
            enumerate_failing: AtomicBool::new(false),
        });

        let slow = std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || shared.refresh()
        });
        while calls.load(AtomicOrdering::SeqCst) < 1 {
            std::thread::sleep(Duration::from_millis(1)); // the slow scan is under way
        }
        let fast = std::thread::spawn({
            let shared = Arc::clone(&shared);
            move || shared.refresh()
        });
        slow.join().unwrap();
        fast.join().unwrap();

        assert_eq!(
            ids(&shared.state.lock().current),
            ["A", "B"],
            "cache went stale"
        );
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![PortEvent::Added(info("B"))],
            "subscribers must see B arrive and never leave"
        );
    }

    #[test]
    fn late_subscriber_snapshot_includes_earlier_changes() {
        let bus = FakeBus::new(&["A"]);
        let source = bus.source(Duration::from_millis(20), LONG);
        let first = source.subscribe();
        expect_snapshot(&first);
        bus.set(&["A", "B"]);
        source.request_refresh();
        assert_eq!(
            first.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("B"))
        );

        let second = source.subscribe();
        assert_eq!(ids(&expect_snapshot(&second)), ["A", "B"]);
    }

    #[test]
    fn every_subscriber_sees_every_event() {
        let bus = FakeBus::new(&[]);
        let source = bus.source(Duration::from_millis(20), LONG);
        let a = source.subscribe();
        let b = source.subscribe();
        assert!(expect_snapshot(&a).is_empty());
        assert!(expect_snapshot(&b).is_empty());

        bus.set(&["X"]);
        source.request_refresh();
        assert_eq!(
            a.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("X"))
        );
        assert_eq!(
            b.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("X"))
        );
    }

    #[test]
    fn dropped_receivers_are_pruned_on_the_next_change() {
        let bus = FakeBus::new(&[]);
        let source = bus.source(Duration::from_millis(20), LONG);
        let keep = source.subscribe();
        let gone = source.subscribe();
        expect_snapshot(&keep);
        assert_eq!(source.subscriber_count(), 2);
        drop(gone);

        bus.set(&["X"]);
        source.request_refresh();
        assert_eq!(
            keep.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("X"))
        );
        assert_eq!(source.subscriber_count(), 1);
    }

    #[test]
    fn polling_alone_finds_changes_without_any_hotplug_event() {
        let bus = FakeBus::new(&["A"]);
        // Debounce never fires and nobody kicks: only the poll timer can notice.
        let source = bus.source(LONG, Duration::from_millis(40));
        let rx = source.subscribe();
        expect_snapshot(&rx);
        bus.set(&["A", "BT"]);
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("BT"))
        );
        bus.set(&["A"]);
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Removed(PortId::new("BT"))
        );
    }

    #[test]
    fn a_hotplug_event_is_reported_after_the_debounce_not_before() {
        let debounce = Duration::from_millis(150);
        let bus = FakeBus::new(&[]);
        let source = bus.source(debounce, LONG);
        let rx = source.subscribe();
        expect_snapshot(&rx);
        bus.wait_for_monitor_startup();

        bus.set(&["NEW"]);
        let started = Instant::now();
        source.request_refresh();
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("NEW"))
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed >= debounce - Duration::from_millis(10),
            "reported after {elapsed:?}, before the debounce elapsed"
        );
    }

    #[test]
    fn bursts_of_events_coalesce_into_one_enumeration() {
        let bus = FakeBus::new(&[]);
        let source = bus.source(Duration::from_millis(300), LONG);
        let rx = source.subscribe();
        expect_snapshot(&rx);
        bus.wait_for_monitor_startup();
        let before = bus.calls();

        bus.set(&["A"]);
        for _ in 0..50 {
            source.request_refresh();
        }
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Added(info("A"))
        );
        std::thread::sleep(Duration::from_millis(100));
        let extra = bus.calls() - before;
        assert!(
            (1..=3).contains(&extra),
            "50 requests caused {extra} enumerations"
        );
    }

    #[test]
    fn enumeration_failure_keeps_the_last_list_and_recovers() {
        let bus = FakeBus::new(&["A"]);
        let source = bus.source(Duration::from_millis(20), Duration::from_millis(30));
        let rx = source.subscribe();
        assert_eq!(ids(&expect_snapshot(&rx)), ["A"]);

        bus.failing.store(true, AtomicOrdering::SeqCst);
        bus.set(&[]); // would be a removal if the failure were mistaken for an empty list
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            rx.try_recv().is_err(),
            "a failed enumeration emitted events"
        );
        assert_eq!(
            ids(&source.snapshot()),
            ["A"],
            "snapshot falls back to the cache"
        );

        bus.failing.store(false, AtomicOrdering::SeqCst);
        assert_eq!(
            rx.recv_timeout(PATIENCE).unwrap(),
            PortEvent::Removed(PortId::new("A"))
        );
    }

    #[test]
    fn dropping_the_source_stops_the_thread() {
        let bus = FakeBus::new(&["A"]);
        let source = bus.source(Duration::from_millis(10), Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(50));
        assert!(bus.calls() > 1, "monitor should be polling");
        assert_eq!(
            Arc::strong_count(&bus.probe),
            2,
            "source holds the enumerator"
        );
        let rx = source.subscribe();

        drop(source);
        // Drop joins the thread, so by now the enumerator (and the probe clone inside it)
        // is gone and the call count cannot move any more.
        assert_eq!(Arc::strong_count(&bus.probe), 1);
        let after_drop = bus.calls();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            bus.calls(),
            after_drop,
            "enumerator ran after the source dropped"
        );

        // Subscribers see their queued snapshot, then the end of the stream.
        assert!(matches!(
            rx.recv_timeout(PATIENCE),
            Ok(PortEvent::Snapshot(_))
        ));
        assert_eq!(
            rx.recv_timeout(PATIENCE),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected)
        );
    }

    #[test]
    fn usb_watcher_starts_or_degrades_without_panicking() {
        let (tx, _rx) = bounded::<()>(1);
        let mut watcher = UsbWatcher::start(tx, POLL_INTERVAL);
        // Whether nusb could start depends on the machine; either way draining is safe
        // and reports no event when nothing happened.
        assert!(!watcher.drain() || watcher.watch.is_some());
    }

    /// Manual check of the nusb wiring: run with `--ignored --nocapture`, then plug or
    /// unplug any USB device within 30 seconds. Needs a human, so it is never run by
    /// default, and it is the only test in this crate that does.
    #[test]
    #[ignore = "needs a person to plug or unplug a USB device while it runs"]
    fn hotplug_event_wakes_the_monitor() {
        let (tx, rx) = bounded::<()>(1);
        let mut watcher = UsbWatcher::start(tx, POLL_INTERVAL);
        assert!(watcher.watch.is_some(), "nusb hotplug is unavailable here");
        watcher.drain();
        eprintln!("plug or unplug a USB device now...");
        rx.recv_timeout(Duration::from_secs(30))
            .expect("no hotplug wakeup within 30 s");
        assert!(watcher.drain(), "woken, but no event was queued");
    }

    #[test]
    fn source_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RealPortSource>();
    }
}
