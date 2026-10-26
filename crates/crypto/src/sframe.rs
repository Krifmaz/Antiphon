//! SFrame (RFC 9605) media encryption, keyed from MLS (spec §3 "Media
//! through a relay -- SFrame; the relay routes encrypted frames without
//! keys").
//!
//! The 1:1 [`Session`](crate::Session) seals each frame *per link*, so a
//! relay that forwards one person's audio to several others -- an SFU --
//! would need a separate upload per listener or the keys to re-seal. SFrame
//! is the other layer: a sender seals a frame **once**, under a key every
//! current group member can derive and nobody else can, and whatever sits
//! in the middle forwards bytes it cannot read. Keys come from the MLS
//! group's exporter ([`crate::group`]), so they follow membership: a new
//! epoch on every join or leave, and someone removed never learns the next
//! one.
//!
//! # What is implemented
//!
//! - The header (RFC 9605 §4.3): compact KID and CTR, checked against the
//!   RFC's Appendix C vectors.
//! - Key and salt derivation (§4.4.2) and the encryption schema (§4.4.3,
//!   §4.4.4) for cipher suite `AES_128_GCM_SHA256_128` (0x0004): a full
//!   128-bit tag, and the AES-GCM the MLS suite already uses. RFC 9605
//!   defines no ChaCha suite, and the truncated-tag AES-CTR suites save a
//!   few bytes a frame at a cost not worth taking for audio.
//! - MLS key provisioning (§5.2): one `base_key` per epoch from
//!   `MLS-Exporter("SFrame 1.0 Base Key", "", 16)`, and each sender's KID
//!   carrying its leaf index and the epoch's low bits, so every sender in
//!   every epoch encrypts under a key and salt of its own and no two ever
//!   share a nonce.
//! - Replay protection (§7.6 leaves it to the application): a 128-frame
//!   sliding window per KID, in the RFC 3711 style the RFC points at.
//!
//! # API for other layers
//!
//! [`SframeContext`] is the one thing to hold. It is deliberately the
//! shape of a per-sender media sealer, so a relay-forwarding (SFU) path can
//! wrap it as one more implementation of whatever trait it seals media
//! through:
//!
//! - [`SframeContext::seal`] -- `&mut self, payload -> Vec<u8>`, under our
//!   own KID in the newest epoch.
//! - [`SframeContext::open`] -- `&mut self, sender_kid, bytes ->
//!   Result<Vec<u8>>`, where `sender_kid` is the member index the transport
//!   says the frame came from: a frame whose header claims a *different*
//!   sender is refused, so one member can't pass their audio off as
//!   another's on a forwarded stream.
//! - [`SframeContext::install_epoch`] after each MLS epoch change (feed it
//!   [`GroupMember::sframe_base_key`](crate::group::GroupMember::sframe_base_key)).
//!
//! The low-level pieces ([`Header`], [`derive_key_salt`], [`seal_frame`],
//! [`open_frame`]) are public for anyone who provisions keys another way.

use std::collections::HashMap;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::Aes128Gcm;

type Nonce = aes_gcm::Nonce<aes_gcm::aead::consts::U12>;
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

use crate::error::CryptoError;

/// `AES_128_GCM_SHA256_128` in the RFC 9605 IANA registry.
pub const CIPHER_SUITE: u16 = 0x0004;
/// `AEAD.Nk` for that suite.
pub const KEY_LEN: usize = 16;
/// `AEAD.Nn`.
pub const NONCE_LEN: usize = 12;
/// `AEAD.Nt`.
pub const TAG_LEN: usize = 16;

/// Low-order epoch bits in an MLS-derived KID (RFC 9605 §5.2's `E`).
/// Sixteen epochs can be live at once -- far more than the one or two a
/// call ever has in flight during a membership change.
pub const EPOCH_BITS: u32 = 4;
/// Member-index bits (`S`). Fixed rather than sized to the group so every
/// member computes the same KID without agreeing on anything; 12 bits is
/// 4096 members, past the spec's "100+".
pub const INDEX_BITS: u32 = 12;

