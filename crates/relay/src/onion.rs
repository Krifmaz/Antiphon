//! Hops for anonymity mode: this node as one link of a caller's circuit
//! (see `antiphon_net::onion` for the whole picture and the wire format).
//!
//! **Off by default** (`antiphon-relay --hops`). When on, the node holds a
//! hop key ([`HopSecret`]) that peels one routing layer per cell and does
//! nothing else: it is not a call key, it never sees call content, and a
//! node whose hop key leaks gives away which neighbours its circuits ran
//! between -- never what was said.
//!
//! What a hop holds, per circuit, and for how long:
//!
//! - **The two neighbouring addresses and the link-local circuit ids**,
//!   because forwarding needs them. For the entry one neighbour is the
//!   caller; for the exit the other is the target relay. No hop is told
//!   both, and nothing says which position a hop holds.
//! - **The circuit's layer keys**, in memory only.
//! - **At an exit, one UDP socket**, so the target relay sees each circuit
//!   as a separate ordinary client rather than one address carrying many.
//!
//! All of it is dropped [`HopLimits::idle`] after the last cell, or at a
//! DESTROY. Nothing is logged; [`HopStats`] are totals.
//!
//! An exit is an open door to the rest of the internet, so it is kept
//! narrow: it sends only to the one target named when the circuit was
//! built, accepts replies only from that target, carries only Antiphon
//! traffic (relay, mailbox and file requests and sealed protocol frames),
//! and by default will not reach private or loopback addresses -- an
//! operator's own LAN is not something strangers should be able to probe.
//!
//! Like [`crate::Relay`], this is a socket-free state machine: it is handed
//! datagrams and returns [`Action`]s; [`crate::run_node_with_hops`] does
//! the I/O, including the exit sockets.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use antiphon_crypto::onion::{HopSecret, LayerKeys};
use antiphon_net::onion::{exit_backward, hop_backward, hop_forward, open_create, pad_create, Cell, Setup, CREATED_MARK};

/// Bounds on what a hop node holds, so offering hops cannot be turned into
/// a way to exhaust the operator's memory or sockets.
#[derive(Clone, Copy, Debug)]
pub struct HopLimits {
    /// Circuits held at once, across everyone.
    pub max_circuits: usize,
    /// Circuits any one neighbouring address may hold. A busy entry relay
    /// is one address carrying many people's circuits, so this is a
    /// flood guard, not a per-person quota.
    pub max_circuits_per_addr: usize,
    /// How long a circuit survives with no traffic either way. A call
    /// sends every 20 ms and a listener re-registers every 20 s, so this
    /// only fires once a circuit has been abandoned.
    pub idle: Duration,
    /// How long a half-built circuit waits for the next hop.
    pub pending: Duration,
    /// Let circuits extend or exit to private and loopback addresses.
    /// Off for a public node; on for a LAN or a test.
    pub allow_private: bool,
}

impl Default for HopLimits {
    fn default() -> Self {
        Self {
            max_circuits: 20_000,
            max_circuits_per_addr: 512,
            idle: Duration::from_secs(120),
            pending: Duration::from_secs(10),
            allow_private: false,
        }
    }
}

/// Aggregate counters -- all a hop can report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HopStats {
    /// Circuits held right now.
    pub circuits: usize,
    /// Of those, how many this node is the exit for.
    pub exits: usize,
    /// Cells forwarded in either direction since start.
    pub cells: u64,
    /// CREATEs refused for limits or a forbidden next hop or target.
    pub refused: u64,
}

/// A circuit's handle at this node, and the name of its exit socket if
/// this node is its exit.
pub type CircuitId = u64;

/// What the I/O loop must do.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Send from the node's own socket.
    Send(SocketAddr, Vec<u8>),
    /// Open a fresh socket for this circuit's exit traffic. Comes before
    /// any [`Action::ExitSend`] for it.
    OpenExit(CircuitId),
    /// Send from the circuit's exit socket.
    ExitSend(CircuitId, SocketAddr, Vec<u8>),
    /// Close the circuit's exit socket.
    CloseExit(CircuitId),
}

