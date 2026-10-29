//! Antiphon on iOS: the shared phone app in `antiphon-mobile`, started
//! from a plain `main` with no Swift, Objective-C or Xcode project.
//!
//! winit's iOS event loop calls `UIApplicationMain` itself, so this binary
//! is the whole app; `build-app.sh` wraps it in a bundle with
//! `Info.plist`. Everything above the window is shared with Android. What
//! is iOS's own lives in the modules below, each a thin layer over one
//! Apple framework through the `objc2` bindings: the audio session,
//! CallKit and PushKit, the document picker, the network monitor.
//!
//! Everything is `cfg(target_os = "ios")`: on a desktop this binary is an
//! empty `main`, so `cargo build --workspace` is unaffected.

#![warn(missing_docs, clippy::missing_docs_in_private_items)]

#[cfg(target_os = "ios")]
mod audio;
#[cfg(target_os = "ios")]
mod background;
#[cfg(target_os = "ios")]
mod callkit;
#[cfg(target_os = "ios")]
mod camera;
#[cfg(target_os = "ios")]
mod files;
#[cfg(target_os = "ios")]
mod logger;
#[cfg(target_os = "ios")]
mod network;
#[cfg(target_os = "ios")]
mod platform;
#[cfg(target_os = "ios")]
mod screen;
#[cfg(target_os = "ios")]
mod shell;

/// Hands the main thread to the app. It must be the process's first
/// thread: UIKit, and so winit's event loop, run only there.
#[cfg(target_os = "ios")]
fn main() {
    shell::run();
}

/// The desktop stand-in, so the workspace builds everywhere.
#[cfg(not(target_os = "ios"))]
fn main() {}
