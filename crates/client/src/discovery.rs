//! Finding contacts again after their address changes (spec §4 "DHT
//! distributed lookup"), and the opt-in community directories ("Opt-in
//! directories for public communities only").
//!
//! With the DHT switched on, the app does three things, all through the
//! bootstrap nodes the user listed -- none ship by default, so nothing is
//! asked of anyone the user didn't choose:
//!
//! - **Publishes where it is now**: direct address, relay and route list,
//!   sealed into a [`DhtRecord`] for today and tomorrow (so a contact whose
//!   clock is past midnight still finds it). Again every
//!   [`REPUBLISH_INTERVAL`], and at once when the address changes -- a
//!   router handing back a different forwarded port on renewal included
//!   (spec §6).
//! - **Looks up contacts in the background** and updates a saved address
//!   when a newer signed record says it moved.
//! - **Looks up a contact on the spot** when a direct call to their saved
//!   address gets no answer, before falling back to their relay.
//!
//! What this exposes is written next to the switch: anyone who knows your
//! identity key -- which is in your invite link -- can look up your
//! current address, the same address the link itself already carries.
//! Storage nodes see neither who you are nor the address.
//!
//! The app is a DHT *client*: it never serves, so being on the DHT never
//! makes this machine a node strangers talk to.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use antiphon_crypto::dht_record::{self, DhtRecord, Whereabouts};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::dht::{self, ListingEntry, LookupOptions};
use tokio::net::UdpSocket;

use crate::config::Contact;

/// How often the record is republished when nothing changed. Records live
/// two days on a node; this keeps them fresh well inside that.
pub const REPUBLISH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// How often saved contacts are looked up in the background.
pub const LOOKUP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// How long a direct call to a saved address gets before the DHT is asked.
pub const DIRECT_TRY: Duration = Duration::from_secs(6);

/// What the app knows about how it can be reached, fed in from the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Local {
    pub listen_port: u16,
    /// The external port the router forwards, if a forward is live.
    pub mapped_port: Option<u16>,
    /// What STUN said, if the user asked.
    pub stun_public: Option<String>,
    pub relay: String,
}

/// The direct addresses to publish, best first.
///
/// A forwarded port at the public IP is the one that reliably works; a
/// STUN mapping works for as long as the NAT keeps it; the listen port at
/// the public IP works when there is no NAT. The public IP comes from
/// what a DHT node saw, so no STUN server is needed for it.
pub fn addresses(local: &Local, observed_ip: Option<IpAddr>) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(ip), Some(port)) = (observed_ip, local.mapped_port) {
        out.push(SocketAddr::new(ip, port).to_string());
    }
    if let Some(stun) = &local.stun_public {
        out.push(stun.clone());
    }
    if let Some(ip) = observed_ip {
        if local.listen_port != 0 {
            out.push(SocketAddr::new(ip, local.listen_port).to_string());
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|a| seen.insert(a.clone()));
    out
}

/// A strictly increasing sequence without a counter on disk: milliseconds
/// since the epoch, nudged past the last one used.
fn next_sequence() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut prev = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Publishes `whereabouts` as `identity`'s record for today and tomorrow.
/// Returns how many nodes took today's.
pub async fn publish(
    identity: &IdentityKeyPair,
    bootstrap: &[SocketAddr],
    whereabouts: &Whereabouts,
    unix_secs: u64,
    opts: LookupOptions,
) -> Result<usize, String> {
    let socket = UdpSocket::bind(unspecified_for(bootstrap)).await.map_err(|e| e.to_string())?;
    let today = dht_record::epoch(unix_secs);
    let sequence = next_sequence();
    let payload = whereabouts.encode();
    let mut stored_today = 0;
    for epoch in [today, today + 1] {
        let record = DhtRecord::seal(identity, epoch, sequence, &payload).map_err(|e| e.to_string())?;
        let stored = dht::publish(&socket, bootstrap, &record, opts).await.map_err(|e| e.to_string())?;
        if epoch == today {
            stored_today = stored;
        }
    }
    Ok(stored_today)
}

