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
use antiphon_net::vpn::TrafficKind;
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

static NOTICE: Mutex<String> = Mutex::new(String::new());

/// Says something about sharing as a whole rather than one file -- for
/// now, why anonymous seeding refused to send anything.
fn set_notice(text: impl Into<String>) {
    *NOTICE.lock().unwrap() = text.into();
    REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The last thing said about sharing as a whole, or empty.
pub fn notice() -> String {
    NOTICE.lock().unwrap().clone()
}

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
    // "This community" scope: a member by our copy of its signed member
    // list, read fresh, so a ban stops serving them at once.
    let members: antiphon_relay::files::MemberCheck = Arc::new(crate::community::is_member);
    let _ = HOST.set(Arc::new(Mutex::new(FileHost::new(store, policy, contacts).with_members(members))));
    HOST.get().cloned()
}

pub fn scope_from_index(i: i32) -> Scope {
    match i {
        1 => Scope::Contacts,
        2 => Scope::Anyone,
        _ => Scope::Conversation,
    }
}

/// Whether this device answers for seeded files itself. With anonymous
/// seeding on it never does: answering from here would hand out the very
/// address anonymous seeding exists to hide, so the seeding is done by
/// pin nodes instead (see [`route`]).
pub fn seeds_from_here(config: &crate::config::AppConfig) -> bool {
    config.seeding && !config.anonymous_seeding
}

