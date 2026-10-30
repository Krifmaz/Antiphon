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
use antiphon_crypto::{mail, mail_key, IdentityKeyPair};

use crate::mail_keys::MailKeys;
use antiphon_net::mailbox::{self, InboxStatus, Label};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use antiphon_net::vpn::TrafficKind;
use antiphon_net::AppSocket;
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

/// Your route list, signed, ready to hand to a contact: [`my_record_with`]
/// for an identity that keeps no separate mail key (a persona, a test).
pub fn my_record(identity: &IdentityKeyPair, routes: &[RouteConfig], strict: bool, sequence: u64) -> SignedRouteRecord {
    my_record_with(identity, &MailKeys::default(), routes, strict, sequence)
}

/// Your route list, signed, with inboxes under the account's current mail
/// key and, once that is not the identity itself, the certificate contacts
/// seal to (see [`crate::mail_keys`]).
pub fn my_record_with(identity: &IdentityKeyPair, keys: &MailKeys, routes: &[RouteConfig], strict: bool, sequence: u64) -> SignedRouteRecord {
    let holder = keys.current(identity);
    let mut out = Vec::new();
    if strict || routes.iter().any(|r| r.kind == RouteKind::Direct && r.enabled) {
        out.push(Route { kind: RouteKind::Direct.wire().to_string(), address: String::new() });
    }
    for (kind, node, _) in inbox_nodes(routes, strict) {
        let label = mail::inbox_label(&holder, &node);
        out.push(Route { kind: kind.wire().to_string(), address: format!("{node}#{}", hex(&label)) });
    }
    // The certificate always fits: it takes the last place if need be.
    let cert = keys.cert(identity);
    out.truncate(if cert.is_some() { 31 } else { 32 });
    if let Some(cert) = cert {
        out.push(Route { kind: mail_key::ROUTE_KIND.to_string(), address: cert.to_address() });
    }
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

/// The prekey a recipient's record says to seal mail to: their current
/// mail key, when they publish one their identity signed, else `None`
/// (seal to the identity's own prekey).
pub fn mail_key_in(record: &SignedRouteRecord) -> Option<[u8; 32]> {
    record
        .record
        .routes
        .iter()
        .filter(|r| r.kind == mail_key::ROUTE_KIND)
        .filter_map(|r| mail_key::MailKeyCert::from_address(&r.address))
        .filter(|c| c.verify(&record.record.identity_key).is_ok())
        .max_by_key(|c| c.epoch)
        .map(|c| c.prekey)
}

/// Where in a recipient's record mail can be left: `(node, label)`, in
/// their order. Direct and anything unparseable are skipped.
pub fn mailboxes_in(record: &SignedRouteRecord) -> Vec<(String, Label)> {
    record
        .record
        .routes
        .iter()
        .filter(|r| r.kind != RouteKind::Direct.wire() && r.kind != mail_key::ROUTE_KIND)
        // A phone's push targets live in the same record; they are where
        // to wake it, never where to leave mail.
        .filter(|r| !antiphon_push::Target::is_push_route(r))
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
    /// A piece of a group chat or community message (spec §9), base64,
    /// instead of a 1:1 text. See `crate::community`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// The message this one answers, by id, when it was written as a
    /// reply. Left out when it isn't one, so older builds read the mail
    /// as before (and show it without its quote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<u64>,
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
    // Their current mail key when they publish one, so a device they have
    // since removed can't open it.
    let prekey = mail_key_in(their_record).unwrap_or(*their_prekey);
    let Ok(blob) = mail::seal(identity, their_identity, &prekey, &plain) else {
        return Sent::Failed("could not seal the message".into());
    };
    if blob.len() > mailbox::MAX_BLOB {
        return Sent::Failed("too long for a mailbox; it will go when you're both online".into());
    }
    // Offline messages go through the VPN when the user routed them
    // there (spec §6), and are held, not sent direct, under the kill switch.
    let socket = match crate::vpn::bind(TrafficKind::Messages, 0).await {
        Ok(s) => s,
        Err(e) => return Sent::Failed(e.to_string()),
    };
    let mut id = [0u8; 16];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    let mut deposited = 0;
    let mut last_error = String::from("no mailbox answered");
    for (node, label) in boxes {
        let Some(addr) = resolve(TrafficKind::Messages, &node).await else {
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
        // A phone that publishes a push target is told mail is waiting, so
        // it fetches now instead of whenever it next polls. Rate-limited
        // per recipient inside, so a burst of queued messages wakes once.
        crate::push_wake::send(identity, their_identity, their_record, antiphon_crypto::push::WakeKind::Message, &[]);
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
    keep: impl FnMut(&Received) -> bool,
) -> Vec<(String, Result<NodeStatus, String>)> {
    collect_with(identity, &MailKeys::default(), routes, strict, hosting, keep).await
}

/// [`collect`] for the account: at the inboxes of every mail key still in
/// use -- the current one, and retired ones contacts may not have heard
/// are gone (see [`crate::mail_keys`]).
pub async fn collect_with(
    identity: &IdentityKeyPair,
    keys: &MailKeys,
    routes: &[RouteConfig],
    strict: bool,
    hosting: bool,
    mut keep: impl FnMut(&Received) -> bool,
) -> Vec<(String, Result<NodeStatus, String>)> {
    let mut report = Vec::new();
    let socket = match crate::vpn::bind(TrafficKind::Messages, 0).await {
        Ok(s) => s,
        Err(e) => {
            for (_, node, _) in inbox_nodes(routes, strict) {
                report.push((node, Err(e.to_string())));
            }
            return report;
        }
    };
    for (kind, node, code) in inbox_nodes(routes, strict) {
        let leave = hosting && kind == RouteKind::OwnDevices;
        let status = collect_one(identity, keys, &socket, &node, &code, leave, &mut keep).await;
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

/// Collects one node: the inbox of each mail key in use there. The status
/// shown is the current key's inbox, the one contacts are told about.
async fn collect_one(
    identity: &IdentityKeyPair,
    keys: &MailKeys,
    socket: &AppSocket,
    node: &str,
    code: &[u8],
    leave_in_place: bool,
    keep: &mut impl FnMut(&Received) -> bool,
) -> Result<NodeStatus, String> {
    let addr = resolve(TrafficKind::Messages, node).await.ok_or("could not find that address")?;
    let now = now_secs();
    let mut status = None;
    for holder in keys.holders(identity, now) {
        let label = mail::inbox_label(&holder, node);
        let secret = mail::fetch_secret(&holder, node);
        let started = Instant::now();
        let registered = mailbox::register(socket, addr, label, mail::fetch_hash(&secret), code, NODE_TIMEOUT).await.map_err(|e| e.to_string());
        // A retired inbox that won't answer is no reason to fail the node.
        if status.is_some() && registered.is_err() {
            continue;
        }
        registered?;
        let response_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let waiting = mailbox::fetch(socket, addr, label, secret, NODE_TIMEOUT).await.map_err(|e| e.to_string())?;
        let mut collected = 0;
        for (id, blob) in waiting {
            let accepted = match keys.open(identity, &blob, now) {
                Some((sender, plain)) => match serde_json::from_slice::<MailBody>(&plain) {
                    Ok(body) => keep(&Received { from: sender.identity_key, body }),
                    // Opened and authenticated but not ours to parse: drop it
                    // rather than collect it forever.
                    Err(_) => true,
                },
                // Not sealed to us. Nobody can ever open it, so holding it
                // only fills the inbox.
                None => true,
            };
            if accepted {
                collected += 1;
                if !leave_in_place {
                    let _ = mailbox::ack(socket, addr, label, secret, id, NODE_TIMEOUT).await;
                }
            }
        }
        match status.as_mut() {
            None => {
                let inbox = mailbox::status(socket, addr, label, secret, NODE_TIMEOUT).await.unwrap_or_default();
                status = Some(NodeStatus { response_ms, inbox, collected });
            }
            Some(s) => s.collected += collected,
        }
    }
    status.ok_or_else(|| "no mail key to collect with".to_string())
}

/// Now, in unix seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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

/// Resolves a node's `host[:port]` the way `kind` of traffic reaches it --
/// through the VPN's DNS when that kind is tunneled (spec §6).
pub async fn resolve(kind: TrafficKind, node: &str) -> Option<SocketAddr> {
    let with_port = if node.contains(':') { node.to_string() } else { format!("{node}:{DEFAULT_PORT}") };
    crate::vpn::resolve(kind, &with_port).await.ok()
}

pub fn hex(bytes: &[u8]) -> String {
    antiphon_crypto::hex::encode(bytes)
}

pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    crate::connection_mode::decode_hex32(s)
}

pub fn encode_record(record: &SignedRouteRecord) -> String {
    hex(&record.encode())
}

/// A contact's record from what they sent, checked: it must be signed by
/// the key we know them by, and newer than the one already held.
pub fn accept_record(encoded: &str, their_identity: &[u8; 32], known_sequence: Option<u64>) -> Option<SignedRouteRecord> {
    let bytes = antiphon_crypto::hex::decode(encoded)?;
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
    hex(&my_record_bytes())
}

/// Your current route list, signed, as bytes: what [`my_record_hex`]
/// encodes, for a caller (discovery) that publishes the bytes themselves.
/// Empty before an identity is loaded.
pub fn my_record_bytes() -> Vec<u8> {
    let Some(identity) = IDENTITY.get() else { return Vec::new() };
    let config = crate::config::load();
    my_record_with(identity, &my_mail_keys(), &config.routes, config.strict_mode, config.route_sequence).encode()
}

/// The account's mail keys as saved in the config folder; epoch 0 (the
/// identity itself) when there are none or they can't be read.
pub fn my_mail_keys() -> MailKeys {
    let Some(dir) = crate::config::config_dir() else { return MailKeys::default() };
    match crate::history::load_or_create_backup_key(&dir) {
        Ok(key) => MailKeys::load(&dir, &key),
        Err(_) => MailKeys::default(),
    }
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

pub(crate) fn notice(line: String) {
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
    let community = crate::community::Store::app();
    let keys = MailKeys::load(&dir, &key);
    let report = collect_with(&identity, &keys, &config.routes, config.strict_mode, config.hold_mail_for_devices, |r| {
        // A piece of a group chat or community message (spec §9): from
        // members who need not be your contacts. Whether they really are
        // members is the community's to check once it is whole.
        if let Some(fragment) = &r.body.group {
            return community.as_ref().is_none_or(|c| c.receive_fragment(r.from, fragment));
        }
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
        // A quote of a message this history doesn't hold would point at
        // nothing; it is kept as a plain message.
        let reply_to = r.body.reply_to.filter(|q| log.iter().any(|m| m.id == *q));
        log.push(crate::history::StoredMessage {
            id: r.body.id,
            from_me: false,
            text: r.body.text.clone(),
            at_secs: r.body.at_secs,
            edited: false,
            deleted: false,
            pinned: false,
            expires_at_secs: contact.expire_after_secs.map(|ttl| r.body.at_secs + ttl),
            reply_to,
            ..Default::default()
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

    // Communities you're in under a separate identity (spec §7): that
    // identity has inboxes of its own at the same nodes, under labels
    // nobody can match with yours. Only community traffic is taken there.
    if let Some(c) = &community {
        for persona in c.personas() {
            let _ = collect(&persona, &config.routes, config.strict_mode, false, |r| match &r.body.group {
                Some(fragment) => c.receive_persona_fragment(r.from, fragment),
                None => true,
            })
            .await;
        }
    }

    // Group chats and communities: over live calls, and -- unless strict
    // mode keeps everything off mailboxes -- to members' mailboxes.
    if let Some(c) = &community {
        c.flush(config.route_fan_out, !config.strict_mode).await;
        // Communities with a board (spec §11 "full scale"): one upload per
        // message out, a page at a time in. Off the network in strict mode.
        if !config.strict_mode {
            c.sync_boards().await;
        }
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
            let body = MailBody { id: message.id, text: message.text.clone(), at_secs: message.queued_at_secs, group: None, reply_to: message.reply_to };
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
    leave_reply(contact_key_hex, text, expire_after_secs, None)
}

/// [`leave_message`] for a message that answers an earlier one
/// (`reply_to`, by id). An id the history doesn't hold is dropped, so the
/// message still goes, only without its quote.
pub fn leave_reply(contact_key_hex: &str, text: &str, expire_after_secs: Option<u64>, reply_to: Option<u64>) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("Write something first.".into());
    }
    let dir = crate::config::config_dir().ok_or("no config directory")?;
    let key = crate::history::load_or_create_backup_key(&dir).map_err(|e| e.to_string())?;
    let mut log = crate::history::load(&dir, contact_key_hex, &key);
    let id = log.iter().map(|m| m.id).max().map(|m| m + 1).unwrap_or(0);
    let reply_to = reply_to.filter(|r| log.iter().any(|m| m.id == *r));
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
        reply_to,
        ..Default::default()
    });
    crate::history::save(&dir, contact_key_hex, &key, &log).map_err(|e| e.to_string())?;
    let mut queue = crate::outbox::load(&dir, contact_key_hex, &key);
    crate::outbox::enqueue(
        &mut queue,
        crate::outbox::Queued { id, text: text.to_string(), queued_at_secs: now, attempts: 0, reply_to },
    );
    crate::outbox::save(&dir, contact_key_hex, &key, &queue).map_err(|e| e.to_string())?;
    kick();
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rotated_mail_key_is_published_and_moves_the_inboxes() {
        let me = IdentityKeyPair::generate();
        let plain = my_record(&me, &with_nodes(), false, 1);
        assert_eq!(mail_key_in(&plain), None);

        let mut keys = MailKeys::default();
        keys.rotate(10);
        let record = my_record_with(&me, &keys, &with_nodes(), false, 2);
        assert_eq!(mail_key_in(&record), Some(keys.current(&me).public().prekey));
        // The certificate is not a mailbox, and the inboxes moved.
        let boxes = mailboxes_in(&record);
        assert_eq!(boxes.len(), 2);
        assert_ne!(boxes[0].1, mailboxes_in(&plain)[0].1);
        assert_eq!(boxes[0].1, mail::inbox_label(&keys.current(&me), "home.example:3479"));

        // Someone else's certificate pasted into the record is ignored.
        let mallory = IdentityKeyPair::generate();
        let forged = mail_key::MailKeyCert::issue(&mallory, 99, &mallory);
        let mut tampered = plain.record;
        tampered.routes.push(Route { kind: mail_key::ROUTE_KIND.into(), address: forged.to_address() });
        let tampered = tampered.sign(&me).unwrap();
        assert_eq!(mail_key_in(&tampered), None);
    }

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

    #[test]
    fn a_reply_keeps_its_quote_in_the_mail_and_older_mail_still_reads() {
        let reply = MailBody { id: 3, text: "same".into(), at_secs: 1, group: None, reply_to: Some(2) };
        let back: MailBody = serde_json::from_slice(&serde_json::to_vec(&reply).unwrap()).unwrap();
        assert_eq!(back.reply_to, Some(2));
        // Mail from a build before replies has no such field.
        let old: MailBody = serde_json::from_str(r#"{"id":1,"text":"hi","at_secs":1}"#).unwrap();
        assert_eq!(old.reply_to, None);
        let plain = MailBody { reply_to: None, ..old };
        assert!(!serde_json::to_string(&plain).unwrap().contains("reply_to"), "a plain message is written as before");
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
        let body = MailBody { id: 7, text: "back at six".into(), at_secs: 1, group: None, reply_to: None };
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
