//! Bandwidth controls for file transfer (spec §5: "upload cap, per-transfer
//! throttle, queue, pause/resume, Wi-Fi-only").
//!
//! The spec lists these under "details that will bite if skipped", and the
//! bite is specific: file transfer and the live call share one connection,
//! so a transfer running flat out competes with the audio. Voice degrades
//! long before a file transfer notices, which means an unthrottled transfer
//! sounds like a bad connection rather than like a busy one -- and people
//! blame the call quality, not the file they are sending.
//!
//! So the default here is not "as fast as possible". It is "fast enough,
//! and never at the expense of the conversation".
//!
//! What this module owns is the pacing arithmetic: given a cap, how long
//! should the sender wait between chunks. Keeping it separate from the
//! transfer loop means the rule can be tested without sockets, which is
//! the part worth being sure about.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How fast a transfer is allowed to go.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct TransferLimits {
    /// Kilobytes per second, or 0 for unlimited.
    pub upload_kbps: u32,
    /// Slow to `in_call_kbps` while a call is running.
    pub throttle_during_calls: bool,
    /// The reduced cap used during a call.
    pub in_call_kbps: u32,
}

impl Default for TransferLimits {
    fn default() -> Self {
        Self {
            // Unlimited when nothing else is happening: there is no reason
            // to be slow for its own sake.
            upload_kbps: 0,
            // But throttled during a call, by default, because the failure
            // it prevents is one people misdiagnose as a bad connection.
            throttle_during_calls: true,
            in_call_kbps: 256,
        }
    }
}

impl TransferLimits {
    /// The cap that applies right now, in kilobytes per second. 0 means
    /// unlimited.
    pub fn effective_kbps(&self, in_call: bool) -> u32 {
        if in_call && self.throttle_during_calls {
            // A user who set a *lower* overall cap meant it, so the
            // in-call limit never raises their own ceiling.
            if self.upload_kbps == 0 {
                self.in_call_kbps
            } else {
                self.upload_kbps.min(self.in_call_kbps)
            }
        } else {
            self.upload_kbps
        }
    }

    /// How long to wait after sending `chunk_bytes` to stay within the cap.
    ///
    /// Returns zero when unlimited. Paced per chunk rather than by
    /// sleeping in bulk: a transfer that sends a burst and then stops for
    /// a second still spikes the link during the burst, which is exactly
    /// what disturbs a call.
    pub fn delay_after_chunk(&self, chunk_bytes: usize, in_call: bool) -> Duration {
        let kbps = self.effective_kbps(in_call);
        if kbps == 0 {
            return Duration::ZERO;
        }
        let bytes_per_second = kbps as f64 * 1024.0;
        Duration::from_secs_f64(chunk_bytes as f64 / bytes_per_second)
    }

    /// A human description of what is currently in force, for the UI.
    pub fn describe(&self, in_call: bool) -> String {
        match self.effective_kbps(in_call) {
            0 => "Sending as fast as the connection allows.".to_string(),
            kbps if in_call && self.throttle_during_calls => {
                format!("Limited to {kbps} KB/s while you're on a call, so it doesn't affect the audio.")
            }
            kbps => format!("Limited to {kbps} KB/s."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_unlimited_but_polite_during_calls() {
        let limits = TransferLimits::default();

        // Nothing else happening: no reason to be slow.
        assert_eq!(limits.effective_kbps(false), 0);
        // On a call: throttled, because the alternative sounds like a bad
        // connection and gets blamed on the call.
        assert_eq!(limits.effective_kbps(true), 256);
    }

    #[test]
    fn a_users_own_cap_is_never_raised_by_the_in_call_limit() {
        // Someone who set 64 KB/s meant it. The in-call rule exists to
        // slow transfers down, never to speed them up.
        let limits = TransferLimits { upload_kbps: 64, throttle_during_calls: true, in_call_kbps: 256 };
        assert_eq!(limits.effective_kbps(true), 64);
        assert_eq!(limits.effective_kbps(false), 64);
    }

    #[test]
    fn throttling_can_be_turned_off_entirely() {
        let limits = TransferLimits { upload_kbps: 0, throttle_during_calls: false, in_call_kbps: 256 };
        assert_eq!(limits.effective_kbps(true), 0);
    }

    #[test]
    fn an_unlimited_transfer_waits_for_nothing() {
        let limits = TransferLimits { upload_kbps: 0, throttle_during_calls: false, in_call_kbps: 0 };
        assert_eq!(limits.delay_after_chunk(4096, false), Duration::ZERO);
    }

    #[test]
    fn the_delay_matches_the_cap() {
        // 100 KB/s and a 1 KB chunk means roughly 10 ms per chunk.
        let limits = TransferLimits { upload_kbps: 100, throttle_during_calls: false, in_call_kbps: 0 };
        let delay = limits.delay_after_chunk(1024, false);

        assert!(
            (delay.as_secs_f64() - 0.01).abs() < 1e-6,
            "expected ~10ms, got {delay:?}"
        );
    }

    #[test]
    fn halving_the_cap_doubles_the_wait() {
        let fast = TransferLimits { upload_kbps: 200, throttle_during_calls: false, in_call_kbps: 0 };
        let slow = TransferLimits { upload_kbps: 100, throttle_during_calls: false, in_call_kbps: 0 };

        let f = fast.delay_after_chunk(4096, false).as_secs_f64();
        let s = slow.delay_after_chunk(4096, false).as_secs_f64();
        assert!((s / f - 2.0).abs() < 1e-6, "{s} is not twice {f}");
    }

    #[test]
    fn a_bigger_chunk_waits_proportionally_longer() {
        let limits = TransferLimits { upload_kbps: 100, throttle_during_calls: false, in_call_kbps: 0 };
        let small = limits.delay_after_chunk(1024, false).as_secs_f64();
        let big = limits.delay_after_chunk(4096, false).as_secs_f64();

        assert!((big / small - 4.0).abs() < 1e-6);
    }

    #[test]
    fn being_on_a_call_changes_the_pacing() {
        let limits = TransferLimits::default();
        assert_eq!(limits.delay_after_chunk(3800, false), Duration::ZERO);
        assert!(limits.delay_after_chunk(3800, true) > Duration::ZERO);
    }

    #[test]
    fn a_zero_byte_chunk_does_not_wait() {
        let limits = TransferLimits { upload_kbps: 100, throttle_during_calls: false, in_call_kbps: 0 };
        assert_eq!(limits.delay_after_chunk(0, false), Duration::ZERO);
    }

    #[test]
    fn the_description_says_why_it_is_slow_during_a_call() {
        let limits = TransferLimits::default();

        // The whole point of saying it: otherwise a throttled transfer
        // looks like a broken one.
        assert!(limits.describe(true).contains("audio"), "{}", limits.describe(true));
        assert!(limits.describe(false).contains("fast as"), "{}", limits.describe(false));
    }

    #[test]
    fn limits_round_trip_through_json() {
        let limits = TransferLimits { upload_kbps: 512, throttle_during_calls: false, in_call_kbps: 128 };
        let json = serde_json::to_string(&limits).unwrap();
        assert_eq!(serde_json::from_str::<TransferLimits>(&json).unwrap(), limits);
    }
}
