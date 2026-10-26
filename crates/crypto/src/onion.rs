//! Layer keys for anonymity mode: a call routed through two or three
//! relays so that no single relay knows both who is calling (their IP
//! address) and whom they are calling (the target relay and the token).
//!
//! Each relay that offers hops holds a **hop key** ([`HopSecret`]), an
//! X25519 keypair whose only job is to peel routing layers. It never
//! touches call content: the call inside the circuit is the same sealed
//! [`crate::Session`] traffic a direct or relayed call carries, so a hop
//! that is compromised -- key and all -- learns where the next layer goes,
//! never a word of the call.
//!
//! For every hop on a circuit the caller makes a fresh ephemeral X25519
//! key, agrees a secret with that hop's key, and expands it with HKDF into
//! three ChaCha20-Poly1305 keys ([`LayerKeys`]):
//!
//! - **setup**, used once, for that hop's slice of the circuit-building
//!   message (where to send the rest). A fresh ephemeral per hop per
//!   circuit makes it a one-time key, so its fixed nonce never repeats.
//! - **forward**, for traffic travelling away from the caller.
//! - **backward**, for traffic travelling back to the caller.
//!
//! Forward and backward are separate keys so a hop's own sealed output can
//! never be reflected back at it as if it came from the other side.
//!
//! A data layer is `[seq: u64 BE][ciphertext + tag]`, and the sequence
//! number is the nonce. It travels in the clear because UDP drops and
//! reorders: a hop cannot keep an implicit counter in step with its
//! neighbour. Each hop's layer carries its own sequence number under its
//! own key, so the bytes one hop forwards share nothing visible with the
//! bytes it received.
//!
//! What this module deliberately does not do: pad to a fixed size. Every
//! layer adds [`LAYER_OVERHEAD`] bytes, so a hop that sees the size of a
//! cell can estimate how many layers remain inside it -- that is, its
//! position on the path. See `antiphon_net::onion` for what that does and
//! does not give away.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroize;

use crate::error::CryptoError;

const HOP_SALT: &[u8] = b"antiphon/onion/hop/v1";

/// Bytes a data layer adds: the sequence number and the AEAD tag.
pub const LAYER_OVERHEAD: usize = 8 + 16;
/// Bytes a setup slice adds: the ephemeral key and the AEAD tag.
pub const SETUP_OVERHEAD: usize = 32 + 16;

/// A relay's hop key. Peels routing layers; never decrypts a call.
pub struct HopSecret(StaticSecret);

impl HopSecret {
    pub fn generate() -> Self {
        Self(StaticSecret::random())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(StaticSecret::from(bytes))
    }

    /// For persisting the key (`antiphon-relay --hop-key FILE`), so the
    /// public half people learned stays valid across restarts.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    pub fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }

    /// The hop's side of the agreement, given the ephemeral key that
    /// arrived in a circuit-building message.
    pub fn keys_for(&self, ephemeral: &[u8; 32]) -> Result<LayerKeys, CryptoError> {
        let shared = self.0.diffie_hellman(&PublicKey::from(*ephemeral));
        if !shared.was_contributory() {
            // A low-order point: the "shared" secret is a constant anyone
            // could compute.
            return Err(CryptoError::DecryptionFailed);
        }
        Ok(LayerKeys::derive(shared.as_bytes(), ephemeral, &self.public()))
    }
}

/// The caller's side: a fresh ephemeral for one hop on one circuit.
/// Returns the ephemeral public key to send and the keys agreed with it.
pub fn client_keys(hop_public: &[u8; 32]) -> Result<([u8; 32], LayerKeys), CryptoError> {
    let eph = StaticSecret::random();
    let eph_pub = PublicKey::from(&eph).to_bytes();
    let shared = eph.diffie_hellman(&PublicKey::from(*hop_public));
    if !shared.was_contributory() {
        return Err(CryptoError::DecryptionFailed);
    }
    Ok((eph_pub, LayerKeys::derive(shared.as_bytes(), &eph_pub, hop_public)))
}

/// The three keys one hop shares with the caller for one circuit.
pub struct LayerKeys {
    setup: [u8; 32],
    forward: [u8; 32],
    backward: [u8; 32],
}

impl Drop for LayerKeys {
    fn drop(&mut self) {
        self.setup.zeroize();
        self.forward.zeroize();
        self.backward.zeroize();
    }
}

impl LayerKeys {
    fn derive(shared: &[u8; 32], eph_pub: &[u8; 32], hop_pub: &[u8; 32]) -> Self {
        let mut info = [0u8; 64];
        info[..32].copy_from_slice(eph_pub);
        info[32..].copy_from_slice(hop_pub);
        let mut okm = [0u8; 96];
        Hkdf::<Sha256>::new(Some(HOP_SALT), shared)
            .expand(&info, &mut okm)
            .expect("96 bytes is a valid HKDF-SHA256 output length");
        let mut keys = Self { setup: [0; 32], forward: [0; 32], backward: [0; 32] };
        keys.setup.copy_from_slice(&okm[..32]);
        keys.forward.copy_from_slice(&okm[32..64]);
        keys.backward.copy_from_slice(&okm[64..]);
        okm.zeroize();
        keys
    }

