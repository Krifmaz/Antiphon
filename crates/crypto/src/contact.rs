//! Contact trust: pinning, trust-on-first-use, and key-change detection
//! (spec §4 "Knowing it's really them").
//!
//! A contact is **an identity key with a local nickname**, never a
//! username and never an address. Two consequences fall out of that:
//!
//! - Addresses can change freely without the contact changing. Someone
//!   moving house, switching networks, or republishing a route record is
//!   still the same contact.
//! - There is no global namespace, so there is nothing to squat and no
//!   directory to scrape. Spec §4's "no global username search by default"
//!   is structural here rather than a policy toggle.
//!
//! The nickname is a **petname**: it is what *you* call them, stored on
//! your device, and it carries no authority. Someone can put anything they
//! like in their own display name; only your petname decides what you see,
//! which removes the display-name impersonation problem entirely.
//!
//! Trust has three states in practice, and this module's job is deciding
//! which one applies when a key shows up:
//!
//! | Situation | What it means |
//! |---|---|
//! | first contact | nothing pinned yet -- pin it (trust on first use) |
//! | key matches | the pinned key came back; nothing to say |
//! | key changed | either they reinstalled without a recovery phrase, or someone is intercepting |
//!
//! That last row is the one that matters, and the reason it must be
//! surfaced loudly rather than resolved silently: a machine-in-the-middle
//! holding a perfectly valid identity is indistinguishable from a friend
//! with a new phone, *except* by the fact that the key changed and by
//! comparing a [`crate::verification_code`] out of band.

use crate::identity::PublicIdentity;

/// How much a contact's key has been checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrustLevel {
    /// Pinned on first contact but never compared out of band. Fine for
    /// most people, and honest about what it is: nobody has confirmed the
    /// key belongs to the human they think it does.
    Unverified,
    /// A verification code was compared out of band and matched.
    Verified,
}

/// What a contact's pinned key looks like on disk.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PinnedContact {
    /// The identity key -- this *is* the contact.
    pub identity_key: [u8; 32],
    /// What you call them. Local to your device.
    pub petname: String,
    pub trust: TrustLevel,
    pub first_seen_secs: u64,
}

/// The outcome of presenting an identity for a contact.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TrustDecision {
    /// Nobody was pinned under this petname yet. Pin and carry on.
    FirstContact,
    /// The pinned key came back unchanged.
    Unchanged { trust: TrustLevel },
    /// The key is different from the one pinned. The caller must warn and
    /// must not silently replace the pin.
    KeyChanged {
        pinned: [u8; 32],
        presented: [u8; 32],
        /// Whether the previously pinned key had been verified out of
        /// band. A change away from a *verified* key is far more alarming
        /// than one away from a merely pinned key, and the UI should say
        /// so differently.
        was_verified: bool,
    },
}

/// Decides what a presented identity means for a known (or unknown)
/// contact.
///
/// Pure and total on purpose: this is the security decision, so it is
/// testable without a filesystem, a socket, or a UI, and it cannot fail
/// open through an error path.
pub fn evaluate(pinned: Option<&PinnedContact>, presented: &PublicIdentity) -> TrustDecision {
    match pinned {
        None => TrustDecision::FirstContact,
        Some(contact) if contact.identity_key == presented.identity_key => {
            TrustDecision::Unchanged { trust: contact.trust }
        }
        Some(contact) => TrustDecision::KeyChanged {
            pinned: contact.identity_key,
            presented: presented.identity_key,
            was_verified: contact.trust == TrustLevel::Verified,
        },
    }
}

