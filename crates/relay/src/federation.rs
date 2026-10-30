//! Relay federation: relays peering with other relays so the mesh, not
//! any one box, is what people depend on.
//!
//! What it gives people: someone registered at relay A can be rung by a
//! caller who dials relay B. B does not hold their token, so it asks the
//! relays it peers with; A answers with proof that it does; B dials the
//! token at A like any client and passes the call through. The caller and
//! the listener each talk only to their own relay, and either relay can be
//! swapped for any other in the mesh. Apps also ask a relay who it peers
//! with ([`Message::PeersRequest`]) and keep those as fallbacks, so losing
//! one relay does not strand anyone.
//!
//! How relays find each other, with no registry and no authority:
//!
//! - the operator names some (`--peer host:port`, optionally pinned to a
//!   key as `host:port#hex`);
//! - every peer gossips the relays it peers with ([`PeerMessage::Gossip`]),
//!   each with its key, and those become candidates;
//! - a candidate becomes a peer only by completing a handshake that proves
//!   it holds the key it was listed with.
//!
//! Peering is untrusted by default. A peer is only ever a relay that proved
//! its key, never one that is believed:
//!
//! - **It cannot read anything.** The calls a relay bridges are sealed end
//!   to end; a bridge forwards datagrams it cannot open, exactly as a relay
//!   always has.
//! - **It cannot learn tokens it does not hold.** A lookup carries a hash
//!   of the token (`antiphon_crypto::federation::lookup_hash`), so a peer
//!   that does not already hold the token learns nothing it could dial or
//!   register with.
//! - **It cannot hijack a route.** A "have" answer must carry a MAC keyed
//!   by the token over this lookup's fresh nonce and the answering relay's
//!   own key (`have_proof`). Claiming a person it does not hold is detected
//!   and ignored; a recorded answer is useless for another lookup.
//! - **It cannot replay or forge peer traffic.** Everything after the
//!   handshake is sealed per direction with a replay window; a spoofed
//!   source address opens nothing.
//! - **It cannot flood.** Candidates, peers, handshakes per second, pending
//!   lookups, lookups per peer per second and bridges are all capped, and a
//!   stranger's requests are padded so no answer is larger than the
//!   question.
//!
//! What it holds, in keeping with the relay's commitment: peer addresses
//! and keys while peering, lookups for at most [`LOOKUP_TIMEOUT`], and per
//! bridge the caller's address, the peer relay's address and the token,
//! only while the call is live. No log, no identity, no key to any call.
//!
//! Like [`crate::onion::HopNode`], this is a pure state machine that
//! returns [`Action`]s; `run_node_with_federation` performs them.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use antiphon_crypto::federation::{
    check_have_proof, have_proof, initiate, lookup_hash, respond, Hello, HelloBack, Initiator, PeerKey, PeerSession,
};
use antiphon_net::federation::{Message, PeerMessage, MAX_GOSSIP_PEERS, MAX_LISTED_PEERS};
use antiphon_net::relay::{Message as RelayMessage, RelayToken};

use crate::Relay;

/// How long a lookup waits for a peer to say it holds the token. A client
/// re-sends its dial every 400 ms, so it is still waiting when this ends.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_millis(1_500);
/// How long a bridge waits for the peer relay to pair it.
pub const BRIDGE_DIAL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a bridge survives with no traffic either way.
pub const BRIDGE_IDLE: Duration = crate::PAIR_IDLE;
/// How long a handshake waits for its answer.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a peer is pinged, keeping the session and any NAT mapping open.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);
/// A peer not heard from for this long is dropped back to a candidate.
pub const PEER_SILENCE: Duration = Duration::from_secs(90);
/// How often peers are told about each other, by default.
pub const GOSSIP_INTERVAL: Duration = Duration::from_secs(60);
/// The first wait before retrying a candidate that failed; doubles each time.
pub const RETRY_BASE: Duration = Duration::from_secs(30);
/// The longest wait between retries of a candidate.
pub const RETRY_MAX: Duration = Duration::from_secs(3_600);
/// A gossiped candidate that fails this many times in a row is forgotten.
/// One the operator named is never forgotten, only retried less often.
pub const MAX_FAILURES: u32 = 6;

