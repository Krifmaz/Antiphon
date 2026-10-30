//! Messages between your own devices: the device list, and notes to self.
//!
//! Each of the account's devices has an inbox of its own at the account's
//! mailbox nodes (the same nodes Settings > Delivery lists). A message for
//! a sibling is sealed to *that device's* key ([`antiphon_crypto::mail`]
//! with the device keypair as recipient), signed by the sending device's
//! key, and left in the sibling's inbox. So:
//!
//! - only the device it was sealed to can open it -- not the node, not a
//!   contact, and not a device that has been removed from the account,
//!   because nothing is sealed to a removed device's key any more;
//! - a message is only believed when its sender is a device the current
//!   list authorizes. The one exception is a device list itself, which
//!   carries the account's own signature and so is believed from anyone
//!   (that is how a removed device learns it is gone).
//!
//! **Inbox labels.** A device's label at a node is derived from the
//! account's master seed, the device's key and the node, so every device
//! of the account can work out where to leave mail for any other, and
//! nobody outside the account can. Collecting needs a fetch secret derived
//! from the *device's own* seed, which only that device has.
//!
//! **Outbox.** What can't be left yet (no node answered, or the sibling
//! hasn't registered its inbox there yet) waits in `devices.outbox`, sealed
//! already -- nothing readable is ever written there -- and is tried again
//! every round for up to [`PENDING_LIFETIME`].
//!
//! **The mail key.** When a device is removed, the device removing it
//! makes the account a new mail key ([`crate::mail_keys`]) and sends it here
//! to every device still listed, sealed to each one's key -- never to the
//! removed one. So the removed device keeps nothing that opens the
//! account's future mail.
//!
//! **Network.** What sockets and names mean differs between the apps (the
//! desktop routes messages through the VPN when asked, the phone simply
//! binds), so a round is given a [`Network`] that answers for it.
//!
//! Every message is safe to receive twice (each is left at every node, so
//! a device reading several nodes may), so nothing here deduplicates more
//! than that.

use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use antiphon_crypto::{hex, mail, BackupKey, DeviceKeyPair, IdentityKeyPair, PublicIdentity, SignedDeviceList};
use antiphon_net::{mailbox, Datagram};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::devices::{self, Accepted};
use crate::history::{self, StoredMessage};
use crate::mail_keys::MailKeys;

/// How the app reaches the account's mailbox nodes.
pub trait Network: Send + Sync + 'static {
    /// The socket a round talks through.
    type Socket: Datagram;
    /// A socket for one round.
    fn bind(&self) -> impl Future<Output = std::io::Result<Self::Socket>> + Send;
    /// Where `node` (`host[:port]`) is now.
    fn resolve(&self, node: &str) -> impl Future<Output = Option<SocketAddr>> + Send;
    /// The account's mailbox nodes, each with its members code. Asked
    /// before every round, so a change in settings is picked up.
    fn nodes(&self) -> Vec<(String, Vec<u8>)>;
}

/// How long one node gets to answer.
const NODE_TIMEOUT: Duration = Duration::from_secs(3);
/// Where pieces of a message wait for the rest.
const PARTS_FILE: &str = "devices.parts";
/// Bytes of frame header: message id, index, count.
const FRAME_HEADER: usize = 8 + 1 + 1;
/// Bytes of message in one frame, so the sealed frame fits a mailbox.
const FRAME_DATA: usize = mailbox::MAX_BLOB - mail::OVERHEAD - FRAME_HEADER;
/// The most frames one message may take (about 200 KB).
const MAX_FRAMES: usize = 255;

/// The file pending sibling mail waits in.
pub const OUTBOX_FILE: &str = "devices.outbox";
/// How long a message for a sibling is retried before it is given up on.
pub const PENDING_LIFETIME: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// How often the sync runs when nothing prompts it sooner.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(45);
/// The most messages kept waiting, so an inbox that never answers can't
/// grow the file without limit. The oldest go first.
const MAX_PENDING: usize = 2_000;
/// Domain separation for device inbox labels.
const LABEL_SALT: &[u8] = b"stoop/device-inbox/label/v1";
/// Domain separation for device fetch secrets.
const SECRET_SALT: &[u8] = b"stoop/device-inbox/fetch/v1";

