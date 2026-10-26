//! What the shared interface needs from the phone it runs on.
//!
//! Each platform shell (`antiphon-android`, `antiphon-ios`) implements
//! [`Platform`] once and hands it over with [`install`] before the event
//! loop starts. The free functions below are what the rest of this crate
//! calls, so `ui` and `call` never name a platform. Every method fails
//! soft: a phone that refuses one still gets a working app, just without
//! that convenience, which is why the defaults do nothing.

use std::sync::OnceLock;

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
}

static PLATFORM: OnceLock<Box<dyn Platform>> = OnceLock::new();

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
