//! Sealing short-lived data held in RAM (spec §2: "the buffer is
//! encrypted, in memory, and discarded on a rolling basis").
//!
//! A random key made when the sealer is, never written anywhere, and
//! zeroed when it is dropped. Whatever was sealed under it becomes
//! unreadable at that moment -- the end of a call is a key being
//! forgotten, not a buffer being scrubbed and hoped about.
//!
//! What it buys, stated plainly: a memory dump, a swap file or a crash
//! report taken after the call holds ciphertext. While the call runs the
//! key is in the same process, so code running as you can still read it;
//! nothing user-space changes that.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use zeroize::Zeroizing;

pub struct MemorySealer {
    key: Zeroizing<[u8; 32]>,
    /// Nonces are a counter: the key never outlives this struct, so a
    /// counter cannot repeat under it.
    counter: u64,
}

impl Default for MemorySealer {
    fn default() -> Self {
        Self::new()
    }
}

impl MemorySealer {
    pub fn new() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        getrandom::fill(&mut key[..]).expect("OS randomness is unavailable");
        Self { key, counter: 0 }
    }

    fn cipher(&self) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&Key::from(*self.key))
    }

    /// Seals `plain`, returning nonce and ciphertext together.
    pub fn seal(&mut self, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        let body = self.cipher().encrypt(&Nonce::from(nonce), plain).expect("in-memory seal cannot fail");
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&body);
        out
    }

    /// Opens what [`seal`](Self::seal) produced under this same sealer.
    pub fn open(&self, sealed: &[u8]) -> Option<Zeroizing<Vec<u8>>> {
        if sealed.len() < 12 {
            return None;
        }
        let (nonce, body) = sealed.split_at(12);
        let nonce: [u8; 12] = nonce.try_into().ok()?;
        self.cipher().decrypt(&Nonce::from(nonce), body).ok().map(Zeroizing::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_hides_the_bytes() {
        let mut s = MemorySealer::new();
        let plain = b"what did they just say";
        let sealed = s.seal(plain);
        assert!(!sealed.windows(plain.len()).any(|w| w == plain));
        assert_eq!(s.open(&sealed).unwrap().as_slice(), plain);
    }

    #[test]
    fn another_sealer_cannot_open_it() {
        let mut a = MemorySealer::new();
        let b = MemorySealer::new();
        assert!(b.open(&a.seal(b"x")).is_none());
    }

    #[test]
    fn the_same_data_seals_differently_each_time() {
        let mut s = MemorySealer::new();
        assert_ne!(s.seal(b"same"), s.seal(b"same"));
    }
}
