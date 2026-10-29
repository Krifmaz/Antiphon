use std::collections::{HashMap, VecDeque};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;
use crate::ratchet::ChainKey;

/// How many message keys we're willing to derive-and-cache ahead of the
/// last one we actually consumed. Bounds both memory use and the cost of a
/// malicious peer sending a huge counter to force wasted ratcheting.
const MAX_SKIP: u64 = 1000;

/// Most skipped keys held at once, across every chain. Oldest go first: a
/// packet that late is not going to be played anyway.
const MAX_SKIPPED_TOTAL: usize = 2000;

/// Messages sent on one chain before this side starts a new one, once the
/// other side has shown it a new ratchet key. About five seconds of audio
/// at 50 frames a second: often enough that a stolen chain key is worth
/// seconds of a call, rarely enough that a DH a few times a minute costs
/// nothing.
const ROTATE_AFTER: u64 = 250;

const ROOT_INFO: &[u8] = b"antiphon/ratchet/root/v1";

/// Bytes the ratchet header adds in front of every ciphertext:
/// `[sender's ratchet public key; 32][previous chain length: u32 BE]`.
pub const HEADER_LEN: usize = 32 + 4;

/// The public key that marks the two chains seeded by the handshake, before
/// either side has taken a DH step.
const EPOCH_ZERO: [u8; 32] = [0; 32];

/// An encrypted frame ready to go on the wire, or just received off it.
/// `counter` is sent in the clear (as protocol framing, spec §3) — it is not
/// secret, only the chain state that derives a key from it is. It counts
/// messages on the *current* sending chain and restarts at zero after a DH
/// step; `ciphertext` begins with the ratchet header saying which chain.
pub struct Sealed {
    pub counter: u64,
    pub ciphertext: Vec<u8>,
}

/// How a session takes its first DH step.
pub(crate) enum RatchetStart {
    /// The handshake's initiator: steps first, against the responder's
    /// handshake ephemeral.
    Initiator { their_ephemeral: [u8; 32] },
    /// The responder: keeps its handshake ephemeral as its ratchet key and
    /// steps only once the initiator has.
    Responder { our_ephemeral: StaticSecret },
}

/// A live 1:1 encrypted session: a Double Ratchet (spec §3 "1:1 -- Double
/// Ratchet style, forward secrecy").
///
/// Two layers, as in Signal's design:
///
/// - **Symmetric chains**: every message key is derived from a chain key
///   that is then ratcheted forward, so a key once used can't be recovered
///   from anything that comes later (forward secrecy).
/// - **DH steps**: every so often each side brings in a fresh X25519 key,
///   mixed into a root key that seeds the next chains. A stolen chain key
///   stops being useful at the next step (post-compromise security) --
///   seconds into a call, not at its end.
///
/// UDP makes two adjustments. Both handshake chains run from the start so
/// either side can send immediately, and the first DH step comes after.
/// And a side only starts a new sending chain every [`ROTATE_AFTER`]
/// messages, once it has seen the other's latest key: stepping on every
/// reply would mean a DH per audio frame. Turn-taking still holds -- a side
/// only steps in answer to a key it hasn't answered yet -- which is what
/// keeps both sides' root keys in agreement.
pub struct Session {
    root: [u8; 32],

    /// Our current ratchet secret: what a step on their side is DH'd
    /// against.
    our_ratchet: StaticSecret,
    /// The peer's newest ratchet key, which our next step is DH'd against;
    /// `EPOCH_ZERO` while unknown.
    their_ratchet: [u8; 32],
    /// Set when a key from them hasn't been answered with one of ours yet.
    should_rotate: bool,

    /// The key in our headers: `EPOCH_ZERO` on the handshake chain, then
    /// the public half of `our_ratchet`.
    send_pub: [u8; 32],
    send_chain: ChainKey,
    send_counter: u64,
    /// Length of the sending chain before the last step, sent in headers
    /// so the peer knows how far the old chain went.
    previous_send_len: u32,

    /// The key in their headers for the chain we are receiving on.
    recv_pub: [u8; 32],
    recv_chain: ChainKey,
    recv_counter: u64,
    skipped_keys: HashMap<SkippedId, [u8; 32]>,
    skipped_order: VecDeque<SkippedId>,
}

/// A message key kept for a late packet: which chain (by its header key)
/// and which message on it.
type SkippedId = ([u8; 32], u64);

