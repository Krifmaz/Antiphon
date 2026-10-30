//! Call audio on Android, opened through AAudio directly rather than cpal.
//!
//! cpal 0.18's AAudio backend never sets an input preset or a usage, so its
//! microphone stream gets AAudio's default preset, VOICE_RECOGNITION, and its
//! speaker stream the default usage, MEDIA. Android only runs the platform's
//! own echo canceller and noise suppressor on a VOICE_COMMUNICATION capture,
//! and only routes a stream to the earpiece (or to the loudspeaker when
//! `setSpeakerphoneOn` asks for it) when its usage is VOICE_COMMUNICATION.
//! So a call over cpal sounds like a dictation app recording a music player.
//! This module opens the two streams itself with those attributes set.
//!
//! The attributes arrived in API level 28, and the app's minimum is 26.
//! Android's linker resolves every symbol when a library loads, so naming
//! `AAudioStreamBuilder_setInputPreset` directly would stop the whole app
//! loading on Android 8. The API 28 functions are therefore looked up at run
//! time with `dlsym`; when they are missing, [`open_capture`] and
//! [`open_playback`] return an error and `device.rs` falls back to cpal,
//! which is what those phones got before. Everything else used here is
//! API 26 and comes from `ndk-sys`.
//!
//! Both streams are opened as mono 32-bit float at 48 kHz, the format the
//! rest of the call pipeline uses. AAudio's legacy path (which the default
//! performance mode selects) converts to whatever the hardware runs at;
//! low-latency mode is deliberately not asked for, because the MMAP path it
//! picks on many phones skips the pre-processing effects this module exists
//! to engage.

use std::ffi::{c_void, CStr};
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use ndk_sys::{
    aaudio_result_t, AAudioStream, AAudioStreamBuilder, AAudioStreamBuilder_delete, AAudioStreamBuilder_openStream,
    AAudioStreamBuilder_setChannelCount, AAudioStreamBuilder_setDataCallback, AAudioStreamBuilder_setDirection,
    AAudioStreamBuilder_setErrorCallback, AAudioStreamBuilder_setFormat, AAudioStreamBuilder_setSampleRate,
    AAudioStreamBuilder_setSharingMode, AAudioStream_close, AAudioStream_getChannelCount, AAudioStream_getFormat,
    AAudioStream_getSampleRate, AAudioStream_requestStart, AAudioStream_requestStop, AAudio_convertResultToText,
    AAudio_createStreamBuilder,
};

/// The rate both streams are opened at, in Hz. The call pipeline encodes
/// and decodes at 48 kHz, and asking AAudio for it moves the resampling
/// into the platform, where it is cheaper than ours.
pub(crate) const SAMPLE_RATE: u32 = 48_000;

// The values below are AAudio's, from <aaudio/AAudio.h>. They are part of
// the NDK's stable ABI. They are spelt out here rather than taken from
// ndk-sys because bindgen gives each enum its own integer type, which is i32
// on some targets and u32 on others, and a cast that is needed on one target
// is a lint on another.

/// `AAUDIO_OK`.
const OK: aaudio_result_t = 0;
/// `AAUDIO_DIRECTION_OUTPUT`.
const DIRECTION_OUTPUT: i32 = 0;
/// `AAUDIO_DIRECTION_INPUT`.
const DIRECTION_INPUT: i32 = 1;
/// `AAUDIO_FORMAT_PCM_FLOAT`.
const FORMAT_PCM_FLOAT: i32 = 2;
/// `AAUDIO_SHARING_MODE_SHARED`: a call must not lock other apps (or the
/// ringtone of a second call) out of the device.
const SHARING_MODE_SHARED: i32 = 1;
/// `AAUDIO_CALLBACK_RESULT_CONTINUE`.
const CALLBACK_CONTINUE: i32 = 0;
/// `AAUDIO_CALLBACK_RESULT_STOP`.
const CALLBACK_STOP: i32 = 1;
/// `AAUDIO_USAGE_VOICE_COMMUNICATION`: routes playback the way a phone call
/// is routed, to the earpiece or, with the speakerphone on, the loudspeaker.
const USAGE_VOICE_COMMUNICATION: i32 = 2;
/// `AAUDIO_CONTENT_TYPE_SPEECH`.
const CONTENT_TYPE_SPEECH: i32 = 1;
/// `AAUDIO_INPUT_PRESET_VOICE_COMMUNICATION`: the capture preset that the
/// platform attaches its echo canceller and noise suppressor to.
const INPUT_PRESET_VOICE_COMMUNICATION: i32 = 7;
/// `AAUDIO_SESSION_ID_ALLOCATE`: asks for a session id, which the Java
/// audio effects need to find the stream.
const SESSION_ID_ALLOCATE: i32 = 0;

