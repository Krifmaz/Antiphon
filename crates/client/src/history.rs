//! Persisted message history, encrypted at rest (spec §9 "Search over
//! your own history, indexed locally on your device").
//!
//! Chat used to exist only while a call was up: hang up and it was gone.
//! This keeps it, and keeps it **per identity key** rather than per
//! address, so someone moving networks keeps their history and someone
//! *taking over* an address does not inherit it.
//!
//! Encrypted with the account's [`antiphon_crypto::BackupKey`], which is the
//! key that gets sealed to authorised devices. That is what makes the
//! device-approval story real rather than decorative: history on disk is
//! unreadable without a key that only approved devices hold, so a stolen
//! laptop with no approval, or a copied config directory, yields nothing.
//!
//! **Known gap, stated rather than hidden:** the backup key itself is
//! written next to the identity as plain bytes, for exactly the reason
//! `antiphon_crypto::store` documents -- an OS keystore integration is a
//! separate piece of work. Someone with read access to the config
//! directory today can therefore read history. This raises the bar from
//! "open the file" to "open two files", which is honest but not a
//! substitute for the keystore.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use antiphon_crypto::BackupKey;

/// One stored message.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoredMessage {
    /// Stable per-message id, so an edit or a delete can name which
    /// message it means (spec §13 "message lifecycle").
    ///
    /// Chosen by the *sender* and unique only within one conversation,
    /// which is all that is needed: a peer can only ever edit their own
    /// messages, so two sides cannot collide on an id that matters.
    #[serde(default)]
    pub id: u64,
    pub from_me: bool,
    pub text: String,
    /// Unix seconds. Informational -- ordering comes from position in the
    /// log, since two devices' clocks cannot be assumed to agree.
    pub at_secs: u64,
    /// True once the sender edited it, so the UI can say so. An edit that
    /// looks identical to an original is a way to put words in someone's
    /// mouth after the fact.
    #[serde(default)]
    pub edited: bool,
    /// Tombstoned by its sender. The text is replaced rather than the
    /// entry removed, so the conversation still shows that something was
    /// here -- a message that vanishes without trace is indistinguishable
    /// from one that never arrived.
    #[serde(default)]
    pub deleted: bool,
    /// Kept at the top of the conversation (spec §13 "pinned messages").
    /// Local to this device: pinning is a note to yourself, not a claim on
    /// the other person's view.
    #[serde(default)]
    pub pinned: bool,
    /// Unix seconds after which this message deletes itself, if the
    /// conversation has disappearing messages switched on.
    #[serde(default)]
    pub expires_at_secs: Option<u64>,
}

impl StoredMessage {
    /// What to show. A deleted message keeps its place in the
    /// conversation with a marker rather than silently disappearing.
    pub fn display_text(&self) -> String {
        if self.deleted {
            "(deleted)".to_string()
        } else if self.edited {
            format!("{} (edited)", self.text)
        } else {
            self.text.clone()
        }
    }

    pub fn is_expired(&self, now_secs: u64) -> bool {
        self.expires_at_secs.is_some_and(|at| now_secs >= at)
    }
}

/// Drops messages whose time is up (spec §13 "disappearing messages").
///
/// Applied on every read rather than by a timer: a message that expired
/// while the app was closed must not reappear just because nothing was
/// running to delete it.
pub fn prune_expired(log: &mut Vec<StoredMessage>, now_secs: u64) -> usize {
    let before = log.len();
    log.retain(|m| !m.is_expired(now_secs));
    before - log.len()
}

/// Cap on messages retained per conversation.
///
/// A bound rather than unlimited growth: the whole log is read, decrypted
/// and re-encrypted on every append, so an unbounded file would make each
/// message progressively more expensive to send.
const MAX_MESSAGES: usize = 2000;

pub fn history_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("history")
}

/// Where one conversation's history lives, keyed by the peer's identity.
pub fn history_path(config_dir: &Path, identity_key_hex: &str) -> PathBuf {
    // Defence in depth: this is already hex from a verified handshake, but
    // it becomes a filename, so anything that isn't hex is dropped rather
    // than trusted to be safe as a path component.
    let safe: String = identity_key_hex
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(64)
        .collect();
    history_dir(config_dir).join(format!("{safe}.hist"))
}

