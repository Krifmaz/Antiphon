//! Data export (spec §13: "take everything with you, in an open format").
//!
//! Two things make an export honest, and both are easy to get wrong:
//!
//! **It has to be complete.** An export that quietly omits your message
//! history is not an export, it is a settings backup with a misleading
//! name. So this walks the same directories the app actually reads and
//! says what it found -- and, importantly, what it deliberately left out.
//!
//! **It has to be readable without this app.** Everything lands as JSON
//! and WAV in a plain directory tree, not a bespoke archive. If the
//! project disappears tomorrow, an export is still a folder of files
//! anyone can open. That is the whole point of "open format": not that a
//! parser exists, but that you do not need one.
//!
//! # What is deliberately not exported
//!
//! - **The identity key and the backup key.** They are the two secrets
//!   that *are* you, and writing them into a folder people email around
//!   or drop in cloud storage is how identities leak. The identity is
//!   recoverable from its 24-word phrase, which is the mechanism designed
//!   for exactly this, and it does not sit in a file that can be
//!   accidentally shared.
//! - **Message history in encrypted form.** History is decrypted on the
//!   way out, because an export you cannot read is not an export. That
//!   makes the exported folder sensitive in a way the config directory is
//!   not, which the caller is expected to say out loud.

use std::path::Path;

use serde::Serialize;

/// A summary of what an export contained, for telling the user plainly.
#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct ExportSummary {
    pub settings: bool,
    pub contacts: usize,
    pub conversations: usize,
    pub messages: usize,
    pub sound_clips: usize,
    pub stored_files: usize,
    /// Things present in the config directory that were left behind, and
    /// why -- surfaced rather than silently skipped.
    pub omitted: Vec<String>,
}

/// The manifest written alongside an export.
#[derive(Serialize)]
struct Manifest<'a> {
    /// Bumped if the layout ever changes, so a future importer can tell.
    format_version: u32,
    exported_at_secs: u64,
    app: &'a str,
    summary: &'a ExportSummary,
    /// Written into the file itself rather than only shown in the UI: an
    /// export gets copied around long after whatever dialog produced it.
    notes: Vec<&'a str>,
}

const FORMAT_VERSION: u32 = 1;

/// Writes a complete, readable export into `dest`.
///
/// `decrypt_history` is supplied by the caller so this module never needs
/// the backup key itself -- it asks for plaintext and writes it.
pub fn export_all(
    config_dir: &Path,
    dest: &Path,
    decrypt_history: impl Fn(&str) -> Option<Vec<crate::history::StoredMessage>>,
) -> std::io::Result<ExportSummary> {
    std::fs::create_dir_all(dest)?;
    let mut summary = ExportSummary::default();

    // Settings, verbatim -- it is already JSON, and rewriting it here
    // would mean two definitions of the format drifting apart.
    let settings_src = config_dir.join("config.json");
    if settings_src.exists() {
        std::fs::copy(&settings_src, dest.join("settings.json"))?;
        summary.settings = true;

        if let Ok(bytes) = std::fs::read(&settings_src) {
            if let Ok(config) = serde_json::from_slice::<crate::config::AppConfig>(&bytes) {
                summary.contacts = config.contacts.len();
                summary.sound_clips = config.sound_clips.len();

                // History, decrypted, one file per conversation named by
                // the petname the user actually recognises rather than by
                // a 64-character key.
                let convo_dir = dest.join("conversations");
                for contact in &config.contacts {
                    if contact.identity_key.is_empty() {
                        continue;
                    }
                    let Some(messages) = decrypt_history(&contact.identity_key) else { continue };
                    if messages.is_empty() {
                        continue;
                    }
                    std::fs::create_dir_all(&convo_dir)?;
                    let name = safe_name(&contact.name, &contact.identity_key);
                    let json = serde_json::to_vec_pretty(&messages)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                    std::fs::write(convo_dir.join(format!("{name}.json")), json)?;
                    summary.conversations += 1;
                    summary.messages += messages.len();
                }
            }
        }
    }

    // Soundboard clips and received files, copied as-is: they are already
    // WAV and whatever the sender sent.
    summary.sound_clips = copy_dir(&config_dir.join("sounds"), &dest.join("sounds"))?;
    summary.stored_files = copy_dir(&config_dir.join("files"), &dest.join("files"))?;

    if config_dir.join("identity.key").exists() {
        summary.omitted.push(
            "identity.key -- your identity, left out on purpose. Restore it with your \
             24-word recovery phrase instead; a key sitting in an exported folder is how \
             identities get leaked."
                .to_string(),
        );
    }
    if config_dir.join("backup.key").exists() {
        summary.omitted.push(
            "backup.key -- the key your message history is encrypted with. History is \
             exported in readable form instead, so the key itself is not needed."
                .to_string(),
        );
    }
    if config_dir.join("partial").exists() {
        summary
            .omitted
            .push("partial/ -- half-finished transfers, which resume rather than travel.".to_string());
    }

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        exported_at_secs: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        app: "antiphon",
        summary: &summary,
        notes: vec![
            "Everything here is plain JSON and WAV. You do not need this app to read it.",
            "conversations/ holds your messages in readable form. Treat this folder as \
             sensitive: it is not encrypted, unlike the copy the app keeps.",
            "Your identity key is not included. Use your 24-word recovery phrase to restore \
             your identity on a new device.",
        ],
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(dest.join("manifest.json"), manifest_json)?;

    Ok(summary)
}

