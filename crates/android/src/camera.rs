//! The phone's camera through the NDK's own camera API (libcamera2ndk),
//! for sending video on a call.
//!
//! The NDK camera is Camera2 without Java: a camera device opened from the
//! `ACameraManager`, a capture session whose one output is an
//! `AImageReader`, and a repeating request that keeps frames coming. Each
//! frame arrives on the image reader's own thread as YUV 4:2:0 and is
//! handed straight to the shared code (`platform::camera_frame`), which
//! converts, turns and shrinks it; the image is released as soon as that
//! returns. No Java helper is needed, so there is no JNI hop per frame.
//!
//! Everything that is opened lives in one [`Open`], kept in [`OPEN`] while
//! the camera runs and closed, in the order the NDK wants, when it is
//! dropped. The camera permission is asked for in `platform` through the
//! activity, like the microphone's.

use std::ffi::{c_int, c_void, CStr, CString};
use std::ptr;
use std::sync::Mutex;

use antiphon_mobile::camera::{pick_fps_range, pick_size, CameraImage, CameraRequest, Facing, Pixels};
use antiphon_mobile::platform;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::native_window::NativeWindow;
use ndk_sys as ffi;

// The camera functions are declared by `ndk-sys` but not linked by it
// (only the media, window and audio libraries are), so the library is
// named here. It ships with every Android 7.0 and later, under the app's
// minimum of 8.0.
#[link(name = "camera2ndk")]
extern "C" {}

/// The camera that is running, if any.
static OPEN: Mutex<Option<Open>> = Mutex::new(None);

/// `AIMAGE_FORMAT_YUV_420_888` as it appears in the stream configuration
/// list, which holds plain integers.
const YUV_420_888: i32 = ffi::AIMAGE_FORMATS::AIMAGE_FORMAT_YUV_420_888.0 as i32;
/// How many images the reader may hand out at once. Two would do for
/// `acquire_latest_image`, which needs one spare to drop older frames
/// into; a third lets the camera keep filling while one is converted.
const MAX_IMAGES: i32 = 3;

/// Starts the camera `request` asks for, closing any camera that was
/// running first. The error is a sentence for the user.
pub fn start(request: CameraRequest) -> Result<(), String> {
    stop();
    let open = Open::new(request)?;
    *OPEN.lock().unwrap() = Some(open);
    Ok(())
}

/// Closes the camera, if one is running, and with it the camera light.
pub fn stop() {
    // Taken out first and dropped outside the lock: closing waits for the
    // camera's own threads, which must never wait on this lock.
    let open = OPEN.lock().unwrap().take();
    drop(open);
}

/// Everything a running camera holds, in the NDK's raw handles. Each is
/// non-null once [`Open::new`] has returned it; a field still null was
/// never made and is skipped on drop.
struct Open {
    /// The camera service connection.
    manager: *mut ffi::ACameraManager,
    /// The camera itself.
    device: *mut ffi::ACameraDevice,
    /// What the device sends its frames into.
    container: *mut ffi::ACaptureSessionOutputContainer,
    /// The image reader as one of the session's outputs.
    output: *mut ffi::ACaptureSessionOutput,
    /// The image reader as the request's target.
    target: *mut ffi::ACameraOutputTarget,
    /// The repeating "keep sending frames" request.
    request: *mut ffi::ACaptureRequest,
    /// The running capture session.
    session: *mut ffi::ACameraCaptureSession,
    /// Callbacks the device holds a pointer to; boxed so they stay put.
    device_callbacks: Box<ffi::ACameraDevice_StateCallbacks>,
    /// Callbacks the session holds a pointer to; boxed so they stay put.
    session_callbacks: Box<ffi::ACameraCaptureSession_stateCallbacks>,
    /// The reader's window, held so it outlives the session that draws
    /// into it.
    window: Option<NativeWindow>,
    /// Where the frames land; dropped last, after the camera stopped
    /// writing to it.
    reader: Option<ImageReader>,
}

// SAFETY: the NDK camera handles are thread-safe objects (the camera
// service calls back on its own threads); `Open` is only ever reached
// through `OPEN`'s mutex, so no two threads use the handles at once.
unsafe impl Send for Open {}

/// A camera found on the phone, with what is needed to open it.
struct Found {
    /// Its id, for `ACameraManager_openCamera`.
    id: CString,
    /// How far its pictures must be turned clockwise to stand upright on
    /// the screen held upright (the app is fixed to portrait).
    orientation: u32,
    /// The YUV output sizes it offers, landscape.
    sizes: Vec<(u32, u32)>,
    /// The frame-rate ranges its auto-exposure can hold, as (low, high).
    fps_ranges: Vec<(i32, i32)>,
}

