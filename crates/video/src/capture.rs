//! The camera and the screen, as a stream of [`Frame`]s (desktops only).
//!
//! Each source runs on its own thread and keeps only the newest frame: a
//! sender that falls behind skips to now instead of working through a
//! backlog of the past. Dropping a [`Capture`] stops its thread and lets
//! the device go, which turns the camera's light off.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::Frame;

/// What the capture thread and its owner share.
#[derive(Default)]
struct Shared {
    /// The newest frame, numbered.
    latest: Mutex<Option<(u64, Arc<Frame>)>>,
    /// Frames delivered so far; the next one's number.
    count: AtomicU64,
    /// Set to ask the thread to stop.
    stop: AtomicBool,
    /// Why capture stopped by itself, if it did.
    failed: Mutex<Option<String>>,
}

impl Shared {
    /// Publishes a new frame as the latest.
    fn deliver(&self, frame: Frame) {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        *self.latest.lock().unwrap() = Some((n, Arc::new(frame)));
    }

    /// Records why capture stopped.
    fn fail(&self, why: String) {
        *self.failed.lock().unwrap() = Some(why);
    }
}

/// A running camera or screen capture.
pub struct Capture {
    /// The state shared with the thread.
    shared: Arc<Shared>,
    /// The capture thread, joined on drop.
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Opens camera `index` (from [`cameras`]) and starts capturing. Waits
    /// until the camera is open, so a camera in use elsewhere, refused by
    /// the system or missing is an error here rather than a silent black
    /// picture later.
    pub fn camera(index: u32) -> Result<Self, String> {
        Self::spawn("camera", move |shared, opened| camera_thread(index, &shared, &opened))
    }

    /// Starts capturing screen `index` (from [`screens`]). On Wayland the
    /// system asks the user which screen or window to share; that choice
    /// wins over `index`.
    pub fn screen(index: u32) -> Result<Self, String> {
        Self::spawn("screen", move |shared, opened| screen_thread(index, &shared, &opened))
    }