/// Looks `identity_key` up: today's key, then tomorrow's and yesterday's
/// for clocks either side of midnight. Only a record newer than
/// `known_sequence` counts.
pub async fn lookup(
    identity_key: &[u8; 32],
    bootstrap: &[SocketAddr],
    known_sequence: Option<u64>,
    unix_secs: u64,
    opts: LookupOptions,
) -> Option<(u64, Whereabouts)> {
    let socket = UdpSocket::bind(unspecified_for(bootstrap)).await.ok()?;
    let today = dht_record::epoch(unix_secs);
    for epoch in [today, today + 1, today.saturating_sub(1)] {
        let key = dht_record::lookup_key(identity_key, epoch).ok()?;
        let Ok(found) = dht::find(&socket, bootstrap, key, opts).await else { continue };
        if let Some((sequence, payload)) = dht_record::best(&found, identity_key, epoch, known_sequence) {
            if let Some(w) = Whereabouts::decode(&payload) {
                return Some((sequence, w));
            }
        }
    }
    None
}

fn unspecified_for(nodes: &[SocketAddr]) -> SocketAddr {
    if nodes.first().is_some_and(SocketAddr::is_ipv6) {
        SocketAddr::from(([0u16; 8], 0))
    } else {
        SocketAddr::from(([0, 0, 0, 0], 0))
    }
}

/// Applies a found record to a saved contact. Returns whether anything
/// changed. The record is already proven to be theirs; the route list
/// inside is checked again against its own signature and sequence, and
/// against the contact's devices in `known`.
pub fn apply(contact: &mut Contact, sequence: u64, found: &Whereabouts, known: &mut crate::contact_devices::ContactDevices) -> bool {
    let mut changed = false;
    if contact.dht_sequence.is_some_and(|s| sequence <= s) {
        return false;
    }
    contact.dht_sequence = Some(sequence);
    if let Some(address) = found.addresses.first() {
        if contact.address != *address {
            contact.address.clone_from(address);
            changed = true;
        }
    }
    if !found.relay.is_empty() && contact.relay != found.relay {
        contact.relay.clone_from(&found.relay);
        changed = true;
    }
    // Proved by one of their devices once their devices are known.
    if !found.route_record.is_empty() && crate::delivery::take_record(contact, &crate::delivery::hex(&found.route_record), known) {
        changed = true;
    }
    changed
}

/// Resolves the user's bootstrap list, `host[:port]` each.
async fn resolve_all(nodes: &[String]) -> Vec<SocketAddr> {
    let mut out = Vec::new();
    for n in nodes {
        if let Some(a) = crate::delivery::resolve(antiphon_net::vpn::TrafficKind::Voice, n.trim()).await {
            out.push(a);
        }
    }
    out
}

// ---- The background task ----

static IDENTITY: OnceLock<Arc<IdentityKeyPair>> = OnceLock::new();
static KICK: OnceLock<tokio::sync::Notify> = OnceLock::new();
static LOCAL: Mutex<Option<Local>> = Mutex::new(None);
static STATUS: Mutex<String> = Mutex::new(String::new());
static REVISION: AtomicU64 = AtomicU64::new(0);

/// Bumped when contacts or the status line changed, so the UI redraws.
pub fn revision() -> u64 {
    REVISION.load(Ordering::Relaxed)
}

pub fn status() -> String {
    STATUS.lock().unwrap().clone()
}

fn set_status(line: String) {
    *STATUS.lock().unwrap() = line;
    REVISION.fetch_add(1, Ordering::Relaxed);
}

/// Runs a round now rather than at the next interval.
pub fn kick() {
    KICK.get_or_init(tokio::sync::Notify::new).notify_one();
}

/// Called often from the UI with what it knows; republishes only when
/// something changed -- a new forwarded port, a new relay, a new STUN
/// answer.
pub fn set_local(local: Local) {
    let mut held = LOCAL.lock().unwrap();
    if held.as_ref() != Some(&local) {
        let first = held.is_none();
        *held = Some(local);
        if !first {
            kick();
        }
    }
}

/// Starts the background task. It idles while the DHT is off.
pub fn start(runtime: &tokio::runtime::Handle, identity: Arc<IdentityKeyPair>) {
    let _ = IDENTITY.set(identity);
    let kick = KICK.get_or_init(tokio::sync::Notify::new);
    runtime.spawn(async move {
        let mut published: Option<(Whereabouts, tokio::time::Instant)> = None;
        let mut looked_up: Option<tokio::time::Instant> = None;
        loop {
            round(&mut published, &mut looked_up).await;
            let _ = tokio::time::timeout(Duration::from_secs(60), kick.notified()).await;
        }
    });
}

