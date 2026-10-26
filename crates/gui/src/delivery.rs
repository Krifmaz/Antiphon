//! Delivery routes (spec §8): getting a message to someone who isn't
//! online right now.
//!
//! **The recipient decides where their mail goes, not the sender.** Your
//! route list is your inbox configuration. It is signed into a
//! [`SignedRouteRecord`] and handed to each contact on every call, and a
//! sender only ever deposits where the *recipient's* record says.
//!
//! The list is walked top to bottom:
//!
//! 1. **Direct** -- straight to them while you're both online. Nothing
//!    stored anywhere. Always first; it is the live call.
//! 2. **My other devices** -- a desktop of yours that holds mail for your
//!    other devices ([`own_device_code`] keeps strangers out of it).
//! 3. **A node you trust** -- a machine you or a friend runs.
//! 4. **Community node** -- run by a community, joined with its code.
//! 5. **Project relay** -- the fallback.
//!
//! Every node holds only sealed blobs ([`antiphon_crypto::mail`]) under a
//! label derived per node, so no node can read a message, see who sent
//! it, or match its inbox up with another node's.
//!
//! **Strict mode** publishes Direct alone: nothing is ever stored, and
//! messages move only when both people are online. **First available**
//! deposits at one node, the least metadata; **fan out** drops the same
//! sealed message at every node, and the recipient keeps whichever copy
//! arrives first.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use antiphon_crypto::{Route, RouteRecord, SignedRouteRecord};
use antiphon_crypto::{mail, IdentityKeyPair};
use antiphon_net::mailbox::{self, InboxStatus, Label};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::net::UdpSocket;

/// How long a single node gets to answer before the next is tried.
pub const NODE_TIMEOUT: Duration = Duration::from_secs(3);

/// The default port a mailbox node listens on (the relay's).
pub const DEFAULT_PORT: u16 = 3479;

/// The five tiers, in the spec's default order.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteKind {
    Direct,
    OwnDevices,
    TrustedNode,
    CommunityNode,
    ProjectRelay,
}

impl RouteKind {
    /// The string a route record carries (see `antiphon_crypto::route`).
    pub fn wire(self) -> &'static str {
        match self {
            RouteKind::Direct => "direct",
            RouteKind::OwnDevices => "own-device",
            RouteKind::TrustedNode => "trusted-node",
            RouteKind::CommunityNode => "community-node",
            RouteKind::ProjectRelay => "project-relay",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            RouteKind::Direct => "Direct",
            RouteKind::OwnDevices => "My other devices",
            RouteKind::TrustedNode => "A node you trust",
            RouteKind::CommunityNode => "Community node",
            RouteKind::ProjectRelay => "Project relay",
        }
    }

    /// The spec's in-app description, word for word.
    pub fn description(self) -> &'static str {
        match self {
            RouteKind::Direct => "Straight to their device, if they're online right now. Nothing is stored anywhere.",
            RouteKind::OwnDevices => "Your desktop or tablet holds messages while your phone is asleep, then hands them over when it wakes up.",
            RouteKind::TrustedNode => "A machine you or a friend runs — a home server, a spare PC, a Raspberry Pi. It holds sealed messages it can't read.",
            RouteKind::CommunityNode => "The community you're talking in holds it for you. Same sealed storage, run by the people you're already talking to.",
            RouteKind::ProjectRelay => "Our fallback. Used only when nothing above is available, and it holds no more information than any other node.",
        }
    }
}

/// One route in your list.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouteConfig {
    pub kind: RouteKind,
    pub enabled: bool,
    /// `host:port` of each node on this route. Empty for Direct.
    #[serde(default)]
    pub nodes: Vec<String>,
    /// A community node's members code, if it serves only its own.
    #[serde(default)]
    pub code: String,
}

/// The spec's default order, every route on. Only Direct works until a
/// node is added, so a fresh install stores nothing anywhere.
pub fn default_routes() -> Vec<RouteConfig> {
    [RouteKind::Direct, RouteKind::OwnDevices, RouteKind::TrustedNode, RouteKind::CommunityNode, RouteKind::ProjectRelay]
        .into_iter()
        .map(|kind| RouteConfig { kind, enabled: true, nodes: Vec::new(), code: String::new() })
        .collect()
}

