//! A Kademlia-style DHT for finding contacts again (spec §4 "DHT
//! distributed lookup, so contacts find you after your IP changes").
//!
//! What it stores are [`DhtRecord`]s: "where I am now", filed under a
//! blinded key that rotates daily, sealed so only people who already know
//! the publisher's identity key can read it, and signed so nobody else can
//! write it. A storage node learns neither who a record belongs to nor the
//! address inside it (see `antiphon_crypto::dht_record`).
//!
//! The spec makes Sybil and eclipse resistance a hard requirement before a
//! DHT ships, so the defences are part of the protocol rather than an
//! afterthought (after S/Kademlia, Baumgart & Mies 2007):
//!
//! - **Node ids cost work and are bound to a key.** A node id is the hash
//!   of an Ed25519 public key, valid only with a nonce that makes a second
//!   hash start with [`POW_BITS`] zero bits. Every reply is signed by that
//!   key, so an id can't be claimed by someone who doesn't hold it, and
//!   filling a region of the id space with fake nodes costs a puzzle per
//!   node. Node keys are never the user's identity key.
//! - **Nodes enter a routing table only by answering.** A node that
//!   merely sends us a request is pinged first, and enters the table only
//!   if a signed reply comes back to the address it claimed, so a spoofed
//!   source address can't plant entries.
//! - **Diversity limits.** At most [`Config::per_subnet_per_bucket`] nodes
//!   from one /24 (IPv4) or /48 (IPv6) per bucket, and
//!   [`Config::per_subnet_total`] in the whole table, so one network can't
//!   occupy someone's view of the DHT.
//! - **Old nodes are kept.** A full bucket keeps its longest-lived entry
//!   while it still answers, and only replaces it when it goes silent, so
//!   a flood of newcomers can't push out nodes that have proved themselves.
//! - **Disjoint lookups.** A lookup runs [`PATHS`] parallel paths that
//!   never share a node, so one malicious region of the network has to
//!   capture every path, not one, to hide a record.
//! - **Only signed, newest records.** Storage nodes verify every record's
//!   signature and keep only the highest sequence per key; readers check
//!   it again against the identity they know and take the highest.
//!   Storage is capped per node and per source network, and expires.
//!
//! The app is a client only: it asks and publishes, but never serves, so
//! running the app never makes your machine a public node. Nodes are
//! `antiphon-relay --dht`, which anyone may run.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::dht_record::{DhtKey, DhtRecord};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use tokio::net::UdpSocket;

use crate::error::NetError;

pub const MAGIC: [u8; 4] = [0xA7, b'D', b'H', b'1'];

/// Bucket size, and how many nodes a record is stored at.
pub const K: usize = 8;
/// Requests in flight per lookup path.
pub const ALPHA: usize = 3;
/// Disjoint lookup paths.
pub const PATHS: usize = 3;
/// Zero bits a node id's puzzle must have. 2^16 hashes is a fraction of a
/// second once, and the same again for every fake node an attacker wants.
pub const POW_BITS: u32 = 16;

const ID_LABEL: &[u8] = b"antiphon/dht/v1/node-id";
const POW_LABEL: &[u8] = b"antiphon/dht/v1/node-pow";
const REPLY_CONTEXT: &[u8] = b"antiphon/dht/v1/reply";

pub type NodeId = [u8; 32];
pub type TxId = [u8; 8];

pub fn node_id(public: &[u8; 32]) -> NodeId {
    let mut h = Sha256::new();
    h.update(ID_LABEL);
    h.update(public);
    h.finalize().into()
}

fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut n = 0;
    for b in bytes {
        if *b == 0 {
            n += 8;
        } else {
            return n + b.leading_zeros();
        }
    }
    n
}

/// Whether `nonce` solves the puzzle for `id`.
pub fn pow_ok(id: &NodeId, nonce: u64, bits: u32) -> bool {
    let mut h = Sha256::new();
    h.update(POW_LABEL);
    h.update(id);
    h.update(nonce.to_be_bytes());
    leading_zero_bits(&h.finalize()) >= bits
}

fn distance(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = a[i] ^ b[i];
    }
    out
}

/// A node as other nodes know it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub struct NodeInfo {
    pub public: [u8; 32],
    pub nonce: u64,
    pub addr: SocketAddr,
}

impl NodeInfo {
    pub fn id(&self) -> NodeId {
        node_id(&self.public)
    }

    pub fn valid(&self, bits: u32) -> bool {
        pow_ok(&self.id(), self.nonce, bits)
    }
}

/// A DHT node's own key and puzzle solution. Persist it: the puzzle is
/// paid once per key, and a stable id lets other nodes keep trusting it.
pub struct NodeKey {
    signing: SigningKey,
    nonce: u64,
}

impl NodeKey {
    /// A fresh key, with its puzzle solved.
    pub fn generate(bits: u32) -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).expect("OS randomness is unavailable");
        let signing = SigningKey::from_bytes(&seed);
        let id = node_id(&signing.verifying_key().to_bytes());
        let nonce = (0u64..).find(|n| pow_ok(&id, *n, bits)).expect("a solution exists");
        Self { signing, nonce }
    }

    /// The 40 bytes to persist: seed then nonce.
    pub fn to_bytes(&self) -> [u8; 40] {
        let mut out = [0u8; 40];
        out[..32].copy_from_slice(&self.signing.to_bytes());
        out[32..].copy_from_slice(&self.nonce.to_be_bytes());
        out
    }

    /// Reloads a key, refusing one whose puzzle doesn't meet `bits`.
    pub fn from_bytes(bytes: &[u8], bits: u32) -> Option<Self> {
        let seed: [u8; 32] = bytes.get(..32)?.try_into().ok()?;
        let nonce = u64::from_be_bytes(bytes.get(32..40)?.try_into().ok()?);
        let key = Self { signing: SigningKey::from_bytes(&seed), nonce };
        pow_ok(&key.id(), nonce, bits).then_some(key)
    }

    pub fn public(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    pub fn id(&self) -> NodeId {
        node_id(&self.public())
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }
}

// ---- Wire format ----

const OP_PING: u8 = 0x01;
const OP_FIND_NODE: u8 = 0x02;
const OP_FIND_VALUE: u8 = 0x03;
const OP_STORE: u8 = 0x04;
const OP_GET_LISTING: u8 = 0x05;
const OP_PONG: u8 = 0x81;
const OP_NODES: u8 = 0x82;
const OP_VALUE: u8 = 0x83;
const OP_STORED: u8 = 0x84;
const OP_LISTING: u8 = 0x85;

