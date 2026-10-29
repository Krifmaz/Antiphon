//! Sending the phone's camera on a call: the camera button, the switch
//! between front and back, the "this is what they see" tile, and what
//! happens to the camera when the app leaves the screen.
//!
//! The sending itself is the desktop's: the camera becomes a
//! `FrameSource` (`camera_source`) and the call's `CallVideo` encodes it
//! with the same tile codec and sends it over the same encrypted
//! connection as the voice, so a desktop shows a phone's camera exactly as
//! it shows another desktop's.
//!
//! Neither phone lets an app in the background use the camera without
//! more than a voice app should ask for (iOS not at all; Android a
//! `camera` foreground service type and its own permission), and a call
//! carried on from the lock screen is a voice call anyway. So the camera
//! is turned off when the app leaves the screen and back on when it
//! returns, if it was on.

use std::time::{Duration, Instant};

use antiphon_ui::{self as stoop, ToastKind};
use antiphon_video::Stream;

use super::App;
use crate::call::Live;
use crate::camera_source::{self, PhoneCamera};
use crate::platform;

/// How long a permission prompt may stay unanswered before the camera
/// button gives up and shows off again. Long enough to read the prompt;
/// short enough that a yes much later, from the system's settings, does
/// not switch the camera on out of the blue.
const PROMPT_WAIT: Duration = Duration::from_secs(60);
/// How often to ask the system whether the prompt said yes.
const PROMPT_CHECK: Duration = Duration::from_millis(400);

/// The camera permission prompt is up, waiting for an answer.
pub(super) struct PermissionWait {
    /// When it was shown.
    asked: Instant,
    /// When the permission was last checked.
    checked: Instant,
}

impl App {
    /// The camera button: on starts sending the camera on the call (asking
    /// for the permission first if need be), off stops it.
    pub(super) fn set_camera(&mut self, on: bool) {
        self.camera_wanted = on;
        let Some(live) = self.engine.live() else {
            self.camera_wanted = false;
            if on {
                self.stoop.toast("The camera starts once you're on a call", ToastKind::Info);
            }
            return;
        };
        if !on {
            live.video.stop(Stream::Camera);
            return;
        }
        if !platform::has_camera_permission() {
            // Neither phone tells native code the prompt's answer in a
            // way the event loop reliably sees (Android's dialog does not
            // always pause the window), so `camera_model` looks for it a
            // few times a second and starts the camera when it is a yes;
            // the user does not have to press again.
            platform::request_camera_permission();
            let now = Instant::now();
            self.camera_prompt = Some(PermissionWait { asked: now, checked: now });
            return;
        }
        self.start_camera(&live);
    }

    /// While the permission prompt is up: starts the camera once the
    /// answer is yes, and gives up after [`PROMPT_WAIT`]. Checks at most
    /// every [`PROMPT_CHECK`], since asking the system is not free and
    /// this runs every frame of a call.
    fn camera_prompt_check(&mut self, live: &Live) {
        let Some(wait) = self.camera_prompt.as_mut() else { return };
        let now = Instant::now();
        if !self.camera_wanted || now.duration_since(wait.asked) > PROMPT_WAIT {
            self.camera_prompt = None;
            self.camera_wanted = false;
            return;
        }
        if now.duration_since(wait.checked) < PROMPT_CHECK {
            return;
        }
        wait.checked = now;
        if platform::has_camera_permission() {
            self.camera_prompt = None;
            if self.foreground {
                self.start_camera(live);
            }
        }
    }

    /// Opens the camera and starts sending it, or says why not.
    fn start_camera(&mut self, live: &Live) {
        match PhoneCamera::open(self.camera_facing) {
            Ok(camera) => live.video.start(Stream::Camera, Box::new(camera), live.peer.clone(), live.runtime().clone()),
            Err(why) => {
                self.camera_wanted = false;
                self.stoop.toast(why, ToastKind::Err);
            }
        }
    }

    /// The switch button: the other camera, front or back, without a gap
    /// in what the other side sees beyond the camera's own start-up.
    pub(super) fn flip_camera(&mut self) {
        match camera_source::switch() {
            Ok(facing) => self.camera_facing = facing,
            Err(why) => self.stoop.toast(why, ToastKind::Err),
        }
    }

    /// The app is on screen again: puts the camera back if it was on when
    /// the app left, or starts it if the permission prompt just said yes.
    pub(super) fn camera_resumed(&mut self) {
        if !self.camera_wanted {
            return;
        }
        let Some(live) = self.engine.live() else {
            // The call ended while the app was away.
            self.camera_wanted = false;
            return;
        };
        if live.video.sending(Stream::Camera) {
            return;
        }
        if platform::has_camera_permission() {
            self.camera_prompt = None;
            self.start_camera(&live);
        } else if self.camera_prompt.is_none() {
            // Taken away in the system's settings while the app was away.
            self.camera_wanted = false;
            self.stoop.toast("Allow the camera in Settings to send video", ToastKind::Info);
        }
    }

    /// The app left the screen: the camera goes off, remembered as wanted
    /// so it comes back with the app. See the module's note on why.
    pub(super) fn camera_suspended(&mut self) {
        if let Some(live) = self.engine.live() {
            if live.video.sending(Stream::Camera) {
                live.video.stop(Stream::Camera);
                self.camera_wanted = true;
            }
        }
    }

    /// Fills in the call model's camera button: whether there is one, and
    /// whether the camera is on. A camera that stopped by itself (taken by
    /// another app, unplugged on a tablet) is reported once and shown off.
    pub(super) fn camera_model(&mut self, live: Option<&Live>, call: &mut stoop::Call) {
        let Some(live) = live else {
            self.camera_wanted = false;
            self.camera_prompt = None;
            return;
        };
        self.camera_prompt_check(live);
        for (stream, why) in live.video.take_failures() {
            if stream == Stream::Camera {
                self.camera_wanted = false;
                self.stoop.toast(why, ToastKind::Err);
            }
        }
        call.can_camera = platform::has_camera();
        call.camera_on = live.video.sending(Stream::Camera);
    }

    /// Our own camera's tile, "You", mirrored as the sender makes it, so
    /// the person sees what the other side sees. Reuses the cached tile
    /// while the preview has not changed, as for their pictures.
    pub(super) fn my_camera_tile(&mut self, live: &Live) -> Option<stoop::VideoTile> {
        let (n, frame) = live.video.preview(Stream::Camera)?;
        const KEY: &str = "my-camera";
        if let Some((v, tile)) = self.tiles.get(KEY) {
            if *v == n {
                return Some(tile.clone());
            }
        }
        let tile = stoop::VideoTile {
            key: KEY.into(),
            label: "You".into(),
            image: iced_core::image::Handle::from_rgba(frame.width, frame.height, frame.rgba.clone()),
            width: frame.width,
            height: frame.height,
            mine: true,
            screen: false,
        };
        self.tiles.insert(KEY.into(), (n, tile.clone()));
        Some(tile)
    }
}
