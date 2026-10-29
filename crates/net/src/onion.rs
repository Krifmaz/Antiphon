//! Anonymity mode: reaching a relay through two or three other relays, so
//! that no single one of them knows both who is calling and whom.
//!
//! A plain relayed call ([`crate::relay`]) already hides the two people
//! from each other, but the relay itself sees the caller's IP address next
//! to the token they dial. A circuit splits that knowledge up:
//!
//! ```text
//! caller --> H1 --> H2 [--> H3] ==plain UDP==> target relay <-- listener
//!   sees:    caller IP    H1, H3       H2, target,  exit's address, token
//!            and H2 only  only         token
//! ```
//!
//! - The **entry** (H1) knows the caller's address, and only that the next
//!   hop is H2. Where the circuit ends is sealed to keys it does not hold.
//! - A **middle** hop knows its two neighbours and nothing else.
//! - The **exit** (the last hop) sends the inner datagrams as ordinary UDP
//!   from a socket of its own, so to the target relay it is just another
//!   client: it sees the relay DIAL or REGISTER and the sealed call frames
//!   -- which it cannot read -- but never the caller's address.
//! - The **target relay** sees the token and the exit's address, as it
//!   would for any relayed call, and nothing of the caller.
//!
//! No hop ever holds a call key. What is layered here is routing only; the
//! call inside is the same end-to-end sealed [`crate::PeerConnection`]
//! traffic as always. Hops can still *collude*: an entry and an exit that
//! compare packet timings can link the two ends, which is true of every
//! low-latency mix network and is why the hops should be run by different
//! people.
//!
//! # Using it from call code
//!
//! [`OnionShim`] turns a circuit into a local UDP socket, so the existing
//! call code runs over it unchanged: aim [`crate::relay::dial`],
//! [`crate::relay::register`] and [`crate::PeerConnection`] at
//! [`OnionShim::relay_addr`] (a loopback address standing in for the
//! target relay) on [`OnionShim::socket`].
//!
//! ```ignore
//! let keys = [fetch_hop_key(&probe, h1, t).await?, fetch_hop_key(&probe, h2, t).await?];
//! let shim = Circuit::build(&[(h1, keys[0]), (h2, keys[1])], target_relay).await?.into_shim().await?;
//! relay::dial(&shim.socket(), shim.relay_addr(), token, t).await?;
//! let call = PeerConnection::connect(shim.socket(), shim.relay_addr(), &me).await?;
//! ```
//!
//! # Wire format
//!
//! Every cell starts with [`MAGIC`]. Circuit ids are chosen by whoever
//! sends the CREATE on a link and mean something only on that link: each
//! hop picks a fresh one for the next link, so ids say nothing end to end.
//!
//! ```text
//! HOPKEY?   MAGIC 0x01 [zero; to HOPKEY_REQUEST_LEN]  "what is your hop key?"
//! CREATE    MAGIC 0x02 circ:u32 eph:[32] sealed-setup
//! FORWARD   MAGIC 0x03 circ:u32 layer                  away from the caller
//! DESTROY   MAGIC 0x04 circ:u32                        tear down, both ways
//! HOPKEY    MAGIC 0x81 key:[32]
//! CREATED   MAGIC 0x82 circ:u32 layer                  built, confirmed by every hop
//! BACKWARD  MAGIC 0x83 circ:u32 layer                  towards the caller
//! REFUSED   MAGIC 0x84 circ:u32                        full, disabled, or bad exit
//! ```
//!
//! The setup sealed to each hop says either "extend to ADDR with this
//! CREATE body" or "exit to ADDR". A circuit is built in one round trip:
//! the caller nests every hop's setup inside the previous one's, each hop
//! peels its own and passes the rest on, and the exit's confirmation comes
//! back wrapped by every hop, so the caller knows the whole path holds.
//!
//! # Known limitations
//!
//! - **No fixed-size cells.** Each layer adds
//!   [`antiphon_crypto::onion::LAYER_OVERHEAD`] bytes, so a hop can tell
//!   from sizes roughly where on the path it sits -- in particular, the
//!   entry can tell its neighbour is the caller. It learns nothing it
//!   could not guess from the caller's address being a home connection,
//!   and still nothing about the far end.
//! - **No replay window at hops.** A replayed cell is forwarded again; the
//!   call's own session rejects the duplicate end to end.
//! - **Hop keys come from the hop itself** ([`fetch_hop_key`]), which an
//!   attacker on the path to it could substitute. Pinning or a signed
//!   directory is future work.
//! - **A broken circuit is not rebuilt.** If a hop restarts or forgets an
//!   idle circuit, traffic stops; the caller builds a new one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use antiphon_crypto::onion::{client_keys, LayerKeys};
use tokio::net::UdpSocket;

