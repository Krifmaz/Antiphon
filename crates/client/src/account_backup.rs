//! Account backups: everything a device needs to *be* your account, in one
//! blob that can go into a file or straight to another device.
//!
//! There is no server holding anyone's account. The recovery phrase brings
//! back the identity key, so contacts see no key change -- but on its own
//! it brings back nothing else: not who your contacts are, not your name
//! or picture, not your settings. Lose the device without a backup and you
//! are still *you* to everyone who knows you, with an empty address book.
//!
//! A [`Bundle`] closes that gap. It carries:
//!
//! - the identity seed (the same 32 bytes the recovery phrase spells),
//! - the history key, which every sealed file in the app is written under:
//!   message history, the outbox, the phone's call log, communities,
//! - the account's own files as this device keeps them: contacts, profile
//!   and settings, and -- when asked for -- message history.
//!
//! The bundle itself is plain bytes; [`antiphon_crypto::account_seal`]
//! seals it under a passphrase for a file, or under a one-time key for a
//! move between devices.
//!
//! # Desktop and phone
//!
//! The two apps keep an account differently: the desktop in one
//! `config.json`, the phone in `settings.json` and `contacts.json`. A
//! bundle remembers which kind of device made it. Restored on the same
//! kind, every file goes back as it was. Restored on the other kind, the
//! parts both understand are translated -- contacts, and the profile (name,
//! status, about, colour, picture) -- and device-only settings such as
//! audio devices or mobile data limits stay as this device had them.
//! History and the outbox are the same format on both (the phone compiles
//! the desktop's `history` and `outbox` modules), so they move either way.
//!
//! # Nothing is deleted
//!
//! Restoring replaces this device's account. The files it replaces are
//! moved into a `replaced-<unix seconds>` folder beside them rather than
//! deleted, so restoring the wrong backup is a mistake that can be undone
//! by hand.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use antiphon_crypto::{BackupKey, IdentityKeyPair};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::history;

/// The first bytes of an encoded bundle, inside the seal.
const MAGIC: &[u8; 9] = b"STOOPBNDL";
/// The encoding this module writes.
const VERSION: u8 = 1;
/// A bundle bigger than this is refused when made and when read. Large
/// enough for years of messages and a soundboard; small enough that a
/// corrupt length cannot make the app allocate the whole disk.
pub const MAX_BUNDLE_BYTES: u64 = 1 << 30;
/// The most files one bundle may list, for the same reason.
const MAX_FILES: usize = 200_000;

/// Profile fields both apps understand, copied across when a bundle moves
/// between a desktop and a phone. Absent fields are left alone. The two
/// `page_` fields are how your own profile page is arranged
/// (`antiphon_ui::ProfileLayout`), saved the same way by both apps.
const PROFILE_KEYS: [&str; 7] = ["status_message", "bio", "color", "avatar", "pronouns", "page_blocks", "page_qr"];

/// Which kind of app made, or is restoring, a bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    /// The desktop app: one `config.json`.
    Desktop,
    /// The phone app: `settings.json` and `contacts.json`.
    Phone,
}

impl Platform {
    /// The account's files and folders, relative to the app's data folder.
    fn layout(self) -> Layout {
        match self {
            Platform::Desktop => Layout {
                files: &["config.json"],
                dirs: &["communities", "sounds", "vpn-configs"],
                history_files: &[],
            },
            Platform::Phone => Layout { files: &["settings.json", "contacts.json"], dirs: &[], history_files: &["calls.log"] },
        }
    }

    /// The name shown in "made on a … ".
    pub fn label(self) -> &'static str {
        match self {
            Platform::Desktop => "computer",
            Platform::Phone => "phone",
        }
    }
}

/// Where one kind of app keeps an account.
struct Layout {
    /// Single files that are the account: contacts, profile, settings.
    files: &'static [&'static str],
    /// Folders that are part of the account whatever the history choice.
    dirs: &'static [&'static str],
    /// Files that only come along with history.
    history_files: &'static [&'static str],
}

/// Folders that only come along with history, the same on both apps.
const HISTORY_DIRS: [&str; 2] = ["history", "outbox"];

