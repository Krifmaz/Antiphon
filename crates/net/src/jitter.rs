use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// What [`JitterBuffer::pop_ready`] handed back.
#[derive(Debug, PartialEq)]
pub enum PopResult {
    /// The next frame in sequence, actually received.
    Frame(Vec<f32>),
    /// The next frame never arrived, but the frame *after* it carried a
    /// low-quality backup copy, so the real audio is recovered rather than
    /// papered over (spec section 2, forward error correction).
    ///
    /// Distinct from [`PopResult::Concealed`] on purpose: this is the lost
    /// moment, muffled. Concealed is a different moment repeated.
    Recovered(Vec<f32>),
    /// The next frame in sequence never arrived (or arrived too late) --
    /// this is the last good frame played again, standing in for it, so
    /// playback gets a repeat instead of a hole (spec §2 "Replay": "the
    /// missing piece can be recovered and played back to you"; this is the
    /// simple version of that -- not real error-correction recovery, a
    /// last-good-frame repeat).
    Concealed(Vec<f32>),
    /// Nothing to hand over yet -- caller should wait and try again.
    Empty,
}

/// Reorders out-of-order UDP arrivals back into sequence and conceals gaps
/// that never fill in (spec §2's "Adaptive jitter buffer" + last-resort
/// "Replay" layers). Sequence numbers come from the crypto session's
/// per-message counter -- already there for replay rejection, reused here
/// as the ordering key rather than inventing a second one.
///
/// Deliberately simplified vs. the spec: `max_wait` is fixed, not adapted
/// to observed network jitter over time (spec says the buffer should
/// "shrink again when things are stable"). A fixed bound is the honest
/// floor this can grow from later without changing the interface.
/// Clean deliveries in a row before the buffer tries holding less audio.
///
/// Deliberately not tiny: shrinking after one or two good frames would make
/// the buffer oscillate on a connection that drops a packet every second or
/// so, which sounds worse than holding a steady margin.
const SHRINK_AFTER_CLEAN: u32 = 200;

/// Denominator of the fraction kept when shrinking -- 3/4 of the current
/// wait. Gentle on the way down, immediate on the way up: shrinking too
/// eagerly costs audible dropouts, while shrinking slowly only costs a
/// little latency.
const SHRINK_FACTOR: u32 = 4;

/// Never adapt below this. Some reordering margin is always worth having;
/// a buffer of zero is just no jitter buffer at all.
const FLOOR: Duration = Duration::from_millis(10);

pub struct JitterBuffer {
    next_wanted: u64,
    pending: BTreeMap<u64, Vec<f32>>,
    last_good: Option<Vec<f32>>,
    /// The ceiling. Adaptation happens below this, never above.
    max_wait: Duration,
    /// What the buffer is actually waiting right now (spec section 2:
    /// "Holds a fraction of a second of audio in reserve to smooth out
    /// unstable connections, and shrinks again when things are stable").
    current_wait: Duration,
    /// Consecutive frames delivered without having to wait, driving the
    /// shrink.
    clean_streak: u32,
    waiting_since: Option<Instant>,
    initialized: bool,
    delivered: u64,
    concealed: u64,
    recovered: u64,
    /// Counters already asked for, so a gap is requested once rather than
    /// on every 10 ms playback tick.
    requested: BTreeSet<u64>,
    /// Backup copies extracted from later frames, keyed by the counter they
    /// can repair. Bounded by `pending`, since a backup is only kept while
    /// the frame carrying it is still buffered.
    backups: BTreeMap<u64, Vec<f32>>,
}

impl JitterBuffer {
    /// `max_wait` is the ceiling, and also where the buffer starts: it
    /// begins cautious and earns its way down to a smaller margin on a
    /// connection that proves stable.
    pub fn new(max_wait: Duration) -> Self {
        Self {
            next_wanted: 0,
            pending: BTreeMap::new(),
            last_good: None,
            max_wait,
            current_wait: max_wait,
            clean_streak: 0,
            waiting_since: None,
            initialized: false,
            delivered: 0,
            concealed: 0,
            recovered: 0,
            requested: BTreeSet::new(),
            backups: BTreeMap::new(),
        }
    }

