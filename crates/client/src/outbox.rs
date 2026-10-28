//! The outbox (spec §8: "messages sent while someone is offline stay on
//! your device and send themselves when they return").
//!
//! This is Route 1 -- Direct -- and only Route 1. Nothing here deposits
//! mail with a node, a relay, or another device, because none of those
//! exist yet. What it does is close the gap that made chat lossy: today a
//! message handed to a connection that is already dying is dropped with
//! its error discarded, and the sender is shown it in their own log as
//! though it arrived.
//!
//! The design rule is that a message leaves the outbox only when the
//! transport says it went. Anything else -- optimistically clearing the
//! queue, or writing it into history as sent and hoping -- produces the
//! one failure a messaging app must not have: your record and theirs
//! disagreeing, with no way to tell which is right.
//!
//! Stored encrypted at rest under the same backup key as history, in a
//! file next to it. A queue of unsent messages is exactly as sensitive as
//! the conversation it belongs to, and it sits on disk for longer.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use antiphon_crypto::BackupKey;

/// Cap on queued messages per contact.
///
/// A queue that grows without limit is a disk-fill waiting for someone who
/// simply never comes back. When the cap is hit the *oldest* is dropped,
/// on the reasoning that the newest message is the one still worth
/// delivering -- and the sender is told, rather than it happening quietly.
pub const MAX_QUEUED: usize = 500;

/// One message waiting to go out.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Queued {
    /// The id the message already has in the sender's chat log, so a
    /// delivery can be matched back to the line the user is looking at.
    pub id: u64,
    pub text: String,
    /// Unix seconds when it was composed. Used for the give-up window and
    /// nothing else -- ordering comes from position in the queue, since
    /// clocks cannot be assumed to move forwards.
    pub queued_at_secs: u64,
    /// How many times delivery has been attempted, for the UI to say
    /// "still trying" honestly rather than implying nothing has happened.
    #[serde(default)]
    pub attempts: u32,
}

/// What the sender is told about a message.
///
/// Deliberately three states and not two: "sent" and "delivered" being the
/// same word is how a messaging app ends up claiming something arrived
/// when it only left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    /// Handed to a live connection, no confirmation yet.
    Sending,
    /// Queued because there is nobody to hand it to.
    Waiting,
    /// The transport accepted it.
    Delivered,
}

impl Status {
    /// The exact wording the spec asks for (§8).
    pub fn wording(&self) -> &'static str {
        match self {
            Status::Sending => "Sending",
            Status::Waiting => "Waiting for them to come online",
            Status::Delivered => "Delivered",
        }
    }
}

/// How long a message keeps trying before the outbox gives up on it.
///
/// The sender controls this and nothing else about delivery (spec §8):
/// where mail may be deposited is the *recipient's* decision, not theirs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum GiveUp {
    /// The default: a message you meant to send is still a message you
    /// meant to send a week later. Expiring by default would silently
    /// discard things people believe they have sent.
    #[default]
    Never,
    AfterSecs(u64),
}

impl GiveUp {
    pub fn expired(&self, queued_at_secs: u64, now_secs: u64) -> bool {
        match self {
            GiveUp::Never => false,
            // Saturating rather than wrapping: a clock that jumped
            // backwards must not make everything look ancient.
            GiveUp::AfterSecs(limit) => now_secs.saturating_sub(queued_at_secs) >= *limit,
        }
    }
}

pub fn outbox_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("outbox")
}

/// Where one contact's queue lives.
pub fn outbox_path(config_dir: &Path, identity_key_hex: &str) -> PathBuf {
    // Same defence as history: this is already hex from a verified
    // handshake, but it becomes a filename, so anything that is not hex is
    // dropped rather than trusted as a path component.
    let safe: String =
        identity_key_hex.chars().filter(|c| c.is_ascii_hexdigit()).take(64).collect();
    outbox_dir(config_dir).join(format!("{safe}.outbox"))
}

/// Reads a contact's queue. A missing or unreadable file is an empty
/// queue, not an error -- the same reasoning as history: an error the user
/// can do nothing about, on every launch, helps nobody.
pub fn load(config_dir: &Path, identity_key_hex: &str, key: &BackupKey) -> Vec<Queued> {
    let Ok(sealed) = std::fs::read(outbox_path(config_dir, identity_key_hex)) else {
        return Vec::new();
    };
    let Ok(plaintext) = key.open_content(&sealed) else {
        return Vec::new();
    };
    serde_json::from_slice(&plaintext).unwrap_or_default()
}

