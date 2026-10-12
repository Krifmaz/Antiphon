use super::Stage;
use crate::util::{coeff_for_time, db_to_lin, lin_to_db};

/// A fixed manual gain multiplier (spec §1 "Gain").
pub struct Gain {
    gain_lin: f32,
}

impl Gain {
    pub fn new(gain_db: f32) -> Self {
        Self { gain_lin: db_to_lin(gain_db) }
    }
}

impl Stage for Gain {
    fn name(&self) -> &'static str {
        "gain"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s *= self.gain_lin;
        }
    }
}

/// Keeps overall mic level consistent without the user touching anything
/// (spec §1 "Auto Gain"). Tracks long-term signal power and slowly nudges a
/// gain toward a target level, clamped to a sane range so it can't runway
/// into silence or a scream.
pub struct AutoGain {
    target_db: f32,
    min_gain_db: f32,
    max_gain_db: f32,
    adapt_coeff: f32,

    power_env: f32,
    current_gain_db: f32,
}

impl AutoGain {
    pub fn new(sample_rate: u32, target_db: f32, min_gain_db: f32, max_gain_db: f32, adapt_time_ms: f32) -> Self {
        Self {
            target_db,
            min_gain_db,
            max_gain_db,
            adapt_coeff: coeff_for_time(adapt_time_ms, sample_rate),
            power_env: 0.0,
            current_gain_db: 0.0,
        }
    }

    pub fn current_gain_db(&self) -> f32 {
        self.current_gain_db
    }
}

impl Stage for AutoGain {
    fn name(&self) -> &'static str {
        "auto_gain"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            self.power_env += (*s * *s - self.power_env) * 0.01;
            let level_db = lin_to_db(self.power_env.sqrt());

            let error_db = self.target_db - level_db;
            self.current_gain_db += error_db * self.adapt_coeff;
            self.current_gain_db = self.current_gain_db.clamp(self.min_gain_db, self.max_gain_db);

            *s *= db_to_lin(self.current_gain_db);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_gain_scales_linearly() {
        let mut gain = Gain::new(6.0); // ~2x
        let mut buf = vec![0.1f32; 10];
        gain.process(&mut buf);
        assert!((buf[0] - 0.1995).abs() < 0.01);
    }

    #[test]
    fn auto_gain_boosts_a_quiet_signal_over_time() {
        let sample_rate = 48_000;
        let mut agc = AutoGain::new(sample_rate, -18.0, -6.0, 24.0, 200.0);
        let mut buf: Vec<f32> = (0..sample_rate * 3)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.02)
            .collect();
        agc.process(&mut buf);
        assert!(agc.current_gain_db() > 5.0, "expected AGC to boost a quiet signal, gain was {}", agc.current_gain_db());
    }
}
