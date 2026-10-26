//! Sealing group-call media end to end, so an SFU forwards frames it
//! cannot read (spec §3 "SFU relay forwarding encrypted frames").
//!
//! The SFU ([`crate::sfu`]) treats every payload as opaque bytes; what
//! makes that true is a [`MediaSealer`] on each member. The trait is
//! deliberately tiny -- seal my frame, open a frame from that member --
//! so the group key schedule behind it can change without touching the
//! transport. [`SenderKeySealer`] is the working default: each member has
//! its own random key, distributed to the others out of band (over the
//! pairwise encrypted sessions members already have). A later MLS/SFrame
//! implementation slots in behind the same trait.

use std::collections::HashMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::sfu::MemberTag;

/// Seals this member's media and opens everyone else's.
///
/// Contract for implementations:
///
/// - `seal` output must be self-contained: whatever the receiver needs
///   besides its keys (a counter, an epoch) travels inside it. Frames go
///   over UDP, so they may arrive late, twice, or not at all.
/// - `open` returns `None` for anything it cannot authenticate -- a
///   forged or corrupted frame, an unknown sender, or a replay -- and
///   never panics on hostile input. `sender` is the member tag the SFU
///   routed the frame under; bind it into the authentication so one
///   member cannot pass a frame off as another's.
/// - Output stays under [`crate::sfu::MAX_MEDIA_PAYLOAD`] for an audio
///   frame; [`SenderKeySealer`] adds [`SEAL_OVERHEAD`] bytes.
pub trait MediaSealer: Send {
    fn seal(&mut self, plain: &[u8]) -> Vec<u8>;
    fn open(&mut self, sender: MemberTag, sealed: &[u8]) -> Option<Vec<u8>>;
}

pub type SenderKey = [u8; 32];

/// Bytes [`SenderKeySealer::seal`] adds: an 8-byte counter and a 16-byte
/// Poly1305 tag.
pub const SEAL_OVERHEAD: usize = 8 + 16;

/// How far behind the newest frame a late frame may still be accepted, in
/// frames. 128 frames of 20 ms is 2.5 s -- longer than any jitter buffer
/// would wait for.
pub const REPLAY_WINDOW: u64 = 128;

const AAD_PREFIX: &[u8] = b"antiphon/sfu/media/v1";

/// A fresh random sender key. Generate one per member per call and never
/// reuse it: the nonce is a counter that starts at zero with every new
/// [`SenderKeySealer`], so a reused key would reuse nonces.
pub fn generate_sender_key() -> SenderKey {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("OS randomness is unavailable");
    key
}

/// The default [`MediaSealer`]: per-sender ChaCha20-Poly1305 keys.
///
/// Each member seals with its own key and a counter nonce; receivers hold
/// every other member's key (added with [`SenderKeySealer::add_sender`]
/// as it arrives out of band) and keep a sliding replay window per
/// sender. Someone leaving the call is handled by
/// [`SenderKeySealer::remove_sender`]; for forward secrecy against a
/// departed member, everyone switches to a new sealer with fresh keys.
pub struct SenderKeySealer {
    me: MemberTag,
    cipher: ChaCha20Poly1305,
    counter: u64,
    senders: HashMap<MemberTag, Receiving>,
}

struct Receiving {
    cipher: ChaCha20Poly1305,
    window: ReplayWindow,
}

impl SenderKeySealer {
    /// `me` is this member's tag in the room, `key` its own sender key
    /// (from [`generate_sender_key`]), which the caller distributes.
    pub fn new(me: MemberTag, key: SenderKey) -> Self {
        Self { me, cipher: ChaCha20Poly1305::new(&Key::from(key)), counter: 0, senders: HashMap::new() }
    }

    /// Accepts frames from `sender` sealed under `key`. Replacing a key
    /// restarts that sender's replay window, which a fresh key requires.
    pub fn add_sender(&mut self, sender: MemberTag, key: SenderKey) {
        self.senders.insert(
            sender,
            Receiving { cipher: ChaCha20Poly1305::new(&Key::from(key)), window: ReplayWindow::default() },
        );
    }

    pub fn remove_sender(&mut self, sender: &MemberTag) {
        self.senders.remove(sender);
    }

    pub fn knows(&self, sender: &MemberTag) -> bool {
        self.senders.contains_key(sender)
    }
}

impl MediaSealer for SenderKeySealer {
    fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let counter = self.counter;
        self.counter += 1;
        let sealed = self
            .cipher
            .encrypt(&nonce(counter), Payload { msg: plain, aad: &aad(&self.me) })
            .expect("ChaCha20-Poly1305 sealing cannot fail for an audio frame");
        let mut out = Vec::with_capacity(8 + sealed.len());
        out.extend_from_slice(&counter.to_be_bytes());
        out.extend_from_slice(&sealed);
        out
    }

    fn open(&mut self, sender: MemberTag, sealed: &[u8]) -> Option<Vec<u8>> {
        let (counter, body) = sealed.split_at_checked(8)?;
        let counter = u64::from_be_bytes(counter.try_into().ok()?);
        let rx = self.senders.get_mut(&sender)?;
        if !rx.window.may_accept(counter) {
            return None;
        }
        let plain = rx.cipher.decrypt(&nonce(counter), Payload { msg: body, aad: &aad(&sender) }).ok()?;
        // Only an authentic frame moves the window: a forged counter must
        // not be able to push real frames out of it.
        rx.window.mark(counter);
        Some(plain)
    }
}