/// Why a stream could not be opened here. `device.rs` only logs it before
/// falling back to cpal, so it carries text rather than structure.
#[derive(Debug)]
pub(crate) struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Turns an AAudio result code into an [`Error`] naming what failed.
fn check(what: &str, code: aaudio_result_t) -> Result<(), Error> {
    if code == OK {
        return Ok(());
    }
    // SAFETY: AAudio returns a static string for every code, known or not.
    let text = unsafe { CStr::from_ptr(AAudio_convertResultToText(code)) };
    Err(Error(format!("{what}: {} ({code})", text.to_string_lossy())))
}

/// The API level 28 builder setters, found at run time (see the module
/// documentation for why they are not linked).
struct Api28 {
    /// `AAudioStreamBuilder_setInputPreset`.
    set_input_preset: unsafe extern "C" fn(*mut AAudioStreamBuilder, i32),
    /// `AAudioStreamBuilder_setUsage`.
    set_usage: unsafe extern "C" fn(*mut AAudioStreamBuilder, i32),
    /// `AAudioStreamBuilder_setContentType`.
    set_content_type: unsafe extern "C" fn(*mut AAudioStreamBuilder, i32),
    /// `AAudioStreamBuilder_setSessionId`.
    set_session_id: unsafe extern "C" fn(*mut AAudioStreamBuilder, i32),
    /// `AAudioStream_getSessionId`.
    get_session_id: unsafe extern "C" fn(*mut AAudioStream) -> i32,
}

/// The API 28 functions, or `None` on Android 8.0 and 8.1, where they do
/// not exist and the caller should use cpal instead. Looked up once.
fn api28() -> Option<&'static Api28> {
    static API: OnceLock<Option<Api28>> = OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: libaaudio is already loaded (ndk-sys links it), so this
        // only takes another reference to it, which is never released.
        let lib = unsafe { libc::dlopen(c"libaaudio.so".as_ptr(), libc::RTLD_NOW) };
        if lib.is_null() {
            return None;
        }
        /// Looks `name` up in libaaudio, as a pointer of whatever function
        /// type the caller transmutes it to.
        fn sym(lib: *mut c_void, name: &CStr) -> Option<*mut c_void> {
            // SAFETY: `lib` is a live handle from dlopen and `name` is
            // NUL-terminated.
            let ptr = unsafe { libc::dlsym(lib, name.as_ptr()) };
            (!ptr.is_null()).then_some(ptr)
        }
        // SAFETY: each symbol is transmuted to the signature its NDK header
        // declares.
        unsafe {
            Some(Api28 {
                set_input_preset: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut AAudioStreamBuilder, i32)>(
                    sym(lib, c"AAudioStreamBuilder_setInputPreset")?,
                ),
                set_usage: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut AAudioStreamBuilder, i32)>(
                    sym(lib, c"AAudioStreamBuilder_setUsage")?,
                ),
                set_content_type: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut AAudioStreamBuilder, i32)>(
                    sym(lib, c"AAudioStreamBuilder_setContentType")?,
                ),
                set_session_id: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut AAudioStreamBuilder, i32)>(
                    sym(lib, c"AAudioStreamBuilder_setSessionId")?,
                ),
                get_session_id: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut AAudioStream) -> i32>(
                    sym(lib, c"AAudioStream_getSessionId")?,
                ),
            })
        }
    })
    .as_ref()
}

/// An `AAudioStreamBuilder`, deleted when dropped.
struct Builder(NonNull<AAudioStreamBuilder>);

