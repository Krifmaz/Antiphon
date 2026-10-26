//! Caps Lock as a held push-to-talk key (spec §1: "Push-to-talk default
//! key: Caps Lock, held -- not a toggle").
//!
//! The key itself is not the problem. Both polling backends report the
//! *physical* state of Caps Lock -- `GetAsyncKeyState`'s high bit on
//! Windows, the `XQueryKeymap` bitmap on X11 -- so "is it held right now"
//! is answerable like any other key. The problem is the side effect: every
//! press also flips the lock, so a minute of talking leaves the user
//! TYPING IN CAPITALS with no idea why.
//!
//! Each platform fixes that differently:
//!
//! - **Windows**: a `WH_KEYBOARD_LL` hook swallows Caps Lock while it is
//!   bound to push-to-talk, so the lock never flips at all. A swallowed key
//!   never reaches `GetAsyncKeyState` either, so the hook records held /
//!   released itself and [`held`] answers from that.
//! - **X11**: no hook; the key goes through and the lock flips. On release
//!   the lock modifier is cleared again with `XkbLockModifiers`, which sets
//!   the state directly rather than faking a second key press.
//! - **macOS and Wayland**: neither is reachable from here (HID access and
//!   a compositor portal respectively), so [`supported`] says no and the
//!   bind flow refuses the key with the reason, as before.
//!
//! Swallowing happens only while Caps Lock is the push-to-talk key *and*
//! the transmit mode is push-to-talk. Bound to anything else, Caps Lock is
//! left exactly as the user's system has it -- including when they have
//! remapped it to Control or Escape, which the hook never sees as
//! `VK_CAPITAL` in the first place.

use std::sync::atomic::{AtomicBool, Ordering};

/// Caps Lock is the push-to-talk key and push-to-talk is the mode.
static BOUND: AtomicBool = AtomicBool::new(false);
/// Held, as seen by the hook. Only meaningful where [`hooked`] is true.
static HELD: AtomicBool = AtomicBool::new(false);
/// The hook installed and its message loop is running.
static HOOKED: AtomicBool = AtomicBool::new(false);

/// Whether Caps Lock can work as a held key on this platform.
pub fn supported() -> bool {
    cfg!(any(windows, all(unix, not(target_os = "macos"))))
}

/// The key a fresh install starts with: Caps Lock where it works, Space
/// where it cannot.
pub fn default_ptt_key() -> &'static str {
    if supported() {
        "CapsLock"
    } else {
        "Space"
    }
}

/// Tell the hook whether Caps Lock is currently push-to-talk. Installs the
/// hook the first time it is needed.
pub fn set_bound(bound: bool) {
    BOUND.store(bound, Ordering::Relaxed);
    if !bound {
        HELD.store(false, Ordering::Relaxed);
    }
    #[cfg(windows)]
    if bound {
        windows_hook::ensure_installed();
    }
}

pub fn is_bound() -> bool {
    BOUND.load(Ordering::Relaxed)
}

/// Whether the hook is supplying Caps Lock's state, in which case the
/// polling backend cannot see it and [`held`] must be asked instead.
pub fn hooked() -> bool {
    HOOKED.load(Ordering::Relaxed) && is_bound()
}

/// Caps Lock is physically down, per the hook.
pub fn held() -> bool {
    HELD.load(Ordering::Relaxed)
}

/// Called on the release edge of Caps Lock-as-push-to-talk. Puts the lock
/// back off where the key press was not swallowed.
pub fn after_release() {
    if hooked() {
        return; // never flipped
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    x11_unlock::clear_caps_lock();
}

#[cfg(windows)]
mod windows_hook {
    use super::{BOUND, HELD, HOOKED};
    use std::sync::atomic::Ordering;
    use std::sync::Once;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, HC_ACTION,
        KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN,
        WM_SYSKEYUP,
    };

    const VK_CAPITAL: u32 = 0x14;

    static INSTALL: Once = Once::new();

    pub fn ensure_installed() {
        INSTALL.call_once(|| {
            // A low-level hook is called on the thread that installed it,
            // through that thread's message loop -- so it gets a thread of
            // its own that does nothing else.
            std::thread::spawn(|| unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0);
                if hook.is_null() {
                    return; // HOOKED stays false; polling carries on as before
                }
                HOOKED.store(true, Ordering::Relaxed);
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                HOOKED.store(false, Ordering::Relaxed);
            });
        });
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION as i32 && BOUND.load(Ordering::Relaxed) {
            let info = &*(lparam as *const KBDLLHOOKSTRUCT);
            // Injected presses are left alone: something else synthesised
            // them on purpose, an on-screen keyboard for instance.
            if info.vkCode == VK_CAPITAL && info.flags & LLKHF_INJECTED == 0 {
                match wparam as u32 {
                    WM_KEYDOWN | WM_SYSKEYDOWN => HELD.store(true, Ordering::Relaxed),
                    WM_KEYUP | WM_SYSKEYUP => HELD.store(false, Ordering::Relaxed),
                    _ => {}
                }
                return 1; // swallowed: the lock never flips
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod x11_unlock {
    use x11::xlib;

    // From XKB.h; the x11 crate exposes the call but not these constants.
    const XKB_USE_CORE_KBD: u32 = 0x0100;

    pub fn clear_caps_lock() {
        unsafe {
            let display = xlib::XOpenDisplay(std::ptr::null());
            if display.is_null() {
                return; // Wayland without XWayland, or no display at all
            }
            xlib::XkbLockModifiers(display, XKB_USE_CORE_KBD, xlib::LockMask, 0);
            xlib::XFlush(display);
            xlib::XCloseDisplay(display);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_key_matches_support() {
        if supported() {
            assert_eq!(default_ptt_key(), "CapsLock");
        } else {
            assert_eq!(default_ptt_key(), "Space");
        }
    }

    #[test]
    fn unbinding_drops_a_stale_held_state() {
        HELD.store(true, Ordering::Relaxed);
        set_bound(false);
        assert!(!held());
        assert!(!hooked());
    }
}
