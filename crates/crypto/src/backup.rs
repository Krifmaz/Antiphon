//! Encrypted backup, sealed to devices (spec §3 "Backup & recovery").
//!
//! This is the module that makes "signing in isn't enough -- you need the
//! original device or another verified one" true rather than a policy
//! promise.
//!
//! The arrangement:
//!
//! - History is encrypted under a random **backup key**, not under
//!   anything derived from the recovery phrase.
//! - That backup key is **sealed separately to each authorized device**,
//!   using the device's X25519 prekey.
//! - A new device holds a keypair nobody has sealed anything to. It can
//!   prove it is the identity (it has the phrase), it can be signed into
//!   the device list, and it still cannot read a byte of history until an
//!   existing device seals the backup key to it.
//!
//! So recovering the phrase gets you your *identity* -- your contacts see
//! no key change, your verifications hold. It does not get you your
//! *history*. Those are deliberately different keys, and conflating them
//! is the mistake this module exists to avoid: if the phrase decrypted
//! history, then anyone who ever saw the phrase could read everything,
//! forever, from anywhere.
//!
//! The sealing primitive is an ephemeral-static ECDH box: a fresh
//! ephemeral X25519 key per seal, DH against the recipient device's
//! prekey, HKDF to an AEAD key, ChaCha20-Poly1305 over the payload. The
//! ephemeral public key travels with the ciphertext. Forward secrecy is
//! deliberately *not* a property here -- a backup that only its own
//! recipient can open, but that stops being openable over time, is not a
//! backup.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::CryptoError;
use crate::identity::{IdentityKeyPair, PublicIdentity};

// Frozen under the project's old name -- see "Frozen labels" in `lib.rs`.
const SEAL_SALT: &[u8] = b"voip-app/backup/seal/v1";
const SEAL_INFO: &[u8] = b"voip-app/backup/seal-key/v1";
const BACKUP_SALT: &[u8] = b"voip-app/backup/content/v1";
const BACKUP_INFO: &[u8] = b"voip-app/backup/content-key/v1";

/// The symmetric key history is encrypted under.
///
/// Never derived from the recovery phrase -- see the module docs for why
/// that separation is the whole point.
#[derive(Clone, PartialEq, Eq)]
pub struct BackupKey([u8; 32]);

/// Redacted on purpose. `Debug` is derived on almost everything else here,
/// but this type holds the key to a user's entire history -- deriving it
/// would put that key into any log line, panic message, or error report
/// that happens to format a struct containing one.
impl std::fmt::Debug for BackupKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BackupKey(<redacted>)")
    }
}

