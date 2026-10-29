//! Cutting a 16 kHz stream into utterances for the recogniser.
//!
//! The model transcribes a clip at a time, so live captions need clips:
//! start when someone starts talking, end when they pause, and hand over
//! what was said. An energy detector is enough for that -- the audio has
//! already been through the sender's noise suppression, so "loud relative
//! to the recent floor" is a good proxy for speech -- and it costs nothing
//! next to the model.
//!
//! Utterances are capped at [`MAX_SEGMENT_SECONDS`] so someone talking
//! without a break still gets captions every few seconds rather than one
//! paragraph at the end.

/// Analysis frame: 20 ms at 16 kHz.
pub const FRAME: usize = 320;
const FRAMES_PER_SECOND: usize = 16_000 / FRAME;

/// Consecutive loud frames needed to open an utterance: 60 ms, which a
/// click or a keyboard tap does not reach.
const OPEN_FRAMES: usize = 3;
/// Quiet frames that close an utterance: half a second, long enough to
/// ride over the gaps between words.
const CLOSE_FRAMES: usize = 25;
/// Audio kept from before the detector fired, so the first consonant is
/// not clipped off.
const PRE_ROLL_FRAMES: usize = 10;
/// Of the closing silence, how much stays on the end of the clip.
const TAIL_FRAMES: usize = 10;
pub const MAX_SEGMENT_SECONDS: usize = 6;
/// Shorter than this is a cough or a bump, not words worth the model.
const MIN_SPEECH_FRAMES: usize = 15;

/// How far above the noise floor counts as speech, in dB.
const MARGIN_DB: f32 = 12.0;
/// Never call anything quieter than this speech, however quiet the floor.
const ABSOLUTE_MIN_DB: f32 = -50.0;

pub struct Vad {
    pending: Vec<f32>,
    floor_db: f32,
    /// Recent frames while idle, for the pre-roll.
    history: std::collections::VecDeque<Vec<f32>>,
    /// The utterance being collected, if one is open.
    current: Option<Vec<f32>>,
    loud_run: usize,
    quiet_run: usize,
    speech_frames: usize,
}

impl Default for Vad {
    fn default() -> Self {
        Self {
            pending: Vec::new(),
            floor_db: -60.0,
            history: Default::default(),
            current: None,
            loud_run: 0,
            quiet_run: 0,
            speech_frames: 0,
        }
    }
}