/// Puts back any tier missing from a saved list (an older config, or a
/// hand edit), so the list always shows all five.
pub fn normalize(routes: &mut Vec<RouteConfig>) {
    for d in default_routes() {
        if !routes.iter().any(|r| r.kind == d.kind) {
            routes.push(d);
        }
    }
    let mut seen = Vec::new();
    routes.retain(|r| {
        let fresh = !seen.contains(&r.kind);
        seen.push(r.kind);
        fresh
    });
}

/// The members code a device of yours uses to register at your own
/// mailbox, derived from your seed so every device of yours knows it and
/// nobody else does.
pub fn own_device_code(identity: &IdentityKeyPair) -> Vec<u8> {
    let mut out = [0u8; 16];
    Hkdf::<Sha256>::new(Some(b"antiphon/own-device-mailbox/v1"), &identity.master_seed())
        .expand(b"code", &mut out)
        .expect("16 bytes is a valid HKDF output length");
    out.to_vec()
}

/// Every (route, node) you want mail held at, in order. Empty in strict
/// mode.
pub fn inbox_nodes(routes: &[RouteConfig], strict: bool) -> Vec<(RouteKind, String, Vec<u8>)> {
    if strict {
        return Vec::new();
    }
    routes
        .iter()
        .filter(|r| r.enabled && r.kind != RouteKind::Direct)
        .flat_map(|r| {
            r.nodes
                .iter()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty())
                .map(move |n| (r.kind, n, r.code.clone().into_bytes()))
        })
        .collect()
}

/// Your route list, signed, ready to hand to a contact.
pub fn my_record(identity: &IdentityKeyPair, routes: &[RouteConfig], strict: bool, sequence: u64) -> SignedRouteRecord {
    let mut out = Vec::new();
    if strict || routes.iter().any(|r| r.kind == RouteKind::Direct && r.enabled) {
        out.push(Route { kind: RouteKind::Direct.wire().to_string(), address: String::new() });
    }
    for (kind, node, _) in inbox_nodes(routes, strict) {
        let label = mail::inbox_label(identity, &node);
        out.push(Route { kind: kind.wire().to_string(), address: format!("{node}#{}", hex(&label)) });
    }
    out.truncate(32);
    RouteRecord {
        identity_key: identity.public().identity_key,
        sequence,
        timestamp_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        routes: out,
    }
    .sign(identity)
    .expect("signing our own record cannot fail")
}

/// Where in a recipient's record mail can be left: `(node, label)`, in
/// their order. Direct and anything unparseable are skipped.
pub fn mailboxes_in(record: &SignedRouteRecord) -> Vec<(String, Label)> {
    record
        .record
        .routes
        .iter()
        .filter(|r| r.kind != RouteKind::Direct.wire())
        .filter_map(|r| {
            let (node, label) = r.address.rsplit_once('#')?;
            Some((node.to_string(), unhex32(label)?))
        })
        .collect()
}

/// What travels inside the seal.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MailBody {
    pub id: u64,
    pub text: String,
    pub at_secs: u64,
}

/// How a message went out.
#[derive(Clone, Debug, PartialEq)]
pub enum Sent {
    /// Left at this many nodes.
    Deposited(usize),
    /// Every node refused or didn't answer; the reason from the last.
    Failed(String),
    /// The recipient publishes no mailbox, or strict mode is on: it waits
    /// for a direct connection.
    NoRoute,
}

