//! The mailbox protocol (spec §8 "Delivery Routes": a node "holds sealed
//! messages it can't read").
//!
//! A mailbox is a node that holds sealed mail for people who registered a
//! label with it. The recipient registers; senders deposit to a label
//! they read from the recipient's signed route list; the recipient
//! fetches and acknowledges. Every blob is sealed end to end
//! ([`antiphon_crypto::mail`]), so the node holds bytes it can't read,
//! addressed to a label it can't link to anyone.
//!
//! Two rules from the spec are enforced by the node, not trusted to
//! clients:
//!
//! - Writes are accepted only to labels currently registered, so a public
//!   node can't be filled by strangers -- an unknown label is refused
//!   without anything being stored.
//! - Only the holder of the fetch secret reads or deletes; the node keeps
//!   just its hash.
//!
//! Each request is one datagram and each reply is one datagram. Fetching
//! returns the waiting mail one blob per datagram followed by a count;
//! anything lost on the way is simply still there next time, since only
//! an acknowledgement deletes.

use std::net::SocketAddr;
use std::time::Duration;

use crate::socket::Datagram;

use crate::error::NetError;

pub const MAGIC: [u8; 4] = [0xA7, b'M', b'B', b'1'];

/// Largest sealed blob a node accepts. Keeps every reply inside one
/// datagram on any path; longer messages wait for a direct connection.
pub const MAX_BLOB: usize = 1100;

pub type Label = [u8; 32];
pub type MailId = [u8; 16];

const OP_REGISTER: u8 = 0x01;
const OP_DEPOSIT: u8 = 0x02;
const OP_FETCH: u8 = 0x03;
const OP_ACK: u8 = 0x04;
const OP_STATUS: u8 = 0x05;
const OP_REGISTER_PUSH: u8 = 0x06;
const OP_WAKE: u8 = 0x07;
const OP_REGISTERED: u8 = 0x81;
const OP_DEPOSITED: u8 = 0x82;
const OP_MAIL: u8 = 0x83;
const OP_FETCH_DONE: u8 = 0x84;
const OP_ACKED: u8 = 0x85;
const OP_REFUSED: u8 = 0x86;
const OP_STATUS_REPLY: u8 = 0x87;
const OP_PUSH_REGISTERED: u8 = 0x88;
const OP_WOKEN: u8 = 0x89;

/// The size of a sealed push wake-up ([`antiphon_crypto::push::SEALED_LEN`]),
/// repeated here because this crate does not otherwise need the crypto
/// crate's push module. The relay's tests check the two agree.
pub const WAKE_LEN: usize = 368;
/// The longest push endpoint a node takes, the same limit a route record
/// puts on one address.
pub const MAX_ENDPOINT: usize = 256;
/// The longest members code a [`Message::Wake`] may carry, as for
/// [`Message::Register`].
const MAX_CODE: usize = 64;

/// Why a node said no. Specific, because each one asks the user for a
/// different next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Nobody registered that label here (or it expired).
    UnknownLabel,
    /// The node, or this label's share of it, is full.
    Full,
    /// Too many deposits to this label just now.
    RateLimited,
    /// This node serves its own community only, and the code was wrong.
    NotAMember,
    TooBig,
    /// Wrong fetch secret, or a label registered by someone else.
    WrongSecret,
    /// The operator hasn't switched mailbox service on.
    Disabled,
    /// This node does not send push wake-ups (no push sender configured,
    /// or none for that kind of endpoint), or refused this one.
    NoPush,
}

impl Refusal {
    pub(crate) fn code(self) -> u8 {
        match self {
            Refusal::UnknownLabel => 1,
            Refusal::Full => 2,
            Refusal::RateLimited => 3,
            Refusal::NotAMember => 4,
            Refusal::TooBig => 5,
            Refusal::WrongSecret => 6,
            Refusal::Disabled => 7,
            Refusal::NoPush => 8,
        }
    }

    pub(crate) fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Refusal::UnknownLabel,
            2 => Refusal::Full,
            3 => Refusal::RateLimited,
            4 => Refusal::NotAMember,
            5 => Refusal::TooBig,
            6 => Refusal::WrongSecret,
            7 => Refusal::Disabled,
            8 => Refusal::NoPush,
            _ => return None,
        })
    }

    pub fn describe(self) -> &'static str {
        match self {
            Refusal::UnknownLabel => "that inbox isn't registered at this node",
            Refusal::Full => "the node is full",
            Refusal::RateLimited => "too many messages to that inbox just now",
            Refusal::NotAMember => "this node only serves its own community",
            Refusal::TooBig => "the message is too long for a mailbox",
            Refusal::WrongSecret => "this inbox belongs to someone else",
            Refusal::Disabled => "this node doesn't hold mail",
            Refusal::NoPush => "this node doesn't send that push wake-up",
        }
    }
}