    /// How long the buffer is currently willing to wait for a late frame --
    /// roughly the latency it is adding, so the UI can show it.
    pub fn current_wait(&self) -> Duration {
        self.current_wait
    }

    /// Lifetime counts of `(frames delivered intact, frames concealed)` --
    /// spec §2's connection-quality readout. Concealed counts every gap
    /// this buffer had to paper over, whether by repeating the last good
    /// frame or (when there was none to repeat) skipping silently.
    pub fn stats(&self) -> (u64, u64) {
        (self.delivered, self.concealed)
    }

    /// How many frames were rebuilt from a backup copy rather than
    /// concealed -- spec section 2's first layer doing its job.
    pub fn recovered(&self) -> u64 {
        self.recovered
    }

    /// Frames sitting in the buffer waiting to be played.
    ///
    /// This is the latency the listener is actually carrying, and it is
    /// what catch-up playback (spec §2) works off: after a stall the
    /// backlog is what has to be drained to get back to live. Frames
    /// *behind* `next_wanted` do not count -- they will never be played,
    /// so they are not a backlog, they are already gone.
    pub fn backlog_frames(&self) -> usize {
        self.pending.range(self.next_wanted..).count()
    }

    /// Counters that are missing, still recoverable, and have not been
    /// asked for yet (spec section 2: "If a chunk goes missing, your app
    /// asks the sender to resend it before it's needed").
    ///
    /// Only reports a gap the buffer is genuinely stuck behind -- a later
    /// frame has arrived, so the missing one is not merely still in
    /// flight. Each counter is returned once: repeating the request every
    /// playback tick would send dozens of them before the first reply
    /// could possibly arrive.
    pub fn take_retransmit_requests(&mut self) -> Vec<u64> {
        if !self.initialized {
            return Vec::new();
        }
        let Some((&lowest_pending, _)) = self.pending.iter().next() else {
            return Vec::new();
        };

        let mut out = Vec::new();
        for counter in self.next_wanted..lowest_pending {
            // A gap already repairable from a backup copy needs no round
            // trip, so asking for it would be wasted bandwidth.
            if self.backups.contains_key(&counter) {
                continue;
            }
            if self.requested.insert(counter) {
                out.push(counter);
            }
        }
        // Anything already played or skipped past can never be wanted
        // again, so the set stays bounded by the live gap.
        self.requested.retain(|c| *c >= self.next_wanted);
        out
    }

    /// Adds a frame together with the backup copy of its predecessor that
    /// it carried (see [`crate::fec`]).
    ///
    /// The backup is only worth keeping if the frame it repairs has not
    /// already been played or given up on.
    pub fn push_with_backup(&mut self, counter: u64, samples: Vec<f32>, backup: Option<Vec<f32>>) {
        if let Some(backup) = backup {
            if counter > 0 {
                let repairs = counter - 1;
                if !self.initialized || repairs >= self.next_wanted {
                    self.backups.insert(repairs, backup);
                }
            }
        }
        self.push(counter, samples);
    }

    /// Adds a received, already-decrypted frame at `counter`. The first
    /// frame ever pushed sets the starting sequence position -- there's no
    /// meaningful "counter 0" to wait for if the stream actually started
    /// at, say, counter 40.
    pub fn push(&mut self, counter: u64, samples: Vec<f32>) {
        if !self.initialized {
            self.initialized = true;
            self.next_wanted = counter;
        }
        if counter < self.next_wanted {
            return; // arrived too late, already delivered or conceal-skipped past it
        }
        self.pending.insert(counter, samples);
    }

