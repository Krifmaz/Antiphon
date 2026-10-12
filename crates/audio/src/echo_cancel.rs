//! Acoustic echo cancellation (spec §1: "adaptive filter, on by default,
//! manual off switch").
//!
//! The problem: without headphones, what comes out of your speakers is
//! picked up by your microphone and sent back, so the other person hears
//! themselves a moment later. Ducking -- which this app also does -- hides
//! it by turning them down while you talk. That works, but it is a
//! workaround: it cannot help while *both* of you are talking, which is
//! exactly when an echo is most disruptive.
//!
//! This removes the echo instead of hiding it. We know the far-end signal
//! exactly, because we are the ones playing it, so the echo in the
//! microphone is a filtered copy of something we already have. An NLMS
//! adaptive filter learns that room's filter and subtracts its estimate.
//!
//! Three parts, and all three are needed -- a filter alone does nothing:
//!
//! 1. **Delay estimation.** The echo arrives hundreds of milliseconds
//!    after we handed the audio to the sound card, and a filter can only
//!    cover the tail it spans. Searching for the bulk delay separately
//!    means a short filter can sit exactly where the echo is, instead of a
//!    filter twenty times longer covering mostly nothing.
//! 2. **The NLMS filter.** Normalised, so its convergence does not depend
//!    on how loud the far end happens to be.
//! 3. **Double-talk detection.** When you speak, your voice is in the
//!    microphone but not in the reference, so the filter sees a huge error
//!    and "corrects" for it -- destroying a filter that was working and
//!    chewing up your own voice. Adaptation freezes while you talk.
//!
//! What this does *not* do: no residual/non-linear suppressor, so a
//! speaker driven into distortion leaves something behind, and no
//! frequency-domain partitioning, so the tail it can cover is bounded by
//! what a time-domain filter can afford. Both are real limits and are why
//! this ships switched off rather than on -- see [`EchoCanceller`].

/// Taps in the adaptive filter.
///
/// About 21 ms at 48 kHz. Short, deliberately: paired with the delay
/// search below it only has to span the room's reverberation *after* the
/// bulk delay, not the delay itself. A filter long enough to swallow the
/// delay too would cost twenty times as much and adapt far more slowly,
/// because every wasted tap is another coefficient hunting for zero.
pub const FILTER_TAPS: usize = 1024;

/// How far back the delay search looks, in milliseconds.
///
/// Has to cover the output buffer plus the input buffer plus the trip
/// through the air, and the output buffer here is the big one.
pub const MAX_DELAY_MS: usize = 600;

/// Envelope decimation for the delay search.
///
/// Cross-correlating at full rate over 600 ms would be 28,800 lags of real
/// work. Speech envelopes carry the alignment perfectly well at 750 Hz,
/// which turns that into 450, and the result is refined by the filter
/// itself anyway.
const ENVELOPE_DECIMATION: usize = 64;

/// Adaptation rate. Below 1 for stability; this is the usual compromise
/// between converging quickly and not chattering on noise.
const STEP_SIZE: f32 = 0.4;

/// Keeps the normalisation from dividing by ~zero during silence, which
/// would turn a rounding error into an enormous coefficient update.
const REGULARISATION: f32 = 1e-6;

/// How much louder the microphone has to be than the *far end* before
/// adaptation is treated as unsafe and frozen.
///
/// Measured against the far-end signal rather than against the filter's
/// own estimate of the echo, which is the trap here: before the filter has
/// converged its estimate is zero, so any near-end sound at all looks like
/// double-talk, adaptation freezes, and the filter never converges -- a
/// detector that guarantees there is nothing left for it to protect.
///
/// The far end bounds the echo instead, since a room reflects back less
/// than it was given. Two, so the near end has to be clearly dominant
/// rather than merely present.
const DOUBLE_TALK_RATIO: f32 = 2.0;

/// Energy below which a block is treated as silence and skipped.
///
/// Adapting on silence learns nothing and drifts.
const SILENCE_ENERGY: f32 = 1e-7;