/// Turns an NDK camera status into a result, with `what` for the log and
/// a sentence for the user.
fn check(status: ffi::camera_status_t, what: &str) -> Result<(), String> {
    if status == ffi::camera_status_t::ACAMERA_OK {
        return Ok(());
    }
    log::warn!("camera: {what} failed: {}", status.0);
    Err(match status {
        ffi::camera_status_t::ACAMERA_ERROR_PERMISSION_DENIED => "Stoop is not allowed to use the camera".into(),
        ffi::camera_status_t::ACAMERA_ERROR_CAMERA_IN_USE | ffi::camera_status_t::ACAMERA_ERROR_MAX_CAMERA_IN_USE => {
            "Another app is using the camera".into()
        }
        ffi::camera_status_t::ACAMERA_ERROR_CAMERA_DISABLED => "The camera is turned off on this phone".into(),
        _ => format!("The camera would not start (error {})", status.0),
    })
}

impl Open {
    /// Opens the camera on `request.facing`'s side and starts its frames
    /// flowing into an image reader. On any failure, whatever was opened
    /// is closed again by dropping the half-built `Open`.
    fn new(request: CameraRequest) -> Result<Self, String> {
        // SAFETY: a plain constructor; null only when the camera service
        // is missing, checked.
        let manager = unsafe { ffi::ACameraManager_create() };
        if manager.is_null() {
            return Err("This phone has no camera service".into());
        }
        let mut open = Open {
            manager,
            device: ptr::null_mut(),
            container: ptr::null_mut(),
            output: ptr::null_mut(),
            target: ptr::null_mut(),
            request: ptr::null_mut(),
            session: ptr::null_mut(),
            device_callbacks: Box::new(ffi::ACameraDevice_StateCallbacks {
                context: ptr::null_mut(),
                onDisconnected: Some(on_disconnected),
                onError: Some(on_error),
            }),
            session_callbacks: Box::new(ffi::ACameraCaptureSession_stateCallbacks {
                context: ptr::null_mut(),
                onClosed: Some(on_session_state),
                onReady: Some(on_session_state),
                onActive: Some(on_session_state),
            }),
            window: None,
            reader: None,
        };
        let found = find(manager, request.facing)?;
        let (width, height) = pick_size(&found.sizes, (request.width, request.height)).unwrap_or((request.width, request.height));
        log::info!("camera: {:?} at {width}x{height}, turned {}", request.facing, found.orientation);

        let mut reader = ImageReader::new(width as i32, height as i32, ImageFormat::YUV_420_888, MAX_IMAGES)
            .map_err(|e| format!("The camera would not start: {e}"))?;
        let rotation = found.orientation;
        reader
            .set_image_listener(Box::new(move |reader: &ImageReader| {
                // The newest frame only: older ones waiting in the reader
                // are released unread, so a slow moment skips to now.
                if let Ok(AcquireResult::Image(image)) = reader.acquire_latest_image() {
                    hand_over(&image, rotation);
                }
            }))
            .map_err(|e| format!("The camera would not start: {e}"))?;
        let window = reader.window().map_err(|e| format!("The camera would not start: {e}"))?;
        let window_ptr = window.ptr().as_ptr();
        open.window = Some(window);
        open.reader = Some(reader);

        // SAFETY: every pointer passed is either one this function just
        // got from the NDK (and checked by `check`), the boxed callbacks
        // owned by `open`, or an out-pointer to one of its fields.
        unsafe {
            check(
                ffi::ACameraManager_openCamera(manager, found.id.as_ptr(), &mut *open.device_callbacks, &mut open.device),
                "opening the camera",
            )?;
            check(ffi::ACaptureSessionOutputContainer_create(&mut open.container), "making the output list")?;
            check(ffi::ACaptureSessionOutput_create(window_ptr, &mut open.output), "making the output")?;
            check(ffi::ACaptureSessionOutputContainer_add(open.container, open.output), "adding the output")?;
            check(ffi::ACameraOutputTarget_create(window_ptr, &mut open.target), "making the target")?;
            // The recording template: a steady frame rate matters more on
            // a call than the preview template's quickest focus.
            check(
                ffi::ACameraDevice_createCaptureRequest(open.device, ffi::ACameraDevice_request_template::TEMPLATE_RECORD, &mut open.request),
                "making the request",
            )?;
            check(ffi::ACaptureRequest_addTarget(open.request, open.target), "aiming the request")?;
            if let Some((lo, hi)) = pick_fps_range(&found.fps_ranges, request.fps) {
                let range = [lo, hi];
                // Not fatal: without it the camera picks its own rate and
                // the shared code thins the frames.
                let _ = check(
                    ffi::ACaptureRequest_setEntry_i32(
                        open.request,
                        ffi::acamera_metadata_tag::ACAMERA_CONTROL_AE_TARGET_FPS_RANGE.0,
                        2,
                        range.as_ptr(),
                    ),
                    "setting the frame rate",
                );
            }
            check(
                ffi::ACameraDevice_createCaptureSession(open.device, open.container, &*open.session_callbacks, &mut open.session),
                "starting the session",
            )?;
            let mut requests = [open.request];
            check(
                ffi::ACameraCaptureSession_setRepeatingRequest(open.session, ptr::null_mut(), 1, requests.as_mut_ptr(), ptr::null_mut()),
                "starting the frames",
            )?;
        }
        Ok(open)
    }
}