impl BackupKey {
    pub fn generate() -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("OS randomness is unavailable");
        Self(key)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// Encrypts backup content under this key.
    pub fn seal_content(&self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let hk = Hkdf::<Sha256>::new(Some(BACKUP_SALT), &self.0);
        let mut aead_key = [0u8; 32];
        hk.expand(BACKUP_INFO, &mut aead_key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");

        let mut nonce_bytes = [0u8; 12];
        getrandom::fill(&mut nonce_bytes).expect("OS randomness is unavailable");

        let cipher = ChaCha20Poly1305::new(&Key::from(aead_key));
        let ciphertext = cipher
            .encrypt(
                &Nonce::from(nonce_bytes),
                Payload { msg: plaintext, aad: BACKUP_SALT },
            )
            .map_err(|_| CryptoError::DecryptionFailed)?;

        let mut out = Vec::with_capacity(12 + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypts backup content produced by [`Self::seal_content`].
    pub fn open_content(&self, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if sealed.len() < 12 {
            return Err(CryptoError::DecryptionFailed);
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(12);
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(nonce_bytes);

        let hk = Hkdf::<Sha256>::new(Some(BACKUP_SALT), &self.0);
        let mut aead_key = [0u8; 32];
        hk.expand(BACKUP_INFO, &mut aead_key)
            .expect("32 bytes is a valid HKDF-SHA256 output length");

        let cipher = ChaCha20Poly1305::new(&Key::from(aead_key));
        cipher
            .decrypt(
                &Nonce::from(nonce),
                Payload { msg: ciphertext, aad: BACKUP_SALT },
            )
            .map_err(|_| CryptoError::DecryptionFailed)
    }
}

/// A backup key encrypted to one specific device.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SealedBackupKey {
    /// Which device this was sealed for. Lets a device pick its own
    /// envelope out of a set without trial decryption.
    pub recipient_device_key: [u8; 32],
    /// Ephemeral X25519 public key from the sealing side.
    pub ephemeral: [u8; 32],
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

/// Seals a backup key to an authorized device.
///
/// Callers must check the device is in the current
/// [`crate::SignedDeviceList`] first -- this function will happily seal to
/// any well-formed key, and the authorization decision belongs with the
/// list, not here.
pub fn seal_backup_key(
    key: &BackupKey,
    recipient: &PublicIdentity,
) -> Result<SealedBackupKey, CryptoError> {
    // A device whose prekey isn't signed by its own identity key must not
    // receive anything: the prekey might not be theirs at all.
    recipient.verify()?;

    let mut ephemeral_seed = [0u8; 32];
    getrandom::fill(&mut ephemeral_seed).expect("OS randomness is unavailable");
    let ephemeral_secret = StaticSecret::from(ephemeral_seed);
    let ephemeral_public = PublicKey::from(&ephemeral_secret).to_bytes();

    let shared = ephemeral_secret.diffie_hellman(&PublicKey::from(recipient.prekey));

    let aead_key = seal_key(&shared.to_bytes(), &ephemeral_public, &recipient.prekey);

    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");

    let cipher = ChaCha20Poly1305::new(&Key::from(aead_key));
    let ciphertext = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: &key.0, aad: SEAL_SALT })
        .map_err(|_| CryptoError::DecryptionFailed)?;

    Ok(SealedBackupKey {
        recipient_device_key: recipient.identity_key,
        ephemeral: ephemeral_public,
        nonce,
        ciphertext,
    })
}

/// Opens a sealed backup key with the recipient device's own keypair.
pub fn open_backup_key(
    sealed: &SealedBackupKey,
    device: &IdentityKeyPair,
) -> Result<BackupKey, CryptoError> {
    if sealed.recipient_device_key != device.public().identity_key {
        return Err(CryptoError::BackupNotSealedForThisDevice);
    }

    let shared = device
        .prekey_secret()
        .diffie_hellman(&PublicKey::from(sealed.ephemeral));

    let aead_key = seal_key(&shared.to_bytes(), &sealed.ephemeral, &device.public().prekey);

    let cipher = ChaCha20Poly1305::new(&Key::from(aead_key));
    let plaintext = cipher
        .decrypt(
            &Nonce::from(sealed.nonce),
            Payload { msg: &sealed.ciphertext, aad: SEAL_SALT },
        )
        .map_err(|_| CryptoError::DecryptionFailed)?;

    if plaintext.len() != 32 {
        return Err(CryptoError::DecryptionFailed);
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&plaintext);
    Ok(BackupKey(key))
}

/// Both public keys are bound into the KDF so a shared secret can only
/// produce the AEAD key for the exact pair it came from.
fn seal_key(shared: &[u8; 32], ephemeral: &[u8; 32], recipient_prekey: &[u8; 32]) -> [u8; 32] {
    let mut ikm = Vec::with_capacity(96);
    ikm.extend_from_slice(shared);
    ikm.extend_from_slice(ephemeral);
    ikm.extend_from_slice(recipient_prekey);

    let hk = Hkdf::<Sha256>::new(Some(SEAL_SALT), &ikm);
    let mut out = [0u8; 32];
    hk.expand(SEAL_INFO, &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceKeyPair;
    use crate::mnemonic::{phrase_from_seed, seed_from_phrase};

    #[test]
    fn a_device_can_open_a_key_sealed_to_it() {
        let desktop = DeviceKeyPair::generate();
        let key = BackupKey::generate();

        let sealed = seal_backup_key(&key, &desktop.public()).unwrap();
        assert_eq!(open_backup_key(&sealed, &desktop).unwrap(), key);
    }

    #[test]
    fn another_device_cannot_open_it() {
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();
        let key = BackupKey::generate();

        let sealed = seal_backup_key(&key, &desktop.public()).unwrap();
        assert!(matches!(
            open_backup_key(&sealed, &phone),
            Err(CryptoError::BackupNotSealedForThisDevice)
        ));
    }

    #[test]
    fn backup_content_round_trips() {
        let key = BackupKey::generate();
        let sealed = key.seal_content(b"the message history").unwrap();

        assert_eq!(key.open_content(&sealed).unwrap(), b"the message history");
    }

    #[test]
    fn a_different_backup_key_cannot_read_the_content() {
        let key = BackupKey::generate();
        let other = BackupKey::generate();
        let sealed = key.seal_content(b"the message history").unwrap();

        assert!(other.open_content(&sealed).is_err());
    }

    #[test]
    fn tampered_backup_content_is_rejected_rather_than_returned_garbled() {
        let key = BackupKey::generate();
        let mut sealed = key.seal_content(b"the message history").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;

        assert!(key.open_content(&sealed).is_err());
    }

    #[test]
    fn the_same_key_sealed_twice_produces_different_ciphertext() {
        let desktop = DeviceKeyPair::generate();
        let key = BackupKey::generate();

        let first = seal_backup_key(&key, &desktop.public()).unwrap();
        let second = seal_backup_key(&key, &desktop.public()).unwrap();

        // Fresh ephemeral and nonce per seal, so envelopes aren't
        // fingerprintable as "same key, same recipient".
        assert_ne!(first.ephemeral, second.ephemeral);
        assert_ne!(first.ciphertext, second.ciphertext);
        // Both still open to the same key.
        assert_eq!(open_backup_key(&first, &desktop).unwrap(), key);
        assert_eq!(open_backup_key(&second, &desktop).unwrap(), key);
    }

    #[test]
    fn recovering_the_phrase_alone_does_not_open_history() {
        // The headline property. Alice has a desktop holding the backup
        // key, and history encrypted under it.
        let alice = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let backup_key = BackupKey::generate();
        let history = backup_key.seal_content(b"everything she ever said").unwrap();
        let sealed_for_desktop = seal_backup_key(&backup_key, &desktop.public()).unwrap();

        // She loses everything and restores the identity from her phrase
        // on a brand new laptop.
        let phrase = phrase_from_seed(&alice.master_seed());
        let restored = IdentityKeyPair::from_master_seed(seed_from_phrase(&phrase).unwrap());
        let new_laptop = DeviceKeyPair::generate();

        // She is unambiguously herself again -- same identity, contacts
        // see no key change.
        assert_eq!(restored.public(), alice.public());

        // And she still cannot read a byte of history, because nothing was
        // ever sealed to this laptop's key.
        assert!(open_backup_key(&sealed_for_desktop, &new_laptop).is_err());
        assert!(open_backup_key(&sealed_for_desktop, &restored).is_err());
        assert!(BackupKey::generate().open_content(&history).is_err());
    }

    #[test]
    fn an_existing_device_can_let_the_new_one_in() {
        // The other half: recovery is possible, it just requires a device
        // that already has the key.
        let desktop = DeviceKeyPair::generate();
        let new_laptop = DeviceKeyPair::generate();
        let backup_key = BackupKey::generate();
        let history = backup_key.seal_content(b"everything she ever said").unwrap();

        let for_desktop = seal_backup_key(&backup_key, &desktop.public()).unwrap();

        // Desktop opens its own envelope and re-seals to the laptop.
        let recovered = open_backup_key(&for_desktop, &desktop).unwrap();
        let for_laptop = seal_backup_key(&recovered, &new_laptop.public()).unwrap();

        let on_laptop = open_backup_key(&for_laptop, &new_laptop).unwrap();
        assert_eq!(on_laptop.open_content(&history).unwrap(), b"everything she ever said");
    }

    #[test]
    fn a_device_with_a_broken_prekey_signature_is_never_sealed_to() {
        let device = DeviceKeyPair::generate();
        let mut public = device.public();
        public.prekey_signature[0] ^= 0x01;

        assert!(seal_backup_key(&BackupKey::generate(), &public).is_err());
    }

    #[test]
    fn a_truncated_envelope_is_an_error_not_a_panic() {
        let key = BackupKey::generate();
        assert!(key.open_content(&[]).is_err());
        assert!(key.open_content(&[0u8; 5]).is_err());
    }
}