/// One message from one of your devices to another.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeviceMail {
    /// The account's device list, encoded ([`SignedDeviceList::encode`]) as
    /// hex. Sent whenever it changes: a device added or removed.
    List {
        /// The encoded list, hex.
        list: String,
    },
    /// A change to the notes-to-self conversation.
    Note(NoteOp),
    /// The account's new mail key, after a device was removed.
    MailKey {
        /// Which key: higher is newer.
        epoch: u64,
        /// Its secret, hex. Sealed to the receiving device, like all of this.
        secret: String,
    },
}

/// A change to the notes-to-self conversation, as one device tells the
/// others.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NoteOp {
    /// A new note.
    Add {
        /// The note's id, random so two devices never pick the same one.
        id: u64,
        /// What it says.
        text: String,
        /// When it was written, unix seconds.
        at_secs: u64,
    },
    /// A note changed.
    Edit {
        /// Which note.
        id: u64,
        /// What it says now.
        text: String,
    },
    /// A note deleted.
    Delete {
        /// Which note.
        id: u64,
    },
    /// A note pinned or unpinned.
    Pin {
        /// Which note.
        id: u64,
        /// Pinned now.
        pinned: bool,
    },
}

/// A sealed message waiting to be left for a sibling device.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Pending {
    /// The sibling's device key, hex.
    to: String,
    /// The mailbox id the copies share, hex.
    id: String,
    /// The sealed message, hex. Sealed to the sibling: unreadable here.
    blob: String,
    /// When it was queued, unix seconds.
    queued_secs: u64,
}

/// What one round did, for the app to act on.
#[derive(Debug, Default, PartialEq)]
pub struct Round {
    /// This device was removed from the account by another one: sign out.
    pub removed_here: bool,
    /// The device list changed.
    pub list_changed: bool,
    /// Notes changed.
    pub notes_changed: bool,
    /// The account's mail key changed: the route record contacts hold
    /// should be published again.
    pub mail_key_changed: bool,
    /// Messages for siblings left this round.
    pub sent: usize,
    /// Messages still waiting for a sibling.
    pub waiting: usize,
}

/// Where `device` collects its mail at `node`. Every device of the account
/// can work this out (it needs the account's seed), nobody else can.
pub fn inbox_label(account: &IdentityKeyPair, device: &[u8; 32], node: &str) -> mailbox::Label {
    derive(LABEL_SALT, &account.master_seed(), device, node)
}

/// What `device` proves it may collect its own inbox at `node` with. Made
/// from the device's own seed, so not even a sibling can collect for it.
fn fetch_secret(device: &DeviceKeyPair, node: &str) -> [u8; 32] {
    derive(SECRET_SALT, &device.master_seed(), &device.public().identity_key, node)
}

/// HKDF over `seed`, with the device key and node as the info.
fn derive(salt: &[u8], seed: &[u8; 32], device: &[u8; 32], node: &str) -> [u8; 32] {
    let mut info = Vec::with_capacity(32 + node.len());
    info.extend_from_slice(device);
    info.extend_from_slice(node.as_bytes());
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), seed).expand(&info, &mut out).expect("32 bytes is a valid HKDF output length");
    out
}

