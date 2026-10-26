//! Files that outlive the call (spec §5 "Seeding", "Pinning").
//!
//! A file sent in a call can also be *shared*: sealed under a key of its
//! own (see `antiphon_crypto::file_seal`) and held, still sealed, by this
//! app while seeding is on and by any pin nodes you name. The other side
//! gets a link -- name, size, id, key and who holds it -- inside the
//! call's encrypted channel, and can fetch it later from whoever is up,
//! several at once (`crate::swarm`).
//!
//! What anyone holding a copy sees is sealed chunks. The key exists only
//! in the two people's shared-files lists, which are encrypted on disk
//! under the backup key like history.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use antiphon_crypto::file_seal::{self, FileId, FileKey, CHUNK};
use antiphon_relay::files::{FileHost, FileStore, HostPolicy, Scope};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;

use crate::delivery::{hex, unhex32};
use crate::swarm::{self, Availability, Progress};

/// One entry in the shared-files list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SharedFile {
    pub name: String,
    pub size: u64,
    pub file_id: String,
    pub key: String,
    /// `host:port` addresses that said they'd hold it.
    pub holders: Vec<String>,
    /// The other person's identity key, hex.
    pub with: String,
    pub at_secs: u64,
    pub mine: bool,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn list_path(dir: &Path) -> PathBuf {
    dir.join("shared-files")
}

static LIST_LOCK: Mutex<()> = Mutex::new(());

pub fn load() -> Vec<SharedFile> {
    let Some(dir) = crate::config::config_dir() else { return Vec::new() };
    let Ok(key) = crate::history::load_or_create_backup_key(&dir) else { return Vec::new() };
    let Ok(sealed) = std::fs::read(list_path(&dir)) else { return Vec::new() };
    let Ok(plain) = key.open_content(&sealed) else { return Vec::new() };
    serde_json::from_slice(&plain).unwrap_or_default()
}

fn save(list: &[SharedFile]) {
    let Some(dir) = crate::config::config_dir() else { return };
    let Ok(key) = crate::history::load_or_create_backup_key(&dir) else { return };
    let Ok(json) = serde_json::to_vec(list) else { return };
    if let Ok(sealed) = key.seal_content(&json) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(list_path(&dir), sealed);
    }
}

/// Adds or refreshes an entry (same file id: holders are merged).
pub fn remember(entry: SharedFile) {
    let _guard = LIST_LOCK.lock().unwrap();
    let mut list = load();
    if let Some(old) = list.iter_mut().find(|f| f.file_id == entry.file_id) {
        for h in entry.holders {
            if !old.holders.contains(&h) {
                old.holders.push(h);
            }
        }
    } else {
        list.insert(0, entry);
    }
    save(&list);
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Takes an entry off the list and, if this app was seeding it, stops.
pub fn forget(file_id: &str) {
    let _guard = LIST_LOCK.lock().unwrap();
    let mut list = load();
    list.retain(|f| f.file_id != file_id);
    save(&list);
    if let (Some(id), Some(host)) = (unhex32(file_id), HOST.get()) {
        host.lock().unwrap().store.purge(&id);
    }
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bumped whenever the list or a status changes, so the UI knows to redraw.
pub fn revision() -> u64 {
    REVISION.load(std::sync::atomic::Ordering::Relaxed)
}

static STATUS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn set_status(file_id: &str, text: impl Into<String>) {
    let mut s = STATUS.lock().unwrap();
    s.retain(|(id, _)| id != file_id);
    s.push((file_id.to_string(), text.into()));
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The last thing said about a file: its availability, or how a download
/// or pin went.
pub fn status(file_id: &str) -> String {
    STATUS.lock().unwrap().iter().find(|(id, _)| id == file_id).map(|(_, t)| t.clone()).unwrap_or_default()
}

// ---- seeding -------------------------------------------------------------

static HOST: OnceLock<Arc<Mutex<FileHost>>> = OnceLock::new();
static SEEDER: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// The app's own store of sealed files, made on first use. Downloads are
/// served from it; nothing is ever uploaded to it from outside.
fn host() -> Option<Arc<Mutex<FileHost>>> {
    if let Some(h) = HOST.get() {
        return Some(h.clone());
    }
    let dir = crate::config::config_dir()?.join("seeds");
    let store = FileStore::open(&dir).ok()?;
    // "Contacts" scope: a saved contact, read fresh, so removing someone
    // stops serving them at once.
    let contacts: antiphon_relay::files::ContactCheck = Arc::new(|key: &[u8; 32]| {
        let wanted = hex(key);
        crate::config::load().contacts.iter().any(|c| c.identity_key.eq_ignore_ascii_case(&wanted))
    });
    let policy = HostPolicy { accept_uploads: false, ..Default::default() };
    let _ = HOST.set(Arc::new(Mutex::new(FileHost::new(store, policy, contacts))));
    HOST.get().cloned()
}

pub fn scope_from_index(i: i32) -> Scope {
    match i {
        1 => Scope::Contacts,
        2 => Scope::Anyone,
        _ => Scope::Conversation,
    }
}

/// Starts or stops answering for seeded files on `port`.
pub fn set_seeding(runtime: &tokio::runtime::Handle, on: bool, port: u16) {
    let mut seeder = SEEDER.lock().unwrap();
    if let Some(task) = seeder.take() {
        task.abort();
    }
    if !on {
        return;
    }
    let Some(host) = host() else { return };
    *seeder = Some(runtime.spawn(async move {
        let Ok(socket) = UdpSocket::bind(("0.0.0.0", port)).await else { return };
        let _ = antiphon_relay::run_node(
            Arc::new(socket),
            Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))),
            None,
            Some(host),
        )
        .await;
    }));
}