    /// Runs `body` on a new thread and waits for it to report the device
    /// open (or not). A minute allows for a permission prompt.
    fn spawn(name: &str, body: impl FnOnce(Arc<Shared>, mpsc::Sender<Result<(), String>>) + Send + 'static) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name(format!("{name} capture"))
            .spawn(move || body(thread_shared, tx))
            .map_err(|e| e.to_string())?;
        let capture = Self { shared, thread: Some(thread) };
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(Ok(())) => Ok(capture),
            Ok(Err(why)) => Err(why),
            Err(_) => Err(format!("The {name} did not start")),
        }
    }

    /// The newest frame and its number, if one has come in. The number
    /// lets a caller tell a new frame from the one it already sent.
    pub fn latest(&self) -> Option<(u64, Arc<Frame>)> {
        self.shared.latest.lock().unwrap().clone()
    }

    /// Why capture stopped by itself (the camera unplugged, the screen
    /// share ended from the system's own controls), if it did.
    pub fn failed(&self) -> Option<String> {
        self.shared.failed.lock().unwrap().clone()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The cameras attached, as `(index, name)`. Empty when there are none or
/// the system will not say.
pub fn cameras() -> Vec<(u32, String)> {
    nokhwa::query(ApiBackend::Auto)
        .map(|list| {
            list.into_iter()
                .enumerate()
                .map(|(i, info)| {
                    let index = match info.index() {
                        CameraIndex::Index(n) => *n,
                        CameraIndex::String(_) => i as u32,
                    };
                    (index, info.human_name())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The screens attached, as `(index, name)`, the main one first.
pub fn screens() -> Vec<(u32, String)> {
    let Ok(monitors) = xcap::Monitor::all() else { return Vec::new() };
    let mut list: Vec<(bool, u32, String)> = monitors
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_else(|_| format!("Screen {}", i + 1));
            (m.is_primary().unwrap_or(false), i as u32, name)
        })
        .collect();
    list.sort_by_key(|(primary, i, _)| (!primary, *i));
    list.into_iter().map(|(_, i, name)| (i, name)).collect()
}

/// macOS asks once before an app may use the camera. Waits for the answer
/// (or a minute) and says whether the camera may be used.
#[cfg(target_os = "macos")]
fn camera_allowed() -> bool {
    if nokhwa::nokhwa_check() {
        return true;
    }
    let (tx, rx) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = tx.send(granted);
    });
    rx.recv_timeout(Duration::from_secs(60)).unwrap_or(false)
}

/// Other systems ask (or not) when the camera is opened.
#[cfg(not(target_os = "macos"))]
fn camera_allowed() -> bool {
    true
}

/// The camera thread: opens the camera, then reads frames until told to
/// stop or the camera goes away.
fn camera_thread(index: u32, shared: &Shared, opened: &mpsc::Sender<Result<(), String>>) {
    if !camera_allowed() {
        let _ = opened.send(Err("Camera access is turned off for Stoop in your system settings".into()));
        return;
    }
    // 640 by 480 at 30 frames a second is what nearly every webcam offers
    // and all the codec sends; asking for more only makes the camera work
    // harder for pixels that are thrown away. Anything it can do if not.
    let wanted = CameraFormat::new(Resolution::new(640, 480), FrameFormat::MJPEG, 30);
    let mut camera = nokhwa::Camera::new(CameraIndex::Index(index), RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(wanted)))
        .or_else(|_| nokhwa::Camera::new(CameraIndex::Index(index), RequestedFormat::new::<RgbAFormat>(RequestedFormatType::AbsoluteHighestFrameRate)));
    let mut camera = match camera.as_mut().map_err(|e| e.to_string()).and_then(|c| c.open_stream().map_err(|e| e.to_string())) {
        Ok(()) => camera.expect("opened"),
        Err(why) => {
            let _ = opened.send(Err(format!("The camera would not open: {why}")));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    let mut failures = 0;
    while !shared.stop.load(Ordering::Relaxed) {
        match camera.frame().map_err(|e| e.to_string()).and_then(|buf| to_frame(&buf)) {
            Ok(frame) => {
                failures = 0;
                shared.deliver(frame);
            }
            // A frame or two can fail to decode; a run of them means the
            // camera is gone.
            Err(why) => {
                failures += 1;
                if failures > 30 {
                    shared.fail(format!("The camera stopped: {why}"));
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    }
    let _ = camera.stop_stream();
}

/// One camera buffer as a frame. MJPEG is decoded here with zune-jpeg;
/// the raw formats (YUYV, NV12, ...) by nokhwa.
fn to_frame(buf: &nokhwa::Buffer) -> Result<Frame, String> {
    let res = buf.resolution();
    if buf.source_frame_format() == FrameFormat::MJPEG {
        let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut dec = JpegDecoder::new_with_options(std::io::Cursor::new(buf.buffer()), options);
        let rgba = dec.decode().map_err(|e| e.to_string())?;
        let (w, h) = dec.dimensions().ok_or("no size")?;
        let frame = Frame { width: w as u32, height: h as u32, rgba };
        return frame.is_valid().then_some(frame).ok_or_else(|| "bad frame".into());
    }
    let image = buf.decode_image::<RgbAFormat>().map_err(|e| e.to_string())?;
    let frame = Frame { width: res.width(), height: res.height(), rgba: image.into_raw() };
    frame.is_valid().then_some(frame).ok_or_else(|| "bad frame".into())
}

/// The screen thread: records screen `index` until told to stop.
fn screen_thread(index: u32, shared: &Shared, opened: &mpsc::Sender<Result<(), String>>) {
    let started = xcap::Monitor::all()
        .map_err(|e| e.to_string())
        .and_then(|all| all.into_iter().nth(index as usize).ok_or_else(|| "That screen is not connected".to_string()))
        .and_then(|monitor| monitor.video_recorder().map_err(|e| e.to_string()))
        .and_then(|(recorder, frames)| recorder.start().map(|()| (recorder, frames)).map_err(|e| e.to_string()));
    let (recorder, frames) = match started {
        Ok(r) => r,
        Err(why) => {
            let _ = opened.send(Err(format!("Screen sharing would not start: {why}")));
            return;
        }
    };
    let _ = opened.send(Ok(()));
    while !shared.stop.load(Ordering::Relaxed) {
        match frames.recv_timeout(Duration::from_millis(200)) {
            Ok(f) => {
                let frame = Frame { width: f.width, height: f.height, rgba: f.raw };
                if frame.is_valid() {
                    shared.deliver(frame);
                }
            }
            // A still screen sends nothing on some systems: keep waiting.
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                shared.fail("Screen sharing was stopped".into());
                break;
            }
        }
    }
    let _ = recorder.stop();
}
