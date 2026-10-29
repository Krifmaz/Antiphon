//! Where the call's audio path meets live captions (spec §7).
//!
//! The recogniser and its window live in each app; the audio path in
//! [`crate::state`] only needs to know whether captions are running and
//! where to push samples. Those two facts are kept here, set by the app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use antiphon_captions::CaptionTap;

/// The source id for your own voice. The other side is 0, and group call
/// legs are their leg id + 1, matching the mixer's numbering.
pub const OWN_VOICE: u64 = u64::MAX;

/// The running recogniser's tap, or `None` while captions are off.
static TAP: Mutex<Option<CaptionTap>> = Mutex::new(None);
/// Whether to caption your own voice as well.
static OWN: AtomicBool = AtomicBool::new(false);

/// The tap for the call's audio path, while captions are running.
pub fn tap() -> Option<CaptionTap> {
    TAP.lock().ok()?.clone()
}

/// Sets (or, with `None`, clears) the tap the audio path feeds.
pub fn set_tap(tap: Option<CaptionTap>) {
    if let Ok(mut t) = TAP.lock() {
        *t = tap;
    }
}

/// Whether to caption your own voice too.
pub fn own_voice() -> bool {
    OWN.load(Ordering::Relaxed)
}

/// Turns captioning your own voice on or off.
pub fn set_own_voice(on: bool) {
    OWN.store(on, Ordering::Relaxed);
}