/// Leaves one message at the recipient's mailboxes.
pub async fn send(
    identity: &IdentityKeyPair,
    their_identity: &[u8; 32],
    their_prekey: &[u8; 32],
    their_record: &SignedRouteRecord,
    body: &MailBody,
    fan_out: bool,
) -> Sent {
    let boxes = mailboxes_in(their_record);
    if boxes.is_empty() {
        return Sent::NoRoute;
    }
    let plain = serde_json::to_vec(body).expect("a mail body always serializes");
    let Ok(blob) = mail::seal(identity, their_identity, their_prekey, &plain) else {
        return Sent::Failed("could not seal the message".into());
    };
    if blob.len() > mailbox::MAX_BLOB {
        return Sent::Failed("too long for a mailbox; it will go when you're both online".into());
    }
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return Sent::Failed("no network".into());
    };
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    let mut deposited = 0;
    let mut last_error = String::from("no mailbox answered");
    for (node, label) in boxes {
        let Some(addr) = resolve(&node).await else {
            last_error = format!("{node}: could not find that address");
            continue;
        };
        match mailbox::deposit(&socket, addr, label, id, &blob, NODE_TIMEOUT).await {
            Ok(()) => {
                deposited += 1;
                if !fan_out {
                    break;
                }
            }
            Err(e) => last_error = format!("{node}: {e}"),
        }
    }
    if deposited > 0 {
        Sent::Deposited(deposited)
    } else {
        Sent::Failed(last_error)
    }
}

/// One message collected from a mailbox.
pub struct Received {
    pub from: [u8; 32],
    pub body: MailBody,
}

/// Registers at every node on your list, then collects and opens what is
/// waiting. `keep` is asked about each message and stores it; only what
/// it accepts is acknowledged (deleted from the node), so a failed write
/// leaves the mail to be collected again.
///
/// `hosting` is set on the device that holds mail for your others: there,
/// mail on the "My other devices" route is read but left in place, so
/// your other devices still get it when they wake. It expires on its own.
pub async fn collect(
    identity: &IdentityKeyPair,
    routes: &[RouteConfig],
    strict: bool,
    hosting: bool,
    mut keep: impl FnMut(&Received) -> bool,
) -> Vec<(String, Result<NodeStatus, String>)> {
    let mut report = Vec::new();
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { return report };
    for (kind, node, code) in inbox_nodes(routes, strict) {
        let leave = hosting && kind == RouteKind::OwnDevices;
        let status = collect_one(identity, &socket, &node, &code, leave, &mut keep).await;
        report.push((node, status));
    }
    report
}

/// The live per-node status the route list shows.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NodeStatus {
    pub response_ms: u32,
    pub inbox: InboxStatus,
    pub collected: usize,
}

async fn collect_one(
    identity: &IdentityKeyPair,
    socket: &UdpSocket,
    node: &str,
    code: &[u8],
    leave_in_place: bool,
    keep: &mut impl FnMut(&Received) -> bool,
) -> Result<NodeStatus, String> {
    let addr = resolve(node).await.ok_or("could not find that address")?;
    let label = mail::inbox_label(identity, node);
    let secret = mail::fetch_secret(identity, node);
    let started = Instant::now();
    mailbox::register(socket, addr, label, mail::fetch_hash(&secret), code, NODE_TIMEOUT)
        .await
        .map_err(|e| e.to_string())?;
    let response_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let waiting = mailbox::fetch(socket, addr, label, secret, NODE_TIMEOUT).await.map_err(|e| e.to_string())?;
    let mut collected = 0;
    for (id, blob) in waiting {
        let accepted = match mail::open(identity, &blob) {
            Ok((sender, plain)) => match serde_json::from_slice::<MailBody>(&plain) {
                Ok(body) => keep(&Received { from: sender.identity_key, body }),
                // Opened and authenticated but not ours to parse: drop it
                // rather than collect it forever.
                Err(_) => true,
            },
            // Not sealed to us. Nobody can ever open it, so holding it
            // only fills the inbox.
            Err(_) => true,
        };
        if accepted {
            collected += 1;
            if !leave_in_place {
                let _ = mailbox::ack(socket, addr, label, secret, id, NODE_TIMEOUT).await;
            }
        }
    }
    let inbox = mailbox::status(socket, addr, label, secret, NODE_TIMEOUT).await.unwrap_or_default();
    Ok(NodeStatus { response_ms, inbox, collected })
}