/// Where a circuit goes after this node.
enum Next {
    /// Extended, waiting for the next hop's CREATED.
    Pending {
        /// The next hop's address.
        addr: SocketAddr,
        /// The circuit id this node chose on the link to it.
        circ: u32,
    },
    /// Extended and confirmed: forward to the next hop.
    Linked {
        /// The next hop's address.
        addr: SocketAddr,
        /// The circuit id on the link to it.
        circ: u32,
    },
    /// This node is the exit and sends to `target` from its own socket.
    Exit {
        /// The only address the exit sends to or accepts replies from.
        target: SocketAddr,
    },
}

/// One circuit as this node sees it: its neighbours, never the whole path.
struct Circuit {
    /// The neighbour towards the caller.
    prev: SocketAddr,
    /// The circuit id on the link to `prev`.
    prev_circ: u32,
    /// Where traffic away from the caller goes.
    next: Next,
    /// This hop's layer keys for the circuit, in memory only.
    keys: LayerKeys,
    /// This hop's place on the path, from its setup: how much of each data
    /// cell is filler (see `antiphon_net::onion` on cell sizes).
    position: u8,
    /// The next backward sequence number under `keys`.
    back_seq: u64,
    /// When a cell last passed, for idle expiry.
    last_seen: Instant,
}

/// The hop side of anonymity mode: a socket-free state machine holding the
/// hop key and every circuit through this node.
pub struct HopNode {
    /// The hop key that peels this node's layers.
    secret: HopSecret,
    /// `secret`'s public half, cached for HOPKEY answers.
    public: [u8; 32],
    /// Limits on circuits held.
    limits: HopLimits,
    /// Every circuit, by this node's own handle.
    circuits: HashMap<CircuitId, Circuit>,
    /// Circuits by the caller-side neighbour and link id.
    by_prev: HashMap<(SocketAddr, u32), CircuitId>,
    /// Circuits by the far-side neighbour and link id.
    by_next: HashMap<(SocketAddr, u32), CircuitId>,
    /// How many circuits each caller-side neighbour holds, for
    /// [`HopLimits::max_circuits_per_addr`].
    per_addr: HashMap<SocketAddr, usize>,
    /// The handle the next circuit gets.
    next_id: CircuitId,
    /// Cells forwarded, for [`HopStats`].
    cells: u64,
    /// CREATEs refused, for [`HopStats`].
    refused: u64,
    /// When idle circuits were last swept, so sweeping stays cheap.
    last_sweep: Option<Instant>,
}

impl HopNode {
    /// A hop node with `secret` as its hop key and no circuits.
    pub fn new(secret: HopSecret, limits: HopLimits) -> Self {
        Self {
            public: secret.public(),
            secret,
            limits,
            circuits: HashMap::new(),
            by_prev: HashMap::new(),
            by_next: HashMap::new(),
            per_addr: HashMap::new(),
            next_id: 1,
            cells: 0,
            refused: 0,
            last_sweep: None,
        }
    }

    /// The hop key's public half: what callers build circuits to.
    pub fn public_key(&self) -> [u8; 32] {
        self.public
    }

    /// Totals for the operator; nothing per circuit or per address.
    pub fn stats(&self) -> HopStats {
        HopStats {
            circuits: self.circuits.len(),
            exits: self.circuits.values().filter(|c| matches!(c.next, Next::Exit { .. })).count(),
            cells: self.cells,
            refused: self.refused,
        }
    }