/// One community in a node's directory (spec §4 "Opt-in directories for
/// public communities only"). A community is listed because its owner
/// asked the node's operator to list it; nobody is listed by default,
/// and people are never listed at all -- only communities, by their
/// invite link.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ListingEntry {
    pub name: String,
    pub description: String,
    /// An `antiphon://` invite link to the community's host.
    pub invite: String,
}

const MAX_NAME: usize = 64;
const MAX_DESCRIPTION: usize = 240;
const MAX_INVITE: usize = 400;
/// Entry bytes per page, so a page and its signature fit in a datagram.
const PAGE_BYTES: usize = 1000;

impl ListingEntry {
    /// Whether it fits the limits a node will serve.
    pub fn fits(&self) -> bool {
        !self.name.trim().is_empty()
            && self.name.len() <= MAX_NAME
            && self.description.len() <= MAX_DESCRIPTION
            && self.invite.len() <= MAX_INVITE
    }

    fn encoded_len(&self) -> usize {
        6 + self.name.len() + self.description.len() + self.invite.len()
    }

    /// Parses an operator's directory file: one community per line,
    /// `name | description | invite link`; `#` starts a comment.
    pub fn parse_file(text: &str) -> Result<Vec<Self>, String> {
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parts: Vec<&str> = line.splitn(3, '|').map(str::trim).collect();
            let [name, description, invite] = parts[..] else {
                return Err(format!("line {}: expected `name | description | invite link`", n + 1));
            };
            let entry = Self { name: name.into(), description: description.into(), invite: invite.into() };
            if !entry.fits() {
                return Err(format!(
                    "line {}: a name (up to {MAX_NAME} bytes), a description up to {MAX_DESCRIPTION} and a link up to {MAX_INVITE}",
                    n + 1
                ));
            }
            out.push(entry);
        }
        Ok(out)
    }
}

/// Splits a listing into pages that each fit one reply.
fn pages(entries: &[ListingEntry]) -> Vec<&[ListingEntry]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (i, e) in entries.iter().enumerate() {
        if i > start && bytes + e.encoded_len() > PAGE_BYTES {
            out.push(&entries[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += e.encoded_len();
    }
    if start < entries.len() {
        out.push(&entries[start..]);
    }
    out.truncate(255);
    out
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Request {
    Ping,
    FindNode(NodeId),
    FindValue(DhtKey),
    Store(DhtRecord),
    /// A page of the node's community directory, if it keeps one.
    GetListing { page: u8 },
}

/// What a node says to a [`Request::Store`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StoreResult {
    Stored,
    /// It already holds a newer record under that key.
    Stale,
    /// The record didn't verify.
    Invalid,
    /// The node, or the sender's network's share of it, is full.
    Full,
}

impl StoreResult {
    fn code(self) -> u8 {
        match self {
            StoreResult::Stored => 0,
            StoreResult::Stale => 1,
            StoreResult::Invalid => 2,
            StoreResult::Full => 3,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            0 => StoreResult::Stored,
            1 => StoreResult::Stale,
            2 => StoreResult::Invalid,
            3 => StoreResult::Full,
            _ => return None,
        })
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Reply {
    /// Carries the address the request came from, which is how a client
    /// behind NAT learns its public address.
    Pong { observed: SocketAddr },
    Nodes(Vec<NodeInfo>),
    Value { record: Option<DhtRecord>, nodes: Vec<NodeInfo> },
    Stored(StoreResult),
    /// `pages` is 0 for a node with no directory.
    Listing { page: u8, pages: u8, entries: Vec<ListingEntry> },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Message {
    /// `sender` is set by nodes that serve, so the receiver may verify and
    /// add them; clients leave it empty and are never added anywhere.
    Request { txid: TxId, sender: Option<([u8; 32], u64)>, body: Request },
    /// Always signed by the responder's node key.
    Reply { txid: TxId, responder: [u8; 32], nonce: u64, body: Reply },
}

fn put_addr(out: &mut Vec<u8>, addr: &SocketAddr) {
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

fn put_nodes(out: &mut Vec<u8>, nodes: &[NodeInfo]) {
    let nodes = &nodes[..nodes.len().min(K)];
    out.push(nodes.len() as u8);
    for n in nodes {
        out.extend_from_slice(&n.public);
        out.extend_from_slice(&n.nonce.to_be_bytes());
        put_addr(out, &n.addr);
    }
}

struct Reader<'a> {
    rest: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, tail) = self.rest.split_at(n);
        self.rest = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn arr32(&mut self) -> Option<[u8; 32]> {
        self.take(32)?.try_into().ok()
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn addr(&mut self) -> Option<SocketAddr> {
        let ip: IpAddr = match self.u8()? {
            4 => <[u8; 4]>::try_from(self.take(4)?).ok()?.into(),
            6 => <[u8; 16]>::try_from(self.take(16)?).ok()?.into(),
            _ => return None,
        };
        let port = u16::from_be_bytes(self.take(2)?.try_into().ok()?);
        Some(SocketAddr::new(ip, port))
    }

    fn nodes(&mut self) -> Option<Vec<NodeInfo>> {
        let count = self.u8()? as usize;
        if count > K {
            return None;
        }
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(NodeInfo { public: self.arr32()?, nonce: self.u64()?, addr: self.addr()? });
        }
        Some(out)
    }

    fn record(&mut self) -> Option<DhtRecord> {
        let len = u16::from_be_bytes(self.take(2)?.try_into().ok()?) as usize;
        DhtRecord::decode(self.take(len)?).ok()
    }
}

fn put_record(out: &mut Vec<u8>, record: &DhtRecord) {
    let bytes = record.encode();
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(&bytes);
}

impl Message {
    pub fn is_dht_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }

    /// Encodes a request. Replies are encoded with [`Self::encode_reply`],
    /// which needs the key to sign them.
    pub fn encode_request(txid: TxId, sender: Option<&NodeKey>, body: &Request) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(match body {
            Request::Ping => OP_PING,
            Request::FindNode(_) => OP_FIND_NODE,
            Request::FindValue(_) => OP_FIND_VALUE,
            Request::Store(_) => OP_STORE,
            Request::GetListing { .. } => OP_GET_LISTING,
        });
        out.extend_from_slice(&txid);
        match sender {
            Some(key) => {
                out.push(1);
                out.extend_from_slice(&key.public());
                out.extend_from_slice(&key.nonce().to_be_bytes());
            }
            None => out.push(0),
        }
        match body {
            Request::Ping => {}
            Request::FindNode(t) | Request::FindValue(t) => out.extend_from_slice(t),
            Request::Store(r) => put_record(&mut out, r),
            Request::GetListing { page } => out.push(*page),
        }
        out
    }

    pub fn encode_reply(txid: TxId, key: &NodeKey, body: &Reply) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.push(match body {
            Reply::Pong { .. } => OP_PONG,
            Reply::Nodes(_) => OP_NODES,
            Reply::Value { .. } => OP_VALUE,
            Reply::Stored(_) => OP_STORED,
            Reply::Listing { .. } => OP_LISTING,
        });
        out.extend_from_slice(&txid);
        out.extend_from_slice(&key.public());
        out.extend_from_slice(&key.nonce().to_be_bytes());
        match body {
            Reply::Pong { observed } => put_addr(&mut out, observed),
            Reply::Nodes(nodes) => put_nodes(&mut out, nodes),
            Reply::Value { record, nodes } => {
                match record {
                    Some(r) => {
                        out.push(1);
                        put_record(&mut out, r);
                    }
                    None => out.push(0),
                }
                put_nodes(&mut out, nodes);
            }
            Reply::Stored(result) => out.push(result.code()),
            Reply::Listing { page, pages, entries } => {
                out.push(*page);
                out.push(*pages);
                out.push(entries.len() as u8);
                for e in entries {
                    for field in [&e.name, &e.description, &e.invite] {
                        out.extend_from_slice(&(field.len() as u16).to_be_bytes());
                        out.extend_from_slice(field.as_bytes());
                    }
                }
            }
        }
        let signature = key.signing.sign(&reply_signing_bytes(&out));
        out.extend_from_slice(&signature.to_bytes());
        out
    }

    /// Decodes a datagram. A reply whose signature doesn't verify under
    /// the key it names is `None`, as is anything malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let mut r = Reader { rest };
        let op = r.u8()?;
        let txid: TxId = r.take(8)?.try_into().ok()?;
        let message = if op < 0x80 {
            let sender = match r.u8()? {
                0 => None,
                1 => Some((r.arr32()?, r.u64()?)),
                _ => return None,
            };
            let body = match op {
                OP_PING => Request::Ping,
                OP_FIND_NODE => Request::FindNode(r.arr32()?),
                OP_FIND_VALUE => Request::FindValue(r.arr32()?),
                OP_STORE => Request::Store(r.record()?),
                OP_GET_LISTING => Request::GetListing { page: r.u8()? },
                _ => return None,
            };
            Message::Request { txid, sender, body }
        } else {
            if bytes.len() < 64 {
                return None;
            }
            let (signed, signature) = bytes.split_at(bytes.len() - 64);
            let mut r = Reader { rest: &signed[MAGIC.len() + 1 + 8..] };
            let responder = r.arr32()?;
            let nonce = r.u64()?;
            let body = match op {
                OP_PONG => Reply::Pong { observed: r.addr()? },
                OP_NODES => Reply::Nodes(r.nodes()?),
                OP_VALUE => {
                    let record = match r.u8()? {
                        0 => None,
                        1 => Some(r.record()?),
                        _ => return None,
                    };
                    Reply::Value { record, nodes: r.nodes()? }
                }
                OP_STORED => Reply::Stored(StoreResult::from_code(r.u8()?)?),
                OP_LISTING => {
                    let page = r.u8()?;
                    let pages = r.u8()?;
                    let count = r.u8()? as usize;
                    let mut entries = Vec::with_capacity(count);
                    for _ in 0..count {
                        let mut field = || -> Option<String> {
                            let len = u16::from_be_bytes(r.take(2)?.try_into().ok()?) as usize;
                            String::from_utf8(r.take(len)?.to_vec()).ok()
                        };
                        let (name, description, invite) = (field()?, field()?, field()?);
                        entries.push(ListingEntry { name, description, invite });
                    }
                    Reply::Listing { page, pages, entries }
                }
                _ => return None,
            };
            if !r.rest.is_empty() {
                return None;
            }
            let key = VerifyingKey::from_bytes(&responder).ok()?;
            let signature = Signature::from_bytes(signature.try_into().ok()?);
            key.verify_strict(&reply_signing_bytes(signed), &signature).ok()?;
            return Some(Message::Reply { txid, responder, nonce, body });
        };
        r.rest.is_empty().then_some(message)
    }
}

