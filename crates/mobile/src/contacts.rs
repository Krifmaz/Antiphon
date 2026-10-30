//! Saved contacts, as JSON in the app's private storage.
//!
//! A contact *is* their identity key; the name is yours for them and the
//! address is only where they were last reachable. That is the desktop's
//! rule (see its `config::Contact`), kept here so the two agree on who
//! someone is even though the phone stores fewer details about them.

use std::path::{Path, PathBuf};

use antiphon_crypto::Invite;
use serde::{Deserialize, Serialize};

/// Someone saved in the address book, keyed by their identity key.
/// Missing fields take their defaults, so an older file still loads.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Contact {
    /// Your name for them. Local to this phone, and never what they call
    /// themselves, so a display name cannot impersonate someone you know.
    pub name: String,
    /// Their identity key, hex. Empty only for a contact saved from a bare
    /// address, until the first call pins who answers there.
    pub identity_key: String,
    /// `host:port` they were last reachable at, or empty.
    pub address: String,
    /// A verification code was compared out loud and matched.
    pub verified: bool,
    /// Unix seconds when the key was first saved.
    pub first_seen_secs: u64,
    /// Seconds after which messages with them delete themselves, or `None`
    /// to keep them. Per contact, as on the desktop: how long a
    /// conversation should linger depends on who it is with.
    pub expire_after_secs: Option<u64>,
    /// What they say about themselves, as they last sent it.
    pub bio: String,
    /// Their chosen avatar colour's id, or empty for the automatic one.
    pub color: String,
    /// Their profile picture as they last sent it (small JPEG, base64).
    pub avatar: String,
    /// Their pronouns as they last sent them; empty for none given.
    pub pronouns: String,
}

impl Contact {
    /// What to dial: an invite carrying the address and the key when both
    /// are known, so the call refuses anyone else who answers there; the
    /// bare address otherwise.
    pub fn dial_target(&self) -> Option<String> {
        if self.address.is_empty() {
            return None;
        }
        Some(match decode_key(&self.identity_key) {
            Some(key) => Invite::new(key, self.address.clone()).to_link(),
            None => self.address.clone(),
        })
    }

    /// The short fingerprint shown next to them, so a changed key is
    /// visible at a glance.
    pub fn fingerprint(&self) -> String {
        decode_key(&self.identity_key).map(|key| antiphon_crypto::fingerprint(&key)).unwrap_or_default()
    }
}

/// Lower-case hex of a 32-byte key, the form keys are stored and
/// compared in throughout the phone's files.
pub fn hex(bytes: &[u8; 32]) -> String {
    antiphon_crypto::hex::encode(bytes)
}

/// The inverse of [`hex`]: `None` for anything that is not exactly 64
/// hex digits, which is how an unpinned contact's empty key reads.
pub fn decode_key(hex: &str) -> Option<[u8; 32]> {
    antiphon_crypto::hex::decode_array(hex)
}

/// The address book, kept sorted by name (case-insensitively) after
/// every addition so the list screen can show it as it is.
#[derive(Default)]
pub struct Contacts {
    /// Every saved contact, in display order.
    pub list: Vec<Contact>,
}

/// The contacts file in the app's private storage.
fn path(dir: &Path) -> PathBuf {
    dir.join("contacts.json")
}

impl Contacts {
    /// Reads saved contacts. A file that does not parse is kept aside
    /// rather than overwritten on the next save: unlike settings, an
    /// address book is not something to quietly start over on.
    pub fn load(dir: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path(dir)) else { return Self::default() };
        match serde_json::from_str(&text) {
            Ok(list) => Self { list },
            Err(e) => {
                log::warn!("contacts.json did not parse ({e}); keeping it as contacts.json.bad");
                let _ = std::fs::rename(path(dir), dir.join("contacts.json.bad"));
                Self::default()
            }
        }
    }

    /// Writes the whole address book as pretty JSON. A failure is logged and
    /// otherwise ignored; the contacts stay in memory for this session.
    pub fn save(&self, dir: &Path) {
        let result = serde_json::to_string_pretty(&self.list)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(path(dir), json));
        if let Err(e) = result {
            log::warn!("could not save contacts: {e}");
        }
    }

    /// The contact with this identity key (hex), if saved. A contact with no
    /// key yet never matches, even an empty `key_hex`.
    pub fn by_key(&self, key_hex: &str) -> Option<&Contact> {
        self.list.iter().find(|c| !c.identity_key.is_empty() && c.identity_key == key_hex)
    }

    /// As [`by_key`](Self::by_key), for changing the contact in place.
    pub fn by_key_mut(&mut self, key_hex: &str) -> Option<&mut Contact> {
        self.list.iter_mut().find(|c| !c.identity_key.is_empty() && c.identity_key == key_hex)
    }

    /// The name to show for a key: theirs in your contacts, or a
    /// fingerprint for someone you have not saved.
    pub fn name_for(&self, key_hex: &str) -> String {
        match self.by_key(key_hex) {
            Some(contact) => contact.name.clone(),
            None => decode_key(key_hex)
                .map(|key| format!("Unsaved {}", antiphon_crypto::fingerprint(&key)))
                .unwrap_or_else(|| "Someone".into()),
        }
    }

    /// Adds someone from a pasted invite link, or updates the name and
    /// address of the contact who already has that key. Returns the
    /// sentence to show.
    pub fn add_from_invite(&mut self, name: &str, link: &str, now_secs: u64) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Give them a name first. It is only for you; they never see it.".into());
        }
        let invite = Invite::parse(link.trim()).map_err(|e| format!("That invite link is not valid: {e}"))?;
        let key = hex(&invite.identity_key);
        if let Some(existing) = self.by_key_mut(&key) {
            existing.name = name.to_string();
            if !invite.address.is_empty() {
                existing.address = invite.address;
            }
            return Ok(format!("{name} was already a contact; updated."));
        }
        self.list.push(Contact {
            name: name.to_string(),
            identity_key: key,
            address: invite.address,
            verified: false,
            first_seen_secs: now_secs,
            expire_after_secs: None,
            ..Contact::default()
        });
        self.sort();
        Ok(format!("Added {name}."))
    }

    /// Forgets the contact with this key. Their history and outbox files are
    /// left alone; deleting a contact is not deleting a conversation.
    pub fn remove(&mut self, key_hex: &str) {
        self.list.retain(|c| c.identity_key != key_hex);
    }

    /// Restores the by-name order the list screen relies on.
    fn sort(&mut self) {
        self.list.sort_by_key(|c| c.name.to_lowercase());
    }
}