/// One account, ready to seal.
pub struct Bundle {
    /// The kind of app that made it.
    pub platform: Platform,
    /// When it was made, unix seconds.
    pub created_secs: u64,
    /// The identity's master seed: the account itself.
    seed: Zeroizing<[u8; 32]>,
    /// The key every sealed file in [`Bundle::files`] is written under.
    history_key: Zeroizing<[u8; 32]>,
    /// Whether message history was included.
    pub has_history: bool,
    /// The account's files: path relative to the data folder (with `/`
    /// between parts), and contents.
    pub files: Vec<(String, Vec<u8>)>,
}

/// The JSON header of an encoded bundle; the file contents follow it.
#[derive(Serialize, Deserialize)]
struct Manifest {
    /// See [`Bundle::platform`].
    platform: Platform,
    /// See [`Bundle::created_secs`].
    created_secs: u64,
    /// The seed, hex.
    seed: String,
    /// The history key, hex.
    history_key: String,
    /// See [`Bundle::has_history`].
    has_history: bool,
    /// Each file's path and length, in the order their bytes follow.
    files: Vec<ManifestFile>,
}

/// One file's entry in a [`Manifest`].
#[derive(Serialize, Deserialize)]
struct ManifestFile {
    /// Relative path, `/`-separated.
    path: String,
    /// Length in bytes.
    len: u64,
}

/// What [`apply`] did, for the interface to report.
#[derive(Debug)]
pub struct Applied {
    /// Where this device's previous account files were moved, if there
    /// were any.
    pub set_aside: Option<PathBuf>,
    /// The restored account's display name, if it has one.
    pub display_name: Option<String>,
    /// How many contacts came back.
    pub contacts: usize,
}

impl Bundle {
    /// The identity this bundle restores.
    pub fn identity(&self) -> IdentityKeyPair {
        IdentityKeyPair::from_master_seed(*self.seed)
    }

    /// The short fingerprint of that identity, as the app shows it.
    pub fn fingerprint(&self) -> String {
        antiphon_crypto::fingerprint(&self.identity().public().identity_key)
    }

    /// The account's display name, read from whichever file holds it.
    pub fn display_name(&self) -> Option<String> {
        let name = match self.platform {
            Platform::Desktop => self.json("config.json")?.get("profile")?.get("display_name")?.as_str()?.to_string(),
            Platform::Phone => self.json("settings.json")?.get("display_name")?.as_str()?.to_string(),
        };
        (!name.trim().is_empty()).then_some(name)
    }

    /// How many contacts the bundle holds.
    pub fn contact_count(&self) -> usize {
        self.contacts().len()
    }

    /// One of the bundle's files parsed as JSON, if it is there and parses.
    fn json(&self, path: &str) -> Option<Value> {
        let (_, bytes) = self.files.iter().find(|(p, _)| p == path)?;
        serde_json::from_slice(bytes).ok()
    }

    /// The contacts, as JSON objects in the shape their app saves them.
    fn contacts(&self) -> Vec<Value> {
        let list = match self.platform {
            Platform::Desktop => self.json("config.json").and_then(|c| c.get("contacts").cloned()),
            Platform::Phone => self.json("contacts.json"),
        };
        match list {
            Some(Value::Array(items)) => items.into_iter().filter(Value::is_object).collect(),
            _ => Vec::new(),
        }
    }

    /// The bundle as bytes, ready for [`antiphon_crypto::account_seal`].
    ///
    /// Layout: magic, version, the manifest's length as a big-endian
    /// `u32`, the manifest as JSON, then every file's bytes in manifest
    /// order. Plain concatenation rather than JSON for the contents, so
    /// years of history do not triple in size as base64 or number arrays.
    pub fn encode(&self) -> Zeroizing<Vec<u8>> {
        let manifest = Manifest {
            platform: self.platform,
            created_secs: self.created_secs,
            seed: hex(&*self.seed),
            history_key: hex(&*self.history_key),
            has_history: self.has_history,
            files: self.files.iter().map(|(path, data)| ManifestFile { path: path.clone(), len: data.len() as u64 }).collect(),
        };
        // Zeroizing: the manifest text holds the seed in hex.
        let json = Zeroizing::new(serde_json::to_vec(&manifest).expect("a manifest always serialises"));
        let total = self.files.iter().map(|(_, d)| d.len()).sum::<usize>();
        let mut out = Zeroizing::new(Vec::with_capacity(MAGIC.len() + 5 + json.len() + total));
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(&(json.len() as u32).to_be_bytes());
        out.extend_from_slice(&json);
        for (_, data) in &self.files {
            out.extend_from_slice(data);
        }
        out
    }