/// What a node tells its owner about their inbox, for the live per-node
/// status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct InboxStatus {
    pub held: u32,
    pub held_bytes: u64,
    /// This inbox's storage cap.
    pub cap_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// `code` is empty for an open node.
    Register { label: Label, fetch_hash: [u8; 32], code: Vec<u8> },
    Deposit { label: Label, id: MailId, blob: Vec<u8> },
    Fetch { label: Label, secret: [u8; 32] },
    Ack { label: Label, secret: [u8; 32], id: MailId },
    Status { label: Label, secret: [u8; 32] },
    Registered { ttl_secs: u32 },
    Deposited { id: MailId },
    Mail { id: MailId, blob: Vec<u8> },
    FetchDone { count: u16 },
    Acked { id: MailId },
    Refused(Refusal),
    StatusReply(InboxStatus),
    /// The inbox owner asks the node to wake their phone when mail
    /// arrives: post a `Message` wake-up, sealed to `push_key`, to
    /// `endpoint` (a push endpoint's text form). An empty endpoint removes
    /// it. Authorised by the fetch secret, like [`Message::Status`].
    RegisterPush { label: Label, secret: [u8; 32], push_key: [u8; 32], endpoint: String },
    /// Pass an already sealed wake-up on to `endpoint`. For a sender that
    /// cannot post it itself (an APNs target needs the operator's Apple
    /// key) or would rather the push service saw the node's address than
    /// its own. `code` is the members code on a community node, empty on
    /// an open one.
    Wake { code: Vec<u8>, endpoint: String, sealed: Vec<u8> },
    /// The node will wake this inbox's owner. `node_key` is the identity
    /// key the node signs those wake-ups with, so the phone can tell them
    /// from a stranger's and accept them as mail notices only.
    PushRegistered { node_key: [u8; 32] },
    /// The node passed a [`Message::Wake`] on to the push service.
    Woken,
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Register { label, fetch_hash, code } => {
                out.push(OP_REGISTER);
                out.extend_from_slice(label);
                out.extend_from_slice(fetch_hash);
                out.extend_from_slice(code);
            }
            Message::Deposit { label, id, blob } => {
                out.push(OP_DEPOSIT);
                out.extend_from_slice(label);
                out.extend_from_slice(id);
                out.extend_from_slice(blob);
            }
            Message::Fetch { label, secret } => {
                out.push(OP_FETCH);
                out.extend_from_slice(label);
                out.extend_from_slice(secret);
            }
            Message::Ack { label, secret, id } => {
                out.push(OP_ACK);
                out.extend_from_slice(label);
                out.extend_from_slice(secret);
                out.extend_from_slice(id);
            }
            Message::Status { label, secret } => {
                out.push(OP_STATUS);
                out.extend_from_slice(label);
                out.extend_from_slice(secret);
            }
            Message::Registered { ttl_secs } => {
                out.push(OP_REGISTERED);
                out.extend_from_slice(&ttl_secs.to_be_bytes());
            }
            Message::Deposited { id } => {
                out.push(OP_DEPOSITED);
                out.extend_from_slice(id);
            }
            Message::Mail { id, blob } => {
                out.push(OP_MAIL);
                out.extend_from_slice(id);
                out.extend_from_slice(blob);
            }
            Message::FetchDone { count } => {
                out.push(OP_FETCH_DONE);
                out.extend_from_slice(&count.to_be_bytes());
            }
            Message::Acked { id } => {
                out.push(OP_ACKED);
                out.extend_from_slice(id);
            }
            Message::Refused(r) => {
                out.push(OP_REFUSED);
                out.push(r.code());
            }
            Message::RegisterPush { label, secret, push_key, endpoint } => {
                out.push(OP_REGISTER_PUSH);
                out.extend_from_slice(label);
                out.extend_from_slice(secret);
                out.extend_from_slice(push_key);
                out.extend_from_slice(endpoint.as_bytes());
            }
            Message::Wake { code, endpoint, sealed } => {
                out.push(OP_WAKE);
                out.push(code.len() as u8);
                out.extend_from_slice(code);
                out.extend_from_slice(sealed);
                out.extend_from_slice(endpoint.as_bytes());
            }
            Message::PushRegistered { node_key } => {
                out.push(OP_PUSH_REGISTERED);
                out.extend_from_slice(node_key);
            }
            Message::Woken => out.push(OP_WOKEN),
            Message::StatusReply(s) => {
                out.push(OP_STATUS_REPLY);
                out.extend_from_slice(&s.held.to_be_bytes());
                out.extend_from_slice(&s.held_bytes.to_be_bytes());
                out.extend_from_slice(&s.cap_bytes.to_be_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, b) = rest.split_first()?;
        let arr32 = |s: &[u8]| -> Option<[u8; 32]> { s.try_into().ok() };
        let arr16 = |s: &[u8]| -> Option<[u8; 16]> { s.try_into().ok() };
        Some(match op {
            OP_REGISTER if b.len() >= 64 && b.len() <= 64 + 64 => Message::Register {
                label: arr32(&b[..32])?,
                fetch_hash: arr32(&b[32..64])?,
                code: b[64..].to_vec(),
            },
            OP_DEPOSIT if b.len() > 48 && b.len() - 48 <= MAX_BLOB => Message::Deposit {
                label: arr32(&b[..32])?,
                id: arr16(&b[32..48])?,
                blob: b[48..].to_vec(),
            },
            OP_FETCH if b.len() == 64 => Message::Fetch { label: arr32(&b[..32])?, secret: arr32(&b[32..])? },
            OP_ACK if b.len() == 80 => Message::Ack {
                label: arr32(&b[..32])?,
                secret: arr32(&b[32..64])?,
                id: arr16(&b[64..])?,
            },
            OP_STATUS if b.len() == 64 => Message::Status { label: arr32(&b[..32])?, secret: arr32(&b[32..])? },
            OP_REGISTERED => Message::Registered { ttl_secs: u32::from_be_bytes(b.try_into().ok()?) },
            OP_DEPOSITED => Message::Deposited { id: arr16(b)? },
            OP_MAIL if b.len() > 16 && b.len() - 16 <= MAX_BLOB => {
                Message::Mail { id: arr16(&b[..16])?, blob: b[16..].to_vec() }
            }
            OP_FETCH_DONE => Message::FetchDone { count: u16::from_be_bytes(b.try_into().ok()?) },
            OP_ACKED => Message::Acked { id: arr16(b)? },
            OP_REFUSED if b.len() == 1 => Message::Refused(Refusal::from_code(b[0])?),
            OP_REGISTER_PUSH if b.len() >= 96 && b.len() - 96 <= MAX_ENDPOINT => Message::RegisterPush {
                label: arr32(&b[..32])?,
                secret: arr32(&b[32..64])?,
                push_key: arr32(&b[64..96])?,
                endpoint: String::from_utf8(b[96..].to_vec()).ok()?,
            },
            OP_WAKE if !b.is_empty() => {
                let code_len = b[0] as usize;
                let rest = &b[1..];
                if code_len > MAX_CODE || rest.len() < code_len + WAKE_LEN || rest.len() - code_len - WAKE_LEN > MAX_ENDPOINT {
                    return None;
                }
                Message::Wake {
                    code: rest[..code_len].to_vec(),
                    sealed: rest[code_len..code_len + WAKE_LEN].to_vec(),
                    endpoint: String::from_utf8(rest[code_len + WAKE_LEN..].to_vec()).ok()?,
                }
            }
            OP_PUSH_REGISTERED => Message::PushRegistered { node_key: arr32(b)? },
            OP_WOKEN if b.is_empty() => Message::Woken,
            OP_STATUS_REPLY if b.len() == 20 => Message::StatusReply(InboxStatus {
                held: u32::from_be_bytes(b[..4].try_into().ok()?),
                held_bytes: u64::from_be_bytes(b[4..12].try_into().ok()?),
                cap_bytes: u64::from_be_bytes(b[12..20].try_into().ok()?),
            }),
            _ => return None,
        })
    }

    pub fn is_mailbox_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
}