use crate::AppSocket;
use tokio::task::JoinHandle;

use crate::error::NetError;

/// Distinct from the relay's, mailbox's and blob's magic, and like theirs
/// never a valid protocol frame version.
pub const MAGIC: [u8; 4] = [0xA7, b'O', b'N', b'1'];

/// Caller asks a node for its hop key.
pub const OP_HOPKEY_REQUEST: u8 = 0x01;
/// Builds a circuit: an ephemeral key and the setup sealed to this hop.
pub const OP_CREATE: u8 = 0x02;
/// A data layer travelling away from the caller.
pub const OP_FORWARD: u8 = 0x03;
/// Tears a circuit down, in either direction.
pub const OP_DESTROY: u8 = 0x04;
/// A node's answer to [`OP_HOPKEY_REQUEST`]: its hop public key.
pub const OP_HOPKEY: u8 = 0x81;
/// The circuit is built; the layer wraps [`CREATED_MARK`] once per hop.
pub const OP_CREATED: u8 = 0x82;
/// A data layer travelling back towards the caller.
pub const OP_BACKWARD: u8 = 0x83;
/// The hop will not carry this circuit: full, hops disabled, or a bad exit.
pub const OP_REFUSED: u8 = 0x84;

/// A HOPKEY request is padded to at least the size of its reply, so a
/// spoofed request cannot turn a hop into a traffic amplifier.
pub const HOPKEY_REQUEST_LEN: usize = 40;

/// Fewest hops that keep the caller's address and the target apart.
pub const MIN_HOPS: usize = 2;
/// More hops add latency to every 20 ms voice frame and little privacy.
pub const MAX_HOPS: usize = 3;

/// What the exit wraps up and sends back when the circuit is built. Every
/// hop adds its layer to it, so opening it proves each one has its keys.
pub const CREATED_MARK: &[u8; 8] = b"ON1BUILT";

/// How long [`Circuit::build`] keeps trying.
pub const BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// One onion datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum Cell {
    /// "What is your hop key?", padded to [`HOPKEY_REQUEST_LEN`].
    HopKeyRequest,
    /// Builds a circuit one hop further.
    Create {
        /// The circuit id on this link, chosen by the sender.
        circ: u32,
        /// The caller's fresh ephemeral key for this hop.
        ephemeral: [u8; 32],
        /// This hop's [`Setup`], sealed under the agreed setup key.
        sealed: Vec<u8>,
    },
    /// Traffic travelling away from the caller.
    Forward {
        /// The circuit id on this link.
        circ: u32,
        /// The onion of remaining layers.
        layer: Vec<u8>,
    },
    /// Tears the circuit down.
    Destroy {
        /// The circuit id on this link.
        circ: u32,
    },
    /// A node's hop public key.
    HopKey([u8; 32]),
    /// The circuit is built, confirmed by every hop.
    Created {
        /// The circuit id on this link.
        circ: u32,
        /// [`CREATED_MARK`] wrapped by each hop's backward layer.
        layer: Vec<u8>,
    },
    /// Traffic travelling back towards the caller.
    Backward {
        /// The circuit id on this link.
        circ: u32,
        /// The reply, with this and earlier hops' backward layers.
        layer: Vec<u8>,
    },
    /// The hop will not carry this circuit.
    Refused {
        /// The circuit id the refusal is for.
        circ: u32,
    },
}

