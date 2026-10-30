//! Your profile picture and the rest of the profile card: turning a
//! picked image into the small picture sent to contacts, and back.
//!
//! The picture rides in one control message (`ControlMessage::Profile`),
//! so it has to fit a datagram: a square cropped from the middle of the
//! image, shrunk to [`AVATAR_SIDE`] pixels and saved as JPEG at the best
//! quality that stays under [`MAX_AVATAR_BYTES`]. Small enough to send on
//! every call and to keep in the config file (and so in backups) with
//! everything else, large enough to look sharp in a list or a call.
//!
//! Shared by the desktop and the phone apps.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};

/// The picture's side, in pixels: the largest the app draws an avatar
/// (the call stage's), so it is never blown up.
pub const AVATAR_SIDE: u32 = 84;

/// The most bytes the JPEG may take. Base64 adds a third (2934 bytes);
/// with a full About line (at most 640 bytes of UTF-8), the JSON and the
/// session's sealing, the message stays under the 4096 bytes a peer reads
/// per datagram.
pub const MAX_AVATAR_BYTES: usize = 2200;

/// The longest About line, in characters.
pub const MAX_BIO: usize = 160;

/// The longest pronouns may be, in characters: room for "they/she/xe",
/// not for a second About line.
pub const MAX_PRONOUNS: usize = 40;

/// Makes the picture to send from an image file's bytes (PNG, JPEG, GIF,
/// WebP...): the largest centred square, shrunk and compressed. Returns
/// it as base64, ready for the profile card and the config file.
pub fn avatar_from_image(bytes: &[u8]) -> Result<String, String> {
    let img = image::load_from_memory(bytes).map_err(|_| "That file isn't a picture Stoop can read".to_string())?;
    let square = crop_square(&img);
    // The best quality that fits: most photos fit near the top, busy
    // ones step down a little, and a very busy one is drawn a little
    // smaller rather than refused.
    for side in [AVATAR_SIDE, 64, 48] {
        let small = square.resize_exact(side, side, FilterType::Lanczos3).to_rgb8();
        for quality in (30..=88).rev().step_by(6) {
            let mut out = Vec::new();
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), quality);
            small.write_with_encoder(encoder).map_err(|e| e.to_string())?;
            if out.len() <= MAX_AVATAR_BYTES {
                return Ok(crate::file_transfer::encode_b64(&out));
            }
        }
    }
    Err("That picture is too detailed to send; try a simpler one".into())
}

/// The most bytes a custom emoji's picture may take: its base64 must fit
/// `antiphon_crypto::community::MAX_EMOJI_IMAGE`.
pub const MAX_EMOJI_BYTES: usize = antiphon_crypto::community::MAX_EMOJI_IMAGE / 4 * 3;

/// Makes a community's custom emoji from an image file's bytes: the
/// middle square at 32 pixels, as PNG so a transparent background stays
/// transparent, smaller (or as JPEG) if that doesn't fit. Base64.
pub fn emoji_from_image(bytes: &[u8]) -> Result<String, String> {
    let img = image::load_from_memory(bytes).map_err(|_| "That file isn't a picture Stoop can read".to_string())?;
    let square = crop_square(&img);
    for side in [32, 24] {
        let small = square.resize_exact(side, side, FilterType::Lanczos3);
        let mut out = Vec::new();
        small.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
        if out.len() <= MAX_EMOJI_BYTES {
            return Ok(crate::file_transfer::encode_b64(&out));
        }
    }
    // A busy photo: JPEG, on the dark ground most themes draw messages on.
    let small = square.resize_exact(32, 32, FilterType::Lanczos3).to_rgb8();
    for quality in (40..=85).rev().step_by(15) {
        let mut out = Vec::new();
        small.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(Cursor::new(&mut out), quality)).map_err(|e| e.to_string())?;
        if out.len() <= MAX_EMOJI_BYTES {
            return Ok(crate::file_transfer::encode_b64(&out));
        }
    }
    Err("That picture is too detailed for an emoji; try a simpler one".into())
}

/// A custom emoji's picture as RGBA pixels, or `None` if it isn't a small
/// PNG or JPEG. Anything bigger than an emoji could be is refused unread.
pub fn decode_emoji(b64: &str) -> Option<(u32, u32, Vec<u8>)> {
    if b64.is_empty() || b64.len() > antiphon_crypto::community::MAX_EMOJI_IMAGE {
        return None;
    }
    let bytes = crate::file_transfer::decode_b64(b64)?;
    let format = image::guess_format(&bytes).ok().filter(|f| matches!(f, image::ImageFormat::Png | image::ImageFormat::Jpeg))?;
    let img = image::load_from_memory_with_format(&bytes, format).ok()?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || w > 64 || h > 64 {
        return None;
    }
    Some((w, h, img.to_rgba8().into_raw()))
}

