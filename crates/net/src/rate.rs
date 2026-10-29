//! Automatic bandwidth for calls (spec §2): what to send, given how the
//! link is doing.
//!
//! Each receiver reports, about once a second, how many voice packets it
//! lost and enough timing to work out the round trip (see the client's
//! `link_quality`). This module turns those reports into a [`Plan`]: a
//! voice tier from [`crate::voice::TIERS`] and a share of the video
//! budget.
//!
//! # The ladder
//!
//! [`LADDER`] orders every step, best first, so that **video gives way
//! before voice does**: first the video budget halves, then quarters, then
//! video pauses, and only then does the voice bitrate start down towards
//! its floor of 6 kbit/s. A call where the picture freezes but the words
//! get through is a working call; the reverse is not.
//!
//! # Moving along it
//!
//! Down fast, up slowly. A bad report (5% loss, or a 400 ms round trip)
//! steps down once; a very bad one (15%, 700 ms) twice; at most once every
//! [`DOWN_EVERY`], so the effect of one step is seen before the next.
//! Stepping back up waits for [`UP_AFTER`] of clean reports, one step at a
//! time -- cheap to wait, and expensive to bounce straight back into the
//! congestion that caused the drop.
//!
//! With several people sending reports (a group call), the worst of them
//! decides: one upload goes to everybody, so it has to suit the weakest
//! link.
//!
//! # Low data
//!
//! The user's "Low data" switch (and the phone's data saver on mobile
//! data) starts the call at [`LOW_DATA_STEP`] -- 8 kbit/s mediumband,
//! video off, about 17 kbit/s on the wire -- and never goes above it. The
//! controller can still drop to the floor below it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::voice::FLOOR_TIER;

/// What to send right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Index into [`crate::voice::TIERS`].
    pub voice_tier: usize,
    /// Percent of the video budget to use; 0 pauses video.
    pub video_percent: u8,
}

/// Every step, best first: video gives way first, then voice.
pub const LADDER: [Plan; 8] = [
    Plan { voice_tier: 0, video_percent: 100 },
    Plan { voice_tier: 0, video_percent: 50 },
    Plan { voice_tier: 0, video_percent: 25 },
    Plan { voice_tier: 1, video_percent: 0 },
    Plan { voice_tier: 2, video_percent: 0 },
    Plan { voice_tier: 3, video_percent: 0 },
    Plan { voice_tier: 4, video_percent: 0 },
    Plan { voice_tier: FLOOR_TIER, video_percent: 0 },
];

/// Where "Low data" starts and the highest it goes: 8 kbit/s mediumband
/// voice in 60 ms packets, no video.
pub const LOW_DATA_STEP: usize = 6;

/// Loss, in percent, from which a report counts as bad.
const POOR_LOSS: u8 = 5;
/// ... and very bad.
const SEVERE_LOSS: u8 = 15;
/// Loss below which a report counts as clean.
const CLEAN_LOSS: u8 = 2;
/// Round trips, in milliseconds, for the same three.
const POOR_RTT_MS: u32 = 400;
const SEVERE_RTT_MS: u32 = 700;
const CLEAN_RTT_MS: u32 = 250;

/// The least time between two steps down.
pub const DOWN_EVERY: Duration = Duration::from_millis(1_500);
/// Clean reports needed, uninterrupted, before a step up.
pub const UP_AFTER: Duration = Duration::from_secs(10);
/// A sender whose reports stop is forgotten after this long, so a person
/// who left a group call doesn't hold everyone at their level.
const STALE_AFTER: Duration = Duration::from_secs(6);

/// The latest report from one receiver.
#[derive(Clone, Copy, Debug)]
struct Report {
    loss_pct: u8,
    rtt_ms: Option<u32>,
    at: Instant,
}

/// Picks the [`Plan`] from receivers' reports.
#[derive(Debug, Default)]
pub struct RateController {
    /// Index into [`LADDER`].
    step: usize,
    low_data: bool,
    reports: HashMap<u64, Report>,
    last_down: Option<Instant>,
    /// When the current run of clean reports began.
    clean_since: Option<Instant>,
}

