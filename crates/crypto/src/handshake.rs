use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;
use crate::identity::{IdentityKeyPair, PublicIdentity};
use crate::session::{RatchetStart, Session};

// Frozen under the project's old name -- see "Frozen labels" in `lib.rs`.
//
// v3: the session became a Double Ratchet, whose frames carry a header the
// v2 session never sent. The salt moved so the two fail closed at the
// handshake rather than exchanging frames neither can read.
const ROOT_SALT: &[u8] = b"voip-app/handshake/root/v3";
const INFO_RATCHET_ROOT: &[u8] = b"antiphon/handshake/ratchet-root/v1";
const INFO_A2B: &[u8] = b"voip-app/chain/a2b/v1";
const INFO_B2A: &[u8] = b"voip-app/chain/b2a/v1";

/// One side of an in-progress authenticated handshake (spec §3).
///
/// Both peers are online for a call, so this is an interactive triple-DH --
/// the shape X3DH reduces to when nobody is offline:
///
/// ```text
/// DH_static_eph = DH(own static prekey, their ephemeral)
/// DH_eph_static = DH(own ephemeral,     their static prekey)
/// DH_eph_eph    = DH(own ephemeral,     their ephemeral)
/// ```
///
/// Each term buys something specific, which is why all three are mixed:
///
/// - the two mixed terms **authenticate** -- only the holder of a static
///   secret can compute them, so completing the handshake proves identity
/// - the ephemeral-ephemeral term gives **forward secrecy** -- it dies
///   with the per-call keys, so compromising a long-term identity later
///   does not decrypt a recorded call
///
/// Version 1 of this handshake mixed *only* the ephemeral term. That is
/// forward secret but entirely unauthenticated -- it proved "whoever is at
/// this address right now" and nothing more, which is why `FriendRequest`
/// used to prove nothing and why verification codes were impossible.
/// `ROOT_SALT` carries `v2` so a v1 peer and a v2 peer derive different
/// keys and fail closed, rather than quietly agreeing on a session only
/// one of them believes is authenticated.
///
/// The per-call key is an `x25519_dalek::StaticSecret` rather than an
/// `EphemeralSecret` for a mechanical reason: `EphemeralSecret` is
/// consumed by its single `diffie_hellman` call, and this construction
/// needs two DHs from the same ephemeral. "Ephemeral" here is a property
/// of how the key is used -- generated per handshake, never persisted,
/// zeroized on drop -- not of the Rust type.
pub struct HandshakeInit {
    ephemeral_secret: StaticSecret,
    ephemeral_public: [u8; 32],
}

/// What one side puts on the wire: its public identity plus this call's
/// ephemeral key.
pub struct HandshakeMessage {
    pub identity: PublicIdentity,
    pub ephemeral: [u8; 32],
}

impl HandshakeMessage {
    pub const ENCODED_LEN: usize = PublicIdentity::ENCODED_LEN + 32;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::ENCODED_LEN);
        out.extend_from_slice(&self.identity.encode());
        out.extend_from_slice(&self.ephemeral);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(CryptoError::MalformedIdentity);
        }
        let identity = PublicIdentity::decode(&bytes[..PublicIdentity::ENCODED_LEN])?;
        let mut ephemeral = [0u8; 32];
        ephemeral.copy_from_slice(&bytes[PublicIdentity::ENCODED_LEN..]);
        Ok(Self { identity, ephemeral })
    }
}

/// A completed handshake: the encrypted session, plus who is on the other
/// end of it.
///
/// The peer identity is handed back rather than discarded because
/// everything downstream needs it -- trust-on-first-use pinning, the
/// verification code, and deciding whether this is a known contact
/// (direct) or a stranger (relayed).
pub struct Handshake {
    pub session: Session,
    pub peer: PublicIdentity,
}

