//! Loading and persisting the long-term identity.
//!
//! The identity is the one piece of state whose loss actually costs
//! something: config can be rebuilt, presets re-tuned, contacts re-added,
//! but a lost identity key means becoming a different person to everyone
//! who has already verified you. Spec §12 open question 1 ("what's the
//! recovery story when someone loses every device?") is exactly this
//! problem, and it is not answered here.
//!
//! **Storage, and what is still missing.** Spec §6 requires private keys
//! in the OS keystore, never in plaintext. Where a keystore is available
//! ([`crate::keystore`] -- today, DPAPI on Windows) the seed is sealed
//! under the logged-in user and the file is no longer readable bytes. An
//! existing plaintext file is migrated in place the first time it is read,
//! so nobody has to do anything. Elsewhere -- Linux, macOS -- there is
//! still no keystore integration and the seed is still written plain;
//! Keychain and libsecret are separate work, and pretending otherwise
//! would be worse than saying so.
//!
//! Sealing is user-scoped, which is its real limit: it stops another
//! account, or the file copied elsewhere, but not code running as the same
//! user. No user-scoped keystore changes that.
//!
//! The file stays separate from `config.json` regardless, so settings
//! export -- which serializes `config.json` -- cannot sweep the key up by
//! accident.

use std::path::{Path, PathBuf};

use crate::identity::IdentityKeyPair;

/// The master seed, raw. One seed, because the whole identity derives
/// from it -- see [`IdentityKeyPair::from_master_seed`].
const SEED_FILE_LEN: usize = 32;

/// The original format: `[identity_seed][prekey_seed]`, two independent
/// seeds, from before recovery phrases existed.
const LEGACY_SEED_FILE_LEN: usize = 64;

/// Where the identity lives, given the directory holding app config.
pub fn identity_path(config_dir: &Path) -> PathBuf {
    config_dir.join("identity.key")
}

/// Reads the identity at `path`, or generates and persists a new one.
///
/// A corrupt or truncated file is an error rather than a silent
/// regeneration: quietly minting a new identity would change who the user
/// is to all of their contacts and trip every key-change warning they
/// have, which is far worse than refusing to start.
pub fn load_or_create(path: &Path) -> std::io::Result<IdentityKeyPair> {
    match std::fs::read(path) {
        Ok(bytes) => {
            // Sealed by this machine's keystore. Recognised by looking
            // rather than by trying to unseal, because a failed unseal
            // cannot tell "not sealed" from "sealed and unreadable here"
            // -- and those need opposite handling.
            if crate::keystore::is_wrapped(&bytes) {
                let Some(plain) = crate::keystore::unwrap(&bytes) else {
                    // Refused, never regenerated. A fresh identity would
                    // change who the user is to every contact they have,
                    // and this file is recoverable from the recovery
                    // phrase -- which a silent replacement would waste.
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} was sealed by the operating system and cannot be unsealed                              here -- this usually means a different Windows account or a                              different machine. Restore from your recovery phrase rather                              than deleting it, or the identity is gone.",
                            path.display()
                        ),
                    ));
                };
                if plain.len() != SEED_FILE_LEN {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{} unsealed to {} bytes, expected {SEED_FILE_LEN}",
                            path.display(), plain.len()),
                    ));
                }
                let mut master_seed = [0u8; 32];
                master_seed.copy_from_slice(&plain);
                return Ok(IdentityKeyPair::from_master_seed(master_seed));
            }
            if bytes.len() == LEGACY_SEED_FILE_LEN {
                // The old format's first 32 bytes were the Ed25519
                // signing seed, not a master seed. Feeding them to
                // `from_master_seed` would derive a *different* identity
                // key -- silently changing who the user is and tripping a
                // key-change warning for every contact they have.
                //
                // So this is refused rather than guessed at. The only
                // identities in this format were throwaway test ones.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} uses the old two-seed format, which predates recovery phrases. \
                         Converting it would change your identity key and trip a key-change \
                         warning for every contact, so it is not done automatically. Delete \
                         the file to start fresh with a recoverable identity.",
                        path.display()
                    ),
                ));
            }
            if bytes.len() != SEED_FILE_LEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} is {} bytes, expected {SEED_FILE_LEN} -- refusing to replace it, \
                         since generating a fresh identity would change who you are to every \
                         contact you have",
                        path.display(),
                        bytes.len()
                    ),
                ));
            }
            let mut master_seed = [0u8; 32];
            master_seed.copy_from_slice(&bytes);
            let identity = IdentityKeyPair::from_master_seed(master_seed);
            // A plaintext file on a machine that can seal one. Upgraded
            // here rather than only for new identities, so an existing
            // install stops leaving the seed readable without the user
            // having to do anything. A failed write is not fatal: the
            // identity is already loaded and correct, and refusing to
            // start over a storage upgrade would be worse than the
            // plaintext file that was already there.
            if crate::keystore::available() {
                if let Err(e) = save(path, &identity) {
                    eprintln!("[identity] could not move the key into the OS keystore: {e}");
                }
            }
            Ok(identity)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let identity = IdentityKeyPair::generate();
            save(path, &identity)?;
            Ok(identity)
        }
        Err(e) => Err(e),
    }
}

