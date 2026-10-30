//! Splitting the recovery phrase into shares (spec §3 "Backup & recovery":
//! "optional Shamir split across trusted friends/devices").
//!
//! Shamir's secret sharing over GF(256), byte by byte: each byte of the
//! seed is the constant term of a random polynomial of degree
//! `threshold - 1`, and share `x` holds that polynomial evaluated at `x`.
//! Any `threshold` shares rebuild the seed by Lagrange interpolation at
//! zero; fewer reveal nothing about it at all -- not a weaker guess, no
//! information.
//!
//! Each share is written as a short header and 24 words, so it can go on
//! paper the same way the phrase does:
//!
//! ```text
//! 2-1-9f3c abandon ability able ... (24 words)
//! ```
//!
//! `2` shares are needed, this is share `1`, and `9f3c` is a tag derived
//! from the seed. The words carry their own checksum, so a mistyped word
//! is caught per share. The tag catches what the words can't: shares from
//! two different splits, or too few of them, interpolate to *some* 32
//! bytes without complaint, and restoring that would silently produce a
//! stranger's identity. With the tag it is an error instead.

use bip39::Mnemonic;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// One share, as written down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Share {
    pub threshold: u8,
    pub index: u8,
    pub tag: u16,
    pub value: [u8; 32],
}

impl Share {
    pub fn to_text(&self) -> String {
        let words = Mnemonic::from_entropy(&self.value).expect("32 bytes is a valid BIP-39 entropy length");
        format!("{}-{}-{:04x} {}", self.threshold, self.index, self.tag, words)
    }

    /// Parses a share typed back in. Forgiving about case and spacing,
    /// strict about everything else.
    pub fn parse(text: &str) -> Result<Self, CryptoError> {
        let mut parts = text.split_whitespace();
        let header = parts.next().ok_or(CryptoError::InvalidShare)?;
        let mut h = header.split('-');
        let threshold: u8 = h.next().and_then(|s| s.parse().ok()).ok_or(CryptoError::InvalidShare)?;
        let index: u8 = h.next().and_then(|s| s.parse().ok()).ok_or(CryptoError::InvalidShare)?;
        let tag = h.next().and_then(|s| u16::from_str_radix(s, 16).ok()).ok_or(CryptoError::InvalidShare)?;
        if h.next().is_some() || threshold < 2 || index == 0 {
            return Err(CryptoError::InvalidShare);
        }
        let words = parts.collect::<Vec<_>>().join(" ").to_lowercase();
        let mnemonic = Mnemonic::parse_normalized(&words).map_err(|_| CryptoError::InvalidShare)?;
        let (entropy, len) = mnemonic.to_entropy_array();
        if len != 32 {
            return Err(CryptoError::InvalidShare);
        }
        let mut value = [0u8; 32];
        value.copy_from_slice(&entropy[..32]);
        Ok(Self { threshold, index, tag, value })
    }
}

fn tag(seed: &[u8; 32]) -> u16 {
    let digest = Sha256::new_with_prefix(b"antiphon/shamir-tag/v1").chain_update(seed).finalize();
    u16::from_be_bytes([digest[0], digest[1]])
}

/// Splits a seed into `shares` shares, any `threshold` of which rebuild it.
pub fn split(seed: &[u8; 32], threshold: u8, shares: u8) -> Result<Vec<Share>, CryptoError> {
    if threshold < 2 || shares < threshold {
        return Err(CryptoError::InvalidShare);
    }
    let t = tag(seed);
    // coefficients[byte][k] for k in 1..threshold; the constant term is the seed.
    let mut coefficients = Zeroizing::new(vec![0u8; 32 * (threshold as usize - 1)]);
    getrandom::fill(&mut coefficients).expect("OS randomness is unavailable");
    Ok((1..=shares)
        .map(|x| {
            let mut value = [0u8; 32];
            for (i, v) in value.iter_mut().enumerate() {
                let c = &coefficients[i * (threshold as usize - 1)..(i + 1) * (threshold as usize - 1)];
                // Horner's rule, highest power first.
                let mut acc = 0u8;
                for &k in c.iter().rev() {
                    acc = mul(acc, x) ^ k;
                }
                *v = mul(acc, x) ^ seed[i];
            }
            Share { threshold, index: x, tag: t, value }
        })
        .collect())
}

