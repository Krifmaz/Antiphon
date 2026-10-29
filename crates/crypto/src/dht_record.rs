//! "Where I am now" records for the DHT (spec §4 "DHT distributed lookup",
//! "DHT records encrypted and signed", "Blinded lookups").
//!
//! A contact whose IP address changed is found again by looking up a
//! record they published into a distributed hash table. The nodes storing
//! and serving that record are strangers, so the record is built to tell
//! them nothing:
//!
//! - **Blinded, rotating key.** Each day the publisher's Ed25519 identity
//!   key `A` is blinded to `A' = h·A`, with `h = H(label, A, day)` -- the
//!   construction Tor v3 onion services use. The record is filed under
//!   `H(A')`. Anyone who already knows `A` can compute today's `A'` and so
//!   today's key; a storage node, which sees only `A'`, cannot tell whose
//!   it is or link it to yesterday's (that would mean solving a
//!   Diffie-Hellman problem). The same goes for a node watching lookups:
//!   it sees a key that changes daily, not who is being searched for.
//! - **Signed under the blinded key.** The publisher holds the matching
//!   blinded secret `h·a`, so the record carries an ordinary Ed25519
//!   signature by `A'`. A storage node can check it -- and refuse forgeries
//!   and replays of older sequences -- without learning `A`; a reader
//!   recomputes `A'` from the identity it already trusts, so a valid
//!   signature proves the identity's owner wrote it. Nobody else can sign
//!   under `A'`, which is what stops a node or another user overwriting
//!   someone's record.
//! - **Encrypted to people who know the identity key.** The payload is
//!   sealed under a key derived from `A` and the day. That is not secrecy
//!   from contacts -- it is the same set of people who can find the record
//!   at all -- but it means the storage node holds bytes it cannot read,
//!   and an address is never readable by someone who only saw the key.
//! - **`sequence`** stops rollback, as in [`crate::SignedRouteRecord`]:
//!   storage nodes keep only the highest, and readers take only the highest.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use curve25519_dalek::scalar::Scalar;
use ed25519_dalek::hazmat::{raw_sign, ExpandedSecretKey};
use ed25519_dalek::{Signature, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256, Sha512};

use crate::error::CryptoError;
use crate::identity::IdentityKeyPair;

const BLIND_LABEL: &[u8] = b"antiphon/dht/v1/blind";
const PREFIX_LABEL: &[u8] = b"antiphon/dht/v1/blind-prefix";
const KEY_LABEL: &[u8] = b"antiphon/dht/v1/key";
const SEAL_SALT: &[u8] = b"antiphon/dht/v1/seal";
const SIGN_CONTEXT: &[u8] = b"antiphon/dht/v1/record";

/// Largest sealed payload, so a record plus a list of nodes always fits
/// in one datagram on any path.
pub const MAX_CIPHERTEXT: usize = 640;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Largest plaintext payload that seals within [`MAX_CIPHERTEXT`].
pub const MAX_PAYLOAD: usize = MAX_CIPHERTEXT - NONCE_LEN - TAG_LEN;

pub type DhtKey = [u8; 32];

/// The rotation period: days since the Unix epoch, as for relay tokens.
pub fn epoch(unix_secs: u64) -> u64 {
    unix_secs / 86_400
}

fn blinding_factor(identity_key: &[u8; 32], epoch: u64) -> Scalar {
    let mut h = Sha512::new();
    h.update(BLIND_LABEL);
    h.update(identity_key);
    h.update(epoch.to_be_bytes());
    Scalar::from_hash(h)
}

/// The blinded public key for `identity_key` on `epoch`. Computable by
/// anyone who knows the identity key; unlinkable to it for anyone who
/// doesn't.
pub fn blinded_key(identity_key: &[u8; 32], epoch: u64) -> Result<[u8; 32], CryptoError> {
    let a = VerifyingKey::from_bytes(identity_key).map_err(|_| CryptoError::MalformedIdentity)?;
    let blinded = blinding_factor(identity_key, epoch) * a.to_edwards();
    Ok(blinded.compress().to_bytes())
}

