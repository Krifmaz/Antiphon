//! Per-file encryption (spec §5: "Each file is encrypted with its own key,
//! delivered inside the encrypted message. Any peer or server can hold a
//! copy without being able to read it.")
//!
//! A file is cut into [`CHUNK`]-byte pieces and each is sealed with
//! ChaCha20-Poly1305 under a key made for that file alone, with the chunk
//! index as the nonce -- unique, because the key is never used for any
//! other file. What anyone holding a copy sees is a list of sealed chunks
//! and the hash of each.
//!
//! The file's id is the hash of its size and those chunk hashes. So a
//! seeder or a server can check every chunk it holds or hands out against
//! the id *without the key*, and a downloader pulling chunks from several
//! sources at once can tell a bad one from a good one the moment it
//! arrives. The key travels only inside the end-to-end encrypted message
//! that shares the file.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::error::CryptoError;

/// Plaintext bytes per chunk. Small enough that a sealed chunk and its
/// header fit one datagram on any path.
pub const CHUNK: usize = 1024;
/// Bytes a sealed chunk adds.
pub const TAG: usize = 16;

pub type FileKey = [u8; 32];
pub type FileId = [u8; 32];
pub type ChunkHash = [u8; 32];

pub fn new_key() -> FileKey {
    let mut key = [0u8; 32];
    getrandom::fill(&mut key).expect("OS randomness is unavailable");
    key
}

/// How many chunks a file of `size` bytes has. An empty file has one,
/// empty, chunk, so it still has an id.
pub fn chunk_count(size: u64) -> u32 {
    size.div_ceil(CHUNK as u64).max(1) as u32
}

fn cipher(key: &FileKey) -> ChaCha20Poly1305 {
    ChaCha20Poly1305::new(&Key::from(*key))
}

fn nonce(index: u32) -> Nonce {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&index.to_le_bytes());
    Nonce::from(n)
}

pub fn seal_chunk(key: &FileKey, index: u32, plain: &[u8]) -> Vec<u8> {
    cipher(key).encrypt(&nonce(index), plain).expect("sealing a chunk cannot fail")
}

pub fn open_chunk(key: &FileKey, index: u32, sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    cipher(key).decrypt(&nonce(index), sealed).map_err(|_| CryptoError::DecryptionFailed)
}

pub fn chunk_hash(sealed: &[u8]) -> ChunkHash {
    Sha256::digest(sealed).into()
}

/// The file's id: its size and the hash of every sealed chunk, hashed.
pub fn file_id(size: u64, hashes: &[ChunkHash]) -> FileId {
    let mut h = Sha256::new_with_prefix(b"antiphon/file/v1");
    h.update(size.to_be_bytes());
    h.update((hashes.len() as u32).to_be_bytes());
    for c in hashes {
        h.update(c);
    }
    h.finalize().into()
}

/// What a holder must show to be handed a file's chunks when it is
/// shared with "this conversation" only: derived from the key, so anyone
/// the file was shared with can make it and nobody else can.
pub fn read_token(key: &FileKey) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(b"antiphon/file/read/v1"), key)
        .expand(b"read", &mut out)
        .expect("32 bytes is a valid HKDF output length");
    out
}

/// Seals a whole file held in memory. For tests and small files; the app
/// streams instead.
pub fn seal_all(key: &FileKey, plain: &[u8]) -> (FileId, Vec<Vec<u8>>) {
    let chunks: Vec<Vec<u8>> = if plain.is_empty() {
        vec![seal_chunk(key, 0, &[])]
    } else {
        plain.chunks(CHUNK).enumerate().map(|(i, c)| seal_chunk(key, i as u32, c)).collect()
    };
    let hashes: Vec<ChunkHash> = chunks.iter().map(|c| chunk_hash(c)).collect();
    (file_id(plain.len() as u64, &hashes), chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_round_trips_chunk_by_chunk() {
        let key = new_key();
        let plain: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let (id, chunks) = seal_all(&key, &plain);
        assert_eq!(chunks.len() as u32, chunk_count(plain.len() as u64));
        let mut back = Vec::new();
        for (i, c) in chunks.iter().enumerate() {
            back.extend(open_chunk(&key, i as u32, c).unwrap());
        }
        assert_eq!(back, plain);
        let hashes: Vec<_> = chunks.iter().map(|c| chunk_hash(c)).collect();
        assert_eq!(file_id(plain.len() as u64, &hashes), id);
    }

    #[test]
    fn a_holder_without_the_key_reads_nothing() {
        let plain = b"the minutes of the meeting".repeat(50);
        let (_, chunks) = seal_all(&new_key(), &plain);
        assert!(open_chunk(&new_key(), 0, &chunks[0]).is_err());
        assert!(!chunks[0].windows(10).any(|w| w == &plain[..10]));
    }

    #[test]
    fn a_chunk_cannot_be_moved_to_another_place() {
        let key = new_key();
        let (_, chunks) = seal_all(&key, &[7u8; 3000]);
        assert!(open_chunk(&key, 1, &chunks[0]).is_err());
    }

    #[test]
    fn the_same_file_under_two_keys_has_two_ids() {
        let plain = [1u8; 2048];
        assert_ne!(seal_all(&new_key(), &plain).0, seal_all(&new_key(), &plain).0);
    }

    #[test]
    fn empty_files_still_have_an_id() {
        let key = new_key();
        let (_, chunks) = seal_all(&key, &[]);
        assert_eq!(chunks.len(), 1);
        assert!(open_chunk(&key, 0, &chunks[0]).unwrap().is_empty());
    }
}