/// Rebuilds the seed from shares. Needs at least `threshold` distinct
/// shares from the same split; anything else is an error, never a wrong
/// seed.
pub fn combine(shares: &[Share]) -> Result<[u8; 32], CryptoError> {
    let first = shares.first().ok_or(CryptoError::NotEnoughShares)?;
    let mut used: Vec<&Share> = Vec::new();
    for s in shares {
        if s.threshold != first.threshold || s.tag != first.tag {
            return Err(CryptoError::SharesDoNotMatch);
        }
        if !used.iter().any(|u| u.index == s.index) {
            used.push(s);
        }
    }
    if used.len() < first.threshold as usize {
        return Err(CryptoError::NotEnoughShares);
    }
    used.truncate(first.threshold as usize);

    let mut seed = [0u8; 32];
    for (i, out) in seed.iter_mut().enumerate() {
        let mut acc = 0u8;
        for (j, sj) in used.iter().enumerate() {
            // Lagrange basis at zero: prod over m != j of x_m / (x_m - x_j).
            // In GF(2^8) subtraction is xor.
            let mut basis = 1u8;
            for (m, sm) in used.iter().enumerate() {
                if m != j {
                    basis = mul(basis, div(sm.index, sm.index ^ sj.index));
                }
            }
            acc ^= mul(basis, sj.value[i]);
        }
        *out = acc;
    }
    if tag(&seed) != first.tag {
        return Err(CryptoError::SharesDoNotMatch);
    }
    Ok(seed)
}

/// Multiplication in GF(2^8) with the AES polynomial. Bitwise rather than
/// table lookups, so timing doesn't depend on the secret bytes.
fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        p ^= a & 0u8.wrapping_sub(b & 1);
        let carry = a >> 7;
        a = (a << 1) ^ (0x1b & 0u8.wrapping_sub(carry));
        b >>= 1;
    }
    p
}

/// a^254 is a's inverse, since the multiplicative group has order 255.
fn inv(a: u8) -> u8 {
    let mut result = 1u8;
    let mut base = a;
    let mut e = 254u8;
    while e > 0 {
        if e & 1 == 1 {
            result = mul(result, base);
        }
        base = mul(base, base);
        e >>= 1;
    }
    result
}

fn div(a: u8, b: u8) -> u8 {
    mul(a, inv(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
        30, 31, 32,
    ];

    #[test]
    fn field_inverse_is_an_inverse() {
        for a in 1..=255u8 {
            assert_eq!(mul(a, inv(a)), 1, "{a}");
        }
    }

    #[test]
    fn any_threshold_shares_rebuild_the_seed() {
        let shares = split(&SEED, 3, 5).unwrap();
        for a in 0..5 {
            for b in a + 1..5 {
                for c in b + 1..5 {
                    let pick = [shares[a].clone(), shares[b].clone(), shares[c].clone()];
                    assert_eq!(combine(&pick).unwrap(), SEED);
                }
            }
        }
    }

    #[test]
    fn too_few_shares_is_an_error_not_a_wrong_seed() {
        let shares = split(&SEED, 3, 5).unwrap();
        assert!(matches!(combine(&shares[..2]), Err(CryptoError::NotEnoughShares)));
        // The same share twice is still one share.
        assert!(matches!(combine(&[shares[0].clone(), shares[0].clone(), shares[1].clone()]), Err(CryptoError::NotEnoughShares)));
    }

    #[test]
    fn shares_from_two_splits_are_refused() {
        let a = split(&SEED, 2, 3).unwrap();
        let mut other = SEED;
        other[0] ^= 1;
        let b = split(&other, 2, 3).unwrap();
        assert!(matches!(combine(&[a[0].clone(), b[1].clone()]), Err(CryptoError::SharesDoNotMatch)));
    }

    #[test]
    fn a_share_survives_being_written_down() {
        let shares = split(&SEED, 2, 3).unwrap();
        for s in &shares {
            let text = s.to_text();
            assert_eq!(text.split_whitespace().count(), 25);
            let shouted = text.to_uppercase().replace(' ', "   ");
            assert_eq!(&Share::parse(&shouted).unwrap(), s);
        }
    }

    #[test]
    fn a_mistyped_share_is_caught() {
        let text = split(&SEED, 2, 3).unwrap()[0].to_text();
        let mut words: Vec<&str> = text.split_whitespace().collect();
        words[5] = if words[5] == "zoo" { "abandon" } else { "zoo" };
        assert!(matches!(Share::parse(&words.join(" ")), Err(CryptoError::InvalidShare)));
        assert!(matches!(Share::parse("1-1-0000 abandon"), Err(CryptoError::InvalidShare)));
    }

    #[test]
    fn one_share_reveals_nothing_obvious() {
        let shares = split(&SEED, 2, 2).unwrap();
        assert_ne!(shares[0].value, SEED);
        assert_ne!(shares[1].value, SEED);
    }

    #[test]
    fn bad_parameters_are_refused() {
        assert!(split(&SEED, 1, 3).is_err());
        assert!(split(&SEED, 4, 3).is_err());
    }
}
