//! Replay history (spec §2: "last N recovered clips, replayable
//! manually").
//!
//! FEC and concealment repair a damaged moment before anyone could be
//! asked about it, which is the right behaviour -- holding audio back to
//! offer a popup would defeat the layer that repaired it. But a repair is
//! not always a *good* repair: a long concealed run is the last good frame
//! repeated, and what you actually heard may have been mush. This is the
//! answer to "wait, what did they just say?" -- the few seconds around
//! each damaged moment, kept so you can play them back deliberately.
//!
//! Two decisions worth stating, because both are easy to get wrong:
//!
//! - A clip is cut *after* the damage settles, not when it starts. The
//!   useful clip includes what came after -- a sentence cut off at the
//!   moment it broke is not something you can make sense of.
//! - Clips are capped and in memory only, like the ring they come from.
//!   Writing them to disk would turn a repair aid into a call recording,
//!   which is a different feature with different consent implications.

use std::collections::VecDeque;

/// How many clips are kept. Small on purpose: this is a "what did they
/// just say" aid, not an archive, and each clip is uncompressed f32.
pub const MAX_CLIPS: usize = 8;

/// Audio kept before the damage, in milliseconds.
///
/// Longer than the tail because speech is understood forwards: hearing the
/// run-up to a broken word is usually what makes it recoverable, while a
/// short tail is enough to know the sentence resumed.
pub const LEAD_IN_MS: usize = 1500;

/// How many clean frames must pass before the damage counts as over.
///
/// Without this, one burst of loss with a single good frame in the middle
/// becomes two clips of nearly the same moment.
pub const SETTLE_FRAMES: u32 = 40;

/// Audio kept after the damage, in milliseconds.
///
/// Derived from the settle window rather than set independently: the tail
/// *is* the settle audio, and the two drifting apart would mean either
/// asking for a tail that was never recorded or throwing away recorded
/// audio to honour a shorter number.
pub const TAIL_MS: usize = SETTLE_FRAMES as usize * 20;

/// A damaged run long enough to be worth keeping.
///
/// A single repaired frame is 20 ms that FEC almost certainly fixed
/// perfectly. Cutting a clip for it would fill the panel with entries
/// nobody needs to hear.
pub const MIN_DAMAGED_FRAMES: u32 = 2;

/// One kept moment.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// Seconds into the call when the damage happened.
    pub at_seconds: u32,
    /// How many frames were repaired or concealed.
    pub damaged_frames: u32,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

impl Clip {
    /// What the panel shows for this clip.
    pub fn label(&self) -> String {
        let minutes = self.at_seconds / 60;
        let seconds = self.at_seconds % 60;
        let ms = self.damaged_frames as usize * 20;
        format!("{minutes}:{seconds:02} -- {ms} ms repaired")
    }
}

/// Decides when a damaged run is over and a clip should be cut.
///
/// A state machine rather than inline flags in the drain loop, so the
/// rules can be tested without an audio device or a socket.
#[derive(Default, Debug)]
pub struct ClipCutter {
    damaged_frames: u32,
    clean_run: u32,
    in_damage: bool,
}

impl ClipCutter {
    /// Feeds one popped frame in. Returns `Some(damaged_frame_count)` on
    /// the frame where a clip should be cut.
    pub fn observe(&mut self, damaged: bool) -> Option<u32> {
        if damaged {
            self.in_damage = true;
            self.damaged_frames += 1;
            self.clean_run = 0;
            return None;
        }
        if !self.in_damage {
            return None;
        }
        self.clean_run += 1;
        if self.clean_run < SETTLE_FRAMES {
            return None;
        }
        let damaged = std::mem::take(&mut self.damaged_frames);
        self.in_damage = false;
        self.clean_run = 0;
        (damaged >= MIN_DAMAGED_FRAMES).then_some(damaged)
    }
}

/// Where in a replay ring of `ring_len` samples a clip sits.
///
/// Returned as a range rather than a copy, so the caller does the one
/// allocation and this stays testable with nothing but a length.
///
/// The clip runs to the end of the ring, because by the time a clip is
/// cut the settle window has already recorded exactly the tail.
pub fn clip_bounds(
    ring_len: usize,
    sample_rate: u32,
    damaged_frames: u32,
) -> std::ops::Range<usize> {
    let per_ms = sample_rate as usize / 1000;
    let damage = damaged_frames as usize * 20 * per_ms;
    // The ring's end: the settle window that delayed this cut is exactly
    // the tail, so there is nothing after the clip to trim off.
    let end = ring_len;
    let want = LEAD_IN_MS * per_ms + damage + TAIL_MS * per_ms;
    let start = end.saturating_sub(want);
    start..end
}

/// The kept clips, newest first.
#[derive(Default, Debug)]
pub struct ReplayHistory {
    clips: VecDeque<Clip>,
    /// Clips ever kept on this call, including ones since dropped. The
    /// replay modes watch this move to know a new moment needs offering.
    cut: u64,
}

impl ReplayHistory {
    pub fn push(&mut self, clip: Clip) {
        self.clips.push_front(clip);
        self.cut += 1;
        while self.clips.len() > MAX_CLIPS {
            self.clips.pop_back();
        }
    }

    pub fn clips(&self) -> impl Iterator<Item = &Clip> {
        self.clips.iter()
    }

    pub fn get(&self, index: usize) -> Option<&Clip> {
        self.clips.get(index)
    }

    pub fn cut(&self) -> u64 {
        self.cut
    }

    pub fn clear(&mut self) {
        self.clips.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cutter: &mut ClipCutter, damaged: bool, frames: u32) -> Vec<u32> {
        (0..frames).filter_map(|_| cutter.observe(damaged)).collect()
    }

