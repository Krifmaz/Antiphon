//! Re-checking files before and while offering them (spec §5: "moved or
//! deleted files -- re-verify by hash before offering; mark unavailable
//! rather than failing mid-transfer").
//!
//! The failure this prevents is specific and infuriating: you offer a
//! file, the other person accepts, the transfer runs for a while and then
//! dies -- because the file changed or vanished after you picked it. The
//! person receiving has no idea what happened, and the person sending
//! usually does not either.
//!
//! Failing *before* the offer costs nothing and explains itself. So a file
//! is opened and hashed at the moment it is picked, and anything that has
//! already gone is reported there rather than attempted.
//!
//! Files are streamed rather than loaded, so there is no size limit beyond
//! the disk. That reopens a window the in-memory version did not have: the
//! bytes sent are read later than the bytes hashed. [`FileSource`] closes
//! it by checking, on every read, that the file is still the size and age
//! it was when it was hashed -- and stopping with an explanation if not,
//! instead of sending bytes that do not match the address they were
//! offered under. Content addressing (see [`crate::content_store`]) means
//! the hash *is* the file's identity, so a mismatch would not merely fail:
//! it would put content in the receiver's store under the wrong name.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use antiphon_net::file_transfer::{chunk_len, sha256_hex_reader, CHUNK_SIZE};

/// Why a file cannot be offered.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Unavailable {
    /// It is not there any more.
    Missing,
    /// It is there and unreadable -- permissions, a lock, a dead network
    /// drive.
    Unreadable(String),
    /// It changed while it was being read, so there is no single version
    /// of it to offer.
    Changing,
}

impl Unavailable {
    /// What to tell the user, in terms of what happened rather than which
    /// check failed.
    pub fn explain(&self, file_name: &str) -> String {
        match self {
            Unavailable::Missing => {
                format!("{file_name} isn't there any more -- it was moved or deleted.")
            }
            Unavailable::Unreadable(why) => {
                format!("{file_name} couldn't be read: {why}")
            }
            Unavailable::Changing => format!(
                "{file_name} changed while it was being read -- something is still writing \
                 to it. Wait for that to finish, then send it again."
            ),
        }
    }
}

/// The bytes being sent, wherever they live.
pub enum FileSource {
    /// Already in memory -- a voice message, which is recorded rather than
    /// picked from disk.
    Memory(Vec<u8>),
    /// A file on disk, read a chunk at a time.
    Disk {
        path: PathBuf,
        len: u64,
        /// When it was last modified as of hashing. Checked on every read
        /// alongside the length.
        modified: Option<SystemTime>,
        file: Mutex<File>,
    },
}

impl FileSource {
    pub fn len(&self) -> u64 {
        match self {
            FileSource::Memory(bytes) => bytes.len() as u64,
            FileSource::Disk { len, .. } => *len,
        }
    }

    /// Whether the file has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes of chunk `index`.
    ///
    /// For a file on disk, refuses if the file is no longer the length and
    /// modification time it had when it was hashed. Sending on regardless
    /// would deliver bytes that do not match the hash they were offered
    /// under -- which the receiver would catch, but only after the whole
    /// transfer, and with nothing to say what went wrong.
    pub fn read_chunk(&self, index: u32) -> std::io::Result<Vec<u8>> {
        let Some(want) = chunk_len(self.len(), index) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("chunk {index} is past the end of the file"),
            ));
        };
        match self {
            FileSource::Memory(bytes) => {
                let start = index as usize * CHUNK_SIZE;
                Ok(bytes[start..start + want].to_vec())
            }
            FileSource::Disk { path, len, modified, file } => {
                let mut file = file.lock().unwrap();
                let meta = file.metadata()?;
                if meta.len() != *len || meta.modified().ok() != *modified {
                    return Err(std::io::Error::other(format!(
                        "{} changed after it was offered",
                        path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
                    )));
                }
                file.seek(SeekFrom::Start(index as u64 * CHUNK_SIZE as u64))?;
                let mut buf = vec![0u8; want];
                file.read_exact(&mut buf)?;
                Ok(buf)
            }
        }
    }
}

