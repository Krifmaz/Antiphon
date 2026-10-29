//! Keys for relay federation: relays talking to other relays, so a person
//! registered at one relay can be rung by someone who only knows another
//! and no single box is the one everyone depends on.
//!
//! Each federating relay holds a **peer key** ([`PeerKey`]), an X25519
//! keypair that is the relay's identity among relays. There is no
//! registry and no authority that vouches for it: a relay is known by its
//! public key, learned from the operator's own config, from another relay
//! that already peers with it, or by asking its address. Peering proves
//! possession of that key and nothing more, which is why nothing a peer
//! says is believed without proof (see [`have_proof`]).
//!
//! Two relays agree a session in one round trip ([`initiate`],
//! [`respond`], [`Initiator::finish`]). The keys mix three agreements:
//! ephemeral with ephemeral (forward secrecy), the initiator's static key
//! with the responder's ephemeral (proves the initiator), and the
//! initiator's ephemeral with the responder's static key (proves the
//! responder). Someone who knows neither static key cannot complete it,
//! and someone who recorded it cannot read it later.
//!
//! A sealed peer message is `[counter: u64 BE][ciphertext + tag]`. The
//! counter is the nonce, so it travels in the clear; a replay window on
//! the receiving side drops any counter already seen, so a recorded
//! message cannot be played back to the same session. Each direction has
//! its own key, so a relay's own output can never be reflected back at it.
//!
//! What a federating relay never gets from this module: a call key. The
//! calls it bridges are the same sealed `Session` traffic a direct call
//! carries, so a relay that is hostile -- key and all -- can drop a call
//! but cannot hear one.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::CryptoError;

/// HKDF salt for peer sessions, versioned so a future layout cannot be
/// confused with this one.
const SESSION_SALT: &[u8] = b"antiphon/federation/session/v1";
/// What the responder seals to prove it holds its static key.
const CONFIRM: &[u8] = b"antiphon/federation/confirm/v1";
/// Domain for [`lookup_hash`].
const LOOKUP_DOMAIN: &[u8] = b"antiphon/federation/lookup/v1";
/// Domain for [`have_proof`].
const HAVE_DOMAIN: &[u8] = b"antiphon/federation/have/v1";

/// Bytes a sealed peer message adds: the counter and the AEAD tag.
pub const SEAL_OVERHEAD: usize = 8 + 16;
/// How far behind the newest counter a message may arrive and still be
/// accepted, once. UDP reorders; this is how much.
pub const REPLAY_WINDOW: u64 = 64;

/// A relay's peer key: its identity among relays.
pub struct PeerKey(StaticSecret);

impl PeerKey {
    /// A fresh random peer key.
    pub fn generate() -> Self {
        Self(StaticSecret::random())
    }

    /// Restores a peer key saved with [`PeerKey::to_bytes`].
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(StaticSecret::from(bytes))
    }

    /// For persisting the key, so peers that pinned the public half keep
    /// recognising this relay across restarts.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The public half other relays know this one by.
    pub fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }

    /// An agreement with `theirs`, refusing a low-order point (whose
    /// "shared" secret is a constant anyone could compute).
    fn agree(&self, theirs: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
        let shared = self.0.diffie_hellman(&PublicKey::from(*theirs));
        if !shared.was_contributory() {
            return Err(CryptoError::DecryptionFailed);
        }
        Ok(*shared.as_bytes())
    }
}

/// The initiator's half-finished handshake, kept until the answer comes.
pub struct Initiator {
    /// This handshake's ephemeral key, used once.
    ephemeral: PeerKey,
    /// The initiator's static public key, as sent.
    static_public: [u8; 32],
    /// The responder key the initiator insists on, if it knew one.
    expected: Option<[u8; 32]>,
}

/// What the initiator sends: its static and ephemeral public keys.
pub struct Hello {
    /// The initiator's peer key, public half.
    pub static_public: [u8; 32],
    /// A fresh ephemeral public key for this handshake.
    pub ephemeral_public: [u8; 32],
}

