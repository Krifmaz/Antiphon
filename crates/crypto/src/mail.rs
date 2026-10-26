//! Sealed mail for someone who is offline (spec §8 "Delivery Routes":
//! a node "holds sealed messages it can't read").
//!
//! A message for a contact who isn't online is sealed to their prekey and
//! dropped at a mailbox they chose. The mailbox sees a label and a blob;
//! it learns neither who sent it nor who it is for, and can't open it.
//!
//! - **Sealing**: a fresh X25519 key per message, agreed against the
//!   recipient's signed prekey, through HKDF to a ChaCha20-Poly1305 key.
//!   Only the recipient's prekey secret opens it.
//! - **Sender authentication** is *inside* the seal: the sender's public
//!   identity and an Ed25519 signature over the recipient, the ephemeral
//!   key and the body. The recipient learns who sent it; the node does not.
//!   Binding the recipient and the ephemeral key into the signature stops
//!   a sealed message being re-sealed to someone else as though the sender
//!   had written to them.
//! - **Labels**: where in a node a person's mail is kept. Derived from the
//!   master seed *and the node's address*, so two nodes can't match up
//!   that they hold mail for the same person. The fetch secret is derived
//!   the same way and only its hash is given to the node, so knowing the
//!   label -- every contact does, it's in the route list -- lets you
//!   deposit but not read or delete.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair, PublicIdentity};

const SEAL_SALT: &[u8] = b"antiphon/mail/seal/v1";
const SIGN_CONTEXT: &[u8] = b"antiphon/mail/sender/v1";
const LABEL_SALT: &[u8] = b"antiphon/mail/label/v1";
const FETCH_SALT: &[u8] = b"antiphon/mail/fetch/v1";

/// Bytes added to a body by sealing: the ephemeral key, the sender's
/// public identity, the signature and the AEAD tag.
pub const OVERHEAD: usize = 32 + PublicIdentity::ENCODED_LEN + 64 + 16;

/// Seals `body` for the person with this identity key and prekey,
/// signed by `sender`.
///
/// The prekey must be one already checked against the identity -- learned
/// from a verified handshake or a [`PublicIdentity::verify`] -- since
/// sealing to an unverified prekey seals to whoever supplied it.
pub fn seal(
    sender: &IdentityKeyPair,
    recipient_identity: &[u8; 32],
    recipient_prekey: &[u8; 32],
    body: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let mut eph_seed = [0u8; 32];
    getrandom::fill(&mut eph_seed).expect("OS randomness is unavailable");
    let eph = StaticSecret::from(eph_seed);
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*recipient_prekey));
    let cipher = cipher(shared.as_bytes(), &eph_pub, recipient_identity);

    let signature = sender.sign(&signed_bytes(recipient_identity, &eph_pub, body));
    let mut plain = Vec::with_capacity(PublicIdentity::ENCODED_LEN + 64 + body.len());
    plain.extend_from_slice(&sender.public().encode());
    plain.extend_from_slice(&signature);
    plain.extend_from_slice(body);

    // A fresh key per message, so a fixed nonce never repeats under one.
    let sealed = cipher.encrypt(&Nonce::from([0u8; 12]), plain.as_slice()).map_err(|_| CryptoError::DecryptionFailed)?;
    let mut out = eph_pub.to_vec();
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Opens mail sealed to `recipient`. Returns who sent it and the body;
/// anything forged, damaged or meant for someone else is an error.
pub fn open(recipient: &IdentityKeyPair, sealed: &[u8]) -> Result<(PublicIdentity, Vec<u8>), CryptoError> {
    if sealed.len() < OVERHEAD {
        return Err(CryptoError::DecryptionFailed);
    }
    let (eph_pub, body) = sealed.split_at(32);
    let eph_pub: [u8; 32] = eph_pub.try_into().expect("split at 32");
    let shared = recipient.prekey_secret().diffie_hellman(&PublicKey::from(eph_pub));
    let me = recipient.public().identity_key;
    let plain = cipher(shared.as_bytes(), &eph_pub, &me)
        .decrypt(&Nonce::from([0u8; 12]), body)
        .map_err(|_| CryptoError::DecryptionFailed)?;

    let sender = PublicIdentity::decode(&plain[..PublicIdentity::ENCODED_LEN])?;
    sender.verify()?;
    let signature: [u8; 64] = plain[PublicIdentity::ENCODED_LEN..PublicIdentity::ENCODED_LEN + 64]
        .try_into()
        .expect("64 bytes");
    let message = &plain[PublicIdentity::ENCODED_LEN + 64..];
    verify_signature(&sender.identity_key, &signed_bytes(&me, &eph_pub, message), &signature)?;
    Ok((sender, message.to_vec()))
}

