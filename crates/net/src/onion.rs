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
//! # Cell sizes
//!
//! Data cells are the same size on every link of a circuit. The caller
//! frames each datagram (`len:u16 | payload | zeros`) and pads it to one of
//! a few sizes ([`CELL_BUCKETS`]); each hop peels its layer from the front
//! of the cell and tops it back up with random filler at the end, so the
//! bytes it forwards are exactly as many as it received. Each hop is told
//! its position in its sealed setup, which is how it knows how much of a
//! cell is filler. An observer of any link sees one of a handful of sizes,
//! and the same size on every link, so sizes no longer say where on the
//! path a link is. The cells that build a circuit are fixed too: every
//! CREATE carries a setup topped up to [`CREATE_SEALED_LEN`] (each hop's
//! setup has a fixed size for its position, and a hop pads what it passes
//! on with filler), and CREATED is framed and padded exactly like the
//! smallest data cell. What still varies is the size bucket, which says
//! roughly how big the datagram inside is.
//!
//! # Known limitations
//!
//! - **No replay window at hops.** A replayed cell is forwarded again; the
//!   call's own session rejects the duplicate end to end.
//! - **A hop key is only as good as where it came from.** Pin it
//!   (`host:port#key` in the app) when you can; otherwise the app trusts
//!   the first key a hop gives ([`fetch_hop_key`]) and refuses a different
//!   one later.
//! - **A hop that restarts breaks its circuits.** Hops answer a cell for a
//!   circuit they don't know with DESTROY, and the caller's shim then
//!   rebuilds the circuit through the same hops on the same socket, so the
//!   call carries on after a short gap.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use antiphon_crypto::onion::{client_keys, LayerKeys, LAYER_OVERHEAD, SETUP_OVERHEAD};
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
/// The circuit is built; the layer wraps [`CREATED_MARK`] once per hop,
/// framed and padded like a data cell so it is the same size on every link.
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

/// What a sealed setup adds to its plaintext: the AEAD tag (the rest of
/// [`SETUP_OVERHEAD`] is the ephemeral key, which travels beside it).
const SETUP_TAG: usize = SETUP_OVERHEAD - 32;

/// An address in a setup, always padded to the IPv6 form's length so the
/// family doesn't change the setup's size.
const SETUP_ADDR_LEN: usize = 1 + 16 + 2;

/// The plaintext length of every setup for the hop at `position`, Extend
/// or Exit alike. The deepest possible hop can only be an exit; every hop
/// before it carries the next hop's ephemeral key and sealed setup. Being
/// fixed per position (not per path) is what lets every CREATE be the same
/// size: see [`CREATE_SEALED_LEN`].
pub const fn setup_plain_len(position: usize) -> usize {
    let mut len = 2 + SETUP_ADDR_LEN;
    let mut p = MAX_HOPS - 1;
    while p > position {
        len = 2 + SETUP_ADDR_LEN + 32 + len + SETUP_TAG;
        p -= 1;
    }
    len
}

/// The sealed length of the setup for the hop at `position`.
pub const fn setup_sealed_len(position: usize) -> usize {
    setup_plain_len(position) + SETUP_TAG
}

/// The sealed part of every CREATE on every link, filler included. A hop
/// passes on a shorter setup than it got (its own is peeled off) and tops
/// it up with random filler, so a CREATE is the same size whether it is
/// the first link of a three-hop path or the last of a two-hop one.
pub const CREATE_SEALED_LEN: usize = setup_sealed_len(0);

/// Tops a sealed setup up to [`CREATE_SEALED_LEN`] with random filler.
pub fn pad_create(mut sealed: Vec<u8>) -> Vec<u8> {
    let short = CREATE_SEALED_LEN.saturating_sub(sealed.len());
    sealed.extend_from_slice(&filler(short));
    sealed
}

/// Opens this hop's setup from a padded CREATE. The hop doesn't know its
/// position until it has opened the setup, so it tries each position's
/// length in turn (at most [`MAX_HOPS`] AEAD checks; the filler after the
/// real setup makes every wrong length fail) and keeps the one whose
/// setup names that same position.
pub fn open_create(keys: &LayerKeys, sealed: &[u8]) -> Option<Setup> {
    (0..MAX_HOPS).find_map(|p| {
        let plain = keys.open_setup(sealed.get(..setup_sealed_len(p))?).ok()?;
        let setup = Setup::decode(&plain)?;
        (usize::from(setup.position()) == p).then_some(setup)
    })
}

