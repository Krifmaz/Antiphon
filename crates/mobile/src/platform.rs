//! What the shared interface needs from the phone it runs on.
//!
//! Each platform shell (`antiphon-android`, `antiphon-ios`) implements
//! [`Platform`] once and hands it over with [`install`] before the event
//! loop starts. The free functions below are what the rest of this crate
//! calls, so `ui` and `call` never name a platform. Every method fails
//! soft: a phone that refuses one still gets a working app, just without
//! that convenience, which is why the defaults do nothing.

use std::sync::{Mutex, OnceLock};

/// The phone-specific half of the app, implemented once by each
/// platform shell and installed with [`install`].
///
/// `Send + Sync` because the shared code calls it from the interface
/// thread and the call's own threads alike; an implementation that must
/// run on the main thread (most of UIKit, some Android APIs) hops there
/// itself. Methods return quickly: anything that waits on the user, such
/// as a permission prompt or the file picker, answers later through
/// [`post`].
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
    /// Puts text on the system clipboard, for sharing an invite link.
    fn copy_to_clipboard(&self, text: &str);
    /// The clipboard's text, or `None` when it is empty, holds something
    /// else, or the system refuses to hand it over.
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
    // that does nothing, so a platform that has no use for it need not
    // implement it.

    /// The call in [`BackgroundWork::call`] is now connected: answered on
    /// this phone, or picked up at the other end. `BackgroundWork` only
    /// says that a call exists, which is not enough for CallKit: it shows
    /// "Calling..." until told an outgoing call connected, and keeps an
    /// incoming call ringing on its own screen when it was answered in the
    /// app instead.
    fn call_connected(&self) {}
    /// Shows an incoming-call alert that can light the screen and open the
    /// app over the lock screen, with Answer and Decline (reported as
    /// [`PlatformEvent::CallAction`]); `None` withdraws it. For when the
    /// app is not in front: a platform may show nothing while it is.
    fn ring(&self, _from: Option<&str>) {}
    /// Asks the user, through the system's own prompt, to exempt the app
    /// from battery optimisation so the network stays up while it listens
    /// for calls in the background. Does nothing if it is already exempt.
    fn request_battery_exemption(&self) {}

    // The camera, for sending video on a call. A platform that leaves
    // these as they are has no camera button: [`Platform::has_camera`]
    // says no.

    /// Whether this platform can run a camera through
    /// [`Platform::start_camera`] at all. Decides whether the call screen
    /// offers a camera button; a phone whose camera is refused or busy
    /// still says yes here and fails when it is started.
    fn has_camera(&self) -> bool {
        false
    }
    /// Whether the user has allowed the camera. Asked at run time on both
    /// platforms, so this can change while the app is open.
    fn has_camera_permission(&self) -> bool {
        false
    }
    /// Shows the system's camera prompt. The answer arrives later; the
    /// shared code checks [`Platform::has_camera_permission`] again when
    /// the app resumes, as for the microphone.
    fn request_camera_permission(&self) {}
    /// Starts the camera on `request.facing`'s side, as near the asked
    /// size and rate as it offers, closing whichever camera was open
    /// first (that is how the shared code switches cameras). Frames go to
    /// [`camera_frame`] on any thread; a camera that stops by itself
    /// afterwards says so through [`camera_failed`]. The error is a
    /// sentence for the user, for a camera that cannot start at all.
    fn start_camera(&self, _request: crate::camera::CameraRequest) -> Result<(), String> {
        Err("This phone's camera is not supported yet".into())
    }
    /// Closes the camera, and with it the camera light. Does nothing when
    /// none is open.
    fn stop_camera(&self) {}
}

/// The network the phone is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Network {
    /// The platform cannot tell. The transfer rules treat this as
    /// unmetered, so a shell that never reports the network does not hold
    /// every file forever.
    #[default]
    Unknown,
    /// No network at all.
    None,
    /// An unmetered network.
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
    /// The file's name as the picker reported it, shown to both sides.
    pub name: String,
    /// Its size in bytes, when the picker knows it; some providers do not
    /// report one until the file is read.
    pub size: Option<u64>,
}

/// Call controls pressed outside the app: a notification button, the
/// lock screen, CallKit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallAction {
    /// Pick up the ringing call.
    Answer,
    /// Refuse the ringing call.
    Decline,
    /// End the call in progress.
    HangUp,
    /// Mute (`true`) or unmute the microphone, from CallKit or a
    /// notification button.
    Mute(bool),
}

/// Something the platform reports back to the shared code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformEvent {
    /// The user chose a file in the picker opened by [`Platform::pick_file`].
    FilePicked(PickedFile),
    /// The user closed the picker without choosing.
    FilePickCancelled,
    /// A call control pressed outside the app.
    CallAction(CallAction),
    /// The push address contacts should send wake-ups to.
    PushRegistered(String),
    /// A push arrived. The bytes are the sealed payload, still encrypted.
    Push(Vec<u8>),
    /// The phone moved to a different kind of network, or lost it.
    NetworkChanged(Network),
}

