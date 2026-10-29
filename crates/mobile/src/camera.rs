//! The phone's camera, as the platform shells hand it over: what to ask
//! the camera for, and how its raw pictures become the RGBA frames the
//! video codec takes.
//!
//! Plain std, so it compiles on a desktop too and its tests run on the
//! host; the part that needs a running phone (the frame source a call
//! sends from) is `camera_source`.
//!
//! Cameras do not hand out RGBA. Android's camera gives YUV 4:2:0 in one
//! of three layouts (planar, or one of two interleaved-chroma orders, all
//! described by the same strides), and iOS gives BGRA when asked to. Both
//! come in the sensor's own orientation, which on a phone held upright is
//! a quarter turn off. [`to_rgba`] does the colour conversion, the turn
//! and a first coarse shrink in one pass over the pixels, so a frame is
//! read once and the pixels that would be thrown away are never converted.

use std::time::{Duration, Instant};

/// Which side of the phone the camera looks out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Facing {
    /// The selfie camera, on the screen's side: the one a video call
    /// starts with, since the person is looking at the screen.
    #[default]
    Front,
    /// The main camera on the back, for showing the other side something.
    Back,
}

impl Facing {
    /// The other camera, for the switch button.
    pub fn flipped(self) -> Self {
        match self {
            Facing::Front => Facing::Back,
            Facing::Back => Facing::Front,
        }
    }
}

/// What the shared code asks a platform's camera for. The platform picks
/// the nearest the hardware offers; frames that come out larger or faster
/// are shrunk and thinned here, so this is a hint to save the camera work,
/// not a promise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CameraRequest {
    /// Pixels across, in the sensor's landscape orientation.
    pub width: u32,
    /// Pixels down, in the sensor's landscape orientation.
    pub height: u32,
    /// Frames a second wanted.
    pub fps: u32,
    /// Which camera.
    pub facing: Facing,
}

/// The largest side the camera's frames are sent at normally. The codec's
/// camera profile sends at most 640 pixels, so anything bigger would be
/// converted only to be shrunk again.
pub const NORMAL_SIDE: u32 = 640;
/// The largest side on a metered network with the data saver on: a
/// quarter of the pixels, which is roughly a quarter of the bytes.
pub const LOW_DATA_SIDE: u32 = 320;
/// Frames a second normally: what the codec's camera profile expects.
pub const NORMAL_FPS: u32 = 15;
/// Frames a second with the data saver on. A face still reads as moving
/// at eight, and the bytes go down with the frames.
pub const LOW_DATA_FPS: u32 = 8;

/// What to ask the camera for, with or without the data saver.
pub fn request_for(low_data: bool, facing: Facing) -> CameraRequest {
    if low_data {
        CameraRequest { width: LOW_DATA_SIDE, height: LOW_DATA_SIDE * 3 / 4, fps: LOW_DATA_FPS, facing }
    } else {
        CameraRequest { width: NORMAL_SIDE, height: NORMAL_SIDE * 3 / 4, fps: NORMAL_FPS, facing }
    }
}

/// The largest side frames are delivered at, with or without the data
/// saver.
pub fn max_side(low_data: bool) -> u32 {
    if low_data {
        LOW_DATA_SIDE
    } else {
        NORMAL_SIDE
    }
}

/// The frame rate frames are delivered at, with or without the data
/// saver.
pub fn fps(low_data: bool) -> u32 {
    if low_data {
        LOW_DATA_FPS
    } else {
        NORMAL_FPS
    }
}