    pub fn seal_setup(&self, plain: &[u8]) -> Vec<u8> {
        cipher(&self.setup).encrypt(&Nonce::from([0u8; 12]), plain).expect("ChaCha20-Poly1305 sealing does not fail")
    }

    pub fn open_setup(&self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        cipher(&self.setup).decrypt(&Nonce::from([0u8; 12]), sealed).map_err(|_| CryptoError::DecryptionFailed)
    }

    /// One forward layer: `[seq][ciphertext]`. `seq` must not repeat
    /// under these keys -- the sender keeps a counter.
    pub fn seal_forward(&self, seq: u64, plain: &[u8]) -> Vec<u8> {
        seal_layer(&self.forward, seq, plain)
    }

    pub fn open_forward(&self, layer: &[u8]) -> Result<Vec<u8>, CryptoError> {
        open_layer(&self.forward, layer)
    }

    pub fn seal_backward(&self, seq: u64, plain: &[u8]) -> Vec<u8> {
        seal_layer(&self.backward, seq, plain)
    }

    pub fn open_backward(&self, layer: &[u8]) -> Result<Vec<u8>, CryptoError> {
        open_layer(&self.backward, layer)
    }
}

fn cipher(key: &[u8; 32]) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&Key::from(*key))
}

fn nonce(seq: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&seq.to_be_bytes());
    Nonce::from(n)
}

fn seal_layer(key: &[u8; 32], seq: u64, plain: &[u8]) -> Vec<u8> {
    let sealed = cipher(key).encrypt(&nonce(seq), plain).expect("ChaCha20-Poly1305 sealing does not fail");
    let mut out = Vec::with_capacity(8 + sealed.len());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&sealed);
    out
}

fn open_layer(key: &[u8; 32], layer: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if layer.len() < LAYER_OVERHEAD {
        return Err(CryptoError::DecryptionFailed);
    }
    let (seq, sealed) = layer.split_at(8);
    let seq = u64::from_be_bytes(seq.try_into().expect("split at 8"));
    cipher(key).decrypt(&nonce(seq), sealed).map_err(|_| CryptoError::DecryptionFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_and_hop_agree_on_every_key() {
        let hop = HopSecret::generate();
        let (eph, mine) = client_keys(&hop.public()).unwrap();
        let theirs = hop.keys_for(&eph).unwrap();
        assert_eq!(theirs.open_setup(&mine.seal_setup(b"next: 10.0.0.2")).unwrap(), b"next: 10.0.0.2");
        assert_eq!(theirs.open_forward(&mine.seal_forward(7, b"fwd")).unwrap(), b"fwd");
        assert_eq!(mine.open_backward(&theirs.seal_backward(9, b"back")).unwrap(), b"back");
        assert_eq!(mine.seal_forward(1, b"x").len(), 1 + LAYER_OVERHEAD);
        assert_eq!(mine.seal_setup(b"x").len() + 32, 1 + SETUP_OVERHEAD);
    }

    #[test]
    fn directions_and_hops_do_not_cross() {
        let hop = HopSecret::generate();
        let other = HopSecret::generate();
        let (eph, mine) = client_keys(&hop.public()).unwrap();
        let theirs = hop.keys_for(&eph).unwrap();
        // A forward layer is not a backward one, even under the same agreement.
        assert!(mine.open_backward(&theirs.seal_forward(1, b"x")).is_err());
        // A different hop key agrees on something else entirely.
        assert!(other.keys_for(&eph).unwrap().open_forward(&mine.seal_forward(1, b"x")).is_err());
    }

    #[test]
    fn damaged_and_short_layers_are_refused() {
        let hop = HopSecret::generate();
        let (eph, mine) = client_keys(&hop.public()).unwrap();
        let theirs = hop.keys_for(&eph).unwrap();
        let mut layer = mine.seal_forward(3, b"hello");
        layer[0] ^= 1; // the sequence number is the nonce: changing it is tampering
        assert!(theirs.open_forward(&layer).is_err());
        assert!(theirs.open_forward(&[0u8; LAYER_OVERHEAD - 1]).is_err());
    }

    #[test]
    fn a_low_order_ephemeral_is_refused() {
        let hop = HopSecret::generate();
        assert!(hop.keys_for(&[0u8; 32]).is_err());
        assert!(client_keys(&[0u8; 32]).is_err());
    }

    #[test]
    fn the_hop_key_survives_a_round_trip_to_disk() {
        let hop = HopSecret::generate();
        assert_eq!(HopSecret::from_bytes(hop.to_bytes()).public(), hop.public());
    }
}
