//! Sealing an account backup: a passphrase-protected file, or a one-time
//! key for moving an account between two devices.
//!
//! The recovery phrase ([`crate::mnemonic`]) brings back *who you are*. It
//! does not bring back your contacts, your profile or your settings, and
//! with no server there is nowhere else they live. An account backup is the
//! answer to that: everything a device needs to *be* your account, in one
//! sealed blob, which the user keeps (a file) or hands straight to their
//! new device (a move).
//!
//! Two ways to seal, for two situations:
//!
//! - **A passphrase** ([`seal_with_passphrase`]), for a file that sits in a
//!   Downloads folder, a USB stick or a cloud drive for months. The key is
//!   stretched from the passphrase with Argon2id, deliberately slow and
//!   memory-hungry, because a file that leaves the device can be attacked
//!   offline for as long as the attacker likes. The cost parameters travel
//!   in the header, so they can be raised later without breaking old files.
//! - **A random 32-byte key** ([`seal_with_key`]), for a move between two
//!   devices standing next to each other. The key is in the link or QR code
//!   the old device shows, never typed, so there is nothing to stretch.
//!
//! Both are ChaCha20-Poly1305 with the whole header as associated data, so
//! a header edited to lower the cost or swap the salt fails to open rather
//! than opening weaker.
//!
//! What this blob is protecting is serious: it holds the identity seed and
//! the key to the user's message history. Holding the file and its
//! passphrase is the same as holding the recovery phrase, and the interface
//! must say so.

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::error::CryptoError;

/// The first bytes of a passphrase-sealed account backup file.
const FILE_MAGIC: &[u8; 9] = b"STOOPACCT";
/// The first bytes of a key-sealed account on its way between devices.
const MOVE_MAGIC: &[u8; 9] = b"STOOPMOVE";
/// The header layout this module writes. Bumped if the layout changes.
const VERSION: u8 = 1;
/// Salt length for Argon2id: 16 bytes is the recommended minimum.
const SALT_LEN: usize = 16;
/// ChaCha20-Poly1305 nonce length.
const NONCE_LEN: usize = 12;
/// Magic, version, three `u32` cost parameters, salt and nonce.
const FILE_HEADER_LEN: usize = FILE_MAGIC.len() + 1 + 12 + SALT_LEN + NONCE_LEN;
/// Magic, version and nonce: a move has no cost parameters or salt.
const MOVE_HEADER_LEN: usize = MOVE_MAGIC.len() + 1 + NONCE_LEN;
/// HKDF label turning a move link's key into the AEAD key, so the same 32
/// bytes are never used directly for two jobs (the hello proof is the other).
const MOVE_INFO: &[u8] = b"stoop/account-move/aead/v1";

/// The shortest passphrase [`seal_with_passphrase`] accepts. Argon2id
/// makes each guess expensive; it cannot make a four-letter word safe.
pub const MIN_PASSPHRASE_CHARS: usize = 8;

/// How hard Argon2id works to turn a passphrase into a key.
///
/// Written into every file's header, so a file always opens with the cost
/// it was made with, and the defaults can rise over the years without
/// stranding older backups.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfCost {
    /// Memory, in KiB.
    pub memory_kib: u32,
    /// Passes over that memory.
    pub iterations: u32,
    /// Lanes. Kept at 1: phones are the slowest devices that must open
    /// these files, and extra lanes buy them nothing.
    pub lanes: u32,
}

impl KdfCost {
    /// The cost new backups are made with: 64 MiB and three passes, around
    /// half a second on a phone. OWASP's second recommended Argon2id
    /// configuration is 46 MiB and one pass; this is comfortably above it.
    pub const DEFAULT: KdfCost = KdfCost { memory_kib: 64 * 1024, iterations: 3, lanes: 1 };

    /// The most a file may ask for when it is opened. The cost comes from
    /// the file, so without a ceiling a crafted file could ask for
    /// gigabytes of memory and take the app down instead of failing.
    const CEILING: KdfCost = KdfCost { memory_kib: 1024 * 1024, iterations: 16, lanes: 4 };

    /// Whether this cost is one a file may legitimately ask for.
    fn is_sane(&self) -> bool {
        self.memory_kib >= 8 * self.lanes.max(1)
            && self.memory_kib <= Self::CEILING.memory_kib
            && (1..=Self::CEILING.iterations).contains(&self.iterations)
            && (1..=Self::CEILING.lanes).contains(&self.lanes)
    }
}

