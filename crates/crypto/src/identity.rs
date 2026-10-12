//! Long-term identity (spec §4 "Knowing it's really them", §12 open
//! question 1).
//!
//! The design decision this module encodes: **identity is a keypair, never
//! a name scoped to a server.** Matrix-style `@alice:example.org` welds who
//! you are to where you're hosted, so changing hosts changes your identity
//! and losing the host loses it entirely. Here the identity is an Ed25519
//! key the user holds, and *where to reach them* is a separate, signed,
//! replaceable record (see [`crate::route`]). Hosting becomes a choice
//! rather than part of your name.
//!
//! Two key types, on purpose:
//!
//! - **Ed25519** signs. It is the identity -- what a QR code publishes,
//!   what signs route records, what a verification code is computed from.
//! - **X25519** does Diffie-Hellman. It cannot sign, which is why the
//!   identity key can't simply be reused for the handshake.
//!
//! The X25519 static key is bound to the identity by an Ed25519 signature
//! over it (a "signed prekey", as in X3DH). That binding is what lets a
//! peer accept a static DH key as genuinely belonging to an identity.
//!
//! Both keypairs are derived from a single 32-byte master seed via
//! domain-separated HKDF, which is what lets one 24-word recovery phrase
//! restore the whole identity (see [`crate::mnemonic`]).
//!
//! Deriving them from a shared seed is not the same as deriving them from
//! *each other*. The tempting shortcut -- converting the Ed25519 key to an
//! X25519 one, or signing with the Montgomery key via XEdDSA -- is
//! deliberately not taken. Those conversions are subtle, easy to get
//! quietly wrong, and save 32 bytes of storage in exchange for a class of
//! bug that is very hard to see.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;

/// Domain separation for the identity -> prekey binding signature, so a
/// signature minted here can never be replayed as a signature over some
/// other structure that happens to be 32 bytes.
const PREKEY_CONTEXT: &[u8] = b"voip-app/identity/prekey/v1";

/// Salt and info for deriving the two keypairs from one master seed.
///
/// Frozen under the project's old name -- see "Frozen labels" in `lib.rs`.
/// Changing any of these changes every identity derived from a seed.
const MASTER_SALT: &[u8] = b"voip-app/identity/master/v1";
const INFO_SIGNING: &[u8] = b"voip-app/identity/signing/v1";
const INFO_PREKEY: &[u8] = b"voip-app/identity/prekey-seed/v1";

/// The public half of an identity: what you hand out.
///
/// This is the whole of "who someone is" in this app. It contains no
/// address, no server name, and nothing that changes when the user moves
/// between hosts.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PublicIdentity {
    /// Ed25519 verifying key -- the identity proper.
    pub identity_key: [u8; 32],
    /// X25519 static public key used for authenticated DH.
    pub prekey: [u8; 32],
    /// Ed25519 signature over `PREKEY_CONTEXT || prekey`, proving the
    /// prekey belongs to `identity_key`.
    pub prekey_signature: [u8; 64],
}

impl PublicIdentity {
    /// Checks that `prekey` really was vouched for by `identity_key`.
    ///
    /// Every path that accepts an identity from the wire must call this.
    /// Without it an attacker could pair their own DH key with someone
    /// else's identity key and the handshake would authenticate the wrong
    /// party.
    pub fn verify(&self) -> Result<(), CryptoError> {
        let verifying = VerifyingKey::from_bytes(&self.identity_key)
            .map_err(|_| CryptoError::MalformedIdentity)?;
        let signature = Signature::from_bytes(&self.prekey_signature);

        let mut signed = Vec::with_capacity(PREKEY_CONTEXT.len() + 32);
        signed.extend_from_slice(PREKEY_CONTEXT);
        signed.extend_from_slice(&self.prekey);

        verifying
            .verify(&signed, &signature)
            .map_err(|_| CryptoError::IdentitySignatureInvalid)
    }

    /// Wire encoding: `[identity_key][prekey][prekey_signature]`.
    ///
    /// Hand-rolled and fixed-width, matching `antiphon-protocol`'s framing --
    /// three fields of known length don't need a serialization framework,
    /// and an explicit stable layout matters more here.
    pub const ENCODED_LEN: usize = 32 + 32 + 64;

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[..32].copy_from_slice(&self.identity_key);
        out[32..64].copy_from_slice(&self.prekey);
        out[64..].copy_from_slice(&self.prekey_signature);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != Self::ENCODED_LEN {
            return Err(CryptoError::MalformedIdentity);
        }
        let mut identity_key = [0u8; 32];
        let mut prekey = [0u8; 32];
        let mut prekey_signature = [0u8; 64];
        identity_key.copy_from_slice(&bytes[..32]);
        prekey.copy_from_slice(&bytes[32..64]);
        prekey_signature.copy_from_slice(&bytes[64..]);
        Ok(Self { identity_key, prekey, prekey_signature })
    }
}

/// A user's long-term identity, secret halves included.
///
/// Persisted between runs -- this is the thing whose loss means losing the
/// identity, and the thing a recovery phrase or a Shamir split among
/// trusted contacts (spec §3 "Backup & recovery") would protect.
pub struct IdentityKeyPair {
    master_seed: [u8; 32],
    signing: SigningKey,
    prekey_secret: StaticSecret,
    public: PublicIdentity,
}

impl IdentityKeyPair {
    pub fn generate() -> Self {
        let mut master_seed = [0u8; 32];
        getrandom::fill(&mut master_seed).expect("OS randomness is unavailable");
        Self::from_master_seed(master_seed)
    }