fn nonce(counter: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::from(bytes)
}

fn aad(sender: &MemberTag) -> Vec<u8> {
    let mut out = AAD_PREFIX.to_vec();
    out.extend_from_slice(sender);
    out
}

/// A sliding window over counters: accepts each counter at most once,
/// and nothing more than [`REPLAY_WINDOW`] behind the newest.
#[derive(Default)]
struct ReplayWindow {
    /// One past the highest counter accepted (0: none yet).
    top: u64,
    /// Bit `i` set: counter `top - 1 - i` was accepted.
    seen: u128,
}

impl ReplayWindow {
    fn may_accept(&self, counter: u64) -> bool {
        if counter >= self.top {
            return true;
        }
        let behind = self.top - 1 - counter;
        behind < REPLAY_WINDOW && self.seen & (1u128 << behind) == 0
    }

    fn mark(&mut self, counter: u64) {
        if counter >= self.top {
            let shift = counter + 1 - self.top;
            self.seen = if shift >= 128 { 0 } else { self.seen << shift };
            self.seen |= 1;
            self.top = counter + 1;
        } else {
            self.seen |= 1u128 << (self.top - 1 - counter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (SenderKeySealer, SenderKeySealer) {
        let (a_tag, b_tag) = ([1u8; 16], [2u8; 16]);
        let (a_key, b_key) = (generate_sender_key(), generate_sender_key());
        let mut a = SenderKeySealer::new(a_tag, a_key);
        let mut b = SenderKeySealer::new(b_tag, b_key);
        a.add_sender(b_tag, b_key);
        b.add_sender(a_tag, a_key);
        (a, b)
    }

    #[test]
    fn frames_round_trip_both_ways() {
        let (mut a, mut b) = pair();
        let sealed = a.seal(b"from a");
        assert_eq!(sealed.len(), 6 + SEAL_OVERHEAD);
        assert_eq!(b.open([1; 16], &sealed).as_deref(), Some(&b"from a"[..]));
        let back = b.seal(b"from b");
        assert_eq!(a.open([2; 16], &back).as_deref(), Some(&b"from b"[..]));
    }

    #[test]
    fn replays_are_refused_but_late_frames_are_not() {
        let (mut a, mut b) = pair();
        let frames: Vec<_> = (0..5).map(|i| a.seal(&[i])).collect();
        assert!(b.open([1; 16], &frames[4]).is_some());
        assert!(b.open([1; 16], &frames[4]).is_none(), "replay");
        assert!(b.open([1; 16], &frames[1]).is_some(), "late but new");
        assert!(b.open([1; 16], &frames[1]).is_none(), "late replay");
        assert!(b.open([1; 16], &frames[0]).is_some());
    }

    #[test]
    fn frames_too_far_behind_are_refused() {
        let (mut a, mut b) = pair();
        let old = a.seal(b"old");
        for _ in 0..REPLAY_WINDOW {
            a.seal(b"skip");
        }
        assert!(b.open([1; 16], &a.seal(b"new")).is_some());
        assert!(b.open([1; 16], &old).is_none());
    }

    #[test]
    fn a_frame_under_another_members_tag_does_not_open() {
        let (mut a, mut b) = pair();
        let c_tag = [3u8; 16];
        // b knows a key for c that happens to equal a's -- the tag in the
        // authenticated data still tells them apart.
        let a_key = generate_sender_key();
        let mut a2 = SenderKeySealer::new([1; 16], a_key);
        b.add_sender(c_tag, a_key);
        assert!(b.open(c_tag, &a2.seal(b"spoof")).is_none());
        assert!(b.open([9; 16], &a.seal(b"unknown sender")).is_none());
    }

    #[test]
    fn tampering_and_garbage_are_refused_without_moving_the_window() {
        let (mut a, mut b) = pair();
        let mut sealed = a.seal(b"frame");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert!(b.open([1; 16], &sealed).is_none());
        assert!(b.open([1; 16], b"short").is_none());
        // A forged far-future counter must not push real frames out.
        let mut forged = vec![0xff; 8];
        forged.extend_from_slice(&[0; 20]);
        assert!(b.open([1; 16], &forged).is_none());
        assert!(b.open([1; 16], &a.seal(b"real")).is_some());
    }

    #[test]
    fn a_removed_sender_is_no_longer_heard() {
        let (mut a, mut b) = pair();
        b.remove_sender(&[1; 16]);
        assert!(!b.knows(&[1; 16]));
        assert!(b.open([1; 16], &a.seal(b"gone")).is_none());
    }

    #[test]
    fn the_sealer_is_usable_as_a_trait_object() {
        let (a, mut b) = pair();
        let mut boxed: Box<dyn MediaSealer> = Box::new(a);
        let sealed = boxed.seal(b"dyn");
        assert!(b.open([1; 16], &sealed).is_some());
    }
}
