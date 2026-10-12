//! Formant correction for the pitch shifter (spec §1: a pitch shift that
//! sounds natural rather than chipmunk/demon).
//!
//! What makes a shifted voice sound like a chipmunk is that a plain pitch
//! shifter moves the *whole* spectrum: the vocal-tract resonances (the
//! formants) slide up with the fundamental, and a formant pattern that high
//! belongs to a much smaller head. Formants are the spectral *envelope*, so
//! restoring them means measuring the envelope before the shift, measuring
//! it again after, and filtering the shifted signal by the ratio.
//!
//! This is a channel vocoder rather than an FFT phase vocoder: a bank of
//! log-spaced bandpass filters over both signals, an envelope follower per
//! band, and per-band gain of `dry_envelope / shifted_envelope`. That reuses
//! the biquads already here instead of pulling in an FFT, and at 24 bands
//! it's ~48 biquads plus followers per sample -- cheap enough for a live
//! voice channel. Trade-off, stated plainly: a filter bank resolves the
//! envelope more coarsely than an FFT would, so this restores the general
//! formant structure rather than reproducing it exactly. It fixes the
//! chipmunk character; it isn't studio-grade formant shifting.

/// Bandpass biquad (RBJ constant-0dB-peak form) plus a one-pole envelope
/// follower, instantiated once per band per signal path.
struct Bandpass {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
}

impl Bandpass {
    fn new(sample_rate: u32, center_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * center_hz / sample_rate as f32;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        // Constant 0 dB peak gain bandpass.
        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0 }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// One-pole smoother over |x|, tracking how much energy a band holds.
struct Envelope {
    value: f32,
    coeff: f32,
}

impl Envelope {
    fn new(sample_rate: u32, ms: f32) -> Self {
        // Standard one-pole time constant: reach ~63% in `ms`.
        let coeff = (-1.0 / (sample_rate as f32 * ms / 1000.0)).exp();
        Self { value: 0.0, coeff }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let rectified = x.abs();
        self.value = rectified + self.coeff * (self.value - rectified);
        self.value
    }
}

struct Band {
    dry_filter: Bandpass,
    wet_filter: Bandpass,
    dry_env: Envelope,
    wet_env: Envelope,
}

/// How many bands the envelope is measured in. 24 log-spaced bands over
/// 80 Hz .. 8 kHz puts roughly 4 bands across each of the first two
/// formant regions, which is what carries vowel identity.
const BANDS: usize = 24;
const LOW_HZ: f32 = 80.0;
const HIGH_HZ: f32 = 8000.0;
/// Fast enough to follow speech transitions, slow enough not to chase
/// individual pitch periods (which would undo the shift rather than
/// correct its formants).
const ENVELOPE_MS: f32 = 12.0;
/// Ceiling on per-band correction, so a band that is near-silent in the
/// shifted signal can't be amplified into a screech.
const MAX_GAIN: f32 = 8.0;

pub struct FormantCorrector {
    bands: Vec<Band>,
}

impl FormantCorrector {
    pub fn new(sample_rate: u32) -> Self {
        // Bandpass Q for log-spaced bands: adjacent centres are a constant
        // ratio apart, and Q ~= center/bandwidth, so one Q suits every band.
        let ratio = (HIGH_HZ / LOW_HZ).powf(1.0 / (BANDS - 1) as f32);
        let q = 1.0 / (ratio - 1.0 / ratio).abs().max(0.05);

        let nyquist = sample_rate as f32 / 2.0;
        let bands = (0..BANDS)
            .map(|i| {
                // Clamp below Nyquist: at 8 kHz sample rate the top bands
                // would otherwise be undefined.
                let center = (LOW_HZ * ratio.powi(i as i32)).min(nyquist * 0.95);
                Band {
                    dry_filter: Bandpass::new(sample_rate, center, q),
                    wet_filter: Bandpass::new(sample_rate, center, q),
                    dry_env: Envelope::new(sample_rate, ENVELOPE_MS),
                    wet_env: Envelope::new(sample_rate, ENVELOPE_MS),
                }
            })
            .collect();
        Self { bands }
    }

    /// Rewrites `shifted` in place so it carries `dry`'s spectral envelope.
    /// Both slices must be the same length and time-aligned.
    pub fn process(&mut self, dry: &[f32], shifted: &mut [f32]) {
        debug_assert_eq!(dry.len(), shifted.len());
        for (d, s) in dry.iter().zip(shifted.iter_mut()) {
            let mut out = 0.0;
            for band in &mut self.bands {
                let dry_level = band.dry_env.process(band.dry_filter.process(*d));
                let wet_band = band.wet_filter.process(*s);
                let wet_level = band.wet_env.process(wet_band);
                // The +1e-6 keeps a silent band from dividing by zero; the
                // clamp keeps a quiet one from being boosted into noise.
                let gain = (dry_level / (wet_level + 1e-6)).min(MAX_GAIN);
                out += wet_band * gain;
            }
            *s = out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice-like test signal: a buzzy fundamental plus harmonics, shaped
    /// by a resonance at `formant_hz` so it has an envelope to measure.
    fn voiced(fundamental: f32, formant_hz: f32, sample_rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let mut acc = 0.0;
                for h in 1..=20 {
                    let f = fundamental * h as f32;
                    // Resonant emphasis near the formant, rolling off away from it.
                    let weight = 1.0 / (1.0 + ((f - formant_hz) / 300.0).powi(2));
                    acc += weight * (2.0 * std::f32::consts::PI * f * t).sin();
                }
                acc * 0.1
            })
            .collect()
    }