    /// Handles one onion datagram arriving at the node's socket.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<Action> {
        let mut out = self.sweep_if_due(now);
        let Some(cell) = Cell::decode(bytes) else { return out };
        match cell {
            Cell::HopKeyRequest => out.push(Action::Send(from, Cell::HopKey(self.public).encode())),
            Cell::Create { circ, ephemeral, sealed } => out.extend(self.create(from, circ, &ephemeral, &sealed, now)),
            Cell::Created { circ, layer } => {
                let Some(&id) = self.by_next.get(&(from, circ)) else { return out };
                let c = self.circuits.get_mut(&id).expect("indexed circuits exist");
                let Next::Pending { addr, circ } = c.next else { return out };
                c.next = Next::Linked { addr, circ };
                c.last_seen = now;
                let Some(layer) = hop_backward(&c.keys, &mut c.back_seq, c.position, &layer) else { return out };
                out.push(Action::Send(c.prev, Cell::Created { circ: c.prev_circ, layer }.encode()));
            }
            Cell::Forward { circ, layer } => {
                let Some(&id) = self.by_prev.get(&(from, circ)) else {
                    // Not a circuit this node knows -- it restarted, or forgot
                    // an idle one. Saying so lets the caller rebuild instead
                    // of talking into silence. Smaller than what came in.
                    out.push(Action::Send(from, Cell::Destroy { circ }.encode()));
                    return out;
                };
                let c = self.circuits.get_mut(&id).expect("indexed circuits exist");
                let exit = matches!(c.next, Next::Exit { .. });
                let Some(inner) = hop_forward(&c.keys, c.position, exit, &layer) else { return out };
                c.last_seen = now;
                match c.next {
                    Next::Linked { addr, circ } => out.push(Action::Send(addr, Cell::Forward { circ, layer: inner }.encode())),
                    Next::Exit { target } if exit_may_carry(&inner) => out.push(Action::ExitSend(id, target, inner)),
                    _ => return out,
                }
                self.cells += 1;
            }
            Cell::Backward { circ, layer } => {
                let Some(&id) = self.by_next.get(&(from, circ)) else {
                    out.push(Action::Send(from, Cell::Destroy { circ }.encode()));
                    return out;
                };
                let c = self.circuits.get_mut(&id).expect("indexed circuits exist");
                if !matches!(c.next, Next::Linked { .. }) {
                    return out;
                }
                c.last_seen = now;
                let Some(layer) = hop_backward(&c.keys, &mut c.back_seq, c.position, &layer) else { return out };
                out.push(Action::Send(c.prev, Cell::Backward { circ: c.prev_circ, layer }.encode()));
                self.cells += 1;
            }
            Cell::Destroy { circ } => {
                // From the caller's side: pass it on towards the exit.
                if let Some(&id) = self.by_prev.get(&(from, circ)) {
                    if let Some(c) = self.circuits.get(&id) {
                        if let Next::Pending { addr, circ } | Next::Linked { addr, circ } = c.next {
                            out.push(Action::Send(addr, Cell::Destroy { circ }.encode()));
                        }
                    }
                    out.extend(self.remove(id));
                // From the far side: pass it back towards the caller.
                } else if let Some(&id) = self.by_next.get(&(from, circ)) {
                    if let Some(c) = self.circuits.get(&id) {
                        out.push(Action::Send(c.prev, Cell::Destroy { circ: c.prev_circ }.encode()));
                    }
                    out.extend(self.remove(id));
                }
            }
            Cell::Refused { circ } => {
                let Some(&id) = self.by_next.get(&(from, circ)) else { return out };
                let c = &self.circuits[&id];
                if matches!(c.next, Next::Pending { .. }) {
                    out.push(Action::Send(c.prev, Cell::Refused { circ: c.prev_circ }.encode()));
                    out.extend(self.remove(id));
                }
            }
            // Replies are for callers.
            Cell::HopKey(_) => {}
        }
        out
    }

    /// Handles a datagram arriving at circuit `id`'s exit socket: a reply
    /// from the target, wrapped and sent back towards the caller.
    pub fn handle_exit(&mut self, id: CircuitId, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<Action> {
        let Some(c) = self.circuits.get_mut(&id) else { return vec![] };
        let Next::Exit { target } = c.next else { return vec![] };
        if from != target {
            // The exit socket answers to its one target and nobody else.
            return vec![];
        }
        c.last_seen = now;
        let Some(layer) = exit_backward(&c.keys, &mut c.back_seq, c.position, bytes) else { return vec![] };
        self.cells += 1;
        vec![Action::Send(c.prev, Cell::Backward { circ: c.prev_circ, layer }.encode())]
    }

    /// Forgets a circuit whose exit socket could not be opened, telling
    /// the caller's side.
    pub fn exit_failed(&mut self, id: CircuitId) -> Vec<Action> {
        let Some(c) = self.circuits.get(&id) else { return vec![] };
        let mut out = vec![Action::Send(c.prev, Cell::Destroy { circ: c.prev_circ }.encode())];
        out.extend(self.remove(id));
        out
    }

    /// Handles a CREATE: opens this hop's setup, then extends or becomes the
    /// exit. Undecryptable CREATEs are dropped silently.
    fn create(&mut self, from: SocketAddr, circ: u32, ephemeral: &[u8; 32], sealed: &[u8], now: Instant) -> Vec<Action> {
        if self.by_prev.contains_key(&(from, circ)) {
            // A resend, or a clashing id: either way the circuit already
            // on this id stands.
            return vec![];
        }
        let held = self.per_addr.get(&from).copied().unwrap_or(0);
        if self.circuits.len() >= self.limits.max_circuits || held >= self.limits.max_circuits_per_addr {
            return self.refuse(from, circ);
        }
        // Garbage, or a CREATE for some other hop key, is dropped without
        // a word: answering would confirm to a scanner that keys differ.
        let Ok(keys) = self.secret.keys_for(ephemeral) else { return vec![] };
        let Some(setup) = open_create(&keys, sealed) else { return vec![] };

        let id = self.next_id;
        self.next_id += 1;
        let mut out = Vec::new();
        let position = match setup {
            Setup::Extend { position, .. } | Setup::Exit { position, .. } => position,
        };
        let next = match setup {
            Setup::Extend { next, ephemeral, sealed, .. } => {
                if !self.may_reach(next.ip()) || next == from {
                    return self.refuse(from, circ);
                }
                let next_circ = loop {
                    let c = random_u32();
                    if !self.by_next.contains_key(&(next, c)) {
                        break c;
                    }
                };
                self.by_next.insert((next, next_circ), id);
                out.push(Action::Send(next, Cell::Create { circ: next_circ, ephemeral, sealed: pad_create(sealed) }.encode()));
                Next::Pending { addr: next, circ: next_circ }
            }
            Setup::Exit { target, .. } => {
                if !self.may_reach(target.ip()) {
                    return self.refuse(from, circ);
                }
                out.push(Action::OpenExit(id));
                Next::Exit { target }
            }
        };
        let mut c = Circuit { prev: from, prev_circ: circ, next, keys, position, back_seq: 0, last_seen: now };
        if matches!(c.next, Next::Exit { .. }) {
            let Some(layer) = exit_backward(&c.keys, &mut c.back_seq, c.position, CREATED_MARK) else { return vec![] };
            out.push(Action::Send(from, Cell::Created { circ, layer }.encode()));
        }
        self.circuits.insert(id, c);
        self.by_prev.insert((from, circ), id);
        *self.per_addr.entry(from).or_default() += 1;
        out
    }

    /// Answers REFUSED on the caller's side and counts it.
    fn refuse(&mut self, to: SocketAddr, circ: u32) -> Vec<Action> {
        self.refused += 1;
        vec![Action::Send(to, Cell::Refused { circ }.encode())]
    }

    /// Whether a circuit may extend or exit to `ip` under the limits.
    fn may_reach(&self, ip: IpAddr) -> bool {
        self.limits.allow_private || is_public(ip)
    }

    /// Forgets a circuit and its indexes, closing its exit socket if it had one.
    fn remove(&mut self, id: CircuitId) -> Vec<Action> {
        let Some(c) = self.circuits.remove(&id) else { return vec![] };
        self.by_prev.remove(&(c.prev, c.prev_circ));
        if let Some(n) = self.per_addr.get_mut(&c.prev) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_addr.remove(&c.prev);
            }
        }
        match c.next {
            Next::Pending { addr, circ } | Next::Linked { addr, circ } => {
                self.by_next.remove(&(addr, circ));
                vec![]
            }
            Next::Exit { .. } => vec![Action::CloseExit(id)],
        }
    }

    /// Sweeps at most every five seconds, from the datagram path.
    fn sweep_if_due(&mut self, now: Instant) -> Vec<Action> {
        if self.last_sweep.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return vec![];
        }
        self.last_sweep = Some(now);
        self.sweep(now)
    }

    /// Drops idle and stalled circuits. Expiry is the retention policy:
    /// nothing outlives it, because nothing is written anywhere else.
    pub fn sweep(&mut self, now: Instant) -> Vec<Action> {
        let (idle, pending) = (self.limits.idle, self.limits.pending);
        // Collected first: `remove` needs the whole of `self`.
        #[expect(clippy::needless_collect, reason = "remove borrows self mutably while circuits is borrowed")]
        let expired: Vec<CircuitId> = self
            .circuits
            .iter()
            .filter(|(_, c)| {
                let age = now.duration_since(c.last_seen);
                age >= idle || (matches!(c.next, Next::Pending { .. }) && age >= pending)
            })
            .map(|(&id, _)| id)
            .collect();
        expired.into_iter().flat_map(|id| self.remove(id)).collect()
    }
}

