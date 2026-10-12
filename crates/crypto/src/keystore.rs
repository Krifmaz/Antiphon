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
//! Elsewhere (Linux, macOS) [`available`] returns false and the caller
//! falls back to a plain file, exactly as before. A fallback that silently
//! pretended to protect the key would be worse than one that says it did
//! not: Keychain and libsecret are real work, and claiming them without
//! doing them is the failure mode this module is here to avoid.

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
    cfg!(windows)
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

    let mut input = blob(plaintext);
    let mut entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

    // SAFETY: all three blobs point at live buffers for the duration of
    // the call, and `output` is written by the OS and freed below.
    let ok = unsafe {
        CryptProtectData(
            &mut input,
            std::ptr::null(),
            &mut entropy,
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

    let mut input = blob(sealed);
    let mut entropy = blob(ENTROPY);
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };

    // SAFETY: as above.
    let ok = unsafe {
        CryptUnprotectData(
            &mut input,
            std::ptr::null_mut(),
            &mut entropy,
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
fn platform_wrap(_plaintext: &[u8]) -> Option<Vec<u8>> {
    None
}

#[cfg(not(windows))]
fn platform_unwrap(_sealed: &[u8]) -> Option<Vec<u8>> {
    None
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

    #[cfg(windows)]
    #[test]
    fn a_seed_survives_a_round_trip_through_the_keystore() {
        let seed = [7u8; 32];
        let sealed = wrap(&seed).expect("DPAPI is available on Windows");
        assert!(is_wrapped(&sealed));
        assert_eq!(unwrap(&sealed).as_deref(), Some(&seed[..]));
    }

    #[cfg(windows)]
    #[test]
    fn the_sealed_blob_does_not_contain_the_seed() {
        // The whole point. A blob that still had the bytes in it would be
        // a plaintext file with a header.
        let seed = [0xAB; 32];
        let sealed = wrap(&seed).expect("DPAPI is available on Windows");
        assert!(
            !sealed.windows(seed.len()).any(|w| w == seed),
            "the seed is sitting in the sealed blob"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_damaged_blob_is_refused_rather_than_half_read() {
        let mut sealed = wrap(&[1u8; 32]).expect("DPAPI is available on Windows");
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
    fn a_platform_without_a_keystore_says_so_rather_than_pretending() {
        // A fallback that silently claimed to protect the key would be
        // worse than one that admits it did not.
        assert!(!available());
        assert!(wrap(&[0u8; 32]).is_none());
    }
}
