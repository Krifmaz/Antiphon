//! The wire format relays use to talk to other relays (federation), and
//! the one question an app may ask a relay about it: who it peers with.
//!
//! Federation lets a call reach someone through any relay in the mesh
//! instead of only the one they registered at. When a caller dials a token
//! a relay does not hold, that relay asks its peers -- with a hash of the
//! token, never the token -- and a peer that holds it answers with proof
//! (`antiphon_crypto::federation::have_proof`). The first relay then dials
//! the token at that peer like any client would and passes the call's
//! sealed datagrams through. Neither relay ever holds a key to the call.
//!
//! Every relay-to-relay message after the handshake travels inside
//! [`Message::Sealed`], under a session only the two relays share; the
//! [`PeerMessage`]s inside are what they say. Outer control datagrams start
//! with [`MAGIC`], whose first byte is never a valid protocol frame
//! version, so a stray one reaching a call is dropped as noise.
//!
//! ```text
//! KEY_REQUEST   MAGIC 0x01 [pad]                 "what is your peer key?"
//! HELLO         MAGIC 0x02 static[32] eph[32] [pad]
//! SEALED        MAGIC 0x03 sealed...             a PeerMessage, sealed
//! PEERS_REQUEST MAGIC 0x04 [pad]                 an app: "who do you peer with?"
//! KEY           MAGIC 0x81 static[32]
//! HELLO_BACK    MAGIC 0x82 static[32] eph[32] confirm...
//! PEER_LIST     MAGIC 0x84 addr*
//! REFUSED       MAGIC 0x85                       federation is off here
//! ```
//!
//! Requests that a stranger can send are padded so that no answer is
//! larger than the request that caused it: a relay cannot be used to
//! amplify traffic at a spoofed address.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use crate::error::NetError;
use crate::socket::Datagram;

/// `0xA7` is not protocol version 1, so `antiphon_protocol::decode`
/// rejects every federation datagram outright.
pub const MAGIC: [u8; 4] = [0xA7, b'F', b'D', b'1'];

/// Opcode: asks a relay for its public peer key.
pub const OP_KEY_REQUEST: u8 = 0x01;
/// Opcode: a handshake opening, from one relay to another.
pub const OP_HELLO: u8 = 0x02;
/// Opcode: a sealed [`PeerMessage`] between two relays with a session.
pub const OP_SEALED: u8 = 0x03;
/// Opcode: an app asks a relay which relays it peers with.
pub const OP_PEERS_REQUEST: u8 = 0x04;
/// Opcode: the answer to [`OP_KEY_REQUEST`].
pub const OP_KEY: u8 = 0x81;
/// Opcode: the answer to [`OP_HELLO`].
pub const OP_HELLO_BACK: u8 = 0x82;
/// Opcode: the answer to [`OP_PEERS_REQUEST`].
pub const OP_PEER_LIST: u8 = 0x84;
/// Opcode: this relay does not federate.
pub const OP_REFUSED: u8 = 0x85;

/// Size a key request is padded to: at least the size of its answer.
pub const KEY_REQUEST_LEN: usize = 64;
/// Size a hello is padded to: at least the size of its answer.
pub const HELLO_LEN: usize = 160;
/// Size a peers request is padded to, and the most a peer list may take.
pub const PEERS_REQUEST_LEN: usize = 512;
/// How many relays a peer list carries at most. 18 bytes per IPv6 entry
/// plus the header stays inside [`PEERS_REQUEST_LEN`].
pub const MAX_LISTED_PEERS: usize = 24;
/// How many relays one gossip message carries at most.
pub const MAX_GOSSIP_PEERS: usize = 32;