/// What an exit will send on a caller's behalf: Antiphon's own services
/// (every one starts `0xA7`: relay, mailbox, files, hops) and protocol
/// frames, which is what a call is. Anything else -- DNS queries, a game
/// server, an amplification probe -- stays inside the circuit.
fn exit_may_carry(payload: &[u8]) -> bool {
    payload.first() == Some(&0xA7) || antiphon_protocol::decode(payload).is_ok()
}

/// Neither loopback, private, link-local, multicast nor unspecified.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
                // 100.64.0.0/10, carrier-grade NAT: somebody's private network.
                || (v4.octets()[0] == 100 && v4.octets()[1] & 0xC0 == 64))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || first & 0xfe00 == 0xfc00 // unique local
                || first & 0xffc0 == 0xfe80) // link local
        }
    }
}

/// A random link-local circuit id.
fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    u32::from_be_bytes(b)
}

/// Reads the hop key from `path`, or makes one and writes it there (owner
/// read/write only, where the OS has such a thing). Keeping it means the
/// public half people have learned stays valid across restarts. It is 32
/// raw bytes; losing it costs nothing but that.
pub fn load_or_create_hop_key(path: &std::path::Path) -> std::io::Result<HopSecret> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "a hop key file holds exactly 32 bytes")
            })?;
            Ok(HopSecret::from_bytes(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let secret = HopSecret::generate();
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            std::io::Write::write_all(&mut options.open(path)?, &secret.to_bytes())?;
            Ok(secret)
        }
        Err(e) => Err(e),
    }
}

