//! This device's own keys, and the account's list of devices (spec §3
//! "Multi-device -- new devices added by approving them from an existing
//! device").
//!
//! Every install has a keypair of its own, kept in `device.key` beside the
//! account's `identity.key`. It never leaves the device: a backup or a
//! move carries the account, never the device key, so two devices of one
//! account always have two different device keys.
//!
//! The account's devices are a [`SignedDeviceList`] in `devices.list`,
//! signed by the account's master key (see [`antiphon_crypto::device`]).
//! That list is what the rest of the app asks "is this really one of my
//! devices?":
//!
//! - a device moved here from another one arrives already in the list --
//!   the old device approved it before sending ([`approve`]);
//! - a device restored from a backup file or a recovery phrase has no
//!   other device to ask, and holds the master key itself, so it signs
//!   itself in on its first start ([`ensure`]);
//! - removing a device from another one is a new list without it, under a
//!   higher sequence number ([`revoke`]). When the removed device next
//!   hears from its siblings it sees itself gone and signs out
//!   ([`accept`], [`Accepted::RemovedHere`]).
//!
//! Pure file handling: nothing here touches the network, so the phone app
//! can share this module by path as it does `history`.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use antiphon_crypto::{DeviceCredential, DeviceKeyPair, DeviceList, IdentityKeyPair, PublicIdentity, SignedDeviceList};

/// The file holding this device's own keypair.
pub const DEVICE_KEY_FILE: &str = "device.key";
/// The file holding the account's signed device list.
pub const LIST_FILE: &str = "devices.list";
/// The longest device name the list accepts (the crypto layer's cap).
pub const MAX_NAME_CHARS: usize = 64;

/// Where this device's keypair is kept.
pub fn device_key_path(dir: &Path) -> PathBuf {
    dir.join(DEVICE_KEY_FILE)
}

/// Where the account's device list is kept.
pub fn list_path(dir: &Path) -> PathBuf {
    dir.join(LIST_FILE)
}

/// This device's keypair, made on first use.
///
/// Stored the way the identity is ([`antiphon_crypto::store`]), so it gets
/// the same operating-system sealing where there is one. A device key is
/// cheap to replace -- a new one only means approving this device again --
/// but it is still refused rather than silently replaced when damaged,
/// for the same reason the identity is: a quiet new key looks, to the
/// account's other devices, like a stranger.
pub fn load_or_create_device(dir: &Path) -> std::io::Result<DeviceKeyPair> {
    antiphon_crypto::store::load_or_create(&device_key_path(dir))
}

/// The account's device list as saved here, if there is one, it verifies,
/// and it belongs to `account` (a list left over from an account this
/// device used to be is ignored).
pub fn load_list(dir: &Path, account: &[u8; 32]) -> Option<SignedDeviceList> {
    let bytes = std::fs::read(list_path(dir)).ok()?;
    let list = SignedDeviceList::decode(&bytes).ok()?;
    (list.list.identity_key == *account && list.verify().is_ok()).then_some(list)
}

/// Writes the list, through a temporary file so a crash mid-write never
/// leaves half a list behind.
pub fn save_list(dir: &Path, list: &SignedDeviceList) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = list_path(dir);
    let tmp = path.with_extension("list.tmp");
    std::fs::write(&tmp, list.encode())?;
    std::fs::rename(&tmp, &path)
}

/// A name for the list, cut to what it holds. Blank becomes `fallback`.
pub fn clean_name(name: &str, fallback: &str) -> String {
    let name = name.trim();
    let name = if name.is_empty() { fallback } else { name };
    // Cut by characters, then by bytes, so the cap is never exceeded and
    // never splits a character.
    let mut out: String = name.chars().take(MAX_NAME_CHARS).collect();
    while out.len() > MAX_NAME_CHARS {
        out.pop();
    }
    out
}