/// A short, stable fingerprint of an identity key, for showing next to a
/// contact so a changed key is visible at a glance.
///
/// Not a substitute for [`crate::verification_code`] -- that one binds
/// *both* parties, which is what defeats a machine-in-the-middle. This is
/// only an at-a-glance label for one key.
pub fn fingerprint(identity_key: &[u8; 32]) -> String {
    let hex = crate::hex::encode(&identity_key[..8]);
    // Pairs of digits joined by colons: "ab:cd:...".
    let mut out = String::with_capacity(8 * 3);
    for (i, pair) in hex.as_bytes().chunks_exact(2).enumerate() {
        if i > 0 {
            out.push(':');
        }
        out.push(char::from(pair[0]));
        out.push(char::from(pair[1]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityKeyPair;

    fn pin(identity: &IdentityKeyPair, petname: &str, trust: TrustLevel) -> PinnedContact {
        PinnedContact {
            identity_key: identity.public().identity_key,
            petname: petname.to_string(),
            trust,
            first_seen_secs: 1_700_000_000,
        }
    }

    #[test]
    fn an_unknown_key_is_first_contact() {
        let stranger = IdentityKeyPair::generate();
        assert_eq!(evaluate(None, &stranger.public()), TrustDecision::FirstContact);
    }

    #[test]
    fn the_same_key_coming_back_is_unchanged() {
        let bob = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Unverified);

        assert_eq!(
            evaluate(Some(&pinned), &bob.public()),
            TrustDecision::Unchanged { trust: TrustLevel::Unverified }
        );
    }

    #[test]
    fn a_different_key_under_a_known_petname_is_flagged() {
        let bob = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Unverified);

        match evaluate(Some(&pinned), &mallory.public()) {
            TrustDecision::KeyChanged { pinned: p, presented, was_verified } => {
                assert_eq!(p, bob.public().identity_key);
                assert_eq!(presented, mallory.public().identity_key);
                assert!(!was_verified);
            }
            other => panic!("expected KeyChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_change_away_from_a_verified_key_is_marked_as_such() {
        let bob = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Verified);

        // The UI needs to distinguish "new phone, probably" from "you
        // checked this in person and it changed anyway".
        match evaluate(Some(&pinned), &mallory.public()) {
            TrustDecision::KeyChanged { was_verified, .. } => assert!(was_verified),
            other => panic!("expected KeyChanged, got {other:?}"),
        }
    }

    #[test]
    fn a_recovered_identity_is_not_a_key_change() {
        use crate::mnemonic::{phrase_from_seed, seed_from_phrase};

        let bob = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Verified);

        // Bob loses his phone and restores from his recovery phrase.
        let phrase = phrase_from_seed(&bob.master_seed());
        let restored = IdentityKeyPair::from_master_seed(seed_from_phrase(&phrase).unwrap());

        // This is what the recovery phrase buys: no alarm, no re-verifying.
        assert_eq!(
            evaluate(Some(&pinned), &restored.public()),
            TrustDecision::Unchanged { trust: TrustLevel::Verified }
        );
    }

    #[test]
    fn a_reinstall_without_a_phrase_does_trip_the_warning() {
        let bob = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Verified);

        // No phrase, so a genuinely new identity -- indistinguishable from
        // an attacker, which is exactly why it must be surfaced.
        let bob_again = IdentityKeyPair::generate();

        assert!(matches!(
            evaluate(Some(&pinned), &bob_again.public()),
            TrustDecision::KeyChanged { .. }
        ));
    }

    #[test]
    fn a_changed_prekey_alone_is_not_a_key_change() {
        let bob = IdentityKeyPair::generate();
        let pinned = pin(&bob, "Bob", TrustLevel::Verified);

        // Only the identity key defines the contact. A rotated prekey --
        // which happens on every migration of the seed format -- must not
        // look like a stranger.
        let mut presented = bob.public();
        presented.prekey = IdentityKeyPair::generate().public().prekey;

        assert_eq!(
            evaluate(Some(&pinned), &presented),
            TrustDecision::Unchanged { trust: TrustLevel::Verified }
        );
    }

    #[test]
    fn fingerprints_are_stable_and_distinguish_keys() {
        let a = IdentityKeyPair::generate();
        let b = IdentityKeyPair::generate();

        assert_eq!(fingerprint(&a.public().identity_key), fingerprint(&a.public().identity_key));
        assert_ne!(fingerprint(&a.public().identity_key), fingerprint(&b.public().identity_key));
        assert_eq!(fingerprint(&a.public().identity_key).split(':').count(), 8);
    }
}