/// Where a blinded key's record is filed.
pub fn key_for_blinded(blinded: &[u8; 32]) -> DhtKey {
    let mut h = Sha256::new();
    h.update(KEY_LABEL);
    h.update(blinded);
    h.finalize().into()
}

/// Where `identity_key`'s record for `epoch` is filed -- what a contact
/// looks up.
pub fn lookup_key(identity_key: &[u8; 32], epoch: u64) -> Result<DhtKey, CryptoError> {
    Ok(key_for_blinded(&blinded_key(identity_key, epoch)?))
}

/// The blinded secret matching [`blinded_key`]: scalar `h·a`, and a
/// nonce-derivation prefix derived from the real one so it is as secret
/// and as deterministic as an ordinary Ed25519 key's.
fn blinded_secret(identity: &IdentityKeyPair, epoch: u64) -> (ExpandedSecretKey, VerifyingKey) {
    let identity_key = identity.public().identity_key;
    let real = ExpandedSecretKey::from(&identity.signing_seed());
    let h = blinding_factor(&identity_key, epoch);
    let mut prefix = Sha512::new();
    prefix.update(PREFIX_LABEL);
    prefix.update(real.hash_prefix);
    prefix.update(epoch.to_be_bytes());
    let mut hash_prefix = [0u8; 32];
    hash_prefix.copy_from_slice(&prefix.finalize()[..32]);
    let esk = ExpandedSecretKey { scalar: h * real.scalar, hash_prefix };
    let public = VerifyingKey::from(&esk);
    (esk, public)
}

fn seal_cipher(identity_key: &[u8; 32], epoch: u64) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(SEAL_SALT), identity_key)
        .expand(&epoch.to_be_bytes(), &mut key)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    ChaCha20Poly1305::new(&Key::from(key))
}

/// One record as stored in and served by the DHT.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DhtRecord {
    /// `H(blinded)`: where it is filed. Checked, not trusted.
    pub key: DhtKey,
    pub blinded: [u8; 32],
    pub sequence: u64,
    /// Nonce followed by the AEAD output.
    pub ciphertext: Vec<u8>,
    pub signature: [u8; 64],
}