/// A parsed outer federation datagram.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Message {
    /// Asks for the relay's public peer key.
    KeyRequest,
    /// Opens a handshake.
    Hello {
        /// The sender's public peer key.
        static_public: [u8; 32],
        /// A fresh ephemeral public key for this handshake.
        ephemeral_public: [u8; 32],
    },
    /// A [`PeerMessage`] sealed under a peer session.
    Sealed(Vec<u8>),
    /// An app asks who this relay peers with.
    PeersRequest,
    /// The relay's public peer key.
    Key([u8; 32]),
    /// Answers a handshake.
    HelloBack {
        /// The responder's public peer key.
        static_public: [u8; 32],
        /// A fresh ephemeral public key for this handshake.
        ephemeral_public: [u8; 32],
        /// Proof the responder holds its static key.
        confirm: Vec<u8>,
    },
    /// Relays this one peers with, for an app to use as fallbacks.
    PeerList(Vec<SocketAddr>),
    /// This relay does not federate.
    Refused,
}

impl Message {
    /// The datagram for this message, padded where the format says so.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::KeyRequest => {
                out.push(OP_KEY_REQUEST);
                out.resize(KEY_REQUEST_LEN, 0);
            }
            Message::Hello { static_public, ephemeral_public } => {
                out.push(OP_HELLO);
                out.extend_from_slice(static_public);
                out.extend_from_slice(ephemeral_public);
                out.resize(HELLO_LEN, 0);
            }
            Message::Sealed(bytes) => {
                out.push(OP_SEALED);
                out.extend_from_slice(bytes);
            }
            Message::PeersRequest => {
                out.push(OP_PEERS_REQUEST);
                out.resize(PEERS_REQUEST_LEN, 0);
            }
            Message::Key(key) => {
                out.push(OP_KEY);
                out.extend_from_slice(key);
            }
            Message::HelloBack { static_public, ephemeral_public, confirm } => {
                out.push(OP_HELLO_BACK);
                out.extend_from_slice(static_public);
                out.extend_from_slice(ephemeral_public);
                out.extend_from_slice(confirm);
            }
            Message::PeerList(addrs) => {
                out.push(OP_PEER_LIST);
                for addr in addrs.iter().take(MAX_LISTED_PEERS) {
                    put_addr(&mut out, addr);
                }
            }
            Message::Refused => out.push(OP_REFUSED),
        }
        out
    }

    /// `None` for anything that is not a well-formed federation datagram,
    /// including a request shorter than its padding.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, body) = rest.split_first()?;
        match op {
            OP_KEY_REQUEST if bytes.len() >= KEY_REQUEST_LEN => Some(Message::KeyRequest),
            OP_HELLO if bytes.len() >= HELLO_LEN => Some(Message::Hello {
                static_public: body[..32].try_into().ok()?,
                ephemeral_public: body[32..64].try_into().ok()?,
            }),
            OP_SEALED if !body.is_empty() => Some(Message::Sealed(body.to_vec())),
            OP_PEERS_REQUEST if bytes.len() >= PEERS_REQUEST_LEN => Some(Message::PeersRequest),
            OP_KEY => Some(Message::Key(body.try_into().ok()?)),
            OP_HELLO_BACK if body.len() > 64 => Some(Message::HelloBack {
                static_public: body[..32].try_into().ok()?,
                ephemeral_public: body[32..64].try_into().ok()?,
                confirm: body[64..].to_vec(),
            }),
            OP_PEER_LIST => {
                let mut addrs = Vec::new();
                let mut rest = body;
                while !rest.is_empty() {
                    let (addr, tail) = take_addr(rest)?;
                    addrs.push(addr);
                    rest = tail;
                }
                (addrs.len() <= MAX_LISTED_PEERS).then_some(Message::PeerList(addrs))
            }
            OP_REFUSED if body.is_empty() => Some(Message::Refused),
            _ => None,
        }
    }

    /// Whether `bytes` starts like a federation datagram.
    pub fn is_federation_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
}

