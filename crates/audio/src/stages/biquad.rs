use super::Stage;

/// Direct Form I biquad, coefficients from the RBJ Audio EQ Cookbook.
struct Biquad {
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

impl Biquad {
    fn highpass(sample_rate: u32, cutoff_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz / sample_rate as f32;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 + cos_w0) / 2.0;
        let b1 = -(1.0 + cos_w0);
        let b2 = (1.0 + cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn lowpass(sample_rate: u32, cutoff_hz: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * cutoff_hz / sample_rate as f32;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let b0 = (1.0 - cos_w0) / 2.0;
        let b1 = 1.0 - cos_w0;
        let b2 = (1.0 - cos_w0) / 2.0;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        Self::normalized(b0, b1, b2, a0, a1, a2)
    }

    fn normalized(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
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

/// Cuts low rumble -- desk bumps, footsteps, breath thumps (spec §1: default ~80 Hz).
pub struct HighPassFilter {
    filter: Biquad,
}

impl HighPassFilter {
    pub fn new(sample_rate: u32, cutoff_hz: f32) -> Self {
        Self {
            filter: Biquad::highpass(sample_rate, cutoff_hz, std::f32::consts::FRAC_1_SQRT_2),
        }
    }

    /// Filters a single sample, for stages (like the de-esser) that need to
    /// split a signal into bands rather than filter a whole buffer in place.
    pub(crate) fn process_sample(&mut self, x: f32) -> f32 {
        self.filter.process(x)
    }
}

impl Stage for HighPassFilter {
    fn name(&self) -> &'static str {
        "high_pass_filter"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s = self.filter.process(*s);
        }
    }
}

/// Cuts high hiss and sharpness (spec §1).
pub struct LowPassFilter {
    filter: Biquad,
}

impl LowPassFilter {
    pub fn new(sample_rate: u32, cutoff_hz: f32) -> Self {
        Self {
            filter: Biquad::lowpass(sample_rate, cutoff_hz, std::f32::consts::FRAC_1_SQRT_2),
        }
    }

    pub(crate) fn process_sample(&mut self, x: f32) -> f32 {
        self.filter.process(x)
    }
}

impl Stage for LowPassFilter {
    fn name(&self) -> &'static str {
        "low_pass_filter"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            *s = self.filter.process(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RMS of a filter's steady-state response to a pure sine tone: the
    /// standard way to check a filter attenuates (or passes) a frequency.
    fn tone_rms(mut filter: impl FnMut(f32) -> f32, sample_rate: u32, freq_hz: f32) -> f32 {
        let n = sample_rate as usize; // 1 second, plenty of settling time
        let mut sum_sq = 0.0;
        let mut count = 0;
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let x = (2.0 * std::f32::consts::PI * freq_hz * t).sin();
            let y = filter(x);
            // Skip the first half to let the filter settle before measuring.
            if i > n / 2 {
                sum_sq += y * y;
                count += 1;
            }
        }
        (sum_sq / count as f32).sqrt()
    }

    #[test]
    fn high_pass_attenuates_low_frequencies_and_passes_high() {
        let sample_rate = 48_000;
        let mut low = HighPassFilter::new(sample_rate, 80.0);
        let low_rms = tone_rms(|x| low.filter.process(x), sample_rate, 40.0);

        let mut high = HighPassFilter::new(sample_rate, 80.0);
        let high_rms = tone_rms(|x| high.filter.process(x), sample_rate, 2000.0);

        assert!(low_rms < 0.3, "40 Hz should be heavily attenuated, got rms {low_rms}");
        assert!(high_rms > 0.6, "2 kHz should pass mostly untouched, got rms {high_rms}");
    }

    #[test]
    fn low_pass_attenuates_high_frequencies_and_passes_low() {
        let sample_rate = 48_000;
        let mut low = LowPassFilter::new(sample_rate, 4000.0);
        let low_rms = tone_rms(|x| low.filter.process(x), sample_rate, 200.0);

        let mut high = LowPassFilter::new(sample_rate, 4000.0);
        let high_rms = tone_rms(|x| high.filter.process(x), sample_rate, 12_000.0);

        assert!(low_rms > 0.6, "200 Hz should pass mostly untouched, got rms {low_rms}");
        assert!(high_rms < 0.3, "12 kHz should be heavily attenuated, got rms {high_rms}");
    }
}