    /// Call this on a steady tick (not per network packet -- packets don't
    /// arrive on a schedule, playback does). Returns the next frame once
    /// it's actually next in line, conceals one that's missed `max_wait`,
    /// or reports `Empty` if the caller should just check back next tick.
    pub fn pop_ready(&mut self) -> PopResult {
        // A frame that actually arrived always beats its backup copy.
        self.backups.remove(&self.next_wanted.saturating_sub(1));

        if let Some(samples) = self.pending.remove(&self.next_wanted) {
            // A frame we were already waiting on arrived late but in time.
            // That is the buffer doing its job, and evidence the current
            // margin is earning its keep -- so it resets the streak rather
            // than counting toward shrinking.
            if self.waiting_since.is_some() {
                self.clean_streak = 0;
            } else {
                self.clean_streak = self.clean_streak.saturating_add(1);
                if self.clean_streak >= SHRINK_AFTER_CLEAN {
                    self.clean_streak = 0;
                    let shrunk = self.current_wait * (SHRINK_FACTOR - 1) / SHRINK_FACTOR;
                    self.current_wait = shrunk.max(FLOOR);
                }
            }
            self.last_good = Some(samples.clone());
            self.next_wanted += 1;
            self.waiting_since = None;
            self.delivered += 1;
            return PopResult::Frame(samples);
        }
        if self.pending.is_empty() {
            return PopResult::Empty; // caught up, not stuck on a gap
        }
        let since = *self.waiting_since.get_or_insert_with(Instant::now);
        if since.elapsed() < self.current_wait {
            return PopResult::Empty;
        }
        // Giving up on a frame means the margin was too small for this
        // connection. Grow at once, up to the ceiling, rather than easing
        // up: a concealment is already audible, and reacting slowly would
        // produce several more before the buffer caught up.
        // Before giving up: a later frame may already have carried a
        // backup copy of exactly this one, in which case the real audio is
        // recoverable and nothing needs concealing.
        if let Some(recovered) = self.backups.remove(&self.next_wanted) {
            self.last_good = Some(recovered.clone());
            self.next_wanted += 1;
            self.waiting_since = None;
            self.recovered += 1;
            // A loss still happened, so the margin was genuinely needed --
            // but it was repaired without a gap, so this is not the alarm
            // that a concealment is and does not force the buffer wider.
            self.clean_streak = 0;
            return PopResult::Recovered(recovered);
        }

        self.current_wait = (self.current_wait * 2).min(self.max_wait);
        self.clean_streak = 0;
        self.next_wanted += 1;
        self.waiting_since = None;
        self.concealed += 1;
        match &self.last_good {
            Some(s) => PopResult::Concealed(s.clone()),
            None => PopResult::Empty, // nothing to repeat yet, e.g. lost the very first frame
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gap_is_requested_once_not_on_every_tick() {
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        jb.push(0, vec![1.0]);
        jb.pop_ready();
        // 1 is missing, 2 has arrived, so the buffer is stuck behind a gap.
        jb.push(2, vec![3.0]);

        assert_eq!(jb.take_retransmit_requests(), vec![1]);
        // Asking again on the next tick would flood the sender before any
        // reply could arrive.
        assert!(jb.take_retransmit_requests().is_empty());
    }

    #[test]
    fn nothing_is_requested_when_simply_caught_up() {
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        jb.push(0, vec![1.0]);
        jb.pop_ready();

        // No later frame has arrived, so the next one is merely still in
        // flight -- not missing.
        assert!(jb.take_retransmit_requests().is_empty());
    }

    #[test]
    fn a_gap_already_covered_by_a_backup_is_not_requested() {
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        jb.push(0, vec![1.0]);
        jb.pop_ready();
        jb.push_with_backup(2, vec![3.0], Some(vec![2.0]));

        // Repairable with no round trip, so asking would waste bandwidth.
        assert!(jb.take_retransmit_requests().is_empty());
    }

    #[test]
    fn several_missing_frames_are_all_requested() {
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        jb.push(0, vec![1.0]);
        jb.pop_ready();
        jb.push(4, vec![5.0]);

        assert_eq!(jb.take_retransmit_requests(), vec![1, 2, 3]);
    }

    #[test]
    fn requests_do_not_accumulate_forever() {
        let mut jb = JitterBuffer::new(Duration::from_millis(10));
        jb.push(0, vec![1.0]);
        jb.pop_ready();
        jb.push(3, vec![4.0]);
        assert_eq!(jb.take_retransmit_requests(), vec![1, 2]);

        // Give up on the gap entirely.
        std::thread::sleep(Duration::from_millis(15));
        while !matches!(jb.pop_ready(), PopResult::Frame(_) | PopResult::Empty) {}

        // Counters behind the playhead are dropped rather than remembered
        // for the life of the call.
        jb.take_retransmit_requests();
        assert!(jb.requested.iter().all(|c| *c >= jb.next_wanted));
    }

    #[test]
    fn a_lost_frame_is_recovered_from_the_next_frames_backup() {
        let mut jb = JitterBuffer::new(Duration::from_millis(30));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]));