impl Builder {
    /// A new builder, set up for a shared mono float stream at
    /// [`SAMPLE_RATE`] in `direction`.
    fn new(direction: i32) -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: `raw` is a valid out-pointer.
        check("creating an AAudio stream builder", unsafe { AAudio_createStreamBuilder(&mut raw) })?;
        let builder = Builder(NonNull::new(raw).ok_or_else(|| Error("AAudio returned no stream builder".into()))?);
        let b = builder.0.as_ptr();
        // SAFETY: `b` is a live builder; these setters only store values.
        unsafe {
            AAudioStreamBuilder_setDirection(b, direction);
            AAudioStreamBuilder_setFormat(b, FORMAT_PCM_FLOAT);
            AAudioStreamBuilder_setChannelCount(b, 1);
            AAudioStreamBuilder_setSampleRate(b, SAMPLE_RATE as i32);
            AAudioStreamBuilder_setSharingMode(b, SHARING_MODE_SHARED);
        }
        Ok(builder)
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // SAFETY: the builder is live and deleted only here. Any stream it
        // opened is independent of it.
        unsafe { AAudioStreamBuilder_delete(self.0.as_ptr()) };
    }
}

/// An open, running AAudio stream with the callback it calls. Dropping it
/// stops and closes the stream, then frees the callback.
pub(crate) struct Stream {
    /// The stream itself.
    raw: NonNull<AAudioStream>,
    /// The boxed callback AAudio was given as user data, owned here and
    /// freed by `free` only after the stream is closed, since AAudio may be
    /// inside it until `AAudioStream_close` returns.
    user: *mut c_void,
    /// Frees `user` as the box type it was made from.
    free: unsafe fn(*mut c_void),
    /// The platform echo canceller and noise suppressor attached to a
    /// capture stream's session, released when the stream is.
    _effects: Option<effects::Effects>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: the stream is live and closed only here; close blocks
        // until any running callback has returned, after which nothing
        // else refers to `user`.
        unsafe {
            AAudioStream_requestStop(self.raw.as_ptr());
            AAudioStream_close(self.raw.as_ptr());
            (self.free)(self.user);
        }
    }
}

/// Frees a `Box<F>` that was turned into a raw pointer.
///
/// # Safety
/// `user` must come from `Box::<F>::into_raw` and not be used again.
unsafe fn free_box<F>(user: *mut c_void) {
    drop(unsafe { Box::from_raw(user.cast::<F>()) });
}

/// AAudio's error callback. AAudio calls it on a thread of its own when the
/// stream dies, most often when the device it was on goes away. Closing the
/// stream from here is not allowed, so it only reports; cpal behaves the
/// same, and the call ends on its own when the audio stops.
unsafe extern "C" fn on_error(_stream: *mut AAudioStream, _user: *mut c_void, error: aaudio_result_t) {
    if let Err(e) = check("AAudio stream error", error) {
        eprintln!("[audio] {e}");
    }
}

/// AAudio's data callback for capture: hands the mono samples to the
/// caller's closure. A panic stops the stream rather than unwinding into C.
unsafe extern "C" fn on_input<F: FnMut(&[f32])>(
    _stream: *mut AAudioStream,
    user: *mut c_void,
    data: *mut c_void,
    frames: i32,
) -> i32 {
    // SAFETY: `user` is the `Box<F>` from `open_capture`, which outlives the
    // stream, and AAudio never runs two callbacks at once. `data` holds
    // `frames` mono floats, as the stream was checked to deliver.
    let ran = catch_unwind(AssertUnwindSafe(|| unsafe {
        let on_frame = &mut *user.cast::<F>();
        on_frame(std::slice::from_raw_parts(data.cast::<f32>(), frames.max(0) as usize));
    }));
    if ran.is_ok() {
        CALLBACK_CONTINUE
    } else {
        CALLBACK_STOP
    }
}

/// AAudio's data callback for playback: has the caller's closure fill the
/// mono buffer. A panic stops the stream rather than unwinding into C.
unsafe extern "C" fn on_output<F: FnMut(&mut [f32])>(
    _stream: *mut AAudioStream,
    user: *mut c_void,
    data: *mut c_void,
    frames: i32,
) -> i32 {
    // SAFETY: as for `on_input`, with `data` writable for `frames` floats.
    let ran = catch_unwind(AssertUnwindSafe(|| unsafe {
        let fill = &mut *user.cast::<F>();
        fill(std::slice::from_raw_parts_mut(data.cast::<f32>(), frames.max(0) as usize));
    }));
    if ran.is_ok() {
        CALLBACK_CONTINUE
    } else {
        CALLBACK_STOP
    }
}

