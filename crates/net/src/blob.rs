//! The file-sharing protocol (spec §5: seeding, pinning, server-hosted
//! files and multi-peer download).
//!
//! Whoever holds a copy of a file -- the sender, someone seeding it after
//! downloading, a device or server it was pinned to -- answers the same
//! three questions: how big is it, what are its chunk hashes, and give me
//! chunk N. Every chunk is sealed under the file's own key
//! (`antiphon_crypto::file_seal`), so holders serve bytes they can't read,
//! and every chunk is checked against the file's id the moment it
//! arrives, so a downloader can take chunks from several holders at once
//! and drop any that lie.
//!
//! Who may download is the holder's *seeding scope*, enforced here rather
//! than in a UI:
//!
//! - **This conversation**: the request must carry the file's read token,
//!   which only people the file was shared with can derive.
//! - **Contacts**: the same, and the requester must first prove, by
//!   signature, an identity the holder has saved as a contact.
//! - **Anyone**: no token. They still can't read it without the key.
//!
//! Pinning uploads a sealed copy to a node with `Put*`; the node checks
//! every chunk against the hashes before keeping it.

use std::net::SocketAddr;
use std::time::Duration;

use crate::socket::Datagram;

use crate::error::NetError;
pub use antiphon_crypto::file_seal::{ChunkHash, FileId, CHUNK, TAG};

pub const MAGIC: [u8; 4] = [0xA7, b'B', b'L', b'1'];

/// Chunk hashes per page: one page is a kilobyte.
pub const HASHES_PER_PAGE: usize = 32;
/// Most bytes a sealed chunk may be.
pub const MAX_SEALED_CHUNK: usize = CHUNK + TAG;
/// Most bytes a members code may be.
pub const MAX_CODE: usize = 64;

const OP_INFO: u8 = 0x01;
const OP_HASHES: u8 = 0x02;
const OP_CHUNK: u8 = 0x03;
const OP_CLAIM: u8 = 0x04;
const OP_PUT_INFO: u8 = 0x05;
const OP_PUT_HASHES: u8 = 0x06;
const OP_PUT_CHUNK: u8 = 0x07;
const OP_INFO_REPLY: u8 = 0x81;
const OP_HASH_PAGE: u8 = 0x82;
const OP_CHUNK_DATA: u8 = 0x83;
const OP_CLAIM_OK: u8 = 0x84;
const OP_PUT_OK: u8 = 0x85;
const OP_REFUSED: u8 = 0x86;

/// `PutOk` for the `PutInfo` step, rather than a page or a chunk.
pub const PUT_INFO_ACK: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// This holder has never had it.
    NotHere,
    /// It was here and has been removed: shared no longer.
    NoLongerShared,
    /// Wrong or missing read token.
    Forbidden,
    /// Shared with contacts (or a community) only, and you haven't shown
    /// you're one.
    NotAContact,
    TooBig,
    Full,
    /// This uploader's quota is used up.
    OverQuota,
    /// This holder doesn't share or host files.
    Disabled,
    /// A chunk that doesn't match its hash, or hashes that don't match
    /// the id.
    BadData,
}

impl Refusal {
    fn code(self) -> u8 {
        self as u8 + 1
    }

    fn from_code(c: u8) -> Option<Self> {
        use Refusal::*;
        [NotHere, NoLongerShared, Forbidden, NotAContact, TooBig, Full, OverQuota, Disabled, BadData]
            .get(c.checked_sub(1)? as usize)
            .copied()
    }

