//! Contact introduction (spec §4: "a mutual friend vouches by passing a
//! key through existing encrypted chat").
//!
//! This is the one discovery mechanism that works with no infrastructure
//! at all, which is why it is worth building before any of the others.
//! Alice already has a verified channel to Bob and to Carol; she can hand
//! Bob's key to Carol over it, and Carol gets a key with a provenance
//! rather than a stranger's claim about themselves.
//!
//! What an introduction is and is not:
//!
//! - It **is** evidence about where a key came from. "Alice sent me this"
//!   is a real, checkable fact if you trust Alice's channel.
//! - It is **not** verification. Alice can be wrong, or lying, or have
//!   been fooled herself. An introduced contact is therefore recorded as
//!   introduced-by-Alice and still **unverified** -- the verification code
//!   is what settles it, and nothing else can.
//!
//! The distinction matters because the tempting shortcut is to mark an
//! introduced key as trusted. That would make one compromised contact able
//! to launder keys into everyone else's address book, which is precisely
//! the attack a verification code exists to catch.

use serde::{Deserialize, Serialize};

/// Someone's key, passed along by a mutual contact.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Introduction {
    /// The key being vouched for.
    pub identity_key: String,
    /// What the introducer calls them.
    pub petname: String,
    /// Optional note from the introducer.
    #[serde(default)]
    pub note: String,
}

/// What happened when an introduction arrived.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Added, unverified, with the introducer recorded.
    Added,
    /// Already a contact under this key -- nothing to do.
    AlreadyKnown,
    /// The introduced key is one this device has blocked.
    Blocked,
    /// The key was malformed.
    Malformed,
    /// The introducer tried to introduce *themselves*.
    SelfIntroduction,
}

impl Outcome {
    pub fn explain(&self, introducer: &str, petname: &str) -> String {
        match self {
            Outcome::Added => format!(
                "Added {petname}, introduced by {introducer}. They are not verified yet -- \
                 compare the code with them on your first call."
            ),
            Outcome::AlreadyKnown => format!("You already have {petname}."),
            Outcome::Blocked => {
                format!("{introducer} tried to introduce someone you've blocked. Ignored.")
            }
            Outcome::Malformed => {
                format!("{introducer} sent an introduction that couldn't be read.")
            }
            Outcome::SelfIntroduction => format!(
                "{introducer} tried to introduce themselves, which proves nothing. Ignored."
            ),
        }
    }
}

/// Whether a string is a plausible identity key.
fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit())
}

/// Decides what to do with an incoming introduction.
///
/// Pure, so the rules can be tested without a filesystem or a socket --
/// and so the one rule that matters (introduced is *not* verified) is
/// visible in one place rather than implied by a call site.
pub fn evaluate(
    introduction: &Introduction,
    introducer_key: &str,
    known_keys: &[String],
    blocked_keys: &[String],
) -> Outcome {
    if !valid_key(&introduction.identity_key) {
        return Outcome::Malformed;
    }
    let key = introduction.identity_key.to_lowercase();

    // Vouching for yourself is not vouching. Anyone can claim their own
    // key is theirs -- that is what the handshake already proves, and it
    // says nothing an introduction is supposed to add.
    if key == introducer_key.to_lowercase() {
        return Outcome::SelfIntroduction;
    }
    if blocked_keys.iter().any(|b| b.eq_ignore_ascii_case(&key)) {
        return Outcome::Blocked;
    }
    if known_keys.iter().any(|k| k.eq_ignore_ascii_case(&key)) {
        return Outcome::AlreadyKnown;
    }
    Outcome::Added
}

/// The note stored on a contact recording where their key came from.
pub fn provenance_note(introducer: &str) -> String {
    if introducer.is_empty() {
        "Introduced by a contact.".to_string()
    } else {
        format!("Introduced by {introducer}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOB: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const ALICE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const CAROL: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn intro(key: &str) -> Introduction {
        Introduction { identity_key: key.to_string(), petname: "Bob".into(), note: String::new() }
    }

    #[test]
    fn a_new_key_from_a_mutual_contact_is_added() {
        assert_eq!(evaluate(&intro(BOB), ALICE, &[], &[]), Outcome::Added);
    }

    #[test]
    fn an_introduction_never_counts_as_verification() {
        // The whole point. Alice can be wrong, lying, or fooled herself;
        // treating her word as proof would let one compromised contact
        // launder keys into everyone's address book.
        let message = Outcome::Added.explain("Alice", "Bob");
        assert!(message.contains("not verified"), "{message}");
        assert!(message.contains("compare the code"), "{message}");
    }

    #[test]
    fn someone_introducing_themselves_is_refused() {
        // Anyone can claim their own key is theirs -- the handshake
        // already proves that, and an introduction is supposed to add
        // something.
        assert_eq!(evaluate(&intro(ALICE), ALICE, &[], &[]), Outcome::SelfIntroduction);
    }

    #[test]
    fn case_does_not_let_a_self_introduction_slip_through() {
        assert_eq!(
            evaluate(&intro(&ALICE.to_uppercase()), ALICE, &[], &[]),
            Outcome::SelfIntroduction
        );
    }

    #[test]
    fn a_blocked_person_cannot_be_reintroduced() {
        // Otherwise blocking someone lasts until any contact passes their
        // key along again.
        assert_eq!(
            evaluate(&intro(BOB), ALICE, &[], &[BOB.to_string()]),
            Outcome::Blocked
        );
    }

    #[test]
    fn blocking_beats_already_knowing_them() {
        // Order matters: a blocked contact you also have saved must not
        // fall through to the harmless "already known" path.
        assert_eq!(
            evaluate(&intro(BOB), ALICE, &[BOB.to_string()], &[BOB.to_string()]),
            Outcome::Blocked
        );
    }

    #[test]
    fn someone_you_already_have_is_left_alone() {
        assert_eq!(
            evaluate(&intro(BOB), ALICE, &[BOB.to_string()], &[]),
            Outcome::AlreadyKnown
        );
    }

    #[test]
    fn a_malformed_key_is_refused() {
        for bad in ["", "nothex", &"z".repeat(64), &BOB[..10], &format!("{BOB}extra")] {
            assert_eq!(evaluate(&intro(bad), ALICE, &[], &[]), Outcome::Malformed, "{bad}");
        }
    }

    #[test]
    fn a_third_partys_key_is_added_normally() {
        assert_eq!(evaluate(&intro(CAROL), ALICE, &[BOB.to_string()], &[]), Outcome::Added);
    }

    #[test]
    fn every_outcome_says_who_was_involved() {
        for outcome in [
            Outcome::Added,
            Outcome::AlreadyKnown,
            Outcome::Blocked,
            Outcome::Malformed,
            Outcome::SelfIntroduction,
        ] {
            let message = outcome.explain("Alice", "Bob");
            assert!(!message.is_empty(), "{outcome:?} has no wording");
            assert!(
                message.contains("Alice") || message.contains("Bob"),
                "{outcome:?} names nobody: {message}"
            );
        }
    }

    #[test]
    fn provenance_is_recorded_even_without_a_name() {
        assert!(provenance_note("Alice").contains("Alice"));
        assert!(!provenance_note("").is_empty());
    }

    #[test]
    fn introductions_round_trip_through_json() {
        let i = Introduction {
            identity_key: BOB.to_string(),
            petname: "Bob".into(),
            note: "from the climbing group".into(),
        };
        let json = serde_json::to_string(&i).unwrap();
        assert_eq!(serde_json::from_str::<Introduction>(&json).unwrap(), i);
    }
}