/// Picks the camera output size for `want` from what the camera lists.
///
/// The smallest listed size that covers `want` in both directions, so
/// nothing has to be scaled up; among those, the one nearest `want`'s
/// shape. If none covers it (an old front camera), the largest there is.
/// Sizes are compared the way cameras list them, landscape.
pub fn pick_size(available: &[(u32, u32)], want: (u32, u32)) -> Option<(u32, u32)> {
    let covering = available.iter().copied().filter(|&(w, h)| w >= want.0 && h >= want.1);
    let shape = |(w, h): (u32, u32)| {
        // How far the shape is from the wanted one, in thousandths, so it
        // sorts as an integer.
        let a = u64::from(w) * u64::from(want.1) * 1000;
        let b = u64::from(h) * u64::from(want.0) * 1000;
        a.abs_diff(b) / (u64::from(h) * u64::from(want.0)).max(1)
    };
    covering
        .min_by_key(|&(w, h)| (u64::from(w) * u64::from(h), shape((w, h))))
        .or_else(|| available.iter().copied().max_by_key(|&(w, h)| u64::from(w) * u64::from(h)))
}

/// Picks the camera's frame-rate range for `fps` from the ranges it lists
/// as (lowest, highest).
///
/// The lowest top that still reaches `fps`, so the sensor is not run
/// faster than frames are used; for that top, the lowest bottom, which
/// lets the camera slow down and expose longer in a dim room rather than
/// send a dark, noisy face. `None` leaves the camera's default.
pub fn pick_fps_range(ranges: &[(i32, i32)], fps: u32) -> Option<(i32, i32)> {
    let fps = i32::try_from(fps).unwrap_or(i32::MAX);
    ranges.iter().copied().filter(|&(lo, hi)| hi >= fps && lo <= hi && lo > 0).min_by_key(|&(lo, hi)| (hi, lo))
}

/// A camera picture's pixels, borrowed from the platform's own buffer for
/// the length of [`to_rgba`].
#[derive(Debug, Clone, Copy)]
pub enum Pixels<'a> {
    /// YUV 4:2:0, as Android's `YUV_420_888`: a full-size luma plane and
    /// two half-size chroma planes. The strides cover every layout that
    /// format stands for: planar I420 has a chroma pixel stride of 1, and
    /// the interleaved NV12 and NV21 have 2, with `u` and `v` pointing one
    /// byte apart into the same buffer.
    Yuv420 {
        /// The luma plane.
        y: &'a [u8],
        /// Bytes from one luma row to the next.
        y_row: usize,
        /// The blue-difference plane.
        u: &'a [u8],
        /// The red-difference plane.
        v: &'a [u8],
        /// Bytes from one chroma row to the next (both chroma planes).
        uv_row: usize,
        /// Bytes from one chroma sample to the next in a row.
        uv_pixel: usize,
    },
    /// Four bytes a pixel, blue first, as iOS's `kCVPixelFormatType_32BGRA`.
    Bgra {
        /// The pixels.
        data: &'a [u8],
        /// Bytes from one row to the next, which can be more than four
        /// times the width: rows are padded for the hardware.
        row: usize,
    },
}

/// One picture from the camera, as the platform got it.
#[derive(Debug, Clone, Copy)]
pub struct CameraImage<'a> {
    /// Pixels across, as stored.
    pub width: u32,
    /// Pixels down, as stored.
    pub height: u32,
    /// The pixels.
    pub pixels: Pixels<'a>,
    /// How far the picture must be turned clockwise to stand upright, in
    /// degrees: 0, 90, 180 or 270. Android reports this as the sensor's
    /// orientation; iOS turns its frames itself and passes 0.
    pub rotation: u32,
}

