use super::Stage;
use crate::stages::biquad::{HighPassFilter, LowPassFilter};
use crate::util::{coeff_for_time, db_to_lin, lin_to_db};

/// Softens harsh "S"/"T" sounds without dulling the rest of the voice (spec
/// §1 "De-esser"). A split-band compressor: the signal is split at
/// `split_hz` into a low band (left untouched) and a high "sibilance" band,
/// the high band is compressed on its own envelope, and the two are summed
/// back together. That's what keeps a de-esser from also softening
/// consonants and low-mid body the way compressing the whole signal would.
pub struct DeEsser {
    low: LowPassFilter,
    high: HighPassFilter,
    threshold_db: f32,
    ratio: f32,
    attack_coeff: f32,
    release_coeff: f32,

    envelope: f32,
    gain_reduction_db: f32,
}

impl DeEsser {
    pub fn new(sample_rate: u32, split_hz: f32, threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32) -> Self {
        Self {
            low: LowPassFilter::new(sample_rate, split_hz),
            high: HighPassFilter::new(sample_rate, split_hz),
            threshold_db,
            ratio: ratio.max(1.0),
            attack_coeff: coeff_for_time(attack_ms, sample_rate),
            release_coeff: coeff_for_time(release_ms, sample_rate),
            envelope: 0.0,
            gain_reduction_db: 0.0,
        }
    }

    /// Current gain reduction applied to the sibilance band, in dB (always >= 0).
    pub fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db
    }
}

impl Stage for DeEsser {
    fn name(&self) -> &'static str {
        "de_esser"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let low = self.low.process_sample(*s);
            let high = self.high.process_sample(*s);

            let rectified = high.abs();
            self.envelope += (rectified - self.envelope) * 0.2;
            let env_db = lin_to_db(self.envelope);

            let target_reduction_db = if env_db > self.threshold_db {
                (env_db - self.threshold_db) * (1.0 - 1.0 / self.ratio)
            } else {
                0.0
            };
            let coeff = if target_reduction_db > self.gain_reduction_db { self.attack_coeff } else { self.release_coeff };
            self.gain_reduction_db += (target_reduction_db - self.gain_reduction_db) * coeff;

            *s = low + high * db_to_lin(-self.gain_reduction_db);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq_hz: f32, sample_rate: u32, amplitude: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sample_rate as f32).sin() * amplitude).collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        let tail = &samples[samples.len() / 2..]; // let envelopes settle first
        (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
    }

    #[test]
    fn loud_sibilance_gets_tamed() {
        let sample_rate = 48_000;
        let mut de_esser = DeEsser::new(sample_rate, 4000.0, -30.0, 6.0, 3.0, 40.0);
        let mut loud_hiss = sine(7000.0, sample_rate, 0.9, sample_rate as usize);
        let reference = loud_hiss.clone();

        de_esser.process(&mut loud_hiss);

        assert!(rms(&loud_hiss) < rms(&reference) * 0.6, "loud sibilance should be noticeably attenuated");
        assert!(de_esser.gain_reduction_db() > 3.0, "expected gain reduction, got {}", de_esser.gain_reduction_db());
    }

    #[test]
    fn low_frequency_voice_body_passes_through() {
        let sample_rate = 48_000;
        let mut de_esser = DeEsser::new(sample_rate, 4000.0, -30.0, 6.0, 3.0, 40.0);
        let mut voice = sine(200.0, sample_rate, 0.9, sample_rate as usize);
        let reference = voice.clone();

        de_esser.process(&mut voice);

        // A low tone lives almost entirely in the untouched low band, so it
        // should come through close to unity gain regardless of how hard
        // the sibilance band is being compressed.
        assert!(rms(&voice) > rms(&reference) * 0.85, "low-frequency body should pass through mostly unaffected");
    }

    #[test]
    fn quiet_sibilance_is_left_alone() {
        let sample_rate = 48_000;
        let mut de_esser = DeEsser::new(sample_rate, 4000.0, -30.0, 6.0, 3.0, 40.0);
        let mut quiet_hiss = sine(7000.0, sample_rate, 0.02, sample_rate as usize);

        de_esser.process(&mut quiet_hiss);

        assert!(de_esser.gain_reduction_db() < 1.0, "quiet sibilance shouldn't trigger meaningful reduction, got {}", de_esser.gain_reduction_db());
    }
}
