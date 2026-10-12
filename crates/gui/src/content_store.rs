//! Content-addressed file storage and resumable transfers (spec §5).
//!
//! > "Files are identified by a hash of their contents, so a file you
//! > already have is never downloaded twice."
//!
//! Two things live here, and they are the same idea applied twice:
//!
//! - **Completed files** are stored under their SHA-256. An offer whose
//!   hash is already present needs no transfer at all -- the receiver
//!   already has those exact bytes, by definition, because the hash *is*
//!   the identity of the content.
//! - **Transfers in progress** are written straight to a file under the
//!   same hash, chunk by chunk at each chunk's own offset. That file is the
//!   reassembly buffer, so nothing is ever held whole in memory, and it is
//!   also the resume state: a dropped connection picks up from whatever is
//!   already on disk, which is spec §5's "transfers resume after
//!   disconnect" without a separate mechanism to keep in step.
//!
//! Addressing by content rather than by name is what makes both work. A
//! filename is a claim; a hash is a fact. Two people sending you the same
//! document under different names are one entry here, and a file renamed
//! mid-transfer is still the same transfer.
//!
//! Storage is deliberately plain files rather than a database: the whole
//! store is inspectable with a file manager, deleting an entry is deleting
//! a file, and there is no index to corrupt or migrate.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use voip_net::file_transfer::{chunk_count, chunk_len, sha256_hex_reader, CHUNK_SIZE};

/// Where completed, hash-named files live.
pub fn store_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("files")
}

/// Where in-progress transfers live.
pub fn partial_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("partial")
}

/// Rejects anything that is not a plain SHA-256 hex string.
///
/// These become filenames, and the hash arrives from a peer, so it is
/// attacker-controlled until checked. A traversal here would let a sender
/// choose where bytes land on the receiver's disk.
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit())
}

fn stored_path(config_dir: &Path, hash: &str) -> Option<PathBuf> {
    valid_hash(hash).then(|| store_dir(config_dir).join(format!("{}.bin", hash.to_lowercase())))
}

fn partial_data_path(config_dir: &Path, hash: &str) -> Option<PathBuf> {
    valid_hash(hash).then(|| partial_dir(config_dir).join(format!("{}.part", hash.to_lowercase())))
}

fn partial_index_path(config_dir: &Path, hash: &str) -> Option<PathBuf> {
    valid_hash(hash).then(|| partial_dir(config_dir).join(format!("{}.idx", hash.to_lowercase())))
}

/// Whether the completed file for `hash` is already held.
///
/// A cheap existence check, for deciding what to do with an offer. It
/// does not verify the contents -- see [`verified_path`], which does, and
/// which is what anything handing the file to the user must go through.
pub fn has(config_dir: &Path, hash: &str) -> bool {
    stored_path(config_dir, hash).is_some_and(|p| p.exists())
}

/// The stored file for `hash`, re-verified against the hash.
///
/// Re-verifies rather than trusting the filename: a file edited or
/// truncated on disk would otherwise be served up as though it were the
/// content its name claims. Streams, so verifying a large file costs a
/// buffer rather than the file -- but it does read all of it, so callers
/// keep it off the audio path.
pub fn verified_path(config_dir: &Path, hash: &str) -> Option<PathBuf> {
    let path = stored_path(config_dir, hash)?;
    let file = File::open(&path).ok()?;
    let actual = sha256_hex_reader(std::io::BufReader::new(file)).ok()?;
    (actual == hash.to_lowercase()).then_some(path)
}

/// Stores `bytes` under their own hash, returning it.
///
/// Idempotent by construction: storing the same content twice writes the
/// same path, so there is no duplicate to reconcile. Only for content that
/// is already in memory anyway; a received transfer is promoted from its
/// partial file by [`DiskReassembly::finish`] instead.
#[cfg(test)]
pub fn put(config_dir: &Path, bytes: &[u8]) -> std::io::Result<String> {
    let hash = voip_net::file_transfer::sha256_hex(bytes);
    let dir = store_dir(config_dir);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(format!("{hash}.bin")), bytes)?;
    Ok(hash)
}