    /// Reads what [`Bundle::encode`] wrote. Every length is checked against
    /// what is actually there, and every path against [`safe_path`], so a
    /// damaged or hostile bundle is an error rather than a write outside
    /// the data folder.
    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let bad = || "This backup is damaged or from a newer version of Stoop.".to_string();
        let header = MAGIC.len() + 5;
        if bytes.len() < header || !bytes.starts_with(MAGIC) || bytes[MAGIC.len()] != VERSION {
            return Err(bad());
        }
        let json_len = u32::from_be_bytes(bytes[MAGIC.len() + 1..header].try_into().expect("four bytes")) as usize;
        let json = bytes.get(header..header + json_len).ok_or_else(bad)?;
        let manifest: Manifest = serde_json::from_slice(json).map_err(|_| bad())?;
        if manifest.files.len() > MAX_FILES {
            return Err(bad());
        }
        let seed = Zeroizing::new(unhex32(&manifest.seed).ok_or_else(bad)?);
        let history_key = Zeroizing::new(unhex32(&manifest.history_key).ok_or_else(bad)?);
        let mut at = header + json_len;
        let mut files = Vec::with_capacity(manifest.files.len());
        for entry in manifest.files {
            safe_path(&entry.path).ok_or_else(bad)?;
            let len = usize::try_from(entry.len).map_err(|_| bad())?;
            let end = at.checked_add(len).ok_or_else(bad)?;
            files.push((entry.path, bytes.get(at..end).ok_or_else(bad)?.to_vec()));
            at = end;
        }
        if at != bytes.len() {
            return Err(bad());
        }
        Ok(Bundle {
            platform: manifest.platform,
            created_secs: manifest.created_secs,
            seed,
            history_key,
            has_history: manifest.has_history,
            files,
        })
    }
}

/// Collects this device's account from its data folder `dir`.
///
/// `include_history` adds message history, the outbox and (on a phone) the
/// call log; without it the bundle is small enough to keep anywhere, and
/// still holds everything needed to be reachable and recognised.
pub fn gather(
    dir: &Path,
    platform: Platform,
    identity: &IdentityKeyPair,
    history_key: &BackupKey,
    include_history: bool,
) -> std::io::Result<Bundle> {
    let layout = platform.layout();
    let mut files = Vec::new();
    for name in layout.files {
        read_into(dir, name, &mut files)?;
    }
    for name in layout.dirs {
        walk_into(dir, name, &mut files)?;
    }
    if include_history {
        for name in layout.history_files {
            read_into(dir, name, &mut files)?;
        }
        for name in HISTORY_DIRS {
            walk_into(dir, name, &mut files)?;
        }
    }
    let total: u64 = files.iter().map(|(_, d): &(String, Vec<u8>)| d.len() as u64).sum();
    if total > MAX_BUNDLE_BYTES {
        return Err(std::io::Error::other(format!(
            "the account is {} MB, more than a backup can hold; try without message history",
            total / 1_000_000
        )));
    }
    Ok(Bundle {
        platform,
        created_secs: now_secs(),
        seed: Zeroizing::new(identity.master_seed()),
        history_key: Zeroizing::new(history_key.to_bytes()),
        has_history: include_history,
        files,
    })
}

