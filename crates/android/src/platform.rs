//! The handful of Android framework calls a NativeActivity cannot make
//! from the NDK alone, made through JNI on the activity `android-activity`
//! hands over. Each one fails soft: a phone that refuses one of these
//! still gets a working app, just without that convenience.

use antiphon_mobile::platform::Platform;
use jni::objects::{JObject, JValue};
use jni::{JNIEnv, JavaVM};

/// Android's half of [`Platform`].
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
}

const RECORD_AUDIO: &str = "android.permission.RECORD_AUDIO";
/// `PackageManager.PERMISSION_GRANTED`.
const PERMISSION_GRANTED: i32 = 0;
/// `AudioManager.MODE_NORMAL` and `MODE_IN_COMMUNICATION`.
const MODE_NORMAL: i32 = 0;
const MODE_IN_COMMUNICATION: i32 = 3;

/// Runs `f` with a JNI environment and the activity, logging any failure.
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
    match f(&mut env, &activity) {
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
    with_activity("checking microphone permission", |env, activity| {
        let name = env.new_string(RECORD_AUDIO)?;
        let result = env
            .call_method(activity, "checkSelfPermission", "(Ljava/lang/String;)I", &[JValue::Object(&name)])?
            .i()?;
        Ok(result == PERMISSION_GRANTED)
    })
    .unwrap_or(false)
}

/// Shows the system's microphone permission prompt. The answer is not
/// reported back to native code, so callers check [`has_mic_permission`]
/// again when the activity resumes.
fn request_mic_permission() {
    with_activity("requesting microphone permission", |env, activity| {
        let name = env.new_string(RECORD_AUDIO)?;
        let array = env.new_object_array(1, "java/lang/String", &name)?;
        env.call_method(activity, "requestPermissions", "([Ljava/lang/String;I)V", &[JValue::Object(&array), JValue::Int(1)])?;
        Ok(())
    });
}

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
        let label = env.new_string("Antiphon")?;
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

/// The parts of the screen the system draws over -- status bar, gesture
/// bar, and the soft keyboard while it is up -- in physical pixels, as
/// (top, bottom). Android 15 draws every app edge to edge, so without this
/// the first row of the app sits under the clock.
fn system_insets() -> (f32, f32) {
    with_activity("reading window insets", |env, activity| {
        let window = env.call_method(activity, "getWindow", "()Landroid/view/Window;", &[])?.l()?;
        let decor = env.call_method(&window, "getDecorView", "()Landroid/view/View;", &[])?.l()?;
        let insets = env.call_method(&decor, "getRootWindowInsets", "()Landroid/view/WindowInsets;", &[])?.l()?;
        if insets.is_null() {
            return Ok((0.0, 0.0));
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
            Ok((top as f32, bottom as f32))
        } else {
            let top = env.call_method(&insets, "getSystemWindowInsetTop", "()I", &[])?.i()?;
            let bottom = env.call_method(&insets, "getSystemWindowInsetBottom", "()I", &[])?.i()?;
            Ok((top as f32, bottom as f32))
        }
    })
    .unwrap_or((0.0, 0.0))
}
