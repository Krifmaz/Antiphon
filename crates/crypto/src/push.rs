//! Sealed push wake-ups (spec §10 "Push notifications", open question 3:
//! "confirm the encrypted-payload design before committing to iOS").
//!
//! A phone that is asleep cannot hold a socket open, so a caller has to
//! knock through the phone's push service -- FCM, APNs, or a UnifiedPush
//! distributor such as ntfy. Every one of those is run by someone else,
//! and every one of them sees each payload. So the payload is a blob only
//! the woken device can open, and it is the same size whatever it says:
//!
//! - **Sealed to the device's push key**, not to the person. Each device
//!   has its own push endpoint, so it has its own X25519 [`PushKeyPair`],
//!   given to contacts beside that endpoint and replaced with it. It is
//!   kept apart from the identity's keys on purpose: the code that opens a
//!   push (an iOS notification service extension, an Android broadcast
//!   receiver) can run before the app has unlocked anything else, and
//!   should not need to.
//! - **A fresh ephemeral key per push**, through HKDF to ChaCha20-Poly1305,
//!   as [`crate::mail`] does: the push service sees a random key and
//!   ciphertext, never the same bytes twice.
//! - **The sender is named only inside**, with an Ed25519 signature over
//!   the push key, the ephemeral key and the body. The woken device learns
//!   who is calling before it rings -- iOS insists a VoIP push be shown as
//!   a call at once, so there is no time to ask the network first -- and
//!   can drop a stranger or someone blocked without making a sound. The
//!   push service learns nothing about who called.
//! - **Constant size.** Every wake-up, call or message, with or without a
//!   hint, seals to exactly [`SEALED_LEN`] bytes, so its length does not
//!   say what it is.
//! - **Freshness is the receiver's job.** A push service could replay a
//!   blob it has seen. Each wake-up carries the time it was sent and a
//!   random id; [`Wake::is_fresh`] refuses old ones, and the receiver
//!   remembers the ids it has seen for that long.
//!
//! What still shows: that *some* wake-up reached this endpoint, and when.
//! The push service already knows which device an endpoint belongs to.
//! See `docs/mobile-push.md`.
//!
//! The wire format, all sizes in bytes:
//!
//! ```text
//! sealed (368)  = ephemeral X25519 public key (32)
//!               || ChaCha20-Poly1305 ciphertext of plain (320)
//!               || Poly1305 tag (16)
//! plain  (320)  = body (91) || sender PublicIdentity (128)
//!               || Ed25519 signature (64) || zero padding (37)
//! body   (91)   = version (1) || kind (1) || sent_at, big-endian u64 (8)
//!               || id (16) || hint length (1) || hint, zero-padded (64)
//! ```

#![warn(clippy::missing_docs_in_private_items)]

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair, PublicIdentity};

/// The HKDF salt for the sealing key, so a key derived here can never
/// collide with one derived for mail or anything else from the same DH.
const SEAL_SALT: &[u8] = b"antiphon/push/seal/v1";
/// Prefixed to what the sender signs, so a push signature cannot be passed
/// off as the identity key's signature on anything else.
const SIGN_CONTEXT: &[u8] = b"antiphon/push/sender/v1";
/// The body format's version byte. A body with any other version is
/// refused as if damaged, since its layout cannot be trusted.
const VERSION: u8 = 1;

/// The most a wake-up's hint can carry: enough for a relay rendezvous
/// token or a mailbox label and a little more.
pub const HINT_MAX: usize = 64;

/// version, kind, sent-at, id, hint length, hint (always padded to full
/// length): 1 + 1 + 8 + 16 + 1 + 64 = 91 bytes.
const BODY_LEN: usize = 1 + 1 + 8 + 16 + 1 + HINT_MAX;
/// The body, the sender's public identity and the signature, padded up so
/// the format has room to grow without changing size. 320 bytes, of which
/// 91 + 128 + 64 = 283 are used today.
const PLAIN_LEN: usize = 320;
// The used part must fit: checked at compile time, so growing the body or
// the identity encoding cannot silently overrun the padding.
const _: () = assert!(BODY_LEN + PublicIdentity::ENCODED_LEN + 64 <= PLAIN_LEN);

/// The size of every sealed wake-up: ephemeral key, ciphertext, AEAD tag.
/// 368 bytes, well under the 4 KB that FCM, APNs and ntfy all accept.
pub const SEALED_LEN: usize = 32 + PLAIN_LEN + 16;

/// A device's key for push wake-ups.
pub struct PushKeyPair {
    /// The X25519 secret, 32 bytes. Zeroed on drop by `x25519-dalek`.
    secret: StaticSecret,
}

