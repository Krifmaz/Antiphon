//! iOS's half of [`Platform`]: each method hands off to the module for its
//! framework.
//!
//! The shared code calls these from whichever thread it is on -- the event
//! loop's, which is the main thread, or the call engine's. UIKit, CallKit
//! and PushKit state is main-thread only, so those calls hop to the main
//! queue: [`on_main`] for the fire-and-forget ones, `run_on_main` for the
//! two that answer. The rest (audio session, files, notifications,
//! process info) are thread-safe and run where they are called.

use std::path::PathBuf;

use antiphon_mobile::platform::{BackgroundWork, Network, Platform};
use block2::RcBlock;
use dispatch2::{run_on_main, DispatchQueue};
use objc2::runtime::Bool;
use objc2::MainThreadMarker;
use objc2_foundation::{NSError, NSProcessInfo, NSString, NSUUID};
use objc2_ui_kit::UIPasteboard;
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNNotificationSound,
    UNUserNotificationCenter,
};

use crate::{audio, background, callkit, files, network, screen};

/// Runs `f` on the main thread: now if this is it, later otherwise.
fn on_main(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    match MainThreadMarker::new() {
        Some(mtm) => f(mtm),
        None => DispatchQueue::main().exec_async(move || {
            // SAFETY: the main queue only ever runs on the main thread.
            f(unsafe { MainThreadMarker::new_unchecked() })
        }),
    }
}

/// Asks to show notifications. iOS prompts once and remembers the answer.
fn request_notifications() {
    let done = RcBlock::new(|granted: Bool, error: *mut NSError| {
        // SAFETY: nil or a live error.
        match unsafe { error.as_ref() } {
            Some(error) => log::warn!("notification permission: {error}"),
            None => log::info!("notification permission: {}", granted.as_bool()),
        }
    });
    UNUserNotificationCenter::currentNotificationCenter().requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound | UNAuthorizationOptions::Badge,
        &done,
    );
}

/// iOS's half of [`Platform`], installed by `shell::run`. It holds no
/// state, so it is `Send` and `Sync` as the trait needs; what state there
/// is lives main-thread-only in the framework modules.
pub struct Ios;

impl Platform for Ios {
    fn has_mic_permission(&self) -> bool {
        audio::has_mic_permission()
    }

    fn request_mic_permission(&self) {
        audio::request_mic_permission();
    }

    fn set_call_audio(&self, in_call: bool, speaker: bool) {
        audio::set_call_audio(in_call, speaker);
    }

    fn copy_to_clipboard(&self, text: &str) {
        let text = text.to_owned();
        on_main(move |_| {
            // SAFETY: a setter with a valid string.
            unsafe { UIPasteboard::generalPasteboard().setString(Some(&NSString::from_str(&text))) };
        });
    }

    /// iOS 16 and later show a "pasted from" banner, or ask, the first
    /// time; that is the system's doing and only happens on a paste the
    /// user pressed.
    fn read_clipboard(&self) -> Option<String> {
        // SAFETY: a property read.
        run_on_main(|_| unsafe { UIPasteboard::generalPasteboard().string() }.map(|s| s.to_string()))
    }

    fn system_insets(&self) -> (f32, f32) {
        run_on_main(screen::insets)
    }

    fn network(&self) -> Network {
        network::current()
    }

    fn battery_saver(&self) -> bool {
        NSProcessInfo::processInfo().isLowPowerModeEnabled()
    }

    fn set_background_work(&self, work: &BackgroundWork) {
        let work = work.clone();
        on_main(move |mtm| background::set(mtm, &work));
    }

    fn pick_file(&self) {
        on_main(files::pick);
    }

    fn read_file(&self, handle: &str) -> std::io::Result<Vec<u8>> {
        files::read(handle)
    }

    /// The app's Documents folder. With `UIFileSharingEnabled` and
    /// `LSSupportsOpeningDocumentsInPlace` in Info.plist, the Files app
    /// shows it under "On My iPhone > Antiphon", which is as close as iOS
    /// comes to a shared Downloads folder.
    fn downloads_dir(&self) -> Option<PathBuf> {
        let dir = PathBuf::from(objc2_foundation::NSHomeDirectory().to_string()).join("Documents");
        match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(dir),
            Err(e) => {
                log::warn!("could not create {}: {e}", dir.display());
                None
            }
        }
    }

    fn notify(&self, title: &str, body: &str) {
        request_notifications();
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        content.setSound(Some(&UNNotificationSound::defaultSound()));
        // A fresh identifier each time: reusing one replaces the earlier
        // notification instead of adding another.
        let id = NSUUID::new().UUIDString();
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
        let done = RcBlock::new(|error: *mut NSError| {
            // SAFETY: nil or a live error.
            if let Some(error) = unsafe { error.as_ref() } {
                log::warn!("could not show a notification: {error}");
            }
        });
        UNUserNotificationCenter::currentNotificationCenter().addNotificationRequest_withCompletionHandler(&request, Some(&done));
    }

    /// VoIP pushes through PushKit, for calls. Notification permission is
    /// asked for here too, since this is when the user chose to be
    /// reachable while the app is closed.
    fn register_push(&self) {
        request_notifications();
        on_main(callkit::register_push);
    }

    fn call_connected(&self) {
        on_main(callkit::call_connected);
    }
}
