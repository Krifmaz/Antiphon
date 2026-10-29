//! What the phone is connected through, from the Network framework's path
//! monitor.
//!
//! `nw_path_monitor` is a C API with no objc2 binding, so the handful of
//! functions it takes are declared here. The monitor calls back on its own
//! queue whenever the path changes; the answer is kept in an atomic so
//! [`current`] never waits, and each change is posted to the shared code.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, Ordering};

use antiphon_mobile::platform::{self, Network, PlatformEvent};
use block2::{DynBlock, RcBlock};
use dispatch2::DispatchQueue;

/// `nw_path_monitor_t` and `nw_path_t`: Objective-C objects, used only
/// through the functions below.
type NwObject = *mut c_void;

/// `nw_path_status_satisfied`.
const STATUS_SATISFIED: i32 = 1;
/// `nw_interface_type_wifi`.
const INTERFACE_WIFI: i32 = 1;
/// `nw_interface_type_cellular`.
const INTERFACE_CELLULAR: i32 = 2;
/// `nw_interface_type_wired`: Ethernet, through an adapter.
const INTERFACE_WIRED: i32 = 3;

#[link(name = "Network", kind = "framework")]
extern "C" {
    /// A new monitor for every interface, retained (+1); null on failure.
    fn nw_path_monitor_create() -> NwObject;
    /// The queue the update handler runs on. Must be set before starting.
    fn nw_path_monitor_set_queue(monitor: NwObject, queue: &DispatchQueue);
    /// The block called with each new path, on the monitor's queue. The
    /// monitor copies the block.
    fn nw_path_monitor_set_update_handler(monitor: NwObject, handler: &DynBlock<dyn Fn(NwObject)>);
    /// Starts monitoring; the handler is called at once with the current
    /// path, then on every change.
    fn nw_path_monitor_start(monitor: NwObject);
    /// The path's `nw_path_status_t`; [`STATUS_SATISFIED`] when usable.
    fn nw_path_get_status(path: NwObject) -> i32;
    /// Whether the path is cellular or a personal hotspot.
    fn nw_path_is_expensive(path: NwObject) -> bool;
    /// Whether the user has Low Data Mode on for the path's network.
    fn nw_path_is_constrained(path: NwObject) -> bool;
    /// Whether the path goes through an interface of the given type.
    fn nw_path_uses_interface_type(path: NwObject, interface: i32) -> bool;
}

/// The last [`Network`] the monitor reported, [`encode`]d, written on the
/// monitor's queue and read from any thread. Relaxed ordering: it is one
/// value standing alone, with nothing else published alongside it.
static CURRENT: AtomicU8 = AtomicU8::new(encode(Network::Unknown));

/// A [`Network`] as a byte, to fit in [`CURRENT`]. `const` so the static
/// can start at `Unknown`.
const fn encode(network: Network) -> u8 {
    match network {
        Network::Unknown => 0,
        Network::None => 1,
        Network::Wifi => 2,
        Network::Metered => 3,
    }
}

/// The inverse of [`encode`]; anything unexpected reads as `Unknown`.
fn decode(value: u8) -> Network {
    match value {
        1 => Network::None,
        2 => Network::Wifi,
        3 => Network::Metered,
        _ => Network::Unknown,
    }
}

/// The network as last reported.
pub fn current() -> Network {
    decode(CURRENT.load(Ordering::Relaxed))
}

/// Reads one path. "Expensive" (cellular, or a phone's hotspot) and
/// "constrained" (Low Data Mode) both count as metered, whatever the
/// interface: the user has said, one way or another, not to spend data.
///
/// # Safety
///
/// `path` must be a live `nw_path_t`.
unsafe fn classify(path: NwObject) -> Network {
    if nw_path_get_status(path) != STATUS_SATISFIED {
        Network::None
    } else if nw_path_is_expensive(path) || nw_path_is_constrained(path) {
        Network::Metered
    } else if nw_path_uses_interface_type(path, INTERFACE_WIFI) || nw_path_uses_interface_type(path, INTERFACE_WIRED) {
        Network::Wifi
    } else if nw_path_uses_interface_type(path, INTERFACE_CELLULAR) {
        Network::Metered
    } else {
        Network::Unknown
    }
}

/// Starts the monitor for the life of the process. It is never stopped or
/// released: there is one, and the app wants to know until it exits.
pub fn start() {
    let queue = DispatchQueue::new("org.antiphon.network", None);
    let handler = RcBlock::new(|path: NwObject| {
        if path.is_null() {
            return;
        }
        // SAFETY: the monitor hands its handler a live path.
        let network = unsafe { classify(path) };
        let previous = CURRENT.swap(encode(network), Ordering::Relaxed);
        if previous != encode(network) {
            log::info!("network: {network:?}");
            platform::post(PlatformEvent::NetworkChanged(network));
            crate::shell::wake();
        }
    });
    // SAFETY: the monitor is checked for null before use; the queue and
    // the handler are retained by it (the handler is copied).
    unsafe {
        let monitor = nw_path_monitor_create();
        if monitor.is_null() {
            log::warn!("no network path monitor");
            return;
        }
        nw_path_monitor_set_queue(monitor, &queue);
        nw_path_monitor_set_update_handler(monitor, &handler);
        nw_path_monitor_start(monitor);
    }
}
