//! A relay server (spec §3 "Everyone else -> Relay", "Community owners can
//! self-host", and "What a relay operator can see -- and the commitment").
//!
//! What it does: pairs two UDP sockets that name the same blinded token,
//! then forwards datagrams between them byte for byte. What it holds, and
//! for how long, is the whole of the privacy story, so it is listed here
//! rather than implied:
//!
//! - **Tokens**, 32 random-looking bytes that rotate daily and that only
//!   the two people involved can compute (`antiphon_crypto::relay_token`).
//!   Never an identity, a name or a key. Dropped [`REGISTRATION_TTL`] after
//!   the listener stops refreshing them.
//! - **Socket addresses**, because routing a packet needs one. Held only
//!   while a registration or a pairing is live, and dropped
//!   [`PAIR_IDLE`] after the last packet.
//! - **Nothing else.** No key ever reaches it -- the datagrams it forwards
//!   are the same sealed frames a direct call carries -- and no log is
//!   written: [`Relay`] has no logging at all, and the binary prints only
//!   aggregate counters, and only when asked to.
//!
//! The same node can also hold sealed mail for people who are offline
//! ([`mailbox`]), host sealed files ([`files`]), and forward group calls
//! as a selective forwarding unit ([`sfu`]): one copy of each member's
//! sealed audio in, a copy out to every other member of the room. It can also be a hop on anonymity-mode circuits ([`onion`]).
//! Each is off unless the operator switches it on. The SFU holds the same kind of
//! thing the relay does -- a blinded, daily-rotating room token, a random
//! per-call member tag and an address per member, for as long as the call
//! is live -- and forwards payloads it cannot read.
//!
//! The core is a plain state machine ([`Relay::handle`]) so it can be
//! tested without sockets; [`run`] is the thin loop around it.

pub mod files;
pub mod mailbox;
pub mod sfu;
pub mod onion;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use antiphon_net::relay::{Message, RelayToken};
use tokio::net::UdpSocket;

/// How long a registration survives without a refresh. Clients refresh
/// every `antiphon_net::relay::REGISTER_INTERVAL` (20 s).
pub const REGISTRATION_TTL: Duration = Duration::from_secs(60);
/// How long a pairing survives with no traffic either way. A call sends
/// audio every 20 ms, so this only ever fires after a call has ended.
pub const PAIR_IDLE: Duration = Duration::from_secs(120);
/// A pairing counts as a call in progress -- and the listener as busy --
/// while traffic is newer than this.
pub const BUSY_WINDOW: Duration = Duration::from_secs(10);

/// Caps, so a public relay cannot be filled by one misbehaving client.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_registrations: usize,
    pub max_tokens_per_addr: usize,
    pub max_pairs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_registrations: 200_000, max_tokens_per_addr: 1_024, max_pairs: 50_000 }
    }
}

/// Aggregate counters -- the only thing the relay can report, by design.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub registrations: usize,
    pub pairs: usize,
    pub forwarded: u64,
}

struct Registration {
    owner: SocketAddr,
    expires: Instant,
}

struct Pair {
    other: SocketAddr,
    last_seen: Instant,
}

#[derive(Default)]
pub struct Relay {
    limits: Limits,
    registrations: HashMap<RelayToken, Registration>,
    tokens_per_addr: HashMap<SocketAddr, usize>,
    pairs: HashMap<SocketAddr, Pair>,
    forwarded: u64,
    last_sweep: Option<Instant>,
}

impl Relay {
    pub fn new(limits: Limits) -> Self {
        Self { limits, ..Default::default() }
    }

    pub fn stats(&self) -> Stats {
        Stats { registrations: self.registrations.len(), pairs: self.pairs.len() / 2, forwarded: self.forwarded }
    }

    /// Handles one datagram from `from`, returning what to send where.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        self.sweep_if_due(now);

        if !Message::is_relay_datagram(bytes) {
            // Ordinary traffic: forward it if this socket is paired, drop
            // it otherwise. Never inspected -- it is ciphertext anyway.
            let Some(pair) = self.pairs.get_mut(&from) else { return vec![] };
            pair.last_seen = now;
            let to = pair.other;
            if let Some(back) = self.pairs.get_mut(&to) {
                back.last_seen = now;
            }
            self.forwarded += 1;
            return vec![(to, bytes.to_vec())];
        }