fn reply_signing_bytes(body: &[u8]) -> Vec<u8> {
    let mut out = REPLY_CONTEXT.to_vec();
    out.extend_from_slice(body);
    out
}

fn new_txid() -> TxId {
    let mut t = [0u8; 8];
    getrandom::fill(&mut t).expect("OS randomness is unavailable");
    t
}

// ---- Routing table ----

/// The network a node is in, for the diversity limits: its /24 for IPv4,
/// its /48 for IPv6.
fn subnet(addr: &SocketAddr) -> Vec<u8> {
    match addr.ip() {
        IpAddr::V4(ip) => ip.octets()[..3].to_vec(),
        IpAddr::V6(ip) => ip.octets()[..6].to_vec(),
    }
}

struct Entry {
    info: NodeInfo,
    last_seen: Instant,
}

/// What [`RoutingTable::insert`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Insert {
    Added,
    Refreshed,
    /// The bucket is full (or the id is known at another address): ping
    /// this one, and replace it only if it doesn't answer.
    Contest(NodeInfo),
    Refused,
}

pub struct RoutingTable {
    own: NodeId,
    /// Each bucket ordered least recently seen first.
    buckets: Vec<Vec<Entry>>,
    per_subnet_per_bucket: usize,
    per_subnet_total: usize,
}

impl RoutingTable {
    pub fn new(own: NodeId, per_subnet_per_bucket: usize, per_subnet_total: usize) -> Self {
        Self { own, buckets: (0..256).map(|_| Vec::new()).collect(), per_subnet_per_bucket, per_subnet_total }
    }

    fn bucket_of(&self, id: &NodeId) -> Option<usize> {
        let bits = leading_zero_bits(&distance(&self.own, id));
        (bits < 256).then_some(bits as usize)
    }

