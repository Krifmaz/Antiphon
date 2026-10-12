//! QR rendering for invite links (spec §4 "QR code / invite link --
//! primary method: show your code, they scan it, you're connected").
//!
//! Rendered straight to a pixel buffer rather than going through the
//! `image` crate, so this doesn't couple the QR library's version to the
//! one the rest of the GUI uses for avatars and wallpapers.

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};

/// Modules of blank margin around the code.
///
/// The QR standard calls for four, and it is not decorative: scanners use
/// the quiet zone to find the code's edges. A code drawn tight to its
/// container is meaningfully harder to scan, which for this feature means
/// "the primary way to add a contact sometimes just doesn't work".
const QUIET_ZONE: usize = 4;

/// Renders `data` as a QR code image, or `None` if it won't fit in one.
///
/// `target_px` is a hint for the output size; the real size is rounded to
/// a whole number of pixels per module, because a fractional scale makes
/// module edges land mid-pixel and blurs exactly the boundaries a scanner
/// is looking for.
pub fn render(data: &str, target_px: u32) -> Option<Image> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let modules = code.width();
    let colors = code.to_colors();

    let total_modules = modules + QUIET_ZONE * 2;
    let scale = (target_px as usize / total_modules).max(1);
    let side = (total_modules * scale) as u32;

    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(side, side);
    let width = buffer.width() as usize;
    let pixels = buffer.make_mut_slice();

    // Opaque white ground, including the quiet zone. Deliberately not the
    // theme background: a QR code inverted or tinted by a dark theme is a
    // QR code that doesn't scan, and correctness beats matching the
    // palette here.
    for pixel in pixels.iter_mut() {
        *pixel = Rgba8Pixel { r: 255, g: 255, b: 255, a: 255 };
    }

    for (i, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let module_x = i % modules;
        let module_y = i / modules;
        let x0 = (module_x + QUIET_ZONE) * scale;
        let y0 = (module_y + QUIET_ZONE) * scale;

        for y in y0..y0 + scale {
            for x in x0..x0 + scale {
                pixels[y * width + x] = Rgba8Pixel { r: 0, g: 0, b: 0, a: 255 };
            }
        }
    }

    Some(Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts that don't need a rendering backend: sizing arithmetic.
    fn side_for(data: &str, target_px: u32) -> Option<u32> {
        let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
        let total = code.width() + QUIET_ZONE * 2;
        let scale = (target_px as usize / total).max(1);
        Some((total * scale) as u32)
    }

    #[test]
    fn a_typical_invite_encodes() {
        let link = format!("voip://{}@203.0.113.7:6001", "ab".repeat(32));
        assert!(qrcode::QrCode::new(link.as_bytes()).is_ok());
    }

    #[test]
    fn the_side_is_a_whole_number_of_modules() {
        let link = format!("voip://{}@203.0.113.7:6001", "ab".repeat(32));
        let code = qrcode::QrCode::new(link.as_bytes()).unwrap();
        let total = (code.width() + QUIET_ZONE * 2) as u32;

        let side = side_for(&link, 240).unwrap();
        assert_eq!(side % total, 0, "side {side} is not a whole multiple of {total} modules");
    }

    #[test]
    fn a_tiny_target_still_produces_at_least_one_pixel_per_module() {
        let link = format!("voip://{}@203.0.113.7:6001", "ab".repeat(32));
        let code = qrcode::QrCode::new(link.as_bytes()).unwrap();
        let total = (code.width() + QUIET_ZONE * 2) as u32;

        // Scale clamps to 1 rather than 0, which would render nothing at
        // all and silently produce an empty image.
        assert_eq!(side_for(&link, 1).unwrap(), total);
    }

    #[test]
    fn the_quiet_zone_is_included_in_the_size() {
        let link = format!("voip://{}@203.0.113.7:6001", "ab".repeat(32));
        let code = qrcode::QrCode::new(link.as_bytes()).unwrap();

        let side = side_for(&link, 240).unwrap();
        let scale = side / (code.width() + QUIET_ZONE * 2) as u32;
        assert_eq!(side, (code.width() as u32 + 8) * scale);
    }

    #[test]
    fn an_identity_only_link_encodes_too() {
        let link = format!("voip://{}", "ab".repeat(32));
        assert!(qrcode::QrCode::new(link.as_bytes()).is_ok());
    }
}
