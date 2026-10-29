//! Bring your own VPN (spec §6): stored WireGuard configs, the running
//! tunnel, and which traffic goes through it.
//!
//! Configs live in their own file, sealed under the backup key like
//! history, never in `config.json`: a WireGuard file *is* a private key,
//! and settings export copies `config.json` verbatim. `config.json` only
//! says which stored config is active, which kinds of traffic use it and
//! whether the kill switch is on.
//!
//! Every app socket that can carry personal traffic asks [`bind`] for its
//! socket with the kind of traffic it is, and [`antiphon_net::vpn::Router`]
//! decides whether that is a direct socket, one inside the tunnel, or a
//! refusal because the tunnel is down and the kill switch is on. LAN-only
//! work (mailbox discovery by broadcast) and the server roles (holding
//! mail, seeding) stay direct: they answer whoever reaches this machine,
//! which a tunnel without a forwarded port would make nobody.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use antiphon_net::vpn::{Router, Routes, TrafficKind, Tunnel, TunnelStatus, WgConfig};
use antiphon_net::{AppSocket, NetError};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// One imported config.
#[derive(Serialize, Deserialize, Clone)]
pub struct StoredConfig {
    /// Stable id, so renaming or re-importing does not lose the selection.
    pub id: String,
    pub name: String,
    /// The wg-quick text as re-written by [`WgConfig::to_conf`] -- keys
    /// included, which is why the whole list is sealed.
    pub conf: String,
    pub imported_at: u64,
}

impl Drop for StoredConfig {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.conf);
    }
}

fn list_path(dir: &Path) -> PathBuf {
    dir.join("vpn-configs")
}

static LIST_LOCK: Mutex<()> = Mutex::new(());

fn load_from(dir: &Path) -> Vec<StoredConfig> {
    let Ok(key) = crate::history::load_or_create_backup_key(dir) else { return Vec::new() };
    let Ok(sealed) = std::fs::read(list_path(dir)) else { return Vec::new() };
    let Ok(plain) = key.open_content(&sealed) else { return Vec::new() };
    let plain = Zeroizing::new(plain);
    serde_json::from_slice(&plain).unwrap_or_default()
}

fn save_to(dir: &Path, list: &[StoredConfig]) -> Result<(), String> {
    let key = crate::history::load_or_create_backup_key(dir).map_err(|e| e.to_string())?;
    let json = Zeroizing::new(serde_json::to_vec(list).map_err(|e| e.to_string())?);
    let sealed = key.seal_content(&json).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(list_path(dir), sealed).map_err(|e| e.to_string())
}

/// The stored configs, newest last.
pub fn load() -> Vec<StoredConfig> {
    crate::config::config_dir().map(|d| load_from(&d)).unwrap_or_default()
}

