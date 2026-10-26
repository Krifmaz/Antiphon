//! Blinded rendezvous tokens for relays (spec §3 "What a relay operator can
//! see": "Blinded session tokens, no persistent user identifiers at the
//! relay").
//!
//! A relay has to pair two sockets somehow, and the obvious key -- the
//! callee's identity -- would hand the operator a log of who is called by
//! whom. Instead both sides meet on a 32-byte token that:
//!
//! - **only the two of them can compute.** A pairwise token is derived
//!   from the static X25519 Diffie-Hellman of their two prekeys, which an
//!   operator cannot do even knowing both public identities. An invite
//!   token is derived from a secret carried only in the invite link.
//! - **changes every day.** The day number is mixed in, so the operator
//!   sees unrelated random values from one day to the next instead of a
//!   stable pseudonym it could follow.
//!
//! The listening side registers yesterday's, today's and tomorrow's token
//! so two clocks either side of midnight still meet; the dialling side
//! uses today's.

use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::PublicKey;

use crate::identity::IdentityKeyPair;

const PAIRWISE_SALT: &[u8] = b"antiphon/relay/pairwise/v1";
const INVITE_SALT: &[u8] = b"antiphon/relay/invite/v1";

pub const TOKEN_LEN: usize = 32;
pub type RelayToken = [u8; TOKEN_LEN];

/// Days since the Unix epoch, the rotation period for every token here.
pub fn day_number(unix_secs: u64) -> u64 {
    unix_secs / 86_400
}

/// The token two contacts meet on, for `day`. Symmetric: each side
/// computes the same value from its own secret and the other's prekey.
pub fn pairwise(identity: &IdentityKeyPair, their_prekey: &[u8; 32], day: u64) -> RelayToken {
    let shared = identity.prekey_secret().diffie_hellman(&PublicKey::from(*their_prekey));
    expand(PAIRWISE_SALT, shared.as_bytes(), day)
}

/// The token a stranger holding your invite link dials, for `day`.
pub fn from_invite_secret(secret: &[u8; 32], day: u64) -> RelayToken {
    expand(INVITE_SALT, secret, day)
}

/// The days a listener registers for, so a dialler whose clock is on the
/// other side of midnight still finds them.
pub fn listening_days(today: u64) -> [u64; 3] {
    [today.saturating_sub(1), today, today + 1]
}

fn expand(salt: &[u8], ikm: &[u8], day: u64) -> RelayToken {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8; TOKEN_LEN];
    hk.expand(&day.to_be_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_the_same_pairwise_token() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let day = 20_000;
        assert_eq!(
            pairwise(&alice, &bob.public().prekey, day),
            pairwise(&bob, &alice.public().prekey, day)
        );
    }

    #[test]
    fn a_third_party_lands_somewhere_else() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        assert_ne!(
            pairwise(&alice, &bob.public().prekey, 1),
            pairwise(&mallory, &bob.public().prekey, 1)
        );
    }

    #[test]
    fn tokens_rotate_daily() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        assert_ne!(pairwise(&alice, &bob.public().prekey, 1), pairwise(&alice, &bob.public().prekey, 2));
        let secret = [7u8; 32];
        assert_ne!(from_invite_secret(&secret, 1), from_invite_secret(&secret, 2));
    }

    #[test]
    fn invite_and_pairwise_tokens_never_collide_by_construction() {
        // Different salts: an invite secret that happened to equal a DH
        // output still lands on a different token.
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let shared = alice.prekey_secret().diffie_hellman(&PublicKey::from(bob.public().prekey));
        assert_ne!(
            pairwise(&alice, &bob.public().prekey, 5),
            from_invite_secret(shared.as_bytes(), 5)
        );
    }

    #[test]
    fn listening_covers_midnight_either_way() {
        assert_eq!(listening_days(100), [99, 100, 101]);
        assert_eq!(listening_days(0), [0, 0, 1]);
    }
}
