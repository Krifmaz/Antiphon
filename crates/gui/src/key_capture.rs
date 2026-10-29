//! Detecting keys that will not work as push-to-talk (spec §1: "detect
//! key-capture failure and prompt for a different key instead of silently
//! not working").
//!
//! The failure the spec names is specific and nasty: you bind a key, the
//! binding appears to save, and then nothing transmits. Nobody suspects
//! the *key* -- they suspect the microphone, the network, the app. So the
//! only acceptable behaviour is to notice at bind time and say so.
//!
//! Two separate problems get conflated here, and they need different
//! answers:
//!
//! - **Keys the OS never delivers to a background app.** These are
//!   knowable in advance from the key itself, so they are refused with an
//!   explanation rather than tried and silently dropped.
//! - **Keys the polling backend cannot observe.** Not knowable in advance,
//!   so the bind flow watches for the key actually arriving and warns when
//!   it does not.
//!
//! On Caps Lock specifically: the spec wants it as the *default* PTT key,
//! and it is genuinely the best physical choice -- big, unused, reachable.
//! Where `caps_lock.rs` can keep the lock from flipping (Windows, X11) it
//! is accepted and is the default; where it cannot (macOS, Wayland) it is
//! refused with a reason rather than accepted and quietly misbehaving.

/// Why a key cannot be used for push-to-talk.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unusable {
    /// A latching key: the OS reports on/off, not held/released.
    Latching,
    /// A modifier the OS may swallow, or that the app cannot tell apart
    /// from its use in a shortcut.
    Modifier,
    /// Reserved by the system and never delivered to an ordinary app.
    SystemReserved,
}

impl Unusable {
    /// What to tell the user, including what to do instead.
    ///
    /// Every one of these says *why* and offers a way forward: an error
    /// that only refuses teaches people the app is broken.
    pub fn explain(self, key: &str) -> String {
        match self {
            Unusable::Latching => format!(
                "{key} is a lock key -- the system reports it as on or off, not held down, \
                 so push-to-talk would stick on. Pick a key you can hold, like a function \
                 key or the right Control."
            ),
            Unusable::Modifier => format!(
                "{key} is a modifier, so the app can't tell holding it to talk apart from \
                 using it in a shortcut. Pick a key that isn't Shift, Control, Alt or the \
                 Windows key."
            ),
            Unusable::SystemReserved => format!(
                "{key} is reserved by the system and never reaches this app. Pick a \
                 different key."
            ),
        }
    }
}

/// Whether a key can serve as push-to-talk, and why not if it cannot.
///
/// Judged from the key's name, which is what the binding stores. Case is
/// ignored because these strings come from a `Keycode`'s `Display` and
/// from hand-edited config alike.
pub fn check(key: &str) -> Result<(), Unusable> {
    let k = key.trim().to_lowercase();

    if matches!(k.as_str(), "capslock" | "caps lock") && !crate::caps_lock::supported() {
        return Err(Unusable::Latching);
    }
    if matches!(k.as_str(), "numlock" | "num lock" | "scrolllock" | "scroll lock") {
        return Err(Unusable::Latching);
    }

    if matches!(
        k.as_str(),
        "lshift" | "rshift" | "shift" | "lcontrol" | "rcontrol" | "control" | "ctrl"
            | "lalt" | "ralt" | "alt" | "lmeta" | "rmeta" | "meta" | "command" | "lwin" | "rwin"
    ) {
        return Err(Unusable::Modifier);
    }

    if matches!(k.as_str(), "printscreen" | "print screen" | "sysrq" | "pause" | "break") {
        return Err(Unusable::SystemReserved);
    }

    Ok(())
}

/// Whether the app should warn that a bound key never seems to arrive.
///
/// Called after a bind, having watched for the key for a moment. Separate
/// from [`check`] because this failure is not predictable from the key --
/// it depends on the backend, the window manager and what else has the
/// keyboard grabbed.
pub fn warn_if_never_seen(key: &str, was_observed: bool) -> Option<String> {
    if was_observed || key.trim().is_empty() {
        return None;
    }
    Some(format!(
        "{key} was saved, but this app never saw it being pressed. It may be taken by \
         another program or by the system. Try holding it now -- if the indicator doesn't \
         light up, pick a different key."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_keys_are_fine() {
        for key in ["Space", "F13", "RControl2", "Grave", "Z", "Numpad0"] {
            assert!(check(key).is_ok(), "{key} should be usable");
        }
    }

    #[test]
    fn caps_lock_is_accepted_exactly_where_it_can_be_held() {
        // The spec's default key. Accepted where the lock can be kept from
        // flipping, refused with the reason everywhere else.
        for key in ["CapsLock", "  capslock  ", "CAPSLOCK", "Caps Lock"] {
            if crate::caps_lock::supported() {
                assert_eq!(check(key), Ok(()), "{key}");
            } else {
                assert_eq!(check(key), Err(Unusable::Latching), "{key}");
            }
        }

        let message = Unusable::Latching.explain("CapsLock");
        assert!(message.contains("held down"), "{message}");
        assert!(message.contains("Pick a key"), "should suggest a way forward: {message}");
    }

    #[test]
    fn the_other_lock_keys_are_refused_too() {
        for key in ["NumLock", "ScrollLock", "num lock"] {
            assert_eq!(check(key), Err(Unusable::Latching), "{key}");
        }
    }

    #[test]
    fn modifiers_are_refused() {
        for key in ["LShift", "RControl", "LAlt", "LMeta", "Shift"] {
            assert_eq!(check(key), Err(Unusable::Modifier), "{key}");
        }
    }

    #[test]
    fn system_keys_are_refused() {
        for key in ["PrintScreen", "Pause"] {
            assert_eq!(check(key), Err(Unusable::SystemReserved), "{key}");
        }
    }

    #[test]
    fn the_check_ignores_case_and_spacing() {
        // These strings come from a Keycode's Display and from hand-edited
        // config, so neither form can be assumed.
        assert_eq!(check("  numlock  "), Err(Unusable::Latching));
        assert_eq!(check("NUMLOCK"), Err(Unusable::Latching));
        assert_eq!(check("Scroll Lock"), Err(Unusable::Latching));
    }

    #[test]
    fn every_refusal_names_the_key_and_says_what_to_do() {
        // A refusal that does not suggest an alternative teaches people
        // the app is broken rather than that the key is.
        for reason in [Unusable::Latching, Unusable::Modifier, Unusable::SystemReserved] {
            let message = reason.explain("TestKey");
            assert!(message.contains("TestKey"), "{reason:?} does not name the key");
            assert!(
                message.to_lowercase().contains("pick a"),
                "{reason:?} does not offer an alternative: {message}"
            );
        }
    }

    #[test]
    fn a_key_that_was_seen_produces_no_warning() {
        assert!(warn_if_never_seen("F13", true).is_none());
    }

    #[test]
    fn a_key_that_never_arrived_produces_a_warning() {
        // This is the exact failure the spec names: bound, saved, and
        // silently doing nothing.
        let warning = warn_if_never_seen("F13", false).expect("should warn");
        assert!(warning.contains("never saw it"), "{warning}");
        assert!(warning.contains("different key"), "{warning}");
    }

    #[test]
    fn an_empty_binding_is_not_warned_about() {
        // Unbound is a normal state for every shortcut except PTT.
        assert!(warn_if_never_seen("", false).is_none());
        assert!(warn_if_never_seen("   ", false).is_none());
    }
}