impl HandshakeInit {
    pub fn new() -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("OS randomness is unavailable");
        let ephemeral_secret = StaticSecret::from(seed);
        let ephemeral_public = PublicKey::from(&ephemeral_secret).to_bytes();
        Self { ephemeral_secret, ephemeral_public }
    }

    /// This side's handshake message, attributed to the given long-term
    /// identity.
    pub fn message(&self, identity: &IdentityKeyPair) -> HandshakeMessage {
        HandshakeMessage { identity: identity.public(), ephemeral: self.ephemeral_public }
    }

    pub fn ephemeral_public(&self) -> [u8; 32] {
        self.ephemeral_public
    }

    /// Completes the handshake against the peer's message.
    ///
    /// `initiator` must be `true` on exactly one side so both peers order
    /// the DH terms and the send/recv chains identically.
    ///
    /// Fails if the peer's prekey is not signed by the identity they claim.
    /// That check is the entire point of the exercise, so it runs before
    /// any key material is derived.
    pub fn complete(
        self,
        identity: &IdentityKeyPair,
        their_message: &HandshakeMessage,
        initiator: bool,
    ) -> Result<Handshake, CryptoError> {
        their_message.identity.verify()?;

        let their_prekey = PublicKey::from(their_message.identity.prekey);
        let their_ephemeral = PublicKey::from(their_message.ephemeral);

        let static_eph = identity.prekey_secret().diffie_hellman(&their_ephemeral);
        let eph_static = self.ephemeral_secret.diffie_hellman(&their_prekey);
        let eph_eph = self.ephemeral_secret.diffie_hellman(&their_ephemeral);

        // Both sides must concatenate the same bytes in the same order.
        // The two mixed terms are mirror images across the wire -- our
        // `static_eph` is their `eph_static` -- so the responder swaps them
        // to agree.
        let mut ikm = Vec::with_capacity(96);
        if initiator {
            ikm.extend_from_slice(static_eph.as_bytes());
            ikm.extend_from_slice(eph_static.as_bytes());
        } else {
            ikm.extend_from_slice(eph_static.as_bytes());
            ikm.extend_from_slice(static_eph.as_bytes());
        }
        ikm.extend_from_slice(eph_eph.as_bytes());

        let hk = Hkdf::<Sha256>::new(Some(ROOT_SALT), &ikm);
        let mut a2b = [0u8; 32];
        let mut b2a = [0u8; 32];
        hk.expand(INFO_A2B, &mut a2b)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hk.expand(INFO_B2A, &mut b2a)
            .expect("32 bytes is a valid HKDF-SHA256 output length");

        let mut root = [0u8; 32];
        hk.expand(INFO_RATCHET_ROOT, &mut root)
            .expect("32 bytes is a valid HKDF-SHA256 output length");

        let (send_chain, recv_chain) = if initiator { (a2b, b2a) } else { (b2a, a2b) };
        // The Double Ratchet's first DH step is the initiator's, against the
        // responder's handshake ephemeral -- which the responder keeps as
        // its first ratchet key instead of discarding.
        let start = if initiator {
            RatchetStart::Initiator { their_ephemeral: their_message.ephemeral }
        } else {
            RatchetStart::Responder { our_ephemeral: self.ephemeral_secret }
        };

        Ok(Handshake {
            session: Session::new(send_chain, recv_chain, root, start),
            peer: their_message.identity,
        })
    }
}

impl Default for HandshakeInit {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs both halves of a handshake and hands back each side's result.
    fn handshake(
        alice_id: &IdentityKeyPair,
        bob_id: &IdentityKeyPair,
    ) -> (Result<Handshake, CryptoError>, Result<Handshake, CryptoError>) {
        let alice = HandshakeInit::new();
        let bob = HandshakeInit::new();
        let alice_msg = alice.message(alice_id);
        let bob_msg = bob.message(bob_id);

        (
            alice.complete(alice_id, &bob_msg, true),
            bob.complete(bob_id, &alice_msg, false),
        )
    }

    #[test]
    fn both_sides_derive_a_session_that_talks_to_each_other() {
        let alice_id = IdentityKeyPair::generate();
        let bob_id = IdentityKeyPair::generate();
        let (alice, bob) = handshake(&alice_id, &bob_id);

        let mut alice = alice.unwrap();
        let mut bob = bob.unwrap();

        let sealed = alice.session.encrypt(b"can you hear me?", b"aad").unwrap();
        let plaintext = bob
            .session
            .decrypt(sealed.counter, &sealed.ciphertext, b"aad")
            .unwrap();
        assert_eq!(plaintext, b"can you hear me?");

        // And the other direction, since the chains are per-direction.
        let sealed = bob.session.encrypt(b"loud and clear", b"aad").unwrap();
        let plaintext = alice
            .session
            .decrypt(sealed.counter, &sealed.ciphertext, b"aad")
            .unwrap();
        assert_eq!(plaintext, b"loud and clear");
    }

