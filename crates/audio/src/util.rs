pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-9).log10()
}

/// One-pole smoothing coefficient that reaches ~63% of a step change in
/// `time_ms` milliseconds at the given sample rate. Used throughout the
/// chain for attack/release/adaptation envelopes.
pub fn coeff_for_time(time_ms: f32, sample_rate: u32) -> f32 {
    let time_s = (time_ms / 1000.0).max(1e-6);
    1.0 - (-1.0 / (time_s * sample_rate as f32)).exp()
}
