//! Community boards (spec §11 "Communities at full scale"): an
//! append-only log a mailbox node keeps for one community, so a message
//! for every member is uploaded **once** and each member reads it from the
//! node, instead of the sender sealing a copy to each member's inbox.
//!
//! The node holds bytes it can't read: every entry is sealed by the
//! members under a key from their MLS group's current epoch, and the board
//! is named by an id only members know. Who may post and read is decided
//! the same way, without the node knowing who anyone is: a request carries
//! a **token** derived from an epoch's MLS secret, and the node keeps the
//! hashes of the tokens for the last few epochs. Whoever moves the group
//! to a new epoch adds that epoch's hash (proving a current token), so a
//! member removed from the group -- who never learns the new epoch's
//! secret -- loses access by the next epoch without anyone telling the
//! node about them.
//!
//! Entries are numbered from 1 and never change; a member keeps the
//! number it has read up to and asks for what came after, a page at a
//! time, which is also how a member who was away catches up. The node
//! drops the oldest entries past its size cap or age.
//!
//! The protocol shares the mailbox's [`MAGIC`](crate::mailbox::MAGIC):
//! boards are a mailbox node's service, on the same port, with opcodes the
//! inbox protocol doesn't use.
//!
//! ```text
//! OPEN      MAGIC 0x20 board:32 token_hash:32           make a board
//! REKEY     MAGIC 0x21 board:32 token:32 new_hash:32    allow a new epoch
//! POST      MAGIC 0x22 board:32 token:32 entry          append
//! FETCH     MAGIC 0x23 board:32 token:32 since:8 max:2  read after `since`
//! OPENED    MAGIC 0xA0 board:32
//! REKEYED   MAGIC 0xA1 board:32
//! POSTED    MAGIC 0xA2 board:32 seq:8
//! ENTRY     MAGIC 0xA3 seq:8 entry                      one per datagram
//! DONE      MAGIC 0xA4 first:8 next:8 last:8            end of a page
//! REFUSED   MAGIC 0xA5 code:1                           as the mailbox's
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::NetError;
use crate::mailbox::{Refusal, MAGIC};
use crate::socket::Datagram;

/// A board's id: random, kept in the community's manifest.
pub type BoardId = [u8; 32];
/// An epoch's access token.
pub type Token = [u8; 32];

/// The largest entry a node takes; the same bound as a mailbox blob, so
/// every reply fits one datagram.
pub const MAX_ENTRY: usize = crate::mailbox::MAX_BLOB;
/// Most entries a node returns per fetch.
pub const MAX_PAGE: u16 = 64;

const OP_OPEN: u8 = 0x20;
const OP_REKEY: u8 = 0x21;
const OP_POST: u8 = 0x22;
const OP_FETCH: u8 = 0x23;
const OP_OPENED: u8 = 0xA0;
const OP_REKEYED: u8 = 0xA1;
const OP_POSTED: u8 = 0xA2;
const OP_ENTRY: u8 = 0xA3;
const OP_DONE: u8 = 0xA4;
const OP_REFUSED: u8 = 0xA5;

/// What a node keeps of a token: its hash.
pub fn token_hash(token: &Token) -> [u8; 32] {
    Sha256::new().chain_update(b"antiphon/board/token").chain_update(token).finalize().into()
}

/// One board datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Open { board: BoardId, token_hash: [u8; 32] },
    Rekey { board: BoardId, token: Token, new_hash: [u8; 32] },
    Post { board: BoardId, token: Token, entry: Vec<u8> },
    Fetch { board: BoardId, token: Token, since: u64, max: u16 },
    Opened { board: BoardId },
    Rekeyed { board: BoardId },
    Posted { board: BoardId, seq: u64 },
    Entry { seq: u64, entry: Vec<u8> },
    /// The page is complete. `first` is the first entry it held (0 for
    /// none), `next` the `since` to ask with for the following page (the
    /// last entry it held), and `last` the newest entry on the board.
    Done { first: u64, next: u64, last: u64 },
    Refused(Refusal),
}