/// The MLS exporter label RFC 9605 §5.2 fixes.
pub const MLS_EXPORTER_LABEL: &str = "SFrame 1.0 Base Key";

/// How many counters behind the newest one a frame may still arrive.
/// 128 frames is 2.5 s of 20 ms audio: anything older is useless to a
/// jitter buffer anyway.
const REPLAY_WINDOW: u64 = 128;

/// The SFrame header (RFC 9605 §4.3): a key id and a counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kid: u64,
    pub ctr: u64,
}

impl Header {
    /// The compact encoding: a config byte, then KID and CTR each in the
    /// fewest big-endian bytes, or folded into the config byte when < 8.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![0u8];
        let mut config = 0u8;
        if self.kid < 8 {
            config |= (self.kid as u8) << 4;
        } else {
            let bytes = minimal_be(self.kid);
            config |= 0x80 | (((bytes.len() - 1) as u8) << 4);
            out.extend_from_slice(&bytes);
        }
        if self.ctr < 8 {
            config |= self.ctr as u8;
        } else {
            let bytes = minimal_be(self.ctr);
            config |= 0x08 | ((bytes.len() - 1) as u8);
            out.extend_from_slice(&bytes);
        }
        out[0] = config;
        out
    }

    /// Parses a header off the front of `bytes`, returning it and its
    /// encoded length.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), CryptoError> {
        let config = *bytes.first().ok_or(CryptoError::MalformedFrame)?;
        let mut at = 1;
        let mut field = |extended: bool, value: u8| -> Result<u64, CryptoError> {
            if !extended {
                return Ok(value as u64);
            }
            let len = value as usize + 1;
            let raw = bytes.get(at..at + len).ok_or(CryptoError::MalformedFrame)?;
            at += len;
            Ok(raw.iter().fold(0u64, |v, b| (v << 8) | *b as u64))
        };
        let kid = field(config & 0x80 != 0, (config >> 4) & 0x07)?;
        let ctr = field(config & 0x08 != 0, config & 0x07)?;
        Ok((Self { kid, ctr }, at))
    }
}

fn minimal_be(value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    let skip = (value.leading_zeros() / 8).min(7) as usize;
    bytes[skip..].to_vec()
}

/// RFC 9605 §4.4.2: `sframe_key` and `sframe_salt` for one KID.
pub fn derive_key_salt(base_key: &[u8], kid: u64) -> ([u8; KEY_LEN], [u8; NONCE_LEN]) {
    let hk = Hkdf::<Sha256>::new(Some(&[]), base_key);
    let label = |what: &[u8]| {
        let mut l = Vec::with_capacity(40);
        l.extend_from_slice(b"SFrame 1.0 Secret ");
        l.extend_from_slice(what);
        l.extend_from_slice(&kid.to_be_bytes());
        l.extend_from_slice(&CIPHER_SUITE.to_be_bytes());
        l
    };
    let mut key = [0u8; KEY_LEN];
    let mut salt = [0u8; NONCE_LEN];
    hk.expand(&label(b"key "), &mut key).expect("16 bytes is a valid HKDF-SHA256 length");
    hk.expand(&label(b"salt "), &mut salt).expect("12 bytes is a valid HKDF-SHA256 length");
    (key, salt)
}

fn nonce(salt: &[u8; NONCE_LEN], ctr: u64) -> Nonce {
    let mut n = *salt;
    for (b, c) in n[NONCE_LEN - 8..].iter_mut().zip(ctr.to_be_bytes()) {
        *b ^= c;
    }
    Nonce::from(n)
}

/// One KID's derived key material.
struct KeyEntry {
    cipher: Aes128Gcm,
    salt: [u8; NONCE_LEN],
}

impl KeyEntry {
    fn new(base_key: &[u8], kid: u64) -> Self {
        let (mut key, salt) = derive_key_salt(base_key, kid);
        let cipher = Aes128Gcm::new_from_slice(&key).expect("16-byte key");
        key.zeroize();
        Self { cipher, salt }
    }
}

