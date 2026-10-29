//! The phone's camera as a [`FrameSource`] a call can send from, the way
//! the desktop's `video_capture` wraps its webcam.
//!
//! The platform shell runs the camera on its own threads and hands each
//! picture to [`deliver`]; the call's video sender (`video::CallVideo`)
//! takes the newest one at the encoder's pace. Only the newest frame is
//! kept, so a sender that falls behind skips to now. There is one camera,
//! so one sink; switching between the front and back cameras ([`switch`])
//! restarts the platform's camera into the same sink, so the call keeps
//! its sender and the other side's picture simply changes.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use antiphon_video::Frame;

use crate::camera::{self, CameraImage, Facing, Pacer};
use crate::platform;
use crate::video::{FrameSource, Numbered};

/// What the camera's callback and the frame source share.
struct Shared {
    /// The newest frame, numbered.
    latest: Mutex<Numbered>,
    /// Frames delivered so far; the next one's number.
    count: AtomicU64,
    /// Why the camera stopped by itself, if it did.
    failed: Mutex<Option<String>>,
    /// Thins a camera that runs faster than frames are sent.
    pacer: Mutex<Pacer>,
    /// Which camera is open, for [`switch`].
    facing: Mutex<Facing>,
}

/// Where [`deliver`] puts frames: the open camera's state, or nothing
/// while the camera is off (a late frame from a camera being closed is
/// dropped).
static SINK: Mutex<Option<Arc<Shared>>> = Mutex::new(None);

/// Whether the data saver applies now. Read for every frame, so turning
/// onto mobile data mid-call shrinks the picture straight away without
/// reopening the camera.
static LOW_DATA: AtomicBool = AtomicBool::new(false);

/// Tells the camera whether the data saver applies now (the setting is on
/// and the phone is on a metered network). Called by the interface on
/// every poll, cheaply.
pub fn set_low_data(on: bool) {
    LOW_DATA.store(on, Ordering::Relaxed);
}

/// Takes one picture from the platform's camera. Safe from any thread;
/// returns quickly when the frame is not needed (camera closed, or it came
/// sooner than the frame rate wants), so a platform may call it for every
/// frame the camera makes.
pub fn deliver(image: &CameraImage) {
    let Some(shared) = SINK.lock().unwrap().clone() else { return };
    let low = LOW_DATA.load(Ordering::Relaxed);
    {
        let mut pacer = shared.pacer.lock().unwrap();
        pacer.set_fps(camera::fps(low));
        if !pacer.admit(Instant::now()) {
            return;
        }
    }
    let Some((width, height, rgba)) = camera::to_rgba(image, camera::max_side(low)) else {
        log::warn!("camera: a {}x{} frame had short buffers; skipped", image.width, image.height);
        return;
    };
    let n = shared.count.fetch_add(1, Ordering::Relaxed);
    *shared.latest.lock().unwrap() = Some((n, Arc::new(Frame { width, height, rgba })));
}

/// Tells the call the camera stopped by itself (taken by another app,
/// disconnected, refused), with a sentence the user can read. The call's
/// sender sees it within a frame, stops, and the button shows the camera
/// off.
pub fn fail(why: String) {
    if let Some(shared) = SINK.lock().unwrap().as_ref() {
        *shared.failed.lock().unwrap() = Some(why);
    }
}

/// Turns to the phone's other camera, front to back or back to front,
/// and says which is now open. Fails, leaving the camera as it was, when
/// none is open or the platform cannot open the other one (a phone with
/// only one camera).
pub fn switch() -> Result<Facing, String> {
    let Some(shared) = SINK.lock().unwrap().clone() else { return Err("The camera is off".into()) };
    let mut facing = shared.facing.lock().unwrap();
    let low = LOW_DATA.load(Ordering::Relaxed);
    let next = facing.flipped();
    if let Err(why) = platform::start_camera(camera::request_for(low, next)) {
        // Put the old camera back; its own failure, if any, reaches the
        // call through `fail` as usual.
        let _ = platform::start_camera(camera::request_for(low, *facing));
        return Err(why);
    }
    *facing = next;
    Ok(next)
}

/// The phone's camera, open. Dropping it closes the camera, which turns
/// the camera light off.
pub struct PhoneCamera {
    /// The state [`deliver`] fills.
    shared: Arc<Shared>,
}

impl PhoneCamera {
    /// Opens the camera on `facing`'s side, or says why not. Frames start
    /// arriving a moment later; until then the call sends nothing.
    pub fn open(facing: Facing) -> Result<Self, String> {
        if !platform::has_camera_permission() {
            return Err("Stoop needs the camera permission to send video".into());
        }
        let low = LOW_DATA.load(Ordering::Relaxed);
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            count: AtomicU64::new(0),
            failed: Mutex::new(None),
            pacer: Mutex::new(Pacer::new(camera::fps(low))),
            facing: Mutex::new(facing),
        });
        // In place before the camera starts, so its first frames land.
        *SINK.lock().unwrap() = Some(shared.clone());
        if let Err(why) = platform::start_camera(camera::request_for(low, facing)) {
            SINK.lock().unwrap().take();
            return Err(why);
        }
        Ok(Self { shared })
    }
}

impl FrameSource for PhoneCamera {
    fn latest(&mut self) -> Numbered {
        self.shared.latest.lock().unwrap().clone()
    }

    fn failed(&self) -> Option<String> {
        self.shared.failed.lock().unwrap().clone()
    }
}

impl Drop for PhoneCamera {
    fn drop(&mut self) {
        let mut sink = SINK.lock().unwrap();
        // Only if the sink is still ours: a camera opened again before
        // this one was dropped (turned off and straight back on) must not
        // be closed by it.
        if sink.as_ref().is_some_and(|s| Arc::ptr_eq(s, &self.shared)) {
            sink.take();
            drop(sink);
            platform::stop_camera();
        }
    }
}
