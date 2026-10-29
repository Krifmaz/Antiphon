//! The desktop's camera and screen as [`FrameSource`]s for a call.

use std::sync::Arc;

use antiphon_video::capture::Capture;
use antiphon_video::Frame;

use crate::video::FrameSource;

impl FrameSource for Capture {
    fn latest(&mut self) -> Option<(u64, Arc<Frame>)> {
        Capture::latest(self)
    }

    fn failed(&self) -> Option<String> {
        Capture::failed(self)
    }
}

/// Opens the first camera, or says why not. Blocks while it opens (and
/// while macOS asks permission), so call it off the UI thread.
pub fn open_camera() -> Result<Box<dyn FrameSource>, String> {
    let index = antiphon_video::capture::cameras().first().map(|(i, _)| *i).ok_or("No camera was found")?;
    Ok(Box::new(Capture::camera(index)?))
}

/// Starts capturing the main screen, or says why not. Blocks while the
/// system asks which screen to share (on Wayland), so call it off the UI
/// thread.
pub fn open_screen() -> Result<Box<dyn FrameSource>, String> {
    let index = antiphon_video::capture::screens().first().map(|(i, _)| *i).unwrap_or(0);
    Ok(Box::new(Capture::screen(index)?))
}