/// Copies a directory's files, returning how many were copied. A missing
/// source is zero, not an error -- not having any soundboard clips is a
/// normal state, not a failed export.
fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<usize> {
    let Ok(entries) = std::fs::read_dir(src) else { return Ok(0) };
    let mut count = 0;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        std::fs::create_dir_all(dest)?;
        std::fs::copy(entry.path(), dest.join(entry.file_name()))?;
        count += 1;
    }
    Ok(count)
}

/// A filename for a conversation: the petname where it is usable, falling
/// back to a short key fragment.
///
/// Names come from the user, so they become a filename only after being
/// stripped -- and two contacts called "Mum" must not overwrite each
/// other, hence the key fragment on the end.
fn safe_name(petname: &str, identity_key: &str) -> String {
    let cleaned: String = petname
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ' ')
        .take(40)
        .collect();
    let cleaned = cleaned.trim();
    let short: String = identity_key.chars().take(8).collect();
    if cleaned.is_empty() {
        format!("contact-{short}")
    } else {
        format!("{cleaned}-{short}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::StoredMessage;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-export-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn message(text: &str) -> StoredMessage {
        StoredMessage {
            id: 1,
            from_me: true,
            text: text.to_string(),
            at_secs: 1_700_000_000,
            edited: false,
            deleted: false,
            pinned: false,
            expires_at_secs: None,
        }
    }

    fn config_with_contact(dir: &Path, name: &str, key: &str) {
        let mut config = crate::config::AppConfig::default();
        config.contacts.push(crate::config::Contact {
            name: name.to_string(),
            address: "127.0.0.1:6000".to_string(),
            is_server: false,
            identity_key: key.to_string(),
            verified: true,
            first_seen_secs: 1,
            volume_db: 0.0,
            muted: false,
            expire_after_secs: None,
            ring_rule: 0,
            eq_bands: Vec::new(),
            note: String::new(),
            ..Default::default()
        });
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec_pretty(&config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn an_export_contains_settings_and_readable_conversations() {
        let src = temp_dir("full-src");
        let dest = temp_dir("full-dest");
        let key = "ab".repeat(32);
        config_with_contact(&src, "Alice", &key);

        let summary =
            export_all(&src, &dest, |_| Some(vec![message("hello"), message("again")])).unwrap();

        assert!(summary.settings);
        assert_eq!(summary.contacts, 1);
        assert_eq!(summary.conversations, 1);
        assert_eq!(summary.messages, 2);
        assert!(dest.join("settings.json").exists());
        assert!(dest.join("manifest.json").exists());
    }

    #[test]
    fn exported_messages_are_readable_without_the_app() {
        let src = temp_dir("readable-src");
        let dest = temp_dir("readable-dest");
        let key = "cd".repeat(32);
        config_with_contact(&src, "Alice", &key);
        export_all(&src, &dest, |_| Some(vec![message("a secret plan")])).unwrap();

        // The whole promise of "open format": the text is right there in
        // a JSON file, no parser and no key required.
        let files: Vec<_> = std::fs::read_dir(dest.join("conversations")).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let content = std::fs::read_to_string(files[0].path()).unwrap();
        assert!(content.contains("a secret plan"), "message text was not readable");
    }

    #[test]
    fn secrets_are_left_out_and_said_so() {
        let src = temp_dir("secrets-src");
        let dest = temp_dir("secrets-dest");
        config_with_contact(&src, "Alice", &"ef".repeat(32));
        std::fs::write(src.join("identity.key"), [0u8; 32]).unwrap();
        std::fs::write(src.join("backup.key"), [0u8; 32]).unwrap();

        let summary = export_all(&src, &dest, |_| None).unwrap();

        // Not merely absent -- absent *and explained*, in the export
        // itself, because an export outlives the dialog that made it.
        assert!(!dest.join("identity.key").exists(), "identity key was exported");
        assert!(!dest.join("backup.key").exists(), "backup key was exported");
        assert_eq!(summary.omitted.len(), 2);
        assert!(summary.omitted.iter().any(|o| o.contains("recovery phrase")));

        let manifest = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(manifest.contains("recovery phrase"));
    }

    #[test]
    fn two_contacts_with_the_same_name_do_not_overwrite_each_other() {
        let a = safe_name("Mum", &"11".repeat(32));
        let b = safe_name("Mum", &"22".repeat(32));
        assert_ne!(a, b);
    }

    #[test]
    fn a_petname_cannot_escape_the_export_directory() {
        // Petnames are user input and become filenames.
        let name = safe_name("../../etc/passwd", &"33".repeat(32));
        assert!(!name.contains(".."), "{name}");
        assert!(!name.contains('/'), "{name}");
    }

    #[test]
    fn a_contact_with_no_name_still_gets_a_usable_filename() {
        let name = safe_name("", &"44".repeat(32));
        assert!(name.starts_with("contact-"), "{name}");
    }

    #[test]
    fn clips_and_received_files_travel_with_the_export() {
        let src = temp_dir("files-src");
        let dest = temp_dir("files-dest");
        config_with_contact(&src, "Alice", &"55".repeat(32));
        std::fs::create_dir_all(src.join("sounds")).unwrap();
        std::fs::write(src.join("sounds/airhorn.wav"), b"RIFF").unwrap();
        std::fs::create_dir_all(src.join("files")).unwrap();
        std::fs::write(src.join("files/abc.bin"), b"data").unwrap();

        let summary = export_all(&src, &dest, |_| None).unwrap();

        assert_eq!(summary.sound_clips, 1);
        assert_eq!(summary.stored_files, 1);
        assert!(dest.join("sounds/airhorn.wav").exists());
        assert!(dest.join("files/abc.bin").exists());
    }

    #[test]
    fn exporting_a_fresh_install_succeeds_with_nothing_in_it() {
        let src = temp_dir("empty-src");
        let dest = temp_dir("empty-dest");

        // No config, no history, no clips. Should produce a valid, empty
        // export rather than an error.
        let summary = export_all(&src, &dest, |_| None).unwrap();

        assert!(!summary.settings);
        assert_eq!(summary.messages, 0);
        assert!(dest.join("manifest.json").exists());
    }

    #[test]
    fn a_conversation_with_no_messages_is_not_written() {
        let src = temp_dir("nomsg-src");
        let dest = temp_dir("nomsg-dest");
        config_with_contact(&src, "Alice", &"66".repeat(32));

        let summary = export_all(&src, &dest, |_| Some(Vec::new())).unwrap();

        assert_eq!(summary.conversations, 0);
        assert!(!dest.join("conversations").exists(), "empty conversation file written");
    }

    #[test]
    fn the_manifest_records_the_format_version() {
        let src = temp_dir("version-src");
        let dest = temp_dir("version-dest");
        export_all(&src, &dest, |_| None).unwrap();

        let manifest = std::fs::read_to_string(dest.join("manifest.json")).unwrap();
        assert!(manifest.contains("\"format_version\": 1"), "{manifest}");
    }
}
