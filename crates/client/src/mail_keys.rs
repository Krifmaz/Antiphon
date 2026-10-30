//! The account's mail keys, as this device keeps them (see
//! [`antiphon_crypto::mail_key`] for what a mail key is and why).
//!
//! `mail.keys` holds the current key and the ones it replaced, sealed under
//! the history key like every other account file, and travels with the
//! account in a backup or a move.
//!
//! - **Epoch 0** is the identity itself: nothing to store, and what every
//!   account starts on.
//! - **Rotating** (when a device is removed) makes a fresh random key the
//!   current one. The device that rotates hands it to the account's other
//!   devices, sealed to each one's own key (`crate::device_sync`).
//! - **Adopting** is the other side of that: a key from a sibling replaces
//!   the current one if it is newer. Two devices that rotate at once both
//!   end up on the same key: the higher epoch wins, and on a tie the larger
//!   secret.
//! - **Retired** keys are still collected and opened for
//!   [`RETIRED_FOR`], so mail a contact left before they heard of the new
//!   key isn't lost.
//!
//! Pure file handling over `antiphon_crypto`, so the phone shares it by path.

use std::path::{Path, PathBuf};

use antiphon_crypto::mail_key::{self, MailKeyCert};
use antiphon_crypto::{hex, mail, BackupKey, IdentityKeyPair, PublicIdentity};
use serde::{Deserialize, Serialize};

/// The file the keys are kept in.
pub const FILE: &str = "mail.keys";
/// How long a replaced key is still collected and opened, in seconds:
/// long enough for every contact to have picked up the new one.
pub const RETIRED_FOR: u64 = 30 * 24 * 60 * 60;

/// One mail key.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Entry {
    /// Which key: higher is newer.
    epoch: u64,
    /// The secret, hex; `None` for epoch 0, which is the identity itself.
    secret: Option<String>,
    /// When it was replaced, unix seconds; `None` while current.
    retired_secs: Option<u64>,
}

/// The account's mail keys: the current one first, then the retired ones,
/// newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct MailKeys {
    /// Never empty.
    entries: Vec<Entry>,
}

impl Default for MailKeys {
    /// An account that has never rotated: epoch 0, the identity itself.
    fn default() -> Self {
        Self { entries: vec![Entry { epoch: 0, secret: None, retired_secs: None }] }
    }
}

/// Where the keys are kept.
fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

impl MailKeys {
    /// The keys saved in `dir`, or epoch 0 when there are none (or they
    /// can't be opened: the identity's own keys still work then).
    pub fn load(dir: &Path, key: &BackupKey) -> Self {
        std::fs::read(path(dir))
            .ok()
            .and_then(|sealed| key.open_content(&sealed).ok())
            .and_then(|plain| serde_json::from_slice::<Vec<Entry>>(&plain).ok())
            .filter(|e| !e.is_empty())
            .map(|entries| Self { entries })
            .unwrap_or_default()
    }

    /// Writes the keys, sealed, through a temporary file.
    pub fn save(&self, dir: &Path, key: &BackupKey) -> std::io::Result<()> {
        let plain = serde_json::to_vec(&self.entries).expect("mail keys always serialize");
        let sealed = key.seal_content(&plain).map_err(|e| std::io::Error::other(e.to_string()))?;
        let p = path(dir);
        let tmp = p.with_extension("keys.tmp");
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, &p)
    }

    /// The current key's epoch.
    pub fn epoch(&self) -> u64 {
        self.entries[0].epoch
    }

    /// The current key's epoch and secret, to hand to a sibling device;
    /// `None` on epoch 0, which every device already holds.
    pub fn current_secret(&self) -> Option<(u64, [u8; 32])> {
        let e = &self.entries[0];
        Some((e.epoch, hex::decode_array(e.secret.as_deref()?)?))
    }

    /// The keypair mail is sealed to and inboxes are derived from now.
    pub fn current(&self, account: &IdentityKeyPair) -> IdentityKeyPair {
        holder_of(&self.entries[0], account)
    }

    /// Every key still worth collecting with and opening by, current first.
    pub fn holders(&self, account: &IdentityKeyPair, now_secs: u64) -> Vec<IdentityKeyPair> {
        self.entries
            .iter()
            .filter(|e| e.retired_secs.is_none_or(|at| now_secs.saturating_sub(at) < RETIRED_FOR))
            .map(|e| holder_of(e, account))
            .collect()
    }

    /// The certificate contacts seal to, for the route record; `None` on
    /// epoch 0, where contacts seal to the identity's own prekey.
    pub fn cert(&self, account: &IdentityKeyPair) -> Option<MailKeyCert> {
        let (epoch, secret) = self.current_secret()?;
        Some(MailKeyCert::issue(account, epoch, &mail_key::holder(secret)))
    }

    /// Opens mail with whichever key it was sealed to.
    pub fn open(&self, account: &IdentityKeyPair, sealed: &[u8], now_secs: u64) -> Option<(PublicIdentity, Vec<u8>)> {
        let me = account.public().identity_key;
        self.holders(account, now_secs).iter().find_map(|h| mail::open_for(&me, h, sealed).ok())
    }

    /// Makes a fresh random key the current one and returns it, retiring
    /// the one before. Done when a device is removed.
    pub fn rotate(&mut self, now_secs: u64) -> (u64, [u8; 32]) {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS randomness is unavailable");
        let epoch = self.epoch().saturating_add(1);
        self.replace(epoch, secret, now_secs);
        (epoch, secret)
    }

    /// Takes a key another device of the account sent, if it supersedes
    /// the current one. Returns whether it did.
    pub fn adopt(&mut self, epoch: u64, secret: [u8; 32], now_secs: u64) -> bool {
        let newer = match self.current_secret() {
            Some((have, ours)) => epoch > have || (epoch == have && secret > ours),
            None => epoch > self.epoch(),
        };
        if newer {
            self.replace(epoch, secret, now_secs);
        }
        newer
    }

    /// Retires the current key and puts `secret` in its place, dropping
    /// keys retired longer than [`RETIRED_FOR`] ago.
    fn replace(&mut self, epoch: u64, secret: [u8; 32], now_secs: u64) {
        self.entries[0].retired_secs = Some(now_secs);
        self.entries.retain(|e| e.retired_secs.is_none_or(|at| now_secs.saturating_sub(at) < RETIRED_FOR));
        self.entries.insert(0, Entry { epoch, secret: Some(hex::encode(&secret)), retired_secs: None });
    }
}

