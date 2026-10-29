//! The Android framework calls a NativeActivity cannot make from the NDK
//! alone, made through JNI on the activity `android-activity` hands over.
//! Each one fails soft: a phone that refuses one of these still gets a
//! working app, just without that convenience.
//!
//! Most are plain JNI calls on framework objects. The ones that need a
//! Java class of the app's own -- the foreground service, the push
//! receiver, anything that has to run on the UI thread -- call into
//! `java/org/antiphon/app`, and the answers come back through
//! [`crate::native`].
//!
//! Threading: these run on the shared app's thread, which is
//! `android_main`'s and not the UI thread. Each call attaches that thread to
//! the JVM (a no-op after the first) and works inside a local frame. The
//! framework calls used here are safe off the UI thread; anything that must
//! run on it is left to the Java side, which posts it there itself.

use std::fs::File;
use std::io::{self, Read};
use std::os::fd::FromRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use antiphon_mobile::camera::CameraRequest;
use antiphon_mobile::platform::{BackgroundWork, Network, Platform};
use jni::objects::{JClass, JObject, JValue};
use jni::{JNIEnv, JavaVM};

use crate::camera;

/// Android's half of [`Platform`]. It holds nothing: the activity and the
/// JavaVM are fetched from `ndk-context` on every call, so it is free to
/// share across threads.
pub struct Android;

impl Platform for Android {
    fn has_mic_permission(&self) -> bool {
        has_mic_permission()
    }

    fn request_mic_permission(&self) {
        request_mic_permission();
    }

    fn set_call_audio(&self, in_call: bool, speaker: bool) {
        set_call_audio(in_call, speaker);
    }

    fn copy_to_clipboard(&self, text: &str) {
        copy_to_clipboard(text);
    }

    fn read_clipboard(&self) -> Option<String> {
        read_clipboard()
    }

    fn system_insets(&self) -> (f32, f32) {
        system_insets()
    }

    fn network(&self) -> Network {
        network()
    }

    fn battery_saver(&self) -> bool {
        battery_saver()
    }

    fn set_background_work(&self, work: &BackgroundWork) {
        set_background_work(work);
    }

    fn pick_file(&self) {
        call_activity("opening the file picker", "pickFile");
    }

    fn read_file(&self, handle: &str) -> io::Result<Vec<u8>> {
        read_file(handle)
    }

    fn downloads_dir(&self) -> Option<PathBuf> {
        static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
        DIR.get_or_init(downloads_dir).clone()
    }

    fn notify(&self, title: &str, body: &str) {
        notify(title, body);
    }

    fn register_push(&self) {
        register_push();
    }

    fn ring(&self, from: Option<&str>) {
        ring(from);
    }

    fn request_battery_exemption(&self) {
        call_activity("asking for a battery exemption", "requestBatteryExemption");
    }

    /// Every Android phone this app runs on has at least one camera
    /// service; a phone without a camera fails when it is started.
    fn has_camera(&self) -> bool {
        true
    }

    fn has_camera_permission(&self) -> bool {
        has_permission(CAMERA)
    }

    fn request_camera_permission(&self) {
        request_permission(CAMERA, CAMERA_REQUEST);
    }

    fn start_camera(&self, request: CameraRequest) -> Result<(), String> {
        camera::start(request)
    }

    fn stop_camera(&self) {
        camera::stop();
    }
}

/// `Manifest.permission.RECORD_AUDIO`, the run-time microphone permission.
const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
/// `Manifest.permission.CAMERA`, the run-time camera permission.
const CAMERA: &str = "android.permission.CAMERA";
/// The request code for the microphone prompt.
const MIC_REQUEST: i32 = 1;
/// The request code for the camera prompt. `AntiphonActivity` uses 2 for
/// the file picker and 3 for notifications, so this takes the next.
const CAMERA_REQUEST: i32 = 4;
/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// `AudioManager.MODE_NORMAL`: no call, audio plays as media.
const MODE_NORMAL: i32 = 0;
/// `AudioManager.MODE_IN_COMMUNICATION`: a VoIP call, with the platform's
/// echo canceller engaged and the earpiece as the default output.
const MODE_IN_COMMUNICATION: i32 = 3;

