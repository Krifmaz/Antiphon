use anyhow::{bail, Result};
use device_query::Keycode;

/// Maps a handful of common key names to a [`Keycode`] for `--ptt-key`.
///
/// Spec §1 defaults push-to-talk to a held Caps Lock. We default to Space
/// instead: the spec's own implementation note flags Caps Lock as a
/// stateful OS-level toggle key that needs a low-level platform hook to
/// capture as "held" rather than "toggled" -- device_query's cross-platform
/// polling reads raw key-down state, which is good enough for Space/Ctrl/
/// etc. but not a reliable stand-in for that hook. Caps Lock is listed here
/// for testing, with that caveat.
pub fn parse_key(name: &str) -> Result<Keycode> {
    use Keycode::*;
    let key = match name.to_lowercase().as_str() {
        "space" => Space,
        "capslock" | "caps_lock" | "caps" => CapsLock,
        "tab" => Tab,
        "ctrl" | "lctrl" | "lcontrol" => LControl,
        "rctrl" | "rcontrol" => RControl,
        "shift" | "lshift" => LShift,
        "rshift" => RShift,
        "alt" | "lalt" => LAlt,
        "ralt" => RAlt,
        "grave" | "backtick" => Grave,
        other => bail!("unknown PTT key '{other}' -- try: space, capslock, tab, ctrl, rctrl, shift, rshift, alt, ralt, grave"),
    };
    Ok(key)
}