/// Makes `bundle` this device's account, in data folder `dir`.
///
/// The files it replaces are moved aside first (see the module docs), the
/// identity and history key are written through the same keystore sealing
/// the app uses at start-up, then the bundle's files are written -- as-is
/// from the same kind of app, translated from the other kind.
///
/// The app must be restarted, or reload everything it read from `dir`,
/// once this returns: what it holds in memory is the old account.
pub fn apply(dir: &Path, platform: Platform, bundle: &Bundle) -> std::io::Result<Applied> {
    let aside = aside_folder(dir);
    // Everything that makes up an account on this device, whichever way
    // the bundle was made: the old account must not leak into the new.
    let layout = platform.layout();
    let mut ours: Vec<&str> = vec!["identity.key", "backup.key"];
    ours.extend(layout.files);
    ours.extend(layout.dirs);
    ours.extend(layout.history_files);
    ours.extend(HISTORY_DIRS);
    let mut moved = false;
    for name in &ours {
        let from = dir.join(name);
        if from.exists() {
            std::fs::create_dir_all(&aside)?;
            std::fs::rename(&from, aside.join(name))?;
            moved = true;
        }
    }

    antiphon_crypto::store::save(&antiphon_crypto::store::identity_path(dir), &bundle.identity())?;
    history::save_backup_key(&history::backup_key_path(dir), &BackupKey::from_bytes(*bundle.history_key))?;

    if bundle.platform == platform {
        for (path, data) in &bundle.files {
            write_file(dir, path, data)?;
        }
    } else {
        // History and the outbox are one format on both apps; everything
        // else is the other app's own file and is translated below.
        for (path, data) in &bundle.files {
            if HISTORY_DIRS.iter().any(|d| path.starts_with(&format!("{d}/"))) {
                write_file(dir, path, data)?;
            }
        }
        let previous = |name: &str| -> Map<String, Value> {
            std::fs::read(aside.join(name))
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
        };
        let contacts = bundle.contacts();
        let profile = portable_profile(bundle);
        match platform {
            Platform::Phone => {
                // Keep this phone's own settings (port, data limits) and
                // put the account's profile on top.
                let mut settings = previous("settings.json");
                for (key, value) in profile {
                    settings.insert(key, value);
                }
                write_file(dir, "settings.json", &serde_json::to_vec_pretty(&settings)?)?;
                write_file(dir, "contacts.json", &serde_json::to_vec_pretty(&contacts)?)?;
            }
            Platform::Desktop => {
                let mut config = previous("config.json");
                let mut desktop_profile = config.get("profile").and_then(Value::as_object).cloned().unwrap_or_default();
                for (key, value) in profile {
                    desktop_profile.insert(key, value);
                }
                // An older phone backup has no status line; the desktop needs one.
                desktop_profile.entry("status_message").or_insert_with(|| Value::String("Online".into()));
                config.insert("profile".into(), Value::Object(desktop_profile));
                // Every desktop contact needs a name and an address; the
                // phone always writes both, but a missing one would make
                // the whole config fail to load.
                let contacts: Vec<Value> = contacts
                    .into_iter()
                    .map(|mut c| {
                        let o = c.as_object_mut().expect("filtered to objects");
                        for key in ["name", "address"] {
                            o.entry(key).or_insert_with(|| Value::String(String::new()));
                        }
                        c
                    })
                    .collect();
                config.insert("contacts".into(), Value::Array(contacts));
                write_file(dir, "config.json", &serde_json::to_vec_pretty(&config)?)?;
            }
        }
    }
    Ok(Applied { set_aside: moved.then_some(aside), display_name: bundle.display_name(), contacts: bundle.contact_count() })
}

/// Replaces only the identity, from a recovery phrase or shares: the old
/// `identity.key` is moved aside (see the module docs), contacts and
/// settings stay. Returns where the old key went, if there was one.
pub fn replace_identity(dir: &Path, identity: &IdentityKeyPair) -> std::io::Result<Option<PathBuf>> {
    let path = antiphon_crypto::store::identity_path(dir);
    let mut aside = None;
    if path.exists() {
        let folder = aside_folder(dir);
        std::fs::create_dir_all(&folder)?;
        std::fs::rename(&path, folder.join("identity.key"))?;
        aside = Some(folder);
    }
    antiphon_crypto::store::save(&path, identity)?;
    Ok(aside)
}