fn read_at(file: &mut std::fs::File, index: u32, size: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let start = index as u64 * CHUNK as u64;
    let len = size.saturating_sub(start).min(CHUNK as u64) as usize;
    let mut buf = vec![0u8; len];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Seals `path` under `key` and every chunk's hash, without keeping the
/// sealed copy: what the id needs. Sealing is deterministic per key and
/// chunk, so a second pass makes the same bytes.
fn hashes_of(path: &Path, key: &FileKey) -> std::io::Result<(u64, Vec<[u8; 32]>)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let mut hashes = Vec::new();
    for i in 0..file_seal::chunk_count(size) {
        hashes.push(file_seal::chunk_hash(&file_seal::seal_chunk(key, i, &read_at(&mut f, i, size)?)));
    }
    Ok((size, hashes))
}

/// Starts seeding the file at `path` under `key`. Returns its id. A file
/// downloaded with the same key gets the same id, so a receiver who seeds
/// joins the same swarm.
pub fn seed(path: &Path, key: &FileKey, scope: Scope) -> Result<(FileId, u64), String> {
    let host = host().ok_or("couldn't open the seed store")?;
    let (size, hashes) = hashes_of(path, key).map_err(|e| e.to_string())?;
    let id = file_seal::file_id(size, &hashes);
    let token = file_seal::read_token(key);
    host.lock().unwrap().store.begin(id, size, hashes, scope, Some(token), [0; 32], now_secs()).map_err(|e| e.to_string())?;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    for i in 0..file_seal::chunk_count(size) {
        let sealed = file_seal::seal_chunk(key, i, &read_at(&mut f, i, size).map_err(|e| e.to_string())?);
        // One lock per chunk, so a big file being sealed doesn't stop
        // the seeder answering for the others.
        host.lock().unwrap().store.put_chunk(&id, i, &sealed).map_err(|r| format!("{r:?}"))?;
    }
    Ok((id, size))
}

// ---- pinning -------------------------------------------------------------

/// What a pin node files your uploads under, for its quotas: your
/// signature over its name, hashed. Ed25519 signs deterministically, so it
/// is the same every time for that node, and different for every node.
fn uploader_label(node: &str) -> Option<[u8; 32]> {
    use sha2::Digest;
    let me = crate::delivery::identity()?;
    let mut msg = b"antiphon/uploader/v1".to_vec();
    msg.extend_from_slice(node.as_bytes());
    Some(sha2::Sha256::digest(me.sign(&msg)).into())
}

/// Uploads a sealed copy to each pin node. Returns the nodes that took it.
pub async fn pin(path: &Path, key: &FileKey, nodes: &[String], code: &str) -> (Vec<String>, Vec<String>) {
    let mut took = Vec::new();
    let mut problems = Vec::new();
    let Ok((size, hashes)) = hashes_of(path, key) else {
        return (took, vec!["couldn't read the file".into()]);
    };
    let id = file_seal::file_id(size, &hashes);
    let token = file_seal::read_token(key);
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { return (took, problems) };
    for node in nodes {
        let Some(addr) = crate::delivery::resolve(node).await else {
            problems.push(format!("{node}: can't find it"));
            continue;
        };
        let Some(uploader) = uploader_label(node) else { continue };
        let Ok(mut f) = std::fs::File::open(path) else { continue };
        let sealed = |i: u32| read_at(&mut f, i, size).ok().map(|plain| file_seal::seal_chunk(key, i, &plain));
        match antiphon_net::blob::put(&socket, addr, id, size, uploader, token, code.as_bytes(), &hashes, sealed, swarm::TIMEOUT).await {
            Ok(()) => took.push(addr.to_string()),
            Err(e) => problems.push(format!("{node}: {e}")),
        }
    }
    (took, problems)
}

// ---- the link ------------------------------------------------------------