/// Mailbox nodes on the local network (spec §8 "auto-detect on your
/// local network"): a broadcast that any node answers, holding mail or
/// not -- a relay without a mailbox says so, and is left out.
pub async fn discover(wait: Duration) -> Vec<SocketAddr> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { return Vec::new() };
    if socket.set_broadcast(true).is_err() {
        return Vec::new();
    }
    let probe = mailbox::Message::Status { label: [0; 32], secret: [0; 32] }.encode();
    let _ = socket.send_to(&probe, (std::net::Ipv4Addr::BROADCAST, DEFAULT_PORT)).await;
    let mut found = Vec::new();
    let deadline = tokio::time::Instant::now() + wait;
    let mut buf = [0u8; 64];
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        if let Some(mailbox::Message::Refused(r)) = mailbox::Message::decode(&buf[..n]) {
            if r != mailbox::Refusal::Disabled && !found.contains(&from) {
                found.push(from);
            }
        }
    }
    found
}

pub(crate) async fn resolve(node: &str) -> Option<SocketAddr> {
    let with_port = if node.contains(':') { node.to_string() } else { format!("{node}:{DEFAULT_PORT}") };
    tokio::net::lookup_host(with_port).await.ok()?.next()
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex32(s: &str) -> Option<[u8; 32]> {
    crate::connection_mode::decode_hex32(s)
}

pub fn encode_record(record: &SignedRouteRecord) -> String {
    hex(&record.encode())
}

/// A contact's record from what they sent, checked: it must be signed by
/// the key we know them by, and newer than the one already held.
pub fn accept_record(encoded: &str, their_identity: &[u8; 32], known_sequence: Option<u64>) -> Option<SignedRouteRecord> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Vec<u8> =
        (0..encoded.len()).step_by(2).map(|i| u8::from_str_radix(encoded.get(i..i + 2)?, 16).ok()).collect::<Option<_>>()?;
    let record = SignedRouteRecord::decode(&bytes).ok()?;
    if record.record.identity_key != *their_identity {
        return None;
    }
    record.accept(known_sequence).ok()?;
    Some(record)
}

// ---- The background courier: the app side of all of the above. ----

use std::sync::{Arc, Mutex, OnceLock};

static IDENTITY: OnceLock<Arc<IdentityKeyPair>> = OnceLock::new();
static KICK: OnceLock<tokio::sync::Notify> = OnceLock::new();
static NOTICES: Mutex<Vec<String>> = Mutex::new(Vec::new());
static NODE_REPORT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
static HOSTED: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);
/// Bumped after every courier round, so the UI knows to redraw.
static ROUND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Which courier round last finished. The UI redraws when this moves.
pub fn rounds() -> u64 {
    ROUND.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn decode_identity(hex: &str) -> Option<[u8; 32]> {
    crate::connection_mode::decode_hex32(hex)
}

/// How often mail is collected and the outbox retried when nothing
/// prompts it sooner.
pub const COURIER_INTERVAL: Duration = Duration::from_secs(60);

pub fn set_identity(identity: Arc<IdentityKeyPair>) {
    let _ = IDENTITY.set(identity);
}

pub fn identity() -> Option<Arc<IdentityKeyPair>> {
    IDENTITY.get().cloned()
}

/// Your current route list, signed and hex-encoded for a `Hello`.
pub fn my_record_hex() -> String {
    let Some(identity) = IDENTITY.get() else { return String::new() };
    let config = crate::config::load();
    encode_record(&my_record(identity, &config.routes, config.strict_mode, config.route_sequence))
}

/// Run the courier now rather than at its next tick -- after a call
/// ends, or the route list changes.
pub fn kick() {
    KICK.get_or_init(tokio::sync::Notify::new).notify_one();
}

/// Lines for the user since last asked: mail that arrived, mail that left.
pub fn take_notices() -> Vec<String> {
    std::mem::take(&mut *NOTICES.lock().unwrap())
}

/// The live per-node status, one line per node, from the last round.
pub fn node_report() -> Vec<(String, String)> {
    NODE_REPORT.lock().unwrap().clone()
}

fn notice(line: String) {
    let mut n = NOTICES.lock().unwrap();
    n.push(line);
    let excess = n.len().saturating_sub(20);
    n.drain(..excess);
}

/// Starts or stops this device holding mail for your other devices.
pub fn set_hosting(runtime: &tokio::runtime::Handle, on: bool, port: u16) {
    let mut hosted = HOSTED.lock().unwrap();
    if let Some(task) = hosted.take() {
        task.abort();
    }
    let Some(identity) = IDENTITY.get().cloned() else { return };
    if !on {
        return;
    }
    *hosted = Some(runtime.spawn(async move {
        use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
        let socket = match UdpSocket::bind(("0.0.0.0", port)).await {
            Ok(s) => Arc::new(s),
            Err(e) => {
                notice(format!("Couldn't hold mail for your devices on port {port}: {e}"));
                return;
            }
        };
        // Only your own devices can register here: the code comes from
        // your seed. Anyone may still deposit to them, as at any node.
        let mailbox = Mailbox::new(MailboxLimits {
            enabled: true,
            member_code: Some(own_device_code(&identity)),
            ..Default::default()
        });
        let _ = antiphon_relay::run_node(
            socket,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))),
            Some(Arc::new(Mutex::new(mailbox))),
            None,
        )
        .await;
    }));
}