        match Message::decode(bytes) {
            Some(Message::Register(tokens)) => {
                let mut accepted = 0u16;
                for token in tokens {
                    if self.register(from, token, now) {
                        accepted += 1;
                    }
                }
                vec![(from, Message::Registered(accepted).encode())]
            }
            Some(Message::Dial(token)) => {
                let reply = if self.dial(from, &token, now) { Message::Dialed } else { Message::NotFound };
                vec![(from, reply.encode())]
            }
            Some(Message::Leave) => {
                self.forget(from);
                vec![]
            }
            // Replies are for clients, and garbage is garbage.
            _ => vec![],
        }
    }

    fn register(&mut self, owner: SocketAddr, token: RelayToken, now: Instant) -> bool {
        let expires = now + REGISTRATION_TTL;
        if let Some(existing) = self.registrations.get_mut(&token) {
            if existing.owner == owner {
                existing.expires = expires;
                return true;
            }
            // Someone else holds it. A token is only computable by the
            // two people involved, so a clash is a moved listener (new
            // NAT mapping) or an attack; either way the newest live
            // registration is the one that can be reached.
            let previous = existing.owner;
            existing.owner = owner;
            existing.expires = expires;
            self.decrement(previous);
            *self.tokens_per_addr.entry(owner).or_default() += 1;
            return true;
        }
        let held = self.tokens_per_addr.get(&owner).copied().unwrap_or(0);
        if held >= self.limits.max_tokens_per_addr || self.registrations.len() >= self.limits.max_registrations {
            return false;
        }
        self.registrations.insert(token, Registration { owner, expires });
        *self.tokens_per_addr.entry(owner).or_default() += 1;
        true
    }

    fn dial(&mut self, caller: SocketAddr, token: &RelayToken, now: Instant) -> bool {
        let Some(registration) = self.registrations.get(token) else { return false };
        if registration.expires <= now {
            return false;
        }
        let listener = registration.owner;
        if listener == caller {
            return false;
        }
        // Already in a call with somebody else: busy. Letting a second
        // dialler take over would cut the first call off.
        if let Some(pair) = self.pairs.get(&listener) {
            if pair.other != caller && now.duration_since(pair.last_seen) < BUSY_WINDOW {
                return false;
            }
        }
        if self.pairs.len() / 2 >= self.limits.max_pairs && !self.pairs.contains_key(&caller) {
            return false;
        }
        self.unpair(caller);
        self.unpair(listener);
        self.pairs.insert(caller, Pair { other: listener, last_seen: now });
        self.pairs.insert(listener, Pair { other: caller, last_seen: now });
        true
    }

    fn unpair(&mut self, addr: SocketAddr) {
        if let Some(pair) = self.pairs.remove(&addr) {
            if self.pairs.get(&pair.other).is_some_and(|p| p.other == addr) {
                self.pairs.remove(&pair.other);
            }
        }
    }

    fn forget(&mut self, addr: SocketAddr) {
        self.unpair(addr);
        self.registrations.retain(|_, r| r.owner != addr);
        self.tokens_per_addr.remove(&addr);
    }

    fn decrement(&mut self, addr: SocketAddr) {
        if let Some(n) = self.tokens_per_addr.get_mut(&addr) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.tokens_per_addr.remove(&addr);
            }
        }
    }

    fn sweep_if_due(&mut self, now: Instant) {
        if self.last_sweep.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return;
        }
        self.last_sweep = Some(now);
        self.sweep(now);
    }

    /// Drops everything expired. Expiry is the retention policy: nothing
    /// outlives it, because nothing is written anywhere else.
    pub fn sweep(&mut self, now: Instant) {
        let expired: Vec<SocketAddr> =
            self.registrations.values().filter(|r| r.expires <= now).map(|r| r.owner).collect();
        self.registrations.retain(|_, r| r.expires > now);
        for owner in expired {
            self.decrement(owner);
        }
        self.pairs.retain(|_, p| now.duration_since(p.last_seen) < PAIR_IDLE);
    }
}

/// Serves `relay` on `socket` until the socket fails.
pub async fn run(socket: Arc<UdpSocket>, relay: Arc<std::sync::Mutex<Relay>>) -> std::io::Result<()> {
    run_node(socket, relay, None, None).await
}

/// Serves the relay and, if given, the mailbox on one socket. Mailbox
/// requests are told apart by their magic before the relay sees them, so
/// a client with a live relayed call can still use the mailbox.
pub async fn run_node(
    socket: Arc<UdpSocket>,
    relay: Arc<std::sync::Mutex<Relay>>,
    mailbox: Option<Arc<std::sync::Mutex<mailbox::Mailbox>>>,
    files: Option<Arc<std::sync::Mutex<files::FileHost>>>,
) -> std::io::Result<()> {
    run_node_full(socket, relay, mailbox, files, None, None, None).await
}