        // Frame 1 is lost. Frame 2 arrives carrying a backup copy of it.
        jb.push_with_backup(2, vec![3.0], Some(vec![2.0]));

        // Nothing yet -- still hoping the real frame 1 turns up.
        assert_eq!(jb.pop_ready(), PopResult::Empty);
        std::thread::sleep(Duration::from_millis(35));

        assert_eq!(jb.pop_ready(), PopResult::Recovered(vec![2.0]));
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![3.0]));
        assert_eq!(jb.recovered(), 1);
    }

    #[test]
    fn a_real_frame_is_preferred_over_its_backup_copy() {
        let mut jb = JitterBuffer::new(Duration::from_millis(30));
        jb.push(0, vec![1.0]);
        jb.pop_ready();

        // Frame 2 shows up first with a backup of frame 1...
        jb.push_with_backup(2, vec![3.0], Some(vec![9.9]));
        // ...then the real frame 1 arrives after all.
        jb.push(1, vec![2.0]);

        // The real thing, not the muffled copy.
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![2.0]));
        assert_eq!(jb.recovered(), 0);
    }

    #[test]
    fn recovery_avoids_a_concealment() {
        let mut jb = JitterBuffer::new(Duration::from_millis(20));
        jb.push(0, vec![1.0]);
        jb.pop_ready();
        jb.push_with_backup(2, vec![3.0], Some(vec![2.0]));

        // The first pop starts the wait; the gap is only given up on after
        // `current_wait` has actually elapsed since then.
        assert_eq!(jb.pop_ready(), PopResult::Empty);
        std::thread::sleep(Duration::from_millis(25));
        assert!(matches!(jb.pop_ready(), PopResult::Recovered(_)));

        let (_, concealed) = jb.stats();
        assert_eq!(concealed, 0, "a repaired frame should not count as concealed");
    }

    #[test]
    fn two_frames_lost_in_a_row_still_conceals_the_second() {
        let mut jb = JitterBuffer::new(Duration::from_millis(20));
        jb.push(0, vec![1.0]);
        jb.pop_ready();

        // Frames 1 and 2 both lost; frame 3 carries a backup of 2 only.
        jb.push_with_backup(3, vec![4.0], Some(vec![3.0]));

        // Frame 1 has no carrier, so it is concealed once the wait expires.
        assert_eq!(jb.pop_ready(), PopResult::Empty);
        std::thread::sleep(Duration::from_millis(25));
        assert!(matches!(jb.pop_ready(), PopResult::Concealed(_)));

        // Frame 2 does have a carrier, so it is recovered rather than
        // concealed a second time.
        assert_eq!(jb.pop_ready(), PopResult::Empty);
        std::thread::sleep(Duration::from_millis(45));
        assert_eq!(jb.pop_ready(), PopResult::Recovered(vec![3.0]));
    }

    #[test]
    fn a_backup_for_an_already_played_frame_is_discarded() {
        let mut jb = JitterBuffer::new(Duration::from_millis(20));
        jb.push(0, vec![1.0]);
        jb.push(1, vec![2.0]);
        jb.pop_ready();
        jb.pop_ready();

        // A late duplicate of frame 2, carrying a backup of frame 1 that is
        // long since played. It must not rewind anything.
        jb.push_with_backup(2, vec![3.0], Some(vec![9.9]));
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![3.0]));
        assert_eq!(jb.recovered(), 0);
    }

    #[test]
    fn a_stable_connection_shrinks_the_buffer() {
        let mut jb = JitterBuffer::new(Duration::from_millis(80));
        assert_eq!(jb.current_wait(), Duration::from_millis(80));

        // Every frame arrives before it is needed: nothing to smooth out.
        for i in 0..SHRINK_AFTER_CLEAN {
            jb.push(i as u64, vec![1.0]);
            assert!(matches!(jb.pop_ready(), PopResult::Frame(_)));
        }

        assert!(
            jb.current_wait() < Duration::from_millis(80),
            "buffer should shrink on a stable connection, still {:?}",
            jb.current_wait()
        );
    }

    #[test]
    fn the_buffer_never_shrinks_below_the_floor() {
        let mut jb = JitterBuffer::new(Duration::from_millis(12));
        let mut counter = 0u64;
        for _ in 0..20 {
            for _ in 0..SHRINK_AFTER_CLEAN {
                jb.push(counter, vec![1.0]);
                jb.pop_ready();
                counter += 1;
            }
        }
        assert!(jb.current_wait() >= FLOOR, "shrank past the floor: {:?}", jb.current_wait());
    }

    #[test]
    fn a_concealment_grows_the_buffer_back() {
        let mut jb = JitterBuffer::new(Duration::from_millis(80));

        for i in 0..SHRINK_AFTER_CLEAN {
            jb.push(i as u64, vec![1.0]);
            jb.pop_ready();
        }
        let shrunk = jb.current_wait();
        assert!(shrunk < Duration::from_millis(80));

        // Skip a counter so the buffer is stuck on a gap rather than
        // simply caught up with nothing to do.
        jb.push(SHRINK_AFTER_CLEAN as u64 + 1, vec![2.0]);
        assert_eq!(jb.pop_ready(), PopResult::Empty);
        std::thread::sleep(shrunk + Duration::from_millis(5));
        assert!(matches!(jb.pop_ready(), PopResult::Concealed(_)));

        assert!(
            jb.current_wait() > shrunk,
            "buffer should grow after a concealment: {:?} -> {:?}",
            shrunk,
            jb.current_wait()
        );
    }

    #[test]
    fn growth_is_capped_at_the_ceiling() {
        let mut jb = JitterBuffer::new(Duration::from_millis(20));
        let mut counter = 0u64;
        for _ in 0..8 {
            counter += 2;
            jb.push(counter, vec![1.0]);
            loop {
                match jb.pop_ready() {
                    PopResult::Empty => std::thread::sleep(Duration::from_millis(5)),
                    _ => break,
                }
            }
        }
        assert!(
            jb.current_wait() <= Duration::from_millis(20),
            "grew past the ceiling: {:?}",
            jb.current_wait()
        );
    }

    #[test]
    fn a_frame_that_arrives_late_but_in_time_does_not_count_as_stable() {
        let mut jb = JitterBuffer::new(Duration::from_millis(80));

        for i in 0..SHRINK_AFTER_CLEAN - 1 {
            jb.push(i as u64, vec![1.0]);
            jb.pop_ready();
        }
        let before = jb.current_wait();

        // A later frame arrives first, so the buffer is genuinely stuck on
        // a gap rather than merely caught up -- being caught up does not
        // count as waiting, and would not exercise this path.
        jb.push(SHRINK_AFTER_CLEAN as u64, vec![2.0]);
        assert_eq!(jb.pop_ready(), PopResult::Empty);

        // Now the missing frame turns up, in time. The margin was needed,
        // so this must not be the frame that tips the buffer into
        // shrinking.
        jb.push(SHRINK_AFTER_CLEAN as u64 - 1, vec![9.0]);
        assert!(matches!(jb.pop_ready(), PopResult::Frame(_)));

        assert_eq!(jb.current_wait(), before);
    }

    #[test]
    fn in_order_frames_pass_straight_through() {
        let mut jb = JitterBuffer::new(Duration::from_millis(50));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]));
        jb.push(1, vec![2.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![2.0]));
    }

    #[test]
    fn out_of_order_arrival_is_reordered() {
        let mut jb = JitterBuffer::new(Duration::from_millis(50));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]), "establishes the stream at counter 0");

        jb.push(2, vec![3.0]); // counter 1 hasn't arrived yet
        assert_eq!(jb.pop_ready(), PopResult::Empty, "shouldn't hand out 2 before 1 arrives");
        jb.push(1, vec![2.0]); // 1 shows up late, out of order relative to 2
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![2.0]));
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![3.0]));
    }

    #[test]
    fn a_frame_that_never_arrives_gets_concealed_after_the_wait() {
        let mut jb = JitterBuffer::new(Duration::from_millis(10));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]));

        // counter 1 is lost; counter 2 arrives instead.
        jb.push(2, vec![3.0]);
        assert_eq!(jb.pop_ready(), PopResult::Empty, "still within the wait window");
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(jb.pop_ready(), PopResult::Concealed(vec![1.0]), "should repeat the last good frame");
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![3.0]), "then deliver the one that was waiting");
    }

    #[test]
    fn starts_at_whatever_counter_arrives_first() {
        let mut jb = JitterBuffer::new(Duration::from_millis(50));
        jb.push(40, vec![9.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![9.0]), "shouldn't wait forever for counters 0..39");
    }

    #[test]
    fn a_stall_shows_up_as_a_backlog_to_catch_up_on() {
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        assert_eq!(jb.backlog_frames(), 0, "a fresh buffer is not behind");
        for counter in 0..5 {
            jb.push(counter, vec![0.1; 480]);
        }
        assert_eq!(jb.backlog_frames(), 5);
        jb.pop_ready();
        assert_eq!(jb.backlog_frames(), 4, "playing a frame did not reduce the backlog");
    }

    #[test]
    fn frames_too_late_to_play_are_not_counted_as_backlog() {
        // A frame behind the playback point will never be heard. Counting
        // it would make catch-up speed up to drain audio that is not
        // there, adding an artefact to fix a problem that does not exist.
        let mut jb = JitterBuffer::new(Duration::from_millis(60));
        for counter in 0..4 {
            jb.push(counter, vec![0.1; 480]);
        }
        for _ in 0..4 {
            jb.pop_ready();
        }
        jb.push(1, vec![0.1; 480]);
        assert_eq!(jb.backlog_frames(), 0);
    }

    #[test]
    fn stats_count_delivered_and_concealed_frames() {
        let mut jb = JitterBuffer::new(Duration::from_millis(10));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]));
        jb.push(2, vec![3.0]); // counter 1 lost
        assert_eq!(jb.pop_ready(), PopResult::Empty, "first poll notices the gap and starts the wait clock");
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(jb.pop_ready(), PopResult::Concealed(vec![1.0]));
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![3.0]));
        assert_eq!(jb.stats(), (2, 1));
    }

    #[test]
    fn late_duplicate_after_delivery_is_dropped_not_replayed() {
        let mut jb = JitterBuffer::new(Duration::from_millis(50));
        jb.push(0, vec![1.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![1.0]));
        jb.push(0, vec![1.0]); // a stray retransmit/duplicate of something already played
        jb.push(1, vec![2.0]);
        assert_eq!(jb.pop_ready(), PopResult::Frame(vec![2.0]));
    }
}