pub fn save(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    queue: &[Queued],
) -> anyhow::Result<()> {
    let path = outbox_path(config_dir, identity_key_hex);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // An empty queue is a deleted file rather than an encrypted empty
    // list: leaving one behind says "there is something pending here" to
    // anyone looking at the directory, which would be false.
    if queue.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    let sealed = key
        .seal_content(&serde_json::to_vec(queue)?)
        .map_err(|e| anyhow::anyhow!("could not encrypt outbox: {e}"))?;
    std::fs::write(&path, sealed)?;
    Ok(())
}

/// Adds a message to a queue, returning how many were dropped to make room.
///
/// Returns the count rather than dropping silently so the caller can say
/// so: a message vanishing without a word is the failure this whole module
/// exists to prevent.
pub fn enqueue(queue: &mut Vec<Queued>, message: Queued) -> usize {
    queue.push(message);
    let excess = queue.len().saturating_sub(MAX_QUEUED);
    if excess > 0 {
        queue.drain(..excess);
    }
    excess
}

/// Drops messages the sender's give-up window has run out on.
///
/// Applied on read, like history's expiry, so a window that elapsed while
/// the app was closed takes effect the moment anyone looks.
pub fn drop_expired(queue: &mut Vec<Queued>, give_up: GiveUp, now_secs: u64) -> Vec<Queued> {
    let mut dropped = Vec::new();
    queue.retain(|m| {
        if give_up.expired(m.queued_at_secs, now_secs) {
            dropped.push(m.clone());
            false
        } else {
            true
        }
    });
    dropped
}

/// Removes one message from the queue, once the transport has accepted it.
///
/// Returns whether it was there. Called only on a *successful* send: a
/// message leaves the outbox when it goes, not when it is attempted.
pub fn mark_delivered(queue: &mut Vec<Queued>, id: u64) -> bool {
    let before = queue.len();
    queue.retain(|m| m.id != id);
    queue.len() != before
}

/// What the UI shows for a message, given whether it is still queued.
pub fn status_of(queue: &[Queued], id: u64, connected: bool) -> Status {
    match queue.iter().find(|m| m.id == id) {
        None => Status::Delivered,
        Some(_) if connected => Status::Sending,
        Some(_) => Status::Waiting,
    }
}

