//! What the shared interface needs from the phone it runs on.
//!
//! Each platform shell (`antiphon-android`, `antiphon-ios`) implements
//! [`Platform`] once and hands it over with [`install`] before the event
//! loop starts. The free functions below are what the rest of this crate
//! calls, so `ui` and `call` never name a platform. Every method fails
//! soft: a phone that refuses one still gets a working app, just without
//! that convenience, which is why the defaults do nothing.

use std::sync::{Mutex, OnceLock};

pub trait Platform: Send + Sync + 'static {
    /// Whether the user has granted microphone access. Both platforms ask
    /// at run time, so this can change while the app is open.
    fn has_mic_permission(&self) -> bool;
    /// Shows the system's microphone prompt. The answer arrives later;
    /// callers check [`Platform::has_mic_permission`] again on resume.
    fn request_mic_permission(&self);
    /// Puts the audio session in voice-call mode (platform echo
    /// cancellation, earpiece unless `speaker`), or back to normal.
    fn set_call_audio(&self, in_call: bool, speaker: bool);
    fn copy_to_clipboard(&self, text: &str);
    fn read_clipboard(&self) -> Option<String>;
    /// The parts of the screen the system draws over, in physical pixels,
    /// as (top, bottom): status bar, home indicator or gesture bar, and
    /// the soft keyboard while it is up.
    fn system_insets(&self) -> (f32, f32) {
        (0.0, 0.0)
    }

    /// What the phone is connected through now, for "only on Wi-Fi" and
    /// cellular limits.
    fn network(&self) -> Network {
        Network::Unknown
    }
    /// Whether the system's battery saver is on.
    fn battery_saver(&self) -> bool {
        false
    }
    /// Tells the platform what must keep running while the app is not in
    /// front: a call (Android foreground service, iOS CallKit and the
    /// background audio session) and file transfers. Called on every
    /// change, including back to nothing.
    fn set_background_work(&self, _work: &BackgroundWork) {}
    /// Opens the system file picker. The answer arrives later as
    /// [`PlatformEvent::FilePicked`] or [`PlatformEvent::FilePickCancelled`].
    fn pick_file(&self) {}
    /// Reads a file the user picked, by the handle in [`PickedFile`]. The
    /// handle survives a restart (a persisted content URI on Android, a
    /// security-scoped bookmark on iOS), which a raw path does not.
    fn read_file(&self, _handle: &str) -> std::io::Result<Vec<u8>> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
    /// Where received files go so the user can find them, as a path the
    /// app can write; `None` keeps them in private storage.
    fn downloads_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// Shows a notification: a message, a missed call, a finished file.
    fn notify(&self, _title: &str, _body: &str) {}
    /// Registers for push wake-ups. The address to give contacts arrives
    /// as [`PlatformEvent::PushRegistered`].
    fn register_push(&self) {}

    // Added after the contract above was first agreed. Each has a default
    // that does nothing, so a platform that has not got to it yet still
    // builds.

    /// Shows an incoming-call alert that can light the screen and open the
    /// app over the lock screen, with Answer and Decline (reported as
    /// [`PlatformEvent::CallAction`]); `None` withdraws it. For when the
    /// app is not in front: a platform may show nothing while it is.
    fn ring(&self, _from: Option<&str>) {}
    /// Asks the user, through the system's own prompt, to exempt the app
    /// from battery optimisation so the network stays up while it listens
    /// for calls in the background. Does nothing if it is already exempt.
    fn request_battery_exemption(&self) {}
}

/// The network the phone is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    #[default]
    Unknown,
    None,
    Wifi,
    /// Mobile data, or any network the system marks as metered.
    Metered,
}

/// What has to keep running while the app is in the background.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackgroundWork {
    /// The person a call is with, or `None` when no call is running.
    pub call: Option<String>,
    /// Listening for incoming calls.
    pub listening: bool,
    /// File transfers in progress.
    pub transfers: usize,
}

