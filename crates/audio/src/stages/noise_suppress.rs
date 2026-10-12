//! Spectral noise suppression (spec §1 "Noise Suppression — strength
//! slider", plus its warning that high strength "can make voices sound
//! thin").
//!
//! **Not the RNNoise-class model the spec names.** That is a trained
//! neural network and this is classical spectral subtraction: estimate how
//! much of each frequency band is steady background, and turn those bands
//! down. It handles what a gate cannot -- a fan, a hiss, an air
//! conditioner running *underneath* speech rather than between sentences
//! -- which is the gap worth closing first. It will not remove a barking
//! dog, and it is the honest half of the feature, marked as such.
//!
//! The mechanism, and why each piece is there:
//!
//! - **Minimum statistics** for the noise floor. Steady noise is what a
//!   band's level falls back to between syllables, so the running minimum
//!   per band *is* the noise estimate -- no voice-activity detector, and
//!   nothing to get wrong during continuous speech.
//! - **Over-subtraction with a floor.** Subtracting exactly the estimate
//!   leaves audible musical noise -- isolated bins flickering above the
//!   threshold. Subtracting somewhat more and never attenuating past a
//!   floor trades that for a quiet residual hiss, which is far less
//!   distracting than warbling.
//! - **Smoothing across time.** A gain that jumps between frames is the
//!   other half of musical noise.
//!
//! The strength slider moves both the over-subtraction factor and the
//! floor, which is why high settings thin a voice out: speech and noise
//! share bands, so removing more noise removes more of the speech sitting
//! in the same place.

use super::Stage;

/// STFT frame size. 512 samples is ~11 ms at 48 kHz: long enough that a
/// band means something, short enough that the added latency (one frame)
/// stays under what anyone notices in a call.
const FRAME: usize = 512;

/// Half-frame hop, so each output sample is covered by exactly two
/// overlapping windows -- the condition that makes a Hann window
/// overlap-add back to unity.
const HOP: usize = FRAME / 2;

const BINS: usize = FRAME / 2 + 1;



/// The noise floor is the minimum each band reached over the last
/// `SUBWINDOWS * SUBWINDOW_FRAMES` frames -- about a second.
///
/// A *windowed* minimum, not an all-time one. An all-time minimum can only
/// ever fall, so a room that gets noisier is never tracked, and one that
/// gets quieter pins the estimate forever at whatever the quietest instant
/// was. Splitting the window into subwindows is what makes it cheap: each
/// keeps its own running minimum, and the floor is the smallest of the
/// completed ones, so nothing has to be re-scanned.
const SUBWINDOWS: usize = 8;
const SUBWINDOW_FRAMES: u32 = 24;

/// How hard the per-band power is smoothed before its minimum is taken,
/// falling and rising.
///
/// Smoothing at all is what makes minimum statistics work: the minimum of
/// a *fluctuating* band sits far below its mean -- for white noise, the
/// smallest of a second's worth of raw magnitudes is roughly a tenth of
/// the average -- so taking the minimum of the raw signal under-reads the
/// noise by an order of magnitude and leaves nearly all of it in place.
/// Measured, that version gave a slider spanning 3.8 dB to 6.3 dB end to
/// end, which is not a control.
///
/// **Asymmetric**, and that is the load-bearing part. Smoothing a band
/// symmetrically means its level cannot fall to the true floor during the
/// gaps between syllables -- it is still decaying from the last syllable
/// when the next one starts. The minimum then tracks the *speech*, and
/// the suppressor removes the voice. Measured: with no background noise
/// at all, a gated tone came out 74% quieter than it went in. Falling
/// fast fixes it, because a band dropping is direct evidence the floor is
/// lower, while a band rising is more likely to be speech.
const POWER_FALL: f32 = 0.5;
const POWER_RISE: f32 = 0.9;

/// What remains of the bias once the power is smoothed.
const MINIMUM_BIAS: f32 = 3.0;

/// Time smoothing of the per-band gain. A gain that jumps frame to frame
/// is what musical noise sounds like.
const GAIN_SMOOTHING: f32 = 0.6;

/// Strength above which the UI should warn about thinning (spec §1).
pub const THIN_VOICE_WARNING_STRENGTH: f32 = 0.7;

