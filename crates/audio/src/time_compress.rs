//! Time compression for catch-up playback (spec §2: "recovered audio
//! plays slightly sped up so you rejoin live").
//!
//! The problem this solves: after a network stall the jitter buffer holds
//! a backlog. Playing it at normal speed drains nothing -- you hear every
//! word, but permanently a few hundred milliseconds behind, and a
//! conversation with that much added latency starts to collide. Playing
//! the backlog slightly fast gets the latency back without dropping any
//! audio.
//!
//! Why SOLA rather than resampling: resampling to 1.1x speed also raises
//! the pitch by about 160 cents, which turns a voice into a cartoon. SOLA
//! (synchronised overlap-add) shortens the signal by finding a *later*
//! point in the waveform that lines up with what was just played and
//! crossfading there, so periods are removed at waveform boundaries and
//! pitch is left alone. The search is what makes it synchronised: without
//! it the crossfades land at arbitrary phase and speech turns burbly.
//!
//! Deliberately narrow: this compresses, it does not stretch. Playing
//! *slower* than real time to smooth over a gap is what the jitter buffer
//! and concealment already do, and doing it in two places would fight.

/// Output produced per step, in milliseconds. Also the granularity at
/// which the speed can change.
const SYNTHESIS_HOP_MS: usize = 10;

/// Crossfade length, in milliseconds. Half the hop: long enough that a
/// splice is inaudible, short enough that two overlapping copies of the
/// signal do not smear a consonant.
const OVERLAP_MS: usize = 5;

/// How far ahead the search looks for a matching waveform, in
/// milliseconds. Ten covers a pitch period down to 100 Hz, which reaches
/// below any speaking voice -- a shorter search would fail to line up on
/// low voices, which is exactly when the artefacts are most audible.
const SEARCH_MS: usize = 10;

/// The speed range catch-up is allowed to use.
///
/// The ceiling is not a DSP limit -- SOLA copes with much more -- it is a
/// listening one. Past about 1.15x the rhythm of speech is audibly wrong,
/// and a listener notices the fix more than the problem it fixed.
pub const MIN_SPEED: f32 = 1.0;
pub const MAX_SPEED: f32 = 1.15;

/// Shortens a stream of audio without changing its pitch.
pub struct TimeCompressor {
    synthesis_hop: usize,
    overlap: usize,
    search: usize,
    /// Input received but not yet consumed.
    pending: Vec<f32>,
    /// The tail of what was last emitted, waiting to be crossfaded into
    /// whatever comes next.
    tail: Vec<f32>,
    /// False until the first segment has been emitted, since there is
    /// nothing to crossfade the very first samples against.
    started: bool,
}

impl TimeCompressor {
    pub fn new(sample_rate: u32) -> Self {
        let per_ms = (sample_rate.max(1000) as usize) / 1000;
        Self {
            synthesis_hop: SYNTHESIS_HOP_MS * per_ms,
            overlap: OVERLAP_MS * per_ms,
            search: SEARCH_MS * per_ms,
            pending: Vec::new(),
            tail: Vec::new(),
            started: false,
        }
    }