/// Converts a camera picture to upright RGBA no larger than about
/// `max_side`, or `None` if its buffers are too short for the size it
/// claims (a driver bug, which must not become a read past the end).
///
/// Shrinking here is by whole steps (every second or third pixel), which
/// is cheap and keeps the result within twice `max_side`; the encoder's
/// own scaler does the fine fit after. Returns (width, height, pixels).
pub fn to_rgba(image: &CameraImage, max_side: u32) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h) = (image.width as usize, image.height as usize);
    if w == 0 || h == 0 || !fits(image) {
        return None;
    }
    let step = (image.width.max(image.height) / max_side.max(1)).max(1) as usize;
    // The size after the coarse shrink, before turning.
    let (sw, sh) = (w / step, h / step);
    if sw == 0 || sh == 0 {
        return None;
    }
    let turned = matches!(image.rotation % 360, 90 | 270);
    let (ow, oh) = if turned { (sh, sw) } else { (sw, sh) };
    let mut out = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        for ox in 0..ow {
            // Where this output pixel comes from in the shrunk source.
            let (x, y) = match image.rotation % 360 {
                90 => (oy, sh - 1 - ox),
                180 => (sw - 1 - ox, sh - 1 - oy),
                270 => (sw - 1 - oy, ox),
                _ => (ox, oy),
            };
            let (x, y) = (x * step, y * step);
            let rgb = match image.pixels {
                Pixels::Yuv420 { y: yp, y_row, u, v, uv_row, uv_pixel } => {
                    let c = (y / 2) * uv_row + (x / 2) * uv_pixel;
                    yuv_to_rgb(yp[y * y_row + x], u[c], v[c])
                }
                Pixels::Bgra { data, row } => {
                    let at = y * row + x * 4;
                    [data[at + 2], data[at + 1], data[at]]
                }
            };
            let at = (oy * ow + ox) * 4;
            out[at..at + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
    }
    Some((ow as u32, oh as u32, out))
}

/// Whether every byte [`to_rgba`] could read is inside the buffers.
fn fits(image: &CameraImage) -> bool {
    let (w, h) = (image.width as usize, image.height as usize);
    match image.pixels {
        Pixels::Yuv420 { y, y_row, u, v, uv_row, uv_pixel } => {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let last_chroma = (ch - 1) * uv_row + (cw - 1) * uv_pixel;
            y_row >= w && uv_pixel >= 1 && y.len() >= (h - 1) * y_row + w && u.len() > last_chroma && v.len() > last_chroma
        }
        Pixels::Bgra { data, row } => row >= w * 4 && data.len() >= (h - 1) * row + w * 4,
    }
}

/// One pixel from full-range BT.601 YUV, which is what phone cameras give
/// (JPEG's colour space), in fixed point: the coefficients times 2^16.
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    let (y, u, v) = (i32::from(y) << 16, i32::from(u) - 128, i32::from(v) - 128);
    let clamp = |x: i32| ((x + (1 << 15)) >> 16).clamp(0, 255) as u8;
    [clamp(y + 91_881 * v), clamp(y - 22_554 * u - 46_802 * v), clamp(y + 116_130 * u)]
}

/// Lets frames through no faster than a set rate. Cameras run at their
/// own pace (30 a second is common, whatever was asked), and a frame that
/// will not be sent is not worth converting.
#[derive(Debug, Clone)]
pub struct Pacer {
    /// The shortest gap between two frames let through.
    interval: Duration,
    /// When the last one was let through.
    last: Option<Instant>,
}

impl Pacer {
    /// A pacer for `fps` frames a second.
    pub fn new(fps: u32) -> Self {
        Self { interval: interval(fps), last: None }
    }

    /// Changes the rate, as the data saver comes and goes.
    pub fn set_fps(&mut self, fps: u32) {
        self.interval = interval(fps);
    }