    #[test]
    fn a_clean_call_never_cuts_a_clip() {
        let mut cutter = ClipCutter::default();
        assert!(run(&mut cutter, false, 1000).is_empty());
    }

    #[test]
    fn a_damaged_run_is_cut_once_it_settles() {
        let mut cutter = ClipCutter::default();
        assert!(run(&mut cutter, true, 5).is_empty(), "cut before the damage ended");
        assert_eq!(run(&mut cutter, false, SETTLE_FRAMES), vec![5]);
    }

    #[test]
    fn one_repaired_frame_is_not_worth_a_clip() {
        // FEC almost certainly fixed a single frame perfectly. Cutting for
        // it would fill the panel with entries nobody needs to hear.
        let mut cutter = ClipCutter::default();
        run(&mut cutter, true, 1);
        assert!(run(&mut cutter, false, SETTLE_FRAMES * 2).is_empty());
    }

    #[test]
    fn a_burst_with_a_gap_in_it_is_one_clip_not_two() {
        // This is what the settle window is for: bad network produces
        // damage-clean-damage, and three clips of the same two seconds is
        // worse than one.
        let mut cutter = ClipCutter::default();
        run(&mut cutter, true, 3);
        assert!(run(&mut cutter, false, SETTLE_FRAMES - 1).is_empty());
        run(&mut cutter, true, 3);
        assert_eq!(run(&mut cutter, false, SETTLE_FRAMES), vec![6]);
    }

    #[test]
    fn damage_that_never_settles_never_cuts() {
        // A call that is broken end to end should not produce a clip every
        // settle window -- there is no good audio to put in one.
        let mut cutter = ClipCutter::default();
        let mut cuts = Vec::new();
        for i in 0..2000 {
            cuts.extend(cutter.observe(i % 3 != 0));
        }
        assert!(cuts.is_empty(), "cut {} clips from unbroken damage", cuts.len());
    }

    #[test]
    fn the_counter_resets_so_clips_do_not_grow_forever() {
        let mut cutter = ClipCutter::default();
        run(&mut cutter, true, 4);
        assert_eq!(run(&mut cutter, false, SETTLE_FRAMES), vec![4]);
        run(&mut cutter, true, 4);
        assert_eq!(run(&mut cutter, false, SETTLE_FRAMES), vec![4], "damage carried over");
    }

    #[test]
    fn a_clip_covers_the_damage_plus_context_either_side() {
        let rate = 48_000;
        let per_ms = rate as usize / 1000;
        let bounds = clip_bounds(rate as usize * 30, rate, 5);
        assert_eq!(bounds.len(), (LEAD_IN_MS + 5 * 20 + TAIL_MS) * per_ms);
    }

    #[test]
    fn the_tail_is_audio_that_was_actually_recorded() {
        // The tail can only come from the settle window -- asking for more
        // than that would silently return a shorter clip than the bounds
        // claim, and the two numbers drifting apart is exactly the bug
        // this pins down.
        assert!(TAIL_MS <= SETTLE_FRAMES as usize * 20);
    }

    #[test]
    fn a_clip_early_in_a_call_is_shorter_rather_than_out_of_bounds() {
        // Damage in the first second has no lead-in to take. The range
        // must stay inside the ring instead of underflowing.
        let rate = 48_000;
        for ring in [0, 100, rate as usize / 2, rate as usize] {
            let bounds = clip_bounds(ring, rate, 3);
            assert!(bounds.end <= ring, "end {} past ring {ring}", bounds.end);
            assert!(bounds.start <= bounds.end, "inverted range for ring {ring}");
        }
    }

    #[test]
    fn history_keeps_the_newest_and_drops_the_oldest() {
        let mut history = ReplayHistory::default();
        for i in 0..MAX_CLIPS as u32 + 4 {
            history.push(Clip {
                at_seconds: i,
                damaged_frames: 2,
                sample_rate: 48_000,
                samples: vec![],
            });
        }
        assert_eq!(history.clips().count(), MAX_CLIPS);
        // Newest first: the most recent damage is the one you are asking
        // about, so it should not be at the bottom of a list.
        assert_eq!(history.get(0).unwrap().at_seconds, MAX_CLIPS as u32 + 3);
        assert_eq!(history.clips().last().unwrap().at_seconds, 4);
    }

    #[test]
    fn a_label_says_when_and_how_much() {
        let clip =
            Clip { at_seconds: 125, damaged_frames: 5, sample_rate: 48_000, samples: vec![] };
        let label = clip.label();
        assert!(label.contains("2:05"), "{label}");
        assert!(label.contains("100 ms"), "{label}");
    }

    #[test]
    fn seconds_under_ten_are_padded_so_labels_line_up() {
        let clip =
            Clip { at_seconds: 65, damaged_frames: 2, sample_rate: 48_000, samples: vec![] };
        assert!(clip.label().contains("1:05"), "{}", clip.label());
    }

    #[test]
    fn the_cut_count_keeps_moving_after_old_clips_drop_off() {
        // The replay modes watch this to offer a new clip; it must keep
        // rising once the list is full, or Auto-play would stop at eight.
        let mut history = ReplayHistory::default();
        for i in 0..(MAX_CLIPS as u32 + 3) {
            history.push(Clip { at_seconds: i, damaged_frames: 2, sample_rate: 48_000, samples: vec![] });
        }
        assert_eq!(history.cut(), MAX_CLIPS as u64 + 3);
        assert_eq!(history.clips().count(), MAX_CLIPS);
    }
}