/// Removes an interrupted transfer, after it completes or is abandoned.
pub fn clear_partial(config_dir: &Path, hash: &str) -> std::io::Result<()> {
    for path in [partial_data_path(config_dir, hash), partial_index_path(config_dir, hash)]
        .into_iter()
        .flatten()
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// How many chunk writes between saving the index.
///
/// The index records which chunks are on disk. Saving it after every chunk
/// would be a file rewrite per 3.8 KB received; saving it only at the end
/// would lose the whole resume state to a crash. Every 256 chunks -- about
/// a megabyte -- bounds what a crash can cost to that much re-fetching.
const PERSIST_EVERY: u32 = 256;

/// The most chunks one repair request may name.
///
/// A request travels as one control datagram, and datagrams here are
/// capped at 4 KB. Naming every missing chunk at once used to mean that a
/// badly damaged transfer produced a request too large to send -- so the
/// repair that was most needed was the one that silently never happened.
/// 256 indices is under 2 KB of JSON; the next round asks for the rest.
pub const MAX_REQUEST: usize = 256;

/// How long without a new chunk before the sender is assumed to have
/// finished its first pass, so the chunks never seen become worth asking
/// for too.
const QUIET_BEFORE_TAIL_REQUEST: std::time::Duration = std::time::Duration::from_secs(2);

/// What is saved alongside a partial file.
///
/// Received chunks are stored as ranges, not as a list: chunks arrive
/// mostly in order, so a gigabyte-sized transfer is a handful of ranges
/// rather than a quarter of a million numbers rewritten every megabyte.
/// `have` is the older list form, still read so a partial saved by an
/// earlier build resumes rather than restarting.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Index {
    file_name: String,
    file_size: u64,
    chunk_count: u32,
    #[serde(default)]
    have_ranges: Vec<(u32, u32)>,
    #[serde(default)]
    have: Vec<u32>,
}

/// An incoming transfer, written to disk as it arrives.
pub struct DiskReassembly {
    config_dir: PathBuf,
    hash: String,
    file_name: String,
    file_size: u64,
    chunk_count: u32,
    /// `None` only once `finish` has closed it to rename it.
    file: Option<File>,
    /// Which chunks are on disk, one bit each.
    have: Vec<u64>,
    received: u32,
    /// Chunks already on disk when this was opened -- a resume.
    resumed: u32,
    since_persist: u32,
    highest_seen: Option<u32>,
    last_chunk_at: Instant,
}