/// Rebuilds an identity from what was typed into a restore box: either the
/// 24-word recovery phrase, or enough Shamir shares (one per line, as
/// [`antiphon_crypto::shamir::Share::to_text`] writes them).
pub fn identity_from_words(text: &str) -> Result<IdentityKeyPair, String> {
    let text = text.trim();
    let words: Vec<&str> = text.split_whitespace().collect();
    // A share starts with its "need-index-tag" header and a phrase never
    // has a dash, so the headers mark where each share begins -- whether
    // they were pasted one per line or the box flattened them onto one.
    if words.first().is_some_and(|w| w.contains('-')) {
        let mut shares_text: Vec<String> = Vec::new();
        for word in words {
            match shares_text.last_mut() {
                Some(share) if !word.contains('-') => {
                    share.push(' ');
                    share.push_str(word);
                }
                _ => shares_text.push(word.to_string()),
            }
        }
        let shares = shares_text
            .iter()
            .map(|l| antiphon_crypto::shamir::Share::parse(l))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("One of those shares is not valid: {e}"))?;
        let seed = antiphon_crypto::shamir::combine(&shares).map_err(|e| format!("Those shares don't rebuild an identity: {e}"))?;
        return Ok(IdentityKeyPair::from_master_seed(seed));
    }
    let seed = antiphon_crypto::mnemonic::seed_from_phrase(text).map_err(|e| format!("That phrase is not valid: {e}"))?;
    Ok(IdentityKeyPair::from_master_seed(seed))
}

/// A `replaced-<unix seconds>` folder under `dir` that does not exist yet:
/// two restores in the same second get `-2`, `-3` and so on, so the second
/// never lands its files on top of the first's.
fn aside_folder(dir: &Path) -> PathBuf {
    let base = format!("replaced-{}", now_secs());
    let mut folder = dir.join(&base);
    let mut n = 2;
    while folder.exists() {
        folder = dir.join(format!("{base}-{n}"));
        n += 1;
    }
    folder
}