impl Session {
    pub(crate) fn new(send_seed: [u8; 32], recv_seed: [u8; 32], root: [u8; 32], start: RatchetStart) -> Self {
        let (our_ratchet, their_ratchet, should_rotate) = match start {
            RatchetStart::Initiator { their_ephemeral } => (random_secret(), their_ephemeral, true),
            RatchetStart::Responder { our_ephemeral } => (our_ephemeral, EPOCH_ZERO, false),
        };
        Self {
            root,
            our_ratchet,
            their_ratchet,
            should_rotate,
            send_pub: EPOCH_ZERO,
            send_chain: ChainKey::new(send_seed),
            send_counter: 0,
            previous_send_len: 0,
            recv_pub: EPOCH_ZERO,
            recv_chain: ChainKey::new(recv_seed),
            recv_counter: 0,
            skipped_keys: HashMap::new(),
            skipped_order: VecDeque::new(),
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8], aad: &[u8]) -> Result<Sealed, CryptoError> {
        if self.should_rotate && self.send_counter >= ROTATE_AFTER {
            self.step_sending();
        }

        let (key_bytes, next_chain) = self.send_chain.ratchet();
        let counter = self.send_counter;
        self.send_chain = next_chain;
        self.send_counter += 1;

        let mut header = [0u8; HEADER_LEN];
        header[..32].copy_from_slice(&self.send_pub);
        header[32..].copy_from_slice(&self.previous_send_len.to_be_bytes());

        let cipher = ChaCha20Poly1305::new(&Key::from(key_bytes));
        let nonce = nonce_for_counter(counter);
        let sealed = cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad: &header_aad(&header, aad) })
            .map_err(|_| CryptoError::DecryptionFailed)?;

        let mut ciphertext = Vec::with_capacity(HEADER_LEN + sealed.len());
        ciphertext.extend_from_slice(&header);
        ciphertext.extend_from_slice(&sealed);
        Ok(Sealed { counter, ciphertext })
    }

    /// Starts a new sending chain from a fresh key and the peer's latest.
    fn step_sending(&mut self) {
        let fresh = random_secret();
        let dh = fresh.diffie_hellman(&PublicKey::from(self.their_ratchet));
        let (root, chain) = kdf_root(&self.root, dh.as_bytes());
        self.root = root;
        self.send_pub = PublicKey::from(&fresh).to_bytes();
        self.our_ratchet = fresh;
        self.send_chain = ChainKey::new(chain);
        self.previous_send_len = self.send_counter as u32;
        self.send_counter = 0;
        self.should_rotate = false;
    }

    pub fn decrypt(&mut self, counter: u64, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if ciphertext.len() < HEADER_LEN {
            return Err(CryptoError::DecryptionFailed);
        }
        let (header, sealed) = ciphertext.split_at(HEADER_LEN);
        let their_pub: [u8; 32] = header[..32].try_into().expect("32 bytes");
        let previous_len = u32::from_be_bytes(header[32..].try_into().expect("4 bytes")) as u64;
        let aad = header_aad(header, aad);

        // A late packet from a chain already moved past.
        if let Some(key) = self.skipped_keys.get(&(their_pub, counter)).copied() {
            let plaintext = open(&key, counter, sealed, &aad)?;
            self.skipped_keys.remove(&(their_pub, counter));
            self.skipped_order.retain(|k| k != &(their_pub, counter));
            return Ok(plaintext);
        }

        // Worked out on copies and kept only if the packet authenticates:
        // a forged header must not be able to move the ratchet.
        let mut root = self.root;
        let mut recv_pub = self.recv_pub;
        let mut recv_chain = self.recv_chain.clone();
        let mut recv_counter = self.recv_counter;
        let mut skipped = Vec::new();
        let stepped = their_pub != self.recv_pub;
        if stepped {
            // A step on their side. Close out the chain they left, as far
            // as they say it went, then derive the new receiving chain.
            if their_pub == EPOCH_ZERO {
                return Err(CryptoError::ReplayOrTooOld { counter });
            }
            skip(&mut recv_chain, &mut recv_counter, recv_pub, previous_len, &mut skipped)?;
            let dh = self.our_ratchet.diffie_hellman(&PublicKey::from(their_pub));
            let (next_root, chain) = kdf_root(&root, dh.as_bytes());
            root = next_root;
            recv_pub = their_pub;
            recv_chain = ChainKey::new(chain);
            recv_counter = 0;
        }

        if counter < recv_counter {
            return Err(CryptoError::ReplayOrTooOld { counter });
        }
        skip(&mut recv_chain, &mut recv_counter, recv_pub, counter, &mut skipped)?;
        let (key, chain) = recv_chain.ratchet();
        let plaintext = open(&key, counter, sealed, &aad)?;

        self.root = root;
        self.recv_pub = recv_pub;
        self.recv_chain = chain;
        self.recv_counter = counter + 1;
        if stepped {
            self.their_ratchet = their_pub;
            self.should_rotate = true;
        }
        for (id, key) in skipped {
            self.skipped_keys.insert(id, key);
            self.skipped_order.push_back(id);
        }
        while self.skipped_order.len() > MAX_SKIPPED_TOTAL {
            if let Some(old) = self.skipped_order.pop_front() {
                self.skipped_keys.remove(&old);
            }
        }
        Ok(plaintext)
    }

    #[cfg(test)]
    fn clone_state(&self) -> Self {
        Self {
            root: self.root,
            our_ratchet: self.our_ratchet.clone(),
            their_ratchet: self.their_ratchet,
            should_rotate: self.should_rotate,
            send_pub: self.send_pub,
            send_chain: self.send_chain.clone(),
            send_counter: self.send_counter,
            previous_send_len: self.previous_send_len,
            recv_pub: self.recv_pub,
            recv_chain: self.recv_chain.clone(),
            recv_counter: self.recv_counter,
            skipped_keys: self.skipped_keys.clone(),
            skipped_order: self.skipped_order.clone(),
        }
    }
}

