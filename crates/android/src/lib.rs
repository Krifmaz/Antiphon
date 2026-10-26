//! Antiphon on Android: the shared phone app in `antiphon-mobile`, started
//! from the platform's own `android.app.NativeActivity`.
//!
//! `android-activity` hands this crate the activity and `winit` turns it
//! into a window; everything above that is shared with iOS. What is
//! Android's own lives in [`platform`]: the framework calls a
//! NativeActivity cannot make from the NDK alone, made through JNI.
//!
//! Everything is `cfg(target_os = "android")`: on a desktop this crate is
//! empty, so `cargo build --workspace` is unaffected.

#![cfg(target_os = "android")]

mod platform;

use android_activity::AndroidApp;
use antiphon_mobile::runner::{Runner, Wake};
use winit::event_loop::EventLoop;
use winit::platform::android::EventLoopBuilderExtAndroid;

/// Entry point, called by `android-activity` on its own thread once the
/// NativeActivity has been created.
#[no_mangle]
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
            log::error!("could not create the event loop: {e}");
            return;
        }
    };
    let data_dir = app.internal_data_path().unwrap_or_else(std::env::temp_dir);
    let mut runner = Runner::new(data_dir, event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut runner) {
        log::error!("event loop ended with an error: {e}");
    }
}
