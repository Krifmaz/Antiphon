//! Per-community identity (spec §7 "Statuses & presence -- custom text,
//! per-community identity").
//!
//! Identity here is a key (see [`crate::identity`]), so "a different
//! identity in each community" can mean two things, and the app offers
//! both:
//!
//! 1. **A different face** -- another name, status and picture in that
//!    community, under the same key. Cheap, and all a lot of people want,
//!    but it hides nothing: the key in the member list is the one your
//!    contacts have pinned, so anyone in two of your communities -- or any
//!    contact who sees one member list -- knows it is you.
//! 2. **A separate key** ([`community_identity`]) -- a whole second
//!    identity, derived from your master seed and the community's id, that
//!    is your MLS credential, your manifest entry, your card and your
//!    mailbox labels there. Two communities then hold two unrelated keys,
//!    and nothing either one sees can be matched with the other or with
//!    your contacts' copy of your key.
//!
//! # Why derived, not generated
//!
//! A random key per community would be one more secret to back up, and
//! losing it would lose your place in that community. Derived through
//! HKDF from the master seed -- the same way the identity's own keys are
//! derived in [`IdentityKeyPair::from_master_seed`] -- it is restored by
//! the recovery phrase or the Shamir shares like everything else, and two
//! of your devices arrive at the same one without talking. HKDF is one way,
//! so a persona key (or its seed, which its mailbox labels are derived
//! from) leaking says nothing about your main key or any other persona.
//!
//! # What it does not hide (the tradeoffs)
//!
//! - **The person who invites you** knows both keys: they invited your
//!   main key, and your answer -- sealed with your main key, so they know
//!   it is the person they invited -- names the persona to add. Nobody
//!   else sees that answer, and your main key is never written into the
//!   community's manifest. Someone you don't want to know shouldn't be the
//!   one to invite you.
//! - **Your contacts in that community** don't learn it from the keys, but
//!   could from what you write, your name, or when you are online.
//!   Unlinkable keys are not anonymity.
//! - **Nodes**: your persona's mail goes to the same mailbox nodes as
//!   yours (under unrelated labels, see [`crate::mail::inbox_label`]), and
//!   from the same network address. A node operator who is in the
//!   community too can line those up by timing; a VPN (spec §6) is the
//!   answer to that, not this.
//! - **Live calls** are with your main key. Community messages for a
//!   separate-identity community never go over a call, because the call
//!   would say who sent them -- they always go by mailbox, which is slower.
//! - **Contacts** stay on your main identity: a 1:1 contact is someone you
//!   chose to be known to, and splitting them across keys would split
//!   their history and verification too.
//! - It is fixed when you join or create. Moving an existing membership to
//!   another key would publish "this key is now that key" to every member
//!   -- the very link it exists to avoid -- so changing it means leaving
//!   and being invited again.

use hkdf::Hkdf;
use sha2::Sha256;

use crate::identity::IdentityKeyPair;

/// Frozen: changing this moves every separate identity to a new key, and
/// everyone using one out of their communities. See "Frozen labels" in
/// `lib.rs`.
const PERSONA_SALT: &[u8] = b"antiphon/community/persona/v1";

/// The identity `main` uses in the community whose manifest id is
/// `community`, when it uses a separate one there.
///
/// Deterministic: the same seed and community always give the same key,
/// so it survives a restore from the recovery phrase or shares. The
/// result is a complete [`IdentityKeyPair`] with its own master seed, so
/// its prekey, mailbox labels and fetch secrets are all its own.
pub fn community_identity(main: &IdentityKeyPair, community: &[u8; 32]) -> IdentityKeyPair {
    let mut seed = [0u8; 32];
    Hkdf::<Sha256>::new(Some(PERSONA_SALT), &main.master_seed())
        .expand(community, &mut seed)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    IdentityKeyPair::from_master_seed(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::verify_signature;
    use crate::mnemonic::{phrase_from_seed, seed_from_phrase};

    #[test]
    fn two_communities_get_unrelated_keys() {
        let me = IdentityKeyPair::generate();
        let a = community_identity(&me, &[1; 32]);
        let b = community_identity(&me, &[2; 32]);
        let ka = a.public().identity_key;
        let kb = b.public().identity_key;
        assert_ne!(ka, kb);
        assert_ne!(ka, me.public().identity_key);
        assert_ne!(kb, me.public().identity_key);
        // Each a whole identity: its prekey binding checks out.
        assert!(a.public().verify().is_ok());
        assert!(b.public().verify().is_ok());
        // And different people get different keys in the same community.
        let other = IdentityKeyPair::generate();
        assert_ne!(community_identity(&other, &[1; 32]).public().identity_key, ka);
    }

    #[test]
    fn the_key_comes_back_from_the_recovery_phrase() {
        let me = IdentityKeyPair::generate();
        let community = [7u8; 32];
        let before = community_identity(&me, &community).public();
        let phrase = phrase_from_seed(&me.master_seed());
        let restored = IdentityKeyPair::from_master_seed(seed_from_phrase(&phrase).unwrap());
        assert_eq!(community_identity(&restored, &community).public(), before);
    }

    #[test]
    fn the_key_comes_back_from_shamir_shares() {
        let me = IdentityKeyPair::generate();
        let community = [9u8; 32];
        let before = community_identity(&me, &community).public();
        let shares = crate::shamir::split(&me.master_seed(), 2, 3).unwrap();
        let seed = crate::shamir::combine(&shares[1..]).unwrap();
        let restored = IdentityKeyPair::from_master_seed(seed);
        assert_eq!(community_identity(&restored, &community).public(), before);
    }

    #[test]
    fn what_is_signed_in_one_community_is_not_yours_in_another() {
        let me = IdentityKeyPair::generate();
        let a = community_identity(&me, &[1; 32]);
        let b = community_identity(&me, &[2; 32]);
        let message = b"posted in A";
        let signature = a.sign(message);
        assert!(verify_signature(&a.public().identity_key, message, &signature).is_ok());
        // Neither the key B knows you by nor your main key verifies it, so
        // a member of B holding it can't tie it to anyone they know.
        assert!(verify_signature(&b.public().identity_key, message, &signature).is_err());
        assert!(verify_signature(&me.public().identity_key, message, &signature).is_err());
    }

    #[test]
    fn mailbox_labels_differ_too() {
        let me = IdentityKeyPair::generate();
        let a = community_identity(&me, &[1; 32]);
        let node = "mail.example:7000";
        assert_ne!(crate::mail::inbox_label(&a, node), crate::mail::inbox_label(&me, node));
        assert_ne!(a.master_seed(), me.master_seed());
    }
}