/// RFC 9605 §4.4.3, given a key already derived for `kid`: header ||
/// AEAD(header || metadata, plaintext).
pub fn seal_frame(base_key: &[u8], kid: u64, ctr: u64, metadata: &[u8], plaintext: &[u8]) -> Vec<u8> {
    seal_with(&KeyEntry::new(base_key, kid), Header { kid, ctr }, metadata, plaintext)
}

/// RFC 9605 §4.4.4: the inverse of [`seal_frame`]. No replay check -- that
/// is [`SframeContext`]'s job, since it needs state.
pub fn open_frame(base_key: &[u8], metadata: &[u8], frame: &[u8]) -> Result<(Header, Vec<u8>), CryptoError> {
    let (header, len) = Header::decode(frame)?;
    let pt = open_with(&KeyEntry::new(base_key, header.kid), header, &frame[..len], &frame[len..], metadata)?;
    Ok((header, pt))
}

fn seal_with(entry: &KeyEntry, header: Header, metadata: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let mut out = header.encode();
    let mut aad = out.clone();
    aad.extend_from_slice(metadata);
    let ct = entry
        .cipher
        .encrypt(&nonce(&entry.salt, header.ctr), Payload { msg: plaintext, aad: &aad })
        .expect("AES-GCM encryption of a frame cannot fail");
    out.extend_from_slice(&ct);
    out
}