/// The profile fields of `bundle` both apps understand, keyed as the phone
/// stores them (and as the desktop's `profile` object does).
fn portable_profile(bundle: &Bundle) -> Map<String, Value> {
    let source = match bundle.platform {
        Platform::Desktop => bundle.json("config.json").and_then(|c| c.get("profile").cloned()),
        Platform::Phone => bundle.json("settings.json"),
    };
    let Some(Value::Object(source)) = source else { return Map::new() };
    let mut out = Map::new();
    for key in std::iter::once("display_name").chain(PROFILE_KEYS) {
        if let Some(value) = source.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    out
}

/// Adds the file `dir/name` to `files` if it exists.
fn read_into(dir: &Path, name: &str, files: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    match std::fs::read(dir.join(name)) {
        Ok(data) => {
            files.push((name.to_string(), data));
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Adds every file under the folder `dir/name` to `files`, recursively.
/// Symbolic links are skipped: a link could point anywhere on the disk,
/// and a backup should hold the account, not whatever it points at.
fn walk_into(dir: &Path, name: &str, files: &mut Vec<(String, Vec<u8>)>) -> std::io::Result<()> {
    let root = dir.join(name);
    if !root.is_dir() {
        return Ok(());
    }
    let mut stack = vec![(root, name.to_string())];
    while let Some((folder, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&folder)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            // Names that are not valid UTF-8 cannot be written into the
            // manifest faithfully; the app never makes them.
            let Some(file_name) = entry.file_name().to_str().map(str::to_string) else { continue };
            let child = format!("{rel}/{file_name}");
            if kind.is_dir() {
                stack.push((entry.path(), child));
            } else if kind.is_file() {
                files.push((child, std::fs::read(entry.path())?));
            }
        }
    }
    Ok(())
}

/// Writes one bundle file under `dir`, creating its folders.
fn write_file(dir: &Path, rel: &str, data: &[u8]) -> std::io::Result<()> {
    let path = dir.join(safe_path(rel).ok_or_else(|| std::io::Error::other(format!("unsafe path in backup: {rel}")))?);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)
}

/// `rel` as a relative path that stays inside the folder it is joined to:
/// plain names only, no `..`, no root, no drive letter. `None` otherwise.
fn safe_path(rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.contains('\\') {
        return None;
    }
    let path = PathBuf::from(rel);
    path.components().all(|c| matches!(c, Component::Normal(_))).then_some(path)
}

/// Lower-case hex.
fn hex(bytes: &[u8]) -> String {
    antiphon_crypto::hex::encode(bytes)
}

/// Exactly 64 hex digits as 32 bytes.
fn unhex32(text: &str) -> Option<[u8; 32]> {
    antiphon_crypto::hex::decode_array(text)
}

/// The time now, unix seconds.
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty data folder for one test.
    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stoop-account-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A desktop data folder with a profile, one contact and some history.
    fn desktop_account(dir: &Path) -> (IdentityKeyPair, BackupKey) {
        let config = serde_json::json!({
            "profile": {"display_name": "Robin", "status_message": "Around", "bio": "Hi", "color": "#ff8800", "avatar": "AAAA"},
            "contacts": [{"name": "Sam", "address": "10.0.0.2:4000", "identity_key": "ab".repeat(32), "verified": true, "volume_db": 3.0}],
        });
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&config).unwrap()).unwrap();
        std::fs::create_dir_all(dir.join("history")).unwrap();
        std::fs::write(dir.join("history").join("abab.hist"), b"sealed history").unwrap();
        (IdentityKeyPair::generate(), BackupKey::generate())
    }

    #[test]
    fn encode_decode_round_trip() {
        let dir = temp("roundtrip");
        let (id, key) = desktop_account(&dir);
        let bundle = gather(&dir, Platform::Desktop, &id, &key, true).unwrap();
        let back = Bundle::decode(&bundle.encode()).unwrap();
        assert_eq!(back.identity().public().identity_key, id.public().identity_key);
        assert_eq!(back.files, bundle.files);
        assert_eq!(back.display_name().as_deref(), Some("Robin"));
        assert_eq!(back.contact_count(), 1);
        assert!(back.has_history);
    }

    #[test]
    fn history_is_left_out_unless_asked() {
        let dir = temp("nohistory");
        let (id, key) = desktop_account(&dir);
        let bundle = gather(&dir, Platform::Desktop, &id, &key, false).unwrap();
        assert!(bundle.files.iter().all(|(p, _)| !p.starts_with("history/")));
    }

    #[test]
    fn same_platform_restore_puts_everything_back_and_sets_the_old_aside() {
        let from = temp("same-from");
        let (id, key) = desktop_account(&from);
        let bundle = Bundle::decode(&gather(&from, Platform::Desktop, &id, &key, true).unwrap().encode()).unwrap();

        let to = temp("same-to");
        std::fs::write(to.join("config.json"), b"{\"old\": true}").unwrap();
        let applied = apply(&to, Platform::Desktop, &bundle).unwrap();

        assert_eq!(std::fs::read(to.join("config.json")).unwrap(), std::fs::read(from.join("config.json")).unwrap());
        assert_eq!(std::fs::read(to.join("history/abab.hist")).unwrap(), b"sealed history");
        let aside = applied.set_aside.expect("the old config was moved aside");
        assert_eq!(std::fs::read(aside.join("config.json")).unwrap(), b"{\"old\": true}");
        let restored = antiphon_crypto::store::load_or_create(&antiphon_crypto::store::identity_path(&to)).unwrap();
        assert_eq!(restored.public().identity_key, id.public().identity_key);
        assert_eq!(history::load_or_create_backup_key(&to).unwrap(), key);
    }

    #[test]
    fn desktop_to_phone_translates_contacts_and_profile() {
        let from = temp("d2p-from");
        let (id, key) = desktop_account(&from);
        let bundle = gather(&from, Platform::Desktop, &id, &key, true).unwrap();

        let to = temp("d2p-to");
        std::fs::write(to.join("settings.json"), b"{\"listen_port\": 5555, \"display_name\": \"Old\"}").unwrap();
        let applied = apply(&to, Platform::Phone, &bundle).unwrap();
        assert_eq!(applied.contacts, 1);

        let settings: Value = serde_json::from_slice(&std::fs::read(to.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["display_name"], "Robin");
        assert_eq!(settings["bio"], "Hi");
        assert_eq!(settings["listen_port"], 5555, "the phone's own settings stay");
        let contacts: Value = serde_json::from_slice(&std::fs::read(to.join("contacts.json")).unwrap()).unwrap();
        assert_eq!(contacts[0]["name"], "Sam");
        assert_eq!(contacts[0]["verified"], true);
        assert!(to.join("history/abab.hist").exists(), "history is the same format on both");
        assert!(!to.join("config.json").exists(), "the desktop's file does not land on the phone");
    }

    #[test]
    fn phone_to_desktop_fills_in_what_the_desktop_needs() {
        let from = temp("p2d-from");
        std::fs::write(from.join("settings.json"), b"{\"display_name\": \"Pat\", \"color\": \"#00ff00\"}").unwrap();
        std::fs::write(from.join("contacts.json"), b"[{\"name\": \"Lee\", \"identity_key\": \"\"}]").unwrap();
        let bundle = gather(&from, Platform::Phone, &IdentityKeyPair::generate(), &BackupKey::generate(), false).unwrap();

        let to = temp("p2d-to");
        apply(&to, Platform::Desktop, &bundle).unwrap();
        let config: Value = serde_json::from_slice(&std::fs::read(to.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["profile"]["display_name"], "Pat");
        assert_eq!(config["profile"]["color"], "#00ff00");
        assert_eq!(config["profile"]["status_message"], "Online");
        assert_eq!(config["contacts"][0]["address"], "", "a missing address is filled in so the config loads");
    }

    #[test]
    fn hostile_paths_are_refused() {
        for bad in ["../evil", "/etc/passwd", "a/../../b", "", "C:\\x", "a\\..\\b"] {
            assert!(safe_path(bad).is_none(), "{bad} should be refused");
        }
        assert!(safe_path("history/abc.hist").is_some());
    }

    #[test]
    fn truncated_or_padded_bundles_are_refused() {
        let dir = temp("damaged");
        let (id, key) = desktop_account(&dir);
        let bytes = gather(&dir, Platform::Desktop, &id, &key, true).unwrap().encode();
        assert!(Bundle::decode(&bytes[..bytes.len() - 1]).is_err());
        let mut longer = bytes.to_vec();
        longer.push(0);
        assert!(Bundle::decode(&longer).is_err());
    }

    #[test]
    fn restore_box_takes_a_phrase_or_shares() {
        let id = IdentityKeyPair::generate();
        let phrase = antiphon_crypto::mnemonic::phrase_from_seed(&id.master_seed());
        assert_eq!(identity_from_words(&phrase).unwrap().public().identity_key, id.public().identity_key);

        let shares = antiphon_crypto::shamir::split(&id.master_seed(), 2, 3).unwrap();
        let typed = format!("{}\n{}", shares[0].to_text(), shares[2].to_text());
        assert_eq!(identity_from_words(&typed).unwrap().public().identity_key, id.public().identity_key);
        let flattened = format!("{} {}", shares[1].to_text(), shares[0].to_text());
        assert_eq!(identity_from_words(&flattened).unwrap().public().identity_key, id.public().identity_key);
        assert!(identity_from_words("not a phrase").is_err());
    }

    #[test]
    fn replacing_the_identity_keeps_the_old_one() {
        let dir = temp("replace-id");
        let old = IdentityKeyPair::generate();
        antiphon_crypto::store::save(&antiphon_crypto::store::identity_path(&dir), &old).unwrap();
        let aside = replace_identity(&dir, &IdentityKeyPair::generate()).unwrap().unwrap();
        let kept = antiphon_crypto::store::load_or_create(&aside.join("identity.key")).unwrap();
        assert_eq!(kept.public().identity_key, old.public().identity_key);
    }
}