/// One line summarising a queue, or `None` when there is nothing pending.
pub fn summary(queue: &[Queued], peer_name: &str) -> Option<String> {
    let n = queue.len();
    if n == 0 {
        return None;
    }
    let plural = if n == 1 { "message" } else { "messages" };
    Some(format!("{n} {plural} waiting for {peer_name} to come online."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("antiphon-outbox-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn message(id: u64, at: u64) -> Queued {
        Queued { id, text: format!("m{id}"), queued_at_secs: at, attempts: 0 }
    }

    #[test]
    fn a_message_stays_queued_until_the_transport_accepts_it() {
        // The rule the whole module rests on. Clearing on attempt is what
        // makes a sender's log and a receiver's log disagree.
        let mut queue = vec![message(1, 0)];
        assert_eq!(status_of(&queue, 1, true), Status::Sending);
        assert!(mark_delivered(&mut queue, 1));
        assert_eq!(status_of(&queue, 1, true), Status::Delivered);
    }

    #[test]
    fn the_same_message_reads_differently_offline() {
        // "Sending" while there is nothing to send to would be a lie, and
        // it is the lie that makes people repeat themselves.
        let queue = vec![message(1, 0)];
        assert_eq!(status_of(&queue, 1, false), Status::Waiting);
    }

    #[test]
    fn delivering_a_message_that_was_never_queued_is_not_an_error() {
        // A message sent on a live connection never enters the queue. The
        // confirmation path must not care.
        let mut queue = vec![message(1, 0)];
        assert!(!mark_delivered(&mut queue, 99));
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn the_three_states_are_worded_as_the_spec_asks() {
        assert_eq!(Status::Sending.wording(), "Sending");
        assert_eq!(Status::Waiting.wording(), "Waiting for them to come online");
        assert_eq!(Status::Delivered.wording(), "Delivered");
    }

    #[test]
    fn sent_and_delivered_are_not_the_same_word() {
        // Collapsing them is how an app ends up claiming something
        // arrived when it only left.
        assert_ne!(Status::Sending.wording(), Status::Delivered.wording());
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_says_how_many() {
        let mut queue: Vec<Queued> = (0..MAX_QUEUED as u64).map(|i| message(i, 0)).collect();
        assert_eq!(enqueue(&mut queue, message(9999, 0)), 1);
        assert_eq!(queue.len(), MAX_QUEUED);
        assert!(!queue.iter().any(|m| m.id == 0), "oldest was kept");
        assert!(queue.iter().any(|m| m.id == 9999), "newest was dropped");
    }

    #[test]
    fn a_queue_under_the_cap_drops_nothing() {
        let mut queue = vec![message(1, 0)];
        assert_eq!(enqueue(&mut queue, message(2, 0)), 0);
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn by_default_a_message_is_never_given_up_on() {
        // Expiring by default would silently discard things people
        // believe they have sent.
        assert!(!GiveUp::default().expired(0, 60 * 60 * 24 * 365));
    }

    #[test]
    fn a_give_up_window_drops_only_what_has_run_out() {
        let mut queue = vec![message(1, 0), message(2, 500)];
        let dropped = drop_expired(&mut queue, GiveUp::AfterSecs(100), 300);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].id, 1);
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_clock_that_jumped_backwards_does_not_expire_everything() {
        // A message queued "in the future" must not read as ancient.
        let mut queue = vec![message(1, 5_000)];
        assert!(drop_expired(&mut queue, GiveUp::AfterSecs(60), 100).is_empty());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn a_summary_is_shown_only_when_something_is_waiting() {
        assert_eq!(summary(&[], "Alice"), None);
        let one = summary(&[message(1, 0)], "Alice").unwrap();
        assert!(one.contains("1 message "), "{one}");
        assert!(one.contains("Alice"), "{one}");
        let two = summary(&[message(1, 0), message(2, 0)], "Alice").unwrap();
        assert!(two.contains("2 messages"), "{two}");
    }

    #[test]
    fn a_path_cannot_escape_the_outbox_directory() {
        // The key is hex from a verified handshake, but it becomes a
        // filename. Trusting that is how a peer names a file elsewhere.
        let dir = Path::new("/config");
        let path = outbox_path(dir, "../../etc/passwd");
        assert_eq!(path.parent().unwrap(), outbox_dir(dir));
        assert!(!path.to_string_lossy().contains(".."), "{}", path.display());
    }

    #[test]
    fn an_over_long_key_cannot_make_an_over_long_filename() {
        let path = outbox_path(Path::new("/config"), &"a".repeat(500));
        assert_eq!(path.file_name().unwrap(), format!("{}.outbox", "a".repeat(64)).as_str());
    }

    #[test]
    fn a_queue_survives_being_written_and_read_back() {
        let dir = temp_dir("roundtrip");
        let key = BackupKey::generate();
        let queue = vec![message(1, 10), message(2, 20)];
        save(&dir, "ab", &key, &queue).unwrap();
        assert_eq!(load(&dir, "ab", &key), queue);
    }

    #[test]
    fn an_emptied_queue_leaves_no_file_behind() {
        // A leftover file says "something is pending here" to anyone
        // looking at the directory, which would be false.
        let dir = temp_dir("emptied");
        let key = BackupKey::generate();
        save(&dir, "ab", &key, &[message(1, 0)]).unwrap();
        assert!(outbox_path(&dir, "ab").exists());
        save(&dir, "ab", &key, &[]).unwrap();
        assert!(!outbox_path(&dir, "ab").exists());
    }

    #[test]
    fn a_queue_written_under_another_key_is_not_readable() {
        // It is stored encrypted for a reason: a list of unsent messages
        // is as sensitive as the conversation, and sits on disk longer.
        let dir = temp_dir("wrongkey");
        save(&dir, "ab", &BackupKey::generate(), &[message(1, 0)]).unwrap();
        assert!(load(&dir, "ab", &BackupKey::generate()).is_empty());
    }

    #[test]
    fn a_missing_queue_is_an_empty_one_not_a_failure() {
        let dir = temp_dir("missing");
        assert!(load(&dir, "ffff", &BackupKey::generate()).is_empty());
    }

    #[test]
    fn two_contacts_do_not_share_a_queue() {
        let dir = temp_dir("separate");
        let key = BackupKey::generate();
        save(&dir, "aa", &key, &[message(1, 0)]).unwrap();
        save(&dir, "bb", &key, &[message(2, 0), message(3, 0)]).unwrap();
        assert_eq!(load(&dir, "aa", &key).len(), 1);
        assert_eq!(load(&dir, "bb", &key).len(), 2);
    }
}