/// Runs `f` with a JNI environment and the activity, logging any failure.
/// Callable from any Rust thread: the thread is attached to the JVM if it
/// was not already, and stays attached. A Java exception thrown inside `f`
/// is described to logcat and cleared before this returns `None`.
fn with_activity<T>(what: &str, f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<T>) -> Option<T> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity initialises ndk-context with the process's
    // JavaVM and a global reference to the activity before android_main
    // runs, and both outlive it.
    let vm = match unsafe { JavaVM::from_raw(ctx.vm().cast()) } {
        Ok(vm) => vm,
        Err(e) => {
            log::warn!("{what}: no JavaVM: {e}");
            return None;
        }
    };
    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            log::warn!("{what}: could not attach to the JavaVM: {e}");
            return None;
        }
    };
    // SAFETY: a global reference, owned by ndk-context; JObject does not
    // delete it on drop.
    let activity = unsafe { JObject::from_raw(ctx.context().cast()) };
    // A frame, so the local references made inside are freed on return:
    // android_main's thread never returns to Java, which would otherwise
    // be the only thing that frees them.
    match env.with_local_frame(16, |env| f(env, &activity)) {
        Ok(value) => Some(value),
        Err(e) => {
            // A Java exception left pending would abort the next JNI call.
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_describe();
                let _ = env.exception_clear();
            }
            log::warn!("{what}: {e}");
            None
        }
    }
}

/// Whether the user has granted microphone access. Android asks at run
/// time, not at install, so this can change while the app is open.
fn has_mic_permission() -> bool {
    has_permission(RECORD_AUDIO)
}

/// Shows the system's microphone permission prompt. The answer is not
/// reported back to native code, so callers check [`has_mic_permission`]
/// again when the activity resumes.
fn request_mic_permission() {
    request_permission(RECORD_AUDIO, MIC_REQUEST);
}

/// Whether the user has granted the run-time permission `permission`
/// (a `Manifest.permission` name).
fn has_permission(permission: &str) -> bool {
    with_activity("checking a permission", |env, activity| {
        let name = env.new_string(permission)?;
        let result = env
            .call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[JValue::Object(&name)])?
            .i()?;
        Ok(result == PERMISSION_GRANTED)
    })
    .unwrap_or(false)
}

/// Shows the system's prompt for `permission`. `code` only tells the
/// answers apart in `onRequestPermissionsResult`, which the shared code
/// does not use: it checks [`has_permission`] again instead.
fn request_permission(permission: &str, code: i32) {
    with_activity("requesting a permission", |env, activity| {
        let name = env.new_string(permission)?;
        let array = env.new_object_array(1, "java/lang/String", &name)?;
        env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&array), JValue::Int(code)])?;
        Ok(())
    });
}

/// The activity's `AudioManager` (`Context.AUDIO_SERVICE`).
fn audio_manager<'a>(env: &mut JNIEnv<'a>, activity: &JObject) -> jni::errors::Result<JObject<'a>> {
    let name = env.new_string("audio")?;
    env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?
        .l()
}

/// Puts the phone in voice-call mode: the platform's own echo canceller
/// and noise suppressor engage, and audio goes to the earpiece unless the
/// speaker is on. Without this a call plays like music, out of the
/// loudspeaker, and the microphone hears it.
fn set_call_audio(in_call: bool, speaker: bool) {
    with_activity("setting the audio mode", |env, activity| {
        let manager = audio_manager(env, activity)?;
        let mode = if in_call { MODE_IN_COMMUNICATION } else { MODE_NORMAL };
        env.call_method(&manager, "setMode", "(I)V", &[JValue::Int(mode)])?;
        env.call_method(&manager, "setSpeakerphoneOn", "(Z)V", &[JValue::Bool(u8::from(in_call && speaker))])?;
        Ok(())
    });
}

