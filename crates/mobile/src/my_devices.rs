//! The phone as one of the account's devices: keeping in touch with the
//! others through the mailboxes in Settings > Devices, removing a device,
//! and notes to self.
//!
//! The phone's half of what `crates/desktop/src/my_devices.rs` does on a
//! computer, over the same shared modules (`devices`, `device_sync`,
//! `mail_keys`), so the two talk to each other. What differs is only the
//! network: the phone has no delivery routes or VPN, just the list of
//! mailboxes it was given ([`PhoneNet`]).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use antiphon_crypto::{BackupKey, DeviceKeyPair, IdentityKeyPair};

use crate::account_backup::{self, Platform};
use crate::device_sync::{self, NoteOp};

/// The port a mailbox node listens on when an address names none.
const DEFAULT_PORT: u16 = 3479;
/// The name the notes conversation goes by.
pub const NOTES_NAME: &str = "Notes to self";

/// The phone's way to the account's mailboxes: a plain socket, and the
/// addresses in Settings > Devices.
struct PhoneNet {
    /// The mailboxes, shared with [`PhoneDevices`] so an edit applies at
    /// the next round.
    nodes: Arc<Mutex<Vec<String>>>,
}

impl device_sync::Network for PhoneNet {
    type Socket = tokio::net::UdpSocket;
    async fn bind(&self) -> std::io::Result<Self::Socket> {
        tokio::net::UdpSocket::bind("0.0.0.0:0").await
    }
    async fn resolve(&self, node: &str) -> Option<SocketAddr> {
        let with_port = if node.contains(':') { node.to_string() } else { format!("{node}:{DEFAULT_PORT}") };
        tokio::net::lookup_host(with_port).await.ok()?.next()
    }
    fn nodes(&self) -> Vec<(String, Vec<u8>)> {
        // No members codes on the phone: its mailboxes are open nodes, or
        // the account's own desktop holding mail.
        self.nodes.lock().unwrap().iter().map(|n| (n.clone(), Vec::new())).collect()
    }
}

/// This phone and the account's other devices.
pub struct PhoneDevices {
    /// The app's private storage.
    dir: PathBuf,
    /// The account.
    account: Arc<IdentityKeyPair>,
    /// The key history is sealed under.
    key: BackupKey,
    /// This phone's own keypair.
    device: DeviceKeyPair,
    /// The mailboxes, as the sync reads them.
    nodes: Arc<Mutex<Vec<String>>>,
    /// The background sync; stopped when this is dropped.
    task: tokio::task::JoinHandle<()>,
    /// The sync's change count at the last look.
    seen: u64,
}

impl Drop for PhoneDevices {
    /// A reloaded account starts a sync of its own; this one stops.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// What changed since the app last asked.
#[derive(Default)]
pub struct Changes {
    /// Notes or the device list changed on another device.
    pub changed: bool,
    /// Another device removed this phone: the account is gone from here.
    pub removed: bool,
}

impl PhoneDevices {
    /// Loads this phone's key and starts syncing with the account's other
    /// devices through `mailboxes`.
    pub fn start(runtime: &tokio::runtime::Handle, dir: &Path, account: &IdentityKeyPair, key: BackupKey, mailboxes: Vec<String>) -> Result<Self, String> {
        let device = crate::devices::load_or_create_device(dir).map_err(|e| format!("Couldn't load this phone's device key: {e}"))?;
        let account = Arc::new(IdentityKeyPair::from_master_seed(account.master_seed()));
        let nodes = Arc::new(Mutex::new(mailboxes));
        let task = device_sync::start(
            runtime,
            dir.to_path_buf(),
            account.clone(),
            key.clone(),
            DeviceKeyPair::from_master_seed(device.master_seed()),
            PhoneNet { nodes: nodes.clone() },
        );
        Ok(Self { dir: dir.to_path_buf(), account, key, device, nodes, task, seen: device_sync::changes() })
    }

    /// Replaces the mailboxes the sync uses, and syncs now.
    pub fn set_mailboxes(&self, mailboxes: Vec<String>) {
        *self.nodes.lock().unwrap() = mailboxes;
        device_sync::kick();
    }

    /// What changed from the other devices since the last call. On removal
    /// the account is taken off this phone before this returns.
    pub fn poll(&mut self) -> Changes {
        let now = device_sync::changes();
        if now == self.seen {
            return Changes::default();
        }
        self.seen = now;
        let removed = device_sync::removed_here();
        if removed {
            if let Err(e) = account_backup::sign_out(&self.dir, Platform::Phone) {
                log::warn!("removed from the account, but couldn't erase it here: {e}");
            }
        }
        Changes { changed: true, removed }
    }

    /// Takes the device with this key (hex) off the account. Returns its name.
    pub fn revoke(&self, id: &str) -> Result<String, String> {
        let gone = antiphon_crypto::hex::decode_array::<32>(id).ok_or("That isn't a device on your account.")?;
        device_sync::remove_device(&self.dir, &self.account, &self.key, &self.device, &gone)
    }

    /// The notes conversation's id: the account's own key, hex.
    pub fn notes_id(&self) -> String {
        device_sync::notes_id(&self.account)
    }

    /// Writes a note and sends it to the account's other devices.
    pub fn add_note(&self, text: &str) -> Result<(), String> {
        device_sync::add_note(&self.dir, &self.account, &self.key, &self.device, text).map(|_| ())
    }

    /// Applies a note change here and, if it changed anything, tells the
    /// other devices.
    pub fn change_note(&self, op: NoteOp) -> Result<(), String> {
        let changed = device_sync::apply_note(&self.dir, &self.account, &self.key, &op).map_err(|e| format!("Couldn't change the note: {e}"))?;
        if changed {
            device_sync::share_note(&self.dir, &self.account, &self.device, op);
        }
        Ok(())
    }
}