/// Where the backup key lives. Separate from `config.json` so a settings
/// export can never sweep it up, matching how the identity is stored.
pub fn backup_key_path(config_dir: &Path) -> PathBuf {
    config_dir.join("backup.key")
}

/// Loads the account's backup key, generating and persisting one on first
/// use.
///
/// Sealed by the OS keystore wherever there is one (spec §6), on the same
/// reasoning as the identity seed and with more at stake day to day: this
/// key decrypts every message the user has ever sent or received. An
/// existing plaintext key is upgraded in place on first read.
pub fn load_or_create_backup_key(config_dir: &Path) -> std::io::Result<BackupKey> {
    let path = backup_key_path(config_dir);
    let unreadable = |detail: &str| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{} {detail} -- refusing to replace it, since a new key would make all                  existing history permanently unreadable",
                path.display()
            ),
        )
    };
    match std::fs::read(&path) {
        Ok(bytes) if antiphon_crypto::keystore::is_wrapped(&bytes) => {
            let plain = antiphon_crypto::keystore::unwrap(&bytes).ok_or_else(|| {
                unreadable("was sealed by the operating system and cannot be unsealed here")
            })?;
            if plain.len() != 32 {
                return Err(unreadable("did not unseal to a 32-byte key"));
            }
            let mut key = [0u8; 32];
            key.copy_from_slice(&plain);
            Ok(BackupKey::from_bytes(key))
        }
        Ok(bytes) if bytes.len() == 32 => {
            let mut bytes32 = [0u8; 32];
            bytes32.copy_from_slice(&bytes);
            let key = BackupKey::from_bytes(bytes32);
            // Upgraded in place, so an existing install stops leaving the
            // key to its whole history readable. Not fatal if the write
            // fails: the key is already loaded and correct, and refusing
            // to start over a storage upgrade would leave the user worse
            // off than the plaintext file they already had.
            if antiphon_crypto::keystore::available() {
                if let Err(e) = save_backup_key(&path, &key) {
                    eprintln!("[history] could not move the backup key into the OS keystore: {e}");
                }
            }
            Ok(key)
        }
        Ok(_) => Err(unreadable("is not a 32-byte backup key")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = BackupKey::generate();
            save_backup_key(&path, &key)?;
            Ok(key)
        }
        Err(e) => Err(e),
    }
}

/// Writes the backup key, sealed where the platform allows it.
fn save_backup_key(path: &Path, key: &BackupKey) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = key.to_bytes();
    match antiphon_crypto::keystore::wrap(&bytes) {
        Some(sealed) => std::fs::write(path, sealed),
        None => std::fs::write(path, bytes),
    }
}