impl DhtRecord {
    /// Seals and signs `payload` as `identity`'s record for `epoch`.
    pub fn seal(identity: &IdentityKeyPair, epoch: u64, sequence: u64, payload: &[u8]) -> Result<Self, CryptoError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(CryptoError::MalformedDhtRecord);
        }
        let identity_key = identity.public().identity_key;
        let (esk, blinded_public) = blinded_secret(identity, epoch);
        let blinded = blinded_public.to_bytes();
        let key = key_for_blinded(&blinded);

        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("OS randomness is unavailable");
        let aad = aad(&key, sequence);
        let sealed = seal_cipher(&identity_key, epoch)
            .encrypt(&Nonce::from(nonce), Payload { msg: payload, aad: &aad })
            .map_err(|_| CryptoError::DecryptionFailed)?;
        let mut ciphertext = nonce.to_vec();
        ciphertext.extend_from_slice(&sealed);

        let mut record = Self { key, blinded, sequence, ciphertext, signature: [0; 64] };
        record.signature = raw_sign::<Sha512>(&esk, &record.signing_bytes(), &blinded_public).to_bytes();
        Ok(record)
    }

    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SIGN_CONTEXT.len() + 72 + self.ciphertext.len());
        out.extend_from_slice(SIGN_CONTEXT);
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.blinded);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// What a storage node can check without knowing whose record it is:
    /// it is filed under its own blinded key, within size, and signed by
    /// that key.
    pub fn verify(&self) -> Result<(), CryptoError> {
        if self.ciphertext.len() > MAX_CIPHERTEXT || self.ciphertext.len() < NONCE_LEN + TAG_LEN {
            return Err(CryptoError::MalformedDhtRecord);
        }
        if key_for_blinded(&self.blinded) != self.key {
            return Err(CryptoError::MalformedDhtRecord);
        }
        let public = VerifyingKey::from_bytes(&self.blinded).map_err(|_| CryptoError::MalformedDhtRecord)?;
        public
            .verify_strict(&self.signing_bytes(), &Signature::from_bytes(&self.signature))
            .map_err(|_| CryptoError::DhtSignatureInvalid)
    }

    /// What a contact does: checks this is `identity_key`'s record for
    /// `epoch` (the blinded key must be the one *they* compute, so a
    /// record signed by anyone else fails here), then decrypts it.
    pub fn open(&self, identity_key: &[u8; 32], epoch: u64) -> Result<Vec<u8>, CryptoError> {
        if blinded_key(identity_key, epoch)? != self.blinded {
            return Err(CryptoError::DhtSignatureInvalid);
        }
        self.verify()?;
        let (nonce, sealed) = self.ciphertext.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| CryptoError::MalformedDhtRecord)?;
        seal_cipher(identity_key, epoch)
            .decrypt(&Nonce::from(nonce), Payload { msg: sealed, aad: &aad(&self.key, self.sequence) })
            .map_err(|_| CryptoError::DecryptionFailed)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + 32 + 8 + 2 + self.ciphertext.len() + 64);
        out.extend_from_slice(&self.key);
        out.extend_from_slice(&self.blinded);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&(self.ciphertext.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.ciphertext);
        out.extend_from_slice(&self.signature);
        out
    }

    /// Decodes without verifying; call [`Self::verify`] or [`Self::open`].
    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bad = || CryptoError::MalformedDhtRecord;
        if bytes.len() < 32 + 32 + 8 + 2 + 64 {
            return Err(bad());
        }
        let key: [u8; 32] = bytes[..32].try_into().map_err(|_| bad())?;
        let blinded: [u8; 32] = bytes[32..64].try_into().map_err(|_| bad())?;
        let sequence = u64::from_be_bytes(bytes[64..72].try_into().map_err(|_| bad())?);
        let len = u16::from_be_bytes([bytes[72], bytes[73]]) as usize;
        if len > MAX_CIPHERTEXT || bytes.len() != 74 + len + 64 {
            return Err(bad());
        }
        let ciphertext = bytes[74..74 + len].to_vec();
        let signature: [u8; 64] = bytes[74 + len..].try_into().map_err(|_| bad())?;
        Ok(Self { key, blinded, sequence, ciphertext, signature })
    }

    /// Encoded length, for fitting replies into a datagram.
    pub fn encoded_len(&self) -> usize {
        32 + 32 + 8 + 2 + self.ciphertext.len() + 64
    }
}

fn aad(key: &DhtKey, sequence: u64) -> Vec<u8> {
    let mut out = key.to_vec();
    out.extend_from_slice(&sequence.to_be_bytes());
    out
}

/// From every candidate a lookup turned up, the one a reader should
/// believe: genuinely `identity_key`'s for `epoch`, decryptable, and with
/// the highest sequence -- which must also beat `known_sequence`, the
/// highest already accepted, so a replayed old record changes nothing.
pub fn best<'a>(
    candidates: impl IntoIterator<Item = &'a DhtRecord>,
    identity_key: &[u8; 32],
    epoch: u64,
    known_sequence: Option<u64>,
) -> Option<(u64, Vec<u8>)> {
    let mut best: Option<(u64, Vec<u8>)> = None;
    for record in candidates {
        if known_sequence.is_some_and(|k| record.sequence <= k) {
            continue;
        }
        if best.as_ref().is_some_and(|(s, _)| record.sequence <= *s) {
            continue;
        }
        if let Ok(payload) = record.open(identity_key, epoch) {
            best = Some((record.sequence, payload));
        }
    }
    best
}