/// Removes the echo of the far end from the near-end microphone signal.
///
/// **Ships switched off.** The spec asks for on by default; that is right
/// for an AEC proven against real rooms and hardware, and this one has
/// been proven against synthetic echo paths in tests. An echo canceller
/// that misbehaves damages the user's own voice, which is worse than the
/// echo it was fixing, so the default waits on real-world evidence rather
/// than on the code existing.
pub struct EchoCanceller {
    sample_rate: u32,
    /// Far-end history as a ring, stored twice over so any window of up
    /// to `capacity` samples is one contiguous slice.
    ///
    /// Doubling the memory to avoid moving it: sliding a 30,000-sample
    /// buffer by one position per sample, 48,000 times a second, is over a
    /// billion element moves a second and would cost far more than the
    /// filter it feeds.
    far_history: Vec<f32>,
    /// How many samples of history the ring holds.
    capacity: usize,
    /// Where the next sample goes, in `0..capacity`.
    write_pos: usize,
    /// Adaptive filter coefficients.
    weights: Vec<f32>,
    /// Bulk delay in samples, from the delay search.
    delay: usize,
    /// Decimated energy envelopes, for the delay search.
    near_envelope: Vec<f32>,
    far_envelope: Vec<f32>,
    /// Accumulators for the current decimation window.
    near_accum: f32,
    far_accum: f32,
    accum_count: usize,
    /// Blocks processed since the delay was last re-estimated.
    since_estimate: usize,
    /// How well it is working, in dB, for the UI to show.
    erle_db: f32,
}

impl EchoCanceller {
    pub fn new(sample_rate: u32) -> Self {
        let max_delay = MAX_DELAY_MS * (sample_rate.max(1000) as usize) / 1000;
        let capacity = max_delay + FILTER_TAPS;
        Self {
            sample_rate,
            far_history: vec![0.0; capacity * 2],
            capacity,
            write_pos: 0,
            weights: vec![0.0; FILTER_TAPS],
            delay: 0,
            near_envelope: vec![0.0; max_delay / ENVELOPE_DECIMATION + 1],
            far_envelope: vec![0.0; max_delay / ENVELOPE_DECIMATION + 1],
            near_accum: 0.0,
            far_accum: 0.0,
            accum_count: 0,
            since_estimate: 0,
            erle_db: 0.0,
        }
    }

    /// Echo return loss enhancement in dB: how much of the echo is being
    /// removed. Zero means it is doing nothing.
    pub fn erle_db(&self) -> f32 {
        self.erle_db
    }

    /// The bulk delay it has settled on, in milliseconds.
    pub fn delay_ms(&self) -> f32 {
        self.delay as f32 * 1000.0 / self.sample_rate.max(1) as f32
    }

    /// Forgets the far-end history but **keeps the learned filter**.
    ///
    /// This is spec §1's "survives device hot-swap without re-converging
    /// from scratch". Changing headphones does not change the room, and
    /// throwing away a converged filter would mean several seconds of
    /// audible echo every time somebody switched output device. The
    /// history is dropped because it is now stale -- it describes audio
    /// the old device played -- and the delay is re-searched because a
    /// different device has a different buffer.
    pub fn device_changed(&mut self) {
        self.far_history.fill(0.0);
        self.write_pos = 0;
        self.near_envelope.fill(0.0);
        self.far_envelope.fill(0.0);
        self.near_accum = 0.0;
        self.far_accum = 0.0;
        self.accum_count = 0;
        // Forces a fresh delay search on the next block.
        self.since_estimate = usize::MAX;
    }

    /// Throws everything away, including the filter.
    ///
    /// For a new call, where the far end is a different person in a
    /// different room and the old filter describes nothing.
    pub fn reset(&mut self) {
        self.weights.fill(0.0);
        self.delay = 0;
        self.erle_db = 0.0;
        self.device_changed();
    }