/// Makes a file shareable: seeds it if seeding is on, pins it to your pin
/// nodes, and returns the link to send. `None` when there is nowhere it
/// would be held after the call.
pub fn share(runtime: &tokio::runtime::Handle, path: &Path, name: &str, with: &str) -> Option<crate::control::ControlMessage> {
    let config = crate::config::load();
    if !config.seeding && config.pin_nodes.is_empty() {
        return None;
    }
    let key = file_seal::new_key();
    let size = std::fs::metadata(path).ok()?.len();
    let id = if config.seeding {
        seed(path, &key, scope_from_index(config.seed_scope)).ok()?.0
    } else {
        file_seal::file_id(size, &hashes_of(path, &key).ok()?.1)
    };
    let pinned = if config.pin_nodes.is_empty() {
        Vec::new()
    } else {
        let (took, problems) = runtime.block_on(pin(path, &key, &config.pin_nodes, &config.pin_code));
        if !problems.is_empty() {
            set_status(&hex(&id), format!("Not pinned everywhere: {}", problems.join("; ")));
        }
        took
    };
    if !config.seeding && pinned.is_empty() {
        return None;
    }
    remember(SharedFile {
        name: name.to_string(),
        size,
        file_id: hex(&id),
        key: hex(&key),
        holders: pinned.clone(),
        with: with.to_string(),
        at_secs: now_secs(),
        mine: true,
    });
    Some(crate::control::ControlMessage::FileLink {
        name: name.to_string(),
        size,
        file_id: hex(&id),
        key: hex(&key),
        seed_port: if config.seeding { config.seed_port } else { 0 },
        holders: pinned,
    })
}

/// A link arrived. `seeder` is the sender's own address, when they're
/// seeding and the call is direct.
pub fn receive_link(name: String, size: u64, file_id: String, key: String, mut holders: Vec<String>, seeder: Option<SocketAddr>, with: String) {
    // Checked before keeping anything: a link whose key is not 32 bytes
    // could never be opened.
    if unhex32(&file_id).is_none() || unhex32(&key).is_none() {
        return;
    }
    holders.retain(|h| h.parse::<SocketAddr>().is_ok());
    holders.truncate(8);
    if let Some(s) = seeder {
        holders.insert(0, s.to_string());
    }
    let name: String = name.chars().filter(|c| !c.is_control() && *c != '/' && *c != '\\').take(200).collect();
    remember(SharedFile { name, size, file_id, key, holders, with, at_secs: now_secs(), mine: false });
}

fn addrs(file: &SharedFile) -> Vec<SocketAddr> {
    file.holders.iter().filter_map(|h| h.parse().ok()).collect()
}

/// Asks the holders of every listed file whether it's still there.
pub fn check_all(runtime: &tokio::runtime::Handle) {
    for file in load() {
        let (Some(id), Some(key)) = (unhex32(&file.file_id), unhex32(&file.key)) else { continue };
        runtime.spawn(async move {
            let a = swarm::availability(&addrs(&file), id, file_seal::read_token(&key)).await;
            set_status(&file.file_id, a.wording());
        });
    }
}

/// Downloads a listed file to `dest`, then seeds it too if seeding is on.
pub fn download(runtime: &tokio::runtime::Handle, file_id: &str, dest: PathBuf) {
    let Some(file) = load().into_iter().find(|f| f.file_id == file_id) else { return };
    let (Some(id), Some(key)) = (unhex32(&file.file_id), unhex32(&file.key)) else { return };
    let rt = runtime.clone();
    runtime.spawn(async move {
        let progress = Arc::new(Mutex::new(Progress::default()));
        let watch = {
            let progress = progress.clone();
            let fid = file.file_id.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                    let p = progress.lock().unwrap().clone();
                    if let Some(percent) = (p.done * 100).checked_div(p.total) {
                        set_status(&fid, format!("Downloading: {percent}% from {} source(s)", p.holders_used.max(1)));
                    }
                }
            })
        };
        let result = swarm::download(&addrs(&file), id, key, crate::delivery::identity(), &dest, progress).await;
        watch.abort();
        match result {
            Ok(()) => {
                set_status(&file.file_id, format!("Saved to {}", dest.display()));
                let config = crate::config::load();
                if config.seeding {
                    let scope = scope_from_index(config.seed_scope);
                    let _ = rt.spawn_blocking(move || seed(&dest, &key, scope)).await;
                }
            }
            Err(e) => {
                let a = swarm::availability(&addrs(&file), id, file_seal::read_token(&key)).await;
                let text = if a == Availability::Available { format!("Download failed: {e}") } else { a.wording().to_string() };
                set_status(&file.file_id, text);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeding_a_file_gives_the_id_the_whole_file_would() {
        let dir = std::env::temp_dir().join(format!("antiphon-sharing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.bin");
        let plain: Vec<u8> = (0..5000u32).map(|i| (i * 3) as u8).collect();
        std::fs::write(&path, &plain).unwrap();
        let key = file_seal::new_key();
        let (size, hashes) = hashes_of(&path, &key).unwrap();
        assert_eq!(size, 5000);
        assert_eq!(file_seal::file_id(size, &hashes), file_seal::seal_all(&key, &plain).0);
    }
}
