//! Send-rate control for file transfers (spec §5 "bandwidth controls").
//!
//! Two jobs, one mechanism:
//!
//! - **Honour a limit** -- the user's upload cap, or the automatic one
//!   while a call is running.
//! - **Find the link's real speed** when there is no limit. A sender that
//!   simply goes as fast as it can outruns every buffer between it and the
//!   receiver, and whatever does not fit is dropped. Measured before this
//!   existed: half the chunks of a large transfer were lost on the first
//!   pass *over loopback*, and on a real home connection flooding the
//!   uplink is also what makes a call on the same connection break up.
//!
//! The second job uses the rule TCP has used for decades, AIMD: speed up
//! steadily while nothing is being lost, and back off sharply when
//! something is. The receiver's repair requests are the loss signal -- it
//! only asks again for chunks that were sent and never arrived.
//!
//! **Deadline-based, not per-chunk sleeps.** The obvious implementation --
//! sleep for `chunk / rate` after every chunk -- does not work: timers on
//! Windows fire in steps of a millisecond or more, so a 0.7 ms sleep is
//! really several, and a 5 MB/s limit quietly became well under half that.
//! Instead each chunk is given a send time, and the sender only sleeps
//! once it is far enough ahead of schedule for a sleep to be accurate. It
//! sends in small bursts, and the average is what was asked for.

use std::time::{Duration, Instant};

/// Where discovery starts. Low enough not to flood a slow uplink on the
/// first second, and doubled away from quickly on a fast one.
const INITIAL_RATE: f64 = 512.0 * 1024.0;

/// Never slower than this, however much is lost -- a transfer that backs
/// itself off to nothing has stopped rather than slowed.
const MIN_RATE: f64 = 32.0 * 1024.0;

/// A ceiling that no real link reaches, so an unlimited transfer on
/// loopback does not grow without bound.
const MAX_RATE: f64 = 1024.0 * 1024.0 * 1024.0;

/// How often the rate is revisited.
const ADJUST_EVERY: Duration = Duration::from_millis(100);

/// Loss below this is noise -- a UDP hop drops the odd packet whatever
/// the rate -- and is not treated as a sign of going too fast.
const LOSS_TOLERANCE: f64 = 0.02;

/// Only sleep once this far ahead of schedule. Anything shorter and the
/// OS timer's granularity, not the rate, decides how long the sleep is.
const MIN_SLEEP: Duration = Duration::from_millis(2);

/// How far behind schedule the sender may fall before it stops trying to
/// catch up. Without this, a pause or a slow disk read would bank "credit"
/// that is then spent as one enormous burst.
const MAX_LAG: Duration = Duration::from_millis(20);

pub struct Pacer {
    /// The discovered rate, bytes per second.
    rate: f64,
    /// Still doubling towards the first loss.
    slow_start: bool,
    /// When the next chunk is due.
    next_send: Instant,
    last_adjust: Instant,
    /// Sent and reported lost since the last adjustment.
    sent: u64,
    lost: u64,
}

impl Default for Pacer {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            rate: INITIAL_RATE,
            slow_start: true,
            next_send: now,
            last_adjust: now,
            sent: 0,
            lost: 0,
        }
    }
}

impl Pacer {
    /// The rate currently being sent at, bytes per second, given a limit.
    pub fn effective_rate(&self, limit: Option<f64>) -> f64 {
        match limit {
            Some(limit) if limit > 0.0 => self.rate.min(limit),
            _ => self.rate,
        }
    }

    /// Records the receiver reporting `count` chunks lost.
    pub fn report_loss(&mut self, count: u64) {
        self.lost += count;
    }

    /// Accounts for a chunk of `len` bytes being sent now, and returns how
    /// long to wait before sending the next one -- `None` when there is no
    /// need to wait yet.
    ///
    /// `limit` is the user's cap or the in-call one, in bytes per second.
    pub fn after_send(&mut self, len: usize, limit: Option<f64>) -> Option<Duration> {
        self.after_send_at(len, limit, Instant::now())
    }

    fn after_send_at(&mut self, len: usize, limit: Option<f64>, now: Instant) -> Option<Duration> {
        self.sent += 1;
        self.adjust(now);

        // Falling badly behind -- after a pause, or a slow read -- resets
        // the schedule rather than letting the sender sprint to catch up.
        if self.next_send + MAX_LAG < now {
            self.next_send = now;
        }
        let rate = self.effective_rate(limit);
        self.next_send += Duration::from_secs_f64(len as f64 / rate);

        let ahead = self.next_send.saturating_duration_since(now);
        (ahead >= MIN_SLEEP).then_some(ahead)
    }

    fn adjust(&mut self, now: Instant) {
        if now.duration_since(self.last_adjust) < ADJUST_EVERY {
            return;
        }
        self.last_adjust = now;
        let loss = if self.sent == 0 { 0.0 } else { self.lost as f64 / self.sent as f64 };
        self.sent = 0;
        self.lost = 0;

        if loss > LOSS_TOLERANCE {
            // Multiplicative decrease: back off sharply, because what is
            // being lost has to be sent again and every extra packet on a
            // full link makes the congestion worse.
            self.rate = (self.rate * 0.7).max(MIN_RATE);
            self.slow_start = false;
        } else if self.slow_start {
            // Grow fast until the first loss says where the edge is.
            self.rate = (self.rate * 1.5).min(MAX_RATE);
        } else {
            // Then creep up, so a link that got faster is noticed without
            // repeatedly overshooting one that did not.
            self.rate = (self.rate * 1.05).min(MAX_RATE);
        }
    }