impl Cell {
    /// The cell as it goes on the wire, starting with [`MAGIC`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Cell::HopKeyRequest => {
                out.push(OP_HOPKEY_REQUEST);
                out.resize(HOPKEY_REQUEST_LEN, 0);
            }
            Cell::Create { circ, ephemeral, sealed } => {
                out.push(OP_CREATE);
                out.extend_from_slice(&circ.to_be_bytes());
                out.extend_from_slice(ephemeral);
                out.extend_from_slice(sealed);
            }
            Cell::Forward { circ, layer } => with_layer(&mut out, OP_FORWARD, *circ, layer),
            Cell::Destroy { circ } => {
                out.push(OP_DESTROY);
                out.extend_from_slice(&circ.to_be_bytes());
            }
            Cell::HopKey(key) => {
                out.push(OP_HOPKEY);
                out.extend_from_slice(key);
            }
            Cell::Created { circ, layer } => with_layer(&mut out, OP_CREATED, *circ, layer),
            Cell::Backward { circ, layer } => with_layer(&mut out, OP_BACKWARD, *circ, layer),
            Cell::Refused { circ } => {
                out.push(OP_REFUSED);
                out.extend_from_slice(&circ.to_be_bytes());
            }
        }
        out
    }

    /// `None` for anything that is not a well-formed onion cell.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, body) = rest.split_first()?;
        if op == OP_HOPKEY_REQUEST {
            // Short requests are not answered: see HOPKEY_REQUEST_LEN.
            return (bytes.len() >= HOPKEY_REQUEST_LEN).then_some(Cell::HopKeyRequest);
        }
        if op == OP_HOPKEY {
            return Some(Cell::HopKey(body.try_into().ok()?));
        }
        if body.len() < 4 {
            return None;
        }
        let (circ, body) = body.split_at(4);
        let circ = u32::from_be_bytes(circ.try_into().expect("split at 4"));
        match op {
            OP_CREATE if body.len() > 32 => Some(Cell::Create {
                circ,
                ephemeral: body[..32].try_into().expect("32 bytes"),
                sealed: body[32..].to_vec(),
            }),
            OP_FORWARD if !body.is_empty() => Some(Cell::Forward { circ, layer: body.to_vec() }),
            OP_DESTROY if body.is_empty() => Some(Cell::Destroy { circ }),
            OP_CREATED if !body.is_empty() => Some(Cell::Created { circ, layer: body.to_vec() }),
            OP_BACKWARD if !body.is_empty() => Some(Cell::Backward { circ, layer: body.to_vec() }),
            OP_REFUSED if body.is_empty() => Some(Cell::Refused { circ }),
            _ => None,
        }
    }

    /// Cheap check for whether a datagram is meant for the onion code at all,
    /// so a socket shared with other protocols can route it.
    pub fn is_onion_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
}

/// Writes `op`, the circuit id and a layer: the shape most cells share.
fn with_layer(out: &mut Vec<u8>, op: u8, circ: u32, layer: &[u8]) {
    out.push(op);
    out.extend_from_slice(&circ.to_be_bytes());
    out.extend_from_slice(layer);
}

/// What one hop is told, sealed to it alone, when a circuit is built.
#[derive(Debug, PartialEq, Eq)]
pub enum Setup {
    /// Pass this CREATE body on to `next`.
    Extend {
        /// The next hop's address.
        next: SocketAddr,
        /// The caller's ephemeral key for the next hop.
        ephemeral: [u8; 32],
        /// The next hop's setup, sealed to it; opaque to this hop.
        sealed: Vec<u8>,
    },
    /// Be the exit: send what arrives to `target` as plain UDP.
    Exit {
        /// The relay the call is placed or received through.
        target: SocketAddr,
    },
}

/// Setup tag: pass the rest on to the next hop.
const SETUP_EXTEND: u8 = 0x01;
/// Setup tag: this hop is the exit.
const SETUP_EXIT: u8 = 0x02;