/// Seals `message` for every device in `list` but this one, plus `also`
/// (a device just removed, so it hears that it was), and queues them.
pub fn send(dir: &Path, device: &DeviceKeyPair, list: &SignedDeviceList, also: Option<&PublicIdentity>, message: &DeviceMail) -> std::io::Result<()> {
    let me = device.public().identity_key;
    let mut to: Vec<PublicIdentity> = list.devices().iter().map(|d| d.device).filter(|d| d.identity_key != me).collect();
    if let Some(extra) = also {
        if !to.iter().any(|d| d.identity_key == extra.identity_key) {
            to.push(*extra);
        }
    }
    if to.is_empty() {
        return Ok(());
    }
    let frames = frames(&serde_json::to_vec(message).expect("device mail always serializes"))?;
    let mut pending = load_pending(dir);
    for recipient in to {
        for frame in &frames {
            let blob = mail::seal(device, &recipient.identity_key, &recipient.prekey, frame).map_err(|e| std::io::Error::other(e.to_string()))?;
            debug_assert!(blob.len() <= mailbox::MAX_BLOB, "frames are cut to fit");
            let mut id = [0u8; 16];
            getrandom::fill(&mut id).expect("OS randomness is unavailable");
            pending.push(Pending { to: hex::encode(&recipient.identity_key), id: hex::encode(&id), blob: hex::encode(&blob), queued_secs: now_secs() });
        }
    }
    save_pending(dir, &pending)?;
    kick();
    Ok(())
}

/// Cuts a message into frames that each fit a mailbox once sealed:
/// `message id[8] index u8 count u8 data`.
///
/// A device list or a long note is bigger than one mailbox blob, so every
/// message travels this way; the receiver puts the pieces back together
/// ([`reassemble`]).
fn frames(plain: &[u8]) -> std::io::Result<Vec<Vec<u8>>> {
    let chunks: Vec<&[u8]> = if plain.is_empty() { vec![plain] } else { plain.chunks(FRAME_DATA).collect() };
    if chunks.len() > MAX_FRAMES {
        return Err(std::io::Error::other("too long to send to your other devices"));
    }
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(i, data)| {
            let mut frame = Vec::with_capacity(FRAME_HEADER + data.len());
            frame.extend_from_slice(&id);
            frame.push(i as u8);
            frame.push(chunks.len() as u8);
            frame.extend_from_slice(data);
            frame
        })
        .collect())
}

/// Pieces of messages still waiting for the rest, by sender and message
/// id: how many pieces there are, the ones here (index → data, hex), and
/// when the first came.
type Parts = std::collections::BTreeMap<String, (u8, std::collections::BTreeMap<u8, String>, u64)>;