/// Seals `plaintext` under a passphrase at the default cost.
///
/// Fails with [`CryptoError::PassphraseTooShort`] for a passphrase under
/// [`MIN_PASSPHRASE_CHARS`] characters.
pub fn seal_with_passphrase(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>, CryptoError> {
    seal_with_passphrase_at(plaintext, passphrase, KdfCost::DEFAULT)
}

/// Seals `plaintext` under a passphrase at an explicit cost. Tests use a
/// cheap one; everything else should use [`seal_with_passphrase`].
pub fn seal_with_passphrase_at(plaintext: &[u8], passphrase: &str, cost: KdfCost) -> Result<Vec<u8>, CryptoError> {
    if passphrase.chars().count() < MIN_PASSPHRASE_CHARS {
        return Err(CryptoError::PassphraseTooShort { min: MIN_PASSPHRASE_CHARS });
    }
    if !cost.is_sane() {
        return Err(CryptoError::NotAnAccountBackup);
    }
    let mut salt = [0u8; SALT_LEN];
    getrandom::fill(&mut salt).expect("OS randomness is unavailable");
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");

    let mut header = Vec::with_capacity(FILE_HEADER_LEN);
    header.extend_from_slice(FILE_MAGIC);
    header.push(VERSION);
    header.extend_from_slice(&cost.memory_kib.to_be_bytes());
    header.extend_from_slice(&cost.iterations.to_be_bytes());
    header.extend_from_slice(&cost.lanes.to_be_bytes());
    header.extend_from_slice(&salt);
    header.extend_from_slice(&nonce);

    let key = stretch(passphrase, &salt, cost)?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad: &header })
        .map_err(|_| CryptoError::DecryptionFailed)?;
    header.extend_from_slice(&ciphertext);
    Ok(header)
}

/// Opens a file made by [`seal_with_passphrase`].
///
/// A file that is not an account backup at all is
/// [`CryptoError::NotAnAccountBackup`]; one that is, but does not open, is
/// [`CryptoError::WrongPassphrase`] -- the passphrase is by far the likelier
/// cause than a damaged file, and the message should send the user there.
pub fn open_with_passphrase(sealed: &[u8], passphrase: &str) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if !is_passphrase_sealed(sealed) || sealed.len() < FILE_HEADER_LEN {
        return Err(CryptoError::NotAnAccountBackup);
    }
    let (header, ciphertext) = sealed.split_at(FILE_HEADER_LEN);
    if header[FILE_MAGIC.len()] != VERSION {
        return Err(CryptoError::NotAnAccountBackup);
    }
    // Fixed offsets after the magic and version byte.
    let at = FILE_MAGIC.len() + 1;
    let word = |i: usize| u32::from_be_bytes(header[at + i * 4..at + i * 4 + 4].try_into().expect("four bytes"));
    let cost = KdfCost { memory_kib: word(0), iterations: word(1), lanes: word(2) };
    if !cost.is_sane() {
        return Err(CryptoError::NotAnAccountBackup);
    }
    let salt = &header[at + 12..at + 12 + SALT_LEN];
    let nonce: [u8; NONCE_LEN] = header[at + 12 + SALT_LEN..].try_into().expect("nonce length");

    let key = stretch(passphrase, salt, cost)?;
    let cipher = ChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .decrypt(&Nonce::from(nonce), Payload { msg: ciphertext, aad: header })
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::WrongPassphrase)
}

/// Whether `bytes` starts like a passphrase-sealed account backup, so a
/// picker can say "that is not a Stoop backup" before asking for a
/// passphrase.
pub fn is_passphrase_sealed(bytes: &[u8]) -> bool {
    bytes.starts_with(FILE_MAGIC)
}

/// Seals `plaintext` under a move link's random key.
pub fn seal_with_key(plaintext: &[u8], key: &[u8; 32]) -> Result<Vec<u8>, CryptoError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");
    let mut header = Vec::with_capacity(MOVE_HEADER_LEN);
    header.extend_from_slice(MOVE_MAGIC);
    header.push(VERSION);
    header.extend_from_slice(&nonce);

    let cipher = ChaCha20Poly1305::new(&Key::from(*move_key(key)));
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: plaintext, aad: &header })
        .map_err(|_| CryptoError::DecryptionFailed)?;
    header.extend_from_slice(&ciphertext);
    Ok(header)
}

