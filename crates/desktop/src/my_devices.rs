//! This computer as one of the account's devices: its own key, the
//! account's device list, removing another device, and notes to self.
//!
//! The engine owns one [`MyDevices`] when there is a data folder and a
//! history key to work with. The storage and the sync with the other
//! devices are `antiphon_client::devices` and `antiphon_client::device_sync`;
//! this is the desktop's side of them: what Settings > Account shows, what
//! the notes conversation is, and noticing that another device removed
//! this one.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use antiphon_client::account_backup::{self, Platform};
use antiphon_client::device_sync::{self, NoteOp};
use antiphon_client::{config, delivery, devices, vpn};
use antiphon_net::vpn::TrafficKind;
use antiphon_crypto::{BackupKey, DeviceKeyPair, IdentityKeyPair, SignedDeviceList};
use antiphon_ui as ui;

/// The name the notes conversation goes by.
pub const NOTES_NAME: &str = "Notes to self";

/// This device and the account's others.
pub struct MyDevices {
    /// The data folder.
    dir: PathBuf,
    /// The account.
    account: Arc<IdentityKeyPair>,
    /// The key history is sealed under.
    key: BackupKey,
    /// This device's own keypair.
    device: DeviceKeyPair,
    /// The account's device list, as last read.
    list: Option<SignedDeviceList>,
    /// The sync's change count at the last look.
    seen: u64,
    /// The sync's mail key count at the last look.
    keys_seen: u64,
}

/// What changed since the engine last asked.
#[derive(Default)]
pub struct Changes {
    /// Notes or the device list changed on another device: redraw.
    pub changed: bool,
    /// Another device removed this one: the account is gone from here.
    pub removed: bool,
    /// Another device sent the account a new mail key: publish the route
    /// record again so contacts seal to it.
    pub mail_key: bool,
}

/// The desktop's way to the account's mailboxes: through the VPN when
/// messages are routed there, to the nodes in Settings > Delivery.
struct DesktopNet;

impl device_sync::Network for DesktopNet {
    type Socket = antiphon_net::AppSocket;
    async fn bind(&self) -> std::io::Result<Self::Socket> {
        vpn::bind(TrafficKind::Messages, 0).await.map_err(|e| std::io::Error::other(e.to_string()))
    }
    async fn resolve(&self, node: &str) -> Option<std::net::SocketAddr> {
        delivery::resolve(TrafficKind::Messages, node).await
    }
    fn nodes(&self) -> Vec<(String, Vec<u8>)> {
        let cfg = config::load();
        delivery::inbox_nodes(&cfg.routes, cfg.strict_mode).into_iter().map(|(_, node, code)| (node, code)).collect()
    }
}

impl MyDevices {
    /// Loads (or makes) this device's key, makes sure the device is on the
    /// account's list, and starts syncing with the account's other devices.
    pub fn start(runtime: &tokio::runtime::Handle, dir: &Path, account: Arc<IdentityKeyPair>, key: BackupKey) -> Result<Self, String> {
        let device = devices::load_or_create_device(dir).map_err(|e| format!("Couldn't load this device's key: {e}"))?;
        let list = devices::ensure(dir, &account, &device, &computer_name())?;
        let copy = DeviceKeyPair::from_master_seed(device.master_seed());
        // Runs for the life of the app: a different account means a restart.
        drop(device_sync::start(runtime, dir.to_path_buf(), account.clone(), key.clone(), copy, DesktopNet));
        Ok(Self { dir: dir.to_path_buf(), account, key, device, list: Some(list), seen: device_sync::changes(), keys_seen: device_sync::mail_key_changes() })
    }

    /// A copy of this device's keypair, for a move offer's thread.
    pub fn device(&self) -> DeviceKeyPair {
        DeviceKeyPair::from_master_seed(self.device.master_seed())
    }

    /// What changed from the other devices since the last call. On removal
    /// the account is taken off this computer before this returns.
    pub fn poll(&mut self) -> Changes {
        let now = device_sync::changes();
        if now == self.seen {
            return Changes::default();
        }
        self.seen = now;
        self.reload();
        let removed = device_sync::removed_here();
        if removed {
            if let Err(e) = account_backup::sign_out(&self.dir, Platform::Desktop) {
                eprintln!("[devices] removed from the account, but couldn't erase it here: {e}");
            }
        }
        let keys = device_sync::mail_key_changes();
        let mail_key = keys != self.keys_seen;
        self.keys_seen = keys;
        Changes { changed: true, removed, mail_key }
    }

    /// Reads the device list again (after a move approved a new device).
    pub fn reload(&mut self) {
        self.list = devices::load_list(&self.dir, &self.account.public().identity_key);
    }

    /// Takes the device with this key (hex) off the account: every device
    /// is told (the removed one so it signs out), the account gets a new
    /// mail key the removed device never sees, and the caller publishes
    /// the route record again. Returns the device's name.
    pub fn revoke(&mut self, id: &str) -> Result<String, String> {
        let gone = antiphon_crypto::hex::decode_array::<32>(id).ok_or("That isn't a device on your account.")?;
        let name = device_sync::remove_device(&self.dir, &self.account, &self.key, &self.device, &gone);
        self.reload();
        name
    }

    /// The account's devices as Settings lists them, this one first.
    pub fn rows(&self) -> Vec<ui::DeviceRow> {
        let me = self.device.public().identity_key;
        let mut rows: Vec<ui::DeviceRow> = self
            .list
            .iter()
            .flat_map(SignedDeviceList::devices)
            .map(|d| ui::DeviceRow {
                id: antiphon_crypto::hex::encode(&d.device.identity_key),
                name: d.name.clone(),
                added: added_label(d.added_at_secs),
                this: d.device.identity_key == me,
                fingerprint: antiphon_crypto::fingerprint(&d.device.identity_key),
            })
            .collect();
        rows.sort_by_key(|r| !r.this);
        rows
    }

    /// How this device keeps in touch with the others, in words.
    pub fn sync_line(cfg: &config::AppConfig) -> String {
        let nodes = delivery::inbox_nodes(&cfg.routes, cfg.strict_mode).len();
        match nodes {
            _ if cfg.strict_mode => "Strict mode is on, so nothing is stored for your devices: removing one and your notes reach the others only once you turn it off.".into(),
            0 => "Your devices keep in touch through your mailboxes. Add one in Settings > Delivery so a removal and your notes reach your other devices.".into(),
            1 => "Your devices keep in touch through your mailbox, sealed so only each device can read what's meant for it.".into(),
            n => format!("Your devices keep in touch through your {n} mailboxes, sealed so only each device can read what's meant for it."),
        }
    }

    /// The notes conversation's id: the account's own key, hex.
    pub fn notes_id(&self) -> String {
        device_sync::notes_id(&self.account)
    }

    /// Writes a note and sends it to the account's other devices.
    pub fn add_note(&self, text: &str) -> Result<(), String> {
        device_sync::add_note(&self.dir, &self.account, &self.key, &self.device, text).map(|_| ())
    }

    /// Tells the other devices about a change already made to a note here.
    pub fn share(&self, op: NoteOp) {
        device_sync::share_note(&self.dir, &self.account, &self.device, op);
    }
}

/// What this computer is called on the network, for the device list.
fn computer_name() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Computer".into())
}

/// "Added 3 March 2026".
fn added_label(secs: u64) -> String {
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|t| format!("Added {}", t.with_timezone(&chrono::Local).format("%-d %B %Y")))
        .unwrap_or_else(|| "Added".into())
}
