//! The phone's camera through AVFoundation, for sending video on a call.
//!
//! A capture session with the chosen camera as its input and a video data
//! output whose frames come to [`FrameDelegate`] on the camera's own
//! serial queue, as BGRA pixel buffers. Each one is handed straight to the
//! shared code (`platform::camera_frame`), which converts and shrinks it,
//! and is let go when that returns, so the camera's buffer pool never runs
//! dry.
//!
//! The connection is set to portrait, so frames arrive upright and the
//! shared code need not turn them, and unmirrored, so the other side sees
//! the front camera the right way round (the self-view tile is mirrored by
//! the shared code instead, as people expect).
//!
//! Starting and stopping a session block for a moment while the hardware
//! comes up, so both run on the camera queue; [`start`] itself only finds
//! the camera and builds the session, which is quick, so a missing camera
//! or a refused permission is still reported straight away.

use std::sync::Mutex;

use antiphon_mobile::camera::{CameraImage, CameraRequest, Facing, Pixels};
use antiphon_mobile::platform;
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_av_foundation::{
    AVAuthorizationStatus, AVCaptureConnection, AVCaptureDevice, AVCaptureDeviceInput, AVCaptureDevicePosition,
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureOutput, AVCaptureSession, AVCaptureSessionPreset352x288,
    AVCaptureSessionPreset640x480, AVCaptureVideoDataOutput, AVCaptureVideoDataOutputSampleBufferDelegate,
    AVMediaTypeVideo,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, kCVReturnSuccess, CVPixelBufferGetBaseAddress,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetDataSize, CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSDictionary, NSNumber, NSString};

/// The camera that is running, if any.
static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// A running capture session and what it needs kept alive.
struct Running {
    /// The session, stopped on drop.
    session: Retained<AVCaptureSession>,
    /// The output's delegate. The output holds it weakly, so this is what
    /// keeps it alive.
    _delegate: Retained<FrameDelegate>,
    /// The serial queue frames arrive on, and where the session is
    /// started and stopped.
    queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: AVCaptureSession is documented as safe to start and stop from
// any thread (Apple recommends a background queue, as here), the delegate
// holds no state, and dispatch queues are thread-safe. `Running` is only
// ever reached through `RUNNING`'s mutex.
unsafe impl Send for Running {}

impl Drop for Running {
    /// Stops the session on the camera queue and waits for it, so the
    /// camera light is off by the time the shared code's `stop_camera`
    /// returns, and no frame arrives after.
    fn drop(&mut self) {
        let session = self.session.clone();
        let session = SendSession(session);
        self.queue.exec_sync(move || {
            let session = session;
            // SAFETY: a plain call on a live session.
            unsafe { session.0.stopRunning() };
        });
    }
}

/// A session handed to the camera queue's closure, which must be `Send`.
struct SendSession(Retained<AVCaptureSession>);

// SAFETY: see `Running`; the session is only started and stopped there.
unsafe impl Send for SendSession {}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "AntiphonCameraDelegate"]
    /// Takes each frame from the video data output, on the camera queue.
    struct FrameDelegate;

    unsafe impl NSObjectProtocol for FrameDelegate {}

    unsafe impl AVCaptureVideoDataOutputSampleBufferDelegate for FrameDelegate {
        #[unsafe(method(captureOutput:didOutputSampleBuffer:fromConnection:))]
        fn did_output(&self, _output: &AVCaptureOutput, buffer: &CMSampleBuffer, _connection: &AVCaptureConnection) {
            hand_over(buffer);
        }
    }
);

impl FrameDelegate {
    /// A fresh delegate.
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        // SAFETY: NSObject's designated initialiser.
        unsafe { msg_send![super(this), init] }
    }
}

/// `AVMediaTypeVideo`, which AVFoundation always defines; the binding
/// only says it might not because it is a weakly linked static.
fn video() -> &'static NSString {
    // SAFETY: reading an extern static.
    unsafe { AVMediaTypeVideo }.expect("AVMediaTypeVideo")
}

/// Whether the user has allowed the camera.
pub fn has_permission() -> bool {
    // SAFETY: a class method with a valid media type.
    unsafe { AVCaptureDevice::authorizationStatusForMediaType(video()) == AVAuthorizationStatus::Authorized }
}

/// Shows the system's camera prompt the first time; afterwards iOS answers
/// from the stored choice without asking, and the user changes it in
/// Settings. The shared code looks for the answer itself; the callback
/// only logs it.
pub fn request_permission() {
    let block = RcBlock::new(|granted: Bool| log::info!("camera permission: {}", granted.as_bool()));
    // SAFETY: a class method; AVFoundation copies the block.
    unsafe { AVCaptureDevice::requestAccessForMediaType_completionHandler(video(), &block) };
}

/// Starts the camera `request` asks for, closing any camera that was
/// running first. The error is a sentence for the user.
pub fn start(request: CameraRequest) -> Result<(), String> {
    stop();
    let running = open(request)?;
    *RUNNING.lock().unwrap() = Some(running);
    Ok(())
}

/// Closes the camera, if one is running.
pub fn stop() {
    // Taken out first and dropped outside the lock: dropping waits for
    // the camera queue, whose frames must never wait on this lock.
    let running = RUNNING.lock().unwrap().take();
    drop(running);
}

