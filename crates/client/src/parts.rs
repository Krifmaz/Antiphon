//! Messages cut into pieces that each fit a mailbox, and put back together.
//!
//! A mailbox holds blobs of at most `antiphon_net::mailbox::MAX_BLOB`
//! bytes, sealing included, and a device list, a route record or a long
//! note is bigger than that. So such a message travels as frames:
//!
//! ```text
//! message id[8] index u8 count u8 data
//! ```
//!
//! The receiver keeps pieces in a file of its own, sealed under the history
//! key (they are readable once opened), until the rest arrive or they are
//! too old to wait for. Pieces are kept apart by sender, so one sender's
//! frames never complete another's message.
//!
//! Pure file handling, shared by the phone by path.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use antiphon_crypto::{hex, BackupKey};

/// Bytes of frame header: message id, index, count.
pub const HEADER: usize = 8 + 1 + 1;
/// The most frames one message may take.
const MAX_FRAMES: usize = 255;

/// Pieces still waiting for the rest, by sender and message id: how many
/// pieces there are, the ones here (index → data, hex), and when the first
/// came.
type Waiting = BTreeMap<String, (u8, BTreeMap<u8, String>, u64)>;

/// Cuts `plain` into frames carrying at most `data_len` bytes of it each.
/// An error when it would take more than 255.
pub fn split(plain: &[u8], data_len: usize) -> std::io::Result<Vec<Vec<u8>>> {
    let chunks: Vec<&[u8]> = if plain.is_empty() { vec![plain] } else { plain.chunks(data_len).collect() };
    if chunks.len() > MAX_FRAMES {
        return Err(std::io::Error::other("too long to send by mailbox"));
    }
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    Ok(chunks
        .iter()
        .enumerate()
        .map(|(i, data)| {
            let mut frame = Vec::with_capacity(HEADER + data.len());
            frame.extend_from_slice(&id);
            frame.push(i as u8);
            frame.push(chunks.len() as u8);
            frame.extend_from_slice(data);
            frame
        })
        .collect())
}

/// Takes one frame from `sender` and returns the whole message once every
/// piece is here. Pieces wait in the file at `path` for up to `lifetime`.
pub fn join(path: &Path, key: &BackupKey, sender: &[u8; 32], frame: &[u8], lifetime: Duration) -> Option<Vec<u8>> {
    if frame.len() < HEADER {
        return None;
    }
    let (id, index, count, data) = (&frame[..8], frame[8], frame[9], &frame[HEADER..]);
    if count == 0 || index >= count {
        return None;
    }
    if count == 1 {
        return Some(data.to_vec());
    }
    let mut waiting: Waiting = std::fs::read(path)
        .ok()
        .and_then(|b| key.open_content(&b).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    waiting.retain(|_, (_, _, first)| now.saturating_sub(*first) <= lifetime.as_secs());
    let slot = format!("{}:{}", hex::encode(sender), hex::encode(id));
    let entry = waiting.entry(slot.clone()).or_insert_with(|| (count, BTreeMap::new(), now));
    if entry.0 != count {
        return None;
    }
    entry.1.insert(index, hex::encode(data));
    let whole = (entry.1.len() == count as usize).then(|| entry.1.values().filter_map(|h| hex::decode(h)).flatten().collect::<Vec<u8>>());
    if whole.is_some() {
        waiting.remove(&slot);
    }
    let plain = serde_json::to_vec(&waiting).expect("pieces always serialize");
    if let Ok(sealed) = key.seal_content(&plain) {
        let _ = std::fs::write(path, sealed);
    }
    whole
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_message_comes_back_whole_from_its_pieces_in_any_order() {
        let dir = std::env::temp_dir().join(format!("stoop-parts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("parts");
        let key = BackupKey::generate();
        let life = Duration::from_secs(60);
        let message: Vec<u8> = (0..3_000u32).map(|i| (i % 251) as u8).collect();
        let mut pieces = split(&message, 500).unwrap();
        assert_eq!(pieces.len(), 6);
        assert!(pieces.iter().all(|f| f.len() <= 500 + HEADER));
        pieces.reverse();
        let last = pieces.pop().unwrap();
        for p in &pieces {
            assert!(join(&path, &key, &[1; 32], p, life).is_none());
        }
        assert_eq!(join(&path, &key, &[1; 32], &last, life), Some(message));
        // Pieces from someone else never complete another sender's message.
        assert!(join(&path, &key, &[2; 32], &pieces[0], life).is_none());
        assert!(split(&[0; 2_000], 1).is_err(), "more than 255 pieces");
    }
}