/// What the exit wraps up and sends back when the circuit is built. Every
/// hop adds its layer to it, so opening it proves each one has its keys.
pub const CREATED_MARK: &[u8; 8] = b"ON1BUILT";

/// The sizes a data cell's layers are padded to, smallest first. A cell
/// takes the smallest that fits its datagram, two length bytes and every
/// hop's layer overhead. The largest stays under the biggest UDP datagram.
pub const CELL_BUCKETS: &[usize] = &[256, 512, 1024, 1536, 4096, 16_384, 65_000];

/// The padded size for a datagram of `payload` bytes through `hops` hops,
/// or `None` if it cannot fit even the largest bucket.
pub fn bucket_for(payload: usize, hops: usize) -> Option<usize> {
    let need = payload + 2 + LAYER_OVERHEAD * hops;
    CELL_BUCKETS.iter().copied().find(|&b| b >= need)
}

/// `len:u16 | payload | zeros`, `size` bytes long. `size` must fit.
pub fn frame(payload: &[u8], size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    out.extend_from_slice(payload);
    out.resize(size, 0);
    out
}

/// The datagram inside a [`frame`], or `None` if the length is impossible.
pub fn unframe(framed: &[u8]) -> Option<&[u8]> {
    let len = u16::from_be_bytes(framed.get(..2)?.try_into().ok()?) as usize;
    framed.get(2..2 + len)
}

/// `n` random bytes, for topping a cell back up to its size.
pub fn filler(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; n];
    getrandom::fill(&mut out).expect("OS randomness is unavailable");
    out
}

/// A hop's handling of a forward data layer, keeping the cell's size:
/// peels this hop's layer from the front (all but the `position` layers'
/// worth of filler at the end), then tops the result back up with fresh
/// filler. For the exit, returns the datagram inside instead.
pub fn hop_forward(keys: &LayerKeys, position: u8, exit: bool, layer: &[u8]) -> Option<Vec<u8>> {
    let keep = layer.len().checked_sub(LAYER_OVERHEAD * usize::from(position))?;
    let inner = keys.open_forward(&layer[..keep]).ok()?;
    if exit {
        return unframe(&inner).map(<[u8]>::to_vec);
    }
    let mut out = inner;
    out.extend_from_slice(&layer[keep..]);
    out.extend_from_slice(&filler(LAYER_OVERHEAD));
    Some(out)
}

/// A middle or entry hop's handling of a backward layer, keeping the
/// cell's size: drops one layer's worth of filler from the end, seals what
/// is left, and keeps the rest of the filler after it.
pub fn hop_backward(keys: &LayerKeys, seq: &mut u64, position: u8, layer: &[u8]) -> Option<Vec<u8>> {
    let keep = layer.len().checked_sub(LAYER_OVERHEAD * (usize::from(position) + 1))?;
    let mut out = keys.seal_backward(*seq, &layer[..keep]);
    *seq += 1;
    out.extend_from_slice(&filler(LAYER_OVERHEAD * usize::from(position)));
    Some(out)
}