/// Makes sure this device is in the account's list, and returns the list.
///
/// - No list yet (a fresh account, or one from before device lists): a
///   new one naming just this device.
/// - A list that doesn't name this device: this device holds the master
///   key -- it was restored from a backup or a phrase, or moved by an
///   older version that didn't send its keys -- so it signs itself in.
///
/// A device that was *removed* never gets here: [`accept`] reports that,
/// and the app signs out instead of starting.
pub fn ensure(dir: &Path, account: &IdentityKeyPair, device: &DeviceKeyPair, name: &str) -> Result<SignedDeviceList, String> {
    let key = account.public().identity_key;
    let me = device.public();
    match load_list(dir, &key) {
        Some(list) if list.authorizes(&me.identity_key) => Ok(list),
        Some(list) => {
            let credential = DeviceCredential::approve(account, me, &clean_name(name, "This device"), now_secs()).map_err(|e| e.to_string())?;
            let next = list.with_device(account, credential).map_err(|e| e.to_string())?;
            save_list(dir, &next).map_err(|e| e.to_string())?;
            Ok(next)
        }
        None => {
            let credential = DeviceCredential::approve(account, me, &clean_name(name, "This device"), now_secs()).map_err(|e| e.to_string())?;
            let list = DeviceList { identity_key: key, sequence: 1, devices: vec![credential] }.sign(account).map_err(|e| e.to_string())?;
            save_list(dir, &list).map_err(|e| e.to_string())?;
            Ok(list)
        }
    }
}

/// Approves another device into the account: the step an existing device
/// takes when a new one asks to join (see `antiphon_net::account_move`).
///
/// Safe to repeat for the same device: one already listed keeps its entry
/// and the list is returned unchanged, so a move retried after a dropped
/// connection doesn't step the sequence again.
pub fn approve(dir: &Path, account: &IdentityKeyPair, this_device: &DeviceKeyPair, new: &PublicIdentity, name: &str) -> Result<SignedDeviceList, String> {
    let list = ensure(dir, account, this_device, "This device")?;
    if list.authorizes(&new.identity_key) {
        return Ok(list);
    }
    let credential = DeviceCredential::approve(account, *new, &clean_name(name, "New device"), now_secs()).map_err(|e| e.to_string())?;
    let next = list.with_device(account, credential).map_err(|e| e.to_string())?;
    save_list(dir, &next).map_err(|e| e.to_string())?;
    Ok(next)
}

/// Removes a device from the account, from this one. Returns the new list,
/// which the caller sends to the account's devices -- including the one
/// removed, so it learns it is gone.
///
/// This device can't remove itself here: that is signing out, and it would
/// leave a list nobody here is in.
pub fn revoke(dir: &Path, account: &IdentityKeyPair, this_device: &DeviceKeyPair, gone: &[u8; 32]) -> Result<SignedDeviceList, String> {
    if *gone == this_device.public().identity_key {
        return Err("This is the device you're using. Remove it from one of your other devices.".into());
    }
    let list = ensure(dir, account, this_device, "This device")?;
    if !list.authorizes(gone) {
        return Err("That device isn't on your account any more.".into());
    }
    let next = list.without_device(account, gone).map_err(|e| e.to_string())?;
    save_list(dir, &next).map_err(|e| e.to_string())?;
    Ok(next)
}

/// What became of a list another device sent.
#[derive(Debug, PartialEq)]
pub enum Accepted {
    /// Not newer than the list here, not this account's, or not validly
    /// signed: nothing changed.
    Ignored,
    /// Saved: it is the account's list now.
    Updated,
    /// Saved, and it no longer names this device: this device was removed
    /// from the account and should sign out.
    RemovedHere,
}

/// Takes a list another of the account's devices sent, if it is newer and
/// genuinely signed by the account.
///
/// Only a list that *used* to name this device and no longer does counts
/// as removal. A device that never made it into the list (an old-style
/// move that hasn't started yet) is not signed out by an unrelated change.
pub fn accept(dir: &Path, account: &[u8; 32], this_device: &[u8; 32], incoming: &SignedDeviceList) -> Accepted {
    if incoming.list.identity_key != *account {
        return Accepted::Ignored;
    }
    let held = load_list(dir, account);
    if incoming.accept(held.as_ref().map(|l| l.list.sequence)).is_err() {
        return Accepted::Ignored;
    }
    if save_list(dir, incoming).is_err() {
        return Accepted::Ignored;
    }
    let was_listed = held.is_some_and(|l| l.authorizes(this_device));
    if was_listed && !incoming.authorizes(this_device) {
        Accepted::RemovedHere
    } else {
        Accepted::Updated
    }
}