/// Writes the identity's secret seeds to `path`, creating parent
/// directories as needed.
/// Sealed by the OS keystore where one exists, and written plain where
/// none does -- see the module docs for why the fallback is not disguised.
pub fn save(path: &Path, identity: &IdentityKeyPair) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let seed = identity.master_seed();
    match crate::keystore::wrap(&seed) {
        Some(sealed) => std::fs::write(path, sealed)?,
        None => std::fs::write(path, seed)?,
    }
    restrict_permissions(path);
    Ok(())
}

/// Best-effort tightening of file permissions to owner-only.
///
/// Unix only -- and Unix is exactly where there is no keystore yet, so on
/// those platforms this is currently the only thing protecting the file.
/// On Windows the file inherits the user profile's ACL and the sealing in
/// [`save`] is what actually protects it.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("voip-identity-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_an_identity_when_none_exists_then_reloads_the_same_one() {
        let dir = temp_dir("create");
        let path = identity_path(&dir);

        let created = load_or_create(&path).unwrap();
        let reloaded = load_or_create(&path).unwrap();

        // Stability across restarts is the entire contract here.
        assert_eq!(created.public(), reloaded.public());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_identity_file_is_an_error_rather_than_a_silent_new_identity() {
        let dir = temp_dir("corrupt");
        let path = identity_path(&dir);
        std::fs::write(&path, b"this is not a keypair").unwrap();

        // Regenerating here would silently change the user's identity and
        // trip every contact's key-change warning.
        assert!(load_or_create(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plaintext_identity_is_upgraded_in_place_and_still_loads_the_same() {
        // An existing install should stop leaving the seed readable
        // without anyone having to do anything -- and, critically,
        // without becoming a different person in the process.
        let dir = temp_dir("migrate");
        let path = identity_path(&dir);

        let identity = IdentityKeyPair::generate();
        std::fs::write(&path, identity.master_seed()).unwrap();

        let loaded = load_or_create(&path).unwrap();
        assert_eq!(loaded.public(), identity.public(), "migration changed the identity");

        let on_disk = std::fs::read(&path).unwrap();
        if crate::keystore::available() {
            assert!(crate::keystore::is_wrapped(&on_disk), "still plaintext after loading");
            assert!(
                !on_disk.windows(32).any(|w| w == identity.master_seed()),
                "the seed is still sitting in the file"
            );
        } else {
            // No keystore here, and the file is left exactly as it was
            // rather than rewritten into a format nothing can read back.
            assert_eq!(on_disk, identity.master_seed());
        }
        assert_eq!(load_or_create(&path).unwrap().public(), identity.public());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sealed_file_that_cannot_be_unsealed_is_refused_not_replaced() {
        // The worst thing this code could do: read a key it cannot
        // unseal, decide there is no identity, and mint a new one --
        // changing who the user is to every contact, and throwing away a
        // key their recovery phrase could still have restored.
        let dir = temp_dir("unsealable");
        let path = identity_path(&dir);

        let mut damaged = crate::keystore::MAGIC.to_vec();
        damaged.extend_from_slice(&[0xff; 64]);
        std::fs::write(&path, &damaged).unwrap();

        assert!(load_or_create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), damaged, "the file was overwritten");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_identity_is_never_written_as_readable_bytes_where_it_need_not_be() {
        let dir = temp_dir("sealed-new");
        let path = identity_path(&dir);
        let identity = load_or_create(&path).unwrap();

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(crate::keystore::is_wrapped(&on_disk), crate::keystore::available());
        if crate::keystore::available() {
            assert!(!on_disk.windows(32).any(|w| w == identity.master_seed()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn saving_and_loading_round_trips_an_existing_identity() {
        let dir = temp_dir("roundtrip");
        let path = identity_path(&dir);

        let identity = IdentityKeyPair::generate();
        save(&path, &identity).unwrap();

        assert_eq!(load_or_create(&path).unwrap().public(), identity.public());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