/// Starts the courier. `in_call_with` names whoever a live call is with:
/// their outbox is that call's to send, not the courier's.
pub fn start(runtime: &tokio::runtime::Handle, in_call_with: impl Fn() -> Option<[u8; 32]> + Send + Sync + 'static) {
    let kick = KICK.get_or_init(tokio::sync::Notify::new);
    runtime.spawn(async move {
        loop {
            round(&in_call_with).await;
            ROUND.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let _ = tokio::time::timeout(COURIER_INTERVAL, kick.notified()).await;
        }
    });
}

async fn round(in_call_with: &(impl Fn() -> Option<[u8; 32]> + Send + Sync)) {
    let Some(identity) = IDENTITY.get().cloned() else { return };
    let config = crate::config::load();
    let Some(dir) = crate::config::config_dir() else { return };
    let Ok(key) = crate::history::load_or_create_backup_key(&dir) else { return };

    // Collect first, so what arrived is on screen before anything leaves.
    let contacts = config.contacts.clone();
    let busy = in_call_with().map(|k| hex(&k));
    let mut arrived: Vec<String> = Vec::new();
    let mut strangers = 0;
    let report = collect(&identity, &config.routes, config.strict_mode, config.hold_mail_for_devices, |r| {
        let hex_key = hex(&r.from);
        let Some(contact) = contacts.iter().find(|c| c.identity_key == hex_key) else {
            // Not someone you saved. Dropped, as a call from a stranger
            // never rings through the courier either.
            strangers += 1;
            return true;
        };
        // On a call with them right now: that call owns their history
        // file, so the mail waits at the node until it ends.
        if busy.as_deref() == Some(hex_key.as_str()) {
            return false;
        }
        let mut log = crate::history::load(&dir, &hex_key, &key);
        if log.iter().any(|m| !m.from_me && m.id == r.body.id && m.text == r.body.text) {
            return true; // a fan-out copy, or one already collected
        }
        log.push(crate::history::StoredMessage {
            id: r.body.id,
            from_me: false,
            text: r.body.text.clone(),
            at_secs: r.body.at_secs,
            edited: false,
            deleted: false,
            pinned: false,
            expires_at_secs: contact.expire_after_secs.map(|ttl| r.body.at_secs + ttl),
        });
        if crate::history::save(&dir, &hex_key, &key, &log).is_err() {
            return false; // leave it at the node and try again
        }
        arrived.push(contact.name.clone());
        true
    })
    .await;
    *NODE_REPORT.lock().unwrap() = report
        .into_iter()
        .map(|(node, status)| {
            let line = match status {
                Ok(s) => format!(
                    "reachable · {} ms · {} waiting · {} of {} KB used",
                    s.response_ms,
                    s.inbox.held,
                    s.inbox.held_bytes.div_ceil(1024),
                    s.inbox.cap_bytes / 1024
                ),
                Err(e) => format!("unreachable: {e}"),
            };
            (node, line)
        })
        .collect();
    for name in dedupe_counts(&arrived) {
        notice(name);
    }
    if strangers > 0 {
        notice(format!("{strangers} message(s) from people not in your contacts were discarded."));
    }

    // Then the outbox, for everyone not on a live call.
    if config.strict_mode {
        return;
    }
    for contact in &config.contacts {
        if contact.message_route != 0 || busy.as_deref() == Some(contact.identity_key.as_str()) {
            continue;
        }
        let (Some(their_key), Some(their_prekey)) = (
            crate::connection_mode::decode_hex32(&contact.identity_key),
            crate::connection_mode::decode_hex32(&contact.prekey),
        ) else {
            continue;
        };
        let Some(record) = accept_record(&contact.route_record, &their_key, None) else { continue };
        let mut queue = crate::outbox::load(&dir, &contact.identity_key, &key);
        if queue.is_empty() {
            continue;
        }
        let mut left = 0;
        let mut failure = None;
        for message in queue.clone() {
            let body = MailBody { id: message.id, text: message.text.clone(), at_secs: message.queued_at_secs };
            match send(&identity, &their_key, &their_prekey, &record, &body, config.route_fan_out).await {
                Sent::Deposited(_) => {
                    crate::outbox::mark_delivered(&mut queue, message.id);
                    left += 1;
                }
                Sent::NoRoute => break,
                Sent::Failed(why) => {
                    if let Some(q) = queue.iter_mut().find(|q| q.id == message.id) {
                        q.attempts += 1;
                    }
                    failure = Some(why);
                    break;
                }
            }
        }
        let _ = crate::outbox::save(&dir, &contact.identity_key, &key, &queue);
        if left > 0 {
            notice(format!("Left {left} message(s) for {} at their mailbox.", contact.name));
        }
        if let Some(why) = failure {
            notice(format!("Couldn't leave a message for {}: {why}", contact.name));
        }
    }
}