impl PushKeyPair {
    /// A new random key, from the OS's randomness. Panics only if the OS
    /// has none to give, when no key would be safe anyway.
    pub fn generate() -> Self {
        let mut seed = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut seed[..]).expect("OS randomness is unavailable");
        Self::from_bytes(*seed)
    }

    /// Restores a key saved with [`PushKeyPair::to_bytes`].
    pub fn from_bytes(secret: [u8; 32]) -> Self {
        Self { secret: StaticSecret::from(secret) }
    }

    /// The 32-byte secret, for the device's own key store; zeroed when the
    /// returned value is dropped. Never sent anywhere.
    pub fn to_bytes(&self) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(self.secret.to_bytes())
    }

    /// What contacts seal to; given out beside the push endpoint.
    pub fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.secret).to_bytes()
    }
}

/// Why the device is being woken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeKind {
    /// Someone is calling now: ring.
    Call,
    /// Mail is waiting at a mailbox: fetch it quietly.
    Message,
    /// A file is being offered.
    File,
    /// A kind this build does not know, from a newer sender. Kept rather
    /// than refused, so the receiver can at least wake up and look.
    Other(u8),
}

impl WakeKind {
    /// The kind's byte in the body. 0 is never sent by this build.
    fn to_byte(self) -> u8 {
        match self {
            WakeKind::Call => 1,
            WakeKind::Message => 2,
            WakeKind::File => 3,
            WakeKind::Other(b) => b,
        }
    }

    /// The inverse of [`WakeKind::to_byte`]; total, so an unknown byte is
    /// [`WakeKind::Other`] rather than an error.
    fn from_byte(b: u8) -> Self {
        match b {
            1 => WakeKind::Call,
            2 => WakeKind::Message,
            3 => WakeKind::File,
            b => WakeKind::Other(b),
        }
    }
}

/// What a wake-up says, once opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wake {
    /// Why the device is being woken, which decides whether it rings.
    pub kind: WakeKind,
    /// When it was sent, in Unix seconds, by the sender's clock.
    pub sent_at: u64,
    /// Random, so a replayed wake-up can be recognised.
    pub id: [u8; 16],
    /// Where to go to find out more -- a relay token, a mailbox label --
    /// at most [`HINT_MAX`] bytes. Opaque to this module.
    pub hint: Vec<u8>,
}

impl Wake {
    /// A wake-up with a fresh random id.
    pub fn new(kind: WakeKind, sent_at: u64, hint: &[u8]) -> Result<Self, CryptoError> {
        if hint.len() > HINT_MAX {
            return Err(CryptoError::PushHintTooLong { max: HINT_MAX });
        }
        let mut id = [0u8; 16];
        getrandom::fill(&mut id).expect("OS randomness is unavailable");
        Ok(Self { kind, sent_at, id, hint: hint.to_vec() })
    }

    /// Whether this was sent within `max_age` seconds of `now`, either
    /// way: two phones' clocks are rarely equal, and a wake-up from a
    /// little in the future is as believable as one from a little ago.
    pub fn is_fresh(&self, now: u64, max_age: u64) -> bool {
        now.abs_diff(self.sent_at) <= max_age
    }

    /// The fixed-size body, laid out as in the module comment. Checks the
    /// hint again because the fields are public and may have been changed
    /// since [`Wake::new`].
    fn body(&self) -> Result<[u8; BODY_LEN], CryptoError> {
        if self.hint.len() > HINT_MAX {
            return Err(CryptoError::PushHintTooLong { max: HINT_MAX });
        }
        let mut b = [0u8; BODY_LEN];
        b[0] = VERSION;
        b[1] = self.kind.to_byte();
        b[2..10].copy_from_slice(&self.sent_at.to_be_bytes());
        b[10..26].copy_from_slice(&self.id);
        b[26] = self.hint.len() as u8;
        b[27..27 + self.hint.len()].copy_from_slice(&self.hint);
        Ok(b)
    }

    /// Parses a body. Only called after the AEAD and the signature have
    /// both checked out, so a bad version or hint length means a sender
    /// that is broken or newer, and is refused like damage.
    fn from_body(b: &[u8]) -> Result<Self, CryptoError> {
        if b.len() != BODY_LEN || b[0] != VERSION {
            return Err(CryptoError::DecryptionFailed);
        }
        let hint_len = b[26] as usize;
        if hint_len > HINT_MAX {
            return Err(CryptoError::DecryptionFailed);
        }
        Ok(Self {
            kind: WakeKind::from_byte(b[1]),
            sent_at: u64::from_be_bytes(b[2..10].try_into().expect("8 bytes")),
            id: b[10..26].try_into().expect("16 bytes"),
            hint: b[27..27 + hint_len].to_vec(),
        })
    }
}