    pub fn describe(self) -> &'static str {
        match self {
            Refusal::NotHere => "they don't have that file",
            Refusal::NoLongerShared => "that file is no longer shared",
            Refusal::Forbidden => "that file isn't shared with you",
            Refusal::NotAContact => "that file is shared with their contacts or community only",
            Refusal::TooBig => "the file is bigger than this node allows",
            Refusal::Full => "the node is full",
            Refusal::OverQuota => "your storage on this node is used up",
            Refusal::Disabled => "this node doesn't host files",
            Refusal::BadData => "a chunk didn't match the file",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Info { file: FileId, token: [u8; 32] },
    Hashes { file: FileId, token: [u8; 32], page: u32 },
    Chunk { file: FileId, token: [u8; 32], index: u32 },
    /// "I am this identity": a signature over the file id and a time.
    Claim { file: FileId, identity: [u8; 32], at_secs: u64, signature: [u8; 64] },
    /// Pinning: announce an upload. `uploader` is a per-node label the
    /// uploader derives, for quotas; `read_token` is what the node will
    /// require of downloaders (all zeros for anyone).
    PutInfo { file: FileId, size: u64, uploader: [u8; 32], read_token: [u8; 32], code: Vec<u8> },
    PutHashes { file: FileId, uploader: [u8; 32], page: u32, hashes: Vec<ChunkHash> },
    PutChunk { file: FileId, uploader: [u8; 32], index: u32, data: Vec<u8> },
    InfoReply { size: u64, chunks: u32 },
    HashPage { page: u32, hashes: Vec<ChunkHash> },
    ChunkData { index: u32, data: Vec<u8> },
    ClaimOk,
    PutOk { which: u32 },
    Refused(Refusal),
}

/// What a contacts-only holder asks to be signed.
pub fn claim_bytes(file: &FileId, at_secs: u64) -> Vec<u8> {
    let mut m = b"antiphon/file/claim/v1".to_vec();
    m.extend_from_slice(file);
    m.extend_from_slice(&at_secs.to_be_bytes());
    m
}

fn arr32(b: &[u8]) -> Option<[u8; 32]> {
    b.try_into().ok()
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn hashes_from(b: &[u8]) -> Option<Vec<ChunkHash>> {
    if !b.len().is_multiple_of(32) || b.len() / 32 > HASHES_PER_PAGE {
        return None;
    }
    Some(b.chunks_exact(32).map(|c| c.try_into().expect("32")).collect())
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Info { file, token } => {
                out.push(OP_INFO);
                out.extend_from_slice(file);
                out.extend_from_slice(token);
            }
            Message::Hashes { file, token, page } => {
                out.push(OP_HASHES);
                out.extend_from_slice(file);
                out.extend_from_slice(token);
                out.extend_from_slice(&page.to_be_bytes());
            }
            Message::Chunk { file, token, index } => {
                out.push(OP_CHUNK);
                out.extend_from_slice(file);
                out.extend_from_slice(token);
                out.extend_from_slice(&index.to_be_bytes());
            }
            Message::Claim { file, identity, at_secs, signature } => {
                out.push(OP_CLAIM);
                out.extend_from_slice(file);
                out.extend_from_slice(identity);
                out.extend_from_slice(&at_secs.to_be_bytes());
                out.extend_from_slice(signature);
            }
            Message::PutInfo { file, size, uploader, read_token, code } => {
                out.push(OP_PUT_INFO);
                out.extend_from_slice(file);
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(uploader);
                out.extend_from_slice(read_token);
                out.extend_from_slice(code);
            }
            Message::PutHashes { file, uploader, page, hashes } => {
                out.push(OP_PUT_HASHES);
                out.extend_from_slice(file);
                out.extend_from_slice(uploader);
                out.extend_from_slice(&page.to_be_bytes());
                for h in hashes {
                    out.extend_from_slice(h);
                }
            }
            Message::PutChunk { file, uploader, index, data } => {
                out.push(OP_PUT_CHUNK);
                out.extend_from_slice(file);
                out.extend_from_slice(uploader);
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(data);
            }
            Message::InfoReply { size, chunks } => {
                out.push(OP_INFO_REPLY);
                out.extend_from_slice(&size.to_be_bytes());
                out.extend_from_slice(&chunks.to_be_bytes());
            }
            Message::HashPage { page, hashes } => {
                out.push(OP_HASH_PAGE);
                out.extend_from_slice(&page.to_be_bytes());
                for h in hashes {
                    out.extend_from_slice(h);
                }
            }
            Message::ChunkData { index, data } => {
                out.push(OP_CHUNK_DATA);
                out.extend_from_slice(&index.to_be_bytes());
                out.extend_from_slice(data);
            }
            Message::ClaimOk => out.push(OP_CLAIM_OK),
            Message::PutOk { which } => {
                out.push(OP_PUT_OK);
                out.extend_from_slice(&which.to_be_bytes());
            }
            Message::Refused(r) => {
                out.push(OP_REFUSED);
                out.push(r.code());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, b) = rest.split_first()?;
        Some(match op {
            OP_INFO if b.len() == 64 => Message::Info { file: arr32(&b[..32])?, token: arr32(&b[32..])? },
            OP_HASHES if b.len() == 68 => {
                Message::Hashes { file: arr32(&b[..32])?, token: arr32(&b[32..64])?, page: u32_at(b, 64)? }
            }
            OP_CHUNK if b.len() == 68 => {
                Message::Chunk { file: arr32(&b[..32])?, token: arr32(&b[32..64])?, index: u32_at(b, 64)? }
            }
            OP_CLAIM if b.len() == 32 + 32 + 8 + 64 => Message::Claim {
                file: arr32(&b[..32])?,
                identity: arr32(&b[32..64])?,
                at_secs: u64_at(b, 64)?,
                signature: b[72..].try_into().ok()?,
            },
            OP_PUT_INFO if b.len() >= 104 && b.len() <= 104 + MAX_CODE => Message::PutInfo {
                file: arr32(&b[..32])?,
                size: u64_at(b, 32)?,
                uploader: arr32(&b[40..72])?,
                read_token: arr32(&b[72..104])?,
                code: b[104..].to_vec(),
            },
            OP_PUT_HASHES if b.len() >= 68 => Message::PutHashes {
                file: arr32(&b[..32])?,
                uploader: arr32(&b[32..64])?,
                page: u32_at(b, 64)?,
                hashes: hashes_from(&b[68..])?,
            },
            OP_PUT_CHUNK if b.len() > 68 && b.len() - 68 <= MAX_SEALED_CHUNK => Message::PutChunk {
                file: arr32(&b[..32])?,
                uploader: arr32(&b[32..64])?,
                index: u32_at(b, 64)?,
                data: b[68..].to_vec(),
            },
            OP_INFO_REPLY if b.len() == 12 => Message::InfoReply { size: u64_at(b, 0)?, chunks: u32_at(b, 8)? },
            OP_HASH_PAGE if b.len() >= 4 => Message::HashPage { page: u32_at(b, 0)?, hashes: hashes_from(&b[4..])? },
            OP_CHUNK_DATA if b.len() >= 4 && b.len() - 4 <= MAX_SEALED_CHUNK => {
                Message::ChunkData { index: u32_at(b, 0)?, data: b[4..].to_vec() }
            }
            OP_CLAIM_OK if b.is_empty() => Message::ClaimOk,
            OP_PUT_OK if b.len() == 4 => Message::PutOk { which: u32_at(b, 0)? },
            OP_REFUSED if b.len() == 1 => Message::Refused(Refusal::from_code(b[0])?),
            _ => return None,
        })
    }

    pub fn is_blob_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }
}