impl Vad {
    /// Feeds 16 kHz audio; returns the utterances that ended in it.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        self.pending.extend_from_slice(samples);
        let mut done = Vec::new();
        let whole = self.pending.len() / FRAME * FRAME;
        let frames: Vec<f32> = self.pending.drain(..whole).collect();
        for frame in frames.chunks(FRAME) {
            if let Some(seg) = self.frame(frame) {
                done.push(seg);
            }
        }
        done
    }

    fn frame(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        let energy = frame.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
        let db = 10.0 * (energy + 1e-12).log10();
        let loud = db > (self.floor_db + MARGIN_DB).max(ABSOLUTE_MIN_DB);

        // The floor drops at once and rises gently through quiet frames.
        // Through loud ones it rises only very slowly (about 40 s): enough
        // that a fan switching on is eventually treated as background, not
        // so much that a long sentence drags it up to its own level.
        if db < self.floor_db {
            self.floor_db = db;
        } else if !loud {
            self.floor_db += (db - self.floor_db) * 0.05;
        } else {
            self.floor_db += (db - self.floor_db) * 0.0005;
        }

        match self.current.as_mut() {
            None => {
                self.history.push_back(frame.to_vec());
                if self.history.len() > PRE_ROLL_FRAMES {
                    self.history.pop_front();
                }
                self.loud_run = if loud { self.loud_run + 1 } else { 0 };
                if self.loud_run >= OPEN_FRAMES {
                    let mut seg = Vec::with_capacity(MAX_SEGMENT_SECONDS * 16_000);
                    for f in self.history.drain(..) {
                        seg.extend_from_slice(&f);
                    }
                    self.current = Some(seg);
                    self.quiet_run = 0;
                    self.speech_frames = self.loud_run;
                    self.loud_run = 0;
                }
                None
            }
            Some(seg) => {
                seg.extend_from_slice(frame);
                if loud {
                    self.quiet_run = 0;
                    self.speech_frames += 1;
                } else {
                    self.quiet_run += 1;
                }
                let too_long = seg.len() >= MAX_SEGMENT_SECONDS * FRAMES_PER_SECOND * FRAME;
                if self.quiet_run >= CLOSE_FRAMES || too_long {
                    let mut seg = self.current.take().unwrap();
                    let trailing = self.quiet_run.saturating_sub(TAIL_FRAMES) * FRAME;
                    seg.truncate(seg.len() - trailing.min(seg.len()));
                    let enough = self.speech_frames >= MIN_SPEECH_FRAMES;
                    self.quiet_run = 0;
                    self.speech_frames = 0;
                    // Cut mid-sentence: stay open, so the next words go
                    // straight into a fresh clip without waiting to re-arm.
                    if too_long && loud {
                        self.current = Some(Vec::with_capacity(MAX_SEGMENT_SECONDS * 16_000));
                    }
                    return enough.then_some(seg);
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Low background hiss, deterministic.
    fn hiss(n: usize, seed: &mut u32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.002
            })
            .collect()
    }

    /// Something voice-like: a pitched tone with a syllable-rate wobble.
    fn voice(seconds: f32) -> Vec<f32> {
        let n = (16_000.0 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 16_000.0;
                let env = 0.6 + 0.4 * (2.0 * std::f32::consts::PI * 4.0 * t).sin();
                0.2 * env * (2.0 * std::f32::consts::PI * 180.0 * t).sin()
            })
            .collect()
    }

    fn feed(vad: &mut Vad, signal: &[f32]) -> Vec<Vec<f32>> {
        signal.chunks(160).flat_map(|c| vad.push(c)).collect()
    }

    #[test]
    fn silence_gives_nothing() {
        let mut seed = 1;
        let mut vad = Vad::default();
        assert!(feed(&mut vad, &hiss(16_000 * 5, &mut seed)).is_empty());
    }

    #[test]
    fn one_utterance_between_silences() {
        let mut seed = 1;
        let mut signal = hiss(16_000, &mut seed);
        signal.extend(voice(1.5));
        signal.extend(hiss(16_000, &mut seed));
        let segs = feed(&mut Vad::default(), &signal);
        assert_eq!(segs.len(), 1);
        let secs = segs[0].len() as f32 / 16_000.0;
        // The speech, plus at most the pre-roll and the kept tail.
        assert!((1.5..=2.0).contains(&secs), "{secs} s");
    }

    #[test]
    fn a_pause_splits_two_utterances() {
        let mut seed = 2;
        let mut signal = hiss(8_000, &mut seed);
        signal.extend(voice(1.0));
        signal.extend(hiss(16_000, &mut seed));
        signal.extend(voice(1.0));
        signal.extend(hiss(16_000, &mut seed));
        assert_eq!(feed(&mut Vad::default(), &signal).len(), 2);
    }

    #[test]
    fn short_gap_between_words_does_not_split() {
        let mut seed = 3;
        let mut signal = hiss(8_000, &mut seed);
        signal.extend(voice(0.8));
        signal.extend(hiss(3_200, &mut seed)); // 200 ms
        signal.extend(voice(0.8));
        signal.extend(hiss(16_000, &mut seed));
        assert_eq!(feed(&mut Vad::default(), &signal).len(), 1);
    }

    #[test]
    fn nonstop_talking_is_cut_into_bounded_clips() {
        let mut seed = 4;
        let mut signal = hiss(8_000, &mut seed);
        signal.extend(voice(15.0));
        signal.extend(hiss(16_000, &mut seed));
        let segs = feed(&mut Vad::default(), &signal);
        assert!(segs.len() >= 3, "{}", segs.len());
        for s in &segs {
            assert!(s.len() <= MAX_SEGMENT_SECONDS * 16_000 + FRAME);
        }
        let total: usize = segs.iter().map(Vec::len).sum();
        assert!(total as f32 / 16_000.0 >= 14.5, "speech lost at the cuts");
    }

    #[test]
    fn a_click_is_not_an_utterance() {
        let mut seed = 5;
        let mut signal = hiss(16_000, &mut seed);
        signal.extend(voice(0.1));
        signal.extend(hiss(16_000, &mut seed));
        assert!(feed(&mut Vad::default(), &signal).is_empty());
    }
}