/// Seals `wake` for the device whose push key is `recipient_push_key`,
/// signed by `sender`. The result is what goes to the push service.
///
/// The push key must have come from the recipient through an
/// authenticated channel (their signed route record or device list);
/// sealing to a key someone else supplied wakes whoever supplied it.
pub fn seal(sender: &IdentityKeyPair, recipient_push_key: &[u8; 32], wake: &Wake) -> Result<[u8; SEALED_LEN], CryptoError> {
    let body = wake.body()?;
    let mut eph_seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(&mut eph_seed[..]).expect("OS randomness is unavailable");
    let eph = StaticSecret::from(*eph_seed);
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*recipient_push_key));

    let signature = sender.sign(&signed_bytes(recipient_push_key, &eph_pub, &body));
    let mut plain = Zeroizing::new([0u8; PLAIN_LEN]);
    plain[..BODY_LEN].copy_from_slice(&body);
    plain[BODY_LEN..BODY_LEN + PublicIdentity::ENCODED_LEN].copy_from_slice(&sender.public().encode());
    plain[BODY_LEN + PublicIdentity::ENCODED_LEN..BODY_LEN + PublicIdentity::ENCODED_LEN + 64].copy_from_slice(&signature);

    // A fresh key per push, so a fixed nonce never repeats under one.
    let sealed = cipher(shared.as_bytes(), &eph_pub, recipient_push_key)
        .encrypt(&Nonce::from([0u8; 12]), plain.as_slice())
        .map_err(|_| CryptoError::DecryptionFailed)?;
    let mut out = [0u8; SEALED_LEN];
    out[..32].copy_from_slice(&eph_pub);
    out[32..].copy_from_slice(&sealed);
    Ok(out)
}

/// Opens a wake-up sealed to this device. Returns who sent it and what it
/// says; anything forged, damaged, or sealed to another device is an
/// error. Freshness is not checked here -- see [`Wake::is_fresh`].
pub fn open(push_key: &PushKeyPair, sealed: &[u8]) -> Result<(PublicIdentity, Wake), CryptoError> {
    if sealed.len() != SEALED_LEN {
        return Err(CryptoError::DecryptionFailed);
    }
    let eph_pub: [u8; 32] = sealed[..32].try_into().expect("32 bytes");
    let me = push_key.public();
    let shared = push_key.secret.diffie_hellman(&PublicKey::from(eph_pub));
    let plain = Zeroizing::new(
        cipher(shared.as_bytes(), &eph_pub, &me)
            .decrypt(&Nonce::from([0u8; 12]), &sealed[32..])
            .map_err(|_| CryptoError::DecryptionFailed)?,
    );

    let body = &plain[..BODY_LEN];
    let sender = PublicIdentity::decode(&plain[BODY_LEN..BODY_LEN + PublicIdentity::ENCODED_LEN])?;
    sender.verify()?;
    let signature: [u8; 64] = plain[BODY_LEN + PublicIdentity::ENCODED_LEN..BODY_LEN + PublicIdentity::ENCODED_LEN + 64]
        .try_into()
        .expect("64 bytes");
    verify_signature(&sender.identity_key, &signed_bytes(&me, &eph_pub, body), &signature)?;
    Ok((sender, Wake::from_body(body)?))
}

/// The AEAD for one push: a 32-byte ChaCha20-Poly1305 key from
/// HKDF-SHA256 over the DH output, with both public keys (64 bytes) as the
/// info so the key is tied to this ephemeral key and this recipient. The
/// key is used for exactly one message, which is why a zero nonce is safe.
fn cipher(shared: &[u8; 32], eph_pub: &[u8; 32], recipient: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut info = [0u8; 64];
    info[..32].copy_from_slice(eph_pub);
    info[32..].copy_from_slice(recipient);
    let mut key = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(Some(SEAL_SALT), shared)
        .expand(&info, &mut key[..])
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    ChaCha20Poly1305::new(&Key::from(*key))
}

/// What the sender signs: [`SIGN_CONTEXT`], the recipient's push key (32),
/// the ephemeral public key (32) and the body (91).
///
/// Binding the push key and the ephemeral key into the signature stops a
/// wake-up being re-sealed to some other device as though the sender had
/// called it.
fn signed_bytes(recipient: &[u8; 32], eph_pub: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGN_CONTEXT.len() + 64 + body.len());
    m.extend_from_slice(SIGN_CONTEXT);
    m.extend_from_slice(recipient);
    m.extend_from_slice(eph_pub);
    m.extend_from_slice(body);
    m
}