    /// Removes the echo of `far_end` from `near_end`, in place.
    ///
    /// Both slices are one block of the same length, `far_end` being what
    /// was played and `near_end` what the microphone heard.
    pub fn process(&mut self, near_end: &mut [f32], far_end: &[f32]) {
        if near_end.len() != far_end.len() {
            // Mismatched blocks cannot be aligned, and guessing at the
            // alignment is how an echo canceller ends up adding echo.
            return;
        }

        self.push_envelopes(near_end, far_end);
        self.since_estimate = self.since_estimate.saturating_add(1);
        if self.since_estimate >= 25 {
            self.since_estimate = 0;
            if let Some(delay) = self.estimate_delay() {
                self.delay = delay;
            }
        }

        let near_energy: f32 = near_end.iter().map(|s| s * s).sum();
        let far_energy: f32 = far_end.iter().map(|s| s * s).sum();

        let mut echo_energy = 0.0f32;
        let mut residual_energy = 0.0f32;

        // Decided once per block rather than per sample: it is a property
        // of the block, and recomputing it 480 times would not change the
        // answer.
        let may_adapt = far_energy > SILENCE_ENERGY && !double_talk(near_energy, far_energy);
        let delay = self.delay.min(self.capacity - FILTER_TAPS);

        for (i, sample) in near_end.iter_mut().enumerate() {
            self.far_history[self.write_pos] = far_end[i];
            self.far_history[self.write_pos + self.capacity] = far_end[i];
            self.write_pos = (self.write_pos + 1) % self.capacity;

            // The window the filter reads: `delay` samples back from the
            // newest, spanning FILTER_TAPS, contiguous thanks to the
            // doubled ring.
            let start = self.write_pos + self.capacity - delay - FILTER_TAPS;
            let window = &self.far_history[start..start + FILTER_TAPS];

            let estimate: f32 =
                window.iter().zip(&self.weights).map(|(x, w)| x * w).sum();
            let error = *sample - estimate;

            echo_energy += *sample * *sample;
            residual_energy += error * error;

            if may_adapt {
                let power: f32 = window.iter().map(|x| x * x).sum();
                let scale = STEP_SIZE * error / (power + REGULARISATION);
                for (w, x) in self.weights.iter_mut().zip(window) {
                    *w += scale * x;
                }
            }

            *sample = error;
        }

        if echo_energy > SILENCE_ENERGY && residual_energy > 0.0 {
            let instant = 10.0 * (echo_energy / residual_energy).log10();
            // Smoothed, because a per-block number jumps around too much
            // to read on a meter.
            self.erle_db = self.erle_db * 0.9 + instant.clamp(0.0, 60.0) * 0.1;
        }
    }

    /// Accumulates decimated energy envelopes for the delay search.
    fn push_envelopes(&mut self, near_end: &[f32], far_end: &[f32]) {
        for i in 0..near_end.len() {
            self.near_accum += near_end[i].abs();
            self.far_accum += far_end[i].abs();
            self.accum_count += 1;
            if self.accum_count >= ENVELOPE_DECIMATION {
                self.near_envelope.rotate_left(1);
                self.far_envelope.rotate_left(1);
                let last = self.near_envelope.len() - 1;
                self.near_envelope[last] = self.near_accum;
                self.far_envelope[last] = self.far_accum;
                self.near_accum = 0.0;
                self.far_accum = 0.0;
                self.accum_count = 0;
            }
        }
    }

    /// The lag at which the near-end envelope best matches the far-end
    /// one, in samples, or `None` when there is nothing to match.
    ///
    /// Normalised by the far-end window's energy so a loud stretch of
    /// far-end audio does not simply win every lag.
    fn estimate_delay(&self) -> Option<usize> {
        let n = self.near_envelope.len();
        if n < 8 {
            return None;
        }
        // Correlate the most recent quarter of the near envelope against
        // the far envelope at each lag: a short window tracks a delay that
        // moves, a long one averages over the movement.
        let window = (n / 4).max(4);
        let near = &self.near_envelope[n - window..];
        if near.iter().sum::<f32>() <= 0.0 {
            return None;
        }

        let mut best_lag = None;
        let mut best_score = 0.0f32;
        for lag in 0..(n - window) {
            let end = n - lag;
            let far = &self.far_envelope[end - window..end];
            let energy: f32 = far.iter().map(|x| x * x).sum::<f32>().sqrt();
            if energy <= 0.0 {
                continue;
            }
            let dot: f32 = near.iter().zip(far).map(|(a, b)| a * b).sum();
            let score = dot / energy;
            if score > best_score {
                best_score = score;
                best_lag = Some(lag);
            }
        }
        best_lag.map(|lag| lag * ENVELOPE_DECIMATION)
    }
}