/// A file the user picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickedFile {
    /// Opaque to the shared code; pass it back to [`Platform::read_file`].
    pub handle: String,
    pub name: String,
    pub size: Option<u64>,
}

/// Call controls pressed outside the app: a notification button, the
/// lock screen, CallKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    Answer,
    Decline,
    HangUp,
    Mute(bool),
}

/// Something the platform reports back to the shared code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformEvent {
    FilePicked(PickedFile),
    FilePickCancelled,
    CallAction(CallAction),
    /// The push address contacts should send wake-ups to.
    PushRegistered(String),
    /// A push arrived. The bytes are the sealed payload, still encrypted.
    Push(Vec<u8>),
    NetworkChanged(Network),
}

static PLATFORM: OnceLock<Box<dyn Platform>> = OnceLock::new();
static EVENTS: Mutex<Vec<PlatformEvent>> = Mutex::new(Vec::new());
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Queues an event for the shared code and wakes the event loop. Safe
/// from any thread, including a JNI callback or an Objective-C delegate.
pub fn post(event: PlatformEvent) {
    EVENTS.lock().unwrap().push(event);
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// Everything posted since the last call.
pub(crate) fn take_events() -> Vec<PlatformEvent> {
    std::mem::take(&mut *EVENTS.lock().unwrap())
}

/// How [`post`] wakes the event loop; set once by the runner.
/// Events posted before this -- a push that started the process, a
/// network change during start-up -- would otherwise wait for some
/// unrelated wake-up, so they get one now.
pub(crate) fn set_waker(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(wake));
    if !EVENTS.lock().unwrap().is_empty() {
        if let Some(wake) = WAKE.get() {
            wake();
        }
    }
}

/// Hands the shared code its platform. Called once, before anything else
/// in this crate runs; a second call is ignored.
pub fn install(platform: impl Platform) {
    let _ = PLATFORM.set(Box::new(platform));
}

fn get() -> Option<&'static dyn Platform> {
    PLATFORM.get().map(|p| p.as_ref())
}

pub fn has_mic_permission() -> bool {
    get().is_some_and(|p| p.has_mic_permission())
}

pub fn request_mic_permission() {
    if let Some(p) = get() {
        p.request_mic_permission();
    }
}

pub fn set_call_audio(in_call: bool, speaker: bool) {
    if let Some(p) = get() {
        p.set_call_audio(in_call, speaker);
    }
}

pub fn copy_to_clipboard(text: &str) {
    if let Some(p) = get() {
        p.copy_to_clipboard(text);
    }
}

pub fn read_clipboard() -> Option<String> {
    get().and_then(|p| p.read_clipboard())
}

pub fn system_insets() -> (f32, f32) {
    get().map_or((0.0, 0.0), |p| p.system_insets())
}

pub fn network() -> Network {
    get().map_or(Network::Unknown, |p| p.network())
}

pub fn battery_saver() -> bool {
    get().is_some_and(|p| p.battery_saver())
}

pub fn set_background_work(work: &BackgroundWork) {
    if let Some(p) = get() {
        p.set_background_work(work);
    }
}

pub fn pick_file() {
    if let Some(p) = get() {
        p.pick_file();
    }
}

pub fn read_file(handle: &str) -> std::io::Result<Vec<u8>> {
    get().map_or(Err(std::io::ErrorKind::Unsupported.into()), |p| p.read_file(handle))
}

pub fn downloads_dir() -> Option<std::path::PathBuf> {
    get().and_then(|p| p.downloads_dir())
}

pub fn notify(title: &str, body: &str) {
    if let Some(p) = get() {
        p.notify(title, body);
    }
}

pub fn register_push() {
    if let Some(p) = get() {
        p.register_push();
    }
}

pub fn ring(from: Option<&str>) {
    if let Some(p) = get() {
        p.ring(from);
    }
}

pub fn request_battery_exemption() {
    if let Some(p) = get() {
        p.request_battery_exemption();
    }
}