/// What the responder sends back: its keys and proof it holds the static one.
pub struct HelloBack {
    /// The responder's peer key, public half.
    pub static_public: [u8; 32],
    /// A fresh ephemeral public key for this handshake.
    pub ephemeral_public: [u8; 32],
    /// [`CONFIRM`] sealed under the responder's sending key, counter 0.
    pub confirm: Vec<u8>,
}

/// Starts a handshake. `expected` pins the responder's key when it is
/// already known; `None` accepts whichever key answers (and the session
/// is then only as good as that first answer -- see the module docs).
pub fn initiate(me: &PeerKey, expected: Option<[u8; 32]>) -> (Initiator, Hello) {
    let ephemeral = PeerKey::generate();
    let hello = Hello { static_public: me.public(), ephemeral_public: ephemeral.public() };
    (Initiator { ephemeral, static_public: me.public(), expected }, hello)
}

/// The responder's side: answers `hello` and returns the session. The
/// initiator is not yet proven -- only a message that opens under the
/// returned session proves it -- so treat the session as pending until one does.
pub fn respond(me: &PeerKey, hello: &Hello) -> Result<(PeerSession, HelloBack), CryptoError> {
    let ephemeral = PeerKey::generate();
    let ee = ephemeral.agree(&hello.ephemeral_public)?;
    let se = ephemeral.agree(&hello.static_public)?;
    let es = me.agree(&hello.ephemeral_public)?;
    let (i2r, r2i) = derive(
        &[ee, se, es],
        [&hello.static_public, &hello.ephemeral_public, &me.public(), &ephemeral.public()],
    );
    let mut session = PeerSession::new(r2i, i2r, hello.static_public, me.public());
    let confirm = session.seal(CONFIRM);
    Ok((session, HelloBack { static_public: me.public(), ephemeral_public: ephemeral.public(), confirm }))
}

impl Initiator {
    /// Finishes the handshake with the responder's answer. Fails if the
    /// responder is not the key `expected`, or cannot prove it holds its key.
    pub fn finish(self, me: &PeerKey, back: &HelloBack) -> Result<PeerSession, CryptoError> {
        if self.expected.is_some_and(|k| k != back.static_public) {
            return Err(CryptoError::DecryptionFailed);
        }
        let ee = self.ephemeral.agree(&back.ephemeral_public)?;
        let se = me.agree(&back.ephemeral_public)?;
        let es = self.ephemeral.agree(&back.static_public)?;
        let (i2r, r2i) = derive(
            &[ee, se, es],
            [&self.static_public, &self.ephemeral.public(), &back.static_public, &back.ephemeral_public],
        );
        let mut session = PeerSession::new(i2r, r2i, back.static_public, self.static_public);
        if session.open(&back.confirm)? != CONFIRM {
            return Err(CryptoError::DecryptionFailed);
        }
        Ok(session)
    }
}

/// Expands the three agreements into the two direction keys, bound to all
/// four public keys so a session cannot be spliced from two handshakes.
fn derive(shared: &[[u8; 32]; 3], keys: [&[u8; 32]; 4]) -> ([u8; 32], [u8; 32]) {
    let mut ikm = [0u8; 96];
    for (i, s) in shared.iter().enumerate() {
        ikm[i * 32..(i + 1) * 32].copy_from_slice(s);
    }
    let mut info = [0u8; 128];
    for (i, k) in keys.iter().enumerate() {
        info[i * 32..(i + 1) * 32].copy_from_slice(*k);
    }
    let mut okm = [0u8; 64];
    Hkdf::<Sha256>::new(Some(SESSION_SALT), &ikm)
        .expand(&info, &mut okm)
        .expect("64 bytes is a valid HKDF-SHA256 output length");
    ikm.zeroize();
    let mut i2r = [0u8; 32];
    let mut r2i = [0u8; 32];
    i2r.copy_from_slice(&okm[..32]);
    r2i.copy_from_slice(&okm[32..]);
    okm.zeroize();
    (i2r, r2i)
}