async fn round(published: &mut Option<(Whereabouts, tokio::time::Instant)>, looked_up: &mut Option<tokio::time::Instant>) {
    let Some(identity) = IDENTITY.get().cloned() else { return };
    let config = crate::config::load();
    if !config.dht_enabled {
        *published = None;
        if !status().is_empty() {
            set_status(String::new());
        }
        return;
    }
    let bootstrap = resolve_all(&config.dht_bootstrap).await;
    if bootstrap.is_empty() {
        set_status("Add a bootstrap node to use the DHT.".into());
        return;
    }
    let opts = LookupOptions::default();
    let local = LOCAL.lock().unwrap().clone().unwrap_or_default();

    // Our public IP, as a node sees it.
    let observed = match UdpSocket::bind(unspecified_for(&bootstrap)).await {
        Ok(s) => {
            let mut ip = None;
            for node in &bootstrap {
                if let Some(a) = dht::observe(&s, *node, Duration::from_secs(2)).await {
                    ip = Some(a.ip());
                    break;
                }
            }
            ip
        }
        Err(_) => None,
    };
    let route_record = crate::delivery::my_record_bytes();
    let whereabouts = Whereabouts { addresses: addresses(&local, observed), relay: local.relay.clone(), route_record };
    let now = tokio::time::Instant::now();
    let due = match published {
        Some((w, at)) => *w != whereabouts || now.duration_since(*at) >= REPUBLISH_INTERVAL,
        None => true,
    };
    if due {
        match publish(&identity, &bootstrap, &whereabouts, now_secs(), opts).await {
            Ok(n) if n > 0 => {
                *published = Some((whereabouts.clone(), now));
                let at = whereabouts.addresses.first().cloned().unwrap_or_else(|| "no direct address".into());
                set_status(format!("Published where you are ({at}) to {n} node(s)."));
            }
            Ok(_) => set_status("The DHT answered but no node stored your record; will retry.".into()),
            Err(e) => set_status(format!("Couldn't reach the DHT: {e}")),
        }
    }

    if looked_up.is_some_and(|t| now.duration_since(t) < LOOKUP_INTERVAL) {
        return;
    }
    *looked_up = Some(now);
    let mut moved = Vec::new();
    for contact in config.contacts.iter().filter(|c| !c.identity_key.is_empty()).take(64) {
        let Some(key) = crate::connection_mode::decode_hex32(&contact.identity_key) else { continue };
        if let Some((sequence, found)) = lookup(&key, &bootstrap, contact.dht_sequence, now_secs(), opts).await {
            moved.push((contact.identity_key.clone(), sequence, found));
        }
    }
    if moved.is_empty() {
        return;
    }
    // Re-read before writing: the user may have changed something while
    // the lookups ran.
    let mut config = crate::config::load();
    let mut names = Vec::new();
    crate::delivery::with_contact_devices(|known| {
        for (key, sequence, found) in moved {
            if let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == key) {
                if apply(contact, sequence, &found, known) {
                    names.push(contact.name.clone());
                }
            }
        }
    });
    crate::config::save(&config);
    if !names.is_empty() {
        set_status(format!("Updated where to reach {}.", names.join(", ")));
    } else {
        REVISION.fetch_add(1, Ordering::Relaxed);
    }
}