impl Setup {
    /// The setup's plaintext, before it is sealed to the hop.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Setup::Extend { next, ephemeral, sealed } => {
                out.push(SETUP_EXTEND);
                encode_addr(&mut out, next);
                out.extend_from_slice(ephemeral);
                out.extend_from_slice(sealed);
            }
            Setup::Exit { target } => {
                out.push(SETUP_EXIT);
                encode_addr(&mut out, target);
            }
        }
        out
    }

    /// `None` for a setup that is malformed or carries trailing bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        let (addr, rest) = decode_addr(rest)?;
        match kind {
            SETUP_EXTEND if rest.len() > 32 => Some(Setup::Extend {
                next: addr,
                ephemeral: rest[..32].try_into().expect("32 bytes"),
                sealed: rest[32..].to_vec(),
            }),
            SETUP_EXIT if rest.is_empty() => Some(Setup::Exit { target: addr }),
            _ => None,
        }
    }
}

/// Appends an address as `family (4 or 6) | ip | port BE`.
fn encode_addr(out: &mut Vec<u8>, addr: &SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            out.push(4);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
}

/// Reads an address written by [`encode_addr`], returning it and the rest.
fn decode_addr(bytes: &[u8]) -> Option<(SocketAddr, &[u8])> {
    let (&family, rest) = bytes.split_first()?;
    let (ip, rest) = match family {
        4 if rest.len() >= 4 => {
            let o: [u8; 4] = rest[..4].try_into().ok()?;
            (IpAddr::V4(Ipv4Addr::from(o)), &rest[4..])
        }
        6 if rest.len() >= 16 => {
            let o: [u8; 16] = rest[..16].try_into().ok()?;
            (IpAddr::V6(Ipv6Addr::from(o)), &rest[16..])
        }
        _ => return None,
    };
    if rest.len() < 2 {
        return None;
    }
    let port = u16::from_be_bytes([rest[0], rest[1]]);
    Some((SocketAddr::new(ip, port), &rest[2..]))
}

/// A random circuit id or counter start, so ids say nothing across links.
fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    u32::from_be_bytes(b)
}

/// Asks the relay at `node` for its hop key. Fails with
/// [`NetError::OnionRefused`] if the node does not offer hops.
///
/// Trust: the answer is whatever came back from `node`'s address. See the
/// module docs on pinning.
pub async fn fetch_hop_key(socket: &impl crate::Datagram, node: SocketAddr, timeout: Duration) -> Result<[u8; 32], NetError> {
    let request = Cell::HopKeyRequest.encode();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 128];
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&request, node).await?;
        let Ok(Ok((n, from))) = tokio::time::timeout(Duration::from_millis(400), socket.recv_from(&mut buf)).await
        else {
            continue;
        };
        if from != node {
            continue;
        }
        match Cell::decode(&buf[..n]) {
            Some(Cell::HopKey(key)) => return Ok(key),
            Some(Cell::Refused { .. }) => return Err(NetError::OnionRefused),
            _ => continue,
        }
    }
    Err(NetError::OnionTimeout)
}

/// The caller's end of a built circuit: its [`Layers`], and the socket
/// that talks to the entry.
pub struct Circuit {
    /// The socket bound for this circuit; only the entry ever sees it.
    socket: Arc<AppSocket>,
    /// The first hop, the only address this circuit talks to.
    entry: SocketAddr,
    /// The keys and counters that wrap and peel traffic.
    layers: Layers,
}

/// The caller's keys for a built circuit, without a socket: wraps
/// datagrams on the way out and peels replies on the way back.
pub struct Layers {
    /// The circuit id on the caller's link to the entry.
    circ: u32,
    /// One set of keys per hop, entry first.
    hops: Vec<LayerKeys>,
    /// The next forward sequence number under each hop's keys.
    seqs: Vec<u64>,
}

impl Layers {
    /// The circuit id on the caller's link to the entry.
    pub fn circ(&self) -> u32 {
        self.circ
    }

    /// Wraps one datagram for the exit to deliver: the exit's layer
    /// innermost, the entry's outermost.
    pub fn wrap(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut layer = payload.to_vec();
        for (k, seq) in self.hops.iter().zip(self.seqs.iter_mut()).rev() {
            layer = k.seal_forward(*seq, &layer);
            *seq += 1;
        }
        Cell::Forward { circ: self.circ, layer }.encode()
    }