impl Drop for Open {
    /// Closes in the reverse order of opening: frames stop, the session
    /// and the camera close, and only then do the reader and its window
    /// go, since the camera may write into them until it is closed.
    fn drop(&mut self) {
        // SAFETY: each handle is either null (skipped) or one the NDK gave
        // `Open::new`, freed exactly once here.
        unsafe {
            if !self.session.is_null() {
                ffi::ACameraCaptureSession_stopRepeating(self.session);
                ffi::ACameraCaptureSession_close(self.session);
            }
            if !self.request.is_null() {
                ffi::ACaptureRequest_free(self.request);
            }
            if !self.target.is_null() {
                ffi::ACameraOutputTarget_free(self.target);
            }
            if !self.container.is_null() {
                ffi::ACaptureSessionOutputContainer_free(self.container);
            }
            if !self.output.is_null() {
                ffi::ACaptureSessionOutput_free(self.output);
            }
            if !self.device.is_null() {
                ffi::ACameraDevice_close(self.device);
            }
        }
        self.window.take();
        self.reader.take();
        // SAFETY: non-null (checked in `new`) and freed once, after
        // everything opened through it.
        unsafe { ffi::ACameraManager_delete(self.manager) };
    }
}

/// Finds the camera facing `facing`, with its orientation, sizes and
/// frame rates. Logical cameras come first in the id list, so the first
/// match is the phone's main camera on that side.
fn find(manager: *mut ffi::ACameraManager, facing: Facing) -> Result<Found, String> {
    let want = match facing {
        Facing::Front => ffi::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_FRONT.0,
        Facing::Back => ffi::acamera_metadata_enum_acamera_lens_facing::ACAMERA_LENS_FACING_BACK.0,
    };
    let mut list: *mut ffi::ACameraIdList = ptr::null_mut();
    // SAFETY: an out-pointer; the list is freed below.
    check(unsafe { ffi::ACameraManager_getCameraIdList(manager, &mut list) }, "listing cameras")?;
    // SAFETY: the NDK returned it non-null with `numCameras` valid ids.
    let ids: Vec<CString> = unsafe {
        let list = &*list;
        (0..list.numCameras.max(0) as usize).map(|i| CStr::from_ptr(*list.cameraIds.add(i)).to_owned()).collect()
    };
    // SAFETY: the list from above, freed once.
    unsafe { ffi::ACameraManager_deleteCameraIdList(list) };
    for id in ids {
        let mut meta: *mut ffi::ACameraMetadata = ptr::null_mut();
        // SAFETY: a valid id and an out-pointer; freed below.
        if unsafe { ffi::ACameraManager_getCameraCharacteristics(manager, id.as_ptr(), &mut meta) } != ffi::camera_status_t::ACAMERA_OK {
            continue;
        }
        let lens = entry_u8(meta, ffi::acamera_metadata_tag::ACAMERA_LENS_FACING.0).first().copied();
        let found = (lens.map(u32::from) == Some(want)).then(|| {
            let orientation = entry_i32(meta, ffi::acamera_metadata_tag::ACAMERA_SENSOR_ORIENTATION.0).first().copied().unwrap_or(0);
            let configs = entry_i32(meta, ffi::acamera_metadata_tag::ACAMERA_SCALER_AVAILABLE_STREAM_CONFIGURATIONS.0);
            Found {
                id: id.clone(),
                orientation: orientation.rem_euclid(360) as u32,
                sizes: yuv_output_sizes(&configs),
                fps_ranges: entry_i32(meta, ffi::acamera_metadata_tag::ACAMERA_CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES.0)
                    .chunks_exact(2)
                    .map(|r| (r[0], r[1]))
                    .collect(),
            }
        });
        // SAFETY: the metadata from above, freed once.
        unsafe { ffi::ACameraMetadata_free(meta) };
        if let Some(found) = found {
            return Ok(found);
        }
    }
    Err(match facing {
        Facing::Front => "This phone has no front camera".into(),
        Facing::Back => "This phone has no back camera".into(),
    })
}

