//! File transfer chunking/reassembly (spec §5): splits a file's bytes into
//! fixed-size chunks for [`crate::PeerConnection::send_file_chunk`], and
//! reassembles + hash-verifies them back on the receiving side.
//!
//! The wire format and the arithmetic live here; where the bytes come
//! from and go to does not. [`encode_chunk`] takes one chunk's bytes, so a
//! sender can read them from disk as it goes, and [`sha256_hex_reader`]
//! hashes a stream, so neither end has to hold a whole file to send or
//! verify it. [`FileReassembler`] is the in-memory receiver, kept for small
//! payloads and for tests; the app's receiver writes straight to disk.
//!
//! Reliability is "blast then repair," not a sliding-window ARQ: the sender
//! sends every chunk once, the receiver tracks what's missing via
//! [`FileReassembler::missing_chunks`] and asks for retransmits (as a
//! `FileMissing` control message -- see `antiphon`'s `control` module,
//! which owns the actual message envelope), and the sender resends just
//! those. Simple, and UDP loss on a single P2P hop rarely needs more than
//! one or two repair rounds in practice.

use sha2::{Digest, Sha256};

/// Conservative relative to [`crate::PeerConnection`]'s 4096-byte
/// `MAX_DATAGRAM`, leaving headroom for the wire frame header (14 bytes),
/// the AEAD tag (16 bytes for ChaCha20-Poly1305), and this module's own
/// 12-byte `[transfer_id][chunk_index]` sub-header -- not cutting it exactly
/// to the limit.
pub const CHUNK_SIZE: usize = 3800;

const SUB_HEADER_LEN: usize = 8 + 4 + 8;

/// A chunk's own checksum, so damage is caught where it happened.
///
/// Eight bytes of SHA-256, not thirty-two: this catches corruption, and
/// corruption is all it is for. Forgery is already covered a layer down --
/// every chunk arrives inside the session's AEAD, so anything that got
/// here at all was sealed with the session key. Spending 32 bytes a chunk
/// to re-solve a solved problem would cost bandwidth for nothing.
///
/// Without it a single damaged byte fails the whole-file hash at the very
/// end, after everything has been transferred, and nothing says which
/// chunk was wrong -- so the only recovery is to send the file again.
/// With it the bad chunk is dropped on arrival and re-requested like any
/// other missing one.
fn chunk_checksum(bytes: &[u8]) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    u64::from_be_bytes(hasher.finalize()[..8].try_into().expect("SHA-256 is 32 bytes"))
}

/// How many [`CHUNK_SIZE`] pieces a file of `file_len` bytes splits into.
pub fn chunk_count(file_len: u64) -> u32 {
    (file_len.div_ceil(CHUNK_SIZE as u64)) as u32
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Splits `data` into [`CHUNK_SIZE`] pieces, each prefixed with the
/// `[transfer_id: u64 BE][chunk_index: u32 BE]` sub-header
/// [`parse_chunk`]/[`crate::PeerConnection::send_file_chunk`] expect.
pub fn build_chunks(transfer_id: u64, data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    data.chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(i, chunk)| {
            let mut out = Vec::with_capacity(SUB_HEADER_LEN + chunk.len());
            out.extend_from_slice(&transfer_id.to_be_bytes());
            out.extend_from_slice(&(i as u32).to_be_bytes());
            out.extend_from_slice(&chunk_checksum(chunk).to_be_bytes());
            out.extend_from_slice(chunk);
            out
        })
        .collect()
}

/// Builds just the one chunk at `index` -- for resending a chunk the
/// receiver reported missing without re-splitting the whole file.
pub fn build_chunk(transfer_id: u64, data: &[u8], index: u32) -> Option<Vec<u8>> {
    let start = index as usize * CHUNK_SIZE;
    let chunk = data.get(start..(start + CHUNK_SIZE).min(data.len()))?;
    if chunk.is_empty() {
        return None;
    }
    Some(encode_chunk(transfer_id, index, chunk))
}