/// Now, in unix seconds.
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty data folder.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stoop-devices-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_device_key_is_its_own_and_stays() {
        let dir = folder("key");
        let account = IdentityKeyPair::generate();
        let device = load_or_create_device(&dir).unwrap();
        assert_ne!(device.public().identity_key, account.public().identity_key);
        assert_eq!(load_or_create_device(&dir).unwrap().public(), device.public());
    }

    #[test]
    fn a_fresh_account_lists_just_this_device() {
        let dir = folder("fresh");
        let account = IdentityKeyPair::generate();
        let device = DeviceKeyPair::generate();
        let list = ensure(&dir, &account, &device, "Desktop").unwrap();
        assert_eq!(list.list.sequence, 1);
        assert_eq!(list.devices().len(), 1);
        assert!(list.authorizes(&device.public().identity_key));
        assert_eq!(load_list(&dir, &account.public().identity_key), Some(list.clone()));
        // A second start changes nothing.
        assert_eq!(ensure(&dir, &account, &device, "Desktop").unwrap(), list);
    }

    #[test]
    fn a_restored_device_signs_itself_in() {
        let (old, new) = (folder("restore-old"), folder("restore-new"));
        let account = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let list = ensure(&old, &account, &desktop, "Desktop").unwrap();
        // The backup carried the list across; the laptop isn't in it.
        save_list(&new, &list).unwrap();
        let laptop = DeviceKeyPair::generate();
        let after = ensure(&new, &account, &laptop, "Laptop").unwrap();
        assert_eq!(after.list.sequence, 2);
        assert!(after.authorizes(&desktop.public().identity_key));
        assert!(after.authorizes(&laptop.public().identity_key));
    }

    #[test]
    fn a_list_from_another_account_is_ignored() {
        let dir = folder("other");
        let theirs = IdentityKeyPair::generate();
        ensure(&dir, &theirs, &DeviceKeyPair::generate(), "Theirs").unwrap();
        let mine = IdentityKeyPair::generate();
        assert!(load_list(&dir, &mine.public().identity_key).is_none());
        let device = DeviceKeyPair::generate();
        let list = ensure(&dir, &mine, &device, "Mine").unwrap();
        assert_eq!(list.list.sequence, 1);
        assert_eq!(list.list.identity_key, mine.public().identity_key);
    }

    #[test]
    fn approving_twice_is_one_entry_and_one_step() {
        let dir = folder("approve");
        let account = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate().public();
        let once = approve(&dir, &account, &desktop, &phone, "Phone").unwrap();
        let twice = approve(&dir, &account, &desktop, &phone, "Phone").unwrap();
        assert_eq!(once, twice);
        assert_eq!(twice.devices().len(), 2);
    }

    #[test]
    fn removing_a_device_is_seen_by_it_and_nobody_else_signs_out() {
        let (here, there, third) = (folder("rev-a"), folder("rev-b"), folder("rev-c"));
        let account = IdentityKeyPair::generate();
        let key = account.public().identity_key;
        let desktop = DeviceKeyPair::generate();
        let phone = DeviceKeyPair::generate();
        let laptop = DeviceKeyPair::generate();
        approve(&here, &account, &desktop, &phone.public(), "Phone").unwrap();
        let both = approve(&here, &account, &desktop, &laptop.public(), "Laptop").unwrap();
        save_list(&there, &both).unwrap();
        save_list(&third, &both).unwrap();

        let without_phone = revoke(&here, &account, &desktop, &phone.public().identity_key).unwrap();
        assert_eq!(accept(&there, &key, &phone.public().identity_key, &without_phone), Accepted::RemovedHere);
        assert_eq!(accept(&third, &key, &laptop.public().identity_key, &without_phone), Accepted::Updated);
        // Hearing it twice changes nothing more.
        assert_eq!(accept(&third, &key, &laptop.public().identity_key, &without_phone), Accepted::Ignored);
        // And the older list can't be replayed to bring the phone back.
        assert_eq!(accept(&third, &key, &laptop.public().identity_key, &both), Accepted::Ignored);
    }

    #[test]
    fn this_device_cannot_remove_itself() {
        let dir = folder("self");
        let account = IdentityKeyPair::generate();
        let desktop = DeviceKeyPair::generate();
        assert!(revoke(&dir, &account, &desktop, &desktop.public().identity_key).is_err());
    }

    #[test]
    fn names_are_cut_to_fit_without_splitting_characters() {
        assert_eq!(clean_name("   ", "Phone"), "Phone");
        let long = "é".repeat(100);
        let cut = clean_name(&long, "x");
        assert!(cut.len() <= MAX_NAME_CHARS);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