    /// The datagram the target sent back, if `cell` is a genuine reply on
    /// this circuit. Checking it came from the entry is the caller's job.
    pub fn unwrap(&self, cell: &[u8]) -> Option<Vec<u8>> {
        match Cell::decode(cell)? {
            Cell::Backward { circ, layer } if circ == self.circ => peel_backward(&self.hops, layer),
            _ => None,
        }
    }
}

/// A circuit's keys and first cell, before anything is sent.
pub struct Plan {
    /// The circuit id the CREATE uses on the link to the entry.
    pub circ: u32,
    /// The encoded CREATE cell to send to the entry.
    pub create: Vec<u8>,
    /// Every hop's keys, entry first, kept until the circuit is confirmed.
    hops: Vec<LayerKeys>,
}

impl Plan {
    /// Lays out a circuit through `hops` (address and hop key, entry
    /// first) ending at `exit_target`, as one nested CREATE.
    pub fn new(hops: &[(SocketAddr, [u8; 32])], exit_target: SocketAddr) -> Result<Self, NetError> {
        if hops.is_empty() {
            return Err(NetError::OnionPathLength);
        }
        let mut keys = Vec::with_capacity(hops.len());
        let mut inner: Option<([u8; 32], Vec<u8>)> = None;
        for i in (0..hops.len()).rev() {
            let setup = match inner.take() {
                None => Setup::Exit { target: exit_target },
                Some((ephemeral, sealed)) => Setup::Extend { next: hops[i + 1].0, ephemeral, sealed },
            };
            let (eph, k) = client_keys(&hops[i].1)?;
            inner = Some((eph, k.seal_setup(&setup.encode())));
            keys.push(k);
        }
        keys.reverse();
        let (ephemeral, sealed) = inner.expect("at least one hop");
        let circ = random_u32();
        Ok(Self { circ, create: Cell::Create { circ, ephemeral, sealed }.encode(), hops: keys })
    }

    /// Checks a CREATED cell: every hop's layer, then the exit's mark.
    pub fn confirms(&self, cell: &[u8]) -> bool {
        let Some(Cell::Created { circ, layer }) = Cell::decode(cell) else { return false };
        circ == self.circ && peel_backward(&self.hops, layer).is_some_and(|inner| inner == CREATED_MARK)
    }

    /// The keys, once [`Self::confirms`] has said the circuit holds.
    pub fn into_layers(self) -> Layers {
        // Each hop's counter starts at a random point, so the sequence
        // numbers two links carry for one packet are unrelated numbers.
        let seqs = (0..self.hops.len()).map(|_| u64::from(random_u32())).collect();
        Layers { circ: self.circ, hops: self.hops, seqs }
    }
}

/// Removes each hop's backward layer, entry first; `None` if any fails.
fn peel_backward(hops: &[LayerKeys], mut layer: Vec<u8>) -> Option<Vec<u8>> {
    for k in hops {
        layer = k.open_backward(&layer).ok()?;
    }
    Some(layer)
}

impl Circuit {
    /// Builds a circuit through `hops` (entry first; [`MIN_HOPS`] to
    /// [`MAX_HOPS`] of them) whose exit sends to `exit_target` -- the relay
    /// the call is placed or received through. Binds its own socket.
    pub async fn build(hops: &[(SocketAddr, [u8; 32])], exit_target: SocketAddr) -> Result<Self, NetError> {
        let any: SocketAddr = match hops.first().map(|h| h.0) {
            Some(SocketAddr::V6(_)) => (Ipv6Addr::UNSPECIFIED, 0).into(),
            _ => (Ipv4Addr::UNSPECIFIED, 0).into(),
        };
        let socket = Arc::new(AppSocket::bind_direct(any).await?);
        Self::build_on(socket, hops, exit_target, BUILD_TIMEOUT).await
    }