/// The installed platform; empty until [`install`], when every wrapper
/// below quietly does nothing.
static PLATFORM: OnceLock<Box<dyn Platform>> = OnceLock::new();
/// Events posted by the platform and not yet taken by the runner, oldest
/// first. A mutex because [`post`] runs on whatever thread the platform
/// calls back on.
static EVENTS: Mutex<Vec<PlatformEvent>> = Mutex::new(Vec::new());
/// Wakes the event loop after a [`post`]; set once by the runner.
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

/// The installed platform, if [`install`] has run.
fn get() -> Option<&'static dyn Platform> {
    PLATFORM.get().map(|p| p.as_ref())
}

/// [`Platform::has_mic_permission`], or `false` before a platform is
/// installed.
pub fn has_mic_permission() -> bool {
    get().is_some_and(|p| p.has_mic_permission())
}

/// [`Platform::request_mic_permission`].
pub fn request_mic_permission() {
    if let Some(p) = get() {
        p.request_mic_permission();
    }
}

/// [`Platform::set_call_audio`].
pub fn set_call_audio(in_call: bool, speaker: bool) {
    if let Some(p) = get() {
        p.set_call_audio(in_call, speaker);
    }
}

/// [`Platform::copy_to_clipboard`].
pub fn copy_to_clipboard(text: &str) {
    if let Some(p) = get() {
        p.copy_to_clipboard(text);
    }
}

/// [`Platform::read_clipboard`].
pub fn read_clipboard() -> Option<String> {
    get().and_then(|p| p.read_clipboard())
}

/// [`Platform::system_insets`], in physical pixels as (top, bottom), or
/// none before a platform is installed.
pub fn system_insets() -> (f32, f32) {
    get().map_or((0.0, 0.0), |p| p.system_insets())
}

/// [`Platform::network`], or [`Network::Unknown`] before a platform is
/// installed.
pub fn network() -> Network {
    get().map_or(Network::Unknown, |p| p.network())
}

/// [`Platform::battery_saver`], or `false` before a platform is
/// installed.
pub fn battery_saver() -> bool {
    get().is_some_and(|p| p.battery_saver())
}

/// [`Platform::set_background_work`].
pub fn set_background_work(work: &BackgroundWork) {
    if let Some(p) = get() {
        p.set_background_work(work);
    }
}

/// [`Platform::pick_file`].
pub fn pick_file() {
    if let Some(p) = get() {
        p.pick_file();
    }
}

/// [`Platform::read_file`], or `Unsupported` before a platform is
/// installed.
pub fn read_file(handle: &str) -> std::io::Result<Vec<u8>> {
    get().map_or(Err(std::io::ErrorKind::Unsupported.into()), |p| p.read_file(handle))
}

/// [`Platform::downloads_dir`].
pub fn downloads_dir() -> Option<std::path::PathBuf> {
    get().and_then(|p| p.downloads_dir())
}

/// [`Platform::notify`].
pub fn notify(title: &str, body: &str) {
    if let Some(p) = get() {
        p.notify(title, body);
    }
}

/// [`Platform::register_push`].
pub fn register_push() {
    if let Some(p) = get() {
        p.register_push();
    }
}

/// [`Platform::call_connected`].
pub fn call_connected() {
    if let Some(p) = get() {
        p.call_connected();
    }
}

/// [`Platform::ring`].
pub fn ring(from: Option<&str>) {
    if let Some(p) = get() {
        p.ring(from);
    }
}

/// [`Platform::request_battery_exemption`].
pub fn request_battery_exemption() {
    if let Some(p) = get() {
        p.request_battery_exemption();
    }
}

/// [`Platform::has_camera`], or `false` before a platform is installed.
pub fn has_camera() -> bool {
    get().is_some_and(|p| p.has_camera())
}

/// [`Platform::has_camera_permission`], or `false` before a platform is
/// installed.
pub fn has_camera_permission() -> bool {
    get().is_some_and(|p| p.has_camera_permission())
}

/// [`Platform::request_camera_permission`].
pub fn request_camera_permission() {
    if let Some(p) = get() {
        p.request_camera_permission();
    }
}

/// [`Platform::start_camera`], or an error before a platform is installed.
pub fn start_camera(request: crate::camera::CameraRequest) -> Result<(), String> {
    get().map_or(Err("No camera".into()), |p| p.start_camera(request))
}

/// [`Platform::stop_camera`].
pub fn stop_camera() {
    if let Some(p) = get() {
        p.stop_camera();
    }
}

/// Hands one camera picture to the shared code, from whatever thread the
/// platform's camera calls back on. The image is only borrowed: it is
/// converted (or skipped, when it came sooner than the frame rate wants)
/// before this returns, so the platform can release its buffer straight
/// after.
pub fn camera_frame(image: &crate::camera::CameraImage) {
    crate::camera_source::deliver(image);
}

/// Says the camera stopped by itself, with a sentence for the user: taken
/// by another app, disconnected, or an error. The call stops sending video
/// and the button shows the camera off.
pub fn camera_failed(why: impl Into<String>) {
    crate::camera_source::fail(why.into());
}