/// Opens what [`seal_with_key`] sealed.
pub fn open_with_key(sealed: &[u8], key: &[u8; 32]) -> Result<Zeroizing<Vec<u8>>, CryptoError> {
    if sealed.len() < MOVE_HEADER_LEN || !sealed.starts_with(MOVE_MAGIC) || sealed[MOVE_MAGIC.len()] != VERSION {
        return Err(CryptoError::NotAnAccountBackup);
    }
    let (header, ciphertext) = sealed.split_at(MOVE_HEADER_LEN);
    let nonce: [u8; NONCE_LEN] = header[MOVE_MAGIC.len() + 1..].try_into().expect("nonce length");
    let cipher = ChaCha20Poly1305::new(&Key::from(*move_key(key)));
    cipher
        .decrypt(&Nonce::from(nonce), Payload { msg: ciphertext, aad: header })
        .map(Zeroizing::new)
        .map_err(|_| CryptoError::DecryptionFailed)
}

/// Stretches a passphrase into a 32-byte key with Argon2id.
fn stretch(passphrase: &str, salt: &[u8], cost: KdfCost) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let params = Params::new(cost.memory_kib, cost.iterations, cost.lanes, Some(32)).map_err(|_| CryptoError::NotAnAccountBackup)?;
    let mut key = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, key.as_mut())
        .map_err(|_| CryptoError::NotAnAccountBackup)?;
    Ok(key)
}

/// The AEAD key a move link's key seals with.
fn move_key(key: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0u8; 32]);
    Hkdf::<Sha256>::new(None, key).expand(MOVE_INFO, out.as_mut()).expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap enough for a debug-build test, still a real Argon2id run.
    const CHEAP: KdfCost = KdfCost { memory_kib: 64, iterations: 1, lanes: 1 };

    #[test]
    fn passphrase_round_trip() {
        let sealed = seal_with_passphrase_at(b"my whole account", "correct horse", CHEAP).unwrap();
        assert!(is_passphrase_sealed(&sealed));
        assert_eq!(&**open_with_passphrase(&sealed, "correct horse").unwrap(), b"my whole account");
    }

    #[test]
    fn wrong_passphrase_is_named_as_such() {
        let sealed = seal_with_passphrase_at(b"x", "correct horse", CHEAP).unwrap();
        assert!(matches!(open_with_passphrase(&sealed, "wrong horse!"), Err(CryptoError::WrongPassphrase)));
    }

    #[test]
    fn short_passphrase_is_refused() {
        assert!(matches!(seal_with_passphrase_at(b"x", "short", CHEAP), Err(CryptoError::PassphraseTooShort { .. })));
    }

    #[test]
    fn a_lowered_cost_in_the_header_does_not_open() {
        let mut sealed = seal_with_passphrase_at(b"x", "correct horse", KdfCost { memory_kib: 128, ..CHEAP }).unwrap();
        // Halve the memory field: the header is authenticated, so this must fail.
        let at = FILE_MAGIC.len() + 1;
        sealed[at..at + 4].copy_from_slice(&64u32.to_be_bytes());
        assert!(open_with_passphrase(&sealed, "correct horse").is_err());
    }

    #[test]
    fn an_absurd_cost_is_refused_before_any_work() {
        let mut sealed = seal_with_passphrase_at(b"x", "correct horse", CHEAP).unwrap();
        let at = FILE_MAGIC.len() + 1;
        sealed[at..at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(open_with_passphrase(&sealed, "correct horse"), Err(CryptoError::NotAnAccountBackup)));
    }

    #[test]
    fn other_files_are_not_backups() {
        assert!(matches!(open_with_passphrase(b"{\"json\": true}", "correct horse"), Err(CryptoError::NotAnAccountBackup)));
    }

    #[test]
    fn key_round_trip_and_wrong_key() {
        let key = [7u8; 32];
        let sealed = seal_with_key(b"moving", &key).unwrap();
        assert_eq!(&**open_with_key(&sealed, &key).unwrap(), b"moving");
        assert!(open_with_key(&sealed, &[8u8; 32]).is_err());
    }
}