/// [`run_node`], also serving as a DHT node (spec §4) when `dht` is given.
/// Off unless the operator asks: a node that isn't one stays silent to
/// DHT traffic rather than refusing it, so it can't be mapped as one.
/// The caller drives [`antiphon_net::dht::DhtNode::tick`] on a timer.
pub async fn run_node_with_dht(
    socket: Arc<UdpSocket>,
    relay: Arc<std::sync::Mutex<Relay>>,
    mailbox: Option<Arc<std::sync::Mutex<mailbox::Mailbox>>>,
    files: Option<Arc<std::sync::Mutex<files::FileHost>>>,
    dht: Option<Arc<std::sync::Mutex<antiphon_net::dht::DhtNode>>>,
) -> std::io::Result<()> {
    run_node_full(socket, relay, mailbox, files, None, None, dht).await
}

/// [`run_node`], plus group-call forwarding if `sfu` is given. SFU
/// datagrams are told apart by their own magic like the other services;
/// without an SFU, a JOIN is answered "disabled" so a client can say why.
pub async fn run_node_with_sfu(
    socket: Arc<UdpSocket>,
    relay: Arc<std::sync::Mutex<Relay>>,
    mailbox: Option<Arc<std::sync::Mutex<mailbox::Mailbox>>>,
    files: Option<Arc<std::sync::Mutex<files::FileHost>>>,
    sfu: Option<Arc<std::sync::Mutex<sfu::Sfu>>>,
) -> std::io::Result<()> {
    run_node_full(socket, relay, mailbox, files, sfu, None, None).await
}

/// [`run_node`], and if `hops` is given, this node as a hop on anonymity
/// circuits ([`onion`]).
pub async fn run_node_with_hops(
    socket: Arc<UdpSocket>,
    relay: Arc<std::sync::Mutex<Relay>>,
    mailbox: Option<Arc<std::sync::Mutex<mailbox::Mailbox>>>,
    files: Option<Arc<std::sync::Mutex<files::FileHost>>>,
    hops: Option<Arc<std::sync::Mutex<onion::HopNode>>>,
) -> std::io::Result<()> {
    run_node_full(socket, relay, mailbox, files, None, hops, None).await
}

/// Every service on one socket: relay, and each optional one if given.
/// Onion cells have their own magic and are told apart the same way; each
/// circuit that exits here gets a socket of its own, read by a small task
/// that feeds replies back into this loop.
pub async fn run_node_full(
    socket: Arc<UdpSocket>,
    relay: Arc<std::sync::Mutex<Relay>>,
    mailbox: Option<Arc<std::sync::Mutex<mailbox::Mailbox>>>,
    files: Option<Arc<std::sync::Mutex<files::FileHost>>>,
    sfu: Option<Arc<std::sync::Mutex<sfu::Sfu>>>,
    hops: Option<Arc<std::sync::Mutex<onion::HopNode>>>,
    dht: Option<Arc<std::sync::Mutex<antiphon_net::dht::DhtNode>>>,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65_536];
    let mut last_sweep = Instant::now();
    let mut exits = Exits::new(&socket)?;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        let (n, from) = tokio::select! {
            r = socket.recv_from(&mut buf) => r?,
            Some((id, from, bytes)) = exits.incoming.recv() => {
                if let Some(h) = &hops {
                    let actions = h.lock().unwrap().handle_exit(id, from, &bytes, Instant::now());
                    exits.perform(&socket, hops.as_ref(), actions).await;
                }
                continue;
            }
            _ = tick.tick() => {
                // Idle circuits go on a clock, not only on traffic, so a
                // quiet node still closes the exit sockets it promised to.
                if let Some(h) = &hops {
                    let actions = h.lock().unwrap().sweep(Instant::now());
                    exits.perform(&socket, hops.as_ref(), actions).await;
                }
                continue;
            }
        };
        let now = Instant::now();
        if antiphon_net::onion::Cell::is_onion_datagram(&buf[..n]) {
            match &hops {
                Some(h) => {
                    let actions = h.lock().unwrap().handle(from, &buf[..n], now);
                    exits.perform(&socket, hops.as_ref(), actions).await;
                }
                None => {
                    if let Some(reply) = onion::refusal_when_disabled(&buf[..n]) {
                        let _ = socket.send_to(&reply, from).await;
                    }
                }
            }
            continue;
        }
        let out = if antiphon_net::dht::Message::is_dht_datagram(&buf[..n]) {
            match &dht {
                Some(d) => d.lock().unwrap().handle(from, &buf[..n], now),
                None => vec![],
            }
        } else if antiphon_net::sfu::Message::is_sfu_datagram(&buf[..n]) {
            match &sfu {
                Some(s) => s.lock().unwrap().handle(from, &buf[..n], now),
                None => match antiphon_net::sfu::Message::decode(&buf[..n]) {
                    Some(antiphon_net::sfu::Message::Join { .. }) => vec![(
                        from,
                        antiphon_net::sfu::Message::Refused(antiphon_net::sfu::Refusal::Disabled).encode(),
                    )],
                    _ => vec![],
                },
            }
        } else if antiphon_net::blob::Message::is_blob_datagram(&buf[..n]) {
            match &files {
                Some(f) => {
                    let secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    f.lock().unwrap().handle(from, &buf[..n], now, secs)
                }
                None => vec![(
                    from,
                    antiphon_net::blob::Message::Refused(antiphon_net::blob::Refusal::Disabled).encode(),
                )],
            }
        } else if antiphon_net::mailbox::Message::is_mailbox_datagram(&buf[..n]) {
            match &mailbox {
                Some(m) => {
                    let mut m = m.lock().unwrap();
                    if now.duration_since(last_sweep) > Duration::from_secs(60) {
                        m.sweep(now);
                        last_sweep = now;
                    }
                    m.handle(from, &buf[..n], now)
                }
                None => vec![(
                    from,
                    antiphon_net::mailbox::Message::Refused(antiphon_net::mailbox::Refusal::Disabled).encode(),
                )],
            }
        } else {
            relay.lock().unwrap().handle(from, &buf[..n], now)
        };
        for (to, bytes) in out {
            // One unreachable peer must not stop the relay for everyone.
            let _ = socket.send_to(&bytes, to).await;
        }
    }
}

