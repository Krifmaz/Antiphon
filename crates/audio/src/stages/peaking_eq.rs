use super::Stage;

/// What shape a band applies (spec §1: "per-band type (bell/shelf/notch)").
///
/// All four are RBJ cookbook biquads and differ only in their coefficients,
/// so they share one filter struct, one `process`, and one magnitude
/// response -- the curve graph needed no changes to draw them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BandType {
    /// Boost or cut around a centre frequency, tapering off either side.
    #[default]
    Bell,
    /// Boost or cut everything *below* the corner, flat above it.
    LowShelf,
    /// Boost or cut everything *above* the corner, flat below it.
    HighShelf,
    /// Deep, narrow cut at one frequency, leaving the rest alone. Gain is
    /// ignored -- a notch is always a cut, and its depth comes from Q.
    Notch,
}

impl BandType {
    /// Stable integers for config files and the UI, which has no shared
    /// enum with Rust. Unknown values fall back to `Bell`, the shape that
    /// behaves most predictably if a config is ever hand-edited.
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => BandType::LowShelf,
            2 => BandType::HighShelf,
            3 => BandType::Notch,
            _ => BandType::Bell,
        }
    }

    pub fn to_i32(self) -> i32 {
        match self {
            BandType::Bell => 0,
            BandType::LowShelf => 1,
            BandType::HighShelf => 2,
            BandType::Notch => 3,
        }
    }
}

/// One band of a parametric EQ (spec §1 "Tone stage"). A multi-band EQ is
/// just several of these composed in series -- [`crate::chain::AudioChain`]
/// already supports that with no extra code.
pub struct PeakingEq {
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

impl PeakingEq {
    /// A bell band, the default shape.
    pub fn new(sample_rate: u32, center_hz: f32, q: f32, gain_db: f32) -> Self {
        Self::with_type(sample_rate, BandType::Bell, center_hz, q, gain_db)
    }

    /// Any band shape. Coefficients are the RBJ audio cookbook forms.
    pub fn with_type(
        sample_rate: u32,
        band_type: BandType,
        center_hz: f32,
        q: f32,
        gain_db: f32,
    ) -> Self {
        // Guard the inputs the caller can set to nonsense from a slider or
        // a hand-edited config: a zero Q divides by zero, and a centre
        // frequency at or above Nyquist has no meaningful response.
        let q = q.max(0.05);
        let nyquist = sample_rate as f32 / 2.0;
        let center_hz = center_hz.clamp(1.0, nyquist * 0.99);

        let a = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * center_hz / sample_rate as f32;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match band_type {
            BandType::Bell => (
                1.0 + alpha * a,
                -2.0 * cos_w0,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos_w0,
                1.0 - alpha / a,
            ),
            BandType::LowShelf => {
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0),
                    a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos_w0),
                    (a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            BandType::HighShelf => {
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0),
                    a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos_w0),
                    (a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            // Gain has no part in a notch: it is a cut by construction.
            BandType::Notch => (
                1.0,
                -2.0 * cos_w0,
                1.0,
                1.0 + alpha,
                -2.0 * cos_w0,
                1.0 - alpha,
            ),
        };

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

    /// The filter's magnitude response in dB at `freq_hz` -- the closed-form
    /// |H(e^jω)| for this biquad's coefficients, not a measurement from
    /// running audio through it. This is what an EQ curve graph plots, and
    /// (since cascaded biquads multiply in linear magnitude) summing this
    /// across several bands gives the combined curve without needing to
    /// actually run a signal through all of them.
    pub fn magnitude_db(&self, sample_rate: u32, freq_hz: f32) -> f32 {
        let w = 2.0 * std::f32::consts::PI * freq_hz / sample_rate as f32;
        let (sin_w, cos_w) = w.sin_cos();
        let (sin_2w, cos_2w) = (2.0 * w).sin_cos();

        let num_re = self.b0 + self.b1 * cos_w + self.b2 * cos_2w;
        let num_im = -(self.b1 * sin_w + self.b2 * sin_2w);
        let den_re = 1.0 + self.a1 * cos_w + self.a2 * cos_2w;
        let den_im = -(self.a1 * sin_w + self.a2 * sin_2w);

        let num_mag = (num_re * num_re + num_im * num_im).sqrt();
        let den_mag = (den_re * den_re + den_im * den_im).sqrt().max(1e-9);
        20.0 * (num_mag / den_mag).max(1e-9).log10()
    }
}

impl Stage for PeakingEq {
    fn name(&self) -> &'static str {
        "peaking_eq"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let x = *s;
            let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2 - self.a1 * self.y1 - self.a2 * self.y2;
            self.x2 = self.x1;
            self.x1 = x;
            self.y2 = self.y1;
            self.y1 = y;
            *s = y;
        }
    }
}

#[cfg(test)]
mod tests {
    /// Shape checks are done through the closed-form magnitude response
    /// rather than by pushing audio through: the response *is* what the
    /// band promises, and asserting on it says "a low shelf lifts the lows
    /// and leaves the highs" directly instead of inferring it from RMS.
    fn response(band_type: BandType, freq: f32, q: f32, gain: f32, at: f32) -> f32 {
        PeakingEq::with_type(48_000, band_type, freq, q, gain).magnitude_db(48_000, at)
    }

