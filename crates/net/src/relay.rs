//! Talking to a relay (spec §3 "Everyone else -> Relay").
//!
//! A relay pairs two sockets that name the same blinded token (see
//! `antiphon_crypto::relay_token`) and then forwards datagrams between
//! them, untouched. It never sees a key, never decrypts anything, and the
//! frames it forwards are the same sealed frames a direct call sends -- so
//! once paired, an ordinary [`crate::PeerConnection`] aimed at the relay's
//! address *is* the relayed call, handshake and all.
//!
//! The wire format is a handful of control datagrams, each starting with
//! [`MAGIC`]. Its first byte can never be a valid protocol frame version,
//! so a relay notice that reaches a `PeerConnection` is dropped as noise
//! rather than misread, and a relay can tell its own control traffic from
//! traffic it should forward without any state.
//!
//! ```text
//! REGISTER  MAGIC 0x01 [token; 32]*      listener: "pair dialers of these to me"
//! DIAL      MAGIC 0x02 [token; 32]       caller:   "pair me with whoever registered this"
//! LEAVE     MAGIC 0x03                    either:   "forget me"
//! REGISTERED MAGIC 0x81 [count: u16]
//! DIALED    MAGIC 0x82
//! NOT_FOUND MAGIC 0x83
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use crate::socket::Datagram;

use crate::error::NetError;

pub use antiphon_crypto::relay_token::{RelayToken, TOKEN_LEN};

/// `0xA7` is not protocol version 1, so `antiphon_protocol::decode`
/// rejects every relay datagram outright.
pub const MAGIC: [u8; 4] = [0xA7, b'R', b'L', b'1'];

pub const OP_REGISTER: u8 = 0x01;
pub const OP_DIAL: u8 = 0x02;
pub const OP_LEAVE: u8 = 0x03;
pub const OP_REGISTERED: u8 = 0x81;
pub const OP_DIALED: u8 = 0x82;
pub const OP_NOT_FOUND: u8 = 0x83;

/// Tokens per REGISTER datagram: keeps one under the 4 KB every receive
/// buffer in this project is sized for.
pub const MAX_TOKENS_PER_REGISTER: usize = 120;

/// How often a listener re-registers. Well inside the relay's expiry, and
/// short enough to keep a NAT mapping to the relay open.
pub const REGISTER_INTERVAL: Duration = Duration::from_secs(20);

/// A parsed relay datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum Message {
    Register(Vec<RelayToken>),
    Dial(RelayToken),
    Leave,
    Registered(u16),
    Dialed,
    NotFound,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Register(tokens) => {
                out.push(OP_REGISTER);
                for t in tokens {
                    out.extend_from_slice(t);
                }
            }
            Message::Dial(token) => {
                out.push(OP_DIAL);
                out.extend_from_slice(token);
            }
            Message::Leave => out.push(OP_LEAVE),
            Message::Registered(n) => {
                out.push(OP_REGISTERED);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Message::Dialed => out.push(OP_DIALED),
            Message::NotFound => out.push(OP_NOT_FOUND),
        }
        out
    }

    /// `None` for anything that is not a well-formed relay datagram --
    /// including every ordinary protocol frame.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, body) = rest.split_first()?;
        match op {
            OP_REGISTER => {
                if body.is_empty() || body.len() % TOKEN_LEN != 0 {
                    return None;
                }
                let tokens = body
                    .chunks_exact(TOKEN_LEN)
                    .map(|c| c.try_into().expect("chunk is TOKEN_LEN"))
                    .collect();
                Some(Message::Register(tokens))
            }
            OP_DIAL => Some(Message::Dial(body.try_into().ok()?)),
            OP_LEAVE if body.is_empty() => Some(Message::Leave),
            OP_REGISTERED => Some(Message::Registered(u16::from_be_bytes(body.try_into().ok()?))),
            OP_DIALED if body.is_empty() => Some(Message::Dialed),
            OP_NOT_FOUND if body.is_empty() => Some(Message::NotFound),
            _ => None,
        }
    }

    pub fn is_relay_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
}

/// Registers `tokens` at `relay` from `socket`, fire-and-forget. The
/// listener's socket is also the one waiting in
/// [`crate::PeerConnection::listen`], which would swallow any reply, so
/// none is awaited: re-sending every [`REGISTER_INTERVAL`] is what makes it
/// reliable.
pub async fn register(socket: &impl Datagram, relay: SocketAddr, tokens: &[RelayToken]) -> Result<(), NetError> {
    for batch in tokens.chunks(MAX_TOKENS_PER_REGISTER) {
        socket.send_to(&Message::Register(batch.to_vec()).encode(), relay).await?;
    }
    Ok(())
}

/// Asks `relay` to pair `socket` with whoever registered `token`, and waits
/// for the answer. On `Ok`, aim a [`crate::PeerConnection::connect`] at
/// `relay` on the same socket.
pub async fn dial(socket: &impl Datagram, relay: SocketAddr, token: RelayToken, timeout: Duration) -> Result<(), NetError> {
    let request = Message::Dial(token).encode();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 64];
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&request, relay).await?;
        let Ok(Ok((n, from))) =
            tokio::time::timeout(Duration::from_millis(400), socket.recv_from(&mut buf)).await
        else {
            continue;
        };
        if from != relay {
            continue;
        }
        match Message::decode(&buf[..n]) {
            Some(Message::Dialed) => return Ok(()),
            Some(Message::NotFound) => return Err(NetError::RelayNotFound),
            _ => continue,
        }
    }
    Err(NetError::RelayTimeout)
}

/// Tells `relay` to forget this socket. Best effort.
pub async fn leave(socket: &impl Datagram, relay: SocketAddr) {
    let _ = socket.send_to(&Message::Leave.encode(), relay).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        for m in [
            Message::Register(vec![[1; 32], [2; 32]]),
            Message::Dial([3; 32]),
            Message::Leave,
            Message::Registered(7),
            Message::Dialed,
            Message::NotFound,
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn protocol_frames_are_never_mistaken_for_relay_traffic() {
        let frame = antiphon_protocol::encode(antiphon_protocol::FrameType::Audio, 1, b"hello");
        assert!(Message::decode(&frame).is_none());
        assert!(!Message::is_relay_datagram(&frame));
        // ...and the other way round.
        assert!(antiphon_protocol::decode(&Message::Dialed.encode()).is_err());
    }

    #[test]
    fn malformed_datagrams_are_rejected() {
        let mut short = Message::Dial([0; 32]).encode();
        short.pop();
        assert!(Message::decode(&short).is_none());
        let mut ragged = Message::Register(vec![[0; 32]]).encode();
        ragged.push(0);
        assert!(Message::decode(&ragged).is_none());
        assert!(Message::decode(&[MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], 0x7f]).is_none());
    }

    #[test]
    fn a_full_register_batch_fits_one_datagram() {
        let m = Message::Register(vec![[0; 32]; MAX_TOKENS_PER_REGISTER]);
        assert!(m.encode().len() <= 4096);
    }
}