    /// [`Self::build`] on a socket of the caller's choosing.
    pub async fn build_on(
        socket: Arc<AppSocket>,
        hops: &[(SocketAddr, [u8; 32])],
        exit_target: SocketAddr,
        timeout: Duration,
    ) -> Result<Self, NetError> {
        if !(MIN_HOPS..=MAX_HOPS).contains(&hops.len()) {
            return Err(NetError::OnionPathLength);
        }
        let entry = hops[0].0;
        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = vec![0u8; 4096];
        while tokio::time::Instant::now() < deadline {
            // A lost CREATE is simply tried again from scratch: new keys,
            // new id. The half-built attempt expires at the hops.
            let plan = Plan::new(hops, exit_target)?;
            socket.send_to(&plan.create, entry).await?;
            let attempt_end = (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline);
            while let Ok(Ok((n, from))) = tokio::time::timeout_at(attempt_end, socket.recv_from(&mut buf)).await {
                if from != entry {
                    continue;
                }
                if plan.confirms(&buf[..n]) {
                    return Ok(Self { socket, entry, layers: plan.into_layers() });
                }
                if Cell::decode(&buf[..n]) == Some(Cell::Refused { circ: plan.circ }) {
                    return Err(NetError::OnionRefused);
                }
            }
        }
        Err(NetError::OnionTimeout)
    }

    /// The entry hop's address.
    pub fn entry(&self) -> SocketAddr {
        self.entry
    }

    /// How many hops the circuit runs through.
    pub fn hop_count(&self) -> usize {
        self.layers.hops.len()
    }

    /// Where the circuit's socket is bound: the only address of the
    /// caller's that any relay sees, and only the entry sees it.
    pub fn local_addr(&self) -> Result<SocketAddr, NetError> {
        Ok(self.socket.local_addr()?)
    }

    /// [`Layers::wrap`]: one datagram, wrapped for the exit to deliver.
    pub fn wrap(&mut self, payload: &[u8]) -> Vec<u8> {
        self.layers.wrap(payload)
    }

    /// [`Layers::unwrap`], for a cell that arrived from `from`: only the
    /// entry speaks for the circuit.
    pub fn unwrap(&self, from: SocketAddr, cell: &[u8]) -> Option<Vec<u8>> {
        if from != self.entry {
            return None;
        }
        self.layers.unwrap(cell)
    }

    /// Wraps `payload` and sends it to the entry.
    pub async fn send(&mut self, payload: &[u8]) -> Result<(), NetError> {
        let cell = self.wrap(payload);
        self.socket.send_to(&cell, self.entry).await?;
        Ok(())
    }

    /// Waits for the next genuine reply on this circuit, skipping anything
    /// that does not unwrap.
    pub async fn recv(&self) -> Result<Vec<u8>, NetError> {
        let mut buf = vec![0u8; 65_536];
        loop {
            let (n, from) = self.socket.recv_from(&mut buf).await?;
            if let Some(p) = self.unwrap(from, &buf[..n]) {
                return Ok(p);
            }
        }
    }

    /// Tears the circuit down at every hop. Best effort; idle expiry does
    /// the same eventually.
    pub async fn destroy(self) {
        let _ = self.socket.send_to(&Cell::Destroy { circ: self.layers.circ }.encode(), self.entry).await;
    }

    /// Turns the circuit into a local socket pair the call code can use
    /// unchanged. See [`OnionShim`].
    pub async fn into_shim(self) -> Result<OnionShim, NetError> {
        let pump = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let user = Arc::new(AppSocket::from(UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await?));
        let relay_addr = pump.local_addr()?;
        let user_addr = user.local_addr()?;
        let circuit_addr = self.local_addr()?;
        let destroy = (self.socket.clone(), self.entry, Cell::Destroy { circ: self.layers.circ }.encode());
        let task = tokio::spawn(pump_loop(self, pump, user_addr));
        Ok(OnionShim { socket: user, relay_addr, circuit_addr, task, destroy })
    }
}

/// Moves datagrams between the call's socket and the circuit: whatever
/// the call sends to the stand-in address is wrapped and sent to the
/// entry; whatever comes back is unwrapped and handed to the call as if
/// the target relay had sent it from the stand-in address.
async fn pump_loop(mut circuit: Circuit, pump: UdpSocket, user: SocketAddr) {
    let mut from_user = vec![0u8; 65_536];
    let mut from_net = vec![0u8; 65_536];
    let net = circuit.socket.clone();
    loop {
        tokio::select! {
            r = pump.recv_from(&mut from_user) => {
                let Ok((n, from)) = r else { return };
                // Loopback, so the source cannot be spoofed from outside;
                // anything but the call's own socket is ignored.
                if from == user {
                    let _ = circuit.send(&from_user[..n]).await;
                }
            }
            r = net.recv_from(&mut from_net) => {
                let Ok((n, from)) = r else { return };
                if let Some(payload) = circuit.unwrap(from, &from_net[..n]) {
                    let _ = pump.send_to(&payload, user).await;
                }
            }
        }
    }
}