    pub fn len(&self) -> usize {
        self.buckets.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn contains(&self, id: &NodeId) -> bool {
        self.bucket_of(id).is_some_and(|b| self.buckets[b].iter().any(|e| e.info.id() == *id))
    }

    pub fn insert(&mut self, info: NodeInfo, now: Instant) -> Insert {
        let id = info.id();
        let Some(b) = self.bucket_of(&id) else { return Insert::Refused };
        if let Some(pos) = self.buckets[b].iter().position(|e| e.info.id() == id) {
            if self.buckets[b][pos].info.addr != info.addr {
                // Same id at a new address: only once the old address
                // stops answering, or anyone could redirect a node.
                return Insert::Contest(self.buckets[b][pos].info);
            }
            let mut entry = self.buckets[b].remove(pos);
            entry.last_seen = now;
            self.buckets[b].push(entry);
            return Insert::Refreshed;
        }
        let net = subnet(&info.addr);
        let in_bucket = self.buckets[b].iter().filter(|e| subnet(&e.info.addr) == net).count();
        let in_table = self.buckets.iter().flatten().filter(|e| subnet(&e.info.addr) == net).count();
        if in_bucket >= self.per_subnet_per_bucket || in_table >= self.per_subnet_total {
            return Insert::Refused;
        }
        if self.buckets[b].len() >= K {
            return Insert::Contest(self.buckets[b][0].info);
        }
        self.buckets[b].push(Entry { info, last_seen: now });
        Insert::Added
    }

    pub fn remove(&mut self, id: &NodeId) {
        if let Some(b) = self.bucket_of(id) {
            self.buckets[b].retain(|e| e.info.id() != *id);
        }
    }

    /// The `n` known nodes closest to `target`.
    pub fn closest(&self, target: &[u8; 32], n: usize) -> Vec<NodeInfo> {
        let mut all: Vec<NodeInfo> = self.buckets.iter().flatten().map(|e| e.info).collect();
        all.sort_by_key(|i| distance(&i.id(), target));
        all.truncate(n);
        all
    }

    fn stale(&self, older_than: Instant) -> Vec<NodeInfo> {
        self.buckets.iter().flatten().filter(|e| e.last_seen < older_than).map(|e| e.info).collect()
    }
}

// ---- A serving node ----

/// Limits for a serving node.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub pow_bits: u32,
    pub per_subnet_per_bucket: usize,
    pub per_subnet_total: usize,
    pub max_records: usize,
    /// Records any one /24 (or /48) may have stored here.
    pub max_records_per_subnet: usize,
    /// How long a record is kept without being republished. Keys rotate
    /// daily, so two days covers a record published just before midnight.
    pub record_ttl: Duration,
    /// How often the node re-explores its neighbourhood and checks that
    /// quiet entries are still alive.
    pub refresh_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pow_bits: POW_BITS,
            per_subnet_per_bucket: 2,
            per_subnet_total: 16,
            max_records: 100_000,
            max_records_per_subnet: 1_000,
            record_ttl: Duration::from_secs(48 * 3600),
            refresh_every: Duration::from_secs(15 * 60),
        }
    }
}

/// Counters, for the operator. Never keys or addresses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub nodes: usize,
    pub records: usize,
    pub stores_refused: u64,
}

struct Stored {
    record: DhtRecord,
    stored_at: Instant,
    subnet: Vec<u8>,
}

enum Pending {
    /// Someone who sent us a request, being checked before entering the
    /// table.
    Verify,
    /// Exploring the neighbourhood of our own id.
    Explore,
    /// A table entry contested by a newcomer; the newcomer wins only if
    /// the old one is silent.
    Contest { old: NodeInfo, newcomer: NodeInfo },
    /// A quiet table entry; dropped if it doesn't answer.
    Liveness(NodeInfo),
}

struct InFlight {
    to: SocketAddr,
    expect: Option<[u8; 32]>,
    kind: Pending,
    sent: Instant,
}

const PENDING_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PENDING: usize = 256;

/// A serving DHT node as a plain state machine: datagrams in, datagrams
/// out, so it can share a socket with the relay and be tested without one.
pub struct DhtNode {
    key: NodeKey,
    config: Config,
    table: RoutingTable,
    store: HashMap<DhtKey, Stored>,
    pending: HashMap<TxId, InFlight>,
    explored: HashSet<NodeId>,
    bootstrap: Vec<SocketAddr>,
    last_refresh: Option<Instant>,
    stores_refused: u64,
    listing: Vec<ListingEntry>,
}

pub type Out = Vec<(SocketAddr, Vec<u8>)>;

impl DhtNode {
    pub fn new(key: NodeKey, config: Config) -> Self {
        let table = RoutingTable::new(key.id(), config.per_subnet_per_bucket, config.per_subnet_total);
        Self {
            key,
            config,
            table,
            store: HashMap::new(),
            pending: HashMap::new(),
            explored: HashSet::new(),
            bootstrap: Vec::new(),
            last_refresh: None,
            stores_refused: 0,
            listing: Vec::new(),
        }
    }

    /// Sets the community directory this node serves. Empty -- the
    /// default -- serves none.
    pub fn set_listing(&mut self, entries: Vec<ListingEntry>) {
        self.listing = entries.into_iter().filter(ListingEntry::fits).collect();
    }

    pub fn id(&self) -> NodeId {
        self.key.id()
    }

    pub fn info(&self, addr: SocketAddr) -> NodeInfo {
        NodeInfo { public: self.key.public(), nonce: self.key.nonce(), addr }
    }

    pub fn stats(&self) -> Stats {
        Stats { nodes: self.table.len(), records: self.store.len(), stores_refused: self.stores_refused }
    }

    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    /// Sets the nodes to join through and starts exploring now.
    pub fn bootstrap(&mut self, nodes: &[SocketAddr], now: Instant) -> Out {
        self.bootstrap = nodes.to_vec();
        self.last_refresh = None;
        self.tick(now)
    }

    fn send(&mut self, out: &mut Out, to: SocketAddr, expect: Option<[u8; 32]>, kind: Pending, body: Request, now: Instant) {
        if self.pending.len() >= MAX_PENDING {
            return;
        }
        let txid = new_txid();
        out.push((to, Message::encode_request(txid, Some(&self.key), &body)));
        self.pending.insert(txid, InFlight { to, expect, kind, sent: now });
    }

    fn pending_to(&self, addr: &SocketAddr) -> bool {
        self.pending.values().any(|p| p.to == *addr)
    }