/// An agreed session between two relays: one key each way, a counter for
/// what this side sends, and a replay window for what it receives.
pub struct PeerSession {
    /// Key for messages this side sends.
    send: [u8; 32],
    /// Key for messages this side receives.
    recv: [u8; 32],
    /// The other relay's peer key, public half, as proven by the handshake.
    peer: [u8; 32],
    /// This relay's own peer key, public half. A message is bound to its
    /// recipient's key, so this is the aad of everything received.
    own: [u8; 32],
    /// The next counter to send.
    next_send: u64,
    /// The highest counter received so far, if any.
    highest: Option<u64>,
    /// Bit `i` set: counter `highest - i` has been received.
    seen: u64,
}

impl Drop for PeerSession {
    fn drop(&mut self) {
        self.send.zeroize();
        self.recv.zeroize();
    }
}

impl PeerSession {
    /// A fresh session with the given direction keys.
    fn new(send: [u8; 32], recv: [u8; 32], peer: [u8; 32], own: [u8; 32]) -> Self {
        Self { send, recv, peer, own, next_send: 0, highest: None, seen: 0 }
    }

    /// The other relay's public peer key.
    pub fn peer(&self) -> [u8; 32] {
        self.peer
    }

    /// Seals `plain` for the other relay, bound to its key.
    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let counter = self.next_send;
        self.next_send += 1;
        let cipher = ChaCha20Poly1305::new(&Key::from(self.send));
        let mut out = counter.to_be_bytes().to_vec();
        let sealed = cipher
            .encrypt(&nonce(counter), Payload { msg: plain, aad: &self.peer })
            .expect("ChaCha20-Poly1305 encryption cannot fail for in-memory input");
        out.extend_from_slice(&sealed);
        out
    }

    /// Opens a message from the other relay, once. A forged, altered or
    /// replayed message is an error, and a replay does not disturb the window.
    pub fn open(&mut self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < SEAL_OVERHEAD {
            return Err(CryptoError::DecryptionFailed);
        }
        let counter = u64::from_be_bytes(sealed[..8].try_into().expect("8 bytes"));
        if !self.fresh(counter) {
            return Err(CryptoError::DecryptionFailed);
        }
        let cipher = ChaCha20Poly1305::new(&Key::from(self.recv));
        let plain = cipher
            .decrypt(&nonce(counter), Payload { msg: &sealed[8..], aad: &self.own })
            .map_err(|_| CryptoError::DecryptionFailed)?;
        self.mark(counter);
        Ok(plain)
    }

    /// Whether `counter` is new: ahead of the window, or inside it and unseen.
    fn fresh(&self, counter: u64) -> bool {
        match self.highest {
            None => true,
            Some(h) if counter > h => true,
            Some(h) => {
                let behind = h - counter;
                behind < REPLAY_WINDOW && self.seen & (1u64 << behind) == 0
            }
        }
    }

    /// Records `counter` as received.
    fn mark(&mut self, counter: u64) {
        match self.highest {
            None => {
                self.highest = Some(counter);
                self.seen = 1;
            }
            Some(h) if counter > h => {
                let shift = counter - h;
                self.seen = if shift >= 64 { 1 } else { (self.seen << shift) | 1 };
                self.highest = Some(counter);
            }
            Some(h) => self.seen |= 1u64 << (h - counter),
        }
    }
}

/// The 96-bit nonce for `counter`: four zero bytes, then the counter.
fn nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(n)
}

/// What a relay asks its peers for when someone dials a token it does not
/// hold: a hash of the token, so a peer that does not hold it learns
/// nothing it could register or dial with.
pub fn lookup_hash(token: &[u8; 32]) -> [u8; 32] {
    Sha256::new_with_prefix(LOOKUP_DOMAIN).chain_update(token).finalize().into()
}

/// A peer's proof that it really holds `token`: a MAC keyed by the token
/// over the asker's fresh `nonce` and the answering relay's own peer key.
/// Only a relay that has the token can make it, a recorded one is useless
/// for another nonce, and it cannot be relayed on behalf of another relay
/// because its key is inside.
pub fn have_proof(token: &[u8; 32], nonce: &[u8; 16], responder: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(HAVE_DOMAIN);
    mac.update(nonce);
    mac.update(responder);
    mac.finalize().into_bytes().into()
}