/// A minimal radix-2 FFT.
///
/// Hand-written rather than a dependency: it is thirty lines, this is the
/// only place in the codebase that needs one, and the alternative is
/// pulling a crate into an audio path for a textbook butterfly.
fn fft(re: &mut [f32], im: &mut [f32], inverse: bool) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let sign = if inverse { 1.0 } else { -1.0 };
        let angle = sign * std::f32::consts::TAU / len as f32;
        let (wr, wi) = (angle.cos(), angle.sin());
        let mut start = 0;
        while start < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (start + k, start + k + len / 2);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let next_cr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next_cr;
            }
            start += len;
        }
        len <<= 1;
    }

    if inverse {
        let scale = 1.0 / n as f32;
        for i in 0..n {
            re[i] *= scale;
            im[i] *= scale;
        }
    }
}

/// The square root of a Hann window, used on both analysis and synthesis.
///
/// Not Hann itself, which is the tempting choice and is wrong here. The
/// window is applied twice -- once going in, once coming out -- so what
/// has to sum to one across overlapping frames is the *square*. Hann
/// squared at 50% overlap does not: it oscillates between 0.5 and 1,
/// which amplitude-modulates the output at the frame rate. Squaring a
/// square root gives Hann, and Hann does sum to one at 50% overlap.
fn sqrt_hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos()).sqrt())
        .collect()
}

pub struct NoiseSuppressor {
    enabled: bool,
    /// 0..1. Zero is a passthrough, one is as hard as this will push.
    strength: f32,
    window: Vec<f32>,
    /// Input not yet formed into a frame.
    input: Vec<f32>,
    /// Finished output waiting to be handed back, plus the tail of the
    /// last frame still being overlap-added into.
    output: Vec<f32>,
    overlap: Vec<f32>,
    /// Per-band smoothed power, whose minimum is the noise estimate.
    smoothed_power: Vec<f32>,
    /// Per-band minimum for each completed subwindow, oldest first.
    subwindow_minima: Vec<Vec<f32>>,
    /// Per-band running minimum for the subwindow being filled.
    current_minimum: Vec<f32>,
    /// Frames into the current subwindow.
    subwindow_frames: u32,
    /// Per-band gain from the previous frame, for time smoothing.
    previous_gain: Vec<f32>,
    /// How much the last frame was pulled down, for the meter.
    reduction_db: f32,
    /// Frames seen, so the floor is not trusted before it has settled.
    frames: u32,
}

impl NoiseSuppressor {
    pub fn new(enabled: bool, strength: f32) -> Self {
        Self {
            enabled,
            strength: strength.clamp(0.0, 1.0),
            window: sqrt_hann(FRAME),
            input: Vec::with_capacity(FRAME * 2),
            // Primed with a hop of silence: the first frame cannot be
            // emitted until a full frame has arrived, and this is the
            // latency that buys, made explicit rather than appearing as a
            // dropout at the start of every call.
            output: vec![0.0; HOP],
            overlap: vec![0.0; FRAME],
            smoothed_power: vec![0.0; BINS],
            subwindow_minima: vec![vec![f32::MAX; BINS]; SUBWINDOWS],
            current_minimum: vec![f32::MAX; BINS],
            subwindow_frames: 0,
            previous_gain: vec![1.0; BINS],
            reduction_db: 0.0,
            frames: 0,
        }
    }

    /// The latency this adds, in samples. One hop.
    pub fn latency_samples() -> usize {
        HOP
    }

    /// Whether this strength is high enough to thin a voice out (spec §1).
    pub fn thins_voice(strength: f32) -> bool {
        strength >= THIN_VOICE_WARNING_STRENGTH
    }