    #[test]
    fn each_side_learns_who_the_other_actually_is() {
        let alice_id = IdentityKeyPair::generate();
        let bob_id = IdentityKeyPair::generate();
        let (alice, bob) = handshake(&alice_id, &bob_id);

        // This is the property v1 could not provide at all: the session is
        // bound to a long-term identity, not just to an address.
        assert_eq!(alice.unwrap().peer, bob_id.public());
        assert_eq!(bob.unwrap().peer, alice_id.public());
    }

    #[test]
    fn an_identity_whose_prekey_is_not_its_own_is_refused() {
        let alice_id = IdentityKeyPair::generate();
        let bob_id = IdentityKeyPair::generate();
        let mallory_id = IdentityKeyPair::generate();

        // Mallory claims Bob's identity key but can only offer her own DH
        // key, since she doesn't hold Bob's prekey secret.
        let forged = HandshakeMessage {
            identity: PublicIdentity {
                identity_key: bob_id.public().identity_key,
                prekey: mallory_id.public().prekey,
                prekey_signature: bob_id.public().prekey_signature,
            },
            ephemeral: HandshakeInit::new().ephemeral_public(),
        };

        let alice = HandshakeInit::new();
        assert!(matches!(
            alice.complete(&alice_id, &forged, true),
            Err(CryptoError::IdentitySignatureInvalid)
        ));
    }

    #[test]
    fn an_impostor_with_a_valid_identity_derives_a_different_session() {
        let alice_id = IdentityKeyPair::generate();
        let bob_id = IdentityKeyPair::generate();
        let mallory_id = IdentityKeyPair::generate();

        // Mallory's own identity is perfectly well-formed -- she just
        // isn't Bob. She sits in the middle and answers Alice.
        let alice = HandshakeInit::new();
        let mallory = HandshakeInit::new();
        let bob = HandshakeInit::new();

        let alice_msg = alice.message(&alice_id);
        let mallory_msg = mallory.message(&mallory_id);

        let mut alice_side = alice.complete(&alice_id, &mallory_msg, true).unwrap();
        // Bob, meanwhile, completes against Alice's real message.
        let mut bob_side = bob.complete(&bob_id, &alice_msg, false).unwrap();

        // Alice's session is with Mallory, not Bob, and Bob cannot read it.
        assert_eq!(alice_side.peer, mallory_id.public());
        let sealed = alice_side.session.encrypt(b"secret", b"aad").unwrap();
        assert!(bob_side
            .session
            .decrypt(sealed.counter, &sealed.ciphertext, b"aad")
            .is_err());
    }

    #[test]
    fn two_handshakes_between_the_same_identities_derive_different_keys() {
        let alice_id = IdentityKeyPair::generate();
        let bob_id = IdentityKeyPair::generate();

        let (first, _) = handshake(&alice_id, &bob_id);
        let (second, _) = handshake(&alice_id, &bob_id);

        let mut first = first.unwrap();
        let mut second = second.unwrap();

        // Forward secrecy: the ephemeral term means a recorded call can't
        // be replayed into a later session even by the same two parties.
        let sealed = first.session.encrypt(b"call one", b"aad").unwrap();
        assert!(second
            .session
            .decrypt(sealed.counter, &sealed.ciphertext, b"aad")
            .is_err());
    }

    #[test]
    fn handshake_messages_round_trip_through_the_wire_encoding() {
        let id = IdentityKeyPair::generate();
        let init = HandshakeInit::new();
        let msg = init.message(&id);

        let decoded = HandshakeMessage::decode(&msg.encode()).unwrap();
        assert_eq!(decoded.identity, id.public());
        assert_eq!(decoded.ephemeral, msg.ephemeral);
    }

    #[test]
    fn a_truncated_handshake_message_is_an_error_not_a_panic() {
        assert!(HandshakeMessage::decode(b"nope").is_err());
    }
}