/// The largest square from the middle of `img`: a portrait loses its top
/// and bottom, a landscape its sides, so a face in the centre stays.
fn crop_square(img: &DynamicImage) -> DynamicImage {
    let (w, h) = img.dimensions();
    let side = w.min(h);
    img.crop_imm((w - side) / 2, (h - side) / 2, side, side)
}

/// A received or saved picture as RGBA pixels, `(width, height, pixels)`,
/// or `None` if it is missing, too large to be one of ours, or not a
/// picture. Anything bigger than a picture could be is refused unread.
pub fn decode_avatar(b64: &str) -> Option<(u32, u32, Vec<u8>)> {
    if b64.is_empty() || b64.len() > MAX_AVATAR_BYTES * 2 {
        return None;
    }
    let bytes = crate::file_transfer::decode_b64(b64)?;
    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?;
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 || w > 256 || h > 256 {
        return None;
    }
    Some((w, h, img.to_rgba8().into_raw()))
}

/// An About line cut to [`MAX_BIO`] characters and one line, as typed
/// text may carry newlines that would break a list row. Other control
/// characters go too: they draw as nothing, and each would cost six bytes
/// escaped in the card's JSON.
pub fn clean_bio(bio: &str) -> String {
    bio.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(MAX_BIO).collect()
}

/// Pronouns as they are kept and sent: one line, spaces collapsed, no
/// control characters, at most [`MAX_PRONOUNS`] characters.
pub fn clean_pronouns(pronouns: &str) -> String {
    pronouns.split_whitespace().collect::<Vec<_>>().join(" ").chars().filter(|c| !c.is_control()).take(MAX_PRONOUNS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pronouns_are_one_short_line() {
        assert_eq!(clean_pronouns("  she /\n her "), "she / her");
        assert_eq!(clean_pronouns(&"x".repeat(100)).chars().count(), MAX_PRONOUNS);
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, 120]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn a_photo_becomes_a_small_square_that_reads_back() {
        let b64 = avatar_from_image(&png(1200, 800)).unwrap();
        let (w, h, rgba) = decode_avatar(&b64).unwrap();
        assert_eq!((w, h), (AVATAR_SIDE, AVATAR_SIDE));
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(crate::file_transfer::decode_b64(&b64).unwrap().len() <= MAX_AVATAR_BYTES);
    }

    #[test]
    fn noise_still_fits_or_says_why() {
        let mut seed = 1u32;
        let img = image::RgbImage::from_fn(400, 400, |_, _| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
        });
        let mut bytes = Vec::new();
        img.write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png).unwrap();
        match avatar_from_image(&bytes) {
            Ok(b64) => assert!(crate::file_transfer::decode_b64(&b64).unwrap().len() <= MAX_AVATAR_BYTES),
            Err(why) => assert!(why.contains("detailed")),
        }
    }

    #[test]
    fn junk_is_refused() {
        assert!(avatar_from_image(b"not a picture").is_err());
        assert!(decode_avatar("").is_none());
        assert!(decode_avatar("!!!!").is_none());
        assert!(decode_avatar(&"A".repeat(MAX_AVATAR_BYTES * 3)).is_none());
    }

    #[test]
    fn the_about_line_is_one_short_line() {
        assert_eq!(clean_bio("  Plays bass.\n\nUp late.  "), "Plays bass. Up late.");
        assert_eq!(clean_bio(&"x".repeat(500)).chars().count(), MAX_BIO);
        assert_eq!(clean_bio("a\u{1}b"), "ab");
    }

    #[test]
    fn an_emoji_is_small_and_reads_back() {
        let b64 = emoji_from_image(&png(300, 200)).unwrap();
        assert!(b64.len() <= antiphon_crypto::community::MAX_EMOJI_IMAGE);
        let (w, h, _) = decode_emoji(&b64).unwrap();
        assert!(w <= 32 && w == h);
        assert!(decode_emoji(&avatar_from_image(&png(300, 300)).unwrap()).is_none(), "an avatar is too big to pass as an emoji");
    }

    /// The largest card there can be -- the biggest picture, an About line
    /// of four-byte characters, the longest colour id -- still fits one
    /// datagram once sealed (16-byte tag) and framed (14-byte header).
    #[test]
    fn the_largest_card_fits_a_datagram() {
        let card = crate::control::ProfileCard {
            bio: clean_bio(&"\u{1F3B8}".repeat(400)),
            color: "lilac".into(),
            avatar: crate::file_transfer::encode_b64(&vec![0xAB; MAX_AVATAR_BYTES]),
            pronouns: clean_pronouns(&"\u{1F3B8}".repeat(100)),
        };
        let bytes = crate::control::ControlMessage::Profile(card).encode();
        assert!(bytes.len() + 16 + 14 <= 4096, "{} bytes", bytes.len());
    }
}