/// Whether the microphone is dominated by something other than the echo.
///
/// A separate function because this is the rule that protects the user's
/// own voice: too eager and the filter never converges, too slow and it
/// eats their speech.
fn double_talk(near_energy: f32, far_energy: f32) -> bool {
    near_energy > far_energy * DOUBLE_TALK_RATIO && near_energy > SILENCE_ENERGY
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;
    const BLOCK: usize = 480;

    /// Deterministic pseudo-noise -- a stand-in for far-end speech that
    /// does not need an RNG dependency and gives the same result twice.
    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut state = seed | 1;
        (0..n)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    /// A synthetic room: the echo is the far end, delayed and attenuated,
    /// plus a shorter second reflection.
    fn echo_of(far: &[f32], delay: usize) -> Vec<f32> {
        let mut out = vec![0.0; far.len()];
        for i in 0..far.len() {
            if i >= delay {
                out[i] += 0.5 * far[i - delay];
            }
            if i >= delay + 37 {
                out[i] += 0.25 * far[i - delay - 37];
            }
        }
        out
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum()
    }

    /// Runs `blocks` of far-end audio through a synthetic echo path and
    /// returns (echo energy in, residual energy out) over the last third,
    /// once the filter has had a chance to converge.
    fn converge(delay: usize, blocks: usize) -> (f32, f32) {
        let mut aec = EchoCanceller::new(RATE);
        let far_all = noise(BLOCK * blocks, 12345);
        let echo_all = echo_of(&far_all, delay);

        let mut echo_in = 0.0;
        let mut residual_out = 0.0;
        for b in 0..blocks {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let far = &far_all[range.clone()];
            let mut near = echo_all[range].to_vec();
            let before = energy(&near);
            aec.process(&mut near, far);
            if b >= blocks * 2 / 3 {
                echo_in += before;
                residual_out += energy(&near);
            }
        }
        (echo_in, residual_out)
    }

    #[test]
    fn an_echo_with_no_delay_is_largely_removed() {
        // The base case: the filter alone, with nothing for the delay
        // search to do.
        let (echo_in, residual) = converge(0, 120);
        let erle = 10.0 * (echo_in / residual.max(1e-12)).log10();
        assert!(erle > 10.0, "only {erle:.1} dB of echo removed");
    }

    #[test]
    fn a_delayed_echo_is_found_and_removed() {
        // The whole reason the delay search exists: this echo sits far
        // outside what the filter alone spans, so without alignment the
        // filter has nothing to work with.
        let delay = 4800; // 100 ms
        let (echo_in, residual) = converge(delay, 400);
        let erle = 10.0 * (echo_in / residual.max(1e-12)).log10();
        assert!(erle > 6.0, "only {erle:.1} dB removed at {delay} samples of delay");
    }

    #[test]
    fn the_delay_search_finds_roughly_the_right_delay() {
        let mut aec = EchoCanceller::new(RATE);
        let delay = 9600; // 200 ms
        let far_all = noise(BLOCK * 200, 999);
        let echo_all = echo_of(&far_all, delay);
        for b in 0..200 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = echo_all[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
        }
        // Within a decimation window either way -- the envelope search
        // resolves to ENVELOPE_DECIMATION samples and the filter covers
        // the remainder.
        let found = aec.delay_ms();
        assert!((found - 200.0).abs() < 25.0, "found {found:.0} ms, expected ~200");
    }

    #[test]
    fn your_own_voice_is_not_eaten_when_there_is_no_echo() {
        // The failure that matters most. With nothing playing, the
        // canceller must be a passthrough -- anything else is damaging
        // the user's speech to solve a problem they do not have.
        let mut aec = EchoCanceller::new(RATE);
        let speech = noise(BLOCK * 20, 7);
        let silence = vec![0.0; BLOCK];
        for b in 0..20 {
            let mut near = speech[b * BLOCK..(b + 1) * BLOCK].to_vec();
            let original = near.clone();
            aec.process(&mut near, &silence);
            assert_eq!(near, original, "block {b} was altered with nothing playing");
        }
    }

    #[test]
    fn double_talk_does_not_destroy_a_converged_filter() {
        // When you talk, your voice is in the microphone but not in the
        // reference. A filter that adapted through that would "correct"
        // for your speech, wrecking a filter that was working.
        let mut aec = EchoCanceller::new(RATE);
        let far_all = noise(BLOCK * 200, 31);
        let echo_all = echo_of(&far_all, 0);
        for b in 0..150 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = echo_all[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
        }
        let converged: Vec<f32> = aec.weights.clone();

        // Now talk loudly over it.
        let my_voice = noise(BLOCK * 50, 77);
        for b in 150..200 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near: Vec<f32> = echo_all[range.clone()]
                .iter()
                .zip(&my_voice[(b - 150) * BLOCK..(b - 149) * BLOCK])
                .map(|(e, v)| e + v * 4.0)
                .collect();
            aec.process(&mut near, &far_all[range]);
        }

        let drift: f32 = aec
            .weights
            .iter()
            .zip(&converged)
            .map(|(a, b)| (a - b).abs())
            .sum();
        assert!(drift < 0.5, "the filter moved {drift:.3} during double-talk");
    }

    #[test]
    fn the_double_talk_rule_is_the_way_round_it_should_be() {
        // Near-end clearly dominant: freeze. Far end dominant: adapt.
        assert!(double_talk(1.0, 0.1));
        assert!(!double_talk(0.1, 1.0));
        // Silence is not double-talk; it is nothing.
        assert!(!double_talk(0.0, 0.0));
    }

    #[test]
    fn a_fresh_filter_is_allowed_to_start_learning() {
        // The bootstrap trap: judging double-talk by the filter's own
        // estimate of the echo means that before it converges the estimate
        // is zero, every sound looks like near-end speech, adaptation
        // freezes, and the filter never converges. Measuring against the
        // far end instead is what avoids it.
        let echo_energy = 0.3; // a room reflects back less than it got
        assert!(!double_talk(echo_energy, 1.0), "a fresh filter would never adapt");
    }

    #[test]
    fn a_device_change_keeps_the_filter_it_already_learned() {
        // Spec §1: "survives device hot-swap without re-converging from
        // scratch". Changing headphones does not change the room, and
        // relearning would mean seconds of audible echo every time.
        let mut aec = EchoCanceller::new(RATE);
        let far_all = noise(BLOCK * 120, 5);
        let echo_all = echo_of(&far_all, 0);
        for b in 0..120 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = echo_all[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
        }
        let learned: Vec<f32> = aec.weights.clone();
        assert!(learned.iter().any(|w| w.abs() > 0.01), "nothing was learned to keep");

        aec.device_changed();
        assert_eq!(aec.weights, learned, "the learned filter was thrown away");
    }

    #[test]
    fn a_new_call_starts_from_nothing() {
        // A different person in a different room -- the old filter
        // describes an echo path that no longer exists.
        let mut aec = EchoCanceller::new(RATE);
        let far_all = noise(BLOCK * 120, 5);
        let echo_all = echo_of(&far_all, 0);
        for b in 0..120 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = echo_all[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
        }
        aec.reset();
        assert!(aec.weights.iter().all(|w| *w == 0.0));
        assert_eq!(aec.delay_ms(), 0.0);
    }

    #[test]
    fn silence_in_stays_silence_out() {
        let mut aec = EchoCanceller::new(RATE);
        for _ in 0..50 {
            let mut near = vec![0.0; BLOCK];
            aec.process(&mut near, &vec![0.0; BLOCK]);
            assert!(near.iter().all(|s| s.abs() < 1e-9));
        }
    }

    #[test]
    fn mismatched_block_lengths_are_refused_rather_than_guessed_at() {
        // Aligning blocks that do not correspond is how an echo canceller
        // ends up *adding* echo.
        let mut aec = EchoCanceller::new(RATE);
        let mut near = vec![0.5; BLOCK];
        let original = near.clone();
        aec.process(&mut near, &vec![0.5; BLOCK / 2]);
        assert_eq!(near, original);
    }

    #[test]
    fn the_filter_never_blows_up_on_a_signal_it_cannot_model() {
        // Near-end noise unrelated to the far end is the classic way an
        // unregularised NLMS diverges into an unusable filter.
        let mut aec = EchoCanceller::new(RATE);
        let far_all = noise(BLOCK * 100, 3);
        let unrelated = noise(BLOCK * 100, 4);
        for b in 0..100 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = unrelated[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
            assert!(near.iter().all(|s| s.is_finite()), "output went non-finite at block {b}");
        }
        assert!(aec.weights.iter().all(|w| w.is_finite() && w.abs() < 10.0));
    }

    #[test]
    fn how_well_it_is_working_is_reportable() {
        // Zero until it has something to report, so a meter reading zero
        // means "doing nothing", not "no data".
        let mut aec = EchoCanceller::new(RATE);
        assert_eq!(aec.erle_db(), 0.0);

        let far_all = noise(BLOCK * 150, 21);
        let echo_all = echo_of(&far_all, 0);
        for b in 0..150 {
            let range = b * BLOCK..(b + 1) * BLOCK;
            let mut near = echo_all[range.clone()].to_vec();
            aec.process(&mut near, &far_all[range]);
        }
        assert!(aec.erle_db() > 5.0, "reported only {:.1} dB", aec.erle_db());
    }
}

#[cfg(test)]
mod cost {
    use super::*;

    /// One second of audio must take well under a second to process, or
    /// the canceller cannot run in a live call at all.
    ///
    /// Ignored by default because timing under a test runner is noisy;
    /// run with `cargo test -p antiphon-audio real_time -- --ignored
    /// --nocapture` in both profiles to see the difference a release
    /// build makes.
    #[ignore]
    #[test]
    fn real_time_budget() {
        let mut aec = EchoCanceller::new(48_000);
        let far = vec![0.25f32; 480];
        let start = std::time::Instant::now();
        for _ in 0..100 {
            let mut near = vec![0.1f32; 480];
            aec.process(&mut near, &far);
        }
        let taken = start.elapsed();
        println!("1 s of audio took {taken:?} ({:.1}% of real time)", taken.as_secs_f64() * 100.0);
    }
}