/// Caps, so neither peers nor strangers can make a relay hold or do much.
#[derive(Clone, Copy, Debug)]
pub struct FedLimits {
    /// Relays peered with at once.
    pub max_peers: usize,
    /// Candidates remembered, named and gossiped together.
    pub max_candidates: usize,
    /// Handshakes answered or started per second, across everyone. Each
    /// costs three Diffie-Hellman agreements.
    pub handshakes_per_sec: u32,
    /// Lookups waiting for an answer at once.
    pub max_lookups: usize,
    /// Lookups one peer may ask of this relay per second.
    pub lookups_per_peer_per_sec: u32,
    /// Calls bridged to peer relays at once.
    pub max_bridges: usize,
    /// Peer with relays at private and loopback addresses. Off for a public
    /// node; on for a LAN or a test.
    pub allow_private: bool,
    /// How often peers are told about each other.
    pub gossip_interval: Duration,
}

impl Default for FedLimits {
    fn default() -> Self {
        Self {
            max_peers: 32,
            max_candidates: 256,
            handshakes_per_sec: 50,
            max_lookups: 1_024,
            lookups_per_peer_per_sec: 200,
            max_bridges: 4_096,
            allow_private: false,
            gossip_interval: GOSSIP_INTERVAL,
        }
    }
}

/// Aggregate counters, the only thing federation reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FedStats {
    /// Relays peered with now.
    pub peers: usize,
    /// Candidates known but not peered with.
    pub candidates: usize,
    /// Calls bridged to a peer relay now.
    pub bridges: usize,
    /// "Have" answers whose proof did not check out, since start.
    pub bad_proofs: u64,
}

/// Names one bridged call; picks its socket in the run loop.
pub type BridgeId = u64;

/// What the run loop should do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Send on the node's own socket.
    Send(SocketAddr, Vec<u8>),
    /// Open a socket for a bridge, and send `first` from it to `relay`.
    OpenBridge {
        /// The bridge this socket belongs to.
        id: BridgeId,
        /// The peer relay the bridge dials.
        relay: SocketAddr,
        /// The first datagram to send: the dial.
        first: Vec<u8>,
    },
    /// Send from a bridge's socket to its peer relay.
    BridgeSend(BridgeId, Vec<u8>),
    /// Close a bridge's socket.
    CloseBridge(BridgeId),
}

/// A relay that might become a peer.
struct Candidate {
    /// The key it must prove, if known. Unknown only for an operator-named
    /// address without one; the first handshake then learns it.
    key: Option<[u8; 32]>,
    /// Named by the operator: never forgotten.
    named: bool,
    /// When to try it next.
    next_try: Instant,
    /// Failures in a row.
    failures: u32,
}

/// A proven peer relay.
struct Peer {
    /// The agreed session.
    session: PeerSession,
    /// When anything last opened under the session.
    last_heard: Instant,
    /// When it was last pinged.
    last_ping: Instant,
    /// Lookups it asked in the current second, and which second.
    lookups: (u32, Instant),
}

/// A lookup waiting for a peer to say it holds the token.
struct Lookup {
    /// Who dialled.
    caller: SocketAddr,
    /// The token they dialled.
    token: RelayToken,
    /// The nonce the proof must cover.
    nonce: [u8; 16],
    /// Peers asked, by address.
    asked: HashSet<SocketAddr>,
    /// When to give up and say "not found".
    deadline: Instant,
}

/// Where a bridge is.
#[derive(Debug, PartialEq, Eq)]
enum BridgeState {
    /// Dialled the peer relay, waiting for "dialed".
    Dialing { deadline: Instant },
    /// Paired: traffic flows.
    Live,
}

/// One call passed through to a peer relay.
struct Bridge {
    /// The caller, on this node's socket.
    caller: SocketAddr,
    /// The peer relay holding the token.
    relay: SocketAddr,
    /// The token, to re-dial and to answer the caller's repeated dials.
    token: RelayToken,
    /// Dialling or live.
    state: BridgeState,
    /// Last traffic either way.
    last_seen: Instant,
}

/// One relay's view of the mesh.
pub struct Federation {
    /// This relay's peer key.
    key: PeerKey,
    /// The caps.
    limits: FedLimits,
    /// Proven peers, by address.
    peers: HashMap<SocketAddr, Peer>,
    /// Handshakes this relay started, waiting for an answer.
    initiating: HashMap<SocketAddr, (Initiator, Instant)>,
    /// Handshakes this relay answered, waiting for the first sealed message
    /// that proves the other side. Kept apart from `peers`, so a spoofed
    /// hello cannot displace a working session.
    responding: HashMap<SocketAddr, (PeerSession, Instant)>,
    /// Relays that might become peers.
    candidates: HashMap<SocketAddr, Candidate>,
    /// Lookups in flight, by id.
    lookups: HashMap<u64, Lookup>,
    /// The lookup for each (caller, token), so a repeated dial does not ask again.
    lookup_for: HashMap<(SocketAddr, RelayToken), u64>,
    /// Bridges, by id.
    bridges: HashMap<BridgeId, Bridge>,
    /// The bridge each caller is on.
    bridge_of: HashMap<SocketAddr, BridgeId>,
    /// Handshakes in the current second, and which second.
    handshakes: (u32, Instant),
    /// The next lookup or bridge id.
    next_id: u64,
    /// When peers were last gossiped to.
    last_gossip: Instant,
    /// Bad proofs seen.
    bad_proofs: u64,
}