/// The keypair an entry stands for: its own, or the identity's on epoch 0.
fn holder_of(entry: &Entry, account: &IdentityKeyPair) -> IdentityKeyPair {
    match entry.secret.as_deref().and_then(hex::decode_array::<32>) {
        Some(secret) => mail_key::holder(secret),
        None => IdentityKeyPair::from_master_seed(account.master_seed()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty data folder.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stoop-mail-keys-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_new_account_uses_its_identity() {
        let account = IdentityKeyPair::generate();
        let keys = MailKeys::default();
        assert_eq!(keys.epoch(), 0);
        assert!(keys.cert(&account).is_none());
        assert_eq!(keys.current(&account).public(), account.public());
    }

    #[test]
    fn rotating_retires_the_old_key_and_still_opens_its_mail_for_a_while() {
        let account = IdentityKeyPair::generate();
        let sender = IdentityKeyPair::generate();
        let me = account.public().identity_key;
        let mut keys = MailKeys::default();
        let before = mail::seal(&sender, &me, &account.public().prekey, b"old").unwrap();
        keys.rotate(1_000);
        let cert = keys.cert(&account).unwrap();
        assert!(cert.verify(&me).is_ok());
        let after = mail::seal(&sender, &me, &cert.prekey, b"new").unwrap();

        assert_eq!(keys.open(&account, &before, 1_000).unwrap().1, b"old");
        assert_eq!(keys.open(&account, &after, 1_000).unwrap().1, b"new");
        // Past the window the old key is gone; the new one stays.
        let later = 1_000 + RETIRED_FOR;
        assert!(keys.open(&account, &before, later).is_none());
        assert_eq!(keys.open(&account, &after, later).unwrap().1, b"new");
    }

    #[test]
    fn devices_that_rotate_at_once_agree() {
        let mut a = MailKeys::default();
        let mut b = MailKeys::default();
        let (ea, sa) = a.rotate(5);
        let (eb, sb) = b.rotate(5);
        // Each hears the other's.
        let a_took = a.adopt(eb, sb, 6);
        let b_took = b.adopt(ea, sa, 6);
        assert!(a_took != b_took, "exactly one gives way");
        assert_eq!(a.current_secret(), b.current_secret());
        // Hearing an older key changes nothing.
        let mut c = a.clone();
        assert!(!c.adopt(0, [9; 32], 7));
        assert_eq!(c, a);
    }

    #[test]
    fn keys_survive_saving_and_need_the_history_key() {
        let dir = folder("save");
        let key = BackupKey::generate();
        let mut keys = MailKeys::default();
        keys.rotate(1);
        keys.save(&dir, &key).unwrap();
        assert_eq!(MailKeys::load(&dir, &key), keys);
        assert_eq!(MailKeys::load(&dir, &BackupKey::generate()), MailKeys::default());
        assert!(!std::fs::read(dir.join(FILE)).unwrap().windows(8).any(|w| w == b"\"secret\""), "sealed on disk");
    }
}
