//! Verification codes (spec §4 "Knowing it's really them").
//!
//! > "A short code both of you can compare in person, by voice, or over
//! > another app. If it matches, nobody is intercepting you."
//!
//! The handshake proves the peer holds a particular identity key. It
//! cannot prove that key belongs to the human you think it does -- that
//! gap is exactly what a machine-in-the-middle exploits. Comparing this
//! code over a channel the attacker doesn't control closes it.
//!
//! The code is derived from both identities, sorted, so both ends compute
//! the same string without needing to agree on who is "first". Digits
//! rather than hex because these get read aloud, and hex is miserable to
//! read aloud.

use sha2::{Digest, Sha256};

const CONTEXT: &[u8] = b"voip-app/verification/v1";

/// How many decimal digits the code carries, in groups of five.
const GROUPS: usize = 8;
const DIGITS_PER_GROUP: u32 = 5;

/// The short code two people compare out of band.
///
/// Both sides pass the same pair of identity keys in either order and get
/// the same string back.
pub fn verification_code(a_identity_key: &[u8; 32], b_identity_key: &[u8; 32]) -> String {
    // Sorting is what makes the code symmetric. Without it Alice and Bob
    // would compute different codes and every comparison would "fail",
    // training users to ignore the check.
    let (first, second) = if a_identity_key <= b_identity_key {
        (a_identity_key, b_identity_key)
    } else {
        (b_identity_key, a_identity_key)
    };

    let mut hasher = Sha256::new();
    hasher.update(CONTEXT);
    hasher.update(first);
    hasher.update(second);
    let digest = hasher.finalize();

    let divisor = 10u32.pow(DIGITS_PER_GROUP);
    let mut groups = Vec::with_capacity(GROUPS);
    for i in 0..GROUPS {
        // Four bytes per group, folded into a 5-digit number.
        let chunk = u32::from_be_bytes([
            digest[i * 4],
            digest[i * 4 + 1],
            digest[i * 4 + 2],
            digest[i * 4 + 3],
        ]);
        groups.push(format!("{:0width$}", chunk % divisor, width = DIGITS_PER_GROUP as usize));
    }

    groups.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityKeyPair;

    #[test]
    fn both_sides_compute_the_same_code_regardless_of_argument_order() {
        let alice = IdentityKeyPair::generate().public().identity_key;
        let bob = IdentityKeyPair::generate().public().identity_key;

        assert_eq!(verification_code(&alice, &bob), verification_code(&bob, &alice));
    }

    #[test]
    fn a_different_peer_produces_a_different_code() {
        let alice = IdentityKeyPair::generate().public().identity_key;
        let bob = IdentityKeyPair::generate().public().identity_key;
        let mallory = IdentityKeyPair::generate().public().identity_key;

        // This is the whole security property: if Mallory is in the
        // middle, Alice's code with Mallory cannot match Alice's code
        // with Bob, so reading it aloud catches her.
        assert_ne!(verification_code(&alice, &bob), verification_code(&alice, &mallory));
    }

    #[test]
    fn the_code_is_readable_aloud() {
        let alice = IdentityKeyPair::generate().public().identity_key;
        let bob = IdentityKeyPair::generate().public().identity_key;
        let code = verification_code(&alice, &bob);

        let groups: Vec<&str> = code.split(' ').collect();
        assert_eq!(groups.len(), GROUPS);
        for group in groups {
            assert_eq!(group.len(), DIGITS_PER_GROUP as usize);
            assert!(group.chars().all(|c| c.is_ascii_digit()));
        }
    }

    #[test]
    fn the_code_is_stable_across_calls() {
        let alice = IdentityKeyPair::generate().public().identity_key;
        let bob = IdentityKeyPair::generate().public().identity_key;

        // A code that changed between calls would be useless -- users
        // verify once and expect it to hold.
        assert_eq!(verification_code(&alice, &bob), verification_code(&alice, &bob));
    }
}