impl DiskReassembly {
    /// Opens the partial file for `hash`, adopting what is already there.
    ///
    /// A leftover partial is adopted only if its index agrees with this
    /// offer about the file's size, and only for chunks that fit inside
    /// the data actually on disk. Anything else is discarded rather than
    /// repaired: restarting is cheap, and resuming from a state that might
    /// be wrong risks a file that fails its hash at the end with no
    /// explanation.
    pub fn open(
        config_dir: &Path,
        hash: &str,
        file_name: &str,
        file_size: u64,
    ) -> std::io::Result<Self> {
        let (Some(data_path), Some(index_path)) =
            (partial_data_path(config_dir, hash), partial_index_path(config_dir, hash))
        else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to store a transfer under a malformed hash",
            ));
        };
        std::fs::create_dir_all(partial_dir(config_dir))?;
        let total = chunk_count(file_size);

        let on_disk = std::fs::metadata(&data_path).map(|m| m.len()).unwrap_or(0);
        let adopted: Vec<u32> = std::fs::read(&index_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
            .filter(|index| index.file_size == file_size && index.chunk_count == total)
            .filter(|_| on_disk <= file_size)
            .map(|index| {
                let mut chunks: Vec<u32> = index.have;
                for (start, end) in index.have_ranges {
                    chunks.extend(start..=end.min(total.saturating_sub(1)));
                }
                chunks
            })
            .unwrap_or_default();

        let adopted: Vec<u32> = adopted
            .into_iter()
            .filter(|&i| {
                // Only chunks whose bytes are genuinely within the file on
                // disk -- an index can claim anything, the file cannot.
                chunk_len(file_size, i)
                    .is_some_and(|len| i as u64 * CHUNK_SIZE as u64 + len as u64 <= on_disk)
            })
            .collect();

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(adopted.is_empty())
            .open(&data_path)?;

        let mut this = Self {
            config_dir: config_dir.to_path_buf(),
            hash: hash.to_lowercase(),
            file_name: file_name.to_string(),
            file_size,
            chunk_count: total,
            file: Some(file),
            have: vec![0u64; (total as usize).div_ceil(64)],
            received: 0,
            resumed: 0,
            since_persist: 0,
            highest_seen: None,
            last_chunk_at: Instant::now(),
        };
        for index in adopted {
            if this.mark(index) {
                this.resumed += 1;
            }
        }
        if this.resumed == 0 {
            // A fresh start leaves no stale index behind claiming chunks
            // from a different attempt.
            let _ = std::fs::remove_file(&index_path);
        }
        Ok(this)
    }

    fn has_chunk(&self, index: u32) -> bool {
        self.have
            .get(index as usize / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    /// Records chunk `index` as present. Returns whether it was new.
    fn mark(&mut self, index: u32) -> bool {
        if index >= self.chunk_count || self.has_chunk(index) {
            return false;
        }
        self.have[index as usize / 64] |= 1 << (index % 64);
        self.received += 1;
        true
    }

    /// Writes one chunk at its own offset. Returns whether it was new.
    ///
    /// A chunk of the wrong length for its position is refused rather than
    /// written. The sender chose those bytes, and a wrong length would
    /// either write past the end of the file or leave a hole in it.
    pub fn write_chunk(&mut self, index: u32, bytes: &[u8]) -> std::io::Result<bool> {
        if chunk_len(self.file_size, index) != Some(bytes.len()) || self.has_chunk(index) {
            return Ok(false);
        }
        let Some(file) = self.file.as_mut() else { return Ok(false) };
        file.seek(SeekFrom::Start(index as u64 * CHUNK_SIZE as u64))?;
        file.write_all(bytes)?;
        self.mark(index);
        self.highest_seen = Some(self.highest_seen.map_or(index, |h| h.max(index)));
        self.last_chunk_at = Instant::now();
        self.since_persist += 1;
        if self.since_persist >= PERSIST_EVERY {
            self.persist()?;
        }
        Ok(true)
    }

    pub fn is_complete(&self) -> bool {
        self.received >= self.chunk_count
    }

    pub fn received(&self) -> u32 {
        self.received
    }

    /// How many chunks were already on disk from an earlier attempt.
    pub fn resumed(&self) -> u32 {
        self.resumed
    }

    /// Chunks worth asking the sender for again, at most [`MAX_REQUEST`].
    ///
    /// While chunks are still arriving, only *gaps* are requested -- chunks
    /// below the highest one seen, which the sender has definitely already
    /// sent. The tail it simply has not reached yet is left alone: asking
    /// for it would have the sender send everything twice. Once nothing has
    /// arrived for a couple of seconds the first pass is over, and the
    /// tail is fair game too.
    pub fn chunks_to_request(&self) -> Vec<u32> {
        let quiet = self.last_chunk_at.elapsed() >= QUIET_BEFORE_TAIL_REQUEST;
        let limit = if quiet {
            self.chunk_count
        } else {
            self.highest_seen.unwrap_or(0)
        };
        (0..limit).filter(|&i| !self.has_chunk(i)).take(MAX_REQUEST).collect()
    }

    /// Saves which chunks are on disk, so an interruption loses nothing.
    ///
    /// The data is flushed to disk *before* the index is written. The
    /// other order could leave an index claiming a chunk whose bytes never
    /// made it, which would surface only as a failed hash at the very end.
    pub fn persist(&mut self) -> std::io::Result<()> {
        self.since_persist = 0;
        let Some(file) = self.file.as_mut() else { return Ok(()) };
        file.flush()?;
        file.sync_data()?;

        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for index in (0..self.chunk_count).filter(|&i| self.has_chunk(i)) {
            match ranges.last_mut() {
                Some((_, end)) if *end + 1 == index => *end = index,
                _ => ranges.push((index, index)),
            }
        }
        let index = Index {
            file_name: self.file_name.clone(),
            file_size: self.file_size,
            chunk_count: self.chunk_count,
            have_ranges: ranges,
            have: Vec::new(),
        };
        let path = partial_index_path(&self.config_dir, &self.hash).expect("hash was validated");
        let json = serde_json::to_vec(&index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Throws the partial away -- for a transfer the user cancelled.
    pub fn discard(mut self) {
        self.file = None;
        let _ = clear_partial(&self.config_dir, &self.hash);
    }

    /// Verifies the finished file against its hash and moves it into the
    /// store.
    ///
    /// `Ok(Some(path))` is the verified file; `Ok(None)` means every chunk
    /// arrived but the result does not match the hash the sender promised,
    /// and the partial has been deleted -- resuming from bytes known to be
    /// wrong would only fail again. Reads the whole file, so callers run it
    /// off the audio path.
    pub fn finish(mut self) -> std::io::Result<Option<PathBuf>> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
            // Closed before the rename: Windows will not move a file that
            // is still open.
            drop(file);
        }
        let data_path = partial_data_path(&self.config_dir, &self.hash).expect("hash was validated");
        let actual = sha256_hex_reader(std::io::BufReader::new(File::open(&data_path)?))?;
        if actual != self.hash {
            clear_partial(&self.config_dir, &self.hash)?;
            return Ok(None);
        }

        let target = stored_path(&self.config_dir, &self.hash).expect("hash was validated");
        std::fs::create_dir_all(store_dir(&self.config_dir))?;
        if target.exists() {
            // Already stored by an earlier transfer of the same content,
            // which by definition is these same bytes.
            std::fs::remove_file(&data_path)?;
        } else {
            std::fs::rename(&data_path, &target)?;
        }
        let _ = std::fs::remove_file(
            partial_index_path(&self.config_dir, &self.hash).expect("hash was validated"),
        );
        Ok(Some(target))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use voip_net::file_transfer::{build_chunks, parse_chunk, sha256_hex};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("voip-cas-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Feeds every chunk of `data` whose index `keep` accepts.
    fn feed(r: &mut DiskReassembly, data: &[u8], keep: impl Fn(u32) -> bool) {
        for chunk in build_chunks(1, data) {
            let (_, index, bytes) = parse_chunk(&chunk).unwrap();
            if keep(index) {
                r.write_chunk(index, bytes).unwrap();
            }
        }
    }

    #[test]
    fn a_stored_file_comes_back_verified() {
        let dir = temp_dir("roundtrip");
        let hash = put(&dir, b"the file contents").unwrap();

        assert!(has(&dir, &hash));
        let path = verified_path(&dir, &hash).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"the file contents");
    }

    #[test]
    fn the_same_content_stores_to_the_same_address_twice() {
        let dir = temp_dir("idempotent");
        let first = put(&dir, b"identical bytes").unwrap();
        let second = put(&dir, b"identical bytes").unwrap();

        // The property the whole feature rests on: content decides the
        // address, so a file you already have is already here.
        assert_eq!(first, second);
        let stored: Vec<_> = std::fs::read_dir(store_dir(&dir)).unwrap().collect();
        assert_eq!(stored.len(), 1, "same content produced two entries");
    }

    #[test]
    fn an_unknown_hash_is_simply_absent() {
        let dir = temp_dir("absent");
        let hash = "ab".repeat(32);
        assert!(!has(&dir, &hash));
        assert!(verified_path(&dir, &hash).is_none());
    }

    #[test]
    fn a_file_tampered_with_on_disk_is_not_served() {
        let dir = temp_dir("tampered");
        let hash = put(&dir, b"original contents").unwrap();
        std::fs::write(stored_path(&dir, &hash).unwrap(), b"something else entirely").unwrap();

        // The name claims one thing, the bytes say another. Trusting the
        // name would hand back content that is not what was asked for.
        assert!(has(&dir, &hash), "the cheap check only looks at the name");
        assert!(verified_path(&dir, &hash).is_none());
    }

    #[test]
    fn a_malformed_hash_cannot_escape_the_store_directory() {
        let dir = temp_dir("traversal");
        // A peer chooses this string, so it must never become a path.
        for bad in ["../../etc/passwd", "..", "", "zzzz", &"ab".repeat(40)] {
            assert!(stored_path(&dir, bad).is_none(), "{bad:?} was accepted");
            assert!(!has(&dir, bad));
            assert!(verified_path(&dir, bad).is_none());
            assert!(DiskReassembly::open(&dir, bad, "x", 10).is_err());
        }
    }

    #[test]
    fn a_whole_transfer_lands_in_the_store_byte_for_byte() {
        let dir = temp_dir("whole");
        let data = sample(CHUNK_SIZE * 3 + 17);
        let hash = sha256_hex(&data);

        let mut r = DiskReassembly::open(&dir, &hash, "big.bin", data.len() as u64).unwrap();
        feed(&mut r, &data, |_| true);
        assert!(r.is_complete());

        let path = r.finish().unwrap().expect("the hash matches");
        assert_eq!(std::fs::read(&path).unwrap(), data);
        assert!(has(&dir, &hash));
        assert!(!partial_data_path(&dir, &hash).unwrap().exists(), "the partial was left behind");
    }

    #[test]
    fn out_of_order_chunks_still_assemble_correctly() {
        // UDP reorders. Writing at each chunk's own offset is what makes
        // arrival order irrelevant.
        let dir = temp_dir("order");
        let data = sample(CHUNK_SIZE * 4 + 3);
        let hash = sha256_hex(&data);
        let mut chunks = build_chunks(1, &data);
        chunks.reverse();

        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        for chunk in chunks {
            let (_, index, bytes) = parse_chunk(&chunk).unwrap();
            r.write_chunk(index, bytes).unwrap();
        }
        assert_eq!(std::fs::read(r.finish().unwrap().unwrap()).unwrap(), data);
    }

    #[test]
    fn an_interrupted_transfer_resumes_from_what_is_on_disk() {
        let dir = temp_dir("resume");
        let data = sample(CHUNK_SIZE * 3 + 17);
        let hash = sha256_hex(&data);

        // First attempt: two chunks land, then the call drops.
        let mut first = DiskReassembly::open(&dir, &hash, "big.bin", data.len() as u64).unwrap();
        feed(&mut first, &data, |i| i < 2);
        first.persist().unwrap();
        drop(first);

        // Second attempt picks those up and needs only the rest.
        let mut second = DiskReassembly::open(&dir, &hash, "big.bin", data.len() as u64).unwrap();
        assert_eq!(second.resumed(), 2, "should have carried two chunks over");
        assert_eq!(second.received(), 2);
        feed(&mut second, &data, |i| i >= 2);

        // The resumed file is byte-identical and passes its own hash.
        let path = second.finish().unwrap().expect("resumed file verifies");
        assert_eq!(std::fs::read(path).unwrap(), data);
    }

    #[test]
    fn a_partial_for_a_different_size_is_not_adopted() {
        // Same hash name, different claimed size: one of the two is lying,
        // and resuming would build a file that cannot verify.
        let dir = temp_dir("size");
        let data = sample(CHUNK_SIZE * 2);
        let hash = sha256_hex(&data);

        let mut first = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut first, &data, |i| i == 0);
        first.persist().unwrap();
        drop(first);

        let second = DiskReassembly::open(&dir, &hash, "f", data.len() as u64 + 1).unwrap();
        assert_eq!(second.resumed(), 0);
    }

    #[test]
    fn an_index_claiming_chunks_the_file_does_not_hold_is_not_believed() {
        // An index can claim anything; the file on disk cannot. Adopting a
        // claimed chunk whose bytes are not there would fail the hash at
        // the very end with nothing to say why.
        let dir = temp_dir("liar");
        let data = sample(CHUNK_SIZE * 3);
        let hash = sha256_hex(&data);
        std::fs::create_dir_all(partial_dir(&dir)).unwrap();
        std::fs::write(partial_data_path(&dir, &hash).unwrap(), &data[..CHUNK_SIZE]).unwrap();
        let index = Index {
            file_name: "f".into(),
            file_size: data.len() as u64,
            chunk_count: 3,
            have_ranges: vec![(0, 2)],
            have: vec![],
        };
        std::fs::write(partial_index_path(&dir, &hash).unwrap(), serde_json::to_vec(&index).unwrap())
            .unwrap();

        let r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        assert_eq!(r.resumed(), 1, "only the chunk actually on disk counts");
    }

    #[test]
    fn a_partial_saved_by_an_earlier_build_still_resumes() {
        // Earlier builds listed every chunk rather than ranges. Refusing
        // that format would restart transfers people were halfway through.
        let dir = temp_dir("legacy");
        let data = sample(CHUNK_SIZE * 3 + 5);
        let hash = sha256_hex(&data);
        let mut on_disk = vec![0u8; data.len()];
        on_disk[..CHUNK_SIZE].copy_from_slice(&data[..CHUNK_SIZE]);
        on_disk[CHUNK_SIZE * 2..CHUNK_SIZE * 3].copy_from_slice(&data[CHUNK_SIZE * 2..CHUNK_SIZE * 3]);
        std::fs::create_dir_all(partial_dir(&dir)).unwrap();
        std::fs::write(partial_data_path(&dir, &hash).unwrap(), &on_disk).unwrap();
        std::fs::write(
            partial_index_path(&dir, &hash).unwrap(),
            format!(r#"{{"file_name":"f","file_size":{},"chunk_count":4,"have":[0,2]}}"#, data.len()),
        )
        .unwrap();

        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        assert_eq!(r.resumed(), 2);
        feed(&mut r, &data, |i| i == 1 || i == 3);
        assert_eq!(std::fs::read(r.finish().unwrap().unwrap()).unwrap(), data);
    }

    #[test]
    fn a_chunk_of_the_wrong_length_for_its_place_is_refused() {
        let dir = temp_dir("length");
        let data = sample(CHUNK_SIZE + 10);
        let hash = sha256_hex(&data);
        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();

        assert!(!r.write_chunk(0, &data[..10]).unwrap(), "short first chunk accepted");
        assert!(!r.write_chunk(1, &data[..CHUNK_SIZE]).unwrap(), "oversized last chunk accepted");
        assert!(!r.write_chunk(5, &data[..10]).unwrap(), "chunk past the end accepted");
        assert_eq!(r.received(), 0);
    }

    #[test]
    fn a_duplicate_chunk_is_not_counted_twice() {
        // Retransmits race originals. Counting both would declare the
        // transfer complete with chunks still missing.
        let dir = temp_dir("dupe");
        let data = sample(CHUNK_SIZE * 2);
        let hash = sha256_hex(&data);
        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        assert!(r.write_chunk(0, &data[..CHUNK_SIZE]).unwrap());
        assert!(!r.write_chunk(0, &data[..CHUNK_SIZE]).unwrap());
        assert_eq!(r.received(), 1);
        assert!(!r.is_complete());
    }

    #[test]
    fn a_file_that_fails_its_hash_is_not_stored_and_not_kept() {
        let dir = temp_dir("badhash");
        let data = sample(CHUNK_SIZE + 3);
        let wrong = "00".repeat(32);
        let mut r = DiskReassembly::open(&dir, &wrong, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |_| true);

        assert!(r.finish().unwrap().is_none());
        assert!(!has(&dir, &wrong), "unverified bytes were stored under a hash they do not match");
        assert!(!partial_data_path(&dir, &wrong).unwrap().exists(), "bad bytes kept for a resume");
    }

    #[test]
    fn only_gaps_are_requested_while_chunks_are_still_arriving() {
        // The tail the sender has not reached yet is not missing, it is
        // just not sent yet. Asking for it would double the transfer.
        let dir = temp_dir("gaps");
        let data = sample(CHUNK_SIZE * 10);
        let hash = sha256_hex(&data);
        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |i| i != 2 && i <= 5);
        assert_eq!(r.chunks_to_request(), vec![2]);
    }

    #[test]
    fn a_repair_request_always_fits_in_one_datagram() {
        // Naming every missing chunk used to produce requests too large to
        // send, so the worst-damaged transfers were the ones never
        // repaired.
        let dir = temp_dir("cap");
        let data = sample(CHUNK_SIZE * 2000);
        let hash = sha256_hex(&data);
        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |i| i == 1999);
        let request = r.chunks_to_request();
        assert_eq!(request.len(), MAX_REQUEST);
        let encoded = crate::control::ControlMessage::FileMissing { transfer_id: u64::MAX, indices: request }
            .encode();
        assert!(encoded.len() < 3500, "a full request is {} bytes", encoded.len());
    }

    #[test]
    fn an_empty_file_is_complete_as_soon_as_it_is_opened() {
        let dir = temp_dir("empty");
        let hash = sha256_hex(b"");
        let r = DiskReassembly::open(&dir, &hash, "empty.txt", 0).unwrap();
        assert!(r.is_complete());
        let path = r.finish().unwrap().expect("an empty file hashes to the empty hash");
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }

    #[test]
    fn cancelling_leaves_nothing_behind() {
        let dir = temp_dir("discard");
        let data = sample(CHUNK_SIZE * 2);
        let hash = sha256_hex(&data);
        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |i| i == 0);
        r.persist().unwrap();
        r.discard();
        assert!(!partial_data_path(&dir, &hash).unwrap().exists());
        assert!(!partial_index_path(&dir, &hash).unwrap().exists());
    }

    #[test]
    fn clearing_a_partial_is_idempotent() {
        let dir = temp_dir("clear");
        let hash = "34".repeat(32);
        clear_partial(&dir, &hash).unwrap();
        clear_partial(&dir, &hash).unwrap();
    }
}