impl Federation {
    /// A relay's federation with no peers yet. `named` are the relays the
    /// operator listed, each with its key if pinned.
    pub fn new(key: PeerKey, limits: FedLimits, named: &[(SocketAddr, Option<[u8; 32]>)], now: Instant) -> Self {
        let mut f = Self {
            key,
            limits,
            peers: HashMap::new(),
            initiating: HashMap::new(),
            responding: HashMap::new(),
            candidates: HashMap::new(),
            lookups: HashMap::new(),
            lookup_for: HashMap::new(),
            bridges: HashMap::new(),
            bridge_of: HashMap::new(),
            handshakes: (0, now),
            next_id: 1,
            last_gossip: now,
            bad_proofs: 0,
        };
        for &(addr, key) in named {
            f.candidates.insert(addr, Candidate { key, named: true, next_try: now, failures: 0 });
        }
        f
    }

    /// This relay's public peer key.
    pub fn public(&self) -> [u8; 32] {
        self.key.public()
    }

    /// The counters.
    pub fn stats(&self) -> FedStats {
        FedStats {
            peers: self.peers.len(),
            candidates: self.candidates.len(),
            bridges: self.bridges.len(),
            bad_proofs: self.bad_proofs,
        }
    }

    /// The addresses of the relays peered with now.
    pub fn peer_addrs(&self) -> Vec<SocketAddr> {
        self.peers.keys().copied().collect()
    }