/// How shared-file traffic leaves this device (spec §5 "Anonymous seeding
/// through relays"): directly, or through an onion circuit over the hop
/// nodes set for anonymity mode.
///
/// With anonymous seeding on but fewer than two hop nodes this is an
/// error, never a quiet fallback to [`swarm::Route::Direct`]: someone who
/// asked not to be seen must not be shown because a setting was missing.
pub fn route(config: &crate::config::AppConfig) -> Result<swarm::Route, String> {
    if !config.anonymous_seeding {
        return Ok(swarm::Route::Direct);
    }
    if config.anonymity_hops.len() < antiphon_net::onion::MIN_HOPS {
        return Err("Anonymous seeding needs at least two hop nodes, set under Anonymity. Nothing was sent.".into());
    }
    Ok(swarm::Route::Anonymous(config.anonymity_hops.iter().take(antiphon_net::onion::MAX_HOPS).cloned().collect()))
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

/// Uploads a sealed copy to each pin node, along `route`. Returns the
/// nodes that took it (by their real address, which is what goes in a
/// link) and what went wrong with the rest.
///
/// Through a circuit, each node gets its own: the node sees the
/// circuit's last hop as the uploader, and the uploader label (a
/// signature over the node's name) is the only thing tying uploads
/// together -- and only at that one node, for its quotas.
pub async fn pin(path: &Path, key: &FileKey, nodes: &[String], code: &str, route: &swarm::Route) -> (Vec<String>, Vec<String>) {
    let mut took = Vec::new();
    let mut problems = Vec::new();
    let Ok((size, hashes)) = hashes_of(path, key) else {
        return (took, vec!["couldn't read the file".into()]);
    };
    let id = file_seal::file_id(size, &hashes);
    let token = file_seal::read_token(key);
    for node in nodes {
        let Some(addr) = crate::delivery::resolve(TrafficKind::Files, node).await else {
            problems.push(format!("{node}: can't find it"));
            continue;
        };
        let (socket, to) = match route {
            swarm::Route::Direct => match crate::vpn::bind(TrafficKind::Files, 0).await {
                Ok(s) => (Arc::new(s), addr),
                Err(e) => {
                    problems.push(format!("{node}: {e}"));
                    continue;
                }
            },
            swarm::Route::Anonymous(hops) => match crate::anonymity::route_to(hops, addr).await {
                Ok(link) => link,
                Err(e) => {
                    problems.push(format!("{node}: couldn't build an anonymous path: {e}"));
                    continue;
                }
            },
        };
        let Some(uploader) = uploader_label(node) else { continue };
        let Ok(mut f) = std::fs::File::open(path) else { continue };
        let sealed = |i: u32| read_at(&mut f, i, size).ok().map(|plain| file_seal::seal_chunk(key, i, &plain));
        match antiphon_net::blob::put(&*socket, to, id, size, uploader, token, code.as_bytes(), &hashes, sealed, swarm::TIMEOUT).await {
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
///
/// With anonymous seeding on, nothing is seeded from here and the link
/// carries no seeding port: only pin nodes, reached through circuits,
/// hold it -- so the link says nothing about where this device is.
pub fn share(runtime: &tokio::runtime::Handle, path: &Path, name: &str, with: &str) -> Option<crate::control::ControlMessage> {
    let config = crate::config::load();
    if !config.seeding && config.pin_nodes.is_empty() {
        return None;
    }
    let route = match route(&config) {
        Ok(r) => r,
        Err(e) => {
            set_notice(e);
            return None;
        }
    };
    let here = seeds_from_here(&config);
    let key = file_seal::new_key();
    let size = std::fs::metadata(path).ok()?.len();
    let id = if here {
        seed(path, &key, scope_from_index(config.seed_scope)).ok()?.0
    } else {
        file_seal::file_id(size, &hashes_of(path, &key).ok()?.1)
    };
    let pinned = if config.pin_nodes.is_empty() {
        Vec::new()
    } else {
        let (took, problems) = runtime.block_on(pin(path, &key, &config.pin_nodes, &config.pin_code, &route));
        if !problems.is_empty() {
            set_status(&hex(&id), format!("Not pinned everywhere: {}", problems.join("; ")));
        }
        took
    };
    if !here && pinned.is_empty() {
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
        seed_port: if here { config.seed_port } else { 0 },
        holders: pinned,
    })
}

/// Shares a file in a community channel (spec §5 "this community",
/// "server-hosted mode -- per-community toggle"). Seeded, if seeding is
/// on, to members only -- or to anyone, if that is your seeding scope --
/// and pinned to your own pin nodes and, when the community has switched
/// server-hosted mode on, to its file nodes with its members code.
/// Returns the link's parts: name, size, id, key, holders.
pub fn share_in_community(
    runtime: &tokio::runtime::Handle,
    path: &Path,
    name: &str,
    community: &antiphon_crypto::community::Manifest,
) -> Result<(String, u64, String, String, Vec<String>), String> {
    let config = crate::config::load();
    let hosted: Vec<&antiphon_crypto::community::NodeRef> =
        if community.server_hosted_files { community.nodes.files.iter().collect() } else { Vec::new() };
    if !config.seeding && config.pin_nodes.is_empty() && hosted.is_empty() {
        return Err("Nothing would hold it: turn on seeding or add a pin node under Shared files.".into());
    }
    let route = route(&config)?;
    let here = seeds_from_here(&config);
    let key = file_seal::new_key();
    let size = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    let scope = if config.seed_scope == 2 { Scope::Anyone } else { Scope::Community(community.id.0) };
    let id = if here {
        seed(path, &key, scope)?.0
    } else {
        file_seal::file_id(size, &hashes_of(path, &key).map_err(|e| e.to_string())?.1)
    };
    let mut holders = Vec::new();
    let mut problems = Vec::new();
    if !config.pin_nodes.is_empty() {
        let (took, p) = runtime.block_on(pin(path, &key, &config.pin_nodes, &config.pin_code, &route));
        holders.extend(took);
        problems.extend(p);
    }
    for node in hosted {
        let (took, p) = runtime.block_on(pin(path, &key, std::slice::from_ref(&node.addr), &node.code, &route));
        holders.extend(took);
        problems.extend(p);
    }
    if !problems.is_empty() {
        set_status(&hex(&id), format!("Not pinned everywhere: {}", problems.join("; ")));
    }
    if !here && holders.is_empty() {
        return Err(format!("No node took it: {}", problems.join("; ")));
    }
    remember(SharedFile {
        name: name.to_string(),
        size,
        file_id: hex(&id),
        key: hex(&key),
        holders: holders.clone(),
        with: format!("community:{}", hex(&community.id.0)),
        at_secs: now_secs(),
        mine: true,
    });
    Ok((name.to_string(), size, hex(&id), hex(&key), holders))
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
    let route = match route(&crate::config::load()) {
        Ok(r) => r,
        Err(e) => {
            set_notice(e);
            return;
        }
    };
    for file in load() {
        let (Some(id), Some(key)) = (unhex32(&file.file_id), unhex32(&file.key)) else { continue };
        let route = route.clone();
        runtime.spawn(async move {
            let a = swarm::availability(&addrs(&file), id, file_seal::read_token(&key), &route).await;
            set_status(&file.file_id, a.wording());
        });
    }
}

/// Downloads a listed file to `dest`, then seeds it too if seeding is on
/// -- from here, or, with anonymous seeding, by handing it to your pin
/// nodes through circuits, so you keep helping without being seen.
pub fn download(runtime: &tokio::runtime::Handle, file_id: &str, dest: PathBuf) {
    let Some(file) = load().into_iter().find(|f| f.file_id == file_id) else { return };
    let (Some(id), Some(key)) = (unhex32(&file.file_id), unhex32(&file.key)) else { return };
    let route = match route(&crate::config::load()) {
        Ok(r) => r,
        Err(e) => {
            set_status(&file.file_id, e);
            return;
        }
    };
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
        // A file from a community you're in under a separate identity is
        // claimed as that identity: the seeder is a member, and your main
        // key would tell them who you are (spec §7).
        let me = crate::community::identity_for_link(&file.with).or_else(crate::delivery::identity);
        let result = swarm::download(&addrs(&file), id, key, me, &dest, progress, &route).await;
        watch.abort();
        match result {
            Ok(()) => {
                set_status(&file.file_id, format!("Saved to {}", dest.display()));
                let config = crate::config::load();
                if seeds_from_here(&config) {
                    let scope = scope_from_index(config.seed_scope);
                    let _ = rt.spawn_blocking(move || seed(&dest, &key, scope)).await;
                } else if config.seeding && config.anonymous_seeding && !config.pin_nodes.is_empty() {
                    let (took, _) = pin(&dest, &key, &config.pin_nodes, &config.pin_code, &route).await;
                    if !took.is_empty() {
                        remember(SharedFile { holders: took, ..file.clone() });
                    }
                }
            }
            Err(e) => {
                let a = swarm::availability(&addrs(&file), id, file_seal::read_token(&key), &route).await;
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
    fn anonymous_seeding_never_falls_back_to_the_open() {
        let mut config = crate::config::AppConfig { seeding: true, ..Default::default() };
        assert_eq!(route(&config).unwrap(), swarm::Route::Direct);
        assert!(seeds_from_here(&config));

        // Asked for, but with one hop: refused, not sent directly.
        config.anonymous_seeding = true;
        config.anonymity_hops = vec!["127.0.0.1:1".into()];
        assert!(route(&config).is_err());
        // And this device stops answering for files itself either way.
        assert!(!seeds_from_here(&config));

        config.anonymity_hops = vec!["a:1".into(), "b:1".into(), "c:1".into(), "d:1".into()];
        assert_eq!(
            route(&config).unwrap(),
            swarm::Route::Anonymous(vec!["a:1".into(), "b:1".into(), "c:1".into()]),
            "at most three hops, as calls use"
        );
    }

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