/// The exit's wrapping of a reply from the target: framed and padded to
/// its size bucket for a path of `position + 1` hops, sealed, and followed
/// by filler for each hop still to add its layer.
pub fn exit_backward(keys: &LayerKeys, seq: &mut u64, position: u8, payload: &[u8]) -> Option<Vec<u8>> {
    let hops = usize::from(position) + 1;
    let size = bucket_for(payload.len(), hops)?;
    let mut out = keys.seal_backward(*seq, &frame(payload, size - LAYER_OVERHEAD * hops));
    *seq += 1;
    out.extend_from_slice(&filler(LAYER_OVERHEAD * usize::from(position)));
    Some(out)
}

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
        /// This hop's place on the path, 0 for the entry: how much of a
        /// data cell is filler (see the module docs on cell sizes).
        position: u8,
        /// The next hop's address.
        next: SocketAddr,
        /// The caller's ephemeral key for the next hop.
        ephemeral: [u8; 32],
        /// The next hop's setup, sealed to it; opaque to this hop.
        sealed: Vec<u8>,
    },
    /// Be the exit: send what arrives to `target` as plain UDP.
    Exit {
        /// This hop's place on the path, 0 for the entry.
        position: u8,
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
            Setup::Extend { position, next, ephemeral, sealed } => {
                out.push(SETUP_EXTEND);
                out.push(*position);
                encode_addr(&mut out, next);
                out.extend_from_slice(ephemeral);
                out.extend_from_slice(sealed);
            }
            Setup::Exit { position, target } => {
                out.push(SETUP_EXIT);
                out.push(*position);
                encode_addr(&mut out, target);
            }
        }
        out.resize(setup_plain_len(usize::from(self.position())), 0);
        out
    }

    /// This hop's place on the path, 0 for the entry.
    pub fn position(&self) -> u8 {
        match self {
            Setup::Extend { position, .. } | Setup::Exit { position, .. } => *position,
        }
    }

    /// `None` for a setup that is malformed or carries trailing bytes.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&kind, rest) = bytes.split_first()?;
        let (&position, rest) = rest.split_first()?;
        if usize::from(position) >= MAX_HOPS {
            return None;
        }
        if bytes.len() != setup_plain_len(usize::from(position)) {
            return None;
        }
        let (addr, rest) = decode_addr(rest)?;
        // Whatever follows the setup itself is padding, and must be zeros.
        let (setup, padding) = match kind {
            SETUP_EXTEND if usize::from(position) + 1 < MAX_HOPS => {
                let end = 32 + setup_sealed_len(usize::from(position) + 1);
                let body = rest.get(..end)?;
                let setup = Setup::Extend {
                    position,
                    next: addr,
                    ephemeral: body[..32].try_into().expect("32 bytes"),
                    sealed: body[32..].to_vec(),
                };
                (setup, &rest[end..])
            }
            SETUP_EXIT => (Setup::Exit { position, target: addr }, rest),
            _ => return None,
        };
        padding.iter().all(|&b| b == 0).then_some(setup)
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
    /// The hops, entry first, kept so a broken circuit can be rebuilt
    /// through the same path.
    path: Vec<(SocketAddr, [u8; 32])>,
    /// Where the exit sends, kept for the same reason.
    target: SocketAddr,
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

    /// Wraps one datagram for the exit to deliver: framed and padded to
    /// its size bucket, the exit's layer innermost, the entry's outermost.
    /// A datagram too big for the largest bucket is cut to fit -- no call
    /// datagram comes near it.
    pub fn wrap(&mut self, payload: &[u8]) -> Vec<u8> {
        let n = self.hops.len();
        let max = CELL_BUCKETS[CELL_BUCKETS.len() - 1] - 2 - LAYER_OVERHEAD * n;
        let payload = &payload[..payload.len().min(max)];
        let size = bucket_for(payload.len(), n).expect("cut to fit above");
        let mut layer = frame(payload, size - LAYER_OVERHEAD * n);
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
            Cell::Backward { circ, layer } if circ == self.circ => {
                peel_backward(&self.hops, layer).and_then(|inner| unframe(&inner).map(<[u8]>::to_vec))
            }
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
            let position = i as u8;
            let setup = match inner.take() {
                None => Setup::Exit { position, target: exit_target },
                Some((ephemeral, sealed)) => Setup::Extend { position, next: hops[i + 1].0, ephemeral, sealed },
            };
            let (eph, k) = client_keys(&hops[i].1)?;
            inner = Some((eph, k.seal_setup(&setup.encode())));
            keys.push(k);
        }
        keys.reverse();
        let (ephemeral, sealed) = inner.expect("at least one hop");
        let circ = random_u32();
        Ok(Self { circ, create: Cell::Create { circ, ephemeral, sealed: pad_create(sealed) }.encode(), hops: keys })
    }

    /// Checks a CREATED cell: every hop's layer, then the exit's mark.
    pub fn confirms(&self, cell: &[u8]) -> bool {
        let Some(Cell::Created { circ, layer }) = Cell::decode(cell) else { return false };
        circ == self.circ
            && peel_backward(&self.hops, layer).is_some_and(|inner| unframe(&inner) == Some(&CREATED_MARK[..]))
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

/// Sends CREATEs to the entry until one is confirmed, refused, or time is
/// up. A lost CREATE is simply tried again from scratch: new keys, new id;
/// the half-built attempt expires at the hops.
async fn negotiate(
    socket: &AppSocket,
    hops: &[(SocketAddr, [u8; 32])],
    exit_target: SocketAddr,
    timeout: Duration,
) -> Result<Layers, NetError> {
    let entry = hops[0].0;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 4096];
    while tokio::time::Instant::now() < deadline {
        let plan = Plan::new(hops, exit_target)?;
        socket.send_to(&plan.create, entry).await?;
        let attempt_end = (tokio::time::Instant::now() + Duration::from_secs(1)).min(deadline);
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(attempt_end, socket.recv_from(&mut buf)).await {
            if from != entry {
                continue;
            }
            if plan.confirms(&buf[..n]) {
                return Ok(plan.into_layers());
            }
            if Cell::decode(&buf[..n]) == Some(Cell::Refused { circ: plan.circ }) {
                return Err(NetError::OnionRefused);
            }
        }
    }
    Err(NetError::OnionTimeout)
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
        let layers = negotiate(&socket, hops, exit_target, timeout).await?;
        Ok(Self { socket, entry: hops[0].0, layers, path: hops.to_vec(), target: exit_target })
    }

    /// Builds the circuit again through the same hops, on the same socket,
    /// after a hop said it no longer knows it. The call code on the other
    /// side of a shim never notices beyond a short gap.
    pub async fn rebuild(&mut self, timeout: Duration) -> Result<(), NetError> {
        self.layers = negotiate(&self.socket, &self.path, self.target, timeout).await?;
        Ok(())
    }

    /// Whether `cell`, from `from`, is the entry saying this circuit is gone.
    pub fn is_destroyed_by(&self, from: SocketAddr, cell: &[u8]) -> bool {
        from == self.entry && Cell::decode(cell) == Some(Cell::Destroy { circ: self.layers.circ })
    }

    /// The entry hop's address.
    pub fn entry(&self) -> SocketAddr {
        self.entry
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
///
/// It also keeps the circuit alive: when the entry says the circuit is gone
/// (a hop restarted or forgot it), or when the call keeps sending but
/// nothing has come back for [`SILENCE_BEFORE_REBUILD`], the circuit is
/// rebuilt through the same hops on the same socket.
async fn pump_loop(mut circuit: Circuit, pump: UdpSocket, user: SocketAddr) {
    let mut from_user = vec![0u8; 65_536];
    let mut from_net = vec![0u8; 65_536];
    let net = circuit.socket.clone();
    let mut heard = tokio::time::Instant::now();
    let mut sent = tokio::time::Instant::now();
    let mut check = tokio::time::interval(Duration::from_secs(5));
    loop {
        let broken = tokio::select! {
            r = pump.recv_from(&mut from_user) => {
                let Ok((n, from)) = r else { return };
                // Loopback, so the source cannot be spoofed from outside;
                // anything but the call's own socket is ignored.
                if from == user {
                    let _ = circuit.send(&from_user[..n]).await;
                    sent = tokio::time::Instant::now();
                }
                false
            }
            r = net.recv_from(&mut from_net) => {
                let Ok((n, from)) = r else { return };
                if let Some(payload) = circuit.unwrap(from, &from_net[..n]) {
                    heard = tokio::time::Instant::now();
                    let _ = pump.send_to(&payload, user).await;
                    false
                } else {
                    circuit.is_destroyed_by(from, &from_net[..n])
                }
            }
            // Quiet both ways is just an idle circuit; sending into silence
            // is a broken one.
            _ = check.tick() => heard.elapsed() >= SILENCE_BEFORE_REBUILD && sent.elapsed() < Duration::from_secs(10),
        };
        if broken {
            // Keep trying: a restarting hop takes a moment to come back.
            // Traffic from the call meanwhile is dropped, as on any outage.
            while circuit.rebuild(BUILD_TIMEOUT).await.is_err() {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            heard = tokio::time::Instant::now();
        }
    }
}

/// How long a circuit that is still being sent on may go without any
/// reply before the shim rebuilds it. A listener's relay answers its registration every 20 s and a call
/// carries audio both ways, so a live circuit is never this quiet.
pub const SILENCE_BEFORE_REBUILD: Duration = Duration::from_secs(60);

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
            Setup::Exit { position: 1, target: addr(3479) },
            Setup::Exit { position: 2, target: v6 },
            Setup::Extend { position: 0, next: v6, ephemeral: [1; 32], sealed: vec![7; setup_sealed_len(1)] },
            Setup::Extend { position: 1, next: addr(1), ephemeral: [2; 32], sealed: vec![8; setup_sealed_len(2)] },
        ] {
            let plain = s.encode();
            assert_eq!(plain.len(), setup_plain_len(usize::from(s.position())), "fixed size per position");
            assert_eq!(Setup::decode(&plain), Some(s));
        }
        let mut dirty = Setup::Exit { position: 1, target: addr(1) }.encode();
        *dirty.last_mut().unwrap() = 1;
        assert!(Setup::decode(&dirty).is_none(), "padding must be zeros");
        assert!(Setup::decode(&[SETUP_EXIT, 0, 4, 1, 2]).is_none());
        assert!(Setup::decode(&[9, 0, 4, 1, 2, 3, 4, 0, 1]).is_none());
        assert!(Setup::decode(&[SETUP_EXIT, 3, 4, 1, 2, 3, 4, 0, 1]).is_none(), "no fourth hop");
    }

    #[test]
    fn framing_round_trips_and_buckets_fit() {
        assert_eq!(unframe(&frame(b"abc", 64)).unwrap(), b"abc");
        assert!(unframe(&[0, 9, 1]).is_none());
        assert_eq!(bucket_for(10, 3), Some(256));
        assert_eq!(bucket_for(1200, 3), Some(1536));
        assert_eq!(bucket_for(70_000, 2), None);
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

        let create_len = plan.create.len();
        let Some(Cell::Create { mut ephemeral, mut sealed, .. }) = Cell::decode(&plan.create) else { panic!() };
        let mut hop_keys = Vec::new();
        for (i, s) in secrets.iter().enumerate() {
            let k = s.keys_for(&ephemeral).unwrap();
            let setup = open_create(&k, &sealed).unwrap();
            hop_keys.push(k);
            match setup {
                Setup::Extend { position, next, ephemeral: e, sealed: s } => {
                    assert_eq!(usize::from(position), i);
                    assert_eq!(next, hops[i + 1].0);
                    // What the hop passes on is as big as what it got.
                    let passed = Cell::Create { circ: 1, ephemeral: e, sealed: pad_create(s) }.encode();
                    assert_eq!(passed.len(), create_len, "CREATE is the same size on every link");
                    let Some(Cell::Create { ephemeral: e, sealed: s, .. }) = Cell::decode(&passed) else { panic!() };
                    ephemeral = e;
                    sealed = s;
                }
                Setup::Exit { position, target: t } => {
                    assert_eq!((i, position), (2, 2));
                    assert_eq!(t, target);
                }
            }
        }

        // Created: the exit marks, every hop wraps on the way back, and it
        // stays one size, the smallest data cell's.
        let mut seq = 0;
        let mut back = exit_backward(&hop_keys[2], &mut seq, 2, CREATED_MARK).unwrap();
        assert_eq!(back.len(), CELL_BUCKETS[0]);
        back = hop_backward(&hop_keys[1], &mut seq, 1, &back).unwrap();
        back = hop_backward(&hop_keys[0], &mut seq, 0, &back).unwrap();
        assert_eq!(back.len(), CELL_BUCKETS[0]);
        assert!(plan.confirms(&Cell::Created { circ: plan.circ, layer: back.clone() }.encode()));
        assert!(!plan.confirms(&Cell::Created { circ: plan.circ ^ 1, layer: back }.encode()));

        let socket = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let circ = plan.circ;
        let mut circuit = Circuit { socket, entry: hops[0].0, layers: plan.into_layers(), path: hops.clone(), target };

        let cell = circuit.wrap(b"\xA7RL1 dial");
        let Some(Cell::Forward { mut layer, .. }) = Cell::decode(&cell) else { panic!() };
        let mut seen = vec![layer.clone()];
        for (i, k) in hop_keys.iter().enumerate().take(2) {
            layer = hop_forward(k, i as u8, false, &layer).unwrap();
            seen.push(layer.clone());
        }
        // Every link carries a cell of the same size.
        assert!(seen.iter().all(|l| l.len() == seen[0].len()), "{:?}", seen.iter().map(Vec::len).collect::<Vec<_>>());
        assert_eq!(hop_forward(&hop_keys[2], 2, true, &layer).unwrap(), b"\xA7RL1 dial");
        // No two links carry a common sequence number.
        let seqs: Vec<&[u8]> = seen[..3].iter().map(|l| &l[..8]).collect();
        assert!(seqs[0] != seqs[1] && seqs[1] != seqs[2]);

        let mut seq = 1;
        let mut back = exit_backward(&hop_keys[2], &mut seq, 2, b"reply").unwrap();
        let size = back.len();
        back = hop_backward(&hop_keys[1], &mut seq, 1, &back).unwrap();
        assert_eq!(back.len(), size);
        back = hop_backward(&hop_keys[0], &mut seq, 0, &back).unwrap();
        assert_eq!(back.len(), size);
        let cell = Cell::Backward { circ, layer: back }.encode();
        assert_eq!(circuit.unwrap(hops[0].0, &cell).unwrap(), b"reply");
        assert!(circuit.unwrap(addr(99), &cell).is_none(), "only the entry speaks for the circuit");
    }
}
