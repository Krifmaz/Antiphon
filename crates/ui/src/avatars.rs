//! People's pictures and chosen colours, for every avatar the screens draw.
//!
//! Kept beside the model rather than in it: an avatar is drawn in a dozen
//! places (the list, the call, the details panel, a message), each knowing
//! only the person's name and id. The app registers a picture and colour
//! per id as it builds the model; [`crate::widgets::avatar`] looks them up.
//! A picture is decoded and handed to the GPU once, when it changes, not
//! on every frame.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use iced_core::image::Handle;
use iced_core::Color;

/// The avatar colours a person can pick, as `(id, name, colour)`. The id
/// is what is saved and sent; an empty id means the automatic colour
/// picked from their key.
pub const PROFILE_COLORS: [(&str, &str, &str); 8] = [
    ("rose", "Rose", "#f2a5a0"),
    ("coral", "Coral", "#f3a37e"),
    ("amber", "Amber", "#f3c97a"),
    ("sage", "Sage", "#a7d3a1"),
    ("teal", "Teal", "#8fd3c8"),
    ("sky", "Sky", "#9cc6f0"),
    ("lilac", "Lilac", "#c8b2f0"),
    ("sand", "Sand", "#e6d3b3"),
];

/// One person's registered look.
#[derive(Clone, Default)]
struct Look {
    /// What the picture was made from, to tell when it changed.
    source: String,
    /// The picture, ready to draw.
    picture: Option<Handle>,
    /// Their chosen colour, if they chose one.
    color: Option<Color>,
}

/// Everyone's registered look, by id.
fn looks() -> &'static Mutex<HashMap<String, Look>> {
    static LOOKS: OnceLock<Mutex<HashMap<String, Look>>> = OnceLock::new();
    LOOKS.get_or_init(Default::default)
}

/// The colour a profile colour id names, or `None` for an empty or
/// unknown id (the automatic colour then).
pub fn named_color(id: &str) -> Option<Color> {
    PROFILE_COLORS.iter().find(|(i, _, _)| *i == id).map(|(_, _, hex)| crate::theme::hex(hex))
}

/// Registers `id`'s picture and colour. `source` is whatever the picture
/// comes from (the saved base64); `decode` turns it into RGBA pixels as
/// `(width, height, pixels)`, and is only called when `source` changed.
/// An empty `source` means no picture.
pub fn set(id: &str, source: &str, color: &str, decode: impl FnOnce(&str) -> Option<(u32, u32, Vec<u8>)>) {
    let mut looks = looks().lock().unwrap_or_else(|e| e.into_inner());
    let look = looks.entry(id.to_string()).or_default();
    look.color = named_color(color);
    if look.source != source {
        look.source = source.to_string();
        look.picture = if source.is_empty() { None } else { decode(source).map(|(w, h, rgba)| Handle::from_rgba(w, h, rgba)) };
    }
}

/// The id a community's custom emoji is registered under, so its picture
/// sits beside people's without the two ever meeting.
pub fn emoji_id(community: &str, name: &str) -> String {
    format!("emoji/{community}/{name}")
}

/// `id`'s picture, if one is registered.
pub fn picture(id: &str) -> Option<Handle> {
    looks().lock().unwrap_or_else(|e| e.into_inner()).get(id).and_then(|l| l.picture.clone())
}

/// `id`'s chosen colour, if they chose one.
pub fn color(id: &str) -> Option<Color> {
    looks().lock().unwrap_or_else(|e| e.into_inner()).get(id).and_then(|l| l.color)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_picture_is_decoded_once_until_it_changes() {
        let calls = std::cell::Cell::new(0);
        let decode = |_: &str| {
            calls.set(calls.get() + 1);
            Some((1, 1, vec![0, 0, 0, 255]))
        };
        set("t1", "abc", "sage", decode);
        set("t1", "abc", "sage", decode);
        assert_eq!(calls.get(), 1);
        assert!(picture("t1").is_some());
        assert_eq!(color("t1"), named_color("sage"));
        set("t1", "", "", decode);
        assert!(picture("t1").is_none() && color("t1").is_none());
    }
}