async fn exchange<T>(
    socket: &impl Datagram,
    holder: SocketAddr,
    request: &Message,
    timeout: Duration,
    mut accept: impl FnMut(Message) -> Option<Result<T, NetError>>,
) -> Result<T, NetError> {
    let bytes = request.encode();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = vec![0u8; 2048];
    while tokio::time::Instant::now() < deadline {
        socket.send_to(&bytes, holder).await?;
        let wait_until = (tokio::time::Instant::now() + Duration::from_millis(400)).min(deadline);
        while let Ok(Ok((n, from))) = tokio::time::timeout_at(wait_until, socket.recv_from(&mut buf)).await {
            if from != holder {
                continue;
            }
            if let Some(m) = Message::decode(&buf[..n]) {
                if let Some(result) = accept(m) {
                    return result;
                }
            }
        }
    }
    Err(NetError::FileHolderTimeout)
}

fn refused<T>(m: &Message) -> Option<Result<T, NetError>> {
    match m {
        Message::Refused(r) => Some(Err(NetError::FileRefused(*r))),
        _ => None,
    }
}

/// Size and chunk count, from one holder.
pub async fn info(
    socket: &impl Datagram,
    holder: SocketAddr,
    file: FileId,
    token: [u8; 32],
    timeout: Duration,
) -> Result<(u64, u32), NetError> {
    exchange(socket, holder, &Message::Info { file, token }, timeout, |m| match m {
        Message::InfoReply { size, chunks } => Some(Ok((size, chunks))),
        other => refused(&other),
    })
    .await
}

/// Every chunk hash, page by page. Not checked here: the caller checks
/// them against the file id, which is the only thing it can trust.
pub async fn hashes(
    socket: &impl Datagram,
    holder: SocketAddr,
    file: FileId,
    token: [u8; 32],
    chunks: u32,
    timeout: Duration,
) -> Result<Vec<ChunkHash>, NetError> {
    let pages = (chunks as usize).div_ceil(HASHES_PER_PAGE) as u32;
    let mut all = Vec::with_capacity(chunks as usize);
    for page in 0..pages {
        let got = exchange(socket, holder, &Message::Hashes { file, token, page }, timeout, |m| match m {
            Message::HashPage { page: p, hashes } if p == page => Some(Ok(hashes)),
            other => refused(&other),
        })
        .await?;
        all.extend(got);
    }
    Ok(all)
}