/// Looks one contact up now, for a call whose saved address didn't
/// answer. Saves what it finds and returns their new direct address, if
/// it differs from `tried`.
pub async fn find_contact_now(identity_key_hex: &str, tried: &str) -> Option<String> {
    let config = crate::config::load();
    if !config.dht_enabled {
        return None;
    }
    let key = crate::connection_mode::decode_hex32(identity_key_hex)?;
    let bootstrap = resolve_all(&config.dht_bootstrap).await;
    if bootstrap.is_empty() {
        return None;
    }
    let known = config.contacts.iter().find(|c| c.identity_key == identity_key_hex).and_then(|c| c.dht_sequence);
    let opts = LookupOptions { overall_timeout: Duration::from_secs(6), ..Default::default() };
    // `known` is passed as None: an equal sequence is still the freshest
    // there is, and this is about where to call, not about replacing.
    let (sequence, found) = lookup(&key, &bootstrap, None, now_secs(), opts).await?;
    let mut config = crate::config::load();
    if let Some(contact) = config.contacts.iter_mut().find(|c| c.identity_key == identity_key_hex) {
        if known.is_none_or(|k| sequence > k) && crate::delivery::with_contact_devices(|devices| apply(contact, sequence, &found, devices)).unwrap_or(false) {
            crate::config::save(&config);
            REVISION.fetch_add(1, Ordering::Relaxed);
        }
    }
    found.addresses.into_iter().find(|a| a != tried)
}

// ---- Community directories ----

/// One listed community, with the directory it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    pub directory: String,
    pub entry: ListingEntry,
}

/// Reads every directory the user added. A directory's key is pinned the
/// first time it answers (`pins`, `address=hex`), and a directory that
/// later answers with a different key is reported rather than shown --
/// someone else is answering at that address.
pub async fn browse(directories: &[String], pins: &mut Vec<String>) -> (Vec<Listed>, Vec<String>) {
    let mut listed = Vec::new();
    let mut problems = Vec::new();
    for dir in directories {
        let Some(addr) = crate::delivery::resolve(antiphon_net::vpn::TrafficKind::Voice, dir.trim()).await else {
            problems.push(format!("{dir}: doesn't resolve"));
            continue;
        };
        let Ok(socket) = UdpSocket::bind(unspecified_for(&[addr])).await else { continue };
        match dht::fetch_listing(&socket, addr, Duration::from_secs(3)).await {
            Ok((signer, entries)) => {
                let hex = crate::delivery::hex(&signer);
                let prefix = format!("{}=", dir.trim());
                match pins.iter().find(|p| p.starts_with(&prefix)) {
                    Some(pin) if pin[prefix.len()..] != hex => {
                        problems.push(format!("{dir}: a different machine is answering than before -- not shown"));
                        continue;
                    }
                    Some(_) => {}
                    None => pins.push(format!("{prefix}{hex}")),
                }
                if entries.is_empty() {
                    problems.push(format!("{dir}: lists no communities"));
                }
                listed.extend(entries.into_iter().map(|entry| Listed { directory: dir.trim().to_string(), entry }));
            }
            Err(_) => problems.push(format!("{dir}: no answer (is it a directory node?)")),
        }
    }
    (listed, problems)
}

/// The listed communities whose name or description has every word of
/// `query` in it, ignoring case; all of them for an empty query.
pub fn search<'a>(listed: &'a [Listed], query: &str) -> Vec<&'a Listed> {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    listed
        .iter()
        .filter(|l| {
            let text = format!("{} {}", l.entry.name, l.entry.description).to_lowercase();
            words.iter().all(|w| text.contains(w.as_str()))
        })
        .collect()
}