    /// Handles a federation datagram from `from`. `relay` answers lookups.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], relay: &Relay, now: Instant) -> Vec<Action> {
        match Message::decode(bytes) {
            Some(Message::KeyRequest) => vec![Action::Send(from, Message::Key(self.public()).encode())],
            Some(Message::PeersRequest) => {
                let mut addrs: Vec<SocketAddr> =
                    self.peers.keys().copied().filter(|a| self.may_reach(a.ip())).collect();
                addrs.sort();
                addrs.truncate(MAX_LISTED_PEERS);
                vec![Action::Send(from, Message::PeerList(addrs).encode())]
            }
            Some(Message::Hello { static_public, ephemeral_public }) => self.on_hello(from, static_public, ephemeral_public, now),
            Some(Message::HelloBack { static_public, ephemeral_public, confirm }) => {
                self.on_hello_back(from, HelloBack { static_public, ephemeral_public, confirm }, now)
            }
            Some(Message::Sealed(sealed)) => self.on_sealed(from, &sealed, relay, now),
            // Keys, lists and refusals are answers for apps and for
            // handshakes that use them; a relay never asks for one here.
            _ => vec![],
        }
    }

    /// Whether a relay at `ip` may be peered with under the limits.
    fn may_reach(&self, ip: IpAddr) -> bool {
        self.limits.allow_private || crate::onion::is_public(ip)
    }

    /// Takes one handshake from the per-second budget, if any is left.
    fn spend_handshake(&mut self, now: Instant) -> bool {
        if now.duration_since(self.handshakes.1) >= Duration::from_secs(1) {
            self.handshakes = (0, now);
        }
        if self.handshakes.0 >= self.limits.handshakes_per_sec {
            return false;
        }
        self.handshakes.0 += 1;
        true
    }

    /// Answers a hello. The session waits in `responding` until the other
    /// side proves itself by sealing something under it.
    fn on_hello(&mut self, from: SocketAddr, static_public: [u8; 32], ephemeral_public: [u8; 32], now: Instant) -> Vec<Action> {
        if static_public == self.public() || !self.may_reach(from.ip()) {
            return vec![];
        }
        // Both sides started at once: the lower key keeps its own
        // handshake and ignores the other's, so they settle on one session.
        if self.initiating.contains_key(&from) && self.public() < static_public {
            return vec![];
        }
        let room = self.peers.contains_key(&from) || self.peers.len() + self.responding.len() < self.limits.max_peers * 2;
        if !room || !self.spend_handshake(now) {
            return vec![];
        }
        let Ok((session, back)) = respond(&self.key, &Hello { static_public, ephemeral_public }) else { return vec![] };
        self.initiating.remove(&from);
        self.responding.insert(from, (session, now));
        vec![Action::Send(
            from,
            Message::HelloBack { static_public: back.static_public, ephemeral_public: back.ephemeral_public, confirm: back.confirm }
                .encode(),
        )]
    }

    /// Finishes a handshake this relay started, and proves itself at once.
    fn on_hello_back(&mut self, from: SocketAddr, back: HelloBack, now: Instant) -> Vec<Action> {
        let Some((init, _)) = self.initiating.remove(&from) else { return vec![] };
        let Ok(mut session) = init.finish(&self.key, &back) else {
            self.failed(from, now);
            return vec![];
        };
        if session.peer() == self.public() {
            // It was us, by another address.
            self.candidates.remove(&from);
            return vec![];
        }
        let ping = session.seal(&PeerMessage::Ping.encode());
        self.promote(from, session, now);
        vec![Action::Send(from, Message::Sealed(ping).encode())]
    }

    /// Makes `session` the peer at `from`, and remembers its key for retries.
    fn promote(&mut self, from: SocketAddr, session: PeerSession, now: Instant) {
        let key = session.peer();
        let named = self.candidates.get(&from).is_some_and(|c| c.named);
        self.candidates.insert(from, Candidate { key: Some(key), named, next_try: now + RETRY_BASE, failures: 0 });
        self.peers.insert(from, Peer { session, last_heard: now, last_ping: now, lookups: (0, now) });
    }

    /// Notes a failed attempt at a candidate, backing off or forgetting it.
    fn failed(&mut self, addr: SocketAddr, now: Instant) {
        let Some(c) = self.candidates.get_mut(&addr) else { return };
        c.failures += 1;
        if !c.named && c.failures >= MAX_FAILURES {
            self.candidates.remove(&addr);
            return;
        }
        let wait = RETRY_BASE.saturating_mul(1 << c.failures.min(10)).min(RETRY_MAX);
        c.next_try = now + wait;
    }

    /// Opens a sealed message from a peer, or from one still proving itself.
    fn on_sealed(&mut self, from: SocketAddr, sealed: &[u8], relay: &Relay, now: Instant) -> Vec<Action> {
        let plain = match self.peers.get_mut(&from).map(|p| p.session.open(sealed)) {
            Some(Ok(plain)) => plain,
            _ => {
                // Not under the current session: perhaps the first message
                // of one this relay answered, which proves that side.
                let Some((mut session, _)) = self.responding.remove(&from) else { return vec![] };
                let Ok(plain) = session.open(sealed) else {
                    self.responding.insert(from, (session, now));
                    return vec![];
                };
                if self.peers.len() >= self.limits.max_peers && !self.peers.contains_key(&from) {
                    return vec![];
                }
                self.promote(from, session, now);
                plain
            }
        };
        let Some(peer) = self.peers.get_mut(&from) else { return vec![] };
        peer.last_heard = now;
        match PeerMessage::decode(&plain) {
            Some(PeerMessage::Ping) => self.seal_to(from, &PeerMessage::Pong),
            Some(PeerMessage::Lookup { id, hash, nonce }) => {
                if now.duration_since(peer.lookups.1) >= Duration::from_secs(1) {
                    peer.lookups = (0, now);
                }
                if peer.lookups.0 >= self.limits.lookups_per_peer_per_sec {
                    return vec![];
                }
                peer.lookups.0 += 1;
                match relay.find_by_lookup(&hash, now) {
                    Some(token) => {
                        let proof = have_proof(&token, &nonce, &self.public());
                        self.seal_to(from, &PeerMessage::Have { id, proof })
                    }
                    // Silence, not "no": a "no" from every peer would only
                    // tell the asker what the timeout already does.
                    None => vec![],
                }
            }
            Some(PeerMessage::Have { id, proof }) => self.on_have(from, id, proof, now),
            Some(PeerMessage::Gossip(list)) => {
                self.on_gossip(&list, now);
                vec![]
            }
            Some(PeerMessage::Pong) | None => vec![],
        }
    }

    /// Seals `message` to the peer at `to`.
    fn seal_to(&mut self, to: SocketAddr, message: &PeerMessage) -> Vec<Action> {
        let Some(peer) = self.peers.get_mut(&to) else { return vec![] };
        vec![Action::Send(to, Message::Sealed(peer.session.seal(&message.encode())).encode())]
    }

    /// Adds gossiped relays as candidates. Each still has to prove its key.
    fn on_gossip(&mut self, list: &[(SocketAddr, [u8; 32])], now: Instant) {
        for &(addr, key) in list.iter().take(MAX_GOSSIP_PEERS) {
            if key == self.public() || !self.may_reach(addr.ip()) || self.candidates.contains_key(&addr) {
                continue;
            }
            if self.candidates.len() >= self.limits.max_candidates {
                break;
            }
            self.candidates.insert(addr, Candidate { key: Some(key), named: false, next_try: now, failures: 0 });
        }
    }

    /// A peer says it holds the token of lookup `id`. Believed only if the
    /// proof checks out; then the call is bridged to that peer.
    fn on_have(&mut self, from: SocketAddr, id: u64, proof: [u8; 32], now: Instant) -> Vec<Action> {
        let Some(lookup) = self.lookups.get(&id) else { return vec![] };
        let Some(peer_key) = self.peers.get(&from).map(|p| p.session.peer()) else { return vec![] };
        if !lookup.asked.contains(&from) || !check_have_proof(&lookup.token, &lookup.nonce, &peer_key, &proof) {
            self.bad_proofs += 1;
            return vec![];
        }
        let lookup = self.lookups.remove(&id).expect("checked above");
        self.lookup_for.remove(&(lookup.caller, lookup.token));
        if self.bridges.len() >= self.limits.max_bridges {
            return vec![Action::Send(lookup.caller, RelayMessage::NotFound.encode())];
        }
        let mut out = self.close_bridge_of(lookup.caller);
        self.bridges.insert(
            id,
            Bridge {
                caller: lookup.caller,
                relay: from,
                token: lookup.token,
                state: BridgeState::Dialing { deadline: now + BRIDGE_DIAL_TIMEOUT },
                last_seen: now,
            },
        );
        self.bridge_of.insert(lookup.caller, id);
        out.push(Action::OpenBridge { id, relay: from, first: RelayMessage::Dial(lookup.token).encode() });
        out
    }

    /// A caller dialled a token this relay does not hold. Asks the peers,
    /// and returns what to do now; the caller's answer comes later.
    /// Without peers to ask, the answer is "not found" straight away.
    pub fn on_dial_not_found(&mut self, caller: SocketAddr, token: RelayToken, now: Instant) -> Vec<Action> {
        if let Some(b) = self.bridge_of.get(&caller).and_then(|id| self.bridges.get(id)) {
            if b.token == token {
                // A repeated dial: answer it as the bridge stands.
                return match b.state {
                    BridgeState::Live => vec![Action::Send(caller, RelayMessage::Dialed.encode())],
                    BridgeState::Dialing { .. } => vec![],
                };
            }
        }
        if self.lookup_for.contains_key(&(caller, token)) {
            return vec![];
        }
        if self.peers.is_empty() || self.lookups.len() >= self.limits.max_lookups {
            return vec![Action::Send(caller, RelayMessage::NotFound.encode())];
        }
        let mut nonce = [0u8; 16];
        if getrandom::fill(&mut nonce).is_err() {
            return vec![Action::Send(caller, RelayMessage::NotFound.encode())];
        }
        let id = self.take_id();
        let hash = lookup_hash(&token);
        let asked: HashSet<SocketAddr> = self.peers.keys().copied().collect();
        let mut out = Vec::new();
        for &peer in &asked {
            out.extend(self.seal_to(peer, &PeerMessage::Lookup { id, hash, nonce }));
        }
        self.lookups.insert(id, Lookup { caller, token, nonce, asked, deadline: now + LOOKUP_TIMEOUT });
        self.lookup_for.insert((caller, token), id);
        out
    }

    /// A fresh lookup or bridge id.
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Whether `from` is a caller on a bridge.
    pub fn is_bridged(&self, from: SocketAddr) -> bool {
        self.bridge_of.contains_key(&from)
    }

    /// Something from a bridged caller: pass call traffic to the peer relay,
    /// answer a repeated dial, close the bridge on "leave".
    pub fn from_caller(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<Action> {
        let Some(&id) = self.bridge_of.get(&from) else { return vec![] };
        if RelayMessage::is_relay_datagram(bytes) {
            return match RelayMessage::decode(bytes) {
                Some(RelayMessage::Leave) => self.close_bridge(id),
                Some(RelayMessage::Dial(token)) => self.on_dial_not_found(from, token, now),
                _ => vec![],
            };
        }
        let Some(b) = self.bridges.get_mut(&id) else { return vec![] };
        if b.state != BridgeState::Live {
            return vec![];
        }
        b.last_seen = now;
        vec![Action::BridgeSend(id, bytes.to_vec())]
    }

    /// Something arrived on a bridge's socket.
    pub fn from_bridge(&mut self, id: BridgeId, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<Action> {
        let Some(b) = self.bridges.get_mut(&id) else { return vec![] };
        if from != b.relay {
            return vec![];
        }
        if RelayMessage::is_relay_datagram(bytes) {
            return match (RelayMessage::decode(bytes), &b.state) {
                (Some(RelayMessage::Dialed), BridgeState::Dialing { .. }) => {
                    b.state = BridgeState::Live;
                    b.last_seen = now;
                    vec![Action::Send(b.caller, RelayMessage::Dialed.encode())]
                }
                (Some(RelayMessage::NotFound), BridgeState::Dialing { .. }) => {
                    let caller = b.caller;
                    let mut out = self.close_bridge(id);
                    out.push(Action::Send(caller, RelayMessage::NotFound.encode()));
                    out
                }
                _ => vec![],
            };
        }
        if b.state != BridgeState::Live {
            return vec![];
        }
        b.last_seen = now;
        vec![Action::Send(b.caller, bytes.to_vec())]
    }

    /// The run loop could not open a bridge's socket.
    pub fn bridge_failed(&mut self, id: BridgeId) -> Vec<Action> {
        let Some(caller) = self.bridges.get(&id).map(|b| b.caller) else { return vec![] };
        let mut out = self.close_bridge(id);
        out.push(Action::Send(caller, RelayMessage::NotFound.encode()));
        out
    }

    /// Closes the bridge `caller` is on, if any.
    fn close_bridge_of(&mut self, caller: SocketAddr) -> Vec<Action> {
        match self.bridge_of.get(&caller).copied() {
            Some(id) => self.close_bridge(id),
            None => vec![],
        }
    }

    /// Forgets a bridge and closes its socket, telling the peer relay to
    /// forget it too.
    fn close_bridge(&mut self, id: BridgeId) -> Vec<Action> {
        let Some(b) = self.bridges.remove(&id) else { return vec![] };
        if self.bridge_of.get(&b.caller) == Some(&id) {
            self.bridge_of.remove(&b.caller);
        }
        vec![Action::BridgeSend(id, RelayMessage::Leave.encode()), Action::CloseBridge(id)]
    }

    /// Everything on a clock: lookups and bridges that ran out of time,
    /// pings, silent peers, handshakes to start, gossip to send.
    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();

        let expired: Vec<u64> = self.lookups.iter().filter(|(_, l)| l.deadline <= now).map(|(&id, _)| id).collect();
        for id in expired {
            let l = self.lookups.remove(&id).expect("listed above");
            self.lookup_for.remove(&(l.caller, l.token));
            out.push(Action::Send(l.caller, RelayMessage::NotFound.encode()));
        }

        let stale: Vec<BridgeId> = self
            .bridges
            .iter()
            .filter(|(_, b)| match b.state {
                BridgeState::Dialing { deadline } => deadline <= now,
                BridgeState::Live => now.duration_since(b.last_seen) >= BRIDGE_IDLE,
            })
            .map(|(&id, _)| id)
            .collect();
        for id in stale {
            out.extend(self.close_bridge(id));
        }

        self.initiating.retain(|_, (_, at)| now.duration_since(*at) < HANDSHAKE_TIMEOUT);
        self.responding.retain(|_, (_, at)| now.duration_since(*at) < HANDSHAKE_TIMEOUT);

        let silent: Vec<SocketAddr> =
            self.peers.iter().filter(|(_, p)| now.duration_since(p.last_heard) >= PEER_SILENCE).map(|(&a, _)| a).collect();
        for addr in silent {
            self.peers.remove(&addr);
            self.failed(addr, now);
        }
        let due: Vec<SocketAddr> =
            self.peers.iter().filter(|(_, p)| now.duration_since(p.last_ping) >= PING_INTERVAL).map(|(&a, _)| a).collect();
        for addr in due {
            if let Some(p) = self.peers.get_mut(&addr) {
                p.last_ping = now;
            }
            out.extend(self.seal_to(addr, &PeerMessage::Ping));
        }

        out.extend(self.start_handshakes(now));

        if now.duration_since(self.last_gossip) >= self.limits.gossip_interval {
            self.last_gossip = now;
            out.extend(self.gossip());
        }
        out
    }

    /// Starts handshakes with candidates that are due, while there is room.
    fn start_handshakes(&mut self, now: Instant) -> Vec<Action> {
        let mut out = Vec::new();
        let due: Vec<(SocketAddr, Option<[u8; 32]>)> = self
            .candidates
            .iter()
            .filter(|(a, c)| c.next_try <= now && !self.peers.contains_key(a) && !self.initiating.contains_key(a))
            .map(|(&a, c)| (a, c.key))
            .collect();
        for (addr, key) in due {
            if self.peers.len() + self.initiating.len() >= self.limits.max_peers || !self.spend_handshake(now) {
                break;
            }
            let (init, hello) = initiate(&self.key, key);
            self.initiating.insert(addr, (init, now));
            // If this one goes unanswered, it is retried after a back-off;
            // an answer resets that.
            self.failed(addr, now);
            out.push(Action::Send(
                addr,
                Message::Hello { static_public: hello.static_public, ephemeral_public: hello.ephemeral_public }.encode(),
            ));
        }
        out
    }

    /// Tells each peer about the others.
    fn gossip(&mut self) -> Vec<Action> {
        let all: Vec<(SocketAddr, [u8; 32])> = self
            .peers
            .iter()
            .filter(|(a, _)| self.may_reach(a.ip()))
            .map(|(&a, p)| (a, p.session.peer()))
            .collect();
        let addrs: Vec<SocketAddr> = self.peers.keys().copied().collect();
        let mut out = Vec::new();
        for addr in addrs {
            let list: Vec<(SocketAddr, [u8; 32])> =
                all.iter().filter(|(a, _)| *a != addr).take(MAX_GOSSIP_PEERS).copied().collect();
            if !list.is_empty() {
                out.extend(self.seal_to(addr, &PeerMessage::Gossip(list)));
            }
        }
        out
    }
}

