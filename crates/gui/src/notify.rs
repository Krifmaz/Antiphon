//! Notification and ring rules (spec §9 "Receiver settings always win",
//! §13 "notification controls -- schedules, do-not-disturb").
//!
//! The spec states the guarantee unusually precisely, and it is worth
//! quoting because it determines the shape of this module:
//!
//! > "A caller can never force a ring through someone's do-not-disturb.
//! > Silent is a courtesy on the sender's side; the guarantee lives on the
//! > receiver's side."
//!
//! So the decision is made *here*, on the receiving machine, from the
//! receiver's own settings. Nothing in an incoming call can raise the
//! outcome -- a caller can only ever ask for less attention than the
//! receiver's rules allow, never more. That is enforced by the type: the
//! caller's wish is an input to [`decide`], and every path through it
//! either honours the receiver's rule or lowers the result.
//!
//! Schedules are wall-clock minutes within a day so that a window can wrap
//! midnight, which is the case people actually want ("quiet from 23:00 to
//! 07:00") and the one a naive start-less-than-end comparison gets wrong.

use serde::{Deserialize, Serialize};

/// How much attention a caller is allowed to demand.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RingRule {
    /// Ring out loud.
    #[default]
    Always,
    /// Show up quietly; never make a sound.
    SilentOnly,
    /// Not even a quiet notification.
    Never,
}

impl RingRule {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => RingRule::SilentOnly,
            2 => RingRule::Never,
            _ => RingRule::Always,
        }
    }

    pub fn to_i32(self) -> i32 {
        match self {
            RingRule::Always => 0,
            RingRule::SilentOnly => 1,
            RingRule::Never => 2,
        }
    }
}

/// What actually happens when a call arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Ring audibly.
    Ring,
    /// Appear, silently.
    Notify,
    /// Nothing at all.
    Ignore,
}

/// A do-not-disturb window, in minutes from midnight.
///
/// `start == end` means the window is empty rather than covering the whole
/// day: "quiet from 9 to 9" almost certainly means someone mis-set it, and
/// silently muting them for 24 hours is the worse failure.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct QuietHours {
    pub enabled: bool,
    pub start_minute: u32,
    pub end_minute: u32,
}

impl QuietHours {
    /// Whether `minute_of_day` falls inside the window.
    pub fn covers(&self, minute_of_day: u32) -> bool {
        if !self.enabled || self.start_minute == self.end_minute {
            return false;
        }
        let m = minute_of_day % 1440;
        if self.start_minute < self.end_minute {
            m >= self.start_minute && m < self.end_minute
        } else {
            // Wraps midnight: 23:00 to 07:00 is two ranges, not one.
            m >= self.start_minute || m < self.end_minute
        }
    }
}

/// Decides what an incoming call does.
///
/// `caller_wants_silent` is the caller's *request* (spec §9's silent
/// call). It can only ever lower the outcome: there is deliberately no
/// input that raises it, because the guarantee is that a caller cannot
/// reach through the receiver's settings.
pub fn decide(
    rule: RingRule,
    quiet: QuietHours,
    minute_of_day: u32,
    caller_wants_silent: bool,
) -> Outcome {
    // The receiver's own rule is checked first and can end it outright.
    let base = match rule {
        RingRule::Never => return Outcome::Ignore,
        RingRule::SilentOnly => Outcome::Notify,
        RingRule::Always => Outcome::Ring,
    };

    // Do-not-disturb lowers, never raises.
    let after_quiet = if quiet.covers(minute_of_day) {
        match base {
            Outcome::Ring => Outcome::Notify,
            other => other,
        }
    } else {
        base
    };

    // The caller's courtesy is applied last and can only lower further.
    if caller_wants_silent {
        match after_quiet {
            Outcome::Ring => Outcome::Notify,
            other => other,
        }
    } else {
        after_quiet
    }
}

/// The rule for a caller who is not a contact but shares group chats or
/// communities with you, from the rules you set on each (spec §9
/// "per-community ring settings"). `None` when you set none, so your
/// default applies.
///
/// A 1:1 call doesn't say which community it came "from", so the most
/// restrictive of them wins: setting a community to Never must hold even
/// if you also share a louder one with the same person. A saved
/// contact's own rule is checked before this and overrides it.
pub fn shared_rule(rules: impl IntoIterator<Item = RingRule>) -> Option<RingRule> {
    rules.into_iter().max_by_key(|r| r.to_i32())
}