    /// Handles one datagram from `from`.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Out {
        let mut out = Vec::new();
        match Message::decode(bytes) {
            Some(Message::Request { txid, sender, body }) => {
                if let Some((public, nonce)) = sender {
                    let info = NodeInfo { public, nonce, addr: from };
                    self.consider(info, now, &mut out);
                }
                let reply = match body {
                    Request::Ping => Reply::Pong { observed: from },
                    Request::FindNode(target) => Reply::Nodes(self.table.closest(&target, K)),
                    Request::FindValue(key) => Reply::Value {
                        record: self
                            .store
                            .get(&key)
                            .filter(|s| now.duration_since(s.stored_at) < self.config.record_ttl)
                            .map(|s| s.record.clone()),
                        nodes: self.table.closest(&key, K),
                    },
                    Request::Store(record) => Reply::Stored(self.store_record(record, &from, now)),
                    Request::GetListing { page } => {
                        let pages = pages(&self.listing);
                        Reply::Listing {
                            page,
                            pages: pages.len() as u8,
                            entries: pages.get(page as usize).map(|p| p.to_vec()).unwrap_or_default(),
                        }
                    }
                };
                out.push((from, Message::encode_reply(txid, &self.key, &reply)));
            }
            Some(Message::Reply { txid, responder, nonce, body }) => {
                let Some(pending) = self.pending.get(&txid) else { return out };
                // Only the address we asked may answer, and only with the
                // key we expected, or a third party could answer for it.
                if pending.to != from || pending.expect.is_some_and(|e| e != responder) {
                    return out;
                }
                let pending = self.pending.remove(&txid).expect("just looked up");
                let info = NodeInfo { public: responder, nonce, addr: from };
                if !info.valid(self.config.pow_bits) || info.id() == self.id() {
                    return out;
                }
                match pending.kind {
                    Pending::Contest { old, .. } if old.public == responder => {
                        // The incumbent is alive: it stays, the newcomer
                        // doesn't get in.
                        self.table.insert(old, now);
                    }
                    Pending::Liveness(_) | Pending::Verify | Pending::Contest { .. } => self.learn(info, now, &mut out),
                    Pending::Explore => {
                        self.learn(info, now, &mut out);
                        if let Reply::Nodes(nodes) = body {
                            for node in nodes {
                                let id = node.id();
                                if id == self.id()
                                    || !node.valid(self.config.pow_bits)
                                    || self.table.contains(&id)
                                    || !self.explored.insert(id)
                                    || self.pending_to(&node.addr)
                                {
                                    continue;
                                }
                                self.send(&mut out, node.addr, Some(node.public), Pending::Explore, Request::FindNode(self.id()), now);
                            }
                        }
                    }
                }
            }
            None => {}
        }
        out
    }

    /// A node that contacted us: pinged before it can enter the table.
    fn consider(&mut self, info: NodeInfo, now: Instant, out: &mut Out) {
        let id = info.id();
        if id == self.id() || !info.valid(self.config.pow_bits) {
            return;
        }
        if self.table.contains(&id) {
            // Known at this address: seen. Known elsewhere: contested
            // through `learn` once it proves this address.
            if self.table.insert(info, now) == Insert::Refreshed {
                return;
            }
        }
        if !self.pending_to(&info.addr) {
            self.send(out, info.addr, Some(info.public), Pending::Verify, Request::Ping, now);
        }
    }

    /// A node that just proved its key and address by answering us.
    fn learn(&mut self, info: NodeInfo, now: Instant, out: &mut Out) {
        if let Insert::Contest(old) = self.table.insert(info, now) {
            if !self.pending_to(&old.addr) {
                self.send(out, old.addr, Some(old.public), Pending::Contest { old, newcomer: info }, Request::Ping, now);
            }
        }
    }

    fn store_record(&mut self, record: DhtRecord, from: &SocketAddr, now: Instant) -> StoreResult {
        if record.verify().is_err() {
            self.stores_refused += 1;
            return StoreResult::Invalid;
        }
        let net = subnet(from);
        if let Some(existing) = self.store.get_mut(&record.key) {
            if record.sequence < existing.record.sequence
                || (record.sequence == existing.record.sequence && record != existing.record)
            {
                self.stores_refused += 1;
                return StoreResult::Stale;
            }
            existing.record = record;
            existing.stored_at = now;
            return StoreResult::Stored;
        }
        let from_net = self.store.values().filter(|s| s.subnet == net).count();
        if self.store.len() >= self.config.max_records || from_net >= self.config.max_records_per_subnet {
            self.stores_refused += 1;
            return StoreResult::Full;
        }
        self.store.insert(record.key, Stored { record, stored_at: now, subnet: net });
        StoreResult::Stored
    }

    /// Housekeeping: timeouts, expiry and periodic refresh. Call about
    /// once a second.
    pub fn tick(&mut self, now: Instant) -> Out {
        let mut out = Vec::new();
        let expired: Vec<TxId> =
            self.pending.iter().filter(|(_, p)| now.duration_since(p.sent) >= PENDING_TIMEOUT).map(|(t, _)| *t).collect();
        for txid in expired {
            let Some(p) = self.pending.remove(&txid) else { continue };
            match p.kind {
                Pending::Contest { old, newcomer } => {
                    self.table.remove(&old.id());
                    self.table.insert(newcomer, now);
                }
                Pending::Liveness(info) => self.table.remove(&info.id()),
                Pending::Verify | Pending::Explore => {}
            }
        }
        let ttl = self.config.record_ttl;
        self.store.retain(|_, s| now.duration_since(s.stored_at) < ttl);

        if self.last_refresh.is_none_or(|t| now.duration_since(t) >= self.config.refresh_every) {
            self.last_refresh = Some(now);
            self.explored.clear();
            let own = self.id();
            for addr in self.bootstrap.clone() {
                self.send(&mut out, addr, None, Pending::Explore, Request::FindNode(own), now);
            }
            for node in self.table.closest(&own, K) {
                self.explored.insert(node.id());
                self.send(&mut out, node.addr, Some(node.public), Pending::Explore, Request::FindNode(own), now);
            }
            if let Some(cutoff) = now.checked_sub(self.config.refresh_every) {
                for node in self.table.stale(cutoff) {
                    if !self.pending_to(&node.addr) {
                        self.send(&mut out, node.addr, Some(node.public), Pending::Liveness(node), Request::Ping, now);
                    }
                }
            }
        }
        out
    }
}

/// Serves `node` on its own socket until the socket fails: for a
/// standalone node, and tests. The relay serves it from `run_node`
/// instead, alongside everything else on its one port.
pub async fn serve(socket: Arc<UdpSocket>, node: Arc<Mutex<DhtNode>>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65_536];
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        let out = tokio::select! {
            r = socket.recv_from(&mut buf) => {
                let (n, from) = r?;
                node.lock().unwrap().handle(from, &buf[..n], Instant::now())
            }
            _ = tick.tick() => node.lock().unwrap().tick(Instant::now()),
        };
        for (to, bytes) in out {
            let _ = socket.send_to(&bytes, to).await;
        }
    }
}

// ---- The client side: what the app does ----

/// How hard a lookup tries.
#[derive(Clone, Copy, Debug)]
pub struct LookupOptions {
    pub pow_bits: u32,
    pub paths: usize,
    pub request_timeout: Duration,
    pub overall_timeout: Duration,
    /// Most requests one lookup may send.
    pub max_queries: usize,
}