fn take<const N: usize>(b: &[u8], at: usize) -> Option<[u8; N]> {
    b.get(at..at + N)?.try_into().ok()
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Open { board, token_hash } => {
                out.push(OP_OPEN);
                out.extend_from_slice(board);
                out.extend_from_slice(token_hash);
            }
            Message::Rekey { board, token, new_hash } => {
                out.push(OP_REKEY);
                out.extend_from_slice(board);
                out.extend_from_slice(token);
                out.extend_from_slice(new_hash);
            }
            Message::Post { board, token, entry } => {
                out.push(OP_POST);
                out.extend_from_slice(board);
                out.extend_from_slice(token);
                out.extend_from_slice(entry);
            }
            Message::Fetch { board, token, since, max } => {
                out.push(OP_FETCH);
                out.extend_from_slice(board);
                out.extend_from_slice(token);
                out.extend_from_slice(&since.to_be_bytes());
                out.extend_from_slice(&max.to_be_bytes());
            }
            Message::Opened { board } => {
                out.push(OP_OPENED);
                out.extend_from_slice(board);
            }
            Message::Rekeyed { board } => {
                out.push(OP_REKEYED);
                out.extend_from_slice(board);
            }
            Message::Posted { board, seq } => {
                out.push(OP_POSTED);
                out.extend_from_slice(board);
                out.extend_from_slice(&seq.to_be_bytes());
            }
            Message::Entry { seq, entry } => {
                out.push(OP_ENTRY);
                out.extend_from_slice(&seq.to_be_bytes());
                out.extend_from_slice(entry);
            }
            Message::Done { first, next, last } => {
                out.push(OP_DONE);
                out.extend_from_slice(&first.to_be_bytes());
                out.extend_from_slice(&next.to_be_bytes());
                out.extend_from_slice(&last.to_be_bytes());
            }
            Message::Refused(r) => {
                out.push(OP_REFUSED);
                out.push(r.code());
            }
        }
        out
    }

    /// `None` for anything that isn't a well-formed board datagram
    /// (including every inbox datagram, which shares the magic).
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let b = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, b) = b.split_first()?;
        Some(match op {
            OP_OPEN if b.len() == 64 => Message::Open { board: take(b, 0)?, token_hash: take(b, 32)? },
            OP_REKEY if b.len() == 96 => Message::Rekey { board: take(b, 0)?, token: take(b, 32)?, new_hash: take(b, 64)? },
            OP_POST if b.len() > 64 && b.len() - 64 <= MAX_ENTRY => {
                Message::Post { board: take(b, 0)?, token: take(b, 32)?, entry: b[64..].to_vec() }
            }
            OP_FETCH if b.len() == 74 => Message::Fetch {
                board: take(b, 0)?,
                token: take(b, 32)?,
                since: u64::from_be_bytes(take(b, 64)?),
                max: u16::from_be_bytes(take(b, 72)?),
            },
            OP_OPENED if b.len() == 32 => Message::Opened { board: take(b, 0)? },
            OP_REKEYED if b.len() == 32 => Message::Rekeyed { board: take(b, 0)? },
            OP_POSTED if b.len() == 40 => Message::Posted { board: take(b, 0)?, seq: u64::from_be_bytes(take(b, 32)?) },
            OP_ENTRY if b.len() > 8 => Message::Entry { seq: u64::from_be_bytes(take(b, 0)?), entry: b[8..].to_vec() },
            OP_DONE if b.len() == 24 => Message::Done {
                first: u64::from_be_bytes(take(b, 0)?),
                next: u64::from_be_bytes(take(b, 8)?),
                last: u64::from_be_bytes(take(b, 16)?),
            },
            OP_REFUSED if b.len() == 1 => Message::Refused(Refusal::from_code(b[0])?),
            _ => return None,
        })
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

fn refused<T>(m: &Message) -> Option<Result<T, NetError>> {
    match m {
        Message::Refused(r) => Some(Err(NetError::MailboxRefused(*r))),
        _ => None,
    }
}

/// Makes board `board` at `node`, letting holders of `token` in.
/// Succeeds again (doing nothing) for a board that already exists with
/// that token allowed.
pub async fn open(socket: &impl Datagram, node: SocketAddr, board: BoardId, token: &Token, timeout: Duration) -> Result<(), NetError> {
    let request = Message::Open { board, token_hash: token_hash(token) };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Opened { board: b } if b == board => Some(Ok(())),
        other => refused(&other),
    })
    .await
}

/// Lets holders of `new` in too, proving `current`.
pub async fn rekey(socket: &impl Datagram, node: SocketAddr, board: BoardId, current: &Token, new: &Token, timeout: Duration) -> Result<(), NetError> {
    let request = Message::Rekey { board, token: *current, new_hash: token_hash(new) };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Rekeyed { board: b } if b == board => Some(Ok(())),
        other => refused(&other),
    })
    .await
}

/// Appends `entry`, returning its number.
pub async fn post(socket: &impl Datagram, node: SocketAddr, board: BoardId, token: &Token, entry: &[u8], timeout: Duration) -> Result<u64, NetError> {
    if entry.len() > MAX_ENTRY {
        return Err(NetError::MailboxRefused(Refusal::TooBig));
    }
    let request = Message::Post { board, token: *token, entry: entry.to_vec() };
    exchange(socket, node, &request, timeout, |m| match m {
        Message::Posted { board: b, seq } if b == board => Some(Ok(seq)),
        other => refused(&other),
    })
    .await
}