/// Reads a conversation. A missing file is an empty conversation, not an
/// error -- that is simply someone you have not talked to yet.
pub fn load(config_dir: &Path, identity_key_hex: &str, key: &BackupKey) -> Vec<StoredMessage> {
    let path = history_path(config_dir, identity_key_hex);
    let Ok(sealed) = std::fs::read(&path) else {
        return Vec::new();
    };
    // A history that will not decrypt is dropped rather than surfaced as
    // an error: the alternative is a modal the user can do nothing about,
    // on every launch, for a file they cannot repair.
    let Ok(plaintext) = key.open_content(&sealed) else {
        return Vec::new();
    };
    let mut log: Vec<StoredMessage> = serde_json::from_slice(&plaintext).unwrap_or_default();
    // Expiry is enforced here, on the way out, so a message whose time ran
    // out while the app was closed is gone the moment anyone looks -- not
    // whenever a timer next happens to fire.
    prune_expired(&mut log, now_secs());
    log
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Applies an edit from whoever sent the message.
///
/// Returns whether anything changed. Only a message the peer actually sent
/// can be edited by them, and only one you sent can be edited by you --
/// otherwise an "edit" would be a way to rewrite the other person's words.
pub fn apply_edit(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    id: u64,
    from_me: bool,
    new_text: &str,
) -> bool {
    let mut log = load(config_dir, identity_key_hex, key);
    let Some(message) = log.iter_mut().find(|m| m.id == id && m.from_me == from_me) else {
        return false;
    };
    if message.deleted {
        return false;
    }
    message.text = new_text.to_string();
    message.edited = true;
    let _ = save(config_dir, identity_key_hex, key, &log);
    true
}

/// Tombstones a message, from whoever sent it.
pub fn apply_delete(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    id: u64,
    from_me: bool,
) -> bool {
    let mut log = load(config_dir, identity_key_hex, key);
    let Some(message) = log.iter_mut().find(|m| m.id == id && m.from_me == from_me) else {
        return false;
    };
    message.deleted = true;
    message.text.clear();
    message.edited = false;
    let _ = save(config_dir, identity_key_hex, key, &log);
    true
}

/// Pins or unpins a message. Local only -- the peer is not told.
pub fn set_pinned(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    id: u64,
    pinned: bool,
) -> bool {
    let mut log = load(config_dir, identity_key_hex, key);
    let Some(message) = log.iter_mut().find(|m| m.id == id) else {
        return false;
    };
    message.pinned = pinned;
    let _ = save(config_dir, identity_key_hex, key, &log);
    true
}

/// Appends one message and writes the conversation back.
pub fn append(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    message: StoredMessage,
) -> anyhow::Result<()> {
    let mut log = load(config_dir, identity_key_hex, key);
    log.push(message);
    if log.len() > MAX_MESSAGES {
        let excess = log.len() - MAX_MESSAGES;
        log.drain(..excess);
    }
    save(config_dir, identity_key_hex, key, &log)
}

pub fn save(
    config_dir: &Path,
    identity_key_hex: &str,
    key: &BackupKey,
    log: &[StoredMessage],
) -> anyhow::Result<()> {
    let path = history_path(config_dir, identity_key_hex);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let plaintext = serde_json::to_vec(log)?;
    let sealed = key
        .seal_content(&plaintext)
        .map_err(|e| anyhow::anyhow!("could not encrypt history: {e}"))?;
    std::fs::write(&path, sealed)?;
    Ok(())
}

/// Deletes one conversation.
pub fn forget(config_dir: &Path, identity_key_hex: &str) -> std::io::Result<()> {
    let path = history_path(config_dir, identity_key_hex);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Everything one live call needs to persist its chat: where to write,
/// whose conversation it is, and the key to seal it with.
///
/// Bundled into one value because the call pipeline is spread across
/// threads and passing three loosely-related fields through each of them
/// invites getting the pairing wrong -- writing one peer's message into
/// another peer's file.
pub struct HistorySink {
    pub dir: PathBuf,
    pub peer_hex: String,
    pub key: BackupKey,
    /// How long messages in this conversation live, or `None` to keep them
    /// (spec §13 "disappearing messages"). Set per contact.
    pub expire_after_secs: Option<u64>,
}

impl HistorySink {
    /// Persists one message. Failures are reported and swallowed: losing a
    /// line of history is bad, but taking down a live call because a disk
    /// write failed is worse.
    /// Persists one message and returns the id it was stored under, so the
    /// caller can later name it in an edit or delete.
    pub fn record(&self, id: u64, from_me: bool, text: &str) -> u64 {
        let at_secs = now_secs();
        let message = StoredMessage {
            id,
            from_me,
            text: text.to_string(),
            at_secs,
            edited: false,
            deleted: false,
            pinned: false,
            expires_at_secs: self.expire_after_secs.map(|ttl| at_secs + ttl),
        };
        if let Err(e) = append(&self.dir, &self.peer_hex, &self.key, message) {
            eprintln!("[history] could not save message: {e}");
        }
        id
    }

    pub fn edit(&self, id: u64, from_me: bool, text: &str) -> bool {
        apply_edit(&self.dir, &self.peer_hex, &self.key, id, from_me, text)
    }

    pub fn delete(&self, id: u64, from_me: bool) -> bool {
        apply_delete(&self.dir, &self.peer_hex, &self.key, id, from_me)
    }

    pub fn pin(&self, id: u64, pinned: bool) -> bool {
        set_pinned(&self.dir, &self.peer_hex, &self.key, id, pinned)
    }

    pub fn load_all(&self) -> Vec<StoredMessage> {
        load(&self.dir, &self.peer_hex, &self.key)
    }
}

/// Case-insensitive substring search over one conversation (spec §9
/// "Search over your own history, indexed locally on your device").
pub fn search<'a>(log: &'a [StoredMessage], query: &str) -> Vec<&'a StoredMessage> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    log.iter()
        .filter(|m| m.text.to_lowercase().contains(&query))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-hist-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn msg(from_me: bool, text: &str) -> StoredMessage {
        StoredMessage {
            id: 0,
            from_me,
            text: text.to_string(),
            at_secs: 1_700_000_000,
            edited: false,
            deleted: false,
            pinned: false,
            expires_at_secs: None,
        }
    }

    fn msg_id(id: u64, from_me: bool, text: &str) -> StoredMessage {
        StoredMessage { id, ..msg(from_me, text) }
    }

    #[test]
    fn an_unknown_conversation_is_empty_not_an_error() {
        let dir = temp_dir("unknown");
        let key = BackupKey::generate();
        assert!(load(&dir, &"bb".repeat(32), &key).is_empty());
    }

    #[test]
    fn messages_round_trip_through_the_encrypted_file() {
        let dir = temp_dir("roundtrip");
        let key = BackupKey::generate();
        let id = "cc".repeat(32);

        append(&dir, &id, &key, msg(true, "hello")).unwrap();
        append(&dir, &id, &key, msg(false, "hi back")).unwrap();

        let log = load(&dir, &id, &key);
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], msg(true, "hello"));
        assert_eq!(log[1], msg(false, "hi back"));
    }

    #[test]
    fn history_is_not_readable_as_plaintext_on_disk() {
        let dir = temp_dir("encrypted");
        let key = BackupKey::generate();
        let id = "dd".repeat(32);

        append(&dir, &id, &key, msg(true, "a very secret sentence")).unwrap();

        let raw = std::fs::read(history_path(&dir, &id)).unwrap();
        let as_text = String::from_utf8_lossy(&raw);
        assert!(
            !as_text.contains("a very secret sentence"),
            "history was written in the clear"
        );
    }

    #[test]
    fn a_different_key_cannot_read_the_history() {
        let dir = temp_dir("wrongkey");
        let key = BackupKey::generate();
        let id = "ee".repeat(32);
        append(&dir, &id, &key, msg(true, "hello")).unwrap();

        // This is the device-approval property in practice: a copied
        // config directory without the right backup key yields nothing.
        assert!(load(&dir, &id, &BackupKey::generate()).is_empty());
    }

    #[test]
    fn conversations_are_kept_apart_by_identity() {
        let dir = temp_dir("separate");
        let key = BackupKey::generate();
        let alice = "11".repeat(32);
        let bob = "22".repeat(32);

        append(&dir, &alice, &key, msg(true, "for alice")).unwrap();
        append(&dir, &bob, &key, msg(true, "for bob")).unwrap();

        assert_eq!(load(&dir, &alice, &key)[0].text, "for alice");
        assert_eq!(load(&dir, &bob, &key)[0].text, "for bob");
    }

    #[test]
    fn the_log_is_capped_and_drops_the_oldest_first() {
        let dir = temp_dir("cap");
        let key = BackupKey::generate();
        let id = "33".repeat(32);

        let log: Vec<StoredMessage> =
            (0..MAX_MESSAGES + 10).map(|i| msg(true, &format!("m{i}"))).collect();
        save(&dir, &id, &key, &log).unwrap();
        append(&dir, &id, &key, msg(true, "newest")).unwrap();

        let stored = load(&dir, &id, &key);
        assert_eq!(stored.len(), MAX_MESSAGES);
        assert_eq!(stored.last().unwrap().text, "newest");
        // The oldest are the ones dropped, not the newest.
        assert_ne!(stored.first().unwrap().text, "m0");
    }

    #[test]
    fn a_corrupt_history_reads_as_empty_rather_than_failing_to_start() {
        let dir = temp_dir("corrupt");
        let key = BackupKey::generate();
        let id = "44".repeat(32);
        append(&dir, &id, &key, msg(true, "hello")).unwrap();

        let path = history_path(&dir, &id);
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();

        assert!(load(&dir, &id, &key).is_empty());
    }

    #[test]
    fn forgetting_a_conversation_removes_it_and_is_idempotent() {
        let dir = temp_dir("forget");
        let key = BackupKey::generate();
        let id = "55".repeat(32);
        append(&dir, &id, &key, msg(true, "hello")).unwrap();

        forget(&dir, &id).unwrap();
        assert!(load(&dir, &id, &key).is_empty());
        // Forgetting something already gone is success, not an error.
        forget(&dir, &id).unwrap();
    }

    #[test]
    fn a_non_hex_identity_cannot_escape_the_history_directory() {
        let dir = temp_dir("traversal");
        let path = history_path(&dir, "../../etc/passwd");

        assert_eq!(path.parent().unwrap(), history_dir(&dir));
        assert!(!path.to_string_lossy().contains(".."));
    }

    #[test]
    fn the_backup_key_persists_across_loads() {
        let dir = temp_dir("backupkey");
        let first = load_or_create_backup_key(&dir).unwrap();
        let second = load_or_create_backup_key(&dir).unwrap();

        // A regenerated key would make every existing conversation
        // permanently unreadable.
        assert_eq!(first.to_bytes(), second.to_bytes());
    }

    #[test]
    fn a_plaintext_backup_key_is_upgraded_without_losing_the_history_it_unlocks() {
        // The key that decrypts every message the user has ever sent.
        // Upgrading it must not change it, or the history it unlocks is
        // gone -- which is worse than the plaintext file it replaces.
        let dir = temp_dir("backupmigrate");
        let key = BackupKey::generate();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(backup_key_path(&dir), key.to_bytes()).unwrap();

        // Something written under the old key, to prove it still opens.
        append(&dir, "ab", &key, msg(true, "before the upgrade")).unwrap();

        let loaded = load_or_create_backup_key(&dir).unwrap();
        assert_eq!(loaded.to_bytes(), key.to_bytes());
        assert_eq!(load(&dir, "ab", &loaded)[0].text, "before the upgrade");

        let on_disk = std::fs::read(backup_key_path(&dir)).unwrap();
        if antiphon_crypto::keystore::available() {
            assert!(antiphon_crypto::keystore::is_wrapped(&on_disk), "still plaintext");
            assert!(
                !on_disk.windows(32).any(|w| w == key.to_bytes()),
                "the key is still sitting in the file"
            );
        } else {
            assert_eq!(on_disk, key.to_bytes());
        }
        assert_eq!(load_or_create_backup_key(&dir).unwrap().to_bytes(), key.to_bytes());
    }

    #[test]
    fn a_sealed_backup_key_that_will_not_open_is_refused_not_replaced() {
        // Generating a new one here would silently make every
        // conversation on the machine permanently unreadable.
        let dir = temp_dir("backupsealed");
        let mut damaged = antiphon_crypto::keystore::MAGIC.to_vec();
        damaged.extend_from_slice(&[0xff; 64]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(backup_key_path(&dir), &damaged).unwrap();

        assert!(load_or_create_backup_key(&dir).is_err());
        assert_eq!(std::fs::read(backup_key_path(&dir)).unwrap(), damaged);
    }

    #[test]
    fn a_malformed_backup_key_is_an_error_rather_than_a_silent_new_one() {
        let dir = temp_dir("badkey");
        std::fs::write(backup_key_path(&dir), b"too short").unwrap();

        assert!(load_or_create_backup_key(&dir).is_err());
    }

    #[test]
    fn an_edit_rewrites_the_message_and_marks_it() {
        let dir = temp_dir("edit");
        let key = BackupKey::generate();
        let id = "aa".repeat(32);
        append(&dir, &id, &key, msg_id(1, true, "helo")).unwrap();

        assert!(apply_edit(&dir, &id, &key, 1, true, "hello"));
        let log = load(&dir, &id, &key);
        assert_eq!(log[0].text, "hello");
        assert!(log[0].edited);
        // Marked, not silently swapped: an edit indistinguishable from the
        // original is a way to put words in someone's mouth afterwards.
        assert_eq!(log[0].display_text(), "hello (edited)");
    }

    #[test]
    fn a_peer_cannot_edit_a_message_you_sent() {
        let dir = temp_dir("edit-forgery");
        let key = BackupKey::generate();
        let id = "bb".repeat(32);
        append(&dir, &id, &key, msg_id(1, true, "my words")).unwrap();

        // `from_me: false` means "the peer is claiming this is theirs".
        // It is not, so the edit must not land.
        assert!(!apply_edit(&dir, &id, &key, 1, false, "words I never said"));
        assert_eq!(load(&dir, &id, &key)[0].text, "my words");
    }

    #[test]
    fn a_delete_tombstones_rather_than_removing() {
        let dir = temp_dir("delete");
        let key = BackupKey::generate();
        let id = "cc".repeat(32);
        append(&dir, &id, &key, msg_id(1, false, "oops")).unwrap();
        append(&dir, &id, &key, msg_id(2, false, "still here")).unwrap();

        assert!(apply_delete(&dir, &id, &key, 1, false));
        let log = load(&dir, &id, &key);
        // The entry keeps its place: a message that vanishes without trace
        // is indistinguishable from one that never arrived.
        assert_eq!(log.len(), 2);
        assert!(log[0].deleted);
        assert_eq!(log[0].display_text(), "(deleted)");
        assert!(log[0].text.is_empty(), "deleted text should not linger on disk");
    }

    #[test]
    fn a_deleted_message_cannot_then_be_edited_back() {
        let dir = temp_dir("undelete");
        let key = BackupKey::generate();
        let id = "dd".repeat(32);
        append(&dir, &id, &key, msg_id(1, true, "gone")).unwrap();
        apply_delete(&dir, &id, &key, 1, true);

        assert!(!apply_edit(&dir, &id, &key, 1, true, "back again"));
        assert!(load(&dir, &id, &key)[0].deleted);
    }

    #[test]
    fn pinning_works_on_either_sides_message() {
        let dir = temp_dir("pin");
        let key = BackupKey::generate();
        let id = "ee".repeat(32);
        append(&dir, &id, &key, msg_id(1, false, "theirs")).unwrap();

        // A pin is a local note to yourself, so it applies to anyone's
        // message -- unlike edit and delete.
        assert!(set_pinned(&dir, &id, &key, 1, true));
        assert!(load(&dir, &id, &key)[0].pinned);
    }

    #[test]
    fn expired_messages_are_gone_the_moment_anyone_looks() {
        let dir = temp_dir("expire");
        let key = BackupKey::generate();
        let id = "ff".repeat(32);

        let mut expiring = msg_id(1, true, "self destructs");
        expiring.expires_at_secs = Some(1);
        let mut keeps = msg_id(2, true, "stays");
        keeps.expires_at_secs = None;
        save(&dir, &id, &key, &[expiring, keeps]).unwrap();

        // Pruned on read rather than by a timer: a message whose time ran
        // out while the app was closed must not come back just because
        // nothing was running to delete it.
        let log = load(&dir, &id, &key);
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].text, "stays");
    }

    #[test]
    fn a_message_not_yet_expired_survives() {
        let far_future = now_secs() + 10_000;
        let mut m = msg_id(1, true, "later");
        m.expires_at_secs = Some(far_future);
        assert!(!m.is_expired(now_secs()));

        let mut log = vec![m];
        assert_eq!(prune_expired(&mut log, now_secs()), 0);
    }

    #[test]
    fn editing_a_message_that_does_not_exist_is_refused() {
        let dir = temp_dir("missing");
        let key = BackupKey::generate();
        let id = "11".repeat(32);
        assert!(!apply_edit(&dir, &id, &key, 99, true, "nothing"));
        assert!(!apply_delete(&dir, &id, &key, 99, true));
        assert!(!set_pinned(&dir, &id, &key, 99, true));
    }

    #[test]
    fn search_finds_messages_case_insensitively() {
        let log = vec![msg(true, "Meeting at noon"), msg(false, "sounds good"), msg(true, "bring notes")];

        assert_eq!(search(&log, "MEETING").len(), 1);
        assert_eq!(search(&log, "oo").len(), 2);
        assert!(search(&log, "").is_empty());
        assert!(search(&log, "absent").is_empty());
    }
}
