use super::Stage;
use crate::util::{coeff_for_time, db_to_lin, lin_to_db};

/// A hard ceiling on volume: catches sudden loud noises before they hit
/// anyone's ears (spec §1 "Limiter"). This is a no-lookahead limiter --
/// gain reduction is smoothed for a musical sound, but the final sample is
/// always additionally clamped to the ceiling so it can never be exceeded,
/// even during the attack ramp.
pub struct Limiter {
    ceiling_lin: f32,
    ceiling_db: f32,
    attack_coeff: f32,
    release_coeff: f32,

    envelope: f32,
    gain_reduction_db: f32,
}

impl Limiter {
    pub fn new(sample_rate: u32, ceiling_db: f32, release_ms: f32) -> Self {
        Self {
            ceiling_lin: db_to_lin(ceiling_db),
            ceiling_db,
            attack_coeff: coeff_for_time(0.5, sample_rate), // near-instant
            release_coeff: coeff_for_time(release_ms, sample_rate),
            envelope: 0.0,
            gain_reduction_db: 0.0,
        }
    }

    pub fn gain_reduction_db(&self) -> f32 {
        self.gain_reduction_db
    }
}

impl Stage for Limiter {
    fn name(&self) -> &'static str {
        "limiter"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let rectified = s.abs();
            self.envelope += (rectified - self.envelope) * 0.5;
            let env_db = lin_to_db(self.envelope);

            let target_reduction_db = (env_db - self.ceiling_db).max(0.0);
            let coeff = if target_reduction_db > self.gain_reduction_db {
                self.attack_coeff
            } else {
                self.release_coeff
            };
            self.gain_reduction_db += (target_reduction_db - self.gain_reduction_db) * coeff;

            let gained = *s * db_to_lin(-self.gain_reduction_db);
            *s = gained.clamp(-self.ceiling_lin, self.ceiling_lin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_exceeds_ceiling() {
        let sample_rate = 48_000;
        let ceiling_db = -3.0;
        let mut limiter = Limiter::new(sample_rate, ceiling_db, 50.0);
        let ceiling_lin = db_to_lin(ceiling_db);

        // A sudden loud burst after silence -- the worst case for a
        // no-lookahead limiter, which is exactly why the hard clamp exists.
        let mut buf = vec![0.0f32; 100];
        buf.extend((0..1000).map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin()));
        limiter.process(&mut buf);

        for s in &buf {
            assert!(s.abs() <= ceiling_lin + 1e-6, "sample {s} exceeded ceiling {ceiling_lin}");
        }
    }
}