/// Asks the directory at `directory` to list a community (an entry with
/// an empty link takes it off), signed by `identity`, which alone can
/// change it later. Says how it went.
pub async fn submit(directory: &str, entry: ListingEntry, identity: &IdentityKeyPair) -> Result<(), String> {
    let addr = crate::delivery::resolve(antiphon_net::vpn::TrafficKind::Voice, directory.trim()).await.ok_or_else(|| format!("{directory} doesn't resolve"))?;
    let socket = UdpSocket::bind(unspecified_for(&[addr])).await.map_err(|e| e.to_string())?;
    let key = identity.public().identity_key;
    match dht::submit_listing(&socket, addr, entry, key, |b| identity.sign(b), Duration::from_secs(4)).await {
        Ok(dht::StoreResult::Stored) => Ok(()),
        Ok(dht::StoreResult::Full) => Err("That directory is full, or has as many from your network as it takes".into()),
        Ok(_) => Err("That directory doesn't take submissions (its operator lists communities by hand)".into()),
        Err(_) => Err(format!("{directory} didn't answer (is it a directory node?)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_net::dht::{Config, DhtNode, NodeKey};

    #[test]
    fn a_forwarded_port_is_published_first_and_nothing_twice() {
        let ip: IpAddr = "203.0.113.5".parse().unwrap();
        let local = Local { listen_port: 4000, mapped_port: Some(51000), stun_public: Some("203.0.113.5:4000".into()), relay: String::new() };
        assert_eq!(addresses(&local, Some(ip)), ["203.0.113.5:51000", "203.0.113.5:4000"]);
        assert!(addresses(&Local::default(), None).is_empty());
    }

    #[test]
    fn a_found_record_updates_a_contact_once() {
        let mut c = Contact { name: "Ana".into(), address: "198.51.100.1:4000".into(), ..Default::default() };
        let w = Whereabouts { addresses: vec!["198.51.100.9:4000".into()], relay: "r.example:3479".into(), route_record: vec![] };
        assert!(apply(&mut c, 5, &w, &mut Default::default()));
        assert_eq!(c.address, "198.51.100.9:4000");
        assert_eq!(c.relay, "r.example:3479");
        let old = Whereabouts { addresses: vec!["192.0.2.1:1".into()], ..w.clone() };
        assert!(!apply(&mut c, 4, &old, &mut Default::default()), "an older record changes nothing");
        assert_eq!(c.address, "198.51.100.9:4000");
    }

    #[test]
    fn sequences_only_go_up() {
        let a = next_sequence();
        let b = next_sequence();
        assert!(b > a);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_contact_who_moved_is_found_through_real_nodes() {
        let mut nodes = Vec::new();
        for i in 0..6 {
            let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
            let config = Config { per_subnet_per_bucket: 64, per_subnet_total: 1024, ..Config::default() };
            let node = Arc::new(Mutex::new(DhtNode::new(NodeKey::generate(dht::POW_BITS), config)));
            if i > 0 {
                let out = node.lock().unwrap().bootstrap(&[nodes[0]], std::time::Instant::now());
                for (to, bytes) in out {
                    socket.send_to(&bytes, to).await.unwrap();
                }
            }
            nodes.push(socket.local_addr().unwrap());
            tokio::spawn(dht::serve(socket, node));
        }
        tokio::time::sleep(Duration::from_millis(600)).await;

        let ana = IdentityKeyPair::generate();
        let me = IdentityKeyPair::generate();
        let route = crate::delivery::my_record(&ana, &crate::delivery::default_routes(), false, 3);
        let w = Whereabouts { addresses: vec!["198.51.100.7:4000".into()], relay: String::new(), route_record: route.encode() };
        let now = now_secs();
        assert!(publish(&ana, &nodes[1..2], &w, now, LookupOptions::default()).await.unwrap() > 0);

        let key = ana.public().identity_key;
        let (sequence, found) = lookup(&key, &nodes[4..5], None, now, LookupOptions::default()).await.unwrap();
        assert_eq!(found, w);
        let mut contact = Contact {
            name: "Ana".into(),
            address: "192.0.2.1:4000".into(),
            identity_key: crate::delivery::hex(&key),
            ..Default::default()
        };
        assert!(apply(&mut contact, sequence, &found, &mut Default::default()));
        assert_eq!(contact.address, "198.51.100.7:4000");
        assert_eq!(contact.route_sequence, Some(3), "the route list inside came along");
        // Someone else can't find her under their own key.
        assert!(lookup(&me.public().identity_key, &nodes[4..5], None, now, LookupOptions::default()).await.is_none());
        // And a record we already have isn't "found" again.
        assert!(lookup(&key, &nodes[2..3], Some(sequence), now, LookupOptions::default()).await.is_none());
    }

    #[test]
    fn a_listing_is_searched_by_every_word() {
        let listed: Vec<Listed> = [("Chess club", "Thursdays in Leeds"), ("Choir", "Sunday singing"), ("Leeds runners", "")]
            .iter()
            .map(|(n, d)| Listed { directory: "d".into(), entry: ListingEntry { name: (*n).into(), description: (*d).into(), invite: String::new() } })
            .collect();
        let names = |q: &str| search(&listed, q).iter().map(|l| l.entry.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("leeds"), ["Chess club", "Leeds runners"]);
        assert_eq!(names("LEEDS chess"), ["Chess club"]);
        assert_eq!(names("").len(), 3);
    }
}