/// Frames one chunk's bytes for the wire, given the bytes themselves.
///
/// The streaming counterpart to [`build_chunk`]: a sender reading a file
/// from disk has chunk `index` in hand and nothing else, and should not
/// have to load the whole file to get it framed.
pub fn encode_chunk(transfer_id: u64, index: u32, chunk: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(SUB_HEADER_LEN + chunk.len());
    out.extend_from_slice(&transfer_id.to_be_bytes());
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&chunk_checksum(chunk).to_be_bytes());
    out.extend_from_slice(chunk);
    out
}

/// How long chunk `index` of a `file_len`-byte file is, or `None` if the
/// file has no such chunk.
///
/// The receiver checks every arriving chunk against this before writing
/// it. The sender chose the bytes, and a chunk that is the wrong length
/// for its position would otherwise write past the end of the file or
/// leave a hole in it -- either way producing a file that fails its hash
/// with nothing to say why.
pub fn chunk_len(file_len: u64, index: u32) -> Option<usize> {
    let start = index as u64 * CHUNK_SIZE as u64;
    if start >= file_len {
        return None;
    }
    Some((file_len - start).min(CHUNK_SIZE as u64) as usize)
}

/// SHA-256 of everything `reader` yields, as hex.
///
/// Streams in 1 MB reads, so hashing a file costs a buffer rather than the
/// file. Both ends need this: the sender hashes what it is about to offer,
/// and the receiver verifies what landed on disk.
pub fn sha256_hex_reader(mut reader: impl std::io::Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// The other half of `build_chunks`/`build_chunk`: pulls `(transfer_id,
/// chunk_index, chunk_bytes)` back out of a decrypted `File` frame's
/// plaintext (see [`crate::Received::File`]).
/// A chunk that fails its own checksum is rejected here rather than
/// stored, so it is re-requested like any other missing chunk instead of
/// poisoning the file and failing at the end.
pub fn parse_chunk(plaintext: &[u8]) -> Option<(u64, u32, &[u8])> {
    if plaintext.len() < SUB_HEADER_LEN {
        return None;
    }
    let transfer_id = u64::from_be_bytes(plaintext[0..8].try_into().ok()?);
    let chunk_index = u32::from_be_bytes(plaintext[8..12].try_into().ok()?);
    let expected = u64::from_be_bytes(plaintext[12..20].try_into().ok()?);
    let body = &plaintext[SUB_HEADER_LEN..];
    if chunk_checksum(body) != expected {
        return None;
    }
    Some((transfer_id, chunk_index, body))
}

/// Collects chunks (which can arrive out of order, duplicated, or not at
/// all) for one incoming transfer, and reassembles + hash-verifies once
/// every chunk has shown up.
pub struct FileReassembler {
    expected_len: u64,
    expected_sha256: String,
    chunks: Vec<Option<Vec<u8>>>,
    received_count: u32,
}

impl FileReassembler {
    pub fn new(total_chunks: u32, expected_len: u64, expected_sha256: String) -> Self {
        Self { expected_len, expected_sha256, chunks: vec![None; total_chunks as usize], received_count: 0 }
    }

    /// Records one chunk. Returns `true` if this was a new chunk (not a
    /// duplicate, e.g. from a retransmit racing the original, or an
    /// out-of-range index) -- callers can use that to decide whether
    /// progress actually advanced.
    pub fn receive_chunk(&mut self, index: u32, bytes: &[u8]) -> bool {
        let Some(slot) = self.chunks.get_mut(index as usize) else { return false };
        if slot.is_some() {
            return false;
        }
        *slot = Some(bytes.to_vec());
        self.received_count += 1;
        true
    }

    pub fn is_complete(&self) -> bool {
        self.received_count as usize == self.chunks.len()
    }

    /// 0..1, for a progress bar.
    pub fn progress(&self) -> f32 {
        if self.chunks.is_empty() {
            return 1.0;
        }
        self.received_count as f32 / self.chunks.len() as f32
    }

    /// The chunk indices still missing -- what a receiver asks the sender
    /// to resend once the initial blast of chunks goes quiet.
    /// The chunks received so far, laid out at their real offsets in a
    /// buffer the size of the finished file, along with which indices are
    /// present.
    ///
    /// For persisting an interrupted transfer: a resume needs both the
    /// bytes and the map of what those bytes actually cover, and deriving
    /// one from the other afterwards is not possible.
    pub fn partial_snapshot(&self, chunk_size: usize) -> (Vec<u8>, Vec<u32>) {
        let mut data = vec![0u8; self.expected_len as usize];
        let mut have = Vec::new();
        for (index, slot) in self.chunks.iter().enumerate() {
            let Some(bytes) = slot else { continue };
            let start = index * chunk_size;
            let end = (start + bytes.len()).min(data.len());
            if start < end {
                data[start..end].copy_from_slice(&bytes[..end - start]);
                have.push(index as u32);
            }
        }
        (data, have)
    }

    pub fn missing_chunks(&self) -> Vec<u32> {
        self.chunks.iter().enumerate().filter(|(_, c)| c.is_none()).map(|(i, _)| i as u32).collect()
    }

    /// Concatenates every chunk and verifies the result against the
    /// expected length and SHA-256. `None` if incomplete, or if what
    /// arrived doesn't actually match -- corruption or a truncated
    /// transfer shouldn't silently produce a wrong file on disk.
    pub fn finish(self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }
        let mut data = Vec::with_capacity(self.expected_len as usize);
        for chunk in self.chunks {
            data.extend_from_slice(&chunk?);
        }
        if data.len() as u64 != self.expected_len || sha256_hex(&data) != self.expected_sha256 {
            return None;
        }
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_one_chunk_matches_slicing_it_from_the_whole_file() {
        // The streaming sender and the in-memory one must put identical
        // bytes on the wire, or a receiver could not tell which it had.
        let data: Vec<u8> = (0..CHUNK_SIZE * 2 + 5).map(|i| (i % 253) as u8).collect();
        for index in 0..3u32 {
            let start = index as usize * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE).min(data.len());
            assert_eq!(
                encode_chunk(9, index, &data[start..end]),
                build_chunk(9, &data, index).unwrap(),
                "chunk {index}"
            );
        }
    }

    #[test]
    fn chunk_lengths_cover_the_file_exactly() {
        let len = CHUNK_SIZE as u64 * 2 + 5;
        assert_eq!(chunk_len(len, 0), Some(CHUNK_SIZE));
        assert_eq!(chunk_len(len, 1), Some(CHUNK_SIZE));
        assert_eq!(chunk_len(len, 2), Some(5), "the last chunk is the remainder");
        assert_eq!(chunk_len(len, 3), None, "there is no fourth chunk");
        assert_eq!(chunk_len(0, 0), None, "an empty file has no chunks");
        let total: u64 = (0..chunk_count(len)).map(|i| chunk_len(len, i).unwrap() as u64).sum();
        assert_eq!(total, len);
    }

    #[test]
    fn a_streamed_hash_matches_hashing_it_all_at_once() {
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(sha256_hex_reader(&data[..]).unwrap(), sha256_hex(&data));
        assert_eq!(sha256_hex_reader(&[][..]).unwrap(), sha256_hex(&[]));
    }

    #[test]
    fn a_damaged_chunk_is_rejected_rather_than_stored() {
        // The point of the per-chunk checksum. Without it a single wrong
        // byte fails the whole-file hash at the very end, after
        // everything has been transferred, and nothing says which chunk
        // was wrong -- so the only recovery is to send the file again.
        let mut chunk = build_chunk(7, b"the quick brown fox", 0).expect("a chunk");
        let last = chunk.len() - 1;
        chunk[last] ^= 0xff;
        assert!(parse_chunk(&chunk).is_none());
    }

    #[test]
    fn damage_anywhere_in_the_body_is_caught() {
        let original = vec![7u8; CHUNK_SIZE];
        for position in [0, 1, CHUNK_SIZE / 2, CHUNK_SIZE - 1] {
            let mut chunk = build_chunk(1, &original, 0).expect("a chunk");
            chunk[SUB_HEADER_LEN + position] ^= 0x01;
            assert!(parse_chunk(&chunk).is_none(), "byte {position} slipped through");
        }
    }

    #[test]
    fn a_damaged_chunk_is_simply_missing_and_gets_re_requested() {
        // Rejected, not stored: the reassembler must still list it as
        // missing so the normal repair path asks for it again.
        let data = vec![3u8; CHUNK_SIZE * 2];
        let mut reassembler =
            FileReassembler::new(chunk_count(data.len() as u64), data.len() as u64, sha256_hex(&data));
        let chunks = build_chunks(1, &data);
        let (_, index, body) = parse_chunk(&chunks[0]).expect("chunk 0 is intact");
        reassembler.receive_chunk(index, body);

        let mut damaged = chunks[1].clone();
        damaged[SUB_HEADER_LEN] ^= 0xff;
        assert!(parse_chunk(&damaged).is_none());
        assert_eq!(reassembler.missing_chunks(), vec![1]);
    }

    #[test]
    fn an_intact_chunk_still_round_trips() {
        let body = b"hello there";
        let chunk = build_chunk(42, body, 0).expect("a chunk");
        assert_eq!(parse_chunk(&chunk), Some((42, 0, &body[..])));
    }

    #[test]
    fn a_truncated_chunk_is_refused_rather_than_indexed_into() {
        // The header grew; a chunk shorter than it must not be read past
        // its end.
        let chunk = build_chunk(1, b"data", 0).expect("a chunk");
        for length in 0..SUB_HEADER_LEN {
            assert!(parse_chunk(&chunk[..length]).is_none(), "length {length} was accepted");
        }
    }

    #[test]
    fn a_chunk_relabelled_with_another_index_still_verifies() {
        // Worth being explicit: the checksum covers the body, not the
        // header, so it catches damage and not misrouting. Misrouting is
        // not a threat here -- the whole chunk arrives inside the
        // session AEAD, and the whole-file hash catches the result.
        let mut chunk = build_chunk(1, b"payload", 0).expect("a chunk");
        chunk[8..12].copy_from_slice(&9u32.to_be_bytes());
        assert_eq!(parse_chunk(&chunk).map(|(_, i, _)| i), Some(9));
    }

    #[test]
    fn chunk_count_rounds_up() {
        assert_eq!(chunk_count(0), 0);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64), 1);
        assert_eq!(chunk_count(CHUNK_SIZE as u64 + 1), 2);
    }

    #[test]
    fn build_and_parse_chunk_round_trips() {
        let data = vec![7u8; CHUNK_SIZE * 2 + 100];
        let chunks = build_chunks(42, &data);
        assert_eq!(chunks.len(), 3);
        for (i, chunk) in chunks.iter().enumerate() {
            let (transfer_id, index, bytes) = parse_chunk(chunk).unwrap();
            assert_eq!(transfer_id, 42);
            assert_eq!(index, i as u32);
            assert_eq!(bytes, &data[i * CHUNK_SIZE..(i * CHUNK_SIZE + bytes.len())]);
        }
    }

    #[test]
    fn build_chunk_matches_the_same_slice_build_chunks_would_produce() {
        let data = vec![9u8; CHUNK_SIZE * 2 + 100];
        let all = build_chunks(1, &data);
        let just_one = build_chunk(1, &data, 1).unwrap();
        assert_eq!(all[1], just_one);
        assert!(build_chunk(1, &data, 99).is_none(), "out-of-range index should return None, not panic/garbage");
    }

    #[test]
    fn reassembler_happy_path_in_order() {
        let data = b"hello file transfer world".repeat(200);
        let chunks = build_chunks(1, &data);
        let mut r = FileReassembler::new(chunks.len() as u32, data.len() as u64, sha256_hex(&data));
        for chunk in &chunks {
            let (_, index, bytes) = parse_chunk(chunk).unwrap();
            assert!(r.receive_chunk(index, bytes));
        }
        assert!(r.is_complete());
        assert_eq!(r.finish().unwrap(), data);
    }

    #[test]
    fn reassembler_handles_out_of_order_and_duplicate_chunks() {
        let data = b"out of order data".repeat(500);
        let chunks = build_chunks(1, &data);
        let mut r = FileReassembler::new(chunks.len() as u32, data.len() as u64, sha256_hex(&data));

        // Feed them in reverse, and once more for good measure.
        for chunk in chunks.iter().rev() {
            let (_, index, bytes) = parse_chunk(chunk).unwrap();
            r.receive_chunk(index, bytes);
        }
        assert!(!r.receive_chunk(0, &[]), "a duplicate of an already-received chunk shouldn't count as new");
        assert!(r.is_complete());
        assert_eq!(r.finish().unwrap(), data);
    }

    #[test]
    fn a_partial_snapshot_places_chunks_at_their_real_offsets() {
        let data = vec![7u8; CHUNK_SIZE + 10];
        let sha = sha256_hex(&data);
        let mut r = FileReassembler::new(2, data.len() as u64, sha);

        // Only the *second* chunk arrives, so the snapshot must leave a
        // hole at the front rather than sliding it to the start.
        r.receive_chunk(1, &data[CHUNK_SIZE..]);
        let (buf, have) = r.partial_snapshot(CHUNK_SIZE);

        assert_eq!(buf.len(), data.len());
        assert_eq!(have, vec![1]);
        assert!(buf[..CHUNK_SIZE].iter().all(|b| *b == 0), "gap was not left empty");
        assert_eq!(&buf[CHUNK_SIZE..], &data[CHUNK_SIZE..]);
    }

    #[test]
    fn a_snapshot_of_everything_reconstructs_the_file() {
        let data: Vec<u8> = (0..CHUNK_SIZE * 2 + 5).map(|i| (i % 251) as u8).collect();
        let sha = sha256_hex(&data);
        let mut r = FileReassembler::new(3, data.len() as u64, sha);
        for (i, chunk) in data.chunks(CHUNK_SIZE).enumerate() {
            r.receive_chunk(i as u32, chunk);
        }

        let (buf, have) = r.partial_snapshot(CHUNK_SIZE);
        assert_eq!(buf, data);
        assert_eq!(have, vec![0, 1, 2]);
    }

    #[test]
    fn reassembler_reports_missing_chunks_and_progress() {
        let data = vec![1u8; CHUNK_SIZE * 4];
        let chunks = build_chunks(1, &data);
        let mut r = FileReassembler::new(chunks.len() as u32, data.len() as u64, sha256_hex(&data));
        let (_, _, bytes0) = parse_chunk(&chunks[0]).unwrap();
        let (_, _, bytes2) = parse_chunk(&chunks[2]).unwrap();
        r.receive_chunk(0, bytes0);
        r.receive_chunk(2, bytes2);

        assert_eq!(r.missing_chunks(), vec![1, 3]);
        assert_eq!(r.progress(), 0.5);
        assert!(!r.is_complete());
        assert!(r.finish().is_none(), "finish() on an incomplete transfer must not return partial/wrong data");
    }

    #[test]
    fn reassembler_rejects_a_hash_mismatch() {
        let data = b"the real file".to_vec();
        let wrong_hash = sha256_hex(b"a different file entirely");
        let chunks = build_chunks(1, &data);
        let mut r = FileReassembler::new(chunks.len() as u32, data.len() as u64, wrong_hash);
        for chunk in &chunks {
            let (_, index, bytes) = parse_chunk(chunk).unwrap();
            r.receive_chunk(index, bytes);
        }
        assert!(r.is_complete());
        assert!(r.finish().is_none(), "a complete-but-corrupted transfer must fail hash verification, not produce a silently-wrong file");
    }

    #[test]
    fn parse_chunk_rejects_undersized_input() {
        assert!(parse_chunk(b"short").is_none());
    }
}