fn dedupe_counts(names: &[String]) -> Vec<String> {
    let mut out: Vec<(String, usize)> = Vec::new();
    for n in names {
        match out.iter_mut().find(|(m, _)| m == n) {
            Some((_, c)) => *c += 1,
            None => out.push((n.clone(), 1)),
        }
    }
    out.into_iter()
        .map(|(n, c)| if c == 1 { format!("New message from {n}.") } else { format!("{c} new messages from {n}.") })
        .collect()
}

/// Queues a message to a contact who isn't on a call with you, and asks
/// the courier to try now. Recorded in your history straight away, like
/// any message you send.
pub fn leave_message(contact_key_hex: &str, text: &str, expire_after_secs: Option<u64>) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Write something first.".into());
    }
    let dir = crate::config::config_dir().ok_or("no config directory")?;
    let key = crate::history::load_or_create_backup_key(&dir).map_err(|e| e.to_string())?;
    let mut log = crate::history::load(&dir, contact_key_hex, &key);
    let id = log.iter().map(|m| m.id).max().map(|m| m + 1).unwrap_or(0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    log.push(crate::history::StoredMessage {
        id,
        from_me: true,
        text: text.to_string(),
        at_secs: now,
        edited: false,
        deleted: false,
        pinned: false,
        expires_at_secs: expire_after_secs.map(|t| now + t),
    });
    crate::history::save(&dir, contact_key_hex, &key, &log).map_err(|e| e.to_string())?;
    let mut queue = crate::outbox::load(&dir, contact_key_hex, &key);
    crate::outbox::enqueue(
        &mut queue,
        crate::outbox::Queued { id, text: text.to_string(), queued_at_secs: now, attempts: 0 },
    );
    crate::outbox::save(&dir, contact_key_hex, &key, &queue).map_err(|e| e.to_string())?;
    kick();
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn with_nodes() -> Vec<RouteConfig> {
        let mut r = default_routes();
        r[2].nodes = vec!["home.example:3479".into()];
        r[3].nodes = vec!["guild.example:3479".into()];
        r[3].code = "guild".into();
        r
    }

    #[test]
    fn the_defaults_are_the_spec_order_and_store_nothing() {
        let r = default_routes();
        let kinds: Vec<_> = r.iter().map(|r| r.kind.title()).collect();
        assert_eq!(kinds, ["Direct", "My other devices", "A node you trust", "Community node", "Project relay"]);
        assert!(inbox_nodes(&r, false).is_empty());
    }

    #[test]
    fn strict_mode_publishes_direct_only() {
        let me = IdentityKeyPair::generate();
        let record = my_record(&me, &with_nodes(), true, 1);
        assert_eq!(record.record.routes.len(), 1);
        assert_eq!(record.record.routes[0].kind, "direct");
        assert!(mailboxes_in(&record).is_empty());
    }

    #[test]
    fn a_record_lists_mailboxes_in_order_with_per_node_labels() {
        let me = IdentityKeyPair::generate();
        let record = my_record(&me, &with_nodes(), false, 1);
        let boxes = mailboxes_in(&record);
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].0, "home.example:3479");
        assert_eq!(boxes[0].1, mail::inbox_label(&me, "home.example:3479"));
        assert_ne!(boxes[0].1, boxes[1].1);
    }

    #[test]
    fn a_disabled_route_is_left_out_and_order_is_respected() {
        let me = IdentityKeyPair::generate();
        let mut r = with_nodes();
        r.swap(2, 3);
        assert_eq!(mailboxes_in(&my_record(&me, &r, false, 1))[0].0, "guild.example:3479");
        r[2].enabled = false;
        assert_eq!(mailboxes_in(&my_record(&me, &r, false, 1)).len(), 1);
    }

    #[test]
    fn records_are_checked_against_who_sent_them() {
        let me = IdentityKeyPair::generate();
        let other = IdentityKeyPair::generate();
        let encoded = encode_record(&my_record(&me, &with_nodes(), false, 5));
        assert!(accept_record(&encoded, &me.public().identity_key, None).is_some());
        assert!(accept_record(&encoded, &other.public().identity_key, None).is_none(), "someone else's record");
        assert!(accept_record(&encoded, &me.public().identity_key, Some(9)).is_none(), "rollback");
        assert!(accept_record("zz", &me.public().identity_key, None).is_none());
    }

    #[test]
    fn normalize_restores_missing_tiers_once() {
        let mut r = vec![RouteConfig { kind: RouteKind::TrustedNode, enabled: false, nodes: vec![], code: String::new() }];
        r.push(r[0].clone());
        normalize(&mut r);
        assert_eq!(r.len(), 5);
        assert_eq!(r[0].kind, RouteKind::TrustedNode, "the user's order is kept");
    }

    #[test]
    fn own_device_code_is_stable_per_person() {
        let me = IdentityKeyPair::generate();
        assert_eq!(own_device_code(&me), own_device_code(&IdentityKeyPair::from_master_seed(me.master_seed())));
        assert_ne!(own_device_code(&me), own_device_code(&IdentityKeyPair::generate()));
    }

    #[tokio::test]
    async fn a_message_goes_through_a_real_node_and_comes_back_out() {
        use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
        use std::sync::{Arc, Mutex};
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let node = sock.local_addr().unwrap().to_string();
        let mb = Arc::new(Mutex::new(Mailbox::new(MailboxLimits { enabled: true, ..Default::default() })));
        tokio::spawn(antiphon_relay::run_node(
            sock,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))),
            Some(mb),
            None,
        ));

        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let mut routes = default_routes();
        routes[2].nodes = vec![node.clone()];
        // Bob registers by collecting once (nothing there yet).
        let report = collect(&bob, &routes, false, false, |_| true).await;
        assert!(report[0].1.is_ok(), "{:?}", report[0].1);

        let record = my_record(&bob, &routes, false, 1);
        let body = MailBody { id: 7, text: "back at six".into(), at_secs: 1 };
        let sent = send(&alice, &bob.public().identity_key, &bob.public().prekey, &record, &body, false).await;
        assert_eq!(sent, Sent::Deposited(1));

        let mut got = Vec::new();
        collect(&bob, &routes, false, false, |r| {
            got.push((r.from, r.body.clone()));
            true
        })
        .await;
        assert_eq!(got, vec![(alice.public().identity_key, body)]);
        // Acknowledged, so it is gone.
        let mut again = 0;
        collect(&bob, &routes, false, false, |_| {
            again += 1;
            true
        })
        .await;
        assert_eq!(again, 0);
    }
}