impl Default for LookupOptions {
    fn default() -> Self {
        Self {
            pow_bits: POW_BITS,
            paths: PATHS,
            request_timeout: Duration::from_millis(1500),
            overall_timeout: Duration::from_secs(10),
            max_queries: 96,
        }
    }
}

/// What a lookup found.
#[derive(Clone, Debug, Default)]
pub struct Lookup {
    /// The closest nodes that answered, closest first.
    pub closest: Vec<NodeInfo>,
    /// Every distinct record under the key that verified. Which of them
    /// to believe is the reader's call ([`antiphon_crypto::dht_record::best`]).
    pub records: Vec<DhtRecord>,
    /// How the first node to answer saw us: our public address.
    pub observed: Option<SocketAddr>,
    pub queried: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Fresh,
    Asked,
    Answered,
    Failed,
}

struct Candidate {
    info: NodeInfo,
    path: usize,
    state: State,
}

/// An iterative lookup for `target` along disjoint paths.
///
/// The bootstrap nodes are asked first; the nodes they return are dealt
/// round-robin, closest first, into [`LookupOptions::paths`] paths. Each
/// path then walks towards the target on its own, and a node one path has
/// claimed is never queried by another -- so a node that lies about its
/// neighbourhood can mislead one path but not the others.
pub async fn lookup(
    socket: &UdpSocket,
    bootstrap: &[SocketAddr],
    target: [u8; 32],
    want_value: bool,
    opts: LookupOptions,
) -> Result<Lookup, NetError> {
    let deadline = tokio::time::Instant::now() + opts.overall_timeout;
    let request = if want_value { Request::FindValue(target) } else { Request::FindNode(target) };
    let mut result = Lookup::default();
    let mut seen_records: HashSet<Vec<u8>> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut claimed: HashSet<NodeId> = HashSet::new();
    // txid -> (candidate index, or None for a bootstrap address; sent at)
    let mut in_flight: HashMap<TxId, (Option<usize>, SocketAddr, tokio::time::Instant)> = HashMap::new();
    let mut buf = vec![0u8; 65_536];
    let paths = opts.paths.max(1);

    // Bootstrap round: their answers seed the paths.
    for addr in bootstrap {
        let txid = new_txid();
        socket.send_to(&Message::encode_request(txid, None, &request), addr).await?;
        in_flight.insert(txid, (None, *addr, tokio::time::Instant::now()));
        result.queried += 1;
    }
    let mut seeds: Vec<NodeInfo> = Vec::new();
    let mut any_answered = false;
    let mut seeded = false;

    loop {
        if seeded {
            // Send more where a path still has unasked nodes among its K
            // closest live candidates.
            for p in 0..paths {
                let mut order: Vec<usize> =
                    (0..candidates.len()).filter(|&i| candidates[i].path == p && candidates[i].state != State::Failed).collect();
                order.sort_by_key(|&i| distance(&candidates[i].info.id(), &target));
                order.truncate(K);
                let asked = order.iter().filter(|&&i| candidates[i].state == State::Asked).count();
                let mut room = ALPHA.saturating_sub(asked);
                for i in order {
                    if room == 0 || result.queried >= opts.max_queries {
                        break;
                    }
                    if candidates[i].state == State::Fresh {
                        let txid = new_txid();
                        let addr = candidates[i].info.addr;
                        socket.send_to(&Message::encode_request(txid, None, &request), addr).await?;
                        candidates[i].state = State::Asked;
                        in_flight.insert(txid, (Some(i), addr, tokio::time::Instant::now()));
                        result.queried += 1;
                        room -= 1;
                    }
                }
            }
        }
        if in_flight.is_empty() {
            if seeded {
                break;
            }
            // Bootstrap round over: deal the seeds into paths.
            if !any_answered {
                return Err(NetError::DhtUnreachable);
            }
            seeds.sort_by_key(|n| distance(&n.id(), &target));
            for (i, info) in seeds.drain(..).enumerate() {
                candidates.push(Candidate { info, path: i % paths, state: State::Fresh });
            }
            seeded = true;
            if candidates.is_empty() {
                break;
            }
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let earliest = in_flight.values().map(|(_, _, t)| *t).min().expect("not empty") + opts.request_timeout;
        let wait_until = earliest.min(deadline);
        match tokio::time::timeout_at(wait_until, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let Some(Message::Reply { txid, responder, nonce, body }) = Message::decode(&buf[..n]) else { continue };
                let Some(&(slot, addr, _)) = in_flight.get(&txid) else { continue };
                if addr != from {
                    continue;
                }
                let info = NodeInfo { public: responder, nonce, addr: from };
                if !info.valid(opts.pow_bits) {
                    continue;
                }
                if let Some(i) = slot {
                    // A node answers only for itself.
                    if candidates[i].info.public != responder {
                        continue;
                    }
                    candidates[i].state = State::Answered;
                }
                in_flight.remove(&txid);
                any_answered = true;
                let (record, nodes) = match body {
                    Reply::Nodes(nodes) => (None, nodes),
                    Reply::Value { record, nodes } => (record, nodes),
                    Reply::Pong { observed } => {
                        result.observed.get_or_insert(observed);
                        (None, vec![])
                    }
                    Reply::Stored(_) | Reply::Listing { .. } => (None, vec![]),
                };
                if let Some(record) = record {
                    if record.key == target && record.verify().is_ok() && seen_records.insert(record.encode()) {
                        result.records.push(record);
                    }
                }
                match slot {
                    None => {
                        // A bootstrap node: itself and its answer seed the paths.
                        if claimed.insert(info.id()) {
                            candidates.push(Candidate { info, path: usize::MAX, state: State::Answered });
                        }
                        for n in nodes {
                            if n.valid(opts.pow_bits) && claimed.insert(n.id()) {
                                seeds.push(n);
                            }
                        }
                    }
                    Some(i) => {
                        let path = candidates[i].path;
                        for n in nodes {
                            if n.valid(opts.pow_bits) && claimed.insert(n.id()) {
                                candidates.push(Candidate { info: n, path, state: State::Fresh });
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                // ICMP unreachable from one dead node surfaces here on some
                // platforms; it is that node's failure, not the lookup's.
                if e.kind() != std::io::ErrorKind::ConnectionReset {
                    return Err(e.into());
                }
            }
            Err(_) => {}
        }
        let now = tokio::time::Instant::now();
        in_flight.retain(|_, (slot, _, sent)| {
            if now.duration_since(*sent) < opts.request_timeout {
                return true;
            }
            if let Some(i) = slot {
                candidates[*i].state = State::Failed;
            }
            false
        });
    }
    if !any_answered {
        return Err(NetError::DhtUnreachable);
    }
    let mut answered: Vec<NodeInfo> =
        candidates.iter().filter(|c| c.state == State::Answered).map(|c| c.info).collect();
    answered.sort_by_key(|n| distance(&n.id(), &target));
    answered.truncate(K);
    result.closest = answered;
    Ok(result)
}

/// Every verified record under `key`.
pub async fn find(
    socket: &UdpSocket,
    bootstrap: &[SocketAddr],
    key: DhtKey,
    opts: LookupOptions,
) -> Result<Vec<DhtRecord>, NetError> {
    Ok(lookup(socket, bootstrap, key, true, opts).await?.records)
}

/// Stores `record` at the nodes closest to its key. Returns how many
/// accepted it.
pub async fn publish(
    socket: &UdpSocket,
    bootstrap: &[SocketAddr],
    record: &DhtRecord,
    opts: LookupOptions,
) -> Result<usize, NetError> {
    let found = lookup(socket, bootstrap, record.key, false, opts).await?;
    let mut waiting: HashMap<TxId, NodeInfo> = HashMap::new();
    for node in &found.closest {
        let txid = new_txid();
        socket.send_to(&Message::encode_request(txid, None, &Request::Store(record.clone())), node.addr).await?;
        waiting.insert(txid, *node);
    }
    let deadline = tokio::time::Instant::now() + opts.request_timeout;
    let mut stored = 0;
    let mut buf = vec![0u8; 65_536];
    while !waiting.is_empty() {
        let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await else { break };
        let Some(Message::Reply { txid, responder, body: Reply::Stored(result), .. }) = Message::decode(&buf[..n]) else {
            continue;
        };
        if waiting.get(&txid).is_some_and(|n| n.addr == from && n.public == responder) {
            waiting.remove(&txid);
            if result == StoreResult::Stored {
                stored += 1;
            }
        }
    }
    Ok(stored)
}

/// Reads a node's whole community directory. Returns the node's public
/// key with it: every page is signed by that key, so a caller that pins
/// it on first use (as the app does) notices if a different machine
/// starts answering at that address.
pub async fn fetch_listing(
    socket: &UdpSocket,
    node: SocketAddr,
    timeout: Duration,
) -> Result<([u8; 32], Vec<ListingEntry>), NetError> {
    let mut entries = Vec::new();
    let mut signer: Option<[u8; 32]> = None;
    let mut page = 0u8;
    let mut buf = vec![0u8; 65_536];
    loop {
        let txid = new_txid();
        let request = Message::encode_request(txid, None, &Request::GetListing { page });
        let deadline = tokio::time::Instant::now() + timeout;
        let mut answer = None;
        'retry: while tokio::time::Instant::now() < deadline {
            socket.send_to(&request, node).await?;
            let wait = (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline);
            while let Ok(Ok((n, from))) = tokio::time::timeout_at(wait, socket.recv_from(&mut buf)).await {
                if from != node {
                    continue;
                }
                if let Some(Message::Reply { txid: t, responder, body: Reply::Listing { pages, entries: got, .. }, .. }) =
                    Message::decode(&buf[..n])
                {
                    if t == txid && signer.is_none_or(|s| s == responder) {
                        answer = Some((responder, pages, got));
                        break 'retry;
                    }
                }
            }
        }
        let Some((responder, pages, got)) = answer else { return Err(NetError::DhtUnreachable) };
        signer = Some(responder);
        entries.extend(got);
        page += 1;
        if page >= pages {
            return Ok((responder, entries));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_BITS: u32 = 8;

    fn addr(a: u8, b: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([10, a, b, 1], port))
    }

    fn fake(port: u16, a: u8) -> NodeInfo {
        let k = NodeKey::generate(TEST_BITS);
        NodeInfo { public: k.public(), nonce: k.nonce(), addr: addr(a, port as u8, port) }
    }

    #[test]
    fn messages_round_trip_and_replies_are_signed() {
        let key = NodeKey::generate(TEST_BITS);
        let alice = antiphon_crypto::IdentityKeyPair::generate();
        let record = DhtRecord::seal(&alice, 1, 1, b"x").unwrap();
        for body in [
            Request::Ping,
            Request::FindNode([1; 32]),
            Request::FindValue([2; 32]),
            Request::Store(record.clone()),
            Request::GetListing { page: 3 },
        ] {
            for sender in [None, Some(&key)] {
                let bytes = Message::encode_request([7; 8], sender, &body);
                let expected = Message::Request { txid: [7; 8], sender: sender.map(|k| (k.public(), k.nonce())), body: body.clone() };
                assert_eq!(Message::decode(&bytes), Some(expected));
            }
        }
        let nodes = vec![fake(1, 1), NodeInfo { addr: "[2001:db8::1]:9".parse().unwrap(), ..fake(2, 2) }];
        for body in [
            Reply::Pong { observed: addr(1, 1, 5) },
            Reply::Nodes(nodes.clone()),
            Reply::Value { record: Some(record), nodes: nodes.clone() },
            Reply::Value { record: None, nodes: vec![] },
            Reply::Stored(StoreResult::Stale),
            Reply::Listing {
                page: 1,
                pages: 2,
                entries: vec![ListingEntry { name: "Chess club".into(), description: "Thursdays".into(), invite: "antiphon://ab@x:1".into() }],
            },
        ] {
            let mut bytes = Message::encode_reply([9; 8], &key, &body);
            let expected = Message::Reply { txid: [9; 8], responder: key.public(), nonce: key.nonce(), body };
            assert_eq!(Message::decode(&bytes), Some(expected));
            // Any change -- the txid, the nodes, the record -- breaks it.
            bytes[6] ^= 1;
            assert!(Message::decode(&bytes).is_none());
        }
    }

    #[test]
    fn other_protocols_are_not_dht() {
        assert!(Message::decode(&crate::mailbox::Message::FetchDone { count: 0 }.encode()).is_none());
        assert!(!Message::is_dht_datagram(&crate::relay::Message::Dialed.encode()));
    }

    #[test]
    fn node_ids_need_the_puzzle() {
        let key = NodeKey::generate(TEST_BITS);
        let good = NodeInfo { public: key.public(), nonce: key.nonce(), addr: addr(1, 1, 1) };
        assert!(good.valid(TEST_BITS));
        let bad = (0..64).map(|n| NodeInfo { nonce: n, ..good }).filter(|i| !i.valid(TEST_BITS)).count();
        assert!(bad > 50, "most nonces are not solutions");
        let reloaded = NodeKey::from_bytes(&key.to_bytes(), TEST_BITS).unwrap();
        assert_eq!(reloaded.id(), key.id());
        assert!(NodeKey::from_bytes(&key.to_bytes(), 64).is_none(), "a weaker puzzle than required is refused");
    }

    #[test]
    fn one_network_cannot_fill_the_table() {
        let own = NodeKey::generate(TEST_BITS).id();
        let mut t = RoutingTable::new(own, 2, 3);
        let now = Instant::now();
        let mut added = 0;
        for port in 0..50 {
            let n = NodeInfo { addr: SocketAddr::from(([10, 0, 0, port as u8], 4000 + port)), ..fake(port, 0) };
            if t.insert(n, now) == Insert::Added {
                added += 1;
            }
        }
        assert_eq!(added, 3, "the whole of 10.0.0.0/24 gets three slots");
        // Other networks still get in.
        assert_eq!(t.insert(fake(1, 77), now), Insert::Added);
    }

    #[test]
    fn a_full_bucket_contests_its_oldest_and_an_id_cannot_move_silently() {
        let own = NodeKey::generate(TEST_BITS).id();
        let mut t = RoutingTable::new(own, 100, 1000);
        let now = Instant::now();
        let n = fake(1, 1);
        assert_eq!(t.insert(n, now), Insert::Added);
        assert_eq!(t.insert(n, now), Insert::Refreshed);
        let moved = NodeInfo { addr: addr(9, 9, 9), ..n };
        assert_eq!(t.insert(moved, now), Insert::Contest(n));
        // Fill bucket 0 (ids whose first bit differs from ours).
        let mut first = None;
        let mut a = 2u8;
        while t.buckets[0].len() < K {
            let f = fake(a as u16, a);
            if t.bucket_of(&f.id()) == Some(0) {
                t.insert(f, now);
                first.get_or_insert(f);
            }
            a = a.wrapping_add(1);
        }
        loop {
            let f = fake(a as u16, a);
            a = a.wrapping_add(1);
            if t.bucket_of(&f.id()) == Some(0) {
                match t.insert(f, now) {
                    Insert::Contest(old) => {
                        assert_eq!(old.id(), t.buckets[0][0].info.id(), "the least recently seen is contested");
                        break;
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_node_stores_only_verified_newer_records_within_limits() {
        let mut node = DhtNode::new(
            NodeKey::generate(TEST_BITS),
            Config { pow_bits: TEST_BITS, max_records_per_subnet: 2, ..Config::default() },
        );
        let now = Instant::now();
        let from = addr(1, 1, 1);
        let alice = antiphon_crypto::IdentityKeyPair::generate();
        let v2 = DhtRecord::seal(&alice, 1, 2, b"new").unwrap();
        let v1 = DhtRecord::seal(&alice, 1, 1, b"old").unwrap();
        assert_eq!(node.store_record(v2.clone(), &from, now), StoreResult::Stored);
        assert_eq!(node.store_record(v1, &from, now), StoreResult::Stale);
        let mut forged = v2.clone();
        forged.sequence = 3;
        assert_eq!(node.store_record(forged, &from, now), StoreResult::Invalid);
        let bob = antiphon_crypto::IdentityKeyPair::generate();
        let carol = antiphon_crypto::IdentityKeyPair::generate();
        assert_eq!(node.store_record(DhtRecord::seal(&bob, 1, 1, b"b").unwrap(), &from, now), StoreResult::Stored);
        assert_eq!(node.store_record(DhtRecord::seal(&carol, 1, 1, b"c").unwrap(), &from, now), StoreResult::Full);
        // Expiry.
        node.tick(now + Config::default().record_ttl + Duration::from_secs(1));
        assert_eq!(node.stats().records, 0);
    }

    #[test]
    fn a_requester_is_pinged_before_it_enters_the_table() {
        let mut node = DhtNode::new(NodeKey::generate(TEST_BITS), Config { pow_bits: TEST_BITS, ..Config::default() });
        let other = NodeKey::generate(TEST_BITS);
        let now = Instant::now();
        let from = addr(2, 2, 2);
        let out = node.handle(from, &Message::encode_request([1; 8], Some(&other), &Request::Ping), now);
        assert_eq!(node.stats().nodes, 0, "not on its word");
        // A verification ping went out, plus the pong.
        let ping = out.iter().find_map(|(to, b)| match Message::decode(b) {
            Some(Message::Request { txid, body: Request::Ping, .. }) if *to == from => Some(txid),
            _ => None,
        });
        let txid = ping.expect("verification ping");
        // An answer from a different address is ignored...
        node.handle(addr(3, 3, 3), &Message::encode_reply(txid, &other, &Reply::Pong { observed: from }), now);
        assert_eq!(node.stats().nodes, 0);
        // ...and from the claimed one, admitted.
        node.handle(from, &Message::encode_reply(txid, &other, &Reply::Pong { observed: from }), now);
        assert_eq!(node.stats().nodes, 1);
    }

    #[test]
    fn a_directory_file_parses_and_pages_fit_a_datagram() {
        let text = "# communities on this node\nChess club | Thursdays at eight | antiphon://aa@chess.example:3479\n\n";
        let parsed = ListingEntry::parse_file(text).unwrap();
        assert_eq!(parsed[0].name, "Chess club");
        assert!(ListingEntry::parse_file("just a name").is_err());
        let many: Vec<ListingEntry> = (0..40)
            .map(|i| ListingEntry { name: format!("Community {i}"), description: "d".repeat(200), invite: "i".repeat(300) })
            .collect();
        let split = pages(&many);
        assert!(split.len() > 1);
        assert_eq!(split.iter().map(|p| p.len()).sum::<usize>(), 40);
        let key = NodeKey::generate(TEST_BITS);
        for (i, p) in split.iter().enumerate() {
            let body = Reply::Listing { page: i as u8, pages: split.len() as u8, entries: p.to_vec() };
            let bytes = Message::encode_reply([0; 8], &key, &body);
            assert!(bytes.len() < 1400, "{}", bytes.len());
        }
    }

    #[tokio::test]
    async fn a_listing_is_fetched_page_by_page_from_a_real_node() {
        let mut node = DhtNode::new(NodeKey::generate(TEST_BITS), Config { pow_bits: TEST_BITS, ..Config::default() });
        let entries: Vec<ListingEntry> = (0..12)
            .map(|i| ListingEntry { name: format!("Community {i}"), description: "x".repeat(150), invite: "antiphon://y".into() })
            .collect();
        node.set_listing(entries.clone());
        let public = node.info(addr(0, 0, 0)).public;
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let at = socket.local_addr().unwrap();
        tokio::spawn(serve(socket, Arc::new(Mutex::new(node))));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (signer, got) = fetch_listing(&client, at, Duration::from_secs(2)).await.unwrap();
        assert_eq!(signer, public);
        assert_eq!(got, entries);
    }
}
