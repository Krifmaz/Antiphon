//! OS-backed protection for the identity seed (spec §6: "private keys in
//! the OS keystore/keychain, never in plaintext").
//!
//! What this does and does not buy:
//!
//! - It **does** stop the seed sitting on disk as 32 readable bytes. On
//!   Windows the blob is sealed with DPAPI under the logged-in user, so
//!   another account on the same machine -- or the file copied to another
//!   machine -- cannot unseal it.
//! - It does **not** protect against code running as that same user. DPAPI
//!   will happily unseal for anything the user runs. That is the honest
//!   limit of user-scoped protection and no keystore changes it.
//!
//! Extra entropy is mixed in as a second factor so the blob is not
//! unsealable by an unrelated program that merely happens to run as the
//! same user and finds the file: it has to know this constant too. That is
//! a speed bump, not a boundary, and is described as one.
//!
//! On macOS and Linux the OS keystore -- Keychain, or the Secret Service
//! (GNOME Keyring, KWallet) -- holds a random 32-byte storage key, and the
//! blob is sealed with ChaCha20-Poly1305 under it. The keystores are built
//! for small secrets, so the key lives there and the sealed file stays a
//! file; the effect is the same as DPAPI: the bytes on disk are useless
//! without the logged-in user's keyring.
//!
//! Where there is no keystore -- a headless Linux box with no Secret
//! Service running -- [`available`] returns false and the caller falls
//! back to a plain file, and says so. A fallback that silently pretended
//! to protect the key would be worse than one that admits it did not.

/// Marks a file whose contents this module sealed.
///
/// Present so an unwrapped legacy file and a sealed one can be told apart
/// by looking, rather than by trying to unseal and interpreting failure --
/// which cannot distinguish "not sealed" from "sealed and damaged", and
/// those two need very different handling.
///
/// Frozen under the project's old name, along with [`ENTROPY`] below --
/// see "Frozen labels" in `lib.rs`. Changing either would make every key
/// already sealed on disk unreadable.
pub const MAGIC: &[u8] = b"VOIPKEY1";

/// Second-factor entropy, mixed into the seal.
///
/// Constant rather than random because it has to be reproducible without
/// somewhere to store it -- if it were stored next to the blob it would be
/// no factor at all.
#[cfg(windows)]
const ENTROPY: &[u8] = b"voip-app identity seed v1";

/// Whether this platform can seal a key.
pub fn available() -> bool {
    #[cfg(windows)]
    return true;
    #[cfg(not(windows))]
    return keyring_key::get().is_some();
}

/// Seals `plaintext`, returning a blob that starts with [`MAGIC`].
///
/// `None` when this platform has no keystore, or when the OS refused --
/// callers treat both as "store it the old way", never as "store nothing".
pub fn wrap(plaintext: &[u8]) -> Option<Vec<u8>> {
    let sealed = platform_wrap(plaintext)?;
    let mut out = Vec::with_capacity(MAGIC.len() + sealed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&sealed);
    Some(out)
}

/// Whether `bytes` is something this module sealed.
pub fn is_wrapped(bytes: &[u8]) -> bool {
    bytes.starts_with(MAGIC)
}

/// Unseals a blob produced by [`wrap`].
///
/// `None` means the blob is genuinely unreadable here -- wrong user, wrong
/// machine, or damaged. The caller must **not** treat that as "no identity
/// yet": generating a fresh one would change who the user is to every
/// contact they have.
pub fn unwrap(bytes: &[u8]) -> Option<Vec<u8>> {
    let sealed = bytes.strip_prefix(MAGIC)?;
    platform_unwrap(sealed)
}

#[cfg(windows)]
fn platform_wrap(plaintext: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB,
    };

    let input = blob(plaintext);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

    // SAFETY: all three blobs point at live buffers for the duration of
    // the call, and `output` is written by the OS and freed below.
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    take_blob(ok != 0, output)
}

#[cfg(windows)]
fn platform_unwrap(sealed: &[u8]) -> Option<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    let input = blob(sealed);
    let entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

    // SAFETY: as above.
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            0,
            &mut output,
        )
    };
    take_blob(ok != 0, output)
}

#[cfg(windows)]
fn blob(bytes: &[u8]) -> windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
    windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    }
}

/// Copies an OS-allocated blob out and frees it.
///
/// Freeing on the failure path too: `CryptProtectData` can return false
/// with nothing allocated, and calling `LocalFree(NULL)` is defined to do
/// nothing, so one path covers both without leaking on the other.
#[cfg(windows)]
fn take_blob(
    ok: bool,
    output: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
) -> Option<Vec<u8>> {
    let result = if ok && !output.pbData.is_null() {
        // SAFETY: the OS wrote `cbData` bytes at `pbData`.
        Some(unsafe {
            std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
        })
    } else {
        None
    };
    if !output.pbData.is_null() {
        // SAFETY: `pbData` came from the OS via LocalAlloc.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(output.pbData as *mut core::ffi::c_void);
        }
    }
    result
}

