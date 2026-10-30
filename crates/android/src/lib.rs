//! Antiphon on Android: the shared phone app in `antiphon-mobile`, started
//! from `AntiphonActivity`, a few lines of Java around the platform's own
//! `android.app.NativeActivity`.
//!
//! `android-activity` hands this crate the activity and `winit` turns it
//! into a window; everything above that is shared with iOS. What is
//! Android's own lives in [`platform`]: the framework calls a
//! NativeActivity cannot make from the NDK alone, made through JNI. The
//! little Java there is (`java/org/antiphon/app`) exists because a
//! service, a broadcast receiver and an activity result can only be
//! received by a Java class; each hands what it gets to [`native`].
//!
//! Audio needs nothing here: cpal's Android backend is AAudio, through the
//! `ndk` crate, with no C++ runtime to ship. The camera, for video calls,
//! is the NDK's own camera API too ([`camera`]), so it needs no Java.
//!
//! Everything is `cfg(target_os = "android")`: on a desktop this crate is
//! empty, so `cargo build --workspace` is unaffected.

#![cfg(target_os = "android")]
#![warn(missing_docs, clippy::missing_docs_in_private_items)]

mod camera;
mod native;
mod platform;

use std::sync::atomic::Ordering;

use android_activity::AndroidApp;
use antiphon_mobile::runner::{Runner, Wake};
use winit::event_loop::EventLoop;
use winit::platform::android::EventLoopBuilderExtAndroid;

/// Entry point, called by `android-activity` on its own thread once the
/// NativeActivity has been created.
#[no_mangle]
#[expect(clippy::needless_pass_by_value, reason = "android-activity calls this entry point with the app by value")]
fn android_main(app: AndroidApp) {
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("antiphon"),
    );
    // A panic on this thread would otherwise vanish: there is no stderr
    // on Android, only logcat.
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    antiphon_mobile::platform::install(platform::Android);

    let event_loop = match EventLoop::<Wake>::with_user_event().with_android_app(app.clone()).build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            // winit allows one event loop per process. AntiphonActivity
            // restarts the process before a second activity gets here, so
            // this is a backstop: ending the process means the next launch
            // works, where returning would leave every later launch
            // closing at once until the app was force-stopped.
            log::error!("could not create the event loop: {e}; ending the process");
            std::process::exit(0);
        }
    };
    let data_dir = app.internal_data_path().unwrap_or_else(std::env::temp_dir);
    let mut runner = Runner::new(data_dir, event_loop.create_proxy());
    native::APP_RUNNING.store(true, Ordering::Release);
    if let Err(e) = event_loop.run_app(&mut runner) {
        log::error!("event loop ended with an error: {e}");
    }
    native::APP_RUNNING.store(false, Ordering::Release);
}