/// What a published record says: where the publisher can be reached now.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Whereabouts {
    /// Direct `host:port` addresses, best first.
    pub addresses: Vec<String>,
    /// The relay they listen at, or empty.
    pub relay: String,
    /// Their signed route list ([`crate::SignedRouteRecord::encode`]), or
    /// empty. Already signed by the identity, so it is checked the usual
    /// way after decoding.
    pub route_record: Vec<u8>,
}

const MAX_ADDRESSES: usize = 4;
const MAX_ADDRESS_LEN: usize = 96;

impl Whereabouts {
    /// Encodes, dropping the route record if the whole would not fit in a
    /// record: an address is what the DHT is for; the route list also
    /// arrives on every call.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![1u8];
        let addresses: Vec<&String> =
            self.addresses.iter().filter(|a| a.len() <= MAX_ADDRESS_LEN).take(MAX_ADDRESSES).collect();
        out.push(addresses.len() as u8);
        for a in addresses {
            out.push(a.len() as u8);
            out.extend_from_slice(a.as_bytes());
        }
        let relay = if self.relay.len() <= MAX_ADDRESS_LEN { self.relay.as_bytes() } else { &[] };
        out.push(relay.len() as u8);
        out.extend_from_slice(relay);
        let route = if out.len() + 2 + self.route_record.len() <= MAX_PAYLOAD { &self.route_record[..] } else { &[] };
        out.extend_from_slice(&(route.len() as u16).to_be_bytes());
        out.extend_from_slice(route);
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&version, mut rest) = bytes.split_first()?;
        if version != 1 {
            return None;
        }
        let mut take = |n: usize| -> Option<&[u8]> {
            if rest.len() < n {
                return None;
            }
            let (head, tail) = rest.split_at(n);
            rest = tail;
            Some(head)
        };
        let count = take(1)?[0] as usize;
        if count > MAX_ADDRESSES {
            return None;
        }
        let mut addresses = Vec::with_capacity(count);
        for _ in 0..count {
            let len = take(1)?[0] as usize;
            addresses.push(String::from_utf8(take(len)?.to_vec()).ok()?);
        }
        let len = take(1)?[0] as usize;
        let relay = String::from_utf8(take(len)?.to_vec()).ok()?;
        let len = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let route_record = take(len)?.to_vec();
        if !rest.is_empty() {
            return None;
        }
        Some(Self { addresses, relay, route_record })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Whereabouts {
        Whereabouts { addresses: vec!["203.0.113.7:4000".into()], relay: "relay.example:3479".into(), route_record: vec![] }
    }

    #[test]
    fn a_contact_finds_and_opens_the_record() {
        let alice = IdentityKeyPair::generate();
        let id = alice.public().identity_key;
        let record = DhtRecord::seal(&alice, 20_000, 1, &sample().encode()).unwrap();
        assert_eq!(record.key, lookup_key(&id, 20_000).unwrap(), "the key a contact computes");
        let payload = record.open(&id, 20_000).unwrap();
        assert_eq!(Whereabouts::decode(&payload).unwrap(), sample());
    }

    #[test]
    fn a_storage_node_can_verify_without_learning_who() {
        let alice = IdentityKeyPair::generate();
        let id = alice.public().identity_key;
        let record = DhtRecord::seal(&alice, 20_000, 1, &sample().encode()).unwrap();
        let decoded = DhtRecord::decode(&record.encode()).unwrap();
        assert!(decoded.verify().is_ok());
        // Nothing the node holds is the identity key or the address.
        let bytes = record.encode();
        assert!(!bytes.windows(32).any(|w| w == id));
        assert!(!bytes.windows(8).any(|w| w == b"203.0.11"));
    }

    #[test]
    fn keys_rotate_daily_and_differ_per_person() {
        let alice = IdentityKeyPair::generate().public().identity_key;
        let bob = IdentityKeyPair::generate().public().identity_key;
        assert_ne!(lookup_key(&alice, 1).unwrap(), lookup_key(&alice, 2).unwrap());
        assert_ne!(lookup_key(&alice, 1).unwrap(), lookup_key(&bob, 1).unwrap());
        assert_ne!(blinded_key(&alice, 1).unwrap(), alice);
    }

    #[test]
    fn someone_else_cannot_publish_under_your_key() {
        let alice = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();
        let id = alice.public().identity_key;
        let mut forged = DhtRecord::seal(&mallory, 5, 9, &sample().encode()).unwrap();
        // Mallory files her record under Alice's key and blinded key: the
        // signature no longer verifies.
        forged.blinded = blinded_key(&id, 5).unwrap();
        forged.key = lookup_key(&id, 5).unwrap();
        assert!(forged.verify().is_err());
        assert!(forged.open(&id, 5).is_err());
        // And her own record, honestly signed, is not Alice's.
        let honest = DhtRecord::seal(&mallory, 5, 9, &sample().encode()).unwrap();
        assert!(honest.verify().is_ok());
        assert!(honest.open(&id, 5).is_err());
    }

    #[test]
    fn tampering_is_caught_by_node_and_reader() {
        let alice = IdentityKeyPair::generate();
        let mut r = DhtRecord::seal(&alice, 5, 1, &sample().encode()).unwrap();
        r.sequence = 100;
        assert!(r.verify().is_err(), "sequence bumped by a node");
        let mut r = DhtRecord::seal(&alice, 5, 1, &sample().encode()).unwrap();
        r.ciphertext[20] ^= 1;
        assert!(r.verify().is_err());
    }

    #[test]
    fn without_the_identity_key_it_cannot_be_read() {
        let alice = IdentityKeyPair::generate();
        let stranger = IdentityKeyPair::generate().public().identity_key;
        let r = DhtRecord::seal(&alice, 5, 1, &sample().encode()).unwrap();
        assert!(r.open(&stranger, 5).is_err());
        assert!(r.open(&alice.public().identity_key, 6).is_err(), "wrong day");
    }

    #[test]
    fn the_highest_sequence_wins_and_old_ones_are_refused() {
        let alice = IdentityKeyPair::generate();
        let id = alice.public().identity_key;
        let mut new = sample();
        new.addresses = vec!["198.51.100.2:4000".into()];
        let old = DhtRecord::seal(&alice, 5, 3, &sample().encode()).unwrap();
        let newer = DhtRecord::seal(&alice, 5, 4, &new.encode()).unwrap();
        let forged = DhtRecord::seal(&IdentityKeyPair::generate(), 5, 99, &sample().encode()).unwrap();
        let (seq, payload) = best([&old, &forged, &newer], &id, 5, None).unwrap();
        assert_eq!(seq, 4);
        assert_eq!(Whereabouts::decode(&payload).unwrap(), new);
        assert!(best([&old], &id, 5, Some(3)).is_none(), "a replay of what we already had");
    }

    #[test]
    fn the_blinded_signer_matches_the_public_blinding() {
        let alice = IdentityKeyPair::generate();
        for day in [0, 1, 19_000, u32::MAX as u64] {
            let (_, public) = blinded_secret(&alice, day);
            assert_eq!(public.to_bytes(), blinded_key(&alice.public().identity_key, day).unwrap());
        }
    }

    #[test]
    fn oversize_and_truncated_inputs_are_errors() {
        let alice = IdentityKeyPair::generate();
        assert!(DhtRecord::seal(&alice, 1, 1, &vec![0; MAX_PAYLOAD + 1]).is_err());
        let bytes = DhtRecord::seal(&alice, 1, 1, &[1, 2, 3]).unwrap().encode();
        for cut in [0, 10, 73, bytes.len() - 1] {
            assert!(DhtRecord::decode(&bytes[..cut]).is_err());
        }
        assert!(Whereabouts::decode(&[1, 9]).is_none());
    }

    #[test]
    fn an_oversized_route_record_is_dropped_not_the_address() {
        let mut w = sample();
        w.route_record = vec![7; MAX_PAYLOAD];
        let decoded = Whereabouts::decode(&w.encode()).unwrap();
        assert!(decoded.route_record.is_empty());
        assert_eq!(decoded.addresses, w.addresses);
    }
}
