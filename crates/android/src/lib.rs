//! Antiphon on Android, written entirely in Rust.
//!
//! The APK's activity is the platform's own `android.app.NativeActivity`,
//! so there is no Java or Kotlin anywhere: `android-activity` hands this
//! crate the activity, `winit` turns it into a window, `wgpu` draws into
//! it, and iced lays out the interface. The call itself is the same code
//! the desktop binaries run -- `antiphon-crypto` for identity and the
//! handshake, `antiphon-audio` for the mic chain, `antiphon-net` for the
//! encrypted UDP link -- so a phone and a desktop can call each other.
//!
//! iced's own `iced_winit` shell does not build for Android (it calls
//! winit APIs that only exist on desktop), so [`runner`] drives iced's
//! `UserInterface` directly, the way iced's `integration` example does,
//! and [`convert`] translates winit's Android events itself.
//!
//! Everything is `cfg(target_os = "android")`: on a desktop this crate is
//! empty, so `cargo build --workspace` is unaffected.

#![cfg(target_os = "android")]

mod call;
mod convert;
mod platform;
mod runner;
mod settings;
mod ui;

use android_activity::AndroidApp;
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

    let event_loop = match EventLoop::<runner::Wake>::with_user_event().with_android_app(app.clone()).build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            log::error!("could not create the event loop: {e}");
            return;
        }
    };
    let mut runner = runner::Runner::new(app, event_loop.create_proxy());
    if let Err(e) = event_loop.run_app(&mut runner) {
        log::error!("event loop ended with an error: {e}");
    }
}