/// Round trips, size, unlinkability, tampering and freshness.
#[cfg(test)]
mod tests {
    use super::*;

    /// The intended device opens a wake-up and learns who sent it.
    #[test]
    fn the_device_opens_it_and_learns_the_sender() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let wake = Wake::new(WakeKind::Call, 1_800_000_000, b"relay token").unwrap();
        let sealed = seal(&alice, &phone.public(), &wake).unwrap();
        let (from, opened) = open(&phone, &sealed).unwrap();
        assert_eq!(from.identity_key, alice.public().identity_key);
        assert_eq!(opened, wake);
    }

    /// The shortest and the longest wake-up both seal to [`SEALED_LEN`].
    #[test]
    fn every_wake_up_is_the_same_size() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let short = Wake::new(WakeKind::Message, 1, b"").unwrap();
        let long = Wake::new(WakeKind::Call, u64::MAX, &[0xAB; HINT_MAX]).unwrap();
        assert_eq!(seal(&alice, &phone.public(), &short).unwrap().len(), SEALED_LEN);
        assert_eq!(seal(&alice, &phone.public(), &long).unwrap().len(), SEALED_LEN);
        assert_eq!(open(&phone, &seal(&alice, &phone.public(), &long).unwrap()).unwrap().1, long);
    }

    /// Two seals of the same wake-up differ, and no public key appears in
    /// the blob in the clear.
    #[test]
    fn the_blob_names_nobody_and_never_repeats() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let wake = Wake::new(WakeKind::Call, 5, b"").unwrap();
        let a = seal(&alice, &phone.public(), &wake).unwrap();
        let b = seal(&alice, &phone.public(), &wake).unwrap();
        assert_ne!(a, b);
        let me = alice.public();
        for key in [me.identity_key, me.prekey, phone.public()] {
            assert!(!a.windows(32).any(|w| w == key));
        }
    }

    /// A different push key cannot open the blob.
    #[test]
    fn another_device_cannot_open_it() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let other = PushKeyPair::generate();
        let sealed = seal(&alice, &phone.public(), &Wake::new(WakeKind::Call, 5, b"").unwrap()).unwrap();
        assert!(open(&other, &sealed).is_err());
    }

    /// A flipped bit anywhere, or a byte too few or too many, is refused.
    #[test]
    fn damage_and_wrong_lengths_are_refused() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let sealed = seal(&alice, &phone.public(), &Wake::new(WakeKind::File, 5, b"x").unwrap()).unwrap();
        for i in [0, 40, SEALED_LEN - 1] {
            let mut bad = sealed;
            bad[i] ^= 1;
            assert!(open(&phone, &bad).is_err());
        }
        assert!(open(&phone, &sealed[..SEALED_LEN - 1]).is_err());
        let mut longer = sealed.to_vec();
        longer.push(0);
        assert!(open(&phone, &longer).is_err());
    }

    /// A hint over [`HINT_MAX`] is refused at construction, not at sealing.
    #[test]
    fn a_hint_too_long_is_refused_when_made() {
        assert!(Wake::new(WakeKind::Call, 0, &[0; HINT_MAX + 1]).is_err());
    }

    /// A kind byte this build does not know is carried through as `Other`.
    #[test]
    fn unknown_kinds_survive_the_trip() {
        let alice = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let wake = Wake::new(WakeKind::Other(42), 5, b"").unwrap();
        let (_, opened) = open(&phone, &seal(&alice, &phone.public(), &wake).unwrap()).unwrap();
        assert_eq!(opened.kind, WakeKind::Other(42));
    }

    /// Freshness tolerates clock skew in both directions, up to `max_age`.
    #[test]
    fn freshness_allows_skew_both_ways() {
        let wake = Wake::new(WakeKind::Call, 1_000, b"").unwrap();
        assert!(wake.is_fresh(1_030, 60));
        assert!(wake.is_fresh(970, 60));
        assert!(!wake.is_fresh(1_061, 60));
        assert!(!wake.is_fresh(939, 60));
    }

    /// A saved and restored push key has the same public half.
    #[test]
    fn a_push_key_round_trips_through_bytes() {
        let key = PushKeyPair::generate();
        let again = PushKeyPair::from_bytes(*key.to_bytes());
        assert_eq!(key.public(), again.public());
    }
}