    #[test]
    fn a_low_shelf_lifts_below_the_corner_and_leaves_above_it() {
        let boost = 6.0;
        // Well below the corner: close to the full boost.
        assert!(response(BandType::LowShelf, 300.0, 0.7, boost, 60.0) > 5.0);
        // Well above it: essentially untouched.
        assert!(response(BandType::LowShelf, 300.0, 0.7, boost, 6000.0).abs() < 0.5);
    }

    #[test]
    fn a_high_shelf_lifts_above_the_corner_and_leaves_below_it() {
        let boost = 6.0;
        assert!(response(BandType::HighShelf, 3000.0, 0.7, boost, 12000.0) > 5.0);
        assert!(response(BandType::HighShelf, 3000.0, 0.7, boost, 200.0).abs() < 0.5);
    }

    #[test]
    fn shelves_cut_as_well_as_boost() {
        assert!(response(BandType::LowShelf, 300.0, 0.7, -6.0, 60.0) < -5.0);
        assert!(response(BandType::HighShelf, 3000.0, 0.7, -6.0, 12000.0) < -5.0);
    }

    #[test]
    fn a_notch_cuts_hard_at_its_centre_and_passes_everything_else() {
        // The classic use: kill one specific whine without dulling anything.
        assert!(response(BandType::Notch, 1000.0, 8.0, 0.0, 1000.0) < -25.0);
        assert!(response(BandType::Notch, 1000.0, 8.0, 0.0, 300.0).abs() < 1.0);
        assert!(response(BandType::Notch, 1000.0, 8.0, 0.0, 5000.0).abs() < 1.0);
    }

    #[test]
    fn a_notch_ignores_gain() {
        // Gain is meaningless for a notch, and must not silently turn it
        // into a boost.
        let with_gain = response(BandType::Notch, 1000.0, 4.0, 12.0, 1000.0);
        let without = response(BandType::Notch, 1000.0, 4.0, 0.0, 1000.0);
        assert!((with_gain - without).abs() < 1e-3);
    }

    #[test]
    fn a_higher_q_notch_is_narrower() {
        let wide = response(BandType::Notch, 1000.0, 1.0, 0.0, 1200.0);
        let narrow = response(BandType::Notch, 1000.0, 12.0, 0.0, 1200.0);
        // Just off centre, the narrow notch has already recovered.
        assert!(narrow > wide);
    }

    #[test]
    fn every_band_type_is_flat_at_zero_gain_except_the_notch() {
        for band_type in [BandType::Bell, BandType::LowShelf, BandType::HighShelf] {
            for at in [100.0, 1000.0, 8000.0] {
                let db = response(band_type, 1000.0, 0.7, 0.0, at);
                assert!(db.abs() < 0.01, "{band_type:?} at {at} Hz was {db} dB, expected flat");
            }
        }
    }

    #[test]
    fn band_types_round_trip_through_their_integer_form() {
        for band_type in [BandType::Bell, BandType::LowShelf, BandType::HighShelf, BandType::Notch] {
            assert_eq!(BandType::from_i32(band_type.to_i32()), band_type);
        }
        // An unrecognised value falls back rather than panicking.
        assert_eq!(BandType::from_i32(99), BandType::Bell);
        assert_eq!(BandType::from_i32(-1), BandType::Bell);
    }

    #[test]
    fn a_degenerate_q_or_frequency_does_not_produce_nan() {
        // Sliders and hand-edited configs can ask for nonsense; the filter
        // must stay finite rather than poisoning the whole chain with NaN.
        for (q, freq) in [(0.0, 1000.0), (-5.0, 1000.0), (1.0, 0.0), (1.0, 40_000.0)] {
            let mut eq = PeakingEq::with_type(48_000, BandType::Bell, freq, q, 6.0);
            let mut buf = vec![0.5f32; 64];
            eq.process(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite()), "q={q} freq={freq} produced non-finite audio");
        }
    }

    use super::*;

    #[test]
    fn boost_raises_energy_at_center_frequency() {
        let sample_rate = 48_000;
        let mut eq = PeakingEq::new(sample_rate, 1000.0, 1.0, 12.0);

        let n = sample_rate as usize;
        let mut sum_sq = 0.0;
        let mut count = 0;
        for i in 0..n {
            let t = i as f32 / sample_rate as f32;
            let x = (2.0 * std::f32::consts::PI * 1000.0 * t).sin();
            let y = eq.process_one(x);
            if i > n / 2 {
                sum_sq += y * y;
                count += 1;
            }
        }
        let rms = (sum_sq / count as f32).sqrt();
        // Unboosted sine has rms ~0.707; a +12 dB boost at the center
        // frequency should push it well above that.
        assert!(rms > 1.5, "expected boosted rms > 1.5, got {rms}");
    }

    impl PeakingEq {
        fn process_one(&mut self, x: f32) -> f32 {
            let mut buf = [x];
            self.process(&mut buf);
            buf[0]
        }
    }

    #[test]
    fn magnitude_db_matches_gain_at_center_and_fades_away_from_it() {
        let sample_rate = 48_000;
        let eq = PeakingEq::new(sample_rate, 1000.0, 1.0, 6.0);

        let at_center = eq.magnitude_db(sample_rate, 1000.0);
        assert!((at_center - 6.0).abs() < 0.5, "expected ~6dB at the center frequency, got {at_center}");

        let far_away = eq.magnitude_db(sample_rate, 50.0);
        assert!(far_away.abs() < 1.0, "expected ~0dB far from the center frequency, got {far_away}");
    }
}
