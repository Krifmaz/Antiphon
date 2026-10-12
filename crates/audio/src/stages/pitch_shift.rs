use super::formant::FormantCorrector;
use super::Stage;

/// Raises or lowers voice pitch (spec §1 Creative stage). A dual-tap
/// granular delay-line shifter: two overlapping read "grains" chase a
/// circular write buffer at a different rate than real time, crossfaded
/// with a Hann window so neither grain's wraparound is audible. This is
/// the standard cheap real-time pitch-shift technique -- no FFT, just a
/// delay line -- and it's what spec's "chipmunk/demon" description is
/// naming: formants shift along with pitch, since that's what this
/// technique does.
///
/// Set `preserve_formants` and a [`FormantCorrector`] runs afterwards,
/// transferring the pre-shift spectral envelope back onto the result --
/// that's the difference between "chipmunk" and "same person, different
/// pitch". See that module for how it works and what it trades off.
pub struct PitchShift {
    buffer: Vec<f32>,
    write_index: usize,
    phase: f32,
    rate: f32,
    formant: Option<FormantCorrector>,
    /// The pre-shift signal, needed as the envelope reference. Kept as a
    /// reused field rather than allocated per buffer, since `process` runs
    /// on the audio thread where an allocation is a real hazard.
    dry: Vec<f32>,
}

const GRAIN_MS: f32 = 60.0;

impl PitchShift {
    pub fn new(sample_rate: u32, semitones: f32, preserve_formants: bool) -> Self {
        let grain_size = ((GRAIN_MS / 1000.0) * sample_rate as f32).round().max(4.0) as usize;
        Self {
            buffer: vec![0.0; grain_size],
            write_index: 0,
            phase: 0.0,
            rate: semitones_to_rate(semitones),
            formant: preserve_formants.then(|| FormantCorrector::new(sample_rate)),
            dry: Vec::new(),
        }
    }

    pub fn set_semitones(&mut self, semitones: f32) {
        self.rate = semitones_to_rate(semitones);
    }

    #[inline]
    fn read_interpolated(&self, pos: f32) -> f32 {
        let n = self.buffer.len() as f32;
        let pos = pos.rem_euclid(n);
        // `rem_euclid` mathematically lands in [0, n), but f32 rounding can
        // still produce exactly `n` (e.g. n - epsilon rounds up at this
        // magnitude) -- guard with `%` rather than trusting the float math.
        let i0 = (pos as usize) % self.buffer.len();
        let i1 = (i0 + 1) % self.buffer.len();
        let frac = pos - pos.floor();
        self.buffer[i0] * (1.0 - frac) + self.buffer[i1] * frac
    }

    #[inline]
    fn grain_output(&self, phase: f32) -> f32 {
        let n = self.buffer.len() as f32;
        let read_pos = self.write_index as f32 - phase * n;
        let window = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * phase).cos();
        self.read_interpolated(read_pos) * window
    }
}

fn semitones_to_rate(semitones: f32) -> f32 {
    2f32.powf(semitones / 12.0)
}

impl Stage for PitchShift {
    fn name(&self) -> &'static str {
        "pitch_shift"
    }

    fn process(&mut self, samples: &mut [f32]) {
        if self.formant.is_some() {
            self.dry.clear();
            self.dry.extend_from_slice(samples);
        }

        let n = self.buffer.len() as f32;
        // read_pos = write_index - phase*n, and write_index advances by 1
        // each sample, so d(read_pos)/dt = 1 - phase_step*n. Setting that
        // equal to the target playback rate gives phase_step = (1-rate)/n
        // -- the sign that makes read_pos actually move at `rate` (not
        // `2 - rate`, which is what the opposite sign works out to).
        let phase_step = (1.0 - self.rate) / n;

        for s in samples.iter_mut() {
            self.buffer[self.write_index] = *s;

            let phase2 = (self.phase + 0.5).fract();
            let out = self.grain_output(self.phase) + self.grain_output(phase2);

            self.write_index = (self.write_index + 1) % self.buffer.len();
            self.phase = (self.phase + phase_step).rem_euclid(1.0);

            *s = out;
        }

        if let Some(formant) = self.formant.as_mut() {
            formant.process(&self.dry, samples);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq_hz: f32, sample_rate: u32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * std::f32::consts::PI * freq_hz * i as f32 / sample_rate as f32).sin()).collect()
    }

    /// Counts zero crossings as a cheap, robust stand-in for "what
    /// frequency is this": a shifted-up tone crosses zero more often per
    /// second than the original.
    fn zero_crossings(samples: &[f32]) -> usize {
        samples.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count()
    }

    #[test]
    fn shifting_up_raises_the_perceived_frequency() {
        let sample_rate = 48_000;
        let mut shift = PitchShift::new(sample_rate, 12.0, false); // one octave up
        let mut buf = sine(220.0, sample_rate, sample_rate as usize);
        let original_crossings = zero_crossings(&buf[sample_rate as usize / 2..]);

        shift.process(&mut buf);
        // Skip the first grain or two while the delay line is still filling.
        let shifted_crossings = zero_crossings(&buf[sample_rate as usize / 2..]);

        assert!(
            shifted_crossings > original_crossings * 3 / 2,
            "an octave up should roughly double zero-crossing rate: {shifted_crossings} vs original {original_crossings}"
        );
    }

    #[test]
    fn formant_preserving_shift_still_raises_pitch_and_stays_finite() {
        let sample_rate = 48_000;
        let mut shift = PitchShift::new(sample_rate, 12.0, true);
        let mut buf = sine(220.0, sample_rate, sample_rate as usize);
        let original_crossings = zero_crossings(&buf[sample_rate as usize / 2..]);

        shift.process(&mut buf);

        assert!(buf.iter().all(|s| s.is_finite()), "formant correction must not produce NaN or inf");
        // Formant correction reshapes the spectrum but must not undo the
        // pitch shift itself -- the whole point is pitch moves, formants don't.
        let shifted_crossings = zero_crossings(&buf[sample_rate as usize / 2..]);
        assert!(
            shifted_crossings > original_crossings * 5 / 4,
            "pitch should still be raised with formant correction on: {shifted_crossings} vs {original_crossings}"
        );
    }

    #[test]
    fn zero_semitones_is_near_unity() {
        let sample_rate = 48_000;
        let mut shift = PitchShift::new(sample_rate, 0.0, false);
        let mut buf = sine(220.0, sample_rate, sample_rate as usize);
        let reference = buf.clone();

        shift.process(&mut buf);

        // With rate == 1.0 the two grains together should reconstruct the
        // input almost exactly (mod a little delay-line settling and
        // windowing ripple), not silence or noise.
        let settle = sample_rate as usize / 2;
        let energy_in: f32 = reference[settle..].iter().map(|s| s * s).sum();
        let energy_out: f32 = buf[settle..].iter().map(|s| s * s).sum();
        assert!(energy_out > energy_in * 0.5, "unity pitch shift shouldn't collapse signal energy");
    }
}
