//! A community's board, the member side (spec §11 "Communities at full
//! scale"; the node side is `antiphon_relay::board`, the protocol
//! [`antiphon_net::board`]).
//!
//! Without a board, a message for everyone in a community is sealed once
//! per member and left at each member's mailbox: fine for a few hundred,
//! not for thousands. With one, it is sealed once and posted once to the
//! community's board, and every member reads the board a page at a time --
//! which is also how a member who was away catches up.
//!
//! What a board entry is:
//!
//! ```text
//! epoch:8 | nonce:12 | ChaCha20-Poly1305(key, from:32 | signature:64 | fragment)
//! ```
//!
//! - `key` and the node's access `token` come from the MLS exporter of
//!   that epoch ([`EpochKey::derive`]), so only who was in the group then
//!   can read the entry or reach the board, and a removed member is cut
//!   off by the next epoch without anyone telling the node.
//! - `from` signs the board, epoch and fragment with their key in the
//!   community, so a member can't post in another's name (the MLS message
//!   inside is authenticated too; this also covers the outer fragment).
//! - `fragment` is one fragment of a community [`Wire`](crate::community::Wire)
//!   message, as a mailbox would carry it, so everything after this is the
//!   same as for mail.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use serde::{Deserialize, Serialize};

use antiphon_crypto::community::Key;
use antiphon_crypto::group::GroupMember;
use antiphon_crypto::IdentityKeyPair;

/// How many epochs' keys a member keeps: as many as the node accepts
/// tokens for (`antiphon_relay::board::TOKEN_WINDOW`). Entries older than
/// that can't be read and are skipped.
pub const KEEP_EPOCHS: usize = 16;

const KEY_LABEL: &str = "antiphon/board/key";
const TOKEN_LABEL: &str = "antiphon/board/token";
const SIGN_CONTEXT: &[u8] = b"antiphon/board/entry/v1";

/// One epoch's entry key and access token.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct EpochKey {
    pub epoch: u64,
    pub key: Key,
    pub token: Key,
}

impl EpochKey {
    /// The current epoch's, from the group's exporter. `community` is the
    /// exporter context, so two communities never share keys.
    pub fn derive(mls: &GroupMember, community: &Key) -> Option<Self> {
        let epoch = mls.epoch().ok()?;
        let key = mls.export_secret(KEY_LABEL, &community.0, 32).ok()?.try_into().ok()?;
        let token = mls.export_secret(TOKEN_LABEL, &community.0, 32).ok()?.try_into().ok()?;
        Some(Self { epoch, key: Key(key), token: Key(token) })
    }
}

/// Adds the group's current epoch to `keys` if it isn't there, keeping
/// the newest [`KEEP_EPOCHS`], in epoch order.
pub fn note_epoch(keys: &mut Vec<EpochKey>, mls: &GroupMember, community: &Key) {
    let Some(k) = EpochKey::derive(mls, community) else { return };
    if keys.iter().any(|e| e.epoch == k.epoch) {
        return;
    }
    keys.push(k);
    keys.sort_by_key(|e| e.epoch);
    let excess = keys.len().saturating_sub(KEEP_EPOCHS);
    keys.drain(..excess);
}

fn aad(board: &Key, epoch: u64) -> Vec<u8> {
    let mut a = board.0.to_vec();
    a.extend_from_slice(&epoch.to_be_bytes());
    a
}

fn signed_bytes(board: &Key, epoch: u64, fragment: &[u8]) -> Vec<u8> {
    let mut m = SIGN_CONTEXT.to_vec();
    m.extend_from_slice(&aad(board, epoch));
    m.extend_from_slice(fragment);
    m
}

/// Seals one fragment for board `board` under `key`, signed by `signer`.
pub fn seal(board: &Key, key: &EpochKey, signer: &IdentityKeyPair, fragment: &str) -> Vec<u8> {
    let mut plain = signer.public().identity_key.to_vec();
    plain.extend_from_slice(&signer.sign(&signed_bytes(board, key.epoch, fragment.as_bytes())));
    plain.extend_from_slice(fragment.as_bytes());
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");
    let cipher = ChaCha20Poly1305::new(&key.key.0.into());
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: &plain, aad: &aad(board, key.epoch) })
        .expect("sealing in memory does not fail");
    let mut out = key.epoch.to_be_bytes().to_vec();
    out.extend_from_slice(&nonce);
    out.extend(sealed);
    out
}