/// Ratchets a receiving chain up to (not including) `until`, collecting
/// the keys passed over for packets still on their way.
fn skip(
    chain: &mut ChainKey,
    counter: &mut u64,
    chain_pub: [u8; 32],
    until: u64,
    out: &mut Vec<(SkippedId, [u8; 32])>,
) -> Result<(), CryptoError> {
    if until <= *counter {
        return Ok(());
    }
    let gap = until - *counter;
    if gap > MAX_SKIP {
        return Err(CryptoError::SkipTooFar { counter: until, gap, max_skip: MAX_SKIP });
    }
    while *counter < until {
        let (mk, next) = chain.ratchet();
        *chain = next;
        out.push(((chain_pub, *counter), mk));
        *counter += 1;
    }
    Ok(())
}

/// The root KDF: mixes a DH output into the root key, giving the next root
/// key and the first key of a new chain.
fn kdf_root(root: &[u8; 32], dh: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<Sha256>::new(Some(root), dh);
    let mut out = [0u8; 64];
    hk.expand(ROOT_INFO, &mut out).expect("64 bytes is a valid HKDF-SHA256 output length");
    (out[..32].try_into().unwrap(), out[32..].try_into().unwrap())
}

/// The header is authenticated along with the caller's AAD, so it can't be
/// swapped onto another ciphertext.
fn header_aad(header: &[u8], aad: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(header.len() + aad.len());
    out.extend_from_slice(header);
    out.extend_from_slice(aad);
    out
}

fn open(key: &[u8; 32], counter: u64, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
    ChaCha20Poly1305::new(&Key::from(*key))
        .decrypt(&nonce_for_counter(counter), Payload { msg: sealed, aad })
        .map_err(|_| CryptoError::DecryptionFailed)
}

fn random_secret() -> StaticSecret {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS randomness is unavailable");
    StaticSecret::from(seed)
}