    /// Whether a frame arriving at `now` should be used. A little slack
    /// (a tenth of the interval) keeps a camera running at exactly the
    /// wanted rate from losing every other frame to timing jitter.
    pub fn admit(&mut self, now: Instant) -> bool {
        let slack = self.interval / 10;
        match self.last {
            Some(last) if now.saturating_duration_since(last) + slack < self.interval => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

/// The gap between frames at `fps` a second.
fn interval(fps: u32) -> Duration {
    Duration::from_secs(1) / fps.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4 by 2 BGRA picture with a red pixel top left and a blue one top
    /// right, the rest black.
    fn bgra_4x2() -> Vec<u8> {
        let mut data = vec![0u8; 4 * 2 * 4];
        data[2] = 255; // top left: red (B, G, R order)
        data[3 * 4] = 255; // top right: blue
        data
    }

    #[test]
    fn bgra_comes_out_rgba_and_unturned() {
        let data = bgra_4x2();
        let image = CameraImage { width: 4, height: 2, pixels: Pixels::Bgra { data: &data, row: 16 }, rotation: 0 };
        let (w, h, out) = to_rgba(&image, 640).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&out[0..4], &[255, 0, 0, 255]);
        assert_eq!(&out[12..16], &[0, 0, 255, 255]);
    }

    #[test]
    fn a_quarter_turn_stands_the_picture_up() {
        let data = bgra_4x2();
        let image = CameraImage { width: 4, height: 2, pixels: Pixels::Bgra { data: &data, row: 16 }, rotation: 90 };
        let (w, h, out) = to_rgba(&image, 640).unwrap();
        assert_eq!((w, h), (2, 4));
        // Turned clockwise, the top row becomes the right-hand column:
        // top left goes to top right, top right to bottom right.
        let px = |x: usize, y: usize| &out[(y * 2 + x) * 4..(y * 2 + x) * 4 + 4];
        assert_eq!(px(1, 0), &[255, 0, 0, 255]);
        assert_eq!(px(1, 3), &[0, 0, 255, 255]);

        let image = CameraImage { rotation: 270, ..image };
        let (_, _, out) = to_rgba(&image, 640).unwrap();
        let px = |x: usize, y: usize| &out[(y * 2 + x) * 4..(y * 2 + x) * 4 + 4];
        // Turned the other way, the top row becomes the left column,
        // read upwards.
        assert_eq!(px(0, 3), &[255, 0, 0, 255]);
        assert_eq!(px(0, 0), &[0, 0, 255, 255]);

        let image = CameraImage { rotation: 180, ..image };
        let (w, h, out) = to_rgba(&image, 640).unwrap();
        assert_eq!((w, h), (4, 2));
        assert_eq!(&out[(4 + 3) * 4..(4 + 3) * 4 + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn padded_rows_are_skipped() {
        // Rows padded to 24 bytes, with junk in the padding.
        let mut data = vec![7u8; 24 * 2];
        for row in 0..2 {
            data[row * 24..row * 24 + 16].fill(0);
        }
        let image = CameraImage { width: 4, height: 2, pixels: Pixels::Bgra { data: &data, row: 24 }, rotation: 0 };
        let (_, _, out) = to_rgba(&image, 640).unwrap();
        assert!(out.chunks(4).all(|p| p == [0, 0, 0, 255]));
    }

    #[test]
    fn yuv_greys_and_colours_convert() {
        assert_eq!(yuv_to_rgb(128, 128, 128), [128, 128, 128]);
        assert_eq!(yuv_to_rgb(0, 128, 128), [0, 0, 0]);
        assert_eq!(yuv_to_rgb(255, 128, 128), [255, 255, 255]);
        // Pure red in full-range BT.601 is about (76, 85, 255).
        let [r, g, b] = yuv_to_rgb(76, 85, 255);
        assert!(r > 250 && g < 5 && b < 5, "{r} {g} {b}");
    }

    #[test]
    fn interleaved_chroma_is_read_through_its_strides() {
        // A 4 by 2 NV21 picture: luma 4x2, then one row of V,U pairs for
        // the two 2x2 blocks. Left block red, right block grey.
        let y = [76u8, 76, 128, 128, 76, 76, 128, 128];
        let vu = [255u8, 85, 128, 128];
        let image = CameraImage {
            width: 4,
            height: 2,
            pixels: Pixels::Yuv420 { y: &y, y_row: 4, u: &vu[1..], v: &vu, uv_row: 4, uv_pixel: 2 },
            rotation: 0,
        };
        let (_, _, out) = to_rgba(&image, 640).unwrap();
        assert!(out[0] > 250 && out[1] < 5, "left is red: {:?}", &out[0..4]);
        assert_eq!(&out[12..16], &[128, 128, 128, 255]);
    }

    #[test]
    fn a_short_buffer_is_refused_not_read_past() {
        let data = vec![0u8; 20];
        let image = CameraImage { width: 4, height: 2, pixels: Pixels::Bgra { data: &data, row: 16 }, rotation: 0 };
        assert!(to_rgba(&image, 640).is_none());
        let y = [0u8; 8];
        let image = CameraImage {
            width: 4,
            height: 2,
            pixels: Pixels::Yuv420 { y: &y, y_row: 4, u: &[0], v: &[0], uv_row: 2, uv_pixel: 1 },
            rotation: 0,
        };
        assert!(to_rgba(&image, 640).is_none());
    }

    #[test]
    fn big_frames_are_shrunk_by_whole_steps() {
        let data = vec![0u8; 1280 * 720 * 4];
        let image = CameraImage { width: 1280, height: 720, pixels: Pixels::Bgra { data: &data, row: 1280 * 4 }, rotation: 90 };
        let (w, h, out) = to_rgba(&image, 640).unwrap();
        assert_eq!((w, h), (360, 640));
        assert_eq!(out.len(), 360 * 640 * 4);
        let (w, h, _) = to_rgba(&image, LOW_DATA_SIDE).unwrap();
        assert_eq!((w, h), (180, 320));
    }

    #[test]
    fn the_data_saver_asks_for_less() {
        let normal = request_for(false, Facing::Front);
        let low = request_for(true, Facing::Back);
        assert_eq!((normal.width, normal.height, normal.fps), (640, 480, 15));
        assert!(low.width < normal.width && low.fps < normal.fps);
        assert_eq!(low.facing, Facing::Back);
        assert_eq!(Facing::Front.flipped(), Facing::Back);
    }

    #[test]
    fn the_smallest_covering_size_is_chosen() {
        let sizes = [(1920, 1080), (1280, 720), (640, 480), (640, 360), (320, 240), (176, 144)];
        assert_eq!(pick_size(&sizes, (640, 480)), Some((640, 480)));
        assert_eq!(pick_size(&sizes, (320, 240)), Some((320, 240)));
        assert_eq!(pick_size(&sizes, (800, 600)), Some((1280, 720)));
        // Nothing big enough: the largest there is.
        assert_eq!(pick_size(&[(320, 240), (176, 144)], (640, 480)), Some((320, 240)));
        assert_eq!(pick_size(&[], (640, 480)), None);
    }

    #[test]
    fn the_slowest_fps_range_that_reaches_the_rate_is_chosen() {
        let ranges = [(15, 15), (7, 30), (30, 30), (15, 30), (8, 15)];
        assert_eq!(pick_fps_range(&ranges, 15), Some((8, 15)));
        assert_eq!(pick_fps_range(&ranges, 8), Some((8, 15)));
        assert_eq!(pick_fps_range(&[(30, 30)], 15), Some((30, 30)));
        assert_eq!(pick_fps_range(&[(5, 10)], 15), None);
    }

    #[test]
    fn the_pacer_thins_a_fast_camera() {
        let start = Instant::now();
        let mut pacer = Pacer::new(15);
        // Thirty frames over one second from a 30 fps camera.
        let let_through = (0..30).filter(|i| pacer.admit(start + Duration::from_millis(i * 1000 / 30))).count();
        assert_eq!(let_through, 15);
        // A camera already at the wanted rate, with jitter, loses nothing.
        let mut pacer = Pacer::new(15);
        let jittered = (0..15).filter(|i| pacer.admit(start + Duration::from_millis(i * 66 + (i % 2) * 3))).count();
        assert_eq!(jittered, 15);
        pacer.set_fps(8);
        assert!(!pacer.admit(start + Duration::from_millis(14 * 66 + 70)));
    }
}