/// Ensures this thread has a Looper. Some framework services build a
/// Handler when first fetched, which throws on a thread without one, and
/// android_main's thread is not the UI thread.
fn prepare_looper(env: &mut JNIEnv) -> jni::errors::Result<()> {
    let looper = env.call_static_method("android/os/Looper", "myLooper", "()Landroid/os/Looper;", &[])?.l()?;
    if looper.is_null() {
        env.call_static_method("android/os/Looper", "prepare", "()V", &[])?;
    }
    Ok(())
}

/// The activity's `ClipboardManager`, with a Looper prepared first because
/// fetching it the first time builds a Handler.
fn clipboard_manager<'a>(env: &mut JNIEnv<'a>, activity: &JObject) -> jni::errors::Result<JObject<'a>> {
    prepare_looper(env)?;
    let name = env.new_string("clipboard")?;
    env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?
        .l()
}

/// Copies `text` to the system clipboard.
fn copy_to_clipboard(text: &str) {
    with_activity("copying to the clipboard", |env, activity| {
        let manager = clipboard_manager(env, activity)?;
        let label = env.new_string("Stoop")?;
        let value = env.new_string(text)?;
        let clip = env
            .call_static_method(
                "android/content/ClipData",
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[JValue::Object(&label), JValue::Object(&value)],
            )?
            .l()?;
        env.call_method(&manager, "setPrimaryClip", "(Landroid/content/ClipData;)V", &[JValue::Object(&clip)])?;
        Ok(())
    });
}

/// The clipboard's text, if it holds any. A soft keyboard has no Ctrl+V,
/// so the paste buttons read it here.
fn read_clipboard() -> Option<String> {
    with_activity("reading the clipboard", |env, activity| {
        let manager = clipboard_manager(env, activity)?;
        let clip = env.call_method(&manager, "getPrimaryClip", "()Landroid/content/ClipData;", &[])?.l()?;
        if clip.is_null() {
            return Ok(None);
        }
        let count = env.call_method(&clip, "getItemCount", "()I", &[])?.i()?;
        if count < 1 {
            return Ok(None);
        }
        let item = env.call_method(&clip, "getItemAt", "(I)Landroid/content/ClipData$Item;", &[JValue::Int(0)])?.l()?;
        let text = env.call_method(&item, "coerceToText", "(Landroid/content/Context;)Ljava/lang/CharSequence;", &[JValue::Object(activity)])?.l()?;
        if text.is_null() {
            return Ok(None);
        }
        let string = env.call_method(&text, "toString", "()Ljava/lang/String;", &[])?.l()?;
        let string: String = env.get_string(&string.into())?.into();
        Ok(Some(string))
    })
    .flatten()
}

/// The parts of the screen the system draws over -- status bar, camera
/// cutout, gesture bar, and the soft keyboard while it is up -- in
/// physical pixels, as (top, bottom). Android 15 draws every app edge to
/// edge, so without this the first row of the app sits under the clock
/// and the last under the home bar.
///
/// The live answer comes from the window's insets. Those are not there
/// until the view is attached (the first frames of a launch) and can fail
/// to read on some builds; reading zero then put the whole app under the
/// bars. So a missing or empty answer falls back to the system's own
/// status and navigation bar sizes, which always exist, and the last good
/// answer is kept for the moments in between.
fn system_insets() -> (f32, f32) {
    static LAST: Mutex<Option<(f32, f32)>> = Mutex::new(None);
    let live = with_activity("reading window insets", window_insets).flatten().filter(|(top, _)| *top > 0.0);
    let insets = live.or_else(|| *LAST.lock().unwrap()).or_else(|| with_activity("reading bar sizes", bar_sizes)).unwrap_or((0.0, 0.0));
    if live.is_some() {
        *LAST.lock().unwrap() = live;
    }
    insets
}