/// Nonces are derived from the never-reused (counter, chain) pair rather
/// than drawn from an RNG: each message key is itself used exactly once, so
/// a deterministic nonce introduces no key/nonce reuse.
fn nonce_for_counter(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pair as the handshake would produce it.
    fn paired_sessions() -> (Session, Session) {
        let a2b = [1u8; 32];
        let b2a = [2u8; 32];
        let root = [3u8; 32];
        let bob_eph = random_secret();
        let bob_eph_pub = PublicKey::from(&bob_eph).to_bytes();
        (
            Session::new(a2b, b2a, root, RatchetStart::Initiator { their_ephemeral: bob_eph_pub }),
            Session::new(b2a, a2b, root, RatchetStart::Responder { our_ephemeral: bob_eph }),
        )
    }

    fn send(from: &mut Session, to: &mut Session, msg: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let s = from.encrypt(msg, b"aad")?;
        to.decrypt(s.counter, &s.ciphertext, b"aad")
    }

    #[test]
    fn round_trip_in_order() {
        let (mut a, mut b) = paired_sessions();
        for i in 0..5 {
            let msg = format!("message {i}");
            let sealed = a.encrypt(msg.as_bytes(), b"call-1").unwrap();
            let opened = b.decrypt(sealed.counter, &sealed.ciphertext, b"call-1").unwrap();
            assert_eq!(opened, msg.as_bytes());
        }
    }

    #[test]
    fn tolerates_out_of_order_delivery() {
        let (mut a, mut b) = paired_sessions();
        let s0 = a.encrypt(b"zero", b"").unwrap();
        let s1 = a.encrypt(b"one", b"").unwrap();
        let s2 = a.encrypt(b"two", b"").unwrap();

        // Deliver 2, then 0, then 1 -- simulating UDP reordering.
        assert_eq!(b.decrypt(s2.counter, &s2.ciphertext, b"").unwrap(), b"two");
        assert_eq!(b.decrypt(s0.counter, &s0.ciphertext, b"").unwrap(), b"zero");
        assert_eq!(b.decrypt(s1.counter, &s1.ciphertext, b"").unwrap(), b"one");
    }

    #[test]
    fn rejects_replay() {
        let (mut a, mut b) = paired_sessions();
        let s0 = a.encrypt(b"zero", b"").unwrap();
        b.decrypt(s0.counter, &s0.ciphertext, b"").unwrap();

        let replayed = b.decrypt(s0.counter, &s0.ciphertext, b"");
        assert!(replayed.is_err());
    }

    #[test]
    fn rejects_wrong_aad() {
        let (mut a, mut b) = paired_sessions();
        let s0 = a.encrypt(b"zero", b"call-1").unwrap();
        let opened = b.decrypt(s0.counter, &s0.ciphertext, b"call-2");
        assert!(opened.is_err());
    }

    #[test]
    fn both_sides_can_send_before_hearing_anything() {
        // A call starts with both people sending at once.
        let (mut a, mut b) = paired_sessions();
        let from_a = a.encrypt(b"hi from a", b"").unwrap();
        let from_b = b.encrypt(b"hi from b", b"").unwrap();
        assert_eq!(a.decrypt(from_b.counter, &from_b.ciphertext, b"").unwrap(), b"hi from b");
        assert_eq!(b.decrypt(from_a.counter, &from_a.ciphertext, b"").unwrap(), b"hi from a");
    }

    #[test]
    fn a_long_two_way_conversation_keeps_stepping_and_stays_in_sync() {
        let (mut a, mut b) = paired_sessions();
        let mut a_keys = std::collections::HashSet::new();
        let mut b_keys = std::collections::HashSet::new();
        for i in 0..(ROTATE_AFTER * 6) {
            let m = i.to_be_bytes();
            assert_eq!(send(&mut a, &mut b, &m).unwrap(), m);
            assert_eq!(send(&mut b, &mut a, &m).unwrap(), m);
            a_keys.insert(a.send_pub);
            b_keys.insert(b.send_pub);
        }
        // Both sides took several DH steps, not just the handshake chains.
        assert!(a_keys.len() >= 3, "a stepped {} times", a_keys.len());
        assert!(b_keys.len() >= 3, "b stepped {} times", b_keys.len());
    }

    #[test]
    fn late_packets_from_before_a_step_still_open() {
        let (mut a, mut b) = paired_sessions();
        // The initiator steps once it has sent ROTATE_AFTER messages.
        for _ in 0..ROTATE_AFTER - 1 {
            send(&mut a, &mut b, b"x").unwrap();
        }
        let late = a.encrypt(b"late, old chain", b"").unwrap();
        let first_new = a.encrypt(b"new chain", b"").unwrap();
        assert_ne!(late.ciphertext[..32], first_new.ciphertext[..32], "should have stepped between these");
        assert_eq!(b.decrypt(first_new.counter, &first_new.ciphertext, b"").unwrap(), b"new chain");
        assert_eq!(b.decrypt(late.counter, &late.ciphertext, b"").unwrap(), b"late, old chain");
    }

    #[test]
    fn a_forged_step_does_not_move_the_ratchet() {
        let (mut a, mut b) = paired_sessions();
        let real = a.encrypt(b"real", b"").unwrap();
        let mut forged = real.ciphertext.clone();
        forged[..32].copy_from_slice(&PublicKey::from(&random_secret()).to_bytes());
        assert!(b.decrypt(real.counter, &forged, b"").is_err());
        // B is unharmed and still reads A.
        assert_eq!(b.decrypt(real.counter, &real.ciphertext, b"").unwrap(), b"real");
    }

    #[test]
    fn an_old_chain_key_is_useless_after_a_step() {
        // Post-compromise security: steal B's state, let both sides step,
        // and the stolen state can no longer read new traffic.
        let (mut a, mut b) = paired_sessions();
        let mut stolen = b.clone_state();
        for _ in 0..(ROTATE_AFTER * 3) {
            send(&mut a, &mut b, b"x").unwrap();
            send(&mut b, &mut a, b"y").unwrap();
        }
        let s = a.encrypt(b"after the steps", b"").unwrap();
        assert!(stolen.decrypt(s.counter, &s.ciphertext, b"").is_err());
        assert_eq!(b.decrypt(s.counter, &s.ciphertext, b"").unwrap(), b"after the steps");
    }

    #[test]
    fn the_skipped_key_store_is_bounded() {
        let (mut a, mut b) = paired_sessions();
        // Deliver only every 900th packet: each gap is within MAX_SKIP,
        // but together they would pass MAX_SKIPPED_TOTAL.
        for i in 0..3000u64 {
            let s = a.encrypt(b"x", b"").unwrap();
            if i % 900 == 0 {
                b.decrypt(s.counter, &s.ciphertext, b"").unwrap();
            }
        }
        assert!(b.skipped_keys.len() <= MAX_SKIPPED_TOTAL);
        assert_eq!(b.skipped_keys.len(), b.skipped_order.len());
    }
}
