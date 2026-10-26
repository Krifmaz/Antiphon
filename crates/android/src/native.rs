//! The Java side's way back in: the `native` methods of
//! `org.antiphon.app.Native`, called from the activity, the foreground
//! service and the push receiver. Each turns what it is given into a
//! [`PlatformEvent`] and posts it; the shared app picks it up on its own
//! thread.
//!
//! A panic must not unwind into the JVM -- it would abort the process,
//! call and all -- so every entry point catches one and logs it.

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

/// Matches `Native.ANSWER` and the rest in Native.java.
const ANSWER: jint = 0;
const DECLINE: jint = 1;
const HANG_UP: jint = 2;
const MUTE: jint = 3;

fn guarded<T>(what: &str, fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(_) => {
            log::error!("{what}: panicked; ignored");
            fallback
        }
    }
}

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

#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_filePickCancelled(_env: JNIEnv, _class: JClass) {
    guarded("a cancelled file pick", (), || post(PlatformEvent::FilePickCancelled));
}

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

#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_networkChanged(_env: JNIEnv, _class: JClass) {
    // The callback fires for every change in the network's capabilities
    // -- signal strength among them -- so only a change of kind is passed
    // on.
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

#[no_mangle]
pub extern "system" fn Java_org_antiphon_app_Native_pushRegistered(mut env: JNIEnv, _class: JClass, endpoint: JString) {
    guarded("a push endpoint", (), || {
        if let Some(endpoint) = string(&mut env, &endpoint) {
            post(PlatformEvent::PushRegistered(endpoint));
        }
    });
}

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