    /// Rebuilds the whole identity from one 32-byte master seed.
    ///
    /// Both keypairs are derived from this single seed, which is what
    /// makes a 24-word recovery phrase sufficient to restore an identity
    /// (see [`crate::mnemonic`]). Storing two independent seeds instead
    /// would need 48 words, and nobody writes down 48 words.
    ///
    /// The derivation is domain-separated per key so that recovering one
    /// never reveals the other, and it is fixed forever: changing it would
    /// turn every existing recovery phrase into a phrase for a different
    /// person.
    pub fn from_master_seed(master_seed: [u8; 32]) -> Self {
        let hk = Hkdf::<Sha256>::new(Some(MASTER_SALT), &master_seed);
        let mut identity_seed = [0u8; 32];
        let mut prekey_seed = [0u8; 32];
        hk.expand(INFO_SIGNING, &mut identity_seed)
            .expect("32 bytes is a valid HKDF-SHA256 output length");
        hk.expand(INFO_PREKEY, &mut prekey_seed)
            .expect("32 bytes is a valid HKDF-SHA256 output length");

        let signing = SigningKey::from_bytes(&identity_seed);
        let prekey_secret = StaticSecret::from(prekey_seed);
        let prekey = PublicKey::from(&prekey_secret).to_bytes();

        let mut signed = Vec::with_capacity(PREKEY_CONTEXT.len() + 32);
        signed.extend_from_slice(PREKEY_CONTEXT);
        signed.extend_from_slice(&prekey);
        let prekey_signature = signing.sign(&signed).to_bytes();

        let public = PublicIdentity {
            identity_key: signing.verifying_key().to_bytes(),
            prekey,
            prekey_signature,
        };

        Self { master_seed, signing, prekey_secret, public }
    }

    pub fn public(&self) -> PublicIdentity {
        self.public
    }

    /// The one seed to persist, and the one to render as a recovery
    /// phrase. Secret -- belongs in the OS keystore, and never in a theme
    /// export or a settings blob.
    pub fn master_seed(&self) -> [u8; 32] {
        self.master_seed
    }

    /// Signs arbitrary bytes with the identity key. Callers are expected
    /// to domain-separate their own payloads.
    pub fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }

    pub(crate) fn prekey_secret(&self) -> &StaticSecret {
        &self.prekey_secret
    }
}

/// Verifies a detached signature against a public identity key.
pub fn verify_signature(
    identity_key: &[u8; 32],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let verifying =
        VerifyingKey::from_bytes(identity_key).map_err(|_| CryptoError::MalformedIdentity)?;
    verifying
        .verify(message, &Signature::from_bytes(signature))
        .map_err(|_| CryptoError::IdentitySignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_freshly_generated_identity_verifies_its_own_prekey() {
        let id = IdentityKeyPair::generate();
        assert!(id.public().verify().is_ok());
    }

    #[test]
    fn reloading_from_the_master_seed_reproduces_the_same_public_identity() {
        let original = IdentityKeyPair::generate();
        let reloaded = IdentityKeyPair::from_master_seed(original.master_seed());

        // The whole point of a keypair identity is that it survives a
        // restart, a reinstall, and a move between hosts unchanged.
        assert_eq!(original.public(), reloaded.public());
    }

    #[test]
    fn the_two_derived_keys_are_independent_of_each_other() {
        let id = IdentityKeyPair::generate();
        let (_, prekey_seed) = {
            let hk = Hkdf::<Sha256>::new(Some(MASTER_SALT), &id.master_seed());
            let mut a = [0u8; 32];
            let mut b = [0u8; 32];
            hk.expand(INFO_SIGNING, &mut a).unwrap();
            hk.expand(INFO_PREKEY, &mut b).unwrap();
            (a, b)
        };
        // Domain separation: the prekey seed must not simply be the
        // signing seed, or the master seed itself.
        assert_ne!(prekey_seed, id.master_seed());
        assert_ne!(prekey_seed, id.signing.to_bytes());
    }

    #[test]
    fn a_prekey_stolen_from_another_identity_is_rejected() {
        let alice = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();

        // Mallory pairs her own DH key with Alice's identity key, hoping a
        // peer authenticates the session as Alice.
        let forged = PublicIdentity {
            identity_key: alice.public().identity_key,
            prekey: mallory.public().prekey,
            prekey_signature: alice.public().prekey_signature,
        };

        assert!(matches!(forged.verify(), Err(CryptoError::IdentitySignatureInvalid)));
    }

    #[test]
    fn a_tampered_signature_is_rejected() {
        let id = IdentityKeyPair::generate();
        let mut public = id.public();
        public.prekey_signature[0] ^= 0x01;

        assert!(public.verify().is_err());
    }

    #[test]
    fn public_identity_round_trips_through_its_wire_encoding() {
        let id = IdentityKeyPair::generate();
        let decoded = PublicIdentity::decode(&id.public().encode()).unwrap();

        assert_eq!(decoded, id.public());
        assert!(decoded.verify().is_ok());
    }

    #[test]
    fn decoding_the_wrong_number_of_bytes_is_an_error_not_a_panic() {
        assert!(PublicIdentity::decode(b"too short").is_err());
    }

    #[test]
    fn detached_signatures_verify_and_reject_the_wrong_message() {
        let id = IdentityKeyPair::generate();
        let signature = id.sign(b"where to reach me");

        assert!(verify_signature(&id.public().identity_key, b"where to reach me", &signature).is_ok());
        assert!(verify_signature(&id.public().identity_key, b"somewhere else", &signature).is_err());
    }
}