    #[cfg(test)]
    fn rate(&self) -> f64 {
        self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 3800;

    /// Sends `seconds` worth of chunks through the pacer on a simulated
    /// clock, returning the bytes-per-second actually achieved.
    fn simulate(pacer: &mut Pacer, limit: Option<f64>, seconds: f64) -> f64 {
        let start = Instant::now();
        let mut now = start;
        pacer.next_send = start;
        pacer.last_adjust = start;
        let end = start + Duration::from_secs_f64(seconds);
        let mut bytes = 0usize;
        while now < end {
            bytes += CHUNK;
            if let Some(wait) = pacer.after_send_at(CHUNK, limit, now) {
                now += wait;
            }
        }
        bytes as f64 / seconds
    }

    #[test]
    fn a_limit_is_met_on_average_despite_coarse_timers() {
        // The bug this replaces: per-chunk sleeps shorter than the timer
        // resolution overslept, and a 5 MB/s limit delivered far less.
        let mut pacer = Pacer { rate: MAX_RATE, slow_start: false, ..Default::default() };
        let limit = 5.0 * 1024.0 * 1024.0;
        let achieved = simulate(&mut pacer, Some(limit), 2.0);
        assert!((achieved - limit).abs() / limit < 0.05, "asked {limit}, got {achieved}");
    }

    #[test]
    fn every_wait_it_asks_for_is_long_enough_to_be_accurate() {
        let mut pacer = Pacer { rate: MAX_RATE, slow_start: false, ..Default::default() };
        let now = Instant::now();
        pacer.next_send = now;
        for _ in 0..1000 {
            if let Some(wait) = pacer.after_send_at(CHUNK, Some(1024.0 * 1024.0), now) {
                assert!(wait >= MIN_SLEEP, "asked for a {wait:?} sleep");
                break;
            }
        }
    }

    #[test]
    fn with_no_loss_it_speeds_up() {
        let mut pacer = Pacer::default();
        let start = pacer.rate();
        simulate(&mut pacer, None, 1.0);
        assert!(pacer.rate() > start * 4.0, "only reached {} from {start}", pacer.rate());
    }

    #[test]
    fn loss_makes_it_back_off() {
        let mut pacer = Pacer { rate: 10.0 * 1024.0 * 1024.0, slow_start: false, ..Default::default() };
        let now = Instant::now();
        pacer.last_adjust = now;
        pacer.sent = 100;
        pacer.report_loss(30);
        pacer.adjust(now + ADJUST_EVERY);
        assert!(pacer.rate() < 10.0 * 1024.0 * 1024.0 * 0.8);
        assert!(!pacer.slow_start, "a loss ends slow start");
    }

    #[test]
    fn a_stray_lost_packet_is_not_mistaken_for_congestion() {
        // UDP drops the odd packet whatever the rate. Backing off for that
        // would make every transfer crawl.
        let mut pacer = Pacer { rate: 1024.0 * 1024.0, slow_start: false, ..Default::default() };
        let now = Instant::now();
        pacer.last_adjust = now;
        pacer.sent = 1000;
        pacer.report_loss(5);
        pacer.adjust(now + ADJUST_EVERY);
        assert!(pacer.rate() >= 1024.0 * 1024.0);
    }

    #[test]
    fn it_never_backs_off_to_nothing() {
        let mut pacer = Pacer { rate: MIN_RATE * 2.0, slow_start: false, ..Default::default() };
        let mut now = Instant::now();
        for _ in 0..50 {
            pacer.sent = 10;
            pacer.report_loss(10);
            now += ADJUST_EVERY;
            pacer.adjust(now);
        }
        assert_eq!(pacer.rate(), MIN_RATE);
    }

    #[test]
    fn a_pause_does_not_bank_a_burst() {
        // Without the lag cap, ten idle seconds would be "owed" and spent
        // as ten seconds' worth of chunks with no gaps at all.
        let mut pacer = Pacer { rate: 1024.0 * 1024.0, slow_start: false, ..Default::default() };
        let start = Instant::now();
        pacer.next_send = start;
        pacer.last_adjust = start;
        let later = start + Duration::from_secs(10);
        let mut sent_without_waiting = 0;
        while pacer.after_send_at(CHUNK, None, later).is_none() {
            sent_without_waiting += 1;
            assert!(sent_without_waiting < 100, "the pause was banked as a burst");
        }
    }

    #[test]
    fn a_limit_below_the_discovered_rate_wins() {
        let pacer = Pacer { rate: 10.0 * 1024.0 * 1024.0, ..Default::default() };
        assert_eq!(pacer.effective_rate(Some(256.0 * 1024.0)), 256.0 * 1024.0);
        assert_eq!(pacer.effective_rate(None), 10.0 * 1024.0 * 1024.0);
        assert_eq!(pacer.effective_rate(Some(0.0)), 10.0 * 1024.0 * 1024.0, "0 means unlimited");
    }
}