    /// Energy in a narrow band, measured by running one bandpass over it.
    fn band_energy(samples: &[f32], sample_rate: u32, center: f32) -> f32 {
        let mut bp = Bandpass::new(sample_rate, center, 4.0);
        samples.iter().map(|s| bp.process(*s).powi(2)).sum()
    }

    #[test]
    fn envelope_follower_tracks_a_steady_level() {
        let sample_rate = 48_000;
        let mut env = Envelope::new(sample_rate, 5.0);
        // Half a second of constant 0.5 amplitude is far longer than the
        // 5ms time constant, so the follower should have settled.
        let mut last = 0.0;
        for _ in 0..sample_rate / 2 {
            last = env.process(0.5);
        }
        assert!((last - 0.5).abs() < 0.01, "follower should settle near the input level, got {last}");
    }

    #[test]
    fn bandpass_passes_its_center_and_rejects_far_away() {
        let sample_rate = 48_000;
        let n = sample_rate as usize / 2;
        let at_center: Vec<f32> =
            (0..n).map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin()).collect();
        let far_off: Vec<f32> =
            (0..n).map(|i| (2.0 * std::f32::consts::PI * 8000.0 * i as f32 / sample_rate as f32).sin()).collect();

        let passed = band_energy(&at_center, sample_rate, 1000.0);
        let rejected = band_energy(&far_off, sample_rate, 1000.0);
        assert!(passed > rejected * 50.0, "1kHz band should strongly prefer 1kHz: {passed} vs {rejected}");
    }

    /// The actual point of the module: after correction, the shifted signal's
    /// formant region should line up with the *dry* signal's, not with where
    /// a plain pitch shift moved it.
    #[test]
    fn correction_pulls_the_formant_region_back_toward_the_dry_signal() {
        let sample_rate = 48_000;
        let n = sample_rate as usize; // 1 second

        // Dry: 120 Hz voice with a formant at 800 Hz.
        let dry = voiced(120.0, 800.0, sample_rate, n);
        // A naive octave-up shift moves that formant to ~1600 Hz, which is
        // exactly the chipmunk artifact -- simulate it directly so the test
        // doesn't depend on the shifter's own behaviour.
        let mut shifted = voiced(240.0, 1600.0, sample_rate, n);

        let before_at_dry_formant = band_energy(&shifted[sample_rate as usize / 2..], sample_rate, 800.0);
        let before_at_shifted_formant = band_energy(&shifted[sample_rate as usize / 2..], sample_rate, 1600.0);
        assert!(
            before_at_shifted_formant > before_at_dry_formant,
            "precondition: the un-corrected signal should have its energy at the moved formant"
        );

        FormantCorrector::new(sample_rate).process(&dry, &mut shifted);

        // Measure over the second half so the filters and followers have settled.
        let tail = &shifted[sample_rate as usize / 2..];
        let after_at_dry_formant = band_energy(tail, sample_rate, 800.0);
        let after_at_shifted_formant = band_energy(tail, sample_rate, 1600.0);

        let before_ratio = before_at_dry_formant / before_at_shifted_formant;
        let after_ratio = after_at_dry_formant / after_at_shifted_formant;

        // The bound here is deliberately modest (~1.5x, measured ~1.7x)
        // rather than aspirational, because a filter bank can only
        // *reweight* energy that already exists -- it can't synthesise a
        // formant where the shifted signal has almost nothing. An FFT phase
        // vocoder can move energy and would do better; that's the trade-off
        // this module's doc comment names. What matters for correctness is
        // the direction and that the dry formant genuinely gains, both of
        // which fail loudly if the correction breaks or inverts.
        assert!(
            after_ratio > before_ratio * 1.5,
            "correction should shift the balance back toward the dry formant: ratio {before_ratio} -> {after_ratio}"
        );
        assert!(
            after_at_dry_formant > before_at_dry_formant,
            "the dry formant region should gain absolute energy, not just win on ratio"
        );
    }

    #[test]
    fn silence_stays_silent_and_finite() {
        let sample_rate = 48_000;
        let dry = vec![0.0f32; 4800];
        let mut shifted = vec![0.0f32; 4800];
        FormantCorrector::new(sample_rate).process(&dry, &mut shifted);
        assert!(shifted.iter().all(|s| s.is_finite()), "the divide-by-zero guard should keep output finite");
        assert!(shifted.iter().all(|s| s.abs() < 1e-3), "silence in, silence out");
    }
}
