//! A per-key rate limit for wake-ups.
//!
//! A wake-up makes a phone light up, buzz, or ring, and a push service
//! delivers whatever it is handed. Without a limit, anyone holding a
//! contact's endpoint (a contact, or a mailbox node) could keep that phone
//! awake and its battery draining. So every party applies one:
//!
//! - the **sender** per recipient, so a stuck redial loop or a burst of
//!   queued mail does not turn into a burst of pushes;
//! - the **mailbox node** per inbox and per endpoint, since it posts on
//!   behalf of people it cannot identify;
//! - the **phone** per sender, so a contact on a misbehaving build still
//!   cannot make it ring more than a few times a minute.
//!
//! It is a token bucket: `burst` wake-ups at once, then one more every
//! `every`. Keys whose bucket has refilled are forgotten, so the table only
//! holds people who were woken recently, and it is capped so a flood of
//! distinct keys cannot grow it without bound.

use std::collections::HashMap;
use std::hash::Hash;
use std::time::{Duration, Instant};

/// How many keys one limiter remembers at most. Past this, keys that have
/// fully refilled are dropped, and if none has, new keys are refused until
/// some do: refusing a wake-up costs a little latency, while an unbounded
/// table on a public node costs its memory.
pub const MAX_KEYS: usize = 65_536;

/// One key's bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    /// Tokens left, as of `at`. Fractional so a refill is exact.
    tokens: f64,
    /// When `tokens` was last brought up to date.
    at: Instant,
}

/// A token bucket per key.
#[derive(Debug, Clone)]
pub struct RateLimit<K> {
    /// Most wake-ups allowed back to back.
    burst: u32,
    /// How long one token takes to come back.
    every: Duration,
    /// The keys seen recently.
    buckets: HashMap<K, Bucket>,
}

impl<K: Hash + Eq + Clone> RateLimit<K> {
    /// `burst` at once, then one per `every`. A `burst` of 0 is treated as
    /// 1, since a limiter that never allows anything is a switch, not a
    /// limit.
    pub fn new(burst: u32, every: Duration) -> Self {
        Self { burst: burst.max(1), every, buckets: HashMap::new() }
    }

    /// The sender-side and phone-side limit for calls: three in a row (a
    /// redial or two), then one every 20 seconds.
    pub fn for_calls() -> Self {
        Self::new(3, Duration::from_secs(20))
    }

    /// The limit for mail and file wake-ups: one a minute. The phone
    /// fetches everything waiting when it wakes, so a second wake-up
    /// within the minute would only fetch what the first already did.
    pub fn for_mail() -> Self {
        Self::new(1, Duration::from_secs(60))
    }

    /// Takes one token for `key` if it has one, and says whether it did.
    pub fn allow(&mut self, key: &K, now: Instant) -> bool {
        let (burst, every) = (self.burst as f64, self.every);
        if !self.buckets.contains_key(key) && self.buckets.len() >= MAX_KEYS {
            self.forget_refilled(now);
            if self.buckets.len() >= MAX_KEYS {
                return false;
            }
        }
        let bucket = self.buckets.entry(key.clone()).or_insert(Bucket { tokens: burst, at: now });
        let refill = if every.is_zero() { burst } else { now.saturating_duration_since(bucket.at).as_secs_f64() / every.as_secs_f64() };
        bucket.tokens = (bucket.tokens + refill).min(burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drops every key whose bucket would be full by `now`: remembering it
    /// changes nothing, since a new bucket starts full too.
    pub fn forget_refilled(&mut self, now: Instant) {
        let (burst, every) = (self.burst as f64, self.every);
        self.buckets.retain(|_, b| {
            let refill = if every.is_zero() { burst } else { now.saturating_duration_since(b.at).as_secs_f64() / every.as_secs_f64() };
            b.tokens + refill < burst
        });
    }

    /// How many keys are remembered now; for tests and the node's stats.
    pub fn len(&self) -> usize {
        self.buckets.len()
    }

    /// Whether no key is remembered.
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }
}

/// Bursts, refills, independence of keys and the memory bound.
#[cfg(test)]
mod tests {
    use super::*;

    /// The burst is allowed, the next is not, and a refill lets one more.
    #[test]
    fn bursts_then_refills() {
        let mut limit = RateLimit::new(3, Duration::from_secs(20));
        let t = Instant::now();
        assert!(limit.allow(&"a", t));
        assert!(limit.allow(&"a", t));
        assert!(limit.allow(&"a", t));
        assert!(!limit.allow(&"a", t));
        assert!(!limit.allow(&"a", t + Duration::from_secs(19)));
        assert!(limit.allow(&"a", t + Duration::from_secs(21)));
        assert!(!limit.allow(&"a", t + Duration::from_secs(22)));
    }

    /// One key running out does not touch another.
    #[test]
    fn keys_are_independent() {
        let mut limit = RateLimit::for_mail();
        let t = Instant::now();
        assert!(limit.allow(&1, t));
        assert!(!limit.allow(&1, t));
        assert!(limit.allow(&2, t));
    }

    /// Fully refilled keys are forgotten; recently used ones are kept.
    #[test]
    fn refilled_keys_are_forgotten() {
        let mut limit = RateLimit::new(1, Duration::from_secs(10));
        let t = Instant::now();
        limit.allow(&1, t);
        limit.allow(&2, t + Duration::from_secs(8));
        limit.forget_refilled(t + Duration::from_secs(11));
        assert_eq!(limit.len(), 1, "key 1 has refilled, key 2 has not");
    }

    /// A burst of 0 still allows one, rather than nothing ever.
    #[test]
    fn zero_burst_means_one() {
        let mut limit = RateLimit::new(0, Duration::from_secs(10));
        assert!(limit.allow(&"x", Instant::now()));
    }
}
