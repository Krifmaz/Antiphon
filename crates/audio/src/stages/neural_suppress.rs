//! Neural noise suppression: the RNNoise-class model spec §1 names
//! ("Noise Suppression — RNNoise-class model, strength slider").
//!
//! This is RNNoise -- Xiph's recurrent network that predicts, per band,
//! how much of each 10 ms frame is voice -- through `nnnoiseless`, a pure
//! Rust port with the trained weights built in. Pure Rust matters here:
//! no C toolchain on Windows, nothing to ship beside the binary, and the
//! same code on every platform the app builds for.
//!
//! What it adds over the spectral suppressor beside it: the spectral one
//! can only remove what is *steady* (a fan, a hiss), because steady is how
//! it tells noise from voice. The model has learned what voice sounds
//! like, so it also takes out keyboard clatter, a dog, a door -- noise
//! that comes and goes.
//!
//! The strength slider blends the cleaned signal with the original,
//! aligned so the blend doesn't comb-filter. At full strength it is the
//! model's output untouched; lower settings keep some of the room in, for
//! anyone who finds a fully cleaned voice too processed.

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

use super::Stage;

/// The model works on 10 ms frames at 48 kHz, and nothing else.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The only rate the model was trained at.
pub const SAMPLE_RATE: u32 = 48_000;

/// The model takes 16-bit-scaled samples in f32.
const SCALE: f32 = 32_768.0;

pub struct NeuralSuppressor {
    state: Box<DenoiseState<'static>>,
    /// 0..1, how much of the cleaned signal replaces the original.
    strength: f32,
    input: Vec<f32>,
    output: VecDeque<f32>,
    /// The frame before this one, untouched. The model's output lags its
    /// input by one frame, so this is the original that lines up with it.
    previous_input: [f32; FRAME],
    scratch_in: [f32; FRAME],
    scratch_out: [f32; FRAME],
    /// The first frame out of the model fades in; it is dropped.
    warmed_up: bool,
    reduction_db: f32,
    /// Smoothed voice probability from the model, 0..1.
    voice_probability: f32,
}

impl NeuralSuppressor {
    pub fn new(strength: f32) -> Self {
        Self {
            state: DenoiseState::new(),
            strength: strength.clamp(0.0, 1.0),
            input: Vec::with_capacity(FRAME * 2),
            // Primed with one frame of silence so every call hands back as
            // many samples as it was given; this is the stage's latency.
            output: std::iter::repeat_n(0.0, FRAME).collect(),
            previous_input: [0.0; FRAME],
            scratch_in: [0.0; FRAME],
            scratch_out: [0.0; FRAME],
            warmed_up: false,
            reduction_db: 0.0,
            voice_probability: 0.0,
        }
    }

    /// Whether the model can run at this rate. The chain falls back to the
    /// spectral suppressor when it can't.
    pub fn supports(sample_rate: u32) -> bool {
        sample_rate == SAMPLE_RATE
    }

    /// The latency this adds, in samples: the buffered frame, plus the
    /// frame the model itself holds back.
    pub fn latency_samples() -> usize {
        2 * FRAME
    }

    /// How sure the model is that someone is speaking, 0..1.
    pub fn voice_probability(&self) -> f32 {
        self.voice_probability
    }

    fn process_frame(&mut self) {
        for (s, x) in self.scratch_in.iter_mut().zip(&self.input[..FRAME]) {
            *s = x * SCALE;
        }
        let vad = self.state.process_frame(&mut self.scratch_out, &self.scratch_in);
        self.voice_probability = 0.8 * self.voice_probability + 0.2 * vad;

        if !self.warmed_up {
            self.warmed_up = true;
            self.output.extend(std::iter::repeat_n(0.0, FRAME));
        } else {
            let wet = self.strength;
            let dry = 1.0 - wet;
            let (mut energy_in, mut energy_out) = (0.0f32, 0.0f32);
            for (cleaned, original) in self.scratch_out.iter().zip(&self.previous_input) {
                let y = (cleaned / SCALE) * wet + original * dry;
                energy_in += original * original;
                energy_out += y * y;
                self.output.push_back(y);
            }
            self.reduction_db = if energy_in > 1e-9 {
                (10.0 * (energy_out.max(1e-12) / energy_in).log10()).min(0.0)
            } else {
                0.0
            };
        }
        self.previous_input.copy_from_slice(&self.input[..FRAME]);
        self.input.drain(..FRAME);
    }
}

impl Stage for NeuralSuppressor {
    fn name(&self) -> &'static str {
        "neural_noise_suppressor"
    }

    fn process(&mut self, samples: &mut [f32]) {
        if self.strength <= 0.0 {
            return;
        }
        self.input.extend_from_slice(samples);
        while self.input.len() >= FRAME {
            self.process_frame();
        }
        for sample in samples.iter_mut() {
            *sample = self.output.pop_front().unwrap_or(0.0);
        }
    }

    fn gain_reduction_db(&self) -> Option<f32> {
        Some(self.reduction_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u32, amplitude: f32) -> Vec<f32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn energy(s: &[f32]) -> f32 {
        s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32
    }

    fn run(stage: &mut NeuralSuppressor, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len());
        // Odd block size on purpose: the device rarely hands over whole frames.
        for chunk in input.chunks(256) {
            let mut block = chunk.to_vec();
            stage.process(&mut block);
            out.extend(block);
        }
        out
    }

    #[test]
    fn hands_back_as_many_samples_as_it_was_given() {
        let mut s = NeuralSuppressor::new(1.0);
        for n in [1, 7, 480, 1000, 33] {
            let mut block = vec![0.1; n];
            s.process(&mut block);
            assert_eq!(block.len(), n);
        }
    }

    #[test]
    fn removes_most_of_a_steady_hiss() {
        let mut s = NeuralSuppressor::new(1.0);
        let input = noise(48_000 * 2, 7, 0.05);
        let out = run(&mut s, &input);
        // Judge the second second, once the model has settled.
        let before = energy(&input[48_000..]);
        let after = energy(&out[48_000..]);
        let removed_db = 10.0 * (before / after.max(1e-12)).log10();
        assert!(removed_db > 10.0, "removed only {removed_db:.1} dB");
    }

    #[test]
    fn zero_strength_is_a_passthrough() {
        let mut s = NeuralSuppressor::new(0.0);
        let input = noise(4_800, 3, 0.2);
        assert_eq!(run(&mut s, &input), input);
    }

    #[test]
    fn the_dry_signal_lines_up_with_the_cleaned_one() {
        // At half strength, a clean low tone should come out at roughly the
        // level it went in. Misaligned, the two halves would partly cancel.
        let tone: Vec<f32> =
            (0..48_000).map(|i| 0.3 * (std::f32::consts::TAU * 200.0 * i as f32 / 48_000.0).sin()).collect();
        let mut half = NeuralSuppressor::new(0.5);
        let mut zero_wet = NeuralSuppressor::new(1e-6);
        let a = run(&mut half, &tone);
        let b = run(&mut zero_wet, &tone);
        let delay = NeuralSuppressor::latency_samples();
        // The near-dry run is the original delayed by the stage latency.
        let err: f32 = b[delay + 4_800..]
            .iter()
            .zip(&tone[4_800..])
            .map(|(x, y)| (x - y).abs())
            .fold(0.0, f32::max);
        assert!(err < 1e-3, "dry path misaligned by more than rounding: {err}");
        assert!(energy(&a[9_600..]) > 0.2 * energy(&tone[9_600..]));
    }
}
