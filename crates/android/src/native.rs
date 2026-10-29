//! The Java side's way back in: the `native` methods of
//! `org.antiphon.app.Native`, called from the activity, the foreground
//! service and the push receiver. Each turns what it is given into a
//! [`PlatformEvent`] and posts it; the shared app picks it up on its own
//! thread.
//!
//! A panic must not unwind into the JVM -- it would abort the process,
//! call and all -- so every entry point catches one and logs it.
//!
//! Threading: the JVM calls these on whichever Java thread the Java side
//! was on -- the UI (main) thread for activity results, notification
//! buttons and pushes, ConnectivityManager's own callback thread for network
//! changes. None of them touches the UI or blocks: [`post`] only queues,
//! so each returns at once and is safe on any thread. The `JNIEnv` each
//! receives is valid only on that thread and only for that call.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use antiphon_mobile::platform::{post, CallAction, Network, PickedFile, PlatformEvent};
use jni::objects::{JByteArray, JClass, JString};
use jni::sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE};
use jni::JNIEnv;

use crate::platform;

/// Whether the app is running to take what is posted. A push or a
/// notification button can start the process without the activity, and
/// then nothing would read the queue until the user opens the app.
pub(crate) static APP_RUNNING: AtomicBool = AtomicBool::new(false);

/// `Native.ANSWER` in Native.java: the Answer button on the incoming-call
/// alert. The four action codes are shared by hand, so a change on either
/// side must be made on both.
const ANSWER: jint = 0;
/// `Native.DECLINE`: the Decline button on the incoming-call alert.
const DECLINE: jint = 1;
/// `Native.HANG_UP`: the Hang up button on the ongoing-call notification.
const HANG_UP: jint = 2;
/// `Native.MUTE`: the Mute/Unmute button on the ongoing-call notification,
/// with the new state in the `on` argument.
const MUTE: jint = 3;

/// Runs `f`, catching a panic so that it cannot unwind across the
/// `extern "system"` boundary into the JVM. On a panic it logs `what` and
/// returns `fallback`, which is what Java then receives.
fn guarded<T>(what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => {
            log::error!("{what}: panicked; ignored");
            fallback
        }
    }
}

/// Copies a Java string into Rust. `None` for a Java `null`, and for a
/// string JNI cannot read, which is logged; a pending exception from that
/// is left for the JVM to clear when the native method returns.
fn string(env: &mut JNIEnv, value: &JString) -> Option<String> {
    if value.is_null() {
        return None;
    }
    match env.get_string(value) {
        Ok(s) => Some(s.into()),
        Err(e) => {
            log::warn!("reading a string from Java: {e}");
            None
        }
    }
}

/// `Native.filePicked`: the user picked a document. Called on the UI
/// thread from `AntiphonActivity.onActivityResult`. `uri` is the content
/// URI, kept as the file's handle for [`platform`]'s `read_file`; `size` is
/// in bytes, or -1 when the provider does not say. A null `uri` is treated
/// as a cancelled pick.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_filePicked(
    mut env: JNIEnv,
    _class: JClass,
    uri: JString,
    name: JString,
    size: jlong,
) {
    guarded("a picked file", (), || {
        let Some(handle) = string(&mut env, &uri) else {
            post(PlatformEvent::FilePickCancelled);
            return;
        };
        let name = string(&mut env, &name).unwrap_or_else(|| "file".into());
        // -1 from Java when the provider does not know the size.
        let size = u64::try_from(size).ok();
        post(PlatformEvent::FilePicked(PickedFile { handle, name, size }));
    });
}

/// `Native.filePickCancelled`: the picker was dismissed, or could not be
/// opened at all. Called on the UI thread. The shared app is waiting for an
/// answer either way, so a cancel is always posted.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_filePickCancelled(_env: JNIEnv, _class: JClass) {
    guarded("a cancelled file pick", (), || post(PlatformEvent::FilePickCancelled));
}

/// `Native.callAction`: a call button pressed outside the app, on a
/// notification or the full-screen incoming-call alert. Called on the main
/// thread from `CallService.onStartCommand` or `AntiphonActivity`. `action`
/// is one of [`ANSWER`], [`DECLINE`], [`HANG_UP`] or [`MUTE`]; `on` is the
/// new mute state and is ignored for the others. An unknown code is logged
/// and dropped rather than guessed at.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_callAction(_env: JNIEnv, _class: JClass, action: jint, on: jboolean) {
    guarded("a call button", (), || {
        let action = match action {
            ANSWER => CallAction::Answer,
            DECLINE => CallAction::Decline,
            HANG_UP => CallAction::HangUp,
            MUTE => CallAction::Mute(on != JNI_FALSE),
            other => {
                log::warn!("unknown call action {other} from Java");
                return;
            }
        };
        post(PlatformEvent::CallAction(action));
    });
}

/// `Native.networkChanged`: the default network changed. Called on a
/// ConnectivityManager callback thread, not the UI thread, so the lookup
/// in [`platform::network`] attaches to the JVM itself. Nothing is posted
/// while the app is not running: there is no one to tell, and the app
/// reads the network afresh when it starts.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_networkChanged(_env: JNIEnv, _class: JClass) {
    // The callback fires for every change in the network's capabilities
    // -- signal strength among them -- so only a change of kind is passed
    // on.
    /// The kind of network last posted, to drop repeats. A mutex because
    /// nothing in the API promises every callback arrives on one thread.
    static LAST: Mutex<Option<Network>> = Mutex::new(None);
    guarded("a network change", (), || {
        if !APP_RUNNING.load(Ordering::Acquire) {
            return;
        }
        let now = platform::network();
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if *last != Some(now) {
            *last = Some(now);
            post(PlatformEvent::NetworkChanged(now));
        }
    });
}

/// `Native.pushRegistered`: the UnifiedPush distributor gave this app an
/// endpoint URL, which the app hands to its contacts so they can wake it.
/// Called on the main thread from `PushReceiver.onReceive`, every time the
/// app registers, even when the endpoint has not changed.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_pushRegistered(mut env: JNIEnv, _class: JClass, endpoint: JString) {
    guarded("a push endpoint", (), || {
        if let Some(endpoint) = string(&mut env, &endpoint) {
            post(PlatformEvent::PushRegistered(endpoint));
        }
    });
}

/// `Native.push`: a sealed wake-up arrived through UnifiedPush. Called on
/// the main thread from `PushReceiver.onReceive`, which Android gives about
/// ten seconds before it may kill the process, so this only copies the
/// bytes and queues them; the shared app opens the seal on its own thread.
/// Returns whether the app is running to act on it. When it is not (the
/// push started the process), Java shows a plain notification instead, and
/// the wake-up waits in the queue for the app.
#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_push(env: JNIEnv, _class: JClass, sealed: JByteArray) -> jboolean {
    guarded("a push", JNI_FALSE, || {
        match env.convert_byte_array(&sealed) {
            Ok(bytes) => post(PlatformEvent::Push(bytes)),
            Err(e) => {
                log::warn!("reading a push from Java: {e}");
                return JNI_FALSE;
            }
        }
        if APP_RUNNING.load(Ordering::Acquire) {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    })
}
