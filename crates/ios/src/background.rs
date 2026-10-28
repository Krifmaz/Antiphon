//! Work that has to go on while the app is not in front.
//!
//! iOS has nothing like Android's foreground service. A call keeps the app
//! running because it holds an active audio session under the `audio`
//! background mode, and CallKit gives it the system's call screen. Anything
//! else -- a file transfer -- gets only the grace period
//! `beginBackgroundTask` grants (about thirty seconds) before the process
//! is suspended. Transfers therefore resume on open rather than finishing
//! in the background: the shared code restarts them when the app comes
//! back, from wherever the receiving side says it got to.
//!
//! Listening for calls in the background is not possible at all on iOS;
//! that is what VoIP pushes are for (see `callkit`).

use std::cell::{Cell, RefCell};

use antiphon_mobile::platform::BackgroundWork;
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_foundation::NSString;
use objc2_ui_kit::{UIApplication, UIBackgroundTaskIdentifier, UIBackgroundTaskInvalid};

use crate::callkit;

thread_local! {
    /// The grace period held for transfers, if any.
    static TASK: Cell<Option<UIBackgroundTaskIdentifier>> = const { Cell::new(None) };
    /// What was last asked for, so only changes reach CallKit.
    static LAST: RefCell<BackgroundWork> = RefCell::new(BackgroundWork::default());
}

pub fn set(mtm: MainThreadMarker, work: &BackgroundWork) {
    let previous = LAST.with(|last| std::mem::replace(&mut *last.borrow_mut(), work.clone()));
    if previous.call != work.call {
        callkit::set_call(mtm, work.call.as_deref());
    }
    if work.transfers > 0 {
        begin_grace(mtm);
    } else {
        end_grace(mtm);
    }
}

/// Asks iOS for time to keep transferring if the user leaves the app.
/// Harmless in the foreground: the clock only runs once in the background.
fn begin_grace(mtm: MainThreadMarker) {
    if TASK.get().is_some() {
        return;
    }
    // When the time is up, the task must be ended at once, or iOS kills
    // the app instead of suspending it.
    let expired = RcBlock::new(move || {
        log::info!("background time for transfers ran out; they resume when the app is opened");
        if let Some(mtm) = MainThreadMarker::new() {
            end_grace(mtm);
        }
    });
    let app = UIApplication::sharedApplication(mtm);
    let task =
        app.beginBackgroundTaskWithName_expirationHandler(Some(&NSString::from_str("Antiphon file transfer")), Some(&expired));
    // SAFETY: reading a constant from UIKit.
    if task != unsafe { UIBackgroundTaskInvalid } {
        TASK.set(Some(task));
    } else {
        log::info!("iOS gave no background time for transfers");
    }
}

fn end_grace(mtm: MainThreadMarker) {
    if let Some(task) = TASK.take() {
        UIApplication::sharedApplication(mtm).endBackgroundTask(task);
    }
}