/// Opens a file and hashes it, returning something that can send it.
///
/// Hashes by streaming, so this costs a buffer rather than the file. The
/// length and modification time are taken before and after hashing; if
/// they moved, the file was being written while it was read and there is
/// no single version of it to offer.
pub fn open_and_hash(path: &Path) -> Result<(FileSource, String), Unavailable> {
    if !path.exists() {
        return Err(Unavailable::Missing);
    }
    let unreadable = |e: std::io::Error| Unavailable::Unreadable(e.to_string());
    let mut file = File::open(path).map_err(unreadable)?;
    let before = file.metadata().map_err(unreadable)?;
    let hash = sha256_hex_reader(std::io::BufReader::new(&mut file)).map_err(unreadable)?;
    let after = file.metadata().map_err(unreadable)?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err(Unavailable::Changing);
    }
    Ok((
        FileSource::Disk {
            path: path.to_path_buf(),
            len: after.len(),
            modified: after.modified().ok(),
            file: Mutex::new(file),
        },
        hash,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-integrity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn read_all(source: &FileSource) -> Vec<u8> {
        let chunks = antiphon_net::file_transfer::chunk_count(source.len());
        (0..chunks).flat_map(|i| source.read_chunk(i).unwrap()).collect()
    }

    #[test]
    fn a_file_hashes_to_its_contents_and_reads_back_whole() {
        let contents: Vec<u8> = (0..CHUNK_SIZE * 3 + 11).map(|i| (i % 247) as u8).collect();
        let path = temp_file("stable.bin", &contents);
        let (source, hash) = open_and_hash(&path).unwrap();

        assert_eq!(hash, antiphon_net::file_transfer::sha256_hex(&contents));
        assert_eq!(source.len(), contents.len() as u64);
        assert_eq!(read_all(&source), contents);
    }

    #[test]
    fn editing_a_file_changes_the_hash_it_would_be_offered_under() {
        // The hash *is* the address, so bytes that do not match it are a
        // lie about what the file is.
        let path = temp_file("edited.bin", b"the original contents");
        let (_, first) = open_and_hash(&path).unwrap();
        std::fs::write(&path, b"something else entirely").unwrap();
        let (_, second) = open_and_hash(&path).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn a_file_changed_after_it_was_offered_stops_the_send() {
        // The window streaming opens: the bytes sent are read after the
        // bytes hashed. Sending on would deliver content that does not
        // match its own address.
        let path = temp_file("moving.bin", &vec![1u8; CHUNK_SIZE * 2]);
        let (source, _) = open_and_hash(&path).unwrap();
        assert!(source.read_chunk(0).is_ok());

        std::fs::write(&path, vec![2u8; CHUNK_SIZE * 3]).unwrap();
        let error = source.read_chunk(1).unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[test]
    fn a_file_that_vanished_is_reported_before_the_transfer_starts() {
        let path = temp_file("gone.bin", b"here for now");
        open_and_hash(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        // Failing here costs nothing. Failing halfway wastes both
        // people's time and produces a mystery.
        assert!(matches!(open_and_hash(&path), Err(Unavailable::Missing)));
    }

    #[test]
    fn a_voice_message_in_memory_reads_back_the_same_way() {
        let bytes: Vec<u8> = (0..CHUNK_SIZE + 9).map(|i| i as u8).collect();
        let source = FileSource::Memory(bytes.clone());
        assert_eq!(read_all(&source), bytes);
    }

    #[test]
    fn asking_for_a_chunk_past_the_end_is_an_error_not_a_panic() {
        // The index comes from a peer's repair request, so it is not to be
        // trusted to be in range.
        let source = FileSource::Memory(vec![0u8; 10]);
        assert!(source.read_chunk(1).is_err());
        assert!(source.read_chunk(u32::MAX).is_err());
    }

    #[test]
    fn every_reason_explains_itself_in_terms_of_what_happened() {
        // Not "hash mismatch" -- what that means to the person who moved
        // a file five minutes ago.
        assert!(Unavailable::Missing.explain("report.pdf").contains("moved or deleted"));
        assert!(Unavailable::Unreadable("denied".into()).explain("report.pdf").contains("denied"));
        assert!(Unavailable::Changing.explain("report.pdf").contains("still writing"));
    }

    #[test]
    fn every_explanation_names_the_file() {
        for reason in [Unavailable::Missing, Unavailable::Unreadable("why".into()), Unavailable::Changing] {
            assert!(
                reason.explain("myfile.txt").contains("myfile.txt"),
                "{reason:?} does not name the file"
            );
        }
    }

    #[test]
    fn an_empty_file_is_perfectly_valid() {
        let path = temp_file("empty.bin", b"");
        let (source, hash) = open_and_hash(&path).unwrap();
        assert_eq!(source.len(), 0);
        assert_eq!(hash, antiphon_net::file_transfer::sha256_hex(b""));
    }
}