/// Takes one frame from `sender` and returns the whole message once every
/// piece is here. Pieces wait in `devices.parts`, sealed under the history
/// key (they are readable once opened), for up to [`PENDING_LIFETIME`].
fn reassemble(dir: &Path, key: &BackupKey, sender: &[u8; 32], frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < FRAME_HEADER {
        return None;
    }
    let (id, index, count, data) = (&frame[..8], frame[8], frame[9], &frame[FRAME_HEADER..]);
    if count == 0 || index >= count {
        return None;
    }
    if count == 1 {
        return Some(data.to_vec());
    }
    let path = dir.join(PARTS_FILE);
    let mut parts: Parts = std::fs::read(&path)
        .ok()
        .and_then(|b| key.open_content(&b).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now = now_secs();
    parts.retain(|_, (_, _, first)| now.saturating_sub(*first) <= PENDING_LIFETIME.as_secs());
    let slot = format!("{}:{}", hex::encode(sender), hex::encode(id));
    let entry = parts.entry(slot.clone()).or_insert_with(|| (count, Default::default(), now));
    if entry.0 != count {
        return None;
    }
    entry.1.insert(index, hex::encode(data));
    let whole = (entry.1.len() == count as usize).then(|| entry.1.values().filter_map(|h| hex::decode(h)).flatten().collect::<Vec<u8>>());
    if whole.is_some() {
        parts.remove(&slot);
    }
    let plain = serde_json::to_vec(&parts).expect("parts always serialize");
    if let Ok(sealed) = key.seal_content(&plain) {
        let _ = std::fs::write(&path, sealed);
    }
    whole
}

/// Tells the account's other devices about a list change. `removed` is a
/// device just taken off, which is told too.
pub fn send_list(dir: &Path, device: &DeviceKeyPair, list: &SignedDeviceList, removed: Option<&PublicIdentity>) -> std::io::Result<()> {
    send(dir, device, list, removed, &DeviceMail::List { list: hex::encode(&list.encode()) })
}

/// Removes a device from the account, from this one: the shorter list goes
/// to every device (the removed one included, so it signs out), and a new
/// mail key to every device still on it. Returns the removed device's name.
///
/// The caller then publishes its route record again so contacts seal to
/// the new key.
pub fn remove_device(dir: &Path, account: &IdentityKeyPair, key: &BackupKey, device: &DeviceKeyPair, gone: &[u8; 32]) -> Result<String, String> {
    let before = devices::load_list(dir, &account.public().identity_key);
    let removed = before.as_ref().and_then(|l| l.credential(gone)).cloned();
    let list = devices::revoke(dir, account, device, gone)?;
    let unsent = |e: std::io::Error| format!("Removed, but couldn't tell your devices yet: {e}");
    send_list(dir, device, &list, removed.as_ref().map(|c| &c.device)).map_err(unsent)?;
    let mut keys = MailKeys::load(dir, key);
    let (epoch, secret) = keys.rotate(now_secs());
    keys.save(dir, key).map_err(|e| format!("Removed, but couldn't save the new mail key: {e}"))?;
    send(dir, device, &list, None, &DeviceMail::MailKey { epoch, secret: hex::encode(&secret) }).map_err(unsent)?;
    Ok(removed.map(|c| c.name).unwrap_or_else(|| "The device".into()))
}

/// Writes a new note here and tells the account's other devices. Returns
/// the note's id.
pub fn add_note(dir: &Path, account: &IdentityKeyPair, key: &BackupKey, device: &DeviceKeyPair, text: &str) -> Result<u64, String> {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    // Ids count from 1 elsewhere in history; keep clear of the small ones
    // a conversation numbers its own messages with.
    let id = u64::from_be_bytes(id) | (1 << 63);
    let op = NoteOp::Add { id, text: text.to_string(), at_secs: now_secs() };
    apply_note(dir, account, key, &op).map_err(|e| e.to_string())?;
    share_note(dir, account, device, op);
    Ok(id)
}

/// Tells the account's other devices about a note change made here (an
/// edit, delete or pin the app has already written to history).
pub fn share_note(dir: &Path, account: &IdentityKeyPair, device: &DeviceKeyPair, op: NoteOp) {
    if let Some(list) = devices::load_list(dir, &account.public().identity_key) {
        if let Err(e) = send(dir, device, &list, None, &DeviceMail::Note(op)) {
            eprintln!("[devices] couldn't queue a note for your other devices: {e}");
        }
    }
}

/// The history file notes to self are kept in: the conversation "with"
/// the account's own key.
pub fn notes_id(account: &IdentityKeyPair) -> String {
    hex::encode(&account.public().identity_key)
}

/// Applies one note change to this device's notes. Safe to repeat.
pub fn apply_note(dir: &Path, account: &IdentityKeyPair, key: &BackupKey, op: &NoteOp) -> std::io::Result<bool> {
    let me = notes_id(account);
    Ok(match op {
        NoteOp::Add { id, text, at_secs } => {
            let log = history::load(dir, &me, key);
            if log.iter().any(|m| m.id == *id) {
                false
            } else {
                let note = StoredMessage { id: *id, from_me: true, text: text.clone(), at_secs: *at_secs, ..Default::default() };
                history::append(dir, &me, key, note).map_err(|e| std::io::Error::other(e.to_string()))?;
                true
            }
        }
        NoteOp::Edit { id, text } => history::apply_edit(dir, &me, key, *id, true, text),
        NoteOp::Delete { id } => history::apply_delete(dir, &me, key, *id, true),
        NoteOp::Pin { id, pinned } => history::set_pinned(dir, &me, key, *id, *pinned),
    })
}

/// One sync: collect this device's inbox at every node, act on what came,
/// then leave whatever is waiting for siblings.
pub async fn round(dir: &Path, account: &IdentityKeyPair, key: &BackupKey, device: &DeviceKeyPair, net: &impl Network) -> Round {
    let mut out = Round::default();
    let Ok(socket) = net.bind().await else { return out };
    let nodes = net.nodes();
    let account_key = account.public().identity_key;
    let me = device.public().identity_key;

    for (node, code) in &nodes {
        let Some(addr) = net.resolve(node).await else { continue };
        let label = inbox_label(account, &me, node);
        let secret = fetch_secret(device, node);
        let timeout = NODE_TIMEOUT;
        if mailbox::register(&socket, addr, label, mail::fetch_hash(&secret), code, timeout).await.is_err() {
            continue;
        }
        let Ok(waiting) = mailbox::fetch(&socket, addr, label, secret, timeout).await else { continue };
        for (id, blob) in waiting {
            receive(dir, account, key, device, &blob, &mut out);
            if out.removed_here {
                // Nothing more to do here: the account is leaving this device.
                return out;
            }
            let _ = mailbox::ack(&socket, addr, label, secret, id, timeout).await;
        }
    }

    // Leave what waits. A message for a device no longer listed is dropped,
    // unless it is the list that removed it (that one it must hear).
    let list = devices::load_list(dir, &account_key);
    let mut pending = load_pending(dir);
    let now = now_secs();
    let mut kept = Vec::with_capacity(pending.len());
    for p in pending.drain(..) {
        if now.saturating_sub(p.queued_secs) > PENDING_LIFETIME.as_secs() {
            continue;
        }
        let (Some(to), Some(blob), Some(id)) = (hex::decode_array::<32>(&p.to), hex::decode(&p.blob), hex::decode_array::<16>(&p.id)) else { continue };
        let listed = list.as_ref().is_some_and(|l| l.authorizes(&to));
        let mut left = false;
        for (node, _) in &nodes {
            let Some(addr) = net.resolve(node).await else { continue };
            if mailbox::deposit(&socket, addr, inbox_label(account, &to, node), id, &blob, NODE_TIMEOUT).await.is_ok() {
                left = true;
            }
        }
        if left {
            out.sent += 1;
        } else if listed || p.queued_secs + 7 * 24 * 60 * 60 > now {
            // A removed device gets a week to hear it was removed.
            kept.push(p);
        }
    }
    out.waiting = kept.len();
    let _ = save_pending(dir, &kept);
    out
}

/// Opens and acts on one message from this device's inbox.
fn receive(dir: &Path, account: &IdentityKeyPair, key: &BackupKey, device: &DeviceKeyPair, blob: &[u8], out: &mut Round) {
    // Sealed to the device's key, never the account's.
    let Ok((sender, frame)) = mail::open(device, blob) else { return };
    let me = &device.public().identity_key;
    let Some(plain) = reassemble(dir, key, &sender.identity_key, &frame) else { return };
    let Ok(message) = serde_json::from_slice::<DeviceMail>(&plain) else { return };
    let account_key = account.public().identity_key;
    match message {
        DeviceMail::List { list } => {
            // Believed for its own signature, whoever carried it.
            let Some(list) = hex::decode(&list).and_then(|b| SignedDeviceList::decode(&b).ok()) else { return };
            match devices::accept(dir, &account_key, me, &list) {
                Accepted::Ignored => {}
                Accepted::Updated => out.list_changed = true,
                Accepted::RemovedHere => {
                    out.list_changed = true;
                    out.removed_here = true;
                }
            }
        }
        DeviceMail::Note(op) => {
            if listed(dir, &account_key, &sender) && apply_note(dir, account, key, &op).unwrap_or(false) {
                out.notes_changed = true;
            }
        }
        DeviceMail::MailKey { epoch, secret } => {
            let Some(secret) = hex::decode_array::<32>(&secret) else { return };
            if !listed(dir, &account_key, &sender) {
                return;
            }
            let mut keys = MailKeys::load(dir, key);
            if keys.adopt(epoch, secret, now_secs()) && keys.save(dir, key).is_ok() {
                out.mail_key_changed = true;
            }
        }
    }
}

/// Whether `sender` is a device the account still lists: the only devices
/// whose notes and keys are believed.
fn listed(dir: &Path, account: &[u8; 32], sender: &PublicIdentity) -> bool {
    devices::load_list(dir, account).is_some_and(|l| l.authorizes(&sender.identity_key))
}

// ---- The background loop the apps run. ----

/// Wakes the loop early: something was queued.
static KICK: OnceLock<tokio::sync::Notify> = OnceLock::new();
/// Bumped whenever notes or the device list change, so the app redraws.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// Set when another device removed this one.
static REMOVED: AtomicBool = AtomicBool::new(false);
/// Bumped whenever another device sent a new mail key.
static MAIL_KEY: AtomicU64 = AtomicU64::new(0);

/// Starts syncing with the account's other devices in the background.
/// Aborting the returned task stops it (the phone does, when it loads a
/// different account without restarting).
pub fn start(
    runtime: &tokio::runtime::Handle,
    dir: PathBuf,
    account: std::sync::Arc<IdentityKeyPair>,
    key: BackupKey,
    device: DeviceKeyPair,
    net: impl Network,
) -> tokio::task::JoinHandle<()> {
    REMOVED.store(false, Ordering::Relaxed);
    let kick = KICK.get_or_init(tokio::sync::Notify::new);
    runtime.spawn(async move {
        loop {
            let r = round(&dir, &account, &key, &device, &net).await;
            if r.list_changed || r.notes_changed {
                CHANGES.fetch_add(1, Ordering::Relaxed);
            }
            if r.mail_key_changed {
                MAIL_KEY.fetch_add(1, Ordering::Relaxed);
                CHANGES.fetch_add(1, Ordering::Relaxed);
            }
            if r.removed_here {
                REMOVED.store(true, Ordering::Relaxed);
                CHANGES.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let _ = tokio::time::timeout(SYNC_INTERVAL, kick.notified()).await;
        }
    })
}

/// Runs the next round now, rather than at its next tick.
pub fn kick() {
    KICK.get_or_init(tokio::sync::Notify::new).notify_one();
}

/// Moves whenever notes or the device list changed from another device.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::Relaxed)
}

/// Moves whenever another device sent the account a new mail key: time to
/// publish the route record again.
pub fn mail_key_changes() -> u64 {
    MAIL_KEY.load(Ordering::Relaxed)
}

/// Another of the account's devices removed this one.
pub fn removed_here() -> bool {
    REMOVED.load(Ordering::Relaxed)
}

// ---- The outbox file. ----

/// Where pending sibling mail waits.
fn outbox_path(dir: &Path) -> PathBuf {
    dir.join(OUTBOX_FILE)
}

/// What waits for siblings. A damaged file is treated as empty: it holds
/// nothing that can't be sent again.
fn load_pending(dir: &Path) -> Vec<Pending> {
    std::fs::read(outbox_path(dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Writes what waits, oldest dropped past [`MAX_PENDING`].
fn save_pending(dir: &Path, pending: &[Pending]) -> std::io::Result<()> {
    let start = pending.len().saturating_sub(MAX_PENDING);
    let path = outbox_path(dir);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&pending[start..]).expect("pending always serializes"))?;
    std::fs::rename(&tmp, &path)
}

/// Now, in unix seconds.
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A fresh, empty data folder.
    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stoop-device-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A mailbox node on loopback, as `host:port`.
    async fn node() -> String {
        use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap().to_string();
        let mailbox = Mailbox::new(MailboxLimits { enabled: true, ..Default::default() });
        tokio::spawn(antiphon_relay::run_node(
            socket,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))),
            Some(Arc::new(Mutex::new(mailbox))),
            None,
        ));
        addr
    }

    /// Plain sockets and loopback names, as a test sees the network.
    struct TestNet {
        /// The mailbox nodes.
        nodes: Vec<(String, Vec<u8>)>,
    }

    impl Network for TestNet {
        type Socket = tokio::net::UdpSocket;
        async fn bind(&self) -> std::io::Result<Self::Socket> {
            tokio::net::UdpSocket::bind("127.0.0.1:0").await
        }
        async fn resolve(&self, node: &str) -> Option<SocketAddr> {
            node.parse().ok()
        }
        fn nodes(&self) -> Vec<(String, Vec<u8>)> {
            self.nodes.clone()
        }
    }

    /// One of the account's devices: its folder and keys.
    struct Device {
        /// Its data folder.
        dir: PathBuf,
        /// Its own keypair.
        keys: DeviceKeyPair,
    }

    #[test]
    fn a_long_message_comes_back_whole_from_its_pieces_in_any_order() {
        let dir = folder("frames");
        let key = BackupKey::generate();
        let message: Vec<u8> = (0..3_000u32).map(|i| (i % 251) as u8).collect();
        let mut pieces = frames(&message).unwrap();
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|f| f.len() + mail::OVERHEAD <= mailbox::MAX_BLOB));
        pieces.reverse();
        let last = pieces.pop().unwrap();
        for p in &pieces {
            assert!(reassemble(&dir, &key, &[1; 32], p).is_none());
        }
        assert_eq!(reassemble(&dir, &key, &[1; 32], &last), Some(message));
        // Pieces from someone else never complete another sender's message.
        assert!(reassemble(&dir, &key, &[2; 32], &pieces[0]).is_none());
    }

    #[test]
    fn labels_differ_per_device_and_node_and_need_the_account() {
        let account = IdentityKeyPair::generate();
        let other = IdentityKeyPair::generate();
        let (a, b) = ([1; 32], [2; 32]);
        assert_ne!(inbox_label(&account, &a, "n:1"), inbox_label(&account, &b, "n:1"));
        assert_ne!(inbox_label(&account, &a, "n:1"), inbox_label(&account, &a, "n:2"));
        assert_ne!(inbox_label(&account, &a, "n:1"), inbox_label(&other, &a, "n:1"));
    }

    #[tokio::test]
    async fn notes_reach_the_other_device_and_a_removed_one_hears_it() {
        let node = node().await;
        let nodes = TestNet { nodes: vec![(node, Vec::new())] };
        let account = IdentityKeyPair::generate();
        let key = BackupKey::generate();
        let desktop = Device { dir: folder("desktop"), keys: DeviceKeyPair::generate() };
        let phone = Device { dir: folder("phone"), keys: DeviceKeyPair::generate() };
        let list = devices::approve(&desktop.dir, &account, &desktop.keys, &phone.keys.public(), "Phone").unwrap();
        devices::save_list(&phone.dir, &list).unwrap();

        // The phone registers its inbox first, as it would on its first round.
        round(&phone.dir, &account, &key, &phone.keys, &nodes).await;
        let id = add_note(&desktop.dir, &account, &key, &desktop.keys, "milk").unwrap();
        let sent = round(&desktop.dir, &account, &key, &desktop.keys, &nodes).await;
        assert_eq!((sent.sent, sent.waiting), (1, 0));
        let got = round(&phone.dir, &account, &key, &phone.keys, &nodes).await;
        assert!(got.notes_changed);
        let notes = history::load(&phone.dir, &notes_id(&account), &key);
        assert_eq!((notes[0].id, notes[0].text.as_str(), notes[0].from_me), (id, "milk", true));

        // The desktop removes the phone; the phone hears it and is told to sign out.
        let gone = devices::revoke(&desktop.dir, &account, &desktop.keys, &phone.keys.public().identity_key).unwrap();
        send_list(&desktop.dir, &desktop.keys, &gone, Some(&phone.keys.public())).unwrap();
        round(&desktop.dir, &account, &key, &desktop.keys, &nodes).await;
        let told = round(&phone.dir, &account, &key, &phone.keys, &nodes).await;
        assert!(told.removed_here);

        // Nothing more is sealed to the removed phone.
        add_note(&desktop.dir, &account, &key, &desktop.keys, "eggs").unwrap();
        assert!(load_pending(&desktop.dir).is_empty(), "no device left to tell");
    }

    #[tokio::test]
    async fn removing_a_device_gives_the_others_a_mail_key_it_never_sees() {
        let node = node().await;
        let nodes = TestNet { nodes: vec![(node, Vec::new())] };
        let account = IdentityKeyPair::generate();
        let key = BackupKey::generate();
        let desktop = Device { dir: folder("key-desktop"), keys: DeviceKeyPair::generate() };
        let laptop = Device { dir: folder("key-laptop"), keys: DeviceKeyPair::generate() };
        let phone = Device { dir: folder("key-phone"), keys: DeviceKeyPair::generate() };
        devices::approve(&desktop.dir, &account, &desktop.keys, &laptop.keys.public(), "Laptop").unwrap();
        let all = devices::approve(&desktop.dir, &account, &desktop.keys, &phone.keys.public(), "Phone").unwrap();
        for d in [&laptop, &phone] {
            devices::save_list(&d.dir, &all).unwrap();
            round(&d.dir, &account, &key, &d.keys, &nodes).await;
        }

        let name = remove_device(&desktop.dir, &account, &key, &desktop.keys, &phone.keys.public().identity_key).unwrap();
        assert_eq!(name, "Phone");
        let sent = MailKeys::load(&desktop.dir, &key);
        assert_eq!(sent.epoch(), 1);
        round(&desktop.dir, &account, &key, &desktop.keys, &nodes).await;

        let laptop_round = round(&laptop.dir, &account, &key, &laptop.keys, &nodes).await;
        assert!(laptop_round.mail_key_changed);
        assert_eq!(MailKeys::load(&laptop.dir, &key), sent);
        let phone_round = round(&phone.dir, &account, &key, &phone.keys, &nodes).await;
        assert!(phone_round.removed_here);
        assert_eq!(MailKeys::load(&phone.dir, &key).epoch(), 0, "the removed phone never gets the new key");
    }

    #[tokio::test]
    async fn notes_from_a_removed_device_are_not_believed() {
        let node = node().await;
        let nodes = TestNet { nodes: vec![(node, Vec::new())] };
        let account = IdentityKeyPair::generate();
        let key = BackupKey::generate();
        let desktop = Device { dir: folder("trust-desktop"), keys: DeviceKeyPair::generate() };
        let stolen = Device { dir: folder("trust-stolen"), keys: DeviceKeyPair::generate() };
        let both = devices::approve(&desktop.dir, &account, &desktop.keys, &stolen.keys.public(), "Phone").unwrap();
        devices::save_list(&stolen.dir, &both).unwrap();
        devices::revoke(&desktop.dir, &account, &desktop.keys, &stolen.keys.public().identity_key).unwrap();

        round(&desktop.dir, &account, &key, &desktop.keys, &nodes).await;
        // The stolen phone ignores its removal and still holds the old list.
        add_note(&stolen.dir, &account, &key, &stolen.keys, "send money to …").unwrap();
        round(&stolen.dir, &account, &key, &stolen.keys, &nodes).await;
        let got = round(&desktop.dir, &account, &key, &desktop.keys, &nodes).await;
        assert!(!got.notes_changed);
        assert!(history::load(&desktop.dir, &notes_id(&account), &key).is_empty());
    }
}