/// Opens and starts the stream `builder` describes, with `callback` as its
/// data callback and `user` as that callback's state. On failure the stream
/// is closed again and `user` is left for the caller to reclaim.
///
/// # Safety
/// `user` must be valid for `callback` until the returned stream is
/// dropped, and `free` must free it.
unsafe fn open(
    builder: &Builder,
    callback: unsafe extern "C" fn(*mut AAudioStream, *mut c_void, *mut c_void, i32) -> i32,
    user: *mut c_void,
    free: unsafe fn(*mut c_void),
) -> Result<Stream, Error> {
    let b = builder.0.as_ptr();
    let mut raw = ptr::null_mut();
    // SAFETY: `b` is live, `raw` is a valid out-pointer, and the caller
    // vouches for `user`.
    unsafe {
        AAudioStreamBuilder_setDataCallback(b, Some(callback), user);
        AAudioStreamBuilder_setErrorCallback(b, Some(on_error), ptr::null_mut());
        check("opening the AAudio stream", AAudioStreamBuilder_openStream(b, &mut raw))?;
    }
    let raw = NonNull::new(raw).ok_or_else(|| Error("AAudio opened no stream".into()))?;
    /// Closes a stream that will not be returned, leaving `user` alone.
    fn discard(raw: NonNull<AAudioStream>) {
        // SAFETY: the stream is live and not used again.
        unsafe { AAudioStream_close(raw.as_ptr()) };
    }
    // The callbacks assume mono floats, and the call pipeline assumes 48 kHz
    // on both streams, so anything else is handed back to cpal, which
    // converts.
    // SAFETY: `raw` is a live stream.
    let (format, channels, rate) =
        unsafe { (AAudioStream_getFormat(raw.as_ptr()), AAudioStream_getChannelCount(raw.as_ptr()), AAudioStream_getSampleRate(raw.as_ptr())) };
    if format != FORMAT_PCM_FLOAT || channels != 1 || rate != SAMPLE_RATE as i32 {
        discard(raw);
        return Err(Error(format!("AAudio offered format {format}, {channels} channels at {rate} Hz")));
    }
    // SAFETY: `raw` is a live stream.
    if let Err(e) = check("starting the AAudio stream", unsafe { AAudioStream_requestStart(raw.as_ptr()) }) {
        discard(raw);
        return Err(e);
    }
    Ok(Stream { raw, user, free, _effects: None })
}

/// Opens the microphone for a call: VOICE_COMMUNICATION preset, so the
/// platform's echo canceller and noise suppressor run, plus the Java
/// `AcousticEchoCanceler` and `NoiseSuppressor` on its session where the
/// phone offers them. `on_frame` gets mono samples at [`SAMPLE_RATE`] on
/// AAudio's callback thread.
///
/// On failure `on_frame` is handed back, so the caller can open cpal with it.
pub(crate) fn open_capture<F>(on_frame: F) -> Result<Stream, (Error, F)>
where
    F: FnMut(&[f32]) + Send + 'static,
{
    let Some(api) = api28() else {
        return Err((Error("AAudio input presets need Android 9".into()), on_frame));
    };
    let builder = match Builder::new(DIRECTION_INPUT) {
        Ok(builder) => builder,
        Err(e) => return Err((e, on_frame)),
    };
    // SAFETY: the builder is live; these setters only store values.
    unsafe {
        (api.set_input_preset)(builder.0.as_ptr(), INPUT_PRESET_VOICE_COMMUNICATION);
        (api.set_session_id)(builder.0.as_ptr(), SESSION_ID_ALLOCATE);
    }
    let user = Box::into_raw(Box::new(on_frame)).cast::<c_void>();
    // SAFETY: `user` is a `Box<F>`, which `free_box::<F>` frees, and it is
    // kept alive by the returned stream.
    match unsafe { open(&builder, on_input::<F>, user, free_box::<F>) } {
        Ok(mut stream) => {
            // SAFETY: the stream is live.
            let session = unsafe { (api.get_session_id)(stream.raw.as_ptr()) };
            stream._effects = effects::Effects::attach(session);
            Ok(stream)
        }
        // SAFETY: `open` failed, so AAudio has let go of `user`, which is
        // still the `Box<F>` made above.
        Err(e) => Err((e, *unsafe { Box::from_raw(user.cast::<F>()) })),
    }
}

