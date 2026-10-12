use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

const MESSAGE_KEY_LABEL: &[u8] = &[0x01];
const CHAIN_KEY_LABEL: &[u8] = &[0x02];

/// A symmetric KDF chain key (Signal's "chain key" construction: each step
/// derives a one-time message key and the next chain key via HMAC with
/// distinct labels, so compromising a later key never reveals earlier ones).
#[derive(Clone)]
pub struct ChainKey(pub(crate) [u8; 32]);

impl ChainKey {
    pub fn new(seed: [u8; 32]) -> Self {
        Self(seed)
    }

    /// Advance the chain by one step, returning the message key for the
    /// message just produced/consumed and the chain key for the next one.
    pub fn ratchet(&self) -> ([u8; 32], ChainKey) {
        let message_key = hmac(&self.0, MESSAGE_KEY_LABEL);
        let next_chain = hmac(&self.0, CHAIN_KEY_LABEL);
        (message_key, ChainKey(next_chain))
    }
}

fn hmac(key: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(label);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratcheting_is_deterministic_and_one_way() {
        let seed = [7u8; 32];
        let chain = ChainKey::new(seed);

        let (mk1, chain2) = chain.ratchet();
        let (mk1_again, _) = chain.ratchet();
        assert_eq!(mk1, mk1_again, "ratcheting the same chain key is deterministic");

        let (mk2, _chain3) = chain2.ratchet();
        assert_ne!(mk1, mk2, "successive message keys differ");
    }
}