/// Parses an operator's `--peer` value: `host:port`, or `host:port#hex`
/// to pin the relay's peer key. Host names are resolved by the caller.
pub fn parse_peer(text: &str) -> Option<(String, Option<[u8; 32]>)> {
    let (addr, key) = match text.trim().split_once('#') {
        Some((a, k)) => (a, Some(k)),
        None => (text.trim(), None),
    };
    if addr.is_empty() {
        return None;
    }
    let key = match key {
        Some(hex) => {
            let bytes = decode_hex(hex.trim())?;
            Some(bytes)
        }
        None => None,
    };
    Some((addr.to_string(), key))
}

/// 64 hex characters to 32 bytes.
fn decode_hex(hex: &str) -> Option<[u8; 32]> {
    antiphon_crypto::hex::decode_array(hex)
}

/// 32 bytes as 64 lowercase hex characters, for printing a relay's key.
pub fn encode_hex(bytes: &[u8; 32]) -> String {
    antiphon_crypto::hex::encode(bytes)
}

/// The answer a relay that does not federate gives to a request that
/// expects one, so the asker fails fast. Never larger than the request.
pub fn refusal_when_disabled(bytes: &[u8]) -> Option<Vec<u8>> {
    match Message::decode(bytes)? {
        Message::KeyRequest | Message::Hello { .. } | Message::PeersRequest => Some(Message::Refused.encode()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A loopback address on `port`.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// Limits that let loopback relays peer.
    fn limits() -> FedLimits {
        FedLimits { allow_private: true, ..FedLimits::default() }
    }

    /// Delivers every `Send` between `a` (at port 1) and `b` (at port 2)
    /// until both fall quiet, returning what was addressed to anyone else.
    fn pump(a: &mut Federation, b: &mut Federation, start: Vec<(SocketAddr, Action)>, now: Instant) -> Vec<Action> {
        let relay = Relay::default();
        let mut queue = std::collections::VecDeque::from(start);
        let mut other = Vec::new();
        while let Some((from, action)) = queue.pop_front() {
            match action {
                Action::Send(to, bytes) if to == addr(1) => {
                    queue.extend(a.handle(from, &bytes, &relay, now).into_iter().map(|x| (addr(1), x)));
                }
                Action::Send(to, bytes) if to == addr(2) => {
                    queue.extend(b.handle(from, &bytes, &relay, now).into_iter().map(|x| (addr(2), x)));
                }
                x => other.push(x),
            }
        }
        other
    }

    /// Two relays at ports 1 and 2, peered (b named a).
    fn peered() -> (Federation, Federation, Instant) {
        let now = Instant::now();
        let mut a = Federation::new(PeerKey::generate(), limits(), &[], now);
        let mut b = Federation::new(PeerKey::generate(), limits(), &[(addr(1), Some(a.public()))], now);
        let start = b.tick(now).into_iter().map(|x| (addr(2), x)).collect();
        pump(&mut a, &mut b, start, now);
        assert_eq!((a.stats().peers, b.stats().peers), (1, 1));
        (a, b, now)
    }

    #[test]
    fn a_named_relay_with_the_wrong_key_never_becomes_a_peer() {
        let now = Instant::now();
        let mut a = Federation::new(PeerKey::generate(), limits(), &[], now);
        let mut b = Federation::new(PeerKey::generate(), limits(), &[(addr(1), Some([9; 32]))], now);
        let start = b.tick(now).into_iter().map(|x| (addr(2), x)).collect();
        pump(&mut a, &mut b, start, now);
        assert_eq!(b.stats().peers, 0);
        assert_eq!(a.stats().peers, 0, "a never saw b prove itself");
    }

    #[test]
    fn a_have_without_proof_is_ignored_and_counted() {
        let (mut a, mut b, now) = peered();
        let caller = addr(9);
        let asks = b.on_dial_not_found(caller, [5; 32], now);
        let Some(Action::Send(_, lookup)) = asks.into_iter().next() else { panic!("b asked nobody") };
        // a does not hold the token, but answers anyway with a made-up proof.
        let Some(Message::Sealed(sealed)) = Message::decode(&lookup) else { panic!() };
        let id = match PeerMessage::decode(&a.peers.get_mut(&addr(2)).unwrap().session.open(&sealed).unwrap()) {
            Some(PeerMessage::Lookup { id, .. }) => id,
            other => panic!("{other:?}"),
        };
        let lie = a.seal_to(addr(2), &PeerMessage::Have { id, proof: [0; 32] });
        let out = pump(&mut a, &mut b, lie.into_iter().map(|x| (addr(1), x)).collect(), now);
        assert!(out.iter().all(|x| !matches!(x, Action::OpenBridge { .. })), "{out:?}");
        assert_eq!(b.stats().bad_proofs, 1);
        assert_eq!(b.stats().bridges, 0);
        // ...and when the lookup runs out, the caller hears "not found".
        let out = b.tick(now + LOOKUP_TIMEOUT);
        assert!(out.contains(&Action::Send(caller, RelayMessage::NotFound.encode())));
    }

    #[test]
    fn a_replayed_peer_message_does_nothing() {
        let (mut a, mut b, now) = peered();
        let ping = a.seal_to(addr(2), &PeerMessage::Ping);
        let Action::Send(_, bytes) = &ping[0] else { panic!() };
        let relay = Relay::default();
        assert_eq!(b.handle(addr(1), bytes, &relay, now).len(), 1, "a pong");
        assert!(b.handle(addr(1), bytes, &relay, now).is_empty(), "a replay is dropped");
    }

    #[test]
    fn a_spoofed_hello_does_not_displace_a_working_peer() {
        let (_a, mut b, now) = peered();
        let mallory = PeerKey::generate();
        let (_, hello) = initiate(&mallory, None);
        let relay = Relay::default();
        let spoof = Message::Hello { static_public: hello.static_public, ephemeral_public: hello.ephemeral_public }.encode();
        b.handle(addr(1), &spoof, &relay, now);
        assert_eq!(b.peers.get(&addr(1)).unwrap().session.peer(), _a.public(), "still the real a");
    }

    #[test]
    fn a_stranger_gets_no_lookup_answers_and_no_peer_traffic() {
        let (mut a, _b, now) = peered();
        let relay = Relay::default();
        // Garbage sealed from an address that is no peer goes nowhere.
        assert!(a.handle(addr(7), &Message::Sealed(vec![0; 64]).encode(), &relay, now).is_empty());
    }

    #[test]
    fn gossip_of_ourselves_or_of_private_addresses_is_not_taken() {
        let now = Instant::now();
        let mut f = Federation::new(PeerKey::generate(), FedLimits::default(), &[], now);
        let me = f.public();
        f.on_gossip(&[(addr(5), [1; 32]), ("203.0.113.9:7100".parse().unwrap(), me)], now);
        assert_eq!(f.stats().candidates, 0);
        f.on_gossip(&[("203.0.113.9:7100".parse().unwrap(), [1; 32])], now);
        assert_eq!(f.stats().candidates, 1);
    }

    #[test]
    fn a_peer_argument_takes_an_optional_pinned_key() {
        assert_eq!(parse_peer("r.example:7100"), Some(("r.example:7100".into(), None)));
        let hex = encode_hex(&[0xab; 32]);
        assert_eq!(parse_peer(&format!("r.example:7100#{hex}")), Some(("r.example:7100".into(), Some([0xab; 32]))));
        assert_eq!(parse_peer("r.example:7100#nothex"), None);
    }
}
