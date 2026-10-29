//! Screen readers: hands the interface's AccessKit tree to the platform's
//! accessibility API and brings the reader's requests back.
//!
//! The tree itself is built by the shared interface
//! ([`antiphon_ui::a11y::tree`]) from the controls each frame registered.
//! This module only connects it to the system:
//!
//! - **Windows** (UI Automation, so Narrator and NVDA) and **macOS**
//!   (NSAccessibility, so VoiceOver) through AccessKit's subclassing
//!   adapters, which hook the window itself. Both must be attached before
//!   the window is first shown, so on those systems the window starts
//!   hidden and [`install`] shows it once the adapter is in place.
//! - **Linux and the BSDs** (AT-SPI over D-Bus, so Orca) through
//!   AccessKit's Unix adapter, which needs no window handle.
//!
//! [`publish`] runs after each view is built and sends the tree only when
//! it changed, so a reader isn't told about frames where nothing did.
//! Requests from the reader (press this, focus that, step this slider) come
//! back through [`requests`] as [`Msg::Reader`], the same path the keyboard
//! takes.

use std::sync::{Mutex, OnceLock};

use accesskit::{ActionHandler, ActionRequest, ActivationHandler, TreeUpdate};
use antiphon_ui::a11y::{self, ReaderAction};
use antiphon_ui::Msg;
use iced::futures::channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use iced::futures::{stream, Stream, StreamExt};

/// The tree last sent, for the adapter's first request and to skip
/// sending a tree that did not change.
static LATEST: Mutex<Option<TreeUpdate>> = Mutex::new(None);

/// Where reader requests go: the sending half, used from the adapter's
/// threads.
static SENDER: OnceLock<UnboundedSender<(u64, ReaderAction)>> = OnceLock::new();

/// The receiving half, taken once by the subscription.
static RECEIVER: Mutex<Option<UnboundedReceiver<(u64, ReaderAction)>>> = Mutex::new(None);

/// The request channel's sender, made on first use (with its receiver
/// parked for [`requests`]).
fn sender() -> &'static UnboundedSender<(u64, ReaderAction)> {
    SENDER.get_or_init(|| {
        let (tx, rx) = unbounded();
        *RECEIVER.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
        tx
    })
}

/// Answers the adapter: the first tree it asks for is the latest one.
struct Activation;

impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// Passes the reader's requests to the interface, as messages.
struct Actions;

impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if let Some(act) = ReaderAction::from_accesskit(request.action) {
            // The window may be closing; a request then has nowhere to go.
            let _ = sender().unbounded_send((request.target_node.0, act));
        }
    }
}

/// Nothing to drop when the reader leaves: the tree is rebuilt every frame.
#[cfg(all(unix, not(target_os = "macos")))]
struct Deactivation;

#[cfg(all(unix, not(target_os = "macos")))]
impl accesskit::DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {}
}

/// The AT-SPI adapter. Made on first use; it needs no window.
#[cfg(all(unix, not(target_os = "macos")))]
static ADAPTER: Mutex<Option<accesskit_unix::Adapter>> = Mutex::new(None);

#[cfg(any(windows, target_os = "macos"))]
thread_local! {
    /// The window's subclassing adapter. It is tied to the window's thread
    /// (the main thread, where iced also builds views), so it lives there.
    #[cfg(windows)]
    static ADAPTER: std::cell::RefCell<Option<accesskit_windows::SubclassingAdapter>> = const { std::cell::RefCell::new(None) };
    /// The window's subclassing adapter (see the Windows one above).
    #[cfg(target_os = "macos")]
    static ADAPTER: std::cell::RefCell<Option<accesskit_macos::SubclassingAdapter>> = const { std::cell::RefCell::new(None) };
}

/// Whether the window must start hidden so [`install`] can attach the
/// adapter before it is shown: true on Windows and macOS.
pub const START_HIDDEN: bool = cfg!(any(windows, target_os = "macos"));

/// Attaches the screen-reader adapter to `window`. Call it on the window's
/// thread before the window is first shown (see [`START_HIDDEN`]); on
/// Linux it starts the AT-SPI adapter instead and ignores the window.
pub fn install(window: &dyn iced::window::Window) {
    let _ = sender();
    #[cfg(windows)]
    {
        use iced::window::raw_window_handle::RawWindowHandle;
        let Ok(handle) = window.window_handle() else { return };
        if let RawWindowHandle::Win32(h) = handle.as_raw() {
            let hwnd = accesskit_windows::HWND(h.hwnd.get() as *mut std::ffi::c_void);
            let adapter = accesskit_windows::SubclassingAdapter::new(hwnd, Activation, Actions);
            ADAPTER.with(|a| *a.borrow_mut() = Some(adapter));
        }
    }
    #[cfg(target_os = "macos")]
    {
        use iced::window::raw_window_handle::RawWindowHandle;
        let Ok(handle) = window.window_handle() else { return };
        if let RawWindowHandle::AppKit(h) = handle.as_raw() {
            // SAFETY: the handle's view is the live window's content view,
            // and the adapter retains it.
            let adapter = unsafe { accesskit_macos::SubclassingAdapter::new(h.ns_view.as_ptr(), Activation, Actions) };
            ADAPTER.with(|a| *a.borrow_mut() = Some(adapter));
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = window;
        let mut adapter = ADAPTER.lock().unwrap_or_else(|e| e.into_inner());
        if adapter.is_none() {
            *adapter = Some(accesskit_unix::Adapter::new(Activation, Actions, Deactivation));
        }
    }
}

/// Sends the frame just built to the reader, as a window named `title`,
/// when it differs from the last one sent.
pub fn publish(title: &str) {
    let tree = a11y::tree(title);
    {
        let mut latest = LATEST.lock().unwrap_or_else(|e| e.into_inner());
        if latest.as_ref() == Some(&tree) {
            return;
        }
        *latest = Some(tree.clone());
    }
    #[cfg(any(windows, target_os = "macos"))]
    ADAPTER.with(|a| {
        if let Some(adapter) = a.borrow_mut().as_mut() {
            if let Some(events) = adapter.update_if_active(|| tree) {
                events.raise();
            }
        }
    });
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(adapter) = ADAPTER.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        adapter.update_if_active(|| tree);
    }
}

/// Tells the reader whether the window has the system's focus (Windows
/// tracks this itself through the hooked window).
pub fn window_focused(focused: bool) {
    #[cfg(target_os = "macos")]
    ADAPTER.with(|a| {
        if let Some(adapter) = a.borrow_mut().as_mut() {
            if let Some(events) = adapter.update_view_focus_state(focused) {
                events.raise();
            }
        }
    });
    #[cfg(all(unix, not(target_os = "macos")))]
    if let Some(adapter) = ADAPTER.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        adapter.update_window_focus_state(focused);
    }
    #[cfg(windows)]
    let _ = focused;
}

/// The reader's requests, as interface messages.
pub fn requests() -> iced::Subscription<Msg> {
    iced::Subscription::run(request_stream)
}

/// The stream behind [`requests`]: the channel's receiving half, or
/// nothing if it was already taken.
fn request_stream() -> impl Stream<Item = Msg> {
    let _ = sender();
    let rx = RECEIVER.lock().unwrap_or_else(|e| e.into_inner()).take();
    match rx {
        Some(rx) => rx.map(|(id, act)| Msg::Reader(id, act)).boxed(),
        None => stream::pending().boxed(),
    }
}