/// The YUV output sizes in a stream configuration list, which is flat
/// groups of four: format, width, height, and 1 for an input or 0 for an
/// output.
fn yuv_output_sizes(configs: &[i32]) -> Vec<(u32, u32)> {
    configs
        .chunks_exact(4)
        .filter(|c| c[0] == YUV_420_888 && c[3] == 0 && c[1] > 0 && c[2] > 0)
        .map(|c| (c[1] as u32, c[2] as u32))
        .collect()
}

/// One metadata entry's bytes, copied out; empty when it is missing.
fn entry_u8(meta: *const ffi::ACameraMetadata, tag: u32) -> Vec<u8> {
    let mut entry = std::mem::MaybeUninit::<ffi::ACameraMetadata_const_entry>::uninit();
    // SAFETY: valid metadata and an out-pointer; on success the entry
    // points at `count` values of the tag's type, which for the tags asked
    // for here is u8, living as long as the metadata.
    unsafe {
        if ffi::ACameraMetadata_getConstEntry(meta, tag, entry.as_mut_ptr()) != ffi::camera_status_t::ACAMERA_OK {
            return Vec::new();
        }
        let entry = entry.assume_init();
        std::slice::from_raw_parts(entry.data.u8_, entry.count as usize).to_vec()
    }
}

/// One metadata entry's 32-bit integers, copied out; empty when it is
/// missing.
fn entry_i32(meta: *const ffi::ACameraMetadata, tag: u32) -> Vec<i32> {
    let mut entry = std::mem::MaybeUninit::<ffi::ACameraMetadata_const_entry>::uninit();
    // SAFETY: as in `entry_u8`, for tags whose type is i32.
    unsafe {
        if ffi::ACameraMetadata_getConstEntry(meta, tag, entry.as_mut_ptr()) != ffi::camera_status_t::ACAMERA_OK {
            return Vec::new();
        }
        let entry = entry.assume_init();
        std::slice::from_raw_parts(entry.data.i32_, entry.count as usize).to_vec()
    }
}

/// Hands one YUV image to the shared code. Skipped (with a log line) if
/// the image is not the three-plane YUV that was asked for.
fn hand_over(image: &Image, rotation: u32) {
    let planes = || -> ndk::media_error::Result<CameraImage<'_>> {
        Ok(CameraImage {
            width: image.width()? as u32,
            height: image.height()? as u32,
            pixels: Pixels::Yuv420 {
                y: image.plane_data(0)?,
                y_row: image.plane_row_stride(0)? as usize,
                u: image.plane_data(1)?,
                v: image.plane_data(2)?,
                uv_row: image.plane_row_stride(1)? as usize,
                uv_pixel: image.plane_pixel_stride(1)? as usize,
            },
            rotation,
        })
    };
    match planes() {
        Ok(picture) => platform::camera_frame(&picture),
        Err(e) => log::warn!("camera: unreadable frame: {e}"),
    }
}

/// The camera was taken away: another app with a higher priority opened
/// it, or it was unplugged.
unsafe extern "C" fn on_disconnected(_context: *mut c_void, _device: *mut ffi::ACameraDevice) {
    platform::camera_failed("Another app took the camera");
}

/// The camera failed. The code is one of the `ERROR_CAMERA_*` values;
/// none is something the user can fix from here.
unsafe extern "C" fn on_error(_context: *mut c_void, _device: *mut ffi::ACameraDevice, error: c_int) {
    log::warn!("camera: device error {error}");
    platform::camera_failed("The camera stopped working");
}

/// The session's state changes. Nothing to do: the session is closed by
/// [`Open`]'s drop, and its frames are seen through the image reader.
unsafe extern "C" fn on_session_state(_context: *mut c_void, _session: *mut ffi::ACameraCaptureSession) {}