/// A circuit presented as a UDP socket. Use [`Self::socket`] where a call
/// would use its own socket and [`Self::relay_addr`] where it would use
/// the target relay's address -- for [`crate::relay::dial`],
/// [`crate::relay::register`], [`crate::PeerConnection::connect`] and
/// [`crate::PeerConnection::listen`] alike. Replies arrive from
/// `relay_addr`, so the "is this from the relay?" checks in that code hold.
///
/// Dropping it stops the pump and tells the hops to forget the circuit.
pub struct OnionShim {
    /// The loopback socket the call code uses in place of its own.
    socket: Arc<AppSocket>,
    /// The loopback stand-in for the target relay's address.
    relay_addr: SocketAddr,
    /// The circuit's real socket address, as the entry sees it.
    circuit_addr: SocketAddr,
    /// The pump task moving datagrams between `socket` and the circuit.
    task: JoinHandle<()>,
    /// What [`Drop`] needs to send DESTROY: the circuit socket, the entry and
    /// the encoded cell.
    destroy: (Arc<AppSocket>, SocketAddr, Vec<u8>),
}

impl OnionShim {
    /// The socket to hand to the call code.
    pub fn socket(&self) -> Arc<AppSocket> {
        self.socket.clone()
    }

    /// The address to use as the target relay's.
    pub fn relay_addr(&self) -> SocketAddr {
        self.relay_addr
    }

    /// The circuit's own socket address: what the entry sees.
    pub fn circuit_addr(&self) -> SocketAddr {
        self.circuit_addr
    }
}

impl Drop for OnionShim {
    fn drop(&mut self) {
        self.task.abort();
        let (socket, entry, cell) = &self.destroy;
        match socket.as_udp() {
            Some(udp) => {
                let _ = udp.try_send_to(cell, *entry);
            }
            // A tunneled socket can only send from a task.
            None => {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let (socket, entry, cell) = (socket.clone(), *entry, cell.clone());
                    rt.spawn(async move {
                        let _ = socket.send_to(&cell, entry).await;
                    });
                }
            }
        }
    }
}