/// One sealed chunk.
pub async fn chunk(
    socket: &impl Datagram,
    holder: SocketAddr,
    file: FileId,
    token: [u8; 32],
    index: u32,
    timeout: Duration,
) -> Result<Vec<u8>, NetError> {
    exchange(socket, holder, &Message::Chunk { file, token, index }, timeout, |m| match m {
        Message::ChunkData { index: i, data } if i == index => Some(Ok(data)),
        other => refused(&other),
    })
    .await
}

/// Proves an identity to a contacts-only holder. The holder remembers
/// this socket address for the file for a few minutes.
pub async fn claim(
    socket: &impl Datagram,
    holder: SocketAddr,
    file: FileId,
    identity: [u8; 32],
    at_secs: u64,
    signature: [u8; 64],
    timeout: Duration,
) -> Result<(), NetError> {
    exchange(socket, holder, &Message::Claim { file, identity, at_secs, signature }, timeout, |m| match m {
        Message::ClaimOk => Some(Ok(())),
        other => refused(&other),
    })
    .await
}

/// Uploads a sealed copy to a node (spec §5 "Pinning").
#[allow(clippy::too_many_arguments)]
pub async fn put(
    socket: &impl Datagram,
    node: SocketAddr,
    file: FileId,
    size: u64,
    uploader: [u8; 32],
    read_token: [u8; 32],
    code: &[u8],
    hashes: &[ChunkHash],
    mut sealed_chunk: impl FnMut(u32) -> Option<Vec<u8>>,
    timeout: Duration,
) -> Result<(), NetError> {
    let ack = |want: u32| move |m: Message| match m {
        Message::PutOk { which } if which == want => Some(Ok(())),
        other => refused(&other),
    };
    let info = Message::PutInfo { file, size, uploader, read_token, code: code.to_vec() };
    exchange(socket, node, &info, timeout, ack(PUT_INFO_ACK)).await?;
    for (page, chunk) in hashes.chunks(HASHES_PER_PAGE).enumerate() {
        let m = Message::PutHashes { file, uploader, page: page as u32, hashes: chunk.to_vec() };
        exchange(socket, node, &m, timeout, ack(page as u32)).await?;
    }
    for index in 0..hashes.len() as u32 {
        let data = sealed_chunk(index).ok_or(NetError::FileRefused(Refusal::BadData))?;
        let m = Message::PutChunk { file, uploader, index, data };
        exchange(socket, node, &m, timeout, ack(index)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        for m in [
            Message::Info { file: [1; 32], token: [2; 32] },
            Message::Hashes { file: [1; 32], token: [2; 32], page: 3 },
            Message::Chunk { file: [1; 32], token: [2; 32], index: 9 },
            Message::Claim { file: [1; 32], identity: [4; 32], at_secs: 77, signature: [5; 64] },
            Message::PutInfo { file: [1; 32], size: 99, uploader: [6; 32], read_token: [7; 32], code: b"club".to_vec() },
            Message::PutHashes { file: [1; 32], uploader: [6; 32], page: 0, hashes: vec![[8; 32]; 32] },
            Message::PutChunk { file: [1; 32], uploader: [6; 32], index: 4, data: vec![9; MAX_SEALED_CHUNK] },
            Message::InfoReply { size: 5000, chunks: 5 },
            Message::HashPage { page: 1, hashes: vec![[3; 32]; 5] },
            Message::ChunkData { index: 2, data: vec![1; 100] },
            Message::ClaimOk,
            Message::PutOk { which: PUT_INFO_ACK },
            Message::Refused(Refusal::NoLongerShared),
            Message::Refused(Refusal::BadData),
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn a_full_chunk_fits_a_datagram() {
        let m = Message::PutChunk { file: [0; 32], uploader: [0; 32], index: 0, data: vec![0; MAX_SEALED_CHUNK] };
        assert!(m.encode().len() < 1200);
    }

    #[test]
    fn other_protocols_never_decode_as_files() {
        assert!(Message::decode(&crate::mailbox::Message::FetchDone { count: 0 }.encode()).is_none());
        assert!(Message::decode(&crate::relay::Message::Dialed.encode()).is_none());
    }
}