/// The exit sockets of circuits that end at this node, and the channel
/// their reader tasks feed.
struct Exits {
    /// Each circuit's own exit socket and the task reading from it. One socket
    /// per circuit, so the target relay sees separate ordinary clients.
    sockets: HashMap<onion::CircuitId, (Arc<UdpSocket>, tokio::task::JoinHandle<()>)>,
    /// Handed to each reader task to report what arrived.
    tx: tokio::sync::mpsc::Sender<(onion::CircuitId, SocketAddr, Vec<u8>)>,
    /// Replies from targets, tagged with the circuit they came in on.
    incoming: tokio::sync::mpsc::Receiver<(onion::CircuitId, SocketAddr, Vec<u8>)>,
    /// Where exit sockets bind: the node's own IP, any port.
    bind: SocketAddr,
}

impl Exits {
    /// An empty set of exits that bind on the same IP as `node`.
    fn new(node: &UdpSocket) -> std::io::Result<Self> {
        // Bounded, so a flood at one exit socket queues a little and then
        // drops rather than growing without limit.
        let (tx, incoming) = tokio::sync::mpsc::channel(4096);
        let bind = match node.local_addr()? {
            SocketAddr::V4(a) => SocketAddr::from((*a.ip(), 0)),
            SocketAddr::V6(a) => SocketAddr::from((*a.ip(), 0)),
        };
        Ok(Self { sockets: HashMap::new(), tx, incoming, bind })
    }

    /// Carries out a hop node's actions: sends, opens and closes exit sockets.
    /// A socket that fails to open is reported back to the hop node, and the
    /// actions that produces are carried out too.
    async fn perform(
        &mut self,
        node: &UdpSocket,
        hops: Option<&Arc<std::sync::Mutex<onion::HopNode>>>,
        actions: Vec<onion::Action>,
    ) {
        let mut queue = std::collections::VecDeque::from(actions);
        while let Some(action) = queue.pop_front() {
            match action {
                onion::Action::Send(to, bytes) => {
                    let _ = node.send_to(&bytes, to).await;
                }
                onion::Action::OpenExit(id) => match UdpSocket::bind(self.bind).await {
                    Ok(s) => {
                        let s = Arc::new(s);
                        let (reader, tx) = (s.clone(), self.tx.clone());
                        let task = tokio::spawn(async move {
                            let mut buf = vec![0u8; 65_536];
                            loop {
                                match reader.recv_from(&mut buf).await {
                                    Ok((n, from)) => {
                                        let _ = tx.try_send((id, from, buf[..n].to_vec()));
                                    }
                                    // Windows reports an ICMP "port
                                    // unreachable" from an earlier send this
                                    // way; it is not a broken socket.
                                    Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
                                    Err(_) => break,
                                }
                            }
                        });
                        self.sockets.insert(id, (s, task));
                    }
                    Err(_) => {
                        if let Some(h) = hops {
                            queue.extend(h.lock().unwrap().exit_failed(id));
                        }
                    }
                },
                onion::Action::ExitSend(id, to, bytes) => {
                    if let Some((s, _)) = self.sockets.get(&id) {
                        let _ = s.send_to(&bytes, to).await;
                    }
                }
                onion::Action::CloseExit(id) => {
                    if let Some((_, task)) = self.sockets.remove(&id) {
                        task.abort();
                    }
                }
            }
        }
    }
}