/// The answer a node without hops gives, so a caller fails fast rather
/// than timing out. Only to requests that are at least as large as it.
pub fn refusal_when_disabled(bytes: &[u8]) -> Option<Vec<u8>> {
    match Cell::decode(bytes)? {
        Cell::HopKeyRequest => Some(Cell::Refused { circ: 0 }.encode()),
        Cell::Create { circ, .. } => Some(Cell::Refused { circ }.encode()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_net::onion::{Layers, Plan};

    /// A private address on `port`, reachable only with [`open`] limits.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    /// A documentation-range address that counts as public.
    fn public(port: u16) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, 7], port))
    }

    /// A hop node with a fresh hop key.
    fn node(limits: HopLimits) -> HopNode {
        HopNode::new(HopSecret::generate(), limits)
    }

    /// Default limits, but letting circuits reach private addresses.
    fn open() -> HopLimits {
        HopLimits { allow_private: true, ..HopLimits::default() }
    }

    /// The cells among `actions` sent from the node's own socket, decoded.
    fn sent(actions: &[Action]) -> Vec<(SocketAddr, Cell)> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Send(to, b) => Some((*to, Cell::decode(b).unwrap())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_hop_hands_out_its_key() {
        let mut h = node(open());
        let out = h.handle(addr(1), &Cell::HopKeyRequest.encode(), Instant::now());
        assert_eq!(out, vec![Action::Send(addr(1), Cell::HopKey(h.public_key()).encode())]);
    }

    /// Two hops driven by hand: the caller's single CREATE becomes a
    /// circuit in one round trip, confirmed by both hops. Returns the
    /// caller's layers and the exit's circuit id.
    fn build_two_hops(
        h1: &mut HopNode,
        h2: &mut HopNode,
        (caller, h1_addr, h2_addr, target): (SocketAddr, SocketAddr, SocketAddr, SocketAddr),
        now: Instant,
    ) -> (Layers, CircuitId) {
        let plan = Plan::new(&[(h1_addr, h1.public_key()), (h2_addr, h2.public_key())], target).unwrap();

        // Caller -> H1: H1 extends to H2 under an id of its own choosing.
        let out = h1.handle(caller, &plan.create, now);
        let [(to, Cell::Create { circ, .. })] = sent(&out)[..] else { panic!("{out:?}") };
        assert_eq!(to, h2_addr);
        assert_ne!(circ, plan.circ, "ids are per link");
        let Action::Send(_, create2) = &out[0] else { panic!() };

        // H1 -> H2: H2 is the exit; it opens a socket and confirms.
        let out = h2.handle(h1_addr, create2, now);
        let Action::OpenExit(exit_id) = out[0] else { panic!("{out:?}") };
        assert_eq!(h2.stats().exits, 1);
        let Action::Send(to, created) = &out[1] else { panic!() };
        assert_eq!(*to, h1_addr);

        // H2 -> H1 -> caller, wrapped twice, and the caller can check it.
        let out = h1.handle(h2_addr, created, now);
        let Action::Send(to, created) = &out[0] else { panic!() };
        assert_eq!(*to, caller);
        assert!(plan.confirms(created));
        (plan.into_layers(), exit_id)
    }

    #[test]
    fn a_full_round_trip_through_the_state_machines() {
        let now = Instant::now();
        let (caller, h1_addr, h2_addr, target) = (addr(1), addr(2), addr(3), addr(3479));
        let mut h1 = node(open());
        let mut h2 = node(open());
        let (mut layers, exit_id) = build_two_hops(&mut h1, &mut h2, (caller, h1_addr, h2_addr, target), now);
        let circ = layers.circ();

        // Out: caller -> H1 -> H2 -> plain UDP to the target.
        let out = h1.handle(caller, &layers.wrap(b"\xA7RL1dial"), now);
        let Action::Send(to, cell) = &out[0] else { panic!() };
        assert_eq!(*to, h2_addr);
        let out = h2.handle(h1_addr, cell, now);
        assert_eq!(out, vec![Action::ExitSend(exit_id, target, b"\xA7RL1dial".to_vec())]);

        // Back: target -> exit socket -> H1 -> caller.
        assert!(h2.handle_exit(exit_id, addr(66), b"\xA7RL1spoof", now).is_empty(), "only the target answers");
        let out = h2.handle_exit(exit_id, target, b"\xA7RL1ok", now);
        let Action::Send(to, cell) = &out[0] else { panic!() };
        assert_eq!(*to, h1_addr);
        let out = h1.handle(h2_addr, cell, now);
        assert_eq!(out.len(), 1);
        let Action::Send(to, cell) = &out[0] else { panic!() };
        assert_eq!(*to, caller);
        assert_eq!(layers.unwrap(cell).unwrap(), b"\xA7RL1ok");

        // Non-Antiphon payloads never leave the exit.
        let out = h1.handle(caller, &layers.wrap(b"GET / HTTP/1.1\r\n"), now);
        let Action::Send(_, cell) = &out[0] else { panic!() };
        assert!(h2.handle(h1_addr, cell, now).is_empty());

        // Destroy from the caller clears both hops and the exit socket.
        let out = h1.handle(caller, &Cell::Destroy { circ }.encode(), now);
        let Action::Send(to, cell) = &out[0] else { panic!() };
        assert_eq!(h1.stats().circuits, 0);
        assert_eq!(*to, h2_addr);
        assert_eq!(h2.handle(h1_addr, cell, now), vec![Action::CloseExit(exit_id)]);
        assert_eq!(h2.stats().circuits, 0);
    }

    #[test]
    fn exits_to_private_addresses_are_refused_on_a_public_node() {
        let now = Instant::now();
        let mut h = node(HopLimits::default());
        let plan = Plan::new(&[(public(1), h.public_key())], addr(3479)).unwrap();
        let out = h.handle(public(9), &plan.create, now);
        assert_eq!(sent(&out), vec![(public(9), Cell::Refused { circ: plan.circ })]);
        let plan = Plan::new(&[(public(1), h.public_key())], public(3479)).unwrap();
        assert!(matches!(h.handle(public(9), &plan.create, now)[0], Action::OpenExit(_)));
    }

    #[test]
    fn a_create_for_another_key_is_dropped_silently() {
        let mut h = node(open());
        let plan = Plan::new(&[(addr(2), HopSecret::generate().public())], addr(3479)).unwrap();
        assert!(h.handle(addr(1), &plan.create, Instant::now()).is_empty());
        assert_eq!(h.stats().circuits, 0);
    }

    #[test]
    fn circuit_caps_hold() {
        let now = Instant::now();
        let mut h = node(HopLimits { max_circuits_per_addr: 2, max_circuits: 3, ..open() });
        let create = |h: &HopNode| Plan::new(&[(addr(2), h.public_key())], addr(3479)).unwrap();
        for _ in 0..2 {
            assert!(matches!(h.handle(addr(1), &create(&h).create, now)[0], Action::OpenExit(_)));
        }
        let p = create(&h);
        assert_eq!(sent(&h.handle(addr(1), &p.create, now)), vec![(addr(1), Cell::Refused { circ: p.circ })]);
        assert!(matches!(h.handle(addr(5), &create(&h).create, now)[0], Action::OpenExit(_)));
        let p = create(&h);
        assert_eq!(sent(&h.handle(addr(6), &p.create, now)), vec![(addr(6), Cell::Refused { circ: p.circ })]);
        assert_eq!(h.stats().refused, 2);
    }

    #[test]
    fn idle_and_stalled_circuits_are_forgotten() {
        let t0 = Instant::now();
        let mut h = node(open());
        // An exit circuit, and an extension that the next hop never answers.
        let exit = Plan::new(&[(addr(2), h.public_key())], addr(3479)).unwrap();
        let Action::OpenExit(exit_id) = h.handle(addr(1), &exit.create, t0)[0] else { panic!() };
        let stalled =
            Plan::new(&[(addr(2), h.public_key()), (addr(3), HopSecret::generate().public())], addr(3479)).unwrap();
        h.handle(addr(1), &stalled.create, t0);
        assert_eq!(h.stats().circuits, 2);

        let out = h.sweep(t0 + HopLimits::default().pending);
        assert!(out.is_empty());
        assert_eq!(h.stats().circuits, 1, "the stalled extension went");
        let out = h.sweep(t0 + HopLimits::default().idle);
        assert_eq!(out, vec![Action::CloseExit(exit_id)]);
        assert_eq!(h.stats(), HopStats { circuits: 0, exits: 0, cells: 0, refused: 0 });
    }

    #[test]
    fn a_refusal_further_on_unwinds_back_to_the_caller() {
        let now = Instant::now();
        let mut h1 = node(open());
        let plan =
            Plan::new(&[(addr(2), h1.public_key()), (addr(3), HopSecret::generate().public())], addr(3479)).unwrap();
        let out = h1.handle(addr(1), &plan.create, now);
        let [(_, Cell::Create { circ, .. })] = sent(&out)[..] else { panic!() };
        let out = h1.handle(addr(3), &Cell::Refused { circ }.encode(), now);
        assert_eq!(sent(&out), vec![(addr(1), Cell::Refused { circ: plan.circ })]);
        assert_eq!(h1.stats().circuits, 0);
    }

    #[test]
    fn a_disabled_node_says_so() {
        assert_eq!(refusal_when_disabled(&Cell::HopKeyRequest.encode()), Some(Cell::Refused { circ: 0 }.encode()));
        assert!(refusal_when_disabled(&Cell::Forward { circ: 1, layer: vec![0; 30] }.encode()).is_none());
    }

    #[test]
    fn private_ranges_are_recognised() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.5.4", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1", "0.0.0.0"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["198.51.100.7", "1.1.1.1", "2001:db8::1", "::ffff:8.8.8.8"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }
}
