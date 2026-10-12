use super::Stage;
use crate::util::{coeff_for_time, db_to_lin};

/// Mutes the mic when nobody's talking (spec §1 "Noise Gate"): keyboard,
/// fan, and room noise between sentences get cut instead of transmitted.
pub struct NoiseGate {
    threshold_lin: f32,
    attack_coeff: f32,
    release_coeff: f32,
    hold_samples: u32,

    detector_env: f32,
    gain: f32,
    hold_counter: u32,
}

impl NoiseGate {
    pub fn new(sample_rate: u32, threshold_db: f32, attack_ms: f32, hold_ms: f32, release_ms: f32) -> Self {
        Self {
            threshold_lin: db_to_lin(threshold_db),
            attack_coeff: coeff_for_time(attack_ms, sample_rate),
            release_coeff: coeff_for_time(release_ms, sample_rate),
            hold_samples: ((hold_ms / 1000.0) * sample_rate as f32) as u32,
            detector_env: 0.0,
            gain: 0.0,
            hold_counter: 0,
        }
    }

    /// Current gate gain in `[0, 1]`, i.e. what the spec's "live meter
    /// showing what's passing through" would display.
    pub fn current_gain(&self) -> f32 {
        self.gain
    }
}

impl NoiseGate {
    /// How far the gate is currently attenuating, in dB (0 when fully
    /// open). Spec §1's "live meter showing what's passing through".
    ///
    /// A closed gate is silence, which is negative infinity in dB, so this
    /// floors at 60: a meter needs a number it can draw, and anything past
    /// 60 dB down is inaudible either way.
    fn attenuation_db(&self) -> f32 {
        if self.gain >= 1.0 {
            0.0
        } else if self.gain <= 0.001 {
            60.0
        } else {
            -20.0 * self.gain.log10()
        }
    }
}

impl Stage for NoiseGate {
    fn name(&self) -> &'static str {
        "noise_gate"
    }

    fn gain_reduction_db(&self) -> Option<f32> {
        Some(self.attenuation_db())
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            // Fast fixed-rate follower just for open/close detection --
            // separate from the attack/release that smooths the actual gain.
            let rectified = s.abs();
            self.detector_env += (rectified - self.detector_env) * 0.5;

            let target_gain = if self.detector_env > self.threshold_lin {
                self.hold_counter = self.hold_samples;
                1.0
            } else if self.hold_counter > 0 {
                self.hold_counter -= 1;
                1.0
            } else {
                0.0
            };

            let coeff = if target_gain > self.gain {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gain += (target_gain - self.gain) * coeff;
            *s *= self.gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_reports_full_attenuation() {
        let mut gate = NoiseGate::new(48_000, -40.0, 1.0, 0.0, 1.0);
        // Silence for long enough that the release has finished.
        let mut quiet = vec![0.0f32; 48_000];
        gate.process(&mut quiet);

        assert!(gate.attenuation_db() > 50.0, "closed gate read {} dB", gate.attenuation_db());
    }

    #[test]
    fn an_open_gate_reports_no_attenuation() {
        let mut gate = NoiseGate::new(48_000, -40.0, 1.0, 100.0, 100.0);
        let mut loud: Vec<f32> = (0..4_800).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        gate.process(&mut loud);

        assert!(gate.attenuation_db() < 1.0, "open gate read {} dB", gate.attenuation_db());
    }

    #[test]
    fn attenuation_is_reported_through_the_stage_trait() {
        // The meter reads this generically, the same way it reads the
        // compressor -- "how much is this stage pulling the signal down"
        // is one question with two answers, not two features.
        let gate = NoiseGate::new(48_000, -40.0, 1.0, 0.0, 1.0);
        assert!(Stage::gain_reduction_db(&gate).is_some());
    }


    #[test]
    fn silence_is_fully_muted_after_release() {
        let mut gate = NoiseGate::new(48_000, -40.0, 5.0, 50.0, 20.0);
        let mut buf = vec![0.0f32; 48_000]; // 1s of silence, longer than hold+release
        gate.process(&mut buf);
        assert!(buf.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn loud_signal_passes_through_once_open() {
        let mut gate = NoiseGate::new(48_000, -40.0, 5.0, 50.0, 20.0);
        let sample_rate = 48_000;
        let mut buf: Vec<f32> = (0..sample_rate)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5)
            .collect();
        gate.process(&mut buf);
        // After the attack ramp, later samples should be passing near-unattenuated.
        let tail_energy: f32 = buf[buf.len() - 1000..].iter().map(|s| s.abs()).sum();
        assert!(tail_energy > 300.0, "expected the gate to be open by the tail, got energy {tail_energy}");
    }
}