/// Opens the speaker for a call: usage VOICE_COMMUNICATION and content
/// type SPEECH, so it plays through the earpiece unless the speakerphone is
/// on, as `platform::set_call_audio` intends. `fill` is asked for mono
/// samples at [`SAMPLE_RATE`] on AAudio's callback thread.
pub(crate) fn open_playback<F>(fill: F) -> Result<Stream, Error>
where
    F: FnMut(&mut [f32]) + Send + 'static,
{
    let api = api28().ok_or_else(|| Error("AAudio usages need Android 9".into()))?;
    let builder = Builder::new(DIRECTION_OUTPUT)?;
    // SAFETY: the builder is live; these setters only store values.
    unsafe {
        (api.set_usage)(builder.0.as_ptr(), USAGE_VOICE_COMMUNICATION);
        (api.set_content_type)(builder.0.as_ptr(), CONTENT_TYPE_SPEECH);
    }
    let user = Box::into_raw(Box::new(fill)).cast::<c_void>();
    // SAFETY: as in `open_capture`.
    unsafe { open(&builder, on_output::<F>, user, free_box::<F>) }.inspect_err(|_| {
        // SAFETY: `open` failed, so AAudio has let go of `user`.
        unsafe { free_box::<F>(user) }
    })
}

/// The Java audio effects (`android.media.audiofx`) on a capture session.
///
/// The VOICE_COMMUNICATION preset is what normally engages the platform's
/// echo canceller and noise suppressor. Some vendors only attach them when
/// an app asks through these classes, which is also what WebRTC does on
/// Android, so this asks as well. Each step is best effort: a phone without
/// the effect, or one that refuses it, still has a working call.
mod effects {
    use jni::objects::{GlobalRef, JValue};
    use jni::{JNIEnv, JavaVM};

    /// The effect classes to attach, each with a static `isAvailable()`
    /// and `create(int sessionId)`.
    const CLASSES: [&str; 2] = ["android/media/audiofx/AcousticEchoCanceler", "android/media/audiofx/NoiseSuppressor"];

    /// The effects attached to one session, released when dropped.
    pub(crate) struct Effects(Vec<GlobalRef>);

    /// The app's Java VM, from the context android-activity recorded.
    fn vm() -> Option<JavaVM> {
        let ctx = ndk_context::android_context();
        // SAFETY: the context holds the process's JavaVM pointer.
        unsafe { JavaVM::from_raw(ctx.vm().cast()) }.ok()
    }

    /// Clears a Java exception left by a failed call, so the next JNI call
    /// on this thread does not trip over it.
    fn clear(env: &mut JNIEnv) {
        if env.exception_check().unwrap_or(false) {
            let _ = env.exception_clear();
        }
    }

    /// Creates and enables `class` on `session`, if the phone has it.
    fn create(env: &mut JNIEnv, class: &str, session: i32) -> jni::errors::Result<Option<GlobalRef>> {
        if !env.call_static_method(class, "isAvailable", "()Z", &[])?.z()? {
            return Ok(None);
        }
        let effect = env.call_static_method(class, "create", format!("(I)L{class};"), &[JValue::Int(session)])?.l()?;
        if effect.is_null() {
            return Ok(None);
        }
        env.call_method(&effect, "setEnabled", "(Z)I", &[JValue::Bool(1)])?;
        Ok(Some(env.new_global_ref(effect)?))
    }

    impl Effects {
        /// Attaches every available effect to `session`. `None` when there
        /// is no session or no effect could be attached.
        pub(crate) fn attach(session: i32) -> Option<Self> {
            if session <= 0 {
                return None;
            }
            let vm = vm()?;
            let mut env = vm.attach_current_thread().ok()?;
            let mut effects = Vec::new();
            for class in CLASSES {
                match create(&mut env, class, session) {
                    Ok(Some(effect)) => effects.push(effect),
                    Ok(None) => {}
                    Err(e) => {
                        clear(&mut env);
                        eprintln!("[audio] could not attach {class}: {e}");
                    }
                }
            }
            (!effects.is_empty()).then_some(Effects(effects))
        }
    }

    impl Drop for Effects {
        /// Releases each effect's native side now rather than whenever the
        /// garbage collector gets to it, since the platform caps how many
        /// effect instances can exist at once.
        fn drop(&mut self) {
            let Some(vm) = vm() else { return };
            let Ok(mut env) = vm.attach_current_thread() else { return };
            for effect in &self.0 {
                if env.call_method(effect, "release", "()V", &[]).is_err() {
                    clear(&mut env);
                }
            }
        }
    }
}