/// [`Circuit::build`] then [`Circuit::into_shim`].
pub async fn connect_via(hops: &[(SocketAddr, [u8; 32])], target_relay: SocketAddr) -> Result<OnionShim, NetError> {
    Circuit::build(hops, target_relay).await?.into_shim().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::onion::HopSecret;

    /// A documentation-range IPv4 address on `port`, for codec tests.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn every_cell_round_trips() {
        for c in [
            Cell::HopKeyRequest,
            Cell::Create { circ: 7, ephemeral: [3; 32], sealed: vec![1, 2, 3] },
            Cell::Forward { circ: 1, layer: vec![9; 40] },
            Cell::Destroy { circ: u32::MAX },
            Cell::HopKey([5; 32]),
            Cell::Created { circ: 2, layer: vec![1] },
            Cell::Backward { circ: 3, layer: vec![2; 3] },
            Cell::Refused { circ: 0 },
        ] {
            assert_eq!(Cell::decode(&c.encode()), Some(c));
        }
    }

    #[test]
    fn setups_round_trip_for_both_address_families() {
        let v6: SocketAddr = "[2001:db8::1]:3479".parse().unwrap();
        for s in [
            Setup::Exit { target: addr(3479) },
            Setup::Exit { target: v6 },
            Setup::Extend { next: v6, ephemeral: [1; 32], sealed: vec![7; 50] },
            Setup::Extend { next: addr(1), ephemeral: [2; 32], sealed: vec![8; 1] },
        ] {
            assert_eq!(Setup::decode(&s.encode()), Some(s));
        }
        assert!(Setup::decode(&[SETUP_EXIT, 4, 1, 2]).is_none());
        assert!(Setup::decode(&[9, 4, 1, 2, 3, 4, 0, 1]).is_none());
    }

    #[test]
    fn short_hopkey_requests_are_not_answered() {
        let mut short = MAGIC.to_vec();
        short.push(OP_HOPKEY_REQUEST);
        assert!(Cell::decode(&short).is_none());
        assert!(Cell::HopKeyRequest.encode().len() >= Cell::HopKey([0; 32]).encode().len());
    }

    #[test]
    fn onion_cells_are_never_mistaken_for_anything_else() {
        let frame = antiphon_protocol::encode(antiphon_protocol::FrameType::Audio, 1, b"hello");
        assert!(Cell::decode(&frame).is_none());
        assert!(!Cell::is_onion_datagram(&crate::relay::Message::Dialed.encode()));
        assert!(!crate::relay::Message::is_relay_datagram(&Cell::Destroy { circ: 1 }.encode()));
        assert!(antiphon_protocol::decode(&Cell::HopKey([0; 32]).encode()).is_err());
    }

    /// The caller's layering against hand-rolled hops: each peels exactly
    /// one layer, learns only its own next step, and the exit gets the
    /// datagram itself.
    #[tokio::test]
    async fn each_hop_peels_one_layer_and_learns_only_its_next_step() {
        let secrets: Vec<HopSecret> = (0..3).map(|_| HopSecret::generate()).collect();
        let hops: Vec<(SocketAddr, [u8; 32])> =
            secrets.iter().enumerate().map(|(i, s)| (addr(10 + i as u16), s.public())).collect();
        let target = addr(3479);
        let plan = Plan::new(&hops, target).unwrap();

        let Some(Cell::Create { mut ephemeral, mut sealed, .. }) = Cell::decode(&plan.create) else { panic!() };
        let mut hop_keys = Vec::new();
        for (i, s) in secrets.iter().enumerate() {
            let k = s.keys_for(&ephemeral).unwrap();
            let setup = Setup::decode(&k.open_setup(&sealed).unwrap()).unwrap();
            hop_keys.push(k);
            match setup {
                Setup::Extend { next, ephemeral: e, sealed: s } => {
                    assert_eq!(next, hops[i + 1].0);
                    ephemeral = e;
                    sealed = s;
                }
                Setup::Exit { target: t } => {
                    assert_eq!(i, 2);
                    assert_eq!(t, target);
                }
            }
        }

        // Created: the exit marks, every hop wraps on the way back.
        let mut back = hop_keys[2].seal_backward(0, CREATED_MARK);
        back = hop_keys[1].seal_backward(0, &back);
        back = hop_keys[0].seal_backward(0, &back);
        assert!(plan.confirms(&Cell::Created { circ: plan.circ, layer: back.clone() }.encode()));
        assert!(!plan.confirms(&Cell::Created { circ: plan.circ ^ 1, layer: back }.encode()));

        let socket = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let circ = plan.circ;
        let mut circuit = Circuit { socket, entry: hops[0].0, layers: plan.into_layers() };

        let cell = circuit.wrap(b"\xA7RL1 dial");
        let Some(Cell::Forward { mut layer, .. }) = Cell::decode(&cell) else { panic!() };
        let mut seen = vec![layer.clone()];
        for k in &hop_keys {
            layer = k.open_forward(&layer).unwrap();
            seen.push(layer.clone());
        }
        assert_eq!(layer, b"\xA7RL1 dial");
        // No two links carry a common sequence number.
        let seqs: Vec<&[u8]> = seen[..3].iter().map(|l| &l[..8]).collect();
        assert!(seqs[0] != seqs[1] && seqs[1] != seqs[2]);

        let mut back = hop_keys[2].seal_backward(1, b"reply");
        back = hop_keys[1].seal_backward(1, &back);
        back = hop_keys[0].seal_backward(1, &back);
        let cell = Cell::Backward { circ, layer: back }.encode();
        assert_eq!(circuit.unwrap(hops[0].0, &cell).unwrap(), b"reply");
        assert!(circuit.unwrap(addr(99), &cell).is_none(), "only the entry speaks for the circuit");
    }
}
