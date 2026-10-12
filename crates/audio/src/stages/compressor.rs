use super::Stage;
use crate::util::{coeff_for_time, db_to_lin, lin_to_db};

/// Evens out volume: quiet talking gets louder, shouting gets quieter
/// (spec §1 "Compressor"). Classic feed-forward design: track an envelope,
/// compute gain reduction above the threshold, smooth it with attack/release,
/// then apply gain reduction plus makeup gain.
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack_coeff: f32,
    release_coeff: f32,
    makeup_gain_lin: f32,

    envelope: f32,
    gain_reduction_db: f32,
}

impl Compressor {
    pub fn new(sample_rate: u32, threshold_db: f32, ratio: f32, attack_ms: f32, release_ms: f32, makeup_gain_db: f32) -> Self {
        Self {
            threshold_db,
            ratio: ratio.max(1.0),
            attack_coeff: coeff_for_time(attack_ms, sample_rate),
            release_coeff: coeff_for_time(release_ms, sample_rate),
            makeup_gain_lin: db_to_lin(makeup_gain_db),
            envelope: 0.0,
            gain_reduction_db: 0.0,
        }
    }

    /// Current gain reduction in dB (spec's "gain reduction meter"), always >= 0.
    pub fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db
    }
}

impl Stage for Compressor {
    fn name(&self) -> &'static str {
        "compressor"
    }

    fn gain_reduction_db(&self) -> Option<f32> {
        Some(self.gain_reduction_db)
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let rectified = s.abs();
            self.envelope += (rectified - self.envelope) * 0.1;
            let env_db = lin_to_db(self.envelope);

            let target_reduction_db = if env_db > self.threshold_db {
                let over_db = env_db - self.threshold_db;
                over_db * (1.0 - 1.0 / self.ratio)
            } else {
                0.0
            };

            let coeff = if target_reduction_db > self.gain_reduction_db {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gain_reduction_db += (target_reduction_db - self.gain_reduction_db) * coeff;

            let gain = db_to_lin(-self.gain_reduction_db) * self.makeup_gain_lin;
            *s *= gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loud_signal_gets_gain_reduced() {
        let sample_rate = 48_000;
        let mut comp = Compressor::new(sample_rate, -20.0, 4.0, 5.0, 50.0, 0.0);
        let mut buf: Vec<f32> = (0..sample_rate)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.9)
            .collect();
        comp.process(&mut buf);
        assert!(comp.gain_reduction_db() > 3.0, "expected meaningful gain reduction, got {}", comp.gain_reduction_db());
    }

    #[test]
    fn quiet_signal_is_left_alone() {
        let sample_rate = 48_000;
        let mut comp = Compressor::new(sample_rate, -20.0, 4.0, 5.0, 50.0, 0.0);
        let mut buf: Vec<f32> = (0..sample_rate)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.01)
            .collect();
        comp.process(&mut buf);
        assert!(comp.gain_reduction_db() < 0.5, "expected ~no gain reduction, got {}", comp.gain_reduction_db());
    }
}
