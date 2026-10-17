//! Animated GIF support for the theme background image and profile
//! avatar. Slint's `Image` element only ever shows a GIF's first frame --
//! there's no built-in playback -- so this decodes every frame up front
//! with the `image` crate and drives playback by hand: a self-rescheduling
//! `slint::Timer::single_shot` per frame (not a fixed-interval repeating
//! timer, since GIFs can give each frame its own delay), swapping in the
//! next frame's already-decoded `slint::Image` each tick.
//!
//! Decoding is split from constructing `slint::Image`s on purpose:
//! `slint::Image` is not `Send` (it wraps a non-atomic refcounted pointer),
//! so it can never cross into a background thread or a
//! `slint::invoke_from_event_loop` closure. [`load_raw`] does the file I/O
//! and GIF decoding into plain `Vec<u8>` RGBA buffers (which *are* `Send`)
//! and can run anywhere; [`apply_raw_loaded`] turns those into actual
//! `slint::Image`s and must only ever run on the UI thread.
//!
//! For the same reason, the "which animation is still current" generation
//! counter lives as a plain `int` property on `MainWindow` (via
//! [`ImageSlot`]) rather than a Rust-side `Rc<Cell<u64>>` -- the latter
//! isn't `Send` either, and would hit the exact same problem the moment it
//! needed to be read back from inside an `invoke_from_event_loop` closure
//! that originated on a background thread. Each scheduled tick checks
//! whether its own generation still matches before doing anything, and
//! just quietly stops rescheduling itself once a newer load (or a clear)
//! has moved the counter on -- no real timer cancellation needed.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use image::{codecs::gif::GifDecoder, AnimationDecoder};

use crate::MainWindow;

/// Installs one decoded frame on whichever image slot is animating.
type SetFrame = Rc<dyn Fn(&MainWindow, slint::Image)>;

/// One decoded frame's raw pixels -- plain data, safe to build on any
/// thread and hand across to the UI thread.
pub struct RawFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
    delay_ms: u64,
}

pub enum RawLoaded {
    /// Not a GIF (or a single-frame one) -- deferred to
    /// `slint::Image::load_from_path`, which itself must run on the UI
    /// thread, so this just carries the path forward.
    Static(PathBuf),
    /// A GIF with exactly one frame: already decoded, no point re-reading
    /// the file a second time through `load_from_path`.
    StaticGifFrame(RawFrame),
    Animated(Vec<RawFrame>),
}

/// Which image slot an animation is playing into -- each has its own
/// generation counter (a plain `MainWindow` property) so loading a new
/// background image can't accidentally stop the avatar's animation or
/// vice versa.
#[derive(Clone, Copy)]
pub enum ImageSlot {
    ThemeBackground,
    ProfileAvatar,
}

impl ImageSlot {
    fn generation(self, app: &MainWindow) -> i32 {
        match self {
            ImageSlot::ThemeBackground => app.get_theme_bg_gif_generation(),
            ImageSlot::ProfileAvatar => app.get_avatar_gif_generation(),
        }
    }
    fn set_generation(self, app: &MainWindow, value: i32) {
        match self {
            ImageSlot::ThemeBackground => app.set_theme_bg_gif_generation(value),
            ImageSlot::ProfileAvatar => app.set_avatar_gif_generation(value),
        }
    }
}

fn is_gif(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

fn decode_gif_raw_frames(bytes: &[u8]) -> anyhow::Result<Vec<RawFrame>> {
    let decoder = GifDecoder::new(std::io::Cursor::new(bytes))?;
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        let frame = frame?;
        let (numer, denom) = frame.delay().numer_denom_ms();
        // A few encoders emit a 0ms delay, which would otherwise spin the
        // reschedule loop as fast as it can tick -- floor it to something
        // still snappy but not a busy-loop.
        let delay_ms = numer.checked_div(denom).map_or(100, |d| d.max(20)) as u64;
        let buffer = frame.into_buffer();
        let (width, height) = buffer.dimensions();
        frames.push(RawFrame { width, height, rgba: buffer.into_raw(), delay_ms });
    }
    anyhow::ensure!(!frames.is_empty(), "GIF has no frames");
    Ok(frames)
}

/// Safe to call from any thread: file I/O plus (for GIFs) decoding, but
/// never constructs a `slint::Image`. Pair with [`apply_raw_loaded`] on
/// the UI thread to actually display the result.
pub fn load_raw(path: &Path) -> anyhow::Result<RawLoaded> {
    if is_gif(path) {
        let bytes = std::fs::read(path)?;
        let mut frames = decode_gif_raw_frames(&bytes)?;
        if frames.len() > 1 {
            return Ok(RawLoaded::Animated(frames));
        }
        return Ok(RawLoaded::StaticGifFrame(frames.remove(0)));
    }
    Ok(RawLoaded::Static(path.to_path_buf()))
}

fn frame_to_image(frame: &RawFrame) -> slint::Image {
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(frame.width, frame.height);
    pixels.make_mut_bytes().copy_from_slice(&frame.rgba);
    slint::Image::from_rgba8(pixels)
}

/// UI-thread only: turns `raw` into actual `slint::Image`(s) and applies
/// them via `set_frame`, starting playback for an animated GIF.
pub fn apply_raw_loaded(
    app: &MainWindow,
    window_weak: slint::Weak<MainWindow>,
    raw: RawLoaded,
    slot: ImageSlot,
    set_frame: SetFrame,
) -> anyhow::Result<()> {
    let my_generation = slot.generation(app).wrapping_add(1);
    slot.set_generation(app, my_generation);
    match raw {
        RawLoaded::Static(path) => set_frame(app, slint::Image::load_from_path(&path)?),
        RawLoaded::StaticGifFrame(frame) => set_frame(app, frame_to_image(&frame)),
        RawLoaded::Animated(frames) => {
            let images: Rc<Vec<slint::Image>> = Rc::new(frames.iter().map(frame_to_image).collect());
            let delays_ms: Rc<Vec<u64>> = Rc::new(frames.iter().map(|f| f.delay_ms).collect());
            schedule_gif_frame(window_weak, images, delays_ms, 0, slot, my_generation, set_frame);
        }
    }
    Ok(())
}

/// Stops whatever animation is currently playing in this image slot
/// (bumps its generation so the next scheduled tick, if any, no-ops)
/// without setting a new image -- for a "Remove" action that falls back
/// to whatever default the caller's `set_frame` wasn't asked to draw.
pub fn stop_animation(app: &MainWindow, slot: ImageSlot) {
    slot.set_generation(app, slot.generation(app).wrapping_add(1));
}

fn schedule_gif_frame(
    window_weak: slint::Weak<MainWindow>,
    images: Rc<Vec<slint::Image>>,
    delays_ms: Rc<Vec<u64>>,
    frame_index: usize,
    slot: ImageSlot,
    my_generation: i32,
    set_frame: SetFrame,
) {
    let Some(app) = window_weak.upgrade() else { return };
    if slot.generation(&app) != my_generation || images.is_empty() {
        return;
    }
    let idx = frame_index % images.len();
    set_frame(&app, images[idx].clone());
    let delay = Duration::from_millis(delays_ms[idx]);
    slint::Timer::single_shot(delay, move || {
        schedule_gif_frame(window_weak, images, delays_ms, frame_index + 1, slot, my_generation, set_frame);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_gif_matches_extension_case_insensitively() {
        assert!(is_gif(Path::new("party.gif")));
        assert!(is_gif(Path::new("PARTY.GIF")));
        assert!(!is_gif(Path::new("photo.png")));
        assert!(!is_gif(Path::new("no-extension")));
    }
}