/// Where at `node` this identity's mail is kept.
pub fn inbox_label(identity: &IdentityKeyPair, node: &str) -> [u8; 32] {
    derive(LABEL_SALT, &identity.master_seed(), node)
}

/// The secret that reads and deletes this identity's mail at `node`.
pub fn fetch_secret(identity: &IdentityKeyPair, node: &str) -> [u8; 32] {
    derive(FETCH_SALT, &identity.master_seed(), node)
}

/// What the node is given in place of the fetch secret.
pub fn fetch_hash(secret: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

fn derive(salt: &[u8], seed: &[u8; 32], node: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), seed)
        .expand(node.trim().to_lowercase().as_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

fn cipher(shared: &[u8; 32], eph_pub: &[u8; 32], recipient: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(eph_pub);
    info.extend_from_slice(recipient);
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(SEAL_SALT), shared)
        .expand(&info, &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    ChaCha20Poly1305::new(&Key::from(key))
}

fn signed_bytes(recipient: &[u8; 32], eph_pub: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGN_CONTEXT.len() + 64 + body.len());
    m.extend_from_slice(SIGN_CONTEXT);
    m.extend_from_slice(recipient);
    m.extend_from_slice(eph_pub);
    m.extend_from_slice(body);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_recipient_opens_it_and_learns_the_sender() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let sealed = seal(&alice, &bob.public().identity_key, &bob.public().prekey, b"see you at eight").unwrap();
        assert_eq!(sealed.len(), b"see you at eight".len() + OVERHEAD);
        let (from, body) = open(&bob, &sealed).unwrap();
        assert_eq!(from.identity_key, alice.public().identity_key);
        assert_eq!(body, b"see you at eight");
    }

    #[test]
    fn nobody_else_can_open_it() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let eve = IdentityKeyPair::generate();
        let sealed = seal(&alice, &bob.public().identity_key, &bob.public().prekey, b"hi").unwrap();
        assert!(open(&eve, &sealed).is_err());
    }

    #[test]
    fn a_tampered_blob_is_refused() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let mut sealed = seal(&alice, &bob.public().identity_key, &bob.public().prekey, b"hi").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(open(&bob, &sealed).is_err());
        assert!(open(&bob, &sealed[..10]).is_err());
    }

    #[test]
    fn the_blob_does_not_name_the_sender() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let sealed = seal(&alice, &bob.public().identity_key, &bob.public().prekey, b"hi").unwrap();
        let key = alice.public().identity_key;
        assert!(!sealed.windows(32).any(|w| w == key));
    }

    #[test]
    fn labels_differ_per_node_and_per_person() {
        let a = IdentityKeyPair::generate();
        let b = IdentityKeyPair::generate();
        assert_ne!(inbox_label(&a, "node1:3479"), inbox_label(&a, "node2:3479"));
        assert_ne!(inbox_label(&a, "node1:3479"), inbox_label(&b, "node1:3479"));
        assert_eq!(inbox_label(&a, "Node1:3479 "), inbox_label(&a, "node1:3479"));
        assert_ne!(inbox_label(&a, "n"), fetch_secret(&a, "n"));
    }
}
