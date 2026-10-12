//! Presence (spec §7 "Statuses & presence").
//!
//! Presence is a privacy decision wearing a convenience costume, and it is
//! worth being explicit about which side this lands on.
//!
//! "Online" is not a fact about a person; it is a claim they broadcast.
//! On a service, presence is computed from your connection to the server,
//! so it is published whether you meant to or not -- and "last seen"
//! timestamps become a surveillance tool that people genuinely use to
//! monitor each other. There is no server here, which means presence must
//! be *sent*, which means it can simply not be sent.
//!
//! So:
//!
//! - Presence is **published to contacts you have, and nobody else**.
//!   There is no directory to broadcast into.
//! - **Appearing offline is a real option**, and when chosen, nothing is
//!   sent at all. Not "offline" -- nothing. A status that lies is still a
//!   status, and an app that transmits one while claiming otherwise has
//!   sold the user out quietly.
//! - **No "last seen".** The single most requested presence feature and
//!   the one most used to control people. Its absence is deliberate.

use serde::{Deserialize, Serialize};

/// What someone is telling their contacts.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Presence {
    #[default]
    Online,
    /// Around, but not paying attention.
    Away,
    /// Here and asking not to be interrupted. Distinct from the ring rules
    /// in [`crate::notify`]: this is what you *say*, that is what the app
    /// *enforces*. Saying "busy" changes nothing on its own.
    Busy,
    /// Nothing is published at all.
    Invisible,
}

impl Presence {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Presence::Away,
            2 => Presence::Busy,
            3 => Presence::Invisible,
            _ => Presence::Online,
        }
    }

    pub fn to_i32(self) -> i32 {
        match self {
            Presence::Online => 0,
            Presence::Away => 1,
            Presence::Busy => 2,
            Presence::Invisible => 3,
        }
    }

    /// Whether anything should be sent at all.
    ///
    /// The whole guarantee of invisible mode: not a status that says
    /// "offline", but silence.
    pub fn should_publish(self) -> bool {
        self != Presence::Invisible
    }

    /// The word a contact sees.
    pub fn label(self) -> &'static str {
        match self {
            Presence::Online => "Online",
            Presence::Away => "Away",
            Presence::Busy => "Busy",
            // Never actually sent; here so the local UI can name the mode.
            Presence::Invisible => "Invisible",
        }
    }

    /// What this means for the person choosing it.
    pub fn explain(self) -> &'static str {
        match self {
            Presence::Online => "Your contacts can see you're around.",
            Presence::Away => "Your contacts see you as away.",
            Presence::Busy => {
                "Your contacts see you're busy. This is only a label -- to actually stop \
                 calls ringing, use the ringing settings."
            }
            Presence::Invisible => {
                "Nothing is sent. Your contacts see no status at all, rather than seeing you \
                 as offline."
            }
        }
    }
}

/// What a contact's presence looks like from your side.
///
/// `Unknown` is the honest state for someone you are not connected to.
/// Without a server there is nothing watching them, so anything else would
/// be a guess presented as knowledge.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PeerPresence {
    #[default]
    Unknown,
    Known(Presence),
}

impl PeerPresence {
    pub fn label(self) -> &'static str {
        match self {
            // Not "Offline": we do not know that. We know we are not
            // talking to them, which is a different thing.
            PeerPresence::Unknown => "",
            PeerPresence::Known(p) => p.label(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_publishes_nothing_at_all() {
        // The guarantee: not a status reading "offline", but silence. An
        // app that transmits something while claiming invisibility has
        // quietly sold the user out.
        assert!(!Presence::Invisible.should_publish());
        for p in [Presence::Online, Presence::Away, Presence::Busy] {
            assert!(p.should_publish(), "{p:?} should be published");
        }
    }

    #[test]
    fn an_unconnected_contact_reads_as_nothing_rather_than_offline() {
        // Claiming "offline" would be asserting knowledge nobody has --
        // there is no server watching them.
        assert_eq!(PeerPresence::Unknown.label(), "");
        assert_eq!(PeerPresence::Known(Presence::Away).label(), "Away");
    }

    #[test]
    fn busy_does_not_pretend_to_enforce_anything() {
        // The distinction people get wrong: a label is not a rule. Saying
        // so up front beats someone discovering it when a call rings.
        let text = Presence::Busy.explain();
        assert!(text.contains("only a label"), "{text}");
        assert!(text.contains("ringing settings"), "should point at what does work: {text}");
    }

    #[test]
    fn invisible_explains_that_it_is_silence_not_a_status() {
        let text = Presence::Invisible.explain();
        assert!(text.contains("Nothing is sent"), "{text}");
        assert!(text.contains("rather than seeing you as offline"), "{text}");
    }

    #[test]
    fn presence_round_trips_through_its_integer_form() {
        for p in [Presence::Online, Presence::Away, Presence::Busy, Presence::Invisible] {
            assert_eq!(Presence::from_i32(p.to_i32()), p);
        }
        // An unrecognised value falls back to the most visible option
        // rather than silently making someone invisible -- a status that
        // silently stops being sent is worse than one that is wrong.
        assert_eq!(Presence::from_i32(99), Presence::Online);
        assert_eq!(Presence::from_i32(-1), Presence::Online);
    }

    #[test]
    fn every_state_has_a_label_and_an_explanation() {
        for p in [Presence::Online, Presence::Away, Presence::Busy, Presence::Invisible] {
            assert!(!p.label().is_empty(), "{p:?} has no label");
            assert!(!p.explain().is_empty(), "{p:?} has no explanation");
        }
    }

    #[test]
    fn presence_carries_no_timestamp() {
        // A deliberate absence, asserted structurally rather than by
        // grepping the source: what is published is a state and nothing
        // else, so there is no "last seen" to derive even if someone
        // wanted one. Adding a field here would fail this immediately.
        for p in [Presence::Online, Presence::Away, Presence::Busy] {
            let json = serde_json::to_string(&p).unwrap();
            // A bare enum serialises to a string, not an object -- there
            // is nowhere for a timestamp to hide.
            assert!(json.starts_with('"'), "presence gained fields: {json}");
            for suspicious in ["secs", "time", "seen", "at", "since"] {
                assert!(
                    !json.to_lowercase().contains(suspicious),
                    "presence looks like it now carries {suspicious}: {json}"
                );
            }
        }
    }
}