/// The window's insets now, or `None` before the view is attached.
fn window_insets(env: &mut JNIEnv, activity: &JObject) -> jni::errors::Result<Option<(f32, f32)>> {
    let window = env.call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
    let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
    let insets = env.call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?.l()?;
    if insets.is_null() {
        return Ok(None);
    }
    let sdk = env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()?;
    if sdk >= 30 {
        // WindowInsets.Type.systemBars() | ime() | displayCutout().
        let bars = env.call_static_method("android/view/WindowInsets$Type", "systemBars", "()I", &[])?.i()?;
        let ime = env.call_static_method("android/view/WindowInsets$Type", "ime", "()I", &[])?.i()?;
        let cutout = env.call_static_method("android/view/WindowInsets$Type", "displayCutout", "()I", &[])?.i()?;
        let rect = env
            .call_method(&insets, "getInsets", "(I)Landroid/graphics/Insets;", &[JValue::Int(bars | ime | cutout)])?
            .l()?;
        let top = env.get_field(&rect, "top", "I")?.i()?;
        let bottom = env.get_field(&rect, "bottom", "I")?.i()?;
        Ok(Some((top as f32, bottom as f32)))
    } else {
        let top = env.call_method(&insets, "getSystemWindowInsetTop", "()I", &[])?.i()?;
        let bottom = env.call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])?.i()?;
        Ok(Some((top as f32, bottom as f32)))
    }
}

/// The system's status bar and navigation bar heights, from its own
/// resources: always readable, and what the bars measure when shown.
fn bar_sizes(env: &mut JNIEnv, activity: &JObject) -> jni::errors::Result<(f32, f32)> {
    let resources = env.call_method(activity, "getResources", "()Landroid/content/res/Resources;", &[])?.l()?;
    let size = |env: &mut JNIEnv, name: &str| -> jni::errors::Result<f32> {
        let name = env.new_string(name)?;
        let kind = env.new_string("dimen")?;
        let package = env.new_string("android")?;
        let id = env
            .call_method(
                &resources,
                "getIdentifier",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)I",
                &[JValue::Object(&name), JValue::Object(&kind), JValue::Object(&package)],
            )?
            .i()?;
        if id == 0 {
            return Ok(0.0);
        }
        Ok(env.call_method(&resources, "getDimensionPixelSize", "(I)I", &[JValue::Int(id)])?.i()? as f32)
    };
    let top = size(env, "status_bar_height")?;
    let bottom = size(env, "navigation_bar_height")?;
    Ok((top, bottom))
}

/// One of the app's own classes. A thread that native code attached
/// finds classes through the system class loader, which knows the
/// framework but not the APK, so this asks the activity's loader instead.
fn app_class<'a>(env: &mut JNIEnv<'a>, activity: &JObject, name: &str) -> jni::errors::Result<JClass<'a>> {
    let loader = env.call_method(activity, "getClassLoader", "()Ljava/lang/ClassLoader;", &[])?.l()?;
    let name = env.new_string(name)?;
    let class = env
        .call_method(&loader, "loadClass", "(Ljava/lang/String;)Ljava/lang/Class;", &[JValue::Object(&name)])?
        .l()?;
    Ok(JClass::from(class))
}

/// Calls a no-argument method of `AntiphonActivity`, which posts whatever
/// it does to the UI thread itself.
fn call_activity(what: &str, method: &str) {
    with_activity(what, |env, activity| {
        env.call_method(activity, method, "()V", &[])?;
        Ok(())
    });
}

/// `Build.VERSION.SDK_INT`, the API level of the running Android.
fn sdk_int(env: &mut JNIEnv) -> jni::errors::Result<i32> {
    env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()
}

/// `Context.getSystemService(name)` on the activity, for services that do
/// not need a Looper on the calling thread. May be a Java `null` on a
/// stripped-down build that lacks the service.
fn system_service<'a>(env: &mut JNIEnv<'a>, activity: &JObject, name: &str) -> jni::errors::Result<JObject<'a>> {
    let name = env.new_string(name)?;
    env.call_method(activity, "getSystemService", "(Ljava/lang/String;)Ljava/lang/Object;", &[JValue::Object(&name)])?
        .l()
}

/// `NetworkCapabilities.NET_CAPABILITY_NOT_METERED`: the user is not charged
/// for data on this network.
const NOT_METERED: i32 = 11;
/// `NetworkCapabilities.NET_CAPABILITY_TEMPORARILY_NOT_METERED` (API 30):
/// metered in general, but free for now, as some 5G plans are.
const TEMPORARILY_NOT_METERED: i32 = 25;

