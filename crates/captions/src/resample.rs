//! Streaming sample-rate conversion to the 16 kHz the speech model reads.
//!
//! Call audio runs at 48 kHz (sometimes 44.1 kHz, whatever the device
//! picked); Whisper was trained on 16 kHz. Throwing away two samples in
//! three is not enough: everything between 8 and 24 kHz would fold back
//! down into the speech band as noise. So this is a proper low-pass +
//! resample: a Blackman-windowed sinc evaluated at each output instant.
//!
//! It is streaming because audio arrives a frame at a time, and the result
//! must not depend on where the frame boundaries fell -- a test feeds the
//! same signal whole and in 20 ms pieces and requires identical output.

use std::f64::consts::PI;

/// Zero crossings of the sinc kept on each side. Eight gives well over
/// 40 dB of stopband, which is plenty for a recogniser and cheap enough to
/// run per output sample (about 50 multiply-adds at 48 -> 16 kHz).
const ZERO_CROSSINGS: f64 = 8.0;

pub struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Cutoff in cycles per *input* sample.
    cutoff: f64,
    /// Kernel half-width in input samples.
    half: usize,
    /// Pending input. Index 0 is the oldest sample still needed.
    buf: Vec<f32>,
    /// Position of the next output sample, in input samples from `buf[0]`.
    pos: f64,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        let in_rate = in_rate.max(1) as f64;
        let out_rate = out_rate.max(1) as f64;
        // A little under Nyquist of the slower side, so the transition
        // band finishes before aliases can land in it.
        let cutoff = 0.5 * (out_rate / in_rate).min(1.0) * 0.92;
        let half = (ZERO_CROSSINGS / (2.0 * cutoff)).ceil() as usize;
        Self {
            step: in_rate / out_rate,
            cutoff,
            half,
            // Pretend the stream was preceded by silence, so the first
            // output is centred on the first real input sample.
            buf: vec![0.0; half],
            pos: half as f64,
        }
    }

    /// Feeds input and appends every output sample that can now be made.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        self.buf.extend_from_slice(input);
        let half = self.half as f64;
        while self.pos + half < self.buf.len() as f64 {
            out.push(self.sample_at(self.pos));
            self.pos += self.step;
        }
        // Drop what no future output can reach.
        let keep_from = (self.pos - half).floor().max(0.0) as usize;
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.pos -= keep_from as f64;
        }
    }

    fn sample_at(&self, t: f64) -> f32 {
        let centre = t.floor() as isize;
        let half = self.half as isize;
        let two_fc = 2.0 * self.cutoff;
        let mut acc = 0.0f64;
        for k in (centre - half + 1)..=(centre + half) {
            if k < 0 || k as usize >= self.buf.len() {
                continue;
            }
            let d = t - k as f64;
            let x = d / self.half as f64;
            if x.abs() >= 1.0 {
                continue;
            }
            let window = 0.42 + 0.5 * (PI * x).cos() + 0.08 * (2.0 * PI * x).cos();
            let arg = two_fc * d;
            let sinc = if arg.abs() < 1e-12 { 1.0 } else { (PI * arg).sin() / (PI * arg) };
            acc += self.buf[k as usize] as f64 * two_fc * sinc * window;
        }
        acc as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(freq: f32, rate: u32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n).map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn three_to_one_length() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        r.process(&vec![0.0; 48_000], &mut out);
        // Everything except the kernel's look-ahead has come out.
        let missing = 16_000 - out.len();
        assert!(missing <= r.half / 3 + 1, "missing {missing}");
    }

    #[test]
    fn speech_band_passes_at_unity() {
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        r.process(&tone(1_000.0, 48_000, 1.0), &mut out);
        let steady = &out[1_000..out.len() - 1_000];
        let gain = rms(steady) / (0.5 / 2f32.sqrt());
        assert!((gain - 1.0).abs() < 0.01, "gain {gain}");
    }

    #[test]
    fn content_above_8k_is_removed_not_folded_down() {
        // 10 kHz would alias to 6 kHz, right in the speech band, with
        // naive decimation.
        let mut r = Resampler::new(48_000, 16_000);
        let mut out = Vec::new();
        r.process(&tone(10_000.0, 48_000, 1.0), &mut out);
        let steady = &out[1_000..out.len() - 1_000];
        let db = 20.0 * (rms(steady) / (0.5 / 2f32.sqrt())).log10();
        assert!(db < -40.0, "only {db} dB down");
    }

    #[test]
    fn frame_boundaries_do_not_change_the_output() {
        let input = tone(440.0, 48_000, 0.5);
        let mut whole = Vec::new();
        Resampler::new(48_000, 16_000).process(&input, &mut whole);

        let mut r = Resampler::new(48_000, 16_000);
        let mut pieces = Vec::new();
        for chunk in input.chunks(960) {
            r.process(chunk, &mut pieces);
        }
        assert_eq!(whole.len(), pieces.len());
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn non_integer_ratio_works() {
        let mut r = Resampler::new(44_100, 16_000);
        let mut out = Vec::new();
        for chunk in tone(1_000.0, 44_100, 1.0).chunks(441) {
            r.process(chunk, &mut out);
        }
        assert!((out.len() as i64 - 16_000).abs() < 30, "{}", out.len());
        let gain = rms(&out[1_000..out.len() - 1_000]) / (0.5 / 2f32.sqrt());
        assert!((gain - 1.0).abs() < 0.01, "gain {gain}");
    }
}