/// What two relays say to each other inside a sealed session.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum PeerMessage {
    /// Keeps the session (and any NAT mapping) alive.
    Ping,
    /// The answer to [`PeerMessage::Ping`].
    Pong,
    /// "Do you hold the token with this hash?"
    Lookup {
        /// Matches the answer to the question.
        id: u64,
        /// `antiphon_crypto::federation::lookup_hash` of the token.
        hash: [u8; 32],
        /// A fresh nonce the proof must cover.
        nonce: [u8; 16],
    },
    /// "Yes, and here is proof."
    Have {
        /// The [`PeerMessage::Lookup`] this answers.
        id: u64,
        /// `antiphon_crypto::federation::have_proof` over the lookup's nonce.
        proof: [u8; 32],
    },
    /// Relays the sender peers with: where they are and their peer keys.
    /// Candidates only -- each still has to complete a handshake.
    Gossip(Vec<(SocketAddr, [u8; 32])>),
}

/// Tag: [`PeerMessage::Ping`].
const PM_PING: u8 = 0x01;
/// Tag: [`PeerMessage::Pong`].
const PM_PONG: u8 = 0x02;
/// Tag: [`PeerMessage::Lookup`].
const PM_LOOKUP: u8 = 0x03;
/// Tag: [`PeerMessage::Have`].
const PM_HAVE: u8 = 0x04;
/// Tag: [`PeerMessage::Gossip`].
const PM_GOSSIP: u8 = 0x05;

impl PeerMessage {
    /// The plaintext to seal.
    pub fn encode(&self) -> Vec<u8> {
        match self {
            PeerMessage::Ping => vec![PM_PING],
            PeerMessage::Pong => vec![PM_PONG],
            PeerMessage::Lookup { id, hash, nonce } => {
                let mut out = vec![PM_LOOKUP];
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(hash);
                out.extend_from_slice(nonce);
                out
            }
            PeerMessage::Have { id, proof } => {
                let mut out = vec![PM_HAVE];
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(proof);
                out
            }
            PeerMessage::Gossip(peers) => {
                let mut out = vec![PM_GOSSIP];
                for (addr, key) in peers.iter().take(MAX_GOSSIP_PEERS) {
                    put_addr(&mut out, addr);
                    out.extend_from_slice(key);
                }
                out
            }
        }
    }

    /// `None` for anything malformed.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let (&tag, body) = bytes.split_first()?;
        match tag {
            PM_PING if body.is_empty() => Some(PeerMessage::Ping),
            PM_PONG if body.is_empty() => Some(PeerMessage::Pong),
            PM_LOOKUP if body.len() == 8 + 32 + 16 => Some(PeerMessage::Lookup {
                id: u64::from_be_bytes(body[..8].try_into().ok()?),
                hash: body[8..40].try_into().ok()?,
                nonce: body[40..].try_into().ok()?,
            }),
            PM_HAVE if body.len() == 8 + 32 => Some(PeerMessage::Have {
                id: u64::from_be_bytes(body[..8].try_into().ok()?),
                proof: body[8..].try_into().ok()?,
            }),
            PM_GOSSIP => {
                let mut peers = Vec::new();
                let mut rest = body;
                while !rest.is_empty() {
                    let (addr, tail) = take_addr(rest)?;
                    let key: [u8; 32] = tail.get(..32)?.try_into().ok()?;
                    peers.push((addr, key));
                    rest = &tail[32..];
                }
                (peers.len() <= MAX_GOSSIP_PEERS).then_some(PeerMessage::Gossip(peers))
            }
            _ => None,
        }
    }
}

/// Appends `addr` as `[4|6][ip][port BE]`.
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

/// Reads one address written by [`put_addr`], and what follows it.
fn take_addr(bytes: &[u8]) -> Option<(SocketAddr, &[u8])> {
    let (&family, rest) = bytes.split_first()?;
    let (ip, rest) = match family {
        4 => {
            let octets: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            (IpAddr::V4(Ipv4Addr::from(octets)), &rest[4..])
        }
        6 => {
            let octets: [u8; 16] = rest.get(..16)?.try_into().ok()?;
            (IpAddr::V6(Ipv6Addr::from(octets)), &rest[16..])
        }
        _ => return None,
    };
    let port = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?);
    Some((SocketAddr::new(ip, port), &rest[2..]))
}

