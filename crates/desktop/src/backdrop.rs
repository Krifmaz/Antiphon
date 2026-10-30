//! Reading a background picture (or animated GIF) off the disk into
//! frames the interface can draw.
//!
//! Every frame is decoded once, up front, off the window's thread; the
//! interface then only picks which to show (see `antiphon_ui::Backdrop`).
//! Cropping, turning, fitting and fading happen as it is drawn, so moving
//! a slider never decodes anything again.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::widget::image::Handle;
use image::{AnimationDecoder, ImageReader};

/// Far more than any picture meant as a background; refused before reading.
const MAX_FILE: u64 = 64 << 20;
/// The most decoded pixels kept, in bytes, across every frame. A long GIF
/// past this keeps the frames that fit and loops those.
const MAX_PIXELS: usize = 512 << 20;
/// The shortest a frame is shown. Browsers do the same: a GIF saying 0 ms
/// (or 10) means "as fast as you like", not "spin the processor".
const MIN_DELAY: Duration = Duration::from_millis(20);

/// A decoded picture, ready to hand to the interface.
#[derive(Clone)]
pub struct Loaded {
    /// The frames and how long each stays up; one for a still picture.
    pub frames: Arc<[(Handle, Duration)]>,
    /// Its size in pixels, `(width, height)`.
    pub size: (u32, u32),
    /// When it was decoded, where its animation starts.
    pub started: Instant,
}

/// The file extensions offered when picking a picture.
pub const EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

/// Reads and decodes the picture at `path`. A GIF keeps every frame (up
/// to [`MAX_PIXELS`]); anything else is one frame.
pub fn load(path: &Path) -> Result<Loaded, String> {
    if std::fs::metadata(path).map_err(|e| e.to_string())?.len() > MAX_FILE {
        return Err("That file is too big for a background.".to_string());
    }
    let reader = ImageReader::open(path).map_err(|e| e.to_string())?.with_guessed_format().map_err(|e| e.to_string())?;
    let frames = if reader.format() == Some(image::ImageFormat::Gif) {
        gif_frames(path)?
    } else {
        let picture = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
        vec![(picture, Duration::ZERO)]
    };
    let Some((first, _)) = frames.first() else { return Err("That picture has nothing in it.".to_string()) };
    let size = first.dimensions();
    let frames = frames.into_iter().map(|(img, delay)| (Handle::from_rgba(img.width(), img.height(), img.into_raw()), delay)).collect();
    Ok(Loaded { frames, size, started: Instant::now() })
}

/// Every frame of the GIF at `path` with its delay, stopping early once
/// [`MAX_PIXELS`] is reached.
fn gif_frames(path: &Path) -> Result<Vec<(image::RgbaImage, Duration)>, String> {
    let file = std::io::BufReader::new(std::fs::File::open(path).map_err(|e| e.to_string())?);
    let decoder = image::codecs::gif::GifDecoder::new(file).map_err(|e| e.to_string())?;
    let mut frames = Vec::new();
    let mut bytes = 0;
    for frame in decoder.into_frames() {
        let frame = frame.map_err(|e| e.to_string())?;
        let delay = Duration::from(frame.delay()).max(MIN_DELAY);
        let picture = frame.into_buffer();
        bytes += picture.as_raw().len();
        if bytes > MAX_PIXELS && !frames.is_empty() {
            break;
        }
        frames.push((picture, delay));
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_still_picture_is_one_frame_of_its_size() {
        let path = std::env::temp_dir().join(format!("stoop-backdrop-{}.png", std::process::id()));
        image::RgbaImage::new(7, 3).save(&path).unwrap();
        let loaded = load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.size, (7, 3));
        assert_eq!(loaded.frames.len(), 1);
    }

    #[test]
    fn a_gif_keeps_its_frames_and_a_sane_pace() {
        use image::codecs::gif::GifEncoder;
        let path = std::env::temp_dir().join(format!("stoop-backdrop-{}.gif", std::process::id()));
        {
            let mut encoder = GifEncoder::new(std::fs::File::create(&path).unwrap());
            let frames = (0..3).map(|_| image::Frame::from_parts(image::RgbaImage::new(4, 4), 0, 0, image::Delay::from_numer_denom_ms(0, 1)));
            encoder.encode_frames(frames).unwrap();
        }
        let loaded = load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.frames.len(), 3);
        assert!(loaded.frames.iter().all(|f| f.1 >= MIN_DELAY));
    }

    #[test]
    fn a_file_that_is_not_a_picture_is_refused() {
        let path = std::env::temp_dir().join(format!("stoop-backdrop-{}.txt", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let loaded = load(&path);
        std::fs::remove_file(&path).unwrap();
        assert!(loaded.is_err());
    }
}