/// Minutes since midnight, local time, for feeding [`decide`].
pub fn current_minute_of_day() -> u32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Deliberately UTC: pulling in a timezone database for a
    // quiet-hours window is a large dependency, and the honest fix is to
    // say so rather than to guess an offset that is wrong half the year.
    ((secs % 86_400) / 60) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: QuietHours = QuietHours { enabled: false, start_minute: 0, end_minute: 0 };

    fn quiet(start: u32, end: u32) -> QuietHours {
        QuietHours { enabled: true, start_minute: start, end_minute: end }
    }

    #[test]
    fn a_normal_call_rings() {
        assert_eq!(decide(RingRule::Always, NONE, 12 * 60, false), Outcome::Ring);
    }

    #[test]
    fn silent_only_never_rings_however_the_call_was_placed() {
        // The receiver's rule, not the caller's intent, decides this.
        assert_eq!(decide(RingRule::SilentOnly, NONE, 12 * 60, false), Outcome::Notify);
        assert_eq!(decide(RingRule::SilentOnly, NONE, 12 * 60, true), Outcome::Notify);
    }

    #[test]
    fn never_means_never() {
        for silent in [true, false] {
            for minute in [0, 8 * 60, 23 * 60] {
                assert_eq!(
                    decide(RingRule::Never, NONE, minute, silent),
                    Outcome::Ignore,
                    "Never was overridden at minute {minute}"
                );
            }
        }
    }

    #[test]
    fn a_caller_can_ask_for_less_attention_but_never_more() {
        // This is the whole guarantee. A silent call lowers a ring to a
        // notification; nothing a caller does raises anything.
        assert_eq!(decide(RingRule::Always, NONE, 12 * 60, true), Outcome::Notify);
        assert_eq!(decide(RingRule::Never, NONE, 12 * 60, false), Outcome::Ignore);
        assert_eq!(decide(RingRule::SilentOnly, NONE, 12 * 60, false), Outcome::Notify);
    }

    #[test]
    fn quiet_hours_lower_a_ring_to_a_notification() {
        // 23:00 to 07:00, checked at 02:00.
        assert_eq!(
            decide(RingRule::Always, quiet(23 * 60, 7 * 60), 2 * 60, false),
            Outcome::Notify
        );
    }

    #[test]
    fn quiet_hours_that_wrap_midnight_cover_both_sides_of_it() {
        let q = quiet(23 * 60, 7 * 60);
        assert!(q.covers(23 * 60), "23:00 should be inside");
        assert!(q.covers(2 * 60), "02:00 should be inside");
        assert!(q.covers(6 * 60 + 59), "06:59 should be inside");
        assert!(!q.covers(7 * 60), "07:00 is the end, exclusive");
        assert!(!q.covers(12 * 60), "midday should be outside");
    }

    #[test]
    fn a_same_day_window_behaves_normally() {
        let q = quiet(9 * 60, 17 * 60);
        assert!(q.covers(12 * 60));
        assert!(!q.covers(8 * 60));
        assert!(!q.covers(18 * 60));
    }

    #[test]
    fn a_disabled_window_covers_nothing() {
        let q = QuietHours { enabled: false, start_minute: 0, end_minute: 1439 };
        assert!(!q.covers(12 * 60));
    }

    #[test]
    fn an_empty_window_does_not_silently_mute_the_whole_day() {
        // "Quiet from 9 to 9" is almost certainly a mis-set window.
        // Treating it as 24 hours of silence is the worse failure.
        let q = quiet(9 * 60, 9 * 60);
        for minute in [0, 9 * 60, 12 * 60, 23 * 60] {
            assert!(!q.covers(minute), "empty window covered minute {minute}");
        }
    }

    #[test]
    fn quiet_hours_do_not_resurrect_a_never_rule() {
        assert_eq!(
            decide(RingRule::Never, quiet(23 * 60, 7 * 60), 12 * 60, false),
            Outcome::Ignore
        );
    }

    #[test]
    fn ring_rules_round_trip_through_their_integer_form() {
        for rule in [RingRule::Always, RingRule::SilentOnly, RingRule::Never] {
            assert_eq!(RingRule::from_i32(rule.to_i32()), rule);
        }
        // Unknown values fall back to the least surprising behaviour
        // rather than silencing someone by accident.
        assert_eq!(RingRule::from_i32(99), RingRule::Always);
        assert_eq!(RingRule::from_i32(-1), RingRule::Always);
    }

    #[test]
    fn a_minute_past_the_end_of_the_day_wraps_rather_than_escaping() {
        let q = quiet(23 * 60, 7 * 60);
        // 1440 is 00:00 the next day.
        assert!(q.covers(1440 + 60));
    }

    #[test]
    fn the_quietest_shared_community_wins() {
        assert_eq!(shared_rule([]), None);
        assert_eq!(shared_rule([RingRule::Always, RingRule::SilentOnly]), Some(RingRule::SilentOnly));
        assert_eq!(shared_rule([RingRule::Never, RingRule::Always]), Some(RingRule::Never));
    }
}
