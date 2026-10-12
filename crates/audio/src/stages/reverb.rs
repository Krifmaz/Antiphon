use super::Stage;

/// A damped feedback comb filter -- one of the parallel resonators that
/// give Freeverb-style reverbs their body. Each has a slightly different
/// delay length so their resonances don't line up and produce an obvious
/// pitched ring.
struct Comb {
    buffer: Vec<f32>,
    index: usize,
    feedback: f32,
    damping: f32,
    filter_store: f32,
}

impl Comb {
    fn new(delay_samples: usize, feedback: f32, damping: f32) -> Self {
        Self { buffer: vec![0.0; delay_samples.max(1)], index: 0, feedback, damping, filter_store: 0.0 }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let out = self.buffer[self.index];
        self.filter_store = out * (1.0 - self.damping) + self.filter_store * self.damping;
        self.buffer[self.index] = x + self.filter_store * self.feedback;
        self.index = (self.index + 1) % self.buffer.len();
        out
    }
}

/// A Schroeder allpass filter -- diffuses the comb filters' output into a
/// smoother, less "flangey" tail without coloring the tone.
struct AllPass {
    buffer: Vec<f32>,
    index: usize,
    feedback: f32,
}

impl AllPass {
    fn new(delay_samples: usize, feedback: f32) -> Self {
        Self { buffer: vec![0.0; delay_samples.max(1)], index: 0, feedback }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let buffered = self.buffer[self.index];
        let out = -x + buffered;
        self.buffer[self.index] = x + buffered * self.feedback;
        self.index = (self.index + 1) % self.buffer.len();
        out
    }
}

/// Adds a sense of space -- "mostly for fun" per spec §1's Creative stage.
/// A mono, scaled-down Freeverb: four parallel damped combs summed together,
/// then two series allpass filters for diffusion, mixed with the dry signal.
/// Classic Freeverb tuning, just fewer combs/allpasses than the original
/// (which was designed for stereo) since this chain is mono throughout.
pub struct Reverb {
    combs: [Comb; 4],
    allpasses: [AllPass; 2],
    wet: f32,
    dry: f32,
}

impl Reverb {
    /// `room_size` and `damping` are both 0..1; `wet_dry` is 0 (fully dry)
    /// to 1 (fully wet).
    pub fn new(sample_rate: u32, room_size: f32, damping: f32, wet_dry: f32) -> Self {
        // Freeverb's tuning constants are delay lengths in samples at 44.1kHz;
        // scale them so the same room character holds at other sample rates.
        let scale = sample_rate as f32 / 44_100.0;
        let comb_delays = [1116, 1188, 1277, 1356];
        let allpass_delays = [556, 441];

        let feedback = 0.7 + room_size.clamp(0.0, 1.0) * 0.28; // 0.70..0.98, stays stable (<1)
        let damping = damping.clamp(0.0, 1.0);

        let combs = comb_delays.map(|d| Comb::new((d as f32 * scale) as usize, feedback, damping));
        let allpasses = allpass_delays.map(|d| AllPass::new((d as f32 * scale) as usize, 0.5));

        let wet = wet_dry.clamp(0.0, 1.0);
        Self { combs, allpasses, wet, dry: 1.0 - wet }
    }
}

impl Stage for Reverb {
    fn name(&self) -> &'static str {
        "reverb"
    }

    fn process(&mut self, samples: &mut [f32]) {
        for s in samples.iter_mut() {
            let input = *s;
            let mut wet = self.combs.iter_mut().fold(0.0, |acc, comb| acc + comb.process(input));
            for allpass in self.allpasses.iter_mut() {
                wet = allpass.process(wet);
            }
            *s = input * self.dry + wet * 0.25 * self.wet;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fully_dry_passes_signal_through_unchanged() {
        let mut reverb = Reverb::new(48_000, 0.5, 0.5, 0.0);
        let mut buf = vec![0.5, -0.3, 0.8, -0.1];
        let reference = buf.clone();
        reverb.process(&mut buf);
        for (out, expected) in buf.iter().zip(reference.iter()) {
            assert!((out - expected).abs() < 1e-6, "dry mix should be an exact passthrough: {out} vs {expected}");
        }
    }

    #[test]
    fn an_impulse_produces_a_decaying_tail() {
        let sample_rate = 48_000;
        let mut reverb = Reverb::new(sample_rate, 0.8, 0.3, 1.0);
        let mut buf = vec![0.0f32; sample_rate as usize / 4];
        buf[0] = 1.0;
        reverb.process(&mut buf);

        // The tail well after the impulse should carry real energy (the
        // reverb is doing something), and it should be smaller than the
        // energy right after the impulse (it's decaying, not resonating
        // forever, which would mean the combs are unstable).
        let early_energy: f32 = buf[1..2000].iter().map(|s| s * s).sum();
        let late_energy: f32 = buf[10_000..12_000].iter().map(|s| s * s).sum();
        assert!(early_energy > 1e-6, "expected an audible tail right after the impulse");
        assert!(late_energy < early_energy, "the tail should decay, not sustain or grow");
    }
}
