//! The window's safe area and the soft keyboard.
//!
//! winit draws edge to edge under the status bar, the notch and the home
//! indicator, and does not report the keyboard at all, so the shared
//! layout would otherwise put its first row under the clock and its text
//! fields under the keys.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSValue};
use objc2_ui_kit::{
    NSValueUIGeometryExtensions, UIApplication, UIKeyboardFrameEndUserInfoKey, UIKeyboardWillChangeFrameNotification,
    UIKeyboardWillHideNotification, UIWindow, UIWindowScene,
};

/// The top edge of the keyboard, in points from the top of the screen,
/// as `f64` bits. Infinity while it is down.
static KEYBOARD_TOP: AtomicU64 = AtomicU64::new(f64::INFINITY.to_bits());

/// The window winit made: the key window of the first window scene. iOS
/// gives every app at least one scene even without a scene manifest.
pub fn key_window(mtm: MainThreadMarker) -> Option<Retained<UIWindow>> {
    let app = UIApplication::sharedApplication(mtm);
    for scene in app.connectedScenes().iter() {
        if let Ok(scene) = scene.downcast::<UIWindowScene>() {
            if let Some(window) = scene.keyWindow().or_else(|| scene.windows().firstObject()) {
                return Some(window);
            }
        }
    }
    None
}

/// Follows the keyboard as it moves. Registered once, at launch, and kept
/// for the life of the process.
pub fn observe_keyboard(_mtm: MainThreadMarker) {
    let changed = RcBlock::new(|notification: NonNull<NSNotification>| {
        // SAFETY: NSNotificationCenter passes a live notification.
        let notification = unsafe { notification.as_ref() };
        let top = notification
            .userInfo()
            .and_then(|info| {
                // SAFETY: a constant key from UIKit.
                let key: &AnyObject = unsafe { UIKeyboardFrameEndUserInfoKey };
                info.objectForKey(key)
            })
            .and_then(|value| value.downcast::<NSValue>().ok())
            // SAFETY: UIKit documents this key's value as a CGRect.
            .map(|value| unsafe { value.CGRectValue() }.origin.y);
        if let Some(top) = top {
            KEYBOARD_TOP.store(top.to_bits(), Ordering::Relaxed);
        }
    });
    let hidden = RcBlock::new(|_: NonNull<NSNotification>| {
        KEYBOARD_TOP.store(f64::INFINITY.to_bits(), Ordering::Relaxed);
    });
    let center = NSNotificationCenter::defaultCenter();
    // SAFETY: constant notification names from UIKit; the blocks are
    // copied by the center. The observers are leaked on purpose: removing
    // them would only matter if the process outlived its window.
    unsafe {
        let observer =
            center.addObserverForName_object_queue_usingBlock(Some(UIKeyboardWillChangeFrameNotification), None, None, &changed);
        std::mem::forget(observer);
        let observer =
            center.addObserverForName_object_queue_usingBlock(Some(UIKeyboardWillHideNotification), None, None, &hidden);
        std::mem::forget(observer);
    }
}

/// (top, bottom) in physical pixels: the safe area, and at the bottom the
/// keyboard instead while it covers more than the home indicator does.
pub fn insets(mtm: MainThreadMarker) -> (f32, f32) {
    let Some(window) = key_window(mtm) else {
        return (0.0, 0.0);
    };
    let safe = window.safeAreaInsets();
    let scale = window.contentScaleFactor();
    let height = window.bounds().size.height;
    let keyboard_top = f64::from_bits(KEYBOARD_TOP.load(Ordering::Relaxed));
    let keyboard = (height - keyboard_top).max(0.0);
    ((safe.top * scale) as f32, (safe.bottom.max(keyboard) * scale) as f32)
}