/// Asks `relay` which relays it peers with, for use as extra fallbacks.
/// An empty list from a relay that federates with nobody yet; an error
/// from one that does not federate at all or does not answer.
pub async fn fetch_peers(socket: &impl Datagram, relay: SocketAddr, timeout: Duration) -> Result<Vec<SocketAddr>, NetError> {
    let request = Message::PeersRequest.encode();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; PEERS_REQUEST_LEN + 64];
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&request, relay).await?;
        let Ok(Ok((n, from))) = tokio::time::timeout(Duration::from_millis(400), socket.recv_from(&mut buf)).await else {
            continue;
        };
        if from != relay {
            continue;
        }
        match Message::decode(&buf[..n]) {
            Some(Message::PeerList(addrs)) => return Ok(addrs),
            Some(Message::Refused) => return Err(NetError::FederationRefused),
            _ => continue,
        }
    }
    Err(NetError::FederationTimeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sample address of each family.
    fn addrs() -> Vec<SocketAddr> {
        vec!["203.0.113.5:7100".parse().unwrap(), "[2001:db8::1]:3479".parse().unwrap()]
    }

    #[test]
    fn every_message_round_trips() {
        for m in [
            Message::KeyRequest,
            Message::Hello { static_public: [1; 32], ephemeral_public: [2; 32] },
            Message::Sealed(vec![9; 40]),
            Message::PeersRequest,
            Message::Key([3; 32]),
            Message::HelloBack { static_public: [4; 32], ephemeral_public: [5; 32], confirm: vec![6; 50] },
            Message::PeerList(addrs()),
            Message::Refused,
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
        let key = [8u8; 32];
        for m in [
            PeerMessage::Ping,
            PeerMessage::Pong,
            PeerMessage::Lookup { id: 7, hash: [1; 32], nonce: [2; 16] },
            PeerMessage::Have { id: 7, proof: [3; 32] },
            PeerMessage::Gossip(addrs().into_iter().map(|a| (a, key)).collect()),
        ] {
            assert_eq!(PeerMessage::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn no_answer_is_larger_than_the_request_that_caused_it() {
        let key = Message::Key([0; 32]).encode().len();
        assert!(key <= Message::KeyRequest.encode().len());
        let back = Message::HelloBack { static_public: [0; 32], ephemeral_public: [0; 32], confirm: vec![0; 64] };
        assert!(back.encode().len() <= Message::Hello { static_public: [0; 32], ephemeral_public: [0; 32] }.encode().len());
        let v6: SocketAddr = "[2001:db8::1]:1".parse().unwrap();
        let full = Message::PeerList(vec![v6; MAX_LISTED_PEERS]).encode();
        assert!(full.len() <= Message::PeersRequest.encode().len());
    }

    #[test]
    fn an_unpadded_request_is_ignored() {
        let mut short = Message::PeersRequest.encode();
        short.truncate(10);
        assert!(Message::decode(&short).is_none());
        let mut short = Message::Hello { static_public: [1; 32], ephemeral_public: [2; 32] }.encode();
        short.truncate(70);
        assert!(Message::decode(&short).is_none());
    }

    #[test]
    fn protocol_frames_are_never_mistaken_for_federation_traffic() {
        let frame = antiphon_protocol::encode(antiphon_protocol::FrameType::Audio, 1, b"hello");
        assert!(!Message::is_federation_datagram(&frame));
        assert!(antiphon_protocol::decode(&Message::Refused.encode()).is_err());
        assert!(!crate::relay::Message::is_relay_datagram(&Message::Refused.encode()));
    }

    #[test]
    fn a_ragged_gossip_or_list_is_rejected() {
        let mut g = PeerMessage::Gossip(vec![(addrs()[0], [1; 32])]).encode();
        g.pop();
        assert!(PeerMessage::decode(&g).is_none());
        let mut l = Message::PeerList(addrs()).encode();
        l.push(4);
        assert!(Message::decode(&l).is_none());
    }
}
