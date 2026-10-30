//! Mixing everyone's audio into one output (spec §3 group calls, and §1
//! "Ducking": "automatically lowers everyone else slightly when one person
//! is talking, so overlapping voices stay clear").
//!
//! Each person's audio arrives on its own schedule, a frame at a time, and
//! people only send while they are talking. Two easy mixers are both
//! wrong: one that outputs whatever arrived each tick plays two people
//! *one after the other* and falls behind; one that pads a quiet source
//! with zeros chops a frame that arrived a few milliseconds late in half.
//!
//! So the output advances by the *shortest* queue among people who are
//! currently talking, and someone counts as talking until they have sent
//! nothing for [`IDLE_PULLS`] pulls. One person alone is a passthrough --
//! a 1:1 call pays nothing for this existing.
//!
//! Ducking is decided per pull: when more than one person is talking, the
//! loudest stays as is and everyone else is lowered by the configured
//! amount, ramped so it doesn't click.

use std::collections::{HashMap, VecDeque};

/// Pulls (10 ms each) without new audio before a source stops holding the
/// mix back. Longer than a frame plus jitter, shorter than anyone notices.
pub const IDLE_PULLS: u32 = 8;
/// A source more than this far ahead is trimmed from the front, so one
/// person's backlog can't grow without bound while another stalls.
pub const MAX_QUEUE: usize = 48_000 / 5; // 200 ms at 48 kHz

#[derive(Default)]
struct Source {
    queue: VecDeque<f32>,
    idle_pulls: u32,
    /// Current ducking gain, linear, ramped toward its target.
    gain: f32,
}

#[derive(Default)]
pub struct Mixer {
    sources: HashMap<u64, Source>,
    /// How far to lower everyone but the loudest, in dB. 0 is off.
    duck_db: f32,
}

impl Mixer {
    pub fn set_duck_db(&mut self, db: f32) {
        self.duck_db = db.max(0.0);
    }

    pub fn push(&mut self, source: u64, samples: &[f32]) {
        let s = self.sources.entry(source).or_insert_with(|| Source { gain: 1.0, ..Default::default() });
        s.queue.extend(samples.iter().copied());
        s.idle_pulls = 0;
        while s.queue.len() > MAX_QUEUE {
            s.queue.pop_front();
        }
    }

    pub fn remove(&mut self, source: u64) {
        self.sources.remove(&source);
    }

    /// Everything that can be played right now, mixed.
    pub fn pull(&mut self) -> Vec<f32> {
        for s in self.sources.values_mut() {
            if s.queue.is_empty() {
                s.idle_pulls = s.idle_pulls.saturating_add(1);
            }
        }
        let active: Vec<u64> =
            self.sources.iter().filter(|(_, s)| s.idle_pulls < IDLE_PULLS).map(|(id, _)| *id).collect();
        let n = active.iter().map(|id| self.sources[id].queue.len()).min().unwrap_or(0);
        // Nobody talking yet, but a source that has gone idle may still
        // hold a tail: flush it rather than strand it.
        let n = if n == 0 && active.is_empty() {
            self.sources.values().map(|s| s.queue.len()).max().unwrap_or(0)
        } else {
            n
        };
        if n == 0 {
            return Vec::new();
        }

        let talking: Vec<u64> = self.sources.iter().filter(|(_, s)| !s.queue.is_empty()).map(|(id, _)| *id).collect();
        let loudest = if talking.len() > 1 && self.duck_db > 0.0 {
            talking.iter().copied().max_by(|a, b| {
                rms(&self.sources[a].queue, n).total_cmp(&rms(&self.sources[b].queue, n))
            })
        } else {
            None
        };
        let ducked = 10f32.powf(-self.duck_db / 20.0);

        let mut out = vec![0.0f32; n];
        for (id, s) in &mut self.sources {
            let target = match loudest {
                Some(l) if l != *id => ducked,
                _ => 1.0,
            };
            // Ramp over the block: ~10 ms, enough to avoid a click.
            let start = s.gain;
            let take = n.min(s.queue.len());
            for (i, o) in out.iter_mut().take(take).enumerate() {
                let g = start + (target - start) * (i + 1) as f32 / n as f32;
                *o += s.queue.pop_front().unwrap_or(0.0) * g;
            }
            s.gain = target;
        }
        for o in &mut out {
            *o = o.clamp(-1.0, 1.0);
        }
        out
    }
}

fn rms(queue: &VecDeque<f32>, n: usize) -> f32 {
    let take = n.min(queue.len()).max(1);
    queue.iter().take(take).map(|s| s * s).sum::<f32>() / take as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_source_is_a_passthrough() {
        let mut m = Mixer::default();
        m.push(0, &[0.1, 0.2, 0.3]);
        assert_eq!(m.pull(), vec![0.1, 0.2, 0.3]);
        assert!(m.pull().is_empty());
    }

    #[test]
    fn two_talkers_are_mixed_not_played_one_after_the_other() {
        let mut m = Mixer::default();
        m.push(0, &[0.1; 960]);
        m.push(1, &[0.2; 960]);
        let out = m.pull();
        assert_eq!(out.len(), 960);
        assert!((out[500] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn a_late_frame_is_waited_for_not_chopped() {
        let mut m = Mixer::default();
        m.push(0, &[0.1; 960]);
        m.push(1, &[0.2; 960]);
        m.pull();
        // Source 0's next frame arrives; source 1's is a tick late.
        m.push(0, &[0.1; 960]);
        assert!(m.pull().is_empty(), "should hold for the other talker");
        m.push(1, &[0.2; 960]);
        assert_eq!(m.pull().len(), 960);
    }

    #[test]
    fn someone_who_stopped_talking_stops_holding_the_mix_back() {
        let mut m = Mixer::default();
        m.push(0, &[0.1; 960]);
        m.push(1, &[0.2; 960]);
        m.pull();
        let mut played = 0;
        for _ in 0..IDLE_PULLS + 2 {
            m.push(0, &[0.1; 480]);
            played += m.pull().len();
        }
        assert!(played > 0, "source 1 went quiet; source 0 must play");
    }

    #[test]
    fn ducking_lowers_everyone_but_the_loudest() {
        let mut m = Mixer::default();
        m.set_duck_db(20.0);
        // Two blocks so the ramp has settled by the second.
        for _ in 0..2 {
            m.push(0, &[0.5; 480]);
            m.push(1, &[0.1; 480]);
            let out = m.pull();
            if let Some(last) = out.last() {
                let expected = 0.5 + 0.1 * 0.1;
                assert!((last - expected).abs() < 1e-3, "{last} vs {expected}");
            }
        }
    }

    #[test]
    fn a_removed_source_is_forgotten() {
        let mut m = Mixer::default();
        m.push(1, &[0.2; 10]);
        m.remove(1);
        assert!(m.pull().is_empty());
    }
}