fn open_with(entry: &KeyEntry, header: Header, header_bytes: &[u8], ct: &[u8], metadata: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut aad = header_bytes.to_vec();
    aad.extend_from_slice(metadata);
    entry
        .cipher
        .decrypt(&nonce(&entry.salt, header.ctr), Payload { msg: ct, aad: &aad })
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// RFC 9605 §5.2: `KID = (context << (S + E)) + (index << E) + (epoch mod 2^E)`,
/// with context 0, which gives the shortest header.
pub fn mls_kid(sender_index: u32, epoch: u64) -> u64 {
    ((sender_index as u64 & ((1 << INDEX_BITS) - 1)) << EPOCH_BITS) | (epoch & ((1 << EPOCH_BITS) - 1))
}

/// The sender index a KID names, the inverse of [`mls_kid`].
pub fn kid_sender(kid: u64) -> u32 {
    ((kid >> EPOCH_BITS) & ((1 << INDEX_BITS) - 1)) as u32
}

/// A 128-frame sliding window: the newest counter seen and a bitmap of
/// which of the ones before it have been accepted.
#[derive(Default)]
struct ReplayWindow {
    newest: Option<u64>,
    seen: u128,
}

impl ReplayWindow {
    fn check(&self, ctr: u64) -> Result<(), CryptoError> {
        match self.newest {
            None => Ok(()),
            Some(n) if ctr > n => Ok(()),
            Some(n) if n - ctr >= REPLAY_WINDOW => Err(CryptoError::ReplayOrTooOld { counter: ctr }),
            Some(n) if self.seen & (1u128 << (n - ctr)) != 0 => Err(CryptoError::ReplayOrTooOld { counter: ctr }),
            Some(_) => Ok(()),
        }
    }

    /// Only after the frame authenticated, so a forged header can't move
    /// the window and lock out real frames.
    fn accept(&mut self, ctr: u64) {
        match self.newest {
            Some(n) if ctr <= n => self.seen |= 1u128 << (n - ctr),
            Some(n) => {
                let shift = ctr - n;
                self.seen = if shift >= 128 { 0 } else { self.seen << shift };
                self.seen |= 1;
                self.newest = Some(ctr);
            }
            None => {
                self.seen = 1;
                self.newest = Some(ctr);
            }
        }
    }
}

struct Epoch {
    epoch: u64,
    base_key: Vec<u8>,
}

impl Drop for Epoch {
    fn drop(&mut self) {
        self.base_key.zeroize();
    }
}

/// SFrame state for one member of an MLS group: our sending key in the
/// newest epoch, and receive keys for every sender in every live epoch,
/// derived on first use.
pub struct SframeContext {
    own_index: u32,
    /// Live epochs by their low `EPOCH_BITS`, as RFC 9605 §5.2 requires:
    /// a new epoch with the same low bits replaces the old one.
    epochs: HashMap<u64, Epoch>,
    current: Option<u64>,
    send: Option<(u64, KeyEntry, u64)>,
    recv: HashMap<u64, (u64, KeyEntry, ReplayWindow)>,
}

impl SframeContext {
    /// A context with no keys yet; `seal` and `open` fail until
    /// [`install_epoch`](Self::install_epoch).
    pub fn new(own_index: u32) -> Self {
        Self { own_index, epochs: HashMap::new(), current: None, send: None, recv: HashMap::new() }
    }

    /// Adds an epoch's `base_key` and starts sending under it.
    ///
    /// Earlier epochs stay readable (frames sealed just before the change
    /// are still in flight) until [`retain_only`](Self::retain_only) or
    /// until a later epoch reuses their low bits. `own_index` is our leaf
    /// index in this epoch -- MLS can move it.
    pub fn install_epoch(&mut self, epoch: u64, own_index: u32, base_key: &[u8]) {
        let slot = epoch & ((1 << EPOCH_BITS) - 1);
        self.epochs.insert(slot, Epoch { epoch, base_key: base_key.to_vec() });
        self.recv.retain(|kid, (e, _, _)| *kid & ((1 << EPOCH_BITS) - 1) != slot || *e == epoch);
        self.own_index = own_index;
        let kid = mls_kid(own_index, epoch);
        self.send = Some((kid, KeyEntry::new(base_key, kid), 0));
        self.current = Some(epoch);
    }

    /// Forgets every epoch but `epoch` -- for once the transition is over,
    /// so frames sealed under a key a removed member knew stop being
    /// accepted.
    pub fn retain_only(&mut self, epoch: u64) {
        self.epochs.retain(|_, e| e.epoch == epoch);
        self.recv.retain(|_, (e, _, _)| *e == epoch);
    }

    /// The newest epoch installed.
    pub fn epoch(&self) -> Option<u64> {
        self.current
    }

    /// Our KID in the current epoch.
    pub fn own_kid(&self) -> Option<u64> {
        self.send.as_ref().map(|(kid, _, _)| *kid)
    }

    /// Seals one frame under our key, with no metadata.
    ///
    /// Each call uses the next counter, so a (KID, CTR) pair is never
    /// used twice (RFC 9605 §4.3). Panics if called before any epoch.
    pub fn seal(&mut self, payload: &[u8]) -> Vec<u8> {
        self.seal_with_metadata(payload, &[])
    }

    /// [`seal`](Self::seal) with metadata authenticated alongside (RFC
    /// 9605 §4.4.3), e.g. a stream id an SFU reads in the clear.
    pub fn seal_with_metadata(&mut self, payload: &[u8], metadata: &[u8]) -> Vec<u8> {
        let (kid, entry, ctr) = self.send.as_mut().expect("SFrame seal before any epoch was installed");
        let header = Header { kid: *kid, ctr: *ctr };
        *ctr = ctr.checked_add(1).expect("SFrame counter exhausted");
        seal_with(entry, header, metadata, payload)
    }

    /// Opens a frame from the member at leaf `sender_kid`'s index.
    ///
    /// `sender_kid` is the *sender index* the transport attributes the
    /// frame to; the header's KID must name the same sender, so a member
    /// can't replay their frames as someone else's on a forwarded stream.
    pub fn open(&mut self, sender_kid: u32, bytes: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (header, _) = Header::decode(bytes)?;
        if kid_sender(header.kid) != sender_kid {
            return Err(CryptoError::DecryptionFailed);
        }
        self.open_any(bytes, &[]).map(|(_, pt)| pt)
    }

    /// Opens a frame from whichever member its KID names, returning that
    /// member's leaf index.
    pub fn open_any(&mut self, bytes: &[u8], metadata: &[u8]) -> Result<(u32, Vec<u8>), CryptoError> {
        let (header, len) = Header::decode(bytes)?;
        if Some(header.kid) == self.own_kid() {
            // Our own frames echoed back (an SFU loop, or a forgery
            // under our KID) -- never something to play.
            return Err(CryptoError::DecryptionFailed);
        }
        let slot = header.kid & ((1 << EPOCH_BITS) - 1);
        let epoch = self.epochs.get(&slot).ok_or(CryptoError::DecryptionFailed)?;
        let entry = self
            .recv
            .entry(header.kid)
            .or_insert_with(|| (epoch.epoch, KeyEntry::new(&epoch.base_key, header.kid), ReplayWindow::default()));
        entry.2.check(header.ctr)?;
        let pt = open_with(&entry.1, header, &bytes[..len], &bytes[len..], metadata)?;
        entry.2.accept(header.ctr);
        Ok((kid_sender(header.kid), pt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).unwrap()
    }

    /// RFC 9605 Appendix C.1, a spread of the header vectors.
    #[test]
    fn header_matches_rfc_vectors() {
        let vectors: &[(u64, u64, &str)] = &[
            (0, 0, "00"),
            (0, 1, "01"),
            (0, 255, "08ff"),
            (1, 16777215, "1affffff"),
            (255, 281474976710655, "8dffffffffffffff"),
            (65535, 1, "91ffff"),
            (65536, 16777216, "ab01000001000000"),
            (16777215, 281474976710656, "aeffffff01000000000000"),
            (4294967295, 255, "b8ffffffffff"),
            (4294967296, 4294967295, "cb0100000000ffffffff"),
            (1099511627775, 72057594037927935, "ceffffffffffffffffffffffff"),
            (281474976710655, 256, "d9ffffffffffff0100"),
            (281474976710656, 4294967296, "ec010000000000000100000000"),
            (72057594037927935, 72057594037927936, "efffffffffffffff0100000000000000"),
            (18446744073709551615, 65535, "f9ffffffffffffffffffff"),
        ];
        for &(kid, ctr, enc) in vectors {
            let h = Header { kid, ctr };
            assert_eq!(h.encode(), hex(enc), "kid {kid} ctr {ctr}");
            assert_eq!(Header::decode(&hex(enc)).unwrap(), (h, enc.len() / 2));
        }
    }

    #[test]
    fn header_round_trips_everywhere_and_refuses_truncation() {
        for kid in [0, 7, 8, 255, 256, u32::MAX as u64, u64::MAX] {
            for ctr in [0, 7, 8, 65535, 1 << 40, u64::MAX] {
                let h = Header { kid, ctr };
                let enc = h.encode();
                assert_eq!(Header::decode(&enc).unwrap(), (h, enc.len()));
                if enc.len() > 1 {
                    assert!(Header::decode(&enc[..enc.len() - 1]).is_err());
                }
            }
        }
        assert!(Header::decode(&[]).is_err());
    }

    /// RFC 9605 Appendix C.4, cipher suite 0x0004.
    #[test]
    fn aes_128_gcm_matches_rfc_vector() {
        let base_key = hex("000102030405060708090a0b0c0d0e0f");
        let (key, salt) = derive_key_salt(&base_key, 0x123);
        assert_eq!(key.to_vec(), hex("d34f547f4ca4f9a7447006fe7fcbf768"));
        assert_eq!(salt.to_vec(), hex("75234edefe07819026751816"));
        assert_eq!(nonce(&salt, 0x4567).to_vec(), hex("75234edefe07819026755d71"));

        let metadata = hex("4945544620534672616d65205747");
        let pt = hex("64726166742d696574662d736672616d652d656e63");
        let ct = hex("9901234567b7412c2513a1b66dbb48841bbaf17f598751176ad847681a69c6d0b091c07018ce4adb34eb");
        assert_eq!(seal_frame(&base_key, 0x123, 0x4567, &metadata, &pt), ct);
        let (header, opened) = open_frame(&base_key, &metadata, &ct).unwrap();
        assert_eq!(header, Header { kid: 0x123, ctr: 0x4567 });
        assert_eq!(opened, pt);
        assert!(open_frame(&base_key, b"other metadata", &ct).is_err());
    }

    fn pair() -> (SframeContext, SframeContext) {
        let base = [7u8; KEY_LEN];
        let mut a = SframeContext::new(0);
        let mut b = SframeContext::new(1);
        a.install_epoch(3, 0, &base);
        b.install_epoch(3, 1, &base);
        (a, b)
    }

    #[test]
    fn round_trip_between_members() {
        let (mut a, mut b) = pair();
        for i in 0..300u32 {
            let frame = a.seal(&i.to_le_bytes());
            assert_eq!(b.open(0, &frame).unwrap(), i.to_le_bytes());
            let back = b.seal(b"reply");
            assert_eq!(a.open_any(&back, &[]).unwrap(), (1, b"reply".to_vec()));
        }
    }

    #[test]
    fn replays_and_stale_frames_are_refused_but_reordering_is_not() {
        let (mut a, mut b) = pair();
        let frames: Vec<_> = (0..200).map(|i: u32| a.seal(&i.to_le_bytes())).collect();
        b.open(0, &frames[5]).unwrap();
        b.open(0, &frames[3]).unwrap();
        assert!(matches!(b.open(0, &frames[5]), Err(CryptoError::ReplayOrTooOld { .. })));
        assert!(matches!(b.open(0, &frames[3]), Err(CryptoError::ReplayOrTooOld { .. })));
        b.open(0, &frames[4]).unwrap();
        b.open(0, &frames[199]).unwrap();
        // 194 behind the newest: outside the window.
        assert!(matches!(b.open(0, &frames[6]), Err(CryptoError::ReplayOrTooOld { .. })));
        b.open(0, &frames[150]).unwrap();
    }

    #[test]
    fn wrong_key_tamper_and_wrong_sender_are_refused() {
        let (mut a, mut b) = pair();
        let mut stranger = SframeContext::new(1);
        stranger.install_epoch(3, 1, &[8u8; KEY_LEN]);
        let frame = a.seal(b"secret");
        assert!(matches!(stranger.open(0, &frame), Err(CryptoError::DecryptionFailed)));

        let mut tampered = frame.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(b.open(0, &tampered).is_err());
        // A frame refused for tampering doesn't burn its counter.
        assert_eq!(b.open(0, &frame).unwrap(), b"secret");

        // Attributed to sender 2 by the transport, but sealed by sender 0.
        let frame = a.seal(b"impersonation");
        assert!(b.open(2, &frame).is_err());
        // Nor is our own frame echoed back accepted.
        let own = b.seal(b"echo");
        assert!(b.open(1, &own).is_err());
    }

    #[test]
    fn a_new_epoch_locks_out_whoever_only_had_the_old_one() {
        let (mut a, mut b) = pair();
        let (_, mut leaver) = pair();
        a.install_epoch(4, 0, &[9u8; KEY_LEN]);
        b.install_epoch(4, 1, &[9u8; KEY_LEN]);
        let frame = a.seal(b"after");
        assert_eq!(Header::decode(&frame).unwrap().0.kid, mls_kid(0, 4));
        assert_eq!(b.open(0, &frame).unwrap(), b"after");
        assert!(leaver.open(0, &frame).is_err());

        // Old-epoch frames still open during the transition, until
        // retain_only drops them.
        let mut old = SframeContext::new(0);
        old.install_epoch(3, 0, &[7u8; KEY_LEN]);
        let late = old.seal(b"late");
        assert_eq!(b.open(0, &late).unwrap(), b"late");
        let later = old.seal(b"later");
        b.retain_only(4);
        assert!(b.open(0, &later).is_err());
    }

    #[test]
    fn kids_encode_sender_and_epoch() {
        assert_eq!(mls_kid(0, 0), 0);
        assert_eq!(mls_kid(3, 14), 0x3e);
        assert_eq!(mls_kid(20, 14), 0x14e);
        assert_eq!(mls_kid(5, 15 + 16), 0x5f);
        assert_eq!(kid_sender(0x14e), 20);
    }
}