/// What the default network is now. "Wi-Fi" means what the user means by
/// it, data that costs nothing: a phone's hotspot, which Android marks as
/// metered, counts as mobile data, and an unmetered Ethernet adapter
/// counts as Wi-Fi.
pub(crate) fn network() -> Network {
    with_activity("reading the network type", |env, activity| {
        let manager = system_service(env, activity, "connectivity")?;
        let active = env.call_method(&manager, "getActiveNetwork", "()Landroid/net/Network;", &[])?.l()?;
        if active.is_null() {
            return Ok(Network::None);
        }
        let caps = env
            .call_method(
                &manager,
                "getNetworkCapabilities",
                "(Landroid/net/Network;)Landroid/net/NetworkCapabilities;",
                &[JValue::Object(&active)],
            )?
            .l()?;
        if caps.is_null() {
            return Ok(Network::None);
        }
        let has = |env: &mut JNIEnv, cap: i32| -> jni::errors::Result<bool> {
            env.call_method(&caps, "hasCapability", "(I)Z", &[JValue::Int(cap)])?.z()
        };
        // Some 5G plans are unmetered for a while (Android 11 and later);
        // the user is not charged for that data either.
        let unmetered = has(env, NOT_METERED)? || (sdk_int(env)? >= 30 && has(env, TEMPORARILY_NOT_METERED)?);
        Ok(if unmetered { Network::Wifi } else { Network::Metered })
    })
    .unwrap_or(Network::Unknown)
}

/// Whether the system's battery saver is on.
fn battery_saver() -> bool {
    with_activity("reading battery saver", |env, activity| {
        let power = system_service(env, activity, "power")?;
        env.call_method(&power, "isPowerSaveMode", "()Z", &[])?.z()
    })
    .unwrap_or(false)
}

/// Starts, updates or stops `CallService`, the foreground service that
/// keeps a call, listening or transfers running with the app out of sight.
fn set_background_work(work: &BackgroundWork) {
    with_activity("updating background work", |env, activity| {
        let class = app_class(env, activity, "org.antiphon.app.CallService")?;
        let call = match &work.call {
            Some(name) => JObject::from(env.new_string(name)?),
            None => JObject::null(),
        };
        let transfers = i32::try_from(work.transfers).unwrap_or(i32::MAX);
        env.call_static_method(
            &class,
            "update",
            "(Landroid/content/Context;Ljava/lang/String;ZI)V",
            &[
                JValue::Object(activity),
                JValue::Object(&call),
                JValue::Bool(u8::from(work.listening)),
                JValue::Int(transfers),
            ],
        )?;
        Ok(())
    });
}

/// Shows or withdraws the incoming-call alert.
fn ring(from: Option<&str>) {
    with_activity("ringing", |env, activity| {
        let class = app_class(env, activity, "org.antiphon.app.CallService")?;
        let from = match from {
            Some(name) => JObject::from(env.new_string(name)?),
            None => JObject::null(),
        };
        env.call_static_method(
            &class,
            "ring",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(activity), JValue::Object(&from)],
        )?;
        Ok(())
    });
}

/// Shows a plain notification that opens the app when tapped, through
/// `CallService.notify`, which also asks for the notification permission on
/// Android 13 and later.
fn notify(title: &str, body: &str) {
    with_activity("showing a notification", |env, activity| {
        let class = app_class(env, activity, "org.antiphon.app.CallService")?;
        let title = env.new_string(title)?;
        let body = env.new_string(body)?;
        env.call_static_method(
            &class,
            "notify",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;)V",
            &[JValue::Object(activity), JValue::Object(&title), JValue::Object(&body)],
        )?;
        Ok(())
    });
}

/// Asks the UnifiedPush distributor for an endpoint, which arrives later
/// as `PlatformEvent::PushRegistered`. With no distributor installed there
/// is nothing to ask, and the app works as before: reachable while open.
fn register_push() {
    let registered = with_activity("registering for push", |env, activity| {
        let class = app_class(env, activity, "org.antiphon.app.PushReceiver")?;
        env.call_static_method(&class, "register", "(Landroid/content/Context;)Z", &[JValue::Object(activity)])?
            .z()
    });
    if registered == Some(false) {
        log::info!("no UnifiedPush distributor installed (ntfy, for one); push wake-ups are off");
    }
}