/// Sends `request` until a reply `accept` takes, or the timeout passes.
async fn exchange<T>(
    socket: &impl Datagram,
    node: SocketAddr,
    request: &Message,
    timeout: Duration,
    mut accept: impl FnMut(Message) -> Option<Result<T, NetError>>,
) -> Result<T, NetError> {
    let bytes = request.encode();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 2048];
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&bytes, node).await?;
        let wait_until = (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline);
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(wait_until, socket.recv_from(&mut buf)).await {
            if from != node {
                continue;
            }
            if let Some(m) = Message::decode(&buf[..n]) {
                if let Some(result) = accept(m) {
                    return result;
                }
            }
        }
    }
    Err(NetError::MailboxTimeout)
}

/// Registers (or refreshes) an inbox. Returns how long it lasts before
/// it must be refreshed.
pub async fn register(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    fetch_hash: [u8; 32],
    code: &[u8],
    timeout: Duration,
) -> Result<Duration, NetError> {
    let request = Message::Register { label, fetch_hash, code: code.to_vec() };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Registered { ttl_secs } => Some(Ok(Duration::from_secs(ttl_secs as u64))),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

/// Drops a sealed blob into someone's inbox.
pub async fn deposit(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    id: MailId,
    blob: &[u8],
    timeout: Duration,
) -> Result<(), NetError> {
    if blob.len() > MAX_BLOB {
        return Err(NetError::MailboxRefused(Refusal::TooBig));
    }
    let request = Message::Deposit { label, id, blob: blob.to_vec() };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Deposited { id: got } if got == id => Some(Ok(())),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

/// Collects what is waiting. Nothing is deleted until [`ack`].
pub async fn fetch(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    secret: [u8; 32],
    timeout: Duration,
) -> Result<Vec<(MailId, Vec<u8>)>, NetError> {
    let mut got: Vec<(MailId, Vec<u8>)> = Vec::new();
    let request = Message::Fetch { label, secret };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Mail { id, blob } => {
            if !got.iter().any(|(g, _)| *g == id) {
                got.push((id, blob));
            }
            None
        }
        Message::FetchDone { .. } => Some(Ok(())),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await?;
    Ok(got)
}

/// Deletes one fetched blob from the node.
pub async fn ack(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    secret: [u8; 32],
    id: MailId,
    timeout: Duration,
) -> Result<(), NetError> {
    let request = Message::Ack { label, secret, id };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Acked { id: got } if got == id => Some(Ok(())),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

/// The owner's view of their inbox at a node.
pub async fn status(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    secret: [u8; 32],
    timeout: Duration,
) -> Result<InboxStatus, NetError> {
    let request = Message::Status { label, secret };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::StatusReply(s) => Some(Ok(s)),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

/// Asks a node to wake this inbox's owner when mail arrives, or with an
/// empty `endpoint` to stop. Returns the key the node will sign its
/// wake-ups with.
pub async fn register_push(
    socket: &impl Datagram,
    node: SocketAddr,
    label: Label,
    secret: [u8; 32],
    push_key: [u8; 32],
    endpoint: &str,
    timeout: Duration,
) -> Result<[u8; 32], NetError> {
    if endpoint.len() > MAX_ENDPOINT {
        return Err(NetError::MailboxRefused(Refusal::TooBig));
    }
    let request = Message::RegisterPush { label, secret, push_key, endpoint: endpoint.to_string() };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::PushRegistered { node_key } => Some(Ok(node_key)),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

/// Asks a node to pass a sealed wake-up on to `endpoint`. Resent until the
/// node answers, like everything else here; a wake-up delivered twice is
/// harmless, since the phone drops a repeated id.
pub async fn forward_wake(
    socket: &impl Datagram,
    node: SocketAddr,
    code: &[u8],
    endpoint: &str,
    sealed: &[u8],
    timeout: Duration,
) -> Result<(), NetError> {
    if sealed.len() != WAKE_LEN || endpoint.len() > MAX_ENDPOINT || code.len() > MAX_CODE {
        return Err(NetError::MailboxRefused(Refusal::TooBig));
    }
    let request = Message::Wake { code: code.to_vec(), endpoint: endpoint.to_string(), sealed: sealed.to_vec() };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Woken => Some(Ok(())),
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(r))),
        _ => None,
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        for m in [
            Message::Register { label: [1; 32], fetch_hash: [2; 32], code: b"club".to_vec() },
            Message::Register { label: [1; 32], fetch_hash: [2; 32], code: vec![] },
            Message::Deposit { label: [3; 32], id: [4; 16], blob: vec![5; 900] },
            Message::Fetch { label: [6; 32], secret: [7; 32] },
            Message::Ack { label: [6; 32], secret: [7; 32], id: [8; 16] },
            Message::Status { label: [6; 32], secret: [7; 32] },
            Message::Registered { ttl_secs: 3600 },
            Message::Deposited { id: [9; 16] },
            Message::Mail { id: [9; 16], blob: vec![1, 2, 3] },
            Message::FetchDone { count: 3 },
            Message::Acked { id: [9; 16] },
            Message::Refused(Refusal::UnknownLabel),
            Message::StatusReply(InboxStatus { held: 2, held_bytes: 300, cap_bytes: 1 << 20 }),
            Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: "https://ntfy.sh/upX".into() },
            Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: String::new() },
            Message::Wake { code: b"club".to_vec(), endpoint: "apns-voip:abcd".into(), sealed: vec![7; WAKE_LEN] },
            Message::Wake { code: vec![], endpoint: "https://h/x".into(), sealed: vec![7; WAKE_LEN] },
            Message::PushRegistered { node_key: [4; 32] },
            Message::Woken,
            Message::Refused(Refusal::NoPush),
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn an_oversized_deposit_is_not_even_decoded() {
        let m = Message::Deposit { label: [0; 32], id: [0; 16], blob: vec![0; MAX_BLOB + 1] };
        assert!(Message::decode(&m.encode()).is_none());
    }

    #[test]
    fn mailbox_and_relay_traffic_never_mix() {
        let relay = crate::relay::Message::Dialed.encode();
        assert!(Message::decode(&relay).is_none());
        let mail = Message::FetchDone { count: 0 }.encode();
        assert!(crate::relay::Message::decode(&mail).is_none());
    }
}