/// Checks a [`have_proof`] in constant time.
pub fn check_have_proof(token: &[u8; 32], nonce: &[u8; 16], responder: &[u8; 32], proof: &[u8; 32]) -> bool {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(token).expect("HMAC accepts any key length");
    mac.update(HAVE_DOMAIN);
    mac.update(nonce);
    mac.update(responder);
    mac.verify_slice(proof).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A completed handshake: (initiator's session, responder's session).
    fn pair(a: &PeerKey, b: &PeerKey) -> (PeerSession, PeerSession) {
        let (init, hello) = initiate(a, Some(b.public()));
        let (resp, back) = respond(b, &hello).unwrap();
        (init.finish(a, &back).unwrap(), resp)
    }

    #[test]
    fn two_relays_agree_and_talk_both_ways() {
        let (a, b) = (PeerKey::generate(), PeerKey::generate());
        let (mut sa, mut sb) = pair(&a, &b);
        assert_eq!(sa.peer(), b.public());
        assert_eq!(sb.peer(), a.public());
        assert_eq!(sb.open(&sa.seal(b"lookup")).unwrap(), b"lookup");
        assert_eq!(sa.open(&sb.seal(b"have")).unwrap(), b"have");
    }

    #[test]
    fn a_replayed_message_is_refused_but_reordering_is_not() {
        let (a, b) = (PeerKey::generate(), PeerKey::generate());
        let (mut sa, mut sb) = pair(&a, &b);
        let (m0, m1, m2) = (sa.seal(b"0"), sa.seal(b"1"), sa.seal(b"2"));
        assert!(sb.open(&m2).is_ok());
        assert!(sb.open(&m0).is_ok(), "late but unseen is fine");
        assert!(sb.open(&m0).is_err(), "seen twice is a replay");
        assert!(sb.open(&m1).is_ok());
        assert!(sb.open(&m2).is_err());
    }

    #[test]
    fn a_message_cannot_be_reflected_back_at_its_sender() {
        let (a, b) = (PeerKey::generate(), PeerKey::generate());
        let (mut sa, _) = pair(&a, &b);
        let own = sa.seal(b"mine");
        assert!(sa.open(&own).is_err());
    }

    #[test]
    fn an_impostor_without_the_pinned_key_cannot_finish() {
        let (a, b, mallory) = (PeerKey::generate(), PeerKey::generate(), PeerKey::generate());
        let (init, hello) = initiate(&a, Some(b.public()));
        let (_, back) = respond(&mallory, &hello).unwrap();
        assert!(init.finish(&a, &back).is_err());
    }

    #[test]
    fn an_impostor_claiming_someone_elses_key_cannot_confirm() {
        let (a, b, mallory) = (PeerKey::generate(), PeerKey::generate(), PeerKey::generate());
        let (init, hello) = initiate(&a, Some(b.public()));
        let (_, mut back) = respond(&mallory, &hello).unwrap();
        back.static_public = b.public();
        assert!(init.finish(&a, &back).is_err());
    }

    #[test]
    fn an_initiator_claiming_someone_elses_key_is_not_understood() {
        let (b, victim, mallory) = (PeerKey::generate(), PeerKey::generate(), PeerKey::generate());
        let (_, mut hello) = initiate(&mallory, None);
        hello.static_public = victim.public();
        let (mut sb, _) = respond(&b, &hello).unwrap();
        // Mallory cannot compute the session without the victim's key, so
        // nothing she seals under a guess opens.
        let (mut guess, _) = respond(&mallory, &hello).unwrap();
        assert!(sb.open(&guess.seal(b"x")).is_err());
    }

    #[test]
    fn a_have_proof_needs_the_token_and_fits_only_its_nonce_and_relay() {
        let (token, nonce, relay) = ([7u8; 32], [1u8; 16], [2u8; 32]);
        let proof = have_proof(&token, &nonce, &relay);
        assert!(check_have_proof(&token, &nonce, &relay, &proof));
        assert!(!check_have_proof(&token, &[9; 16], &relay, &proof));
        assert!(!check_have_proof(&token, &nonce, &[3; 32], &proof));
        assert!(!check_have_proof(&[8; 32], &nonce, &relay, &proof));
        assert_ne!(lookup_hash(&token), token);
    }
}