/// Builds and starts a session for `request`.
fn open(request: CameraRequest) -> Result<Running, String> {
    let position = match request.facing {
        Facing::Front => AVCaptureDevicePosition::Front,
        Facing::Back => AVCaptureDevicePosition::Back,
    };
    // SAFETY: plain AVFoundation calls with valid arguments; every object
    // is retained for as long as it is used.
    unsafe {
        let device = AVCaptureDevice::defaultDeviceWithDeviceType_mediaType_position(
            AVCaptureDeviceTypeBuiltInWideAngleCamera,
            Some(video()),
            position,
        )
        .ok_or_else(|| match request.facing {
            Facing::Front => "This phone has no front camera".to_string(),
            Facing::Back => "This phone has no back camera".to_string(),
        })?;
        let input = AVCaptureDeviceInput::deviceInputWithDevice_error(&device).map_err(|e| {
            log::warn!("camera: {e}");
            "The camera would not start".to_string()
        })?;

        let session = AVCaptureSession::new();
        session.beginConfiguration();
        // The smallest preset that covers the request: frames bigger than
        // that are only shrunk again before sending.
        let preset = if request.width <= 352 { AVCaptureSessionPreset352x288 } else { AVCaptureSessionPreset640x480 };
        if session.canSetSessionPreset(preset) {
            session.setSessionPreset(preset);
        }
        if !session.canAddInput(&input) {
            return Err("Another app is using the camera".into());
        }
        session.addInput(&input);

        let output = AVCaptureVideoDataOutput::new();
        // BGRA, which the shared code converts with a byte swap; the
        // camera's native YUV would save the camera a conversion but cost
        // the shared code a second, slower path for one platform.
        // `kCVPixelBufferPixelFormatTypeKey` is a CFString, toll-free
        // bridged to NSString.
        let key: &NSString = &*(kCVPixelBufferPixelFormatTypeKey as *const _ as *const NSString);
        let format = NSNumber::new_u32(kCVPixelFormatType_32BGRA);
        let value: &AnyObject = &format;
        let settings = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[value]);
        output.setVideoSettings(Some(&settings));
        // A late frame is worth less than the next one.
        output.setAlwaysDiscardsLateVideoFrames(true);
        let delegate = FrameDelegate::new();
        let queue = DispatchQueue::new("org.antiphon.camera", None);
        output.setSampleBufferDelegate_queue(Some(ProtocolObject::from_ref(&*delegate)), Some(&queue));
        if !session.canAddOutput(&output) {
            return Err("The camera would not start".into());
        }
        session.addOutput(&output);
        if let Some(connection) = output.connectionWithMediaType(video()) {
            // `videoOrientation` is deprecated from iOS 17 for
            // `videoRotationAngle`, which iOS 15 and 16 (still supported)
            // do not have; the old property still works everywhere.
            #[allow(deprecated)]
            if connection.isVideoOrientationSupported() {
                connection.setVideoOrientation(objc2_av_foundation::AVCaptureVideoOrientation::Portrait);
            }
            if connection.isVideoMirroringSupported() {
                connection.setAutomaticallyAdjustsVideoMirroring(false);
                connection.setVideoMirrored(false);
            }
        }
        set_frame_rate(&device, request.fps);
        session.commitConfiguration();

        let start = SendSession(session.clone());
        queue.exec_async(move || {
            let start = start;
            start.0.startRunning();
        });
        log::info!("camera: {:?} started", request.facing);
        Ok(Running { session, _delegate: delegate, queue })
    }
}

/// Caps the camera at `fps` frames a second when it can be locked for
/// configuration; otherwise it runs at its own rate (usually 30) and the
/// shared code skips the frames it does not need.
///
/// # Safety
///
/// `device` must be a live capture device.
unsafe fn set_frame_rate(device: &AVCaptureDevice, fps: u32) {
    if device.lockForConfiguration().is_err() {
        return;
    }
    let duration = objc2_core_media::CMTime::new(1, fps.max(1) as i32);
    // Only the shortest frame time: leaving the longest alone lets the
    // camera slow down in a dim room rather than send a dark picture.
    // Setting a rate the active format does not support throws, so the
    // format's own ranges are checked first.
    let supported = device
        .activeFormat()
        .videoSupportedFrameRateRanges()
        .iter()
        .any(|range| range.minFrameRate() <= f64::from(fps) && f64::from(fps) <= range.maxFrameRate());
    if supported {
        device.setActiveVideoMinFrameDuration(duration);
    }
    device.unlockForConfiguration();
}

/// Hands one frame to the shared code, reading its pixel buffer in place
/// under a read-only lock.
fn hand_over(buffer: &CMSampleBuffer) {
    // SAFETY: the sample buffer is live for this call; its image buffer is
    // retained here, and its bytes are read only between lock and unlock,
    // within the size CoreVideo reports.
    unsafe {
        let Some(pixels) = buffer.image_buffer() else { return };
        if CVPixelBufferGetPixelFormatType(&pixels) != kCVPixelFormatType_32BGRA {
            return;
        }
        if CVPixelBufferLockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly) != kCVReturnSuccess {
            return;
        }
        let base = CVPixelBufferGetBaseAddress(&pixels);
        if !base.is_null() {
            let data = std::slice::from_raw_parts(base as *const u8, CVPixelBufferGetDataSize(&pixels));
            let image = CameraImage {
                width: CVPixelBufferGetWidth(&pixels) as u32,
                height: CVPixelBufferGetHeight(&pixels) as u32,
                pixels: Pixels::Bgra { data, row: CVPixelBufferGetBytesPerRow(&pixels) },
                rotation: 0,
            };
            platform::camera_frame(&image);
        }
        CVPixelBufferUnlockBaseAddress(&pixels, CVPixelBufferLockFlags::ReadOnly);
    }
}
