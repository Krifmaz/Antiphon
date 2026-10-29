//! Shrinking a frame to the size it is sent at.

use crate::Frame;

/// `frame`, shrunk so neither side is longer than `max_side`, keeping its
/// shape. A frame that already fits comes back as it is.
///
/// An area average: each output pixel is the mean of the source pixels it
/// covers. Slower than picking the nearest pixel but it keeps thin lines
/// -- the strokes of text on a shared screen -- instead of dropping every
/// other one.
pub fn fit(frame: &Frame, max_side: u32) -> Frame {
    let long = frame.width.max(frame.height);
    if long <= max_side {
        return frame.clone();
    }
    let w = ((frame.width as u64 * max_side as u64) / long as u64).max(1) as u32;
    let h = ((frame.height as u64 * max_side as u64) / long as u64).max(1) as u32;
    shrink(frame, w, h)
}

/// `frame` resampled to exactly `w` by `h`, which must not be larger.
fn shrink(frame: &Frame, w: u32, h: u32) -> Frame {
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    let (w, h) = (w as usize, h as usize);
    // The source columns each output column covers, worked out once.
    let cols: Vec<(usize, usize)> = (0..w).map(|x| (x * sw / w, ((x + 1) * sw / w).max(x * sw / w + 1))).collect();
    let mut out = vec![0u8; w * h * 4];
    let mut sums = vec![0u32; w * 4];
    for y in 0..h {
        let (y0, y1) = (y * sh / h, ((y + 1) * sh / h).max(y * sh / h + 1));
        sums.iter_mut().for_each(|s| *s = 0);
        for sy in y0..y1 {
            let row = &frame.rgba[sy * sw * 4..(sy + 1) * sw * 4];
            for (x, &(x0, x1)) in cols.iter().enumerate() {
                let acc = &mut sums[x * 4..x * 4 + 4];
                for px in row[x0 * 4..x1 * 4].chunks_exact(4) {
                    for c in 0..4 {
                        acc[c] += px[c] as u32;
                    }
                }
            }
        }
        let dst = &mut out[y * w * 4..(y + 1) * w * 4];
        for (x, &(x0, x1)) in cols.iter().enumerate() {
            let n = ((x1 - x0) * (y1 - y0)) as u32;
            for c in 0..4 {
                dst[x * 4 + c] = ((sums[x * 4 + c] + n / 2) / n) as u8;
            }
        }
    }
    Frame { width: w as u32, height: h as u32, rgba: out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_shape_and_the_colour() {
        let f = Frame::solid(1920, 1080, [10, 20, 30, 255]);
        let s = fit(&f, 640);
        assert_eq!((s.width, s.height), (640, 360));
        assert!(s.is_valid());
        assert_eq!(&s.rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn a_frame_that_fits_is_untouched() {
        let f = Frame::solid(320, 240, [1, 2, 3, 4]);
        assert_eq!(fit(&f, 640), f);
    }

    #[test]
    fn averages_rather_than_drops() {
        // Alternating black and white columns average to grey.
        let mut f = Frame::solid(4, 2, [0, 0, 0, 255]);
        for y in 0..2 {
            for x in [1usize, 3] {
                f.rgba[(y * 4 + x) * 4..(y * 4 + x) * 4 + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let s = fit(&f, 2);
        assert_eq!((s.width, s.height), (2, 1));
        assert!((120..=135).contains(&s.rgba[0]));
    }
}
