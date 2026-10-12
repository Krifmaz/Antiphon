use std::collections::HashMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::error::CryptoError;
use crate::ratchet::ChainKey;

/// How many message keys we're willing to derive-and-cache ahead of the
/// last one we actually consumed. Bounds both memory use and the cost of a
/// malicious peer sending a huge counter to force wasted ratcheting.
const MAX_SKIP: u64 = 1000;

/// An encrypted frame ready to go on the wire, or just received off it.
/// `counter` is sent in the clear (as protocol framing, spec §3) — it is not
/// secret, only the chain state that derives a key from it is.
pub struct Sealed {
    pub counter: u64,
    pub ciphertext: Vec<u8>,
}

/// A live 1:1 encrypted session: one send chain, one receive chain, each
/// ratcheting forward independently. See [`crate::handshake`] for how the
/// two chains are seeded from a handshake.
pub struct Session {
    send_chain: ChainKey,
    send_counter: u64,

    recv_chain: ChainKey,
    recv_counter: u64,
    skipped_keys: HashMap<u64, [u8; 32]>,
}

impl Session {
    pub(crate) fn new(send_seed: [u8; 32], recv_seed: [u8; 32]) -> Self {
        Self {
            send_chain: ChainKey::new(send_seed),
            send_counter: 0,
            recv_chain: ChainKey::new(recv_seed),
            recv_counter: 0,
            skipped_keys: HashMap::new(),
        }
    }

    pub fn encrypt(&mut self, plaintext: &[u8], aad: &[u8]) -> Result<Sealed, CryptoError> {
        let (key_bytes, next_chain) = self.send_chain.ratchet();
        let counter = self.send_counter;
        self.send_chain = next_chain;
        self.send_counter += 1;

        let cipher = ChaCha20Poly1305::new(&Key::from(key_bytes));
        let nonce = nonce_for_counter(counter);
        let ciphertext = cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad })
            .map_err(|_| CryptoError::DecryptionFailed)?;

        Ok(Sealed { counter, ciphertext })
    }

    pub fn decrypt(&mut self, counter: u64, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let key_bytes = if counter < self.recv_counter {
            self.skipped_keys
                .remove(&counter)
                .ok_or(CryptoError::ReplayOrTooOld { counter })?
        } else {
            let gap = counter - self.recv_counter;
            if gap > MAX_SKIP {
                return Err(CryptoError::SkipTooFar {
                    counter,
                    gap,
                    max_skip: MAX_SKIP,
                });
            }

            // Ratchet forward, caching every key we skip over so an
            // out-of-order (but not lost) packet can still be decrypted
            // later. This is what makes the chain tolerant of UDP reordering.
            let mut chain = self.recv_chain.clone();
            let mut key_bytes = [0u8; 32];
            for i in self.recv_counter..=counter {
                let (mk, next_chain) = chain.ratchet();
                chain = next_chain;
                if i == counter {
                    key_bytes = mk;
                } else {
                    self.skipped_keys.insert(i, mk);
                }
            }
            self.recv_chain = chain;
            self.recv_counter = counter + 1;
            key_bytes
        };

        let cipher = ChaCha20Poly1305::new(&Key::from(key_bytes));
        let nonce = nonce_for_counter(counter);
        cipher
            .decrypt(&nonce, Payload { msg: ciphertext, aad })
            .map_err(|_| CryptoError::DecryptionFailed)
    }
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

    fn paired_sessions() -> (Session, Session) {
        let a2b = [1u8; 32];
        let b2a = [2u8; 32];
        (Session::new(a2b, b2a), Session::new(b2a, a2b))
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
}