#[cfg(not(windows))]
fn platform_wrap(plaintext: &[u8]) -> Option<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    let key = keyring_key::get()?;
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).ok()?;
    let cipher = ChaCha20Poly1305::new(&Key::from(**key));
    let sealed = cipher.encrypt(&Nonce::from(nonce), plaintext).ok()?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Some(out)
}

#[cfg(not(windows))]
fn platform_unwrap(sealed: &[u8]) -> Option<Vec<u8>> {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

    if sealed.len() < 12 {
        return None;
    }
    let key = keyring_key::get()?;
    let (nonce, body) = sealed.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().ok()?;
    let cipher = ChaCha20Poly1305::new(&Key::from(**key));
    cipher.decrypt(&Nonce::from(nonce), body).ok()
}

/// The storage key kept in the OS keystore on macOS and Linux.
#[cfg(not(windows))]
mod keyring_key {
    use std::sync::OnceLock;
    use zeroize::Zeroizing;

    /// Frozen: renaming either would orphan every sealed file.
    const SERVICE: &str = "antiphon";
    const ACCOUNT: &str = "storage-key";

    /// Looked up once per run. Asking the keystore on every read would
    /// mean a D-Bus round trip, and on some desktops an unlock prompt,
    /// each time.
    static KEY: OnceLock<Option<Zeroizing<[u8; 32]>>> = OnceLock::new();

    pub fn get() -> Option<&'static Zeroizing<[u8; 32]>> {
        KEY.get_or_init(load_or_create).as_ref()
    }

    fn load_or_create() -> Option<Zeroizing<[u8; 32]>> {
        if std::env::var_os("ANTIPHON_NO_KEYSTORE").is_some() {
            return None;
        }
        #[cfg(not(target_os = "ios"))]
        let entry = keyring::Entry::new(SERVICE, ACCOUNT).ok()?;
        // keyring's front end has no iOS store, so install the app's own
        // keychain (the "protected data" store) and use keyring-core's
        // entry, which has the same calls and the same errors.
        #[cfg(target_os = "ios")]
        let entry = {
            keyring_core::set_default_store(apple_native_keyring_store::protected::Store::new().ok()?);
            keyring_core::Entry::new(SERVICE, ACCOUNT).ok()?
        };
        match entry.get_secret() {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                let key: [u8; 32] = bytes.as_slice().try_into().ok()?;
                Some(Zeroizing::new(key))
            }
            // Only a definite "not there" creates one. Any other error --
            // the keyring is locked, the service went away -- must not
            // mint a new key, or every file sealed under the old one would
            // become unreadable the next time it worked.
            Err(keyring::Error::NoEntry) => {
                let mut key = Zeroizing::new([0u8; 32]);
                getrandom::fill(&mut key[..]).ok()?;
                entry.set_secret(&key[..]).ok()?;
                // Read it back: a store that accepts writes and forgets
                // them would otherwise seal files nobody can open.
                let check = Zeroizing::new(entry.get_secret().ok()?);
                (check.as_slice() == &key[..]).then_some(key)
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_blob_is_recognisable_without_unsealing_it() {
        // Telling "not sealed" from "sealed and damaged" by looking is the
        // point: those two need very different handling, and a failed
        // unseal cannot distinguish them.
        assert!(!is_wrapped(&[0u8; 32]));
        assert!(is_wrapped(MAGIC));
    }

    #[test]
    fn unsealing_something_that_was_never_sealed_fails_cleanly() {
        assert!(unwrap(&[0u8; 32]).is_none());
        assert!(unwrap(&[]).is_none());
        assert!(unwrap(b"VOIPKEY").is_none(), "a truncated magic must not match");
    }

    #[test]
    fn a_seed_survives_a_round_trip_through_the_keystore() {
        if !available() {
            return;
        }
        let seed = [7u8; 32];
        let sealed = wrap(&seed).expect("keystore is available");
        assert!(is_wrapped(&sealed));
        assert_eq!(unwrap(&sealed).as_deref(), Some(&seed[..]));
    }

    #[test]
    fn the_sealed_blob_does_not_contain_the_seed() {
        if !available() {
            return;
        }
        // The whole point. A blob that still had the bytes in it would be
        // a plaintext file with a header.
        let seed = [0xAB; 32];
        let sealed = wrap(&seed).expect("keystore is available");
        assert!(
            !sealed.windows(seed.len()).any(|w| w == seed),
            "the seed is sitting in the sealed blob"
        );
    }

    #[test]
    fn a_damaged_blob_is_refused_rather_than_half_read() {
        if !available() {
            return;
        }
        let mut sealed = wrap(&[1u8; 32]).expect("keystore is available");
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(unwrap(&sealed).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn sealing_is_reported_as_available() {
        assert!(available());
    }

    #[cfg(not(windows))]
    #[test]
    fn without_a_keystore_it_says_so_rather_than_pretending() {
        // A fallback that silently claimed to protect the key would be
        // worse than one that admits it did not.
        assert_eq!(available(), wrap(&[0u8; 32]).is_some());
    }
}