/// The operator commitment (spec §3), printed at start-up and in `--help`
/// so it is stated plainly rather than buried in a policy.
pub const COMMITMENT: &str = "\
This relay forwards encrypted packets it cannot read. It never holds a key.
It sees rotating blinded tokens, never who you are. It keeps addresses only
while a call or registration is live, and writes no connection logs.
If it holds mail, the mail is sealed end to end: it sees an inbox label it
cannot link to anyone, and never a sender, a recipient or a word.
If it forwards group calls, it sees a room token that changes daily and a
random tag per member per call, never a group, a name or a word: the audio
is sealed by the members and forwarded as it came.
If it offers hops for anonymity mode, its hop key only peels routing layers:
it never decrypts a call, and learns only the hop before and the hop after.
If it is a DHT node, it stores records filed under keys that change daily,
sealed so it can read neither whose they are nor the address inside.";

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn a_dial_pairs_caller_and_listener_and_traffic_flows_both_ways() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        let (listener, caller, token) = (addr(1), addr(2), [9u8; 32]);

        let out = r.handle(listener, &Message::Register(vec![token]).encode(), now);
        assert_eq!(out, vec![(listener, Message::Registered(1).encode())]);

        let out = r.handle(caller, &Message::Dial(token).encode(), now);
        assert_eq!(out, vec![(caller, Message::Dialed.encode())]);

        assert_eq!(r.handle(caller, b"\x01sealed", now), vec![(listener, b"\x01sealed".to_vec())]);
        assert_eq!(r.handle(listener, b"\x01reply", now), vec![(caller, b"\x01reply".to_vec())]);
        assert_eq!(r.stats().forwarded, 2);
    }

    #[test]
    fn unknown_tokens_and_unpaired_traffic_go_nowhere() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        assert_eq!(r.handle(addr(2), &Message::Dial([1; 32]).encode(), now), vec![(addr(2), Message::NotFound.encode())]);
        assert!(r.handle(addr(2), b"\x01anything", now).is_empty());
    }

    #[test]
    fn registrations_expire_without_a_refresh() {
        let mut r = Relay::new(Limits::default());
        let t0 = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), t0);
        let later = t0 + REGISTRATION_TTL + Duration::from_secs(1);
        let out = r.handle(addr(2), &Message::Dial([5; 32]).encode(), later);
        assert_eq!(out, vec![(addr(2), Message::NotFound.encode())]);
        r.sweep(later);
        assert_eq!(r.stats().registrations, 0);
    }

    #[test]
    fn idle_pairs_are_forgotten() {
        let mut r = Relay::new(Limits::default());
        let t0 = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), t0);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), t0);
        assert_eq!(r.stats().pairs, 1);
        r.sweep(t0 + PAIR_IDLE + Duration::from_secs(1));
        assert_eq!(r.stats().pairs, 0);
        assert!(r.handle(addr(2), b"\x01late", t0 + PAIR_IDLE * 2).is_empty());
    }

    #[test]
    fn a_listener_in_a_call_is_busy_to_a_second_caller() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        let out = r.handle(addr(3), &Message::Dial([5; 32]).encode(), now);
        assert_eq!(out, vec![(addr(3), Message::NotFound.encode())]);
        // ...but once that call has gone quiet, the listener is free.
        let later = now + BUSY_WINDOW + Duration::from_secs(1);
        let out = r.handle(addr(3), &Message::Dial([5; 32]).encode(), later);
        assert_eq!(out, vec![(addr(3), Message::Dialed.encode())]);
    }

    #[test]
    fn per_address_token_cap_holds() {
        let mut r = Relay::new(Limits { max_tokens_per_addr: 2, ..Limits::default() });
        let out = r.handle(addr(1), &Message::Register(vec![[1; 32], [2; 32], [3; 32]]).encode(), Instant::now());
        assert_eq!(out, vec![(addr(1), Message::Registered(2).encode())]);
    }

    #[test]
    fn leave_forgets_everything_about_an_address() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        r.handle(addr(1), &Message::Leave.encode(), now);
        assert_eq!(r.stats(), Stats { registrations: 0, pairs: 0, forwarded: 0 });
    }

    #[test]
    fn a_moved_listener_takes_its_token_with_it() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(7), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        assert_eq!(r.handle(addr(2), b"\x01x", now), vec![(addr(7), b"\x01x".to_vec())]);
    }
}