/// What an entry turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Opened {
    /// A fragment, and who posted it (checked).
    Fragment { from: [u8; 32], fragment: String },
    /// From an epoch we haven't reached yet: wait for the commit that
    /// brings us there (it is earlier on the board, or on its way).
    Ahead(u64),
    /// From an epoch too old to read, or damaged, or forged: skip it.
    Unreadable,
}

/// Opens an entry with whichever of `keys` it was sealed under.
pub fn open(board: &Key, keys: &[EpochKey], entry: &[u8]) -> Opened {
    if entry.len() < 20 {
        return Opened::Unreadable;
    }
    let epoch = u64::from_be_bytes(entry[..8].try_into().expect("8 bytes"));
    let Some(key) = keys.iter().find(|k| k.epoch == epoch) else {
        return if keys.last().is_some_and(|k| epoch > k.epoch) { Opened::Ahead(epoch) } else { Opened::Unreadable };
    };
    let nonce: [u8; 12] = entry[8..20].try_into().expect("12 bytes");
    let cipher = ChaCha20Poly1305::new(&key.key.0.into());
    let Ok(plain) = cipher.decrypt(&Nonce::from(nonce), Payload { msg: &entry[20..], aad: &aad(board, epoch) }) else {
        return Opened::Unreadable;
    };
    if plain.len() < 96 {
        return Opened::Unreadable;
    }
    let from: [u8; 32] = plain[..32].try_into().expect("32 bytes");
    let signature: [u8; 64] = plain[32..96].try_into().expect("64 bytes");
    let fragment = &plain[96..];
    if antiphon_crypto::verify_signature(&from, &signed_bytes(board, epoch, fragment), &signature).is_err() {
        return Opened::Unreadable;
    }
    match String::from_utf8(fragment.to_vec()) {
        Ok(fragment) => Opened::Fragment { from, fragment },
        Err(_) => Opened::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(epoch: u64, b: u8) -> EpochKey {
        EpochKey { epoch, key: Key([b; 32]), token: Key([b ^ 0xff; 32]) }
    }

    #[test]
    fn an_entry_opens_for_its_epoch_and_names_its_poster() {
        let me = IdentityKeyPair::generate();
        let board = Key([3; 32]);
        let keys = vec![key(4, 1), key(5, 2)];
        let entry = seal(&board, &keys[0], &me, "fragment");
        assert!(entry.len() <= antiphon_net::board::MAX_ENTRY);
        assert_eq!(open(&board, &keys, &entry), Opened::Fragment { from: me.public().identity_key, fragment: "fragment".into() });
        // Another board's, an epoch we don't hold yet, one too old.
        assert_eq!(open(&Key([4; 32]), &keys, &entry), Opened::Unreadable);
        assert_eq!(open(&board, &keys, &seal(&board, &key(6, 9), &me, "x")), Opened::Ahead(6));
        assert_eq!(open(&board, &keys, &seal(&board, &key(2, 9), &me, "x")), Opened::Unreadable);
        // A damaged one.
        let mut bad = entry;
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(open(&board, &keys, &bad), Opened::Unreadable);
    }

    #[test]
    fn a_full_size_fragment_fits_one_entry() {
        let wire = crate::community::Wire::App { community: Key([1; 32]), data: "x".repeat(4000) };
        let me = IdentityKeyPair::generate();
        for f in crate::community::fragments(&wire, Some(&IdentityKeyPair::generate())) {
            assert!(seal(&Key([0; 32]), &key(1, 1), &me, &f).len() <= antiphon_net::board::MAX_ENTRY);
        }
    }

    #[test]
    fn keys_follow_the_epoch_and_only_the_newest_are_kept() {
        let mut keys: Vec<EpochKey> = (0..KEEP_EPOCHS as u64).map(|e| key(e, e as u8)).collect();
        let me = IdentityKeyPair::generate();
        let mut mls = GroupMember::new(&me);
        mls.create().unwrap();
        // The new group is at epoch 0, which is already held.
        note_epoch(&mut keys, &mls, &Key([1; 32]));
        assert_eq!(keys.len(), KEEP_EPOCHS);
        let mut fresh = Vec::new();
        note_epoch(&mut fresh, &mls, &Key([1; 32]));
        let mut other = Vec::new();
        note_epoch(&mut other, &mls, &Key([2; 32]));
        assert_ne!(fresh[0].key, other[0].key, "per community");
        keys.retain(|k| k.epoch != 0);
        keys.push(key(99, 0));
        note_epoch(&mut keys, &mls, &Key([1; 32]));
        assert_eq!(keys.len(), KEEP_EPOCHS);
        assert_eq!(keys[0].epoch, 1, "the oldest went");
    }
}