    fn process_frame(&mut self, frame: &[f32]) {
        let mut re = vec![0.0f32; FRAME];
        let mut im = vec![0.0f32; FRAME];
        for i in 0..FRAME {
            re[i] = frame[i] * self.window[i];
        }
        fft(&mut re, &mut im, false);

        self.frames = self.frames.saturating_add(1);
        self.subwindow_frames += 1;
        let rotate = self.subwindow_frames >= SUBWINDOW_FRAMES;
        // Over-subtraction rises with strength; the floor falls. Both at
        // once is what makes high settings aggressive -- and thin.
        let over_subtract = 1.0 + 2.0 * self.strength;
        // Nearly transparent at the bottom of the slider and severe at the
        // top, so the control spans something worth having rather than
        // three decibels either side of "on".
        let gain_floor = 0.9 - 0.85 * self.strength;

        let mut input_energy = 0.0f32;
        let mut output_energy = 0.0f32;

        for bin in 0..BINS {
            let magnitude = (re[bin] * re[bin] + im[bin] * im[bin]).sqrt();

            // Minimum statistics, on the *smoothed* power: a band falls
            // back to the noise floor between syllables, so the minimum
            // over the last second is the estimate. Speech is modulated,
            // so its bands rise well above their own minimum; steady
            // noise does not, which is exactly the distinction being
            // drawn.
            let power = magnitude * magnitude;
            self.smoothed_power[bin] = if self.frames == 1 {
                power
            } else {
                let a = if power < self.smoothed_power[bin] { POWER_FALL } else { POWER_RISE };
                self.smoothed_power[bin] * a + power * (1.0 - a)
            };
            self.current_minimum[bin] = self.current_minimum[bin].min(self.smoothed_power[bin]);
            let windowed_minimum = self
                .subwindow_minima
                .iter()
                .map(|w| w[bin])
                .fold(self.current_minimum[bin], f32::min);
            let floor = if windowed_minimum.is_finite() {
                (windowed_minimum * MINIMUM_BIAS).sqrt()
            } else {
                0.0
            };

            let target = if magnitude > 1e-12 {
                ((magnitude - over_subtract * floor) / magnitude).max(gain_floor)
            } else {
                1.0
            };
            let gain = self.previous_gain[bin] * GAIN_SMOOTHING + target * (1.0 - GAIN_SMOOTHING);
            self.previous_gain[bin] = gain;

            input_energy += magnitude * magnitude;
            output_energy += (magnitude * gain) * (magnitude * gain);

            re[bin] *= gain;
            im[bin] *= gain;
            // The upper half of the spectrum mirrors the lower, and has to
            // stay a mirror or the inverse transform is not real-valued.
            if bin > 0 && bin < BINS - 1 {
                let mirror = FRAME - bin;
                re[mirror] = re[bin];
                im[mirror] = -im[bin];
            }
        }

        if rotate {
            self.subwindow_minima.rotate_left(1);
            let last = SUBWINDOWS - 1;
            self.subwindow_minima[last].copy_from_slice(&self.current_minimum);
            self.current_minimum.fill(f32::MAX);
            self.subwindow_frames = 0;
        }

        if input_energy > 1e-12 && output_energy > 0.0 {
            let instant = 10.0 * (input_energy / output_energy).log10();
            self.reduction_db = self.reduction_db * 0.8 + instant.clamp(0.0, 40.0) * 0.2;
        }

        fft(&mut re, &mut im, true);

        // Overlap-add. A Hann window applied on analysis and again on
        // synthesis sums to unity at 50% overlap, which is what makes the
        // seams inaudible.
        for i in 0..FRAME {
            self.overlap[i] += re[i] * self.window[i];
        }
        self.output.extend_from_slice(&self.overlap[..HOP]);
        self.overlap.copy_within(HOP.., 0);
        for value in &mut self.overlap[HOP..] {
            *value = 0.0;
        }
    }
}