/// Reads a picked document by its content URI. The content resolver opens
/// a file descriptor and the bytes are read from that, rather than copied
/// across JNI one Java array at a time. A plain path is read directly.
fn read_file(handle: &str) -> io::Result<Vec<u8>> {
    if handle.starts_with('/') {
        return std::fs::read(handle);
    }
    let fd = with_activity("opening a picked file", |env, activity| {
        let resolver = env
            .call_method(activity, "getContentResolver", "()Landroid/content/ContentResolver;", &[])?
            .l()?;
        let text = env.new_string(handle)?;
        let uri = env
            .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[JValue::Object(&text)])?
            .l()?;
        let mode = env.new_string("r")?;
        let descriptor = env
            .call_method(
                &resolver,
                "openFileDescriptor",
                "(Landroid/net/Uri;Ljava/lang/String;)Landroid/os/ParcelFileDescriptor;",
                &[JValue::Object(&uri), JValue::Object(&mode)],
            )?
            .l()?;
        if descriptor.is_null() {
            return Ok(-1);
        }
        env.call_method(&descriptor, "detachFd", "()I", &[])?.i()
    });
    match fd {
        Some(fd) if fd >= 0 => {
            // SAFETY: detachFd hands over sole ownership of an open
            // descriptor; nothing on the Java side will close it, and File
            // closes it when dropped.
            let mut file = unsafe { File::from_raw_fd(fd) };
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        // Usually the file was deleted, or its grant revoked, since it was
        // picked; with_activity has logged the exception.
        _ => Err(io::Error::new(io::ErrorKind::NotFound, "the picked file can no longer be opened")),
    }
}

/// Where received files go. On Android 11 and later an app may write into
/// the shared Downloads folder with no permission at all, and what it
/// writes shows up in the Files app, so that is the place, in a folder of
/// its own. Before that, or if the shared folder refuses for any other
/// reason, the app's own folder on shared storage, which the Files app can
/// also reach (Android/data/org.antiphon.app/files/Download).
fn downloads_dir() -> Option<PathBuf> {
    let (sdk, public, private) = with_activity("finding the downloads folder", |env, activity| {
        let sdk = sdk_int(env)?;
        let kind = env.new_string("Download")?;
        let public = env
            .call_static_method(
                "android/os/Environment",
                "getExternalStoragePublicDirectory",
                "(Ljava/lang/String;)Ljava/io/File;",
                &[JValue::Object(&kind)],
            )?
            .l()?;
        let public = file_path(env, &public)?;
        let private = env
            .call_method(activity, "getExternalFilesDir", "(Ljava/lang/String;)Ljava/io/File;", &[JValue::Object(&kind)])?
            .l()?;
        let private = file_path(env, &private)?;
        Ok((sdk, public, private))
    })?;
    if sdk >= 30 {
        if let Some(dir) = public.map(|p| p.join("Antiphon")).filter(|d| writable(d)) {
            return Some(dir);
        }
    }
    private.filter(|d| writable(d))
}

/// The absolute path of a `java.io.File`, or `None` for a Java `null`, which
/// the storage calls return when a volume is not mounted.
fn file_path(env: &mut JNIEnv, file: &JObject) -> jni::errors::Result<Option<PathBuf>> {
    if file.is_null() {
        return Ok(None);
    }
    let path = env.call_method(file, "getAbsolutePath", "()Ljava/lang/String;", &[])?.l()?;
    let path: String = env.get_string(&path.into())?.into();
    Ok(Some(PathBuf::from(path)))
}

/// Whether files can really be made in `dir`, made if missing. Tried
/// rather than assumed: storage can be unmounted, and vendors differ on
/// what the shared folders allow.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(".antiphon-write-test");
    let ok = std::fs::create_dir_all(dir).is_ok() && std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    if !ok {
        log::info!("{} is not writable", dir.display());
    }
    ok
}