impl RateController {
    /// A controller at the top of the ladder.
    pub fn new() -> Self {
        Self::default()
    }

    /// What to send now.
    pub fn plan(&self) -> Plan {
        let step = if self.low_data { self.step.max(LOW_DATA_STEP) } else { self.step };
        LADDER[step]
    }

    /// The step on [`LADDER`] in use (after "Low data").
    pub fn step(&self) -> usize {
        if self.low_data {
            self.step.max(LOW_DATA_STEP)
        } else {
            self.step
        }
    }

    /// Turns "Low data" on or off.
    pub fn set_low_data(&mut self, on: bool) {
        self.low_data = on;
        if on {
            self.step = self.step.max(LOW_DATA_STEP);
        }
    }

    /// The worst loss any receiver reported recently, in percent: what
    /// the encoder should spend on FEC.
    pub fn expected_loss(&self) -> u8 {
        self.reports.values().map(|r| r.loss_pct).max().unwrap_or(0)
    }

    /// Stops listening to `source`, e.g. when they leave.
    pub fn forget(&mut self, source: u64) {
        self.reports.remove(&source);
    }

    /// Takes a report from `source`: `loss_pct` of its voice packets were
    /// lost since its last report, and the round trip was `rtt_ms` (when
    /// known). Moves along the ladder if the worst recent report says to.
    pub fn on_report(&mut self, source: u64, loss_pct: u8, rtt_ms: Option<u32>, now: Instant) {
        self.reports.insert(source, Report { loss_pct: loss_pct.min(100), rtt_ms, at: now });
        self.reports.retain(|_, r| now.saturating_duration_since(r.at) < STALE_AFTER);

        // Down on the report just in, so one bad report counts once
        // however many others arrive while it is still fresh. Up only
        // when every fresh report is clean.
        let rtt = rtt_ms.unwrap_or(0);
        let severe = loss_pct >= SEVERE_LOSS || rtt >= SEVERE_RTT_MS;
        let poor = loss_pct >= POOR_LOSS || rtt >= POOR_RTT_MS;
        let worst_rtt = self.reports.values().filter_map(|r| r.rtt_ms).max().unwrap_or(0);
        let clean = self.expected_loss() < CLEAN_LOSS && worst_rtt < CLEAN_RTT_MS;

        if severe || poor {
            self.clean_since = None;
            let may_step = self.last_down.is_none_or(|t| now.saturating_duration_since(t) >= DOWN_EVERY);
            if may_step {
                let by = if severe { 2 } else { 1 };
                let next = (self.step + by).min(LADDER.len() - 1);
                if next != self.step {
                    self.step = next;
                    self.last_down = Some(now);
                }
            }
        } else if clean {
            let since = *self.clean_since.get_or_insert(now);
            let floor = if self.low_data { LOW_DATA_STEP } else { 0 };
            if self.step > floor && now.saturating_duration_since(since) >= UP_AFTER {
                self.step -= 1;
                self.clean_since = Some(now);
            }
        } else {
            // Neither bad nor clean: hold where we are.
            self.clean_since = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, secs: f32) -> Instant {
        start + Duration::from_secs_f32(secs)
    }

    #[test]
    fn video_gives_way_before_voice() {
        let first_voice_drop = LADDER.iter().position(|p| p.voice_tier > 0).unwrap();
        assert!(LADDER[..first_voice_drop].windows(2).all(|w| w[1].video_percent < w[0].video_percent));
        assert_eq!(LADDER[first_voice_drop].video_percent, 0, "video is paused before voice drops");
        assert!(LADDER.windows(2).all(|w| w[1].voice_tier >= w[0].voice_tier));
        assert_eq!(LADDER[LADDER.len() - 1].voice_tier, FLOOR_TIER);
    }

    #[test]
    fn heavy_loss_walks_down_to_the_floor_and_no_further() {
        let start = Instant::now();
        let mut c = RateController::new();
        for i in 0..20 {
            c.on_report(0, 30, Some(100), at(start, i as f32 * 1.5));
        }
        assert_eq!(c.plan(), LADDER[LADDER.len() - 1]);
        assert_eq!(c.plan().voice_tier, FLOOR_TIER);
    }

    #[test]
    fn steps_down_are_spaced_out() {
        let start = Instant::now();
        let mut c = RateController::new();
        c.on_report(0, 8, None, start);
        assert_eq!(c.step(), 1);
        // A second bad report straight after doesn't stack on the first.
        c.on_report(0, 8, None, at(start, 0.5));
        assert_eq!(c.step(), 1);
        c.on_report(0, 8, None, at(start, 1.6));
        assert_eq!(c.step(), 2);
    }

    #[test]
    fn a_long_round_trip_alone_is_enough_to_step_down() {
        let mut c = RateController::new();
        c.on_report(0, 0, Some(800), Instant::now());
        assert_eq!(c.step(), 2);
    }

    #[test]
    fn recovery_is_one_step_per_quiet_spell() {
        let start = Instant::now();
        let mut c = RateController::new();
        c.on_report(0, 20, None, start);
        assert_eq!(c.step(), 2);
        for i in 1..=9 {
            c.on_report(0, 0, Some(80), at(start, i as f32));
        }
        assert_eq!(c.step(), 2, "nine clean seconds isn't enough");
        c.on_report(0, 0, Some(80), at(start, 11.0));
        assert_eq!(c.step(), 1);
        c.on_report(0, 0, Some(80), at(start, 12.0));
        assert_eq!(c.step(), 1, "the next step waits for another quiet spell");
    }

    #[test]
    fn a_middling_report_holds_and_restarts_the_clean_count() {
        let start = Instant::now();
        let mut c = RateController::new();
        c.on_report(0, 20, None, start);
        for i in 1..=8 {
            c.on_report(0, 0, None, at(start, i as f32));
        }
        c.on_report(0, 3, None, at(start, 9.0)); // not bad, not clean
        c.on_report(0, 0, None, at(start, 12.0));
        assert_eq!(c.step(), 2);
    }

    #[test]
    fn the_worst_receiver_decides_until_it_goes_quiet() {
        let start = Instant::now();
        let mut c = RateController::new();
        c.on_report(1, 0, Some(50), start);
        c.on_report(2, 25, Some(50), start);
        assert_eq!(c.step(), 2);
        assert_eq!(c.expected_loss(), 25);
        // Person 1's clean reports don't lift it while person 2's bad one
        // is fresh, and don't re-count it either.
        c.on_report(1, 0, Some(50), at(start, 3.0));
        assert_eq!(c.step(), 2);
        // Person 2 leaves; only person 1's clean reports remain.
        for i in 4..=30 {
            c.on_report(1, 0, Some(50), at(start, i as f32));
        }
        assert_eq!(c.expected_loss(), 0);
        assert_eq!(c.step(), 0);
    }

    #[test]
    fn low_data_caps_the_top_but_not_the_bottom() {
        let start = Instant::now();
        let mut c = RateController::new();
        c.set_low_data(true);
        assert_eq!(c.plan(), LADDER[LOW_DATA_STEP]);
        assert_eq!(c.plan().video_percent, 0);
        for i in 0..30 {
            c.on_report(0, 0, Some(20), at(start, i as f32));
        }
        assert_eq!(c.step(), LOW_DATA_STEP, "clean reports don't lift it above low data");
        c.on_report(0, 30, None, at(start, 40.0));
        assert_eq!(c.plan().voice_tier, FLOOR_TIER);
        c.set_low_data(false);
        for i in 41..200 {
            c.on_report(0, 0, Some(20), at(start, i as f32));
        }
        assert_eq!(c.step(), 0, "off again, it climbs back to the top");
    }
}