/// One page of a board after entry `since`.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// `(number, entry)`, in order and with no gaps.
    pub entries: Vec<(u64, Vec<u8>)>,
    /// The `since` to ask with for the next page.
    pub next: u64,
    /// The newest entry the board holds; caught up once `next == last`.
    pub last: u64,
}

/// Reads up to `max` entries after `since`. Entries older than the node
/// keeps are skipped (the page then starts past them).
///
/// Each entry comes in its own datagram, so one can be lost. The page is
/// cut at the first one missing, and `next` points there, so entries are
/// always handed on in order and the rest comes with the next fetch.
pub async fn fetch(socket: &impl Datagram, node: SocketAddr, board: BoardId, token: &Token, since: u64, max: u16, timeout: Duration) -> Result<Page, NetError> {
    let max = max.clamp(1, MAX_PAGE);
    let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
    let request = Message::Fetch { board, token: *token, since, max };
    let (first, next, last) = exchange(socket, node, &request, timeout, |m| match m {
        Message::Entry { seq, entry } => {
            if seq > since && !got.iter().any(|(s, _)| *s == seq) {
                got.push((seq, entry));
            }
            None
        }
        Message::Done { first, next, last } => Some(Ok((first, next, last))),
        other => refused(&other),
    })
    .await?;
    Ok(in_order(since, got, first, next, last))
}

/// The unbroken run of `got` from `first` up to `next`, and where to ask
/// from after it.
fn in_order(since: u64, mut got: Vec<(u64, Vec<u8>)>, first: u64, next: u64, last: u64) -> Page {
    if first == 0 {
        return Page { entries: Vec::new(), next: next.max(since), last };
    }
    got.sort_by_key(|(s, _)| *s);
    let kept = got.iter().zip(first..).take_while(|((s, _), want)| s == want && *s <= next).count();
    got.truncate(kept);
    let next = got.last().map_or(since, |(s, _)| *s);
    Page { entries: got, next, last }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips_and_inbox_datagrams_are_not_ours() {
        for m in [
            Message::Open { board: [1; 32], token_hash: [2; 32] },
            Message::Rekey { board: [1; 32], token: [3; 32], new_hash: [4; 32] },
            Message::Post { board: [1; 32], token: [3; 32], entry: vec![9; 100] },
            Message::Fetch { board: [1; 32], token: [3; 32], since: 41, max: 32 },
            Message::Opened { board: [1; 32] },
            Message::Rekeyed { board: [1; 32] },
            Message::Posted { board: [1; 32], seq: 7 },
            Message::Entry { seq: 7, entry: vec![1, 2, 3] },
            Message::Done { first: 3, next: 8, last: 12 },
            Message::Refused(Refusal::WrongSecret),
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
        let inbox = crate::mailbox::Message::Fetch { label: [0; 32], secret: [0; 32] }.encode();
        assert_eq!(Message::decode(&inbox), None);
        let too_big = Message::Post { board: [1; 32], token: [3; 32], entry: vec![0; MAX_ENTRY + 1] }.encode();
        assert_eq!(Message::decode(&too_big), None);
    }

    #[test]
    fn a_page_is_cut_at_the_first_lost_entry() {
        let e = |s: u64| (s, vec![s as u8]);
        // Everything arrived (out of order): the whole page.
        let page = in_order(4, vec![e(6), e(5), e(7)], 5, 7, 20);
        assert_eq!((page.entries.len(), page.next, page.last), (3, 7, 20));
        // 6 was lost: only 5, and the next fetch starts after it.
        let page = in_order(4, vec![e(5), e(7)], 5, 7, 20);
        assert_eq!((page.entries, page.next), (vec![e(5)], 5));
        // The first was lost: nothing yet, ask again from the same place.
        let page = in_order(4, vec![e(6), e(7)], 5, 7, 20);
        assert_eq!((page.entries.len(), page.next), (0, 4));
        // Old entries the node dropped are skipped over.
        let page = in_order(1, vec![e(9)], 9, 9, 9);
        assert_eq!((page.entries, page.next), (vec![e(9)], 9));
        // An empty page: caught up.
        assert_eq!(in_order(9, vec![], 0, 9, 9), Page { entries: vec![], next: 9, last: 9 });
    }

    #[test]
    fn a_token_is_kept_only_as_its_hash() {
        assert_ne!(token_hash(&[1; 32]), [1; 32]);
        assert_eq!(token_hash(&[1; 32]), token_hash(&[1; 32]));
        assert_ne!(token_hash(&[1; 32]), token_hash(&[2; 32]));
    }
}