impl Stage for NoiseSuppressor {
    fn name(&self) -> &'static str {
        "noise_suppressor"
    }

    fn process(&mut self, samples: &mut [f32]) {
        if !self.enabled || self.strength <= 0.0 {
            return;
        }
        self.input.extend_from_slice(samples);
        while self.input.len() >= FRAME {
            let frame: Vec<f32> = self.input[..FRAME].to_vec();
            self.process_frame(&frame);
            self.input.drain(..HOP);
        }

        // Hand back exactly as many samples as came in, so this looks like
        // any other stage to the chain. What it costs is one hop of
        // latency, paid once at the start rather than by dropping audio.
        for sample in samples.iter_mut() {
            *sample = if self.output.is_empty() { 0.0 } else { self.output.remove(0) };
        }
    }

    fn gain_reduction_db(&self) -> Option<f32> {
        Some(self.reduction_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn noise(n: usize, seed: u32, amplitude: f32) -> Vec<f32> {
        let mut state = seed | 1;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                ((state as f32 / u32::MAX as f32) * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    fn tone(n: usize, freq: f32, amplitude: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (i as f32 * freq * std::f32::consts::TAU / RATE).sin() * amplitude)
            .collect()
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum()
    }

    /// Runs `input` through in blocks and returns the output, skipping the
    /// first second so the noise floor has settled.
    fn run(input: &[f32], strength: f32) -> Vec<f32> {
        let mut ns = NoiseSuppressor::new(true, strength);
        let mut out = Vec::with_capacity(input.len());
        for block in input.chunks(480) {
            let mut buf = block.to_vec();
            ns.process(&mut buf);
            out.extend_from_slice(&buf);
        }
        out
    }

    #[test]
    fn the_analysis_and_synthesis_windows_sum_to_unity() {
        // The window is applied twice, so it is the *square* that has to
        // sum to one. Hann squared does not -- it oscillates between 0.5
        // and 1, amplitude-modulating the output at the frame rate --
        // which is why the window here is a square root.
        let w = sqrt_hann(FRAME);
        for n in 0..HOP {
            let summed = w[n] * w[n] + w[n + HOP] * w[n + HOP];
            assert!((summed - 1.0).abs() < 1e-4, "at {n}: {summed}");
        }
    }

    #[test]
    fn the_fft_inverts_itself() {
        // Everything below rests on this being right.
        let original = noise(512, 3, 1.0);
        let mut re = original.clone();
        let mut im = vec![0.0; 512];
        fft(&mut re, &mut im, false);
        fft(&mut re, &mut im, true);
        for (a, b) in re.iter().zip(&original) {
            assert!((a - b).abs() < 1e-4, "{a} != {b}");
        }
    }

    #[test]
    fn a_known_tone_lands_in_the_right_bin() {
        // A transform that inverts but puts energy in the wrong place
        // would suppress the wrong bands.
        let bin = 40;
        let n = 512;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (std::f32::consts::TAU * bin as f32 * i as f32 / n as f32).sin())
            .collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im, false);
        let magnitudes: Vec<f32> =
            (0..n / 2).map(|i| (re[i] * re[i] + im[i] * im[i]).sqrt()).collect();
        let peak = magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        assert_eq!(peak, bin);
    }

    /// How much steady noise a given strength removes, in dB, measured
    /// over the settled second -- the floor estimate has to see the noise
    /// before it can remove it.
    fn reduction_db(strength: f32) -> f32 {
        let input = noise(96_000, 11, 0.1);
        let out = run(&input, strength);
        10.0 * (energy(&input[48_000..]) / energy(&out[48_000..]).max(1e-12)).log10()
    }

    #[test]
    fn steady_background_noise_is_reduced() {
        let reduction = reduction_db(1.0);
        assert!(reduction > 15.0, "only {reduction:.1} dB of noise removed at full strength");
    }

    #[test]
    fn the_slider_spans_a_range_worth_having() {
        // A control that runs from "barely on" to "barely more on" is not
        // a control. The low end has to be nearly transparent and the top
        // end has to be worth reaching for.
        let gentle = reduction_db(0.1);
        let hard = reduction_db(1.0);
        assert!(gentle < 3.0, "the lowest setting already removes {gentle:.1} dB");
        assert!(hard - gentle > 15.0, "the whole slider spans {:.1} dB", hard - gentle);
    }

    /// A tone gated at a syllable rate -- the property that separates
    /// speech from a fan, and the one minimum statistics relies on.
    fn modulated_tone(n: usize, freq: f32, amplitude: f32) -> Vec<f32> {
        tone(n, freq, amplitude)
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let envelope = if (i / 6000) % 2 == 0 { 1.0 } else { 0.0 };
                s * envelope
            })
            .collect()
    }

    #[test]
    fn a_voice_over_noise_survives_better_than_the_noise() {
        // The point of the whole thing: it must remove more of the
        // background than of the thing you are trying to hear.
        let voice = modulated_tone(96_000, 300.0, 0.5);
        let background = noise(96_000, 5, 0.1);
        let mixed: Vec<f32> = voice.iter().zip(&background).map(|(t, n)| t + n).collect();

        let clean = run(&mixed, 0.5);
        let settled = 48_000..96_000;
        let kept = energy(&clean[settled.clone()]) / energy(&voice[settled]).max(1e-12);
        assert!(kept > 0.5, "the voice was cut to {:.0}% of itself", kept * 100.0);
    }

    #[test]
    fn a_sustained_tone_is_treated_as_noise_and_that_is_correct() {
        // Worth pinning down rather than leaving as a surprise: a
        // perfectly steady tone *is* steady background by every measure
        // this method has. Speech is modulated, which is the whole basis
        // for telling the two apart -- and why this is not the RNNoise
        // the spec asks for, which learns what a voice sounds like.
        let steady = tone(96_000, 300.0, 0.5);
        let out = run(&steady, 0.8);
        let kept = energy(&out[48_000..]) / energy(&steady[48_000..]);
        assert!(kept < 0.5, "a steady tone survived at {:.0}%", kept * 100.0);
    }

    #[test]
    fn zero_strength_is_a_true_passthrough() {
        // Someone who leaves the slider at zero should get their audio
        // back untouched -- not run through an STFT round trip that costs
        // latency and rounds every sample for nothing.
        let input = noise(4800, 9, 0.3);
        let mut ns = NoiseSuppressor::new(true, 0.0);
        let mut buf = input.clone();
        ns.process(&mut buf);
        assert_eq!(buf, input);
    }

    #[test]
    fn switched_off_is_a_true_passthrough() {
        let input = noise(4800, 9, 0.3);
        let mut ns = NoiseSuppressor::new(false, 1.0);
        let mut buf = input.clone();
        ns.process(&mut buf);
        assert_eq!(buf, input);
    }

    #[ignore]
    #[test]
    fn measure_the_slider_range() {
        let input = noise(96_000, 13, 0.1);
        for strength in [0.1f32, 0.25, 0.5, 0.75, 1.0] {
            let out = run(&input, strength);
            let db = 10.0
                * (energy(&input[48_000..]) / energy(&out[48_000..]).max(1e-12)).log10();
            println!("strength {strength:.2} -> {db:.1} dB");
        }
    }

    #[test]
    fn a_voice_with_no_background_passes_through_untouched() {
        // The bug this pins down: smoothing a band symmetrically means
        // its level cannot fall to the true floor during the gaps between
        // syllables, so the noise estimate tracks the *speech* and the
        // suppressor removes the voice. With no background at all there
        // is nothing to remove, and anything less than all of the voice
        // coming back means the estimate is contaminated by the signal.
        let voice = modulated_tone(96_000, 300.0, 0.5);
        let settled = 48_000..96_000;
        for strength in [0.05f32, 0.5, 1.0] {
            let out = run(&voice, strength);
            let kept = energy(&out[settled.clone()]) / energy(&voice[settled.clone()]);
            assert!(kept > 0.95, "strength {strength}: kept only {:.0}%", kept * 100.0);
        }
    }

    #[test]
    fn more_strength_removes_more() {
        let input = noise(48_000, 13, 0.1);
        let gentle = energy(&run(&input, 0.2)[24_000..]);
        let hard = energy(&run(&input, 0.9)[24_000..]);
        assert!(hard < gentle, "strength did nothing: {hard} vs {gentle}");
    }

    #[test]
    fn the_slider_warns_where_a_voice_starts_to_thin() {
        // Spec §1 asks for the warning specifically. It belongs next to
        // the same number that causes it, not in a note somewhere.
        assert!(!NoiseSuppressor::thins_voice(0.0));
        assert!(!NoiseSuppressor::thins_voice(0.5));
        assert!(NoiseSuppressor::thins_voice(THIN_VOICE_WARNING_STRENGTH));
        assert!(NoiseSuppressor::thins_voice(1.0));
    }

    #[test]
    fn it_returns_exactly_as_many_samples_as_it_was_given() {
        // The chain hands every stage a buffer and expects it back the
        // same length; a stage that grew or shrank one would corrupt
        // every stage after it.
        let mut ns = NoiseSuppressor::new(true, 0.5);
        for length in [1, 7, 128, 480, 960, 1024] {
            let mut buf = vec![0.25f32; length];
            ns.process(&mut buf);
            assert_eq!(buf.len(), length);
        }
    }

    #[test]
    fn the_latency_it_costs_is_stated() {
        // One hop, paid once at the start rather than by dropping audio.
        assert_eq!(NoiseSuppressor::latency_samples(), HOP);
        assert!(HOP as f32 / RATE < 0.01, "more than 10 ms would be felt in a call");
    }

    #[test]
    fn silence_stays_silent_rather_than_gaining_a_hiss() {
        let mut ns = NoiseSuppressor::new(true, 0.5);
        let mut out = Vec::new();
        for _ in 0..100 {
            let mut buf = vec![0.0f32; 480];
            ns.process(&mut buf);
            out.extend_from_slice(&buf);
        }
        assert!(out.iter().all(|s| s.abs() < 1e-5), "silence acquired something");
    }

    #[test]
    fn how_much_it_is_removing_is_reportable() {
        let mut ns = NoiseSuppressor::new(true, 0.7);
        assert_eq!(ns.gain_reduction_db(), Some(0.0), "nothing seen yet is nothing removed");
        for block in noise(48_000, 17, 0.1).chunks(480) {
            let mut buf = block.to_vec();
            ns.process(&mut buf);
        }
        assert!(ns.gain_reduction_db().unwrap() > 1.0);
    }

    #[test]
    fn the_output_never_goes_non_finite() {
        // An overlap-add that mismanages its buffers produces NaN, and a
        // NaN in an audio path is a click that propagates.
        let mut ns = NoiseSuppressor::new(true, 1.0);
        for block in noise(96_000, 23, 0.8).chunks(333) {
            let mut buf = block.to_vec();
            ns.process(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite()));
        }
    }
}