fn import_into(dir: &Path, file_name: &str, text: &str) -> Result<StoredConfig, String> {
    let parsed = WgConfig::parse(text).map_err(|e| e.to_string())?;
    let _guard = LIST_LOCK.lock().unwrap();
    let mut list = load_from(dir);
    let mut id_bytes = [0u8; 8];
    getrandom::fill(&mut id_bytes).map_err(|e| e.to_string())?;
    let base = Path::new(file_name).file_stem().and_then(|s| s.to_str()).unwrap_or("VPN").trim().to_string();
    let base = if base.is_empty() { "VPN".to_string() } else { base };
    let mut name = base.clone();
    let mut n = 2;
    while list.iter().any(|c| c.name == name) {
        name = format!("{base} ({n})");
        n += 1;
    }
    let entry = StoredConfig {
        id: crate::delivery::hex(&id_bytes),
        name,
        conf: parsed.to_conf().to_string(),
        imported_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    list.push(entry.clone());
    save_to(dir, &list)?;
    Ok(entry)
}

/// Imports a wg-quick file. The file itself is left where it was; what is
/// stored is the parsed config, sealed.
pub fn import(path: &Path) -> Result<StoredConfig, String> {
    let dir = crate::config::config_dir().ok_or("no settings folder")?;
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > 64 * 1024 {
        return Err("that file is far too large to be a WireGuard config".into());
    }
    let text = Zeroizing::new(std::fs::read_to_string(path).map_err(|e| format!("couldn't read it: {e}"))?);
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("VPN");
    let entry = import_into(&dir, name, &text)?;
    bump();
    Ok(entry)
}

fn delete_from(dir: &Path, id: &str) -> Result<(), String> {
    let _guard = LIST_LOCK.lock().unwrap();
    let mut list = load_from(dir);
    list.retain(|c| c.id != id);
    save_to(dir, &list)
}

pub fn delete(id: &str) -> Result<(), String> {
    let dir = crate::config::config_dir().ok_or("no settings folder")?;
    delete_from(&dir, id)?;
    bump();
    Ok(())
}

/// A one-line summary of a stored config, for the list.
pub fn describe(entry: &StoredConfig) -> String {
    match WgConfig::parse(&entry.conf) {
        Ok(c) => {
            let full = c.routes("1.1.1.1".parse().unwrap());
            let pubkey = crate::delivery::hex(&c.public_key());
            format!(
                "{} · {} · your key {}…",
                c.endpoint,
                if full { "all traffic" } else { "split tunnel: only some addresses" },
                &pubkey[..8]
            )
        }
        Err(e) => format!("unreadable: {e}"),
    }
}

// ---- The running tunnel and the routing policy ----

struct Active {
    tunnel: Option<Arc<Tunnel>>,
    name: String,
    /// Why the last connect failed, if it did.
    error: Option<String>,
}

static ACTIVE: Mutex<Active> = Mutex::new(Active { tunnel: None, name: String::new(), error: None });
static ROUTER: RwLock<Option<Router>> = RwLock::new(None);
static POLICY: Mutex<(bool, Routes, bool)> = Mutex::new((false, Routes { voice: false, files: false, messages: false }, false));
static REVISION: AtomicU64 = AtomicU64::new(0);

fn bump() {
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Changes whenever the list or the tunnel's state does, so the UI knows
/// to redraw.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

fn rebuild_router() {
    let (enabled, routes, kill_switch) = *POLICY.lock().unwrap();
    let router = if enabled {
        Router::new(ACTIVE.lock().unwrap().tunnel.clone(), routes, kill_switch)
    } else {
        Router::direct()
    };
    *ROUTER.write().unwrap() = Some(router);
}

/// The router new sockets are made by.
pub fn router() -> Router {
    ROUTER.read().unwrap().clone().unwrap_or_default()
}

/// A socket for this kind of traffic; see [`Router::bind`].
pub async fn bind(kind: TrafficKind, port: u16) -> Result<AppSocket, NetError> {
    router().bind(kind, port).await
}

/// Resolves `host:port` the way this kind of traffic reaches it.
pub async fn resolve(kind: TrafficKind, host_port: &str) -> Result<std::net::SocketAddr, NetError> {
    router().resolve(kind, host_port).await
}

/// Applies the settings: whether the VPN is on, which config, which
/// traffic, and the kill switch. Starts or stops the tunnel as needed.
pub fn apply(runtime: &tokio::runtime::Handle, enabled: bool, active_id: &str, routes: Routes, kill_switch: bool) {
    *POLICY.lock().unwrap() = (enabled, routes, kill_switch);
    let wanted = if enabled { load().into_iter().find(|c| c.id == active_id) } else { None };
    let running_name = {
        let active = ACTIVE.lock().unwrap();
        active.tunnel.as_ref().filter(|t| t.is_running()).map(|_| active.name.clone())
    };
    match (wanted, running_name) {
        (Some(want), Some(name)) if want.name == name => rebuild_router(),
        (Some(want), _) => {
            disconnect_inner();
            rebuild_router();
            let conf = want.conf.clone();
            let name = want.name.clone();
            runtime.spawn(async move { connect(name, conf).await });
        }
        (None, _) => {
            disconnect_inner();
            if enabled && !active_id.is_empty() {
                ACTIVE.lock().unwrap().error = Some("the selected config is gone; import or pick another".into());
            }
            rebuild_router();
        }
    }
    bump();
}

fn disconnect_inner() {
    let mut active = ACTIVE.lock().unwrap();
    if let Some(t) = active.tunnel.take() {
        t.stop("turned off");
    }
    active.error = None;
}

async fn connect(name: String, conf: String) {
    {
        let mut active = ACTIVE.lock().unwrap();
        active.name = name.clone();
        active.error = None;
    }
    bump();
    let result = match WgConfig::parse(&conf) {
        Ok(config) => Tunnel::start(config).await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match result {
        Ok(tunnel) => {
            {
                let mut active = ACTIVE.lock().unwrap();
                // Settings may have moved on while this was resolving.
                if active.name != name {
                    tunnel.stop("replaced");
                    return;
                }
                active.tunnel = Some(tunnel.clone());
            }
            rebuild_router();
            bump();
            if !tunnel.wait_up(Duration::from_secs(10)).await {
                ACTIVE.lock().unwrap().error =
                    Some("the server hasn't answered yet -- check the file is current and the server is up".into());
            }
        }
        Err(e) => {
            ACTIVE.lock().unwrap().error = Some(e);
            rebuild_router();
        }
    }
    bump();
}

/// One line for the settings screen.
pub fn status_line() -> String {
    let (enabled, routes, kill_switch) = *POLICY.lock().unwrap();
    if !enabled {
        return "Off: everything goes over your normal connection.".into();
    }
    let active = ACTIVE.lock().unwrap();
    let kinds: Vec<&str> = [(routes.voice, "calls"), (routes.files, "shared files"), (routes.messages, "offline messages")]
        .into_iter()
        .filter_map(|(on, k)| on.then_some(k))
        .collect();
    let kinds = if kinds.is_empty() { "nothing".to_string() } else { kinds.join(", ") };
    let held = if kill_switch && !kinds.is_empty() {
        format!(" Kill switch is holding {kinds} until it's back.")
    } else if !kinds.is_empty() {
        format!(" {kinds} go over your normal connection meanwhile.")
    } else {
        String::new()
    };
    match active.tunnel.as_ref().map(|t| t.status()) {
        Some(TunnelStatus::Up { last_handshake }) if last_handshake < Duration::from_secs(200) => {
            format!("Connected to {} -- carrying {kinds}.", active.name)
        }
        Some(TunnelStatus::Up { .. }) => format!(
            "{}: the server stopped answering; still trying. Nothing falls back to your normal connection.",
            active.name
        ),
        Some(TunnelStatus::Connecting) => format!(
            "Connecting to {}...{}",
            active.name,
            active.error.as_deref().map(|e| format!(" ({e})")).unwrap_or_default()
        ),
        Some(TunnelStatus::Down(why)) => format!("Down ({why}).{held}"),
        None => match &active.error {
            Some(e) => format!("Couldn't connect: {e}.{held}"),
            None if active.name.is_empty() => format!("No config selected.{held}"),
            None => format!("Starting {}...", active.name),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONF: &str = "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.0.0.2/32\n\n[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.1:51820\n";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-vpn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn configs_are_sealed_on_disk_and_listed_back() {
        let dir = temp_dir("seal");
        let a = import_into(&dir, "mullvad-se.conf", CONF).unwrap();
        let b = import_into(&dir, "mullvad-se.conf", CONF).unwrap();
        assert_eq!(a.name, "mullvad-se");
        assert_eq!(b.name, "mullvad-se (2)");
        let raw = std::fs::read(list_path(&dir)).unwrap();
        let raw_text = String::from_utf8_lossy(&raw);
        assert!(!raw_text.contains("yAnz5TF"), "the private key is not on disk in the clear");
        assert!(!raw_text.contains("PrivateKey"));
        let listed = load_from(&dir);
        assert_eq!(listed.len(), 2);
        assert!(WgConfig::parse(&listed[0].conf).is_ok());
        delete_from(&dir, &a.id).unwrap();
        assert_eq!(load_from(&dir).iter().map(|c| c.id.clone()).collect::<Vec<_>>(), vec![b.id.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_file_is_refused_with_the_reason() {
        let dir = temp_dir("bad");
        let err = import_into(&dir, "x.conf", "[Interface]\nAddress = 10.0.0.2/32\n[Peer]\n").err().unwrap();
        assert!(err.contains("PrivateKey"), "{err}");
        assert!(load_from(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_never_carry_the_key() {
        let config = crate::config::AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.to_lowercase().contains("privatekey"));
        assert!(!config.vpn_enabled, "VPN is off by default");
        assert!(!config.vpn_kill_switch, "kill switch is off by default");
    }
}