    /// Forgets everything buffered.
    ///
    /// Called when playback stops, so the next call does not start by
    /// crossfading into the last words of the previous one.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.tail.clear();
        self.started = false;
    }

    /// How much audio is held back waiting for enough input to splice.
    ///
    /// Bounded by construction -- a step needs `search + hop + overlap`
    /// samples and consumes at least `hop` -- so this never grows with the
    /// length of a call.
    pub fn buffered(&self) -> usize {
        self.pending.len()
    }

    /// Compresses `input` by `speed`, appending the result to `out`.
    ///
    /// A speed at or below 1.0 passes audio straight through: catch-up
    /// that is not needed should cost nothing and change nothing, not run
    /// every sample through a crossfade at unity.
    pub fn process(&mut self, input: &[f32], speed: f32, out: &mut Vec<f32>) {
        if speed <= MIN_SPEED {
            // Anything still buffered from a previous burst comes out
            // first, or it would be silently dropped.
            out.append(&mut self.pending);
            self.tail.clear();
            self.started = false;
            out.extend_from_slice(input);
            return;
        }
        let speed = speed.min(MAX_SPEED);
        self.pending.extend_from_slice(input);

        let analysis_hop = (self.synthesis_hop as f32 * speed).round() as usize;
        let need = self.search + self.synthesis_hop + self.overlap;

        while self.pending.len() >= need.max(analysis_hop) {
            let offset = if self.started {
                self.best_offset()
            } else {
                // Nothing to line up against yet.
                0
            };
            let segment = &self.pending[offset..];

            if self.started {
                for (i, (tail, new)) in self.tail.iter().zip(segment).take(self.overlap).enumerate() {
                    let w = (i + 1) as f32 / (self.overlap + 1) as f32;
                    out.push(tail * (1.0 - w) + new * w);
                }
                out.extend_from_slice(&segment[self.overlap..self.synthesis_hop]);
            } else {
                out.extend_from_slice(&segment[..self.synthesis_hop]);
                self.started = true;
            }

            self.tail.clear();
            self.tail
                .extend_from_slice(&segment[self.synthesis_hop..self.synthesis_hop + self.overlap]);

            // Consuming more input than was emitted is the whole
            // mechanism: `analysis_hop` in, `synthesis_hop` out.
            //
            // The search offset is deliberately *not* added here. It picks
            // where to read within this step, not how far the stream
            // advances; adding it would make the speed depend on what the
            // waveform happened to line up with, which is both wrong and
            // unpredictable.
            self.pending.drain(..analysis_hop.min(self.pending.len()));
        }
    }

    /// Where in `pending` the waveform best continues `tail`.
    ///
    /// Plain cross-correlation, not normalised: the two candidates are
    /// windows of the same signal moments apart, so their energies are
    /// comparable, and normalising would bias the choice toward quiet
    /// stretches where any splice is inaudible anyway.
    fn best_offset(&self) -> usize {
        let limit = self.search.min(self.pending.len().saturating_sub(
            self.synthesis_hop + self.overlap,
        ));
        let mut best = 0;
        let mut best_score = f32::NEG_INFINITY;
        for offset in 0..=limit {
            let candidate = &self.pending[offset..offset + self.overlap];
            let score: f32 =
                self.tail.iter().zip(candidate).map(|(a, b)| a * b).sum();
            if score > best_score {
                best_score = score;
                best = offset;
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(freq: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|i| (i as f32 * freq * std::f32::consts::TAU / RATE as f32).sin())
            .collect()
    }

    fn compress(input: &[f32], speed: f32) -> Vec<f32> {
        let mut c = TimeCompressor::new(RATE);
        let mut out = Vec::new();
        for block in input.chunks(960) {
            c.process(block, speed, &mut out);
        }
        out
    }

    /// Zero crossings per second, which tracks the fundamental of a clean
    /// sine and so detects a pitch shift.
    fn crossings_per_second(samples: &[f32]) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f32 * RATE as f32 / samples.len().max(1) as f32
    }

    #[test]
    fn normal_speed_changes_nothing_at_all() {
        // Catch-up that is not needed must cost nothing -- not run every
        // sample through a crossfade sitting at unity.
        let input = sine(220.0, 48_000);
        assert_eq!(compress(&input, 1.0), input);
    }

    #[test]
    fn a_speed_below_one_is_not_used_to_stretch() {
        // Playing slower to smooth a gap is the jitter buffer's job;
        // doing it here too would mean two mechanisms fighting.
        let input = sine(220.0, 9600);
        assert_eq!(compress(&input, 0.8), input);
    }

    #[test]
    fn faster_playback_produces_proportionally_less_audio() {
        let input = sine(220.0, 48_000 * 2);
        for speed in [1.05f32, 1.10, 1.15] {
            let out = compress(&input, speed);
            let expected = input.len() as f32 / speed;
            let error = (out.len() as f32 - expected).abs() / expected;
            assert!(error < 0.05, "speed {speed}: got {} want ~{expected}", out.len());
        }
    }

    #[test]
    fn the_search_does_not_secretly_speed_playback_up() {
        // The offset chooses where to read, not how far to advance. If it
        // also advanced the stream, the real speed would depend on what
        // the waveform lined up with -- here it compressed roughly twice
        // as hard as asked, and silently.
        let input = sine(220.0, 48_000);
        let gentle = compress(&input, 1.02).len() as f32;
        assert!(gentle > input.len() as f32 * 0.95, "1.02x removed {}%",
            100.0 - gentle * 100.0 / input.len() as f32);
    }

    #[test]
    fn speed_is_capped_rather_than_obeyed_without_limit() {
        // Past about 1.15x the rhythm of speech is audibly wrong. A caller
        // asking for 3x should get the cap, not a chipmunk.
        let input = sine(220.0, 48_000 * 2);
        let capped = compress(&input, MAX_SPEED).len() as f32;
        let absurd = compress(&input, 3.0).len() as f32;
        assert!((capped - absurd).abs() / capped < 0.02, "{capped} vs {absurd}");
    }

    #[test]
    fn the_pitch_is_left_alone() {
        // The reason this is SOLA and not a resampler. At 1.15x a
        // resampler would report ~253 Hz here instead of 220.
        let input = sine(220.0, 48_000 * 2);
        let out = compress(&input, 1.15);
        let before = crossings_per_second(&input);
        let after = crossings_per_second(&out);
        assert!((after - before).abs() / before < 0.05, "{before} Hz became {after} Hz");
    }

    #[test]
    fn a_low_voice_keeps_its_pitch_too() {
        // The search window has to reach a full pitch period. An 85 Hz
        // voice is where a too-short search would start splicing at
        // arbitrary phase and turn speech burbly.
        let input = sine(85.0, 48_000 * 2);
        let out = compress(&input, 1.12);
        let before = crossings_per_second(&input);
        let after = crossings_per_second(&out);
        assert!((after - before).abs() / before < 0.06, "{before} Hz became {after} Hz");
    }

    #[test]
    fn a_compressed_sine_stays_a_clean_sine() {
        // Length and pitch can both be right while the output is full of
        // splice discontinuities. Nothing should exceed the input's range
        // by more than the crossfade can explain.
        let input = sine(220.0, 48_000);
        let out = compress(&input, 1.1);
        let peak = out.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 1.05, "peak {peak} -- splices are adding energy");
    }

    #[test]
    fn silence_compresses_to_silence() {
        let out = compress(&vec![0.0; 48_000], 1.1);
        assert!(out.iter().all(|s| s.abs() < 1e-6));
    }

    #[test]
    fn what_is_held_back_does_not_grow_with_the_call() {
        // A step needs a bounded amount of input and consumes at least a
        // hop of it, so the backlog cannot creep upward over an hour.
        let mut c = TimeCompressor::new(RATE);
        let mut out = Vec::new();
        let block = sine(220.0, 960);
        let mut worst = 0;
        for _ in 0..500 {
            c.process(&block, 1.1, &mut out);
            worst = worst.max(c.buffered());
        }
        assert!(worst < RATE as usize / 10, "held back {worst} samples");
    }

    #[test]
    fn switching_back_to_normal_speed_does_not_swallow_buffered_audio() {
        // The backlog drains and catch-up switches off mid-stream. Audio
        // already handed in must still come out.
        let mut c = TimeCompressor::new(RATE);
        let mut out = Vec::new();
        c.process(&sine(220.0, 5000), 1.1, &mut out);
        let held = c.buffered();
        assert!(held > 0, "nothing was buffered, test proves nothing");
        c.process(&[], 1.0, &mut out);
        assert_eq!(c.buffered(), 0, "buffered audio was dropped on the way out");
    }

    #[test]
    fn a_reset_does_not_splice_the_next_call_onto_the_last_one() {
        let mut c = TimeCompressor::new(RATE);
        let mut out = Vec::new();
        c.process(&sine(220.0, 9600), 1.1, &mut out);
        c.reset();
        assert_eq!(c.buffered(), 0);

        let mut fresh = Vec::new();
        c.process(&sine(440.0, 9600), 1.1, &mut fresh);
        let mut reference = TimeCompressor::new(RATE);
        let mut expected = Vec::new();
        reference.process(&sine(440.0, 9600), 1.1, &mut expected);
        assert_eq!(fresh, expected);
    }

    #[test]
    fn odd_block_sizes_do_not_change_the_result() {
        // The network hands over whatever a frame happens to be; output
        // must not depend on how the input was cut up.
        let input = sine(220.0, 48_000);
        let mut a = TimeCompressor::new(RATE);
        let mut out_a = Vec::new();
        for block in input.chunks(960) {
            a.process(block, 1.1, &mut out_a);
        }
        let mut b = TimeCompressor::new(RATE);
        let mut out_b = Vec::new();
        for block in input.chunks(137) {
            b.process(block, 1.1, &mut out_b);
        }
        assert_eq!(out_a, out_b);
    }
}
