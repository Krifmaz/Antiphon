//! Crash reports (spec §13: "crash reporting -- opt-in only, no
//! identifiers").
//!
//! Two constraints from the spec, and both are the kind that get eroded by
//! well-meaning additions later, so they are enforced here rather than
//! promised in a policy:
//!
//! **Opt-in only.** Nothing is written unless the user turned it on. Not
//! "collected but not sent", not "cached until you decide" -- a report
//! that exists on disk is a report that can leak, and the honest reading
//! of opt-in is that opting out means nothing happens.
//!
//! **No identifiers.** A crash report carries the panic message, the
//! location in the source, and the app version. Not the username, not the
//! machine name, not the identity key, not who they were talking to, not
//! any file path from the user's disk -- and paths are the sneaky one,
//! because `C:\Users\Alice\...` in a backtrace is a name whether or not
//! anyone meant it to be.
//!
//! Reports are written to a local folder and **never transmitted**. There
//! is no endpoint to send them to, and adding one would be a decision
//! about where a user's data goes that belongs to the person building the
//! service, not to this module.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A crash, stripped of anything identifying.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CrashReport {
    /// Unix seconds, rounded to the hour.
    ///
    /// A precise timestamp is a correlation handle: paired with anything
    /// else it narrows down who someone is. The hour is enough to tell
    /// whether two reports are the same incident.
    pub at_hour_secs: u64,
    pub app_version: String,
    /// The panic message, with anything path-like scrubbed out.
    pub message: String,
    /// `file:line` from the panic location, source-relative.
    pub location: String,
}

pub fn reports_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("crash-reports")
}

/// Strips things that identify a person or a machine.
///
/// Deliberately aggressive: it is far better to redact a harmless word
/// than to ship someone's home directory to a text file they might attach
/// to a bug report.
pub fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut token = String::new();

    let flush = |token: &mut String, out: &mut String| {
        if token.is_empty() {
            return;
        }
        let looks_like_path = token.contains('/')
            || token.contains('\\')
            || (token.len() > 2 && token.as_bytes()[1] == b':');
        // A source path from the build is fine and useful; anything else
        // that looks like a path is somebody's disk.
        let is_source = token.contains("crates")
            || token.contains("src")
            || token.starts_with("/rustc/");
        if looks_like_path && !is_source {
            out.push_str("<path>");
        } else if token.contains('@') && token.contains('.') {
            out.push_str("<address>");
        } else if token.len() >= 32 && token.chars().all(|c| c.is_ascii_hexdigit()) {
            // A long hex run is a key or a hash -- an identity key is
            // exactly this shape.
            out.push_str("<key>");
        } else {
            out.push_str(token);
        }
        token.clear();
    };

    for ch in text.chars() {
        if ch.is_whitespace() {
            flush(&mut token, &mut out);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

/// Builds a report from a panic.
pub fn build(message: &str, location: &str, app_version: &str, now_secs: u64) -> CrashReport {
    CrashReport {
        at_hour_secs: now_secs - (now_secs % 3600),
        app_version: app_version.to_string(),
        message: scrub(message),
        location: scrub(location),
    }
}

/// Writes a report, but only if crash reporting is switched on.
///
/// Returns the path written, or `None` when reporting is off -- which is
/// the whole point: opting out means nothing reaches the disk.
pub fn save_if_enabled(
    config_dir: &Path,
    enabled: bool,
    report: &CrashReport,
) -> Option<PathBuf> {
    if !enabled {
        return None;
    }
    let dir = reports_dir(config_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("crash-{}.json", report.at_hour_secs));
    let json = serde_json::to_vec_pretty(report).ok()?;
    std::fs::write(&path, json).ok()?;
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn nothing_is_written_when_reporting_is_off() {
        let dir = temp_dir("off");
        let report = build("boom", "src/main.rs:1", "0.1.0", 1_700_000_000);

        // Opting out has to mean nothing happens -- not "collected but not
        // sent". A report on disk is a report that can leak.
        assert!(save_if_enabled(&dir, false, &report).is_none());
        assert!(!reports_dir(&dir).exists());
    }

    #[test]
    fn a_report_is_written_when_reporting_is_on() {
        let dir = temp_dir("on");
        let report = build("boom", "src/main.rs:1", "0.1.0", 1_700_000_000);

        let path = save_if_enabled(&dir, true, &report).unwrap();
        assert!(path.exists());
        let back: CrashReport =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, report);
    }

    #[test]
    fn a_home_directory_is_scrubbed_out() {
        // The sneaky one: a Windows user path contains a real name.
        let scrubbed = scrub(r"failed to open C:\Users\Alice\Documents\notes.txt");
        assert!(!scrubbed.contains("Alice"), "{scrubbed}");
        assert!(scrubbed.contains("<path>"), "{scrubbed}");
    }

    #[test]
    fn a_unix_home_directory_is_scrubbed_too() {
        let scrubbed = scrub("could not read /home/alice/.config/thing");
        assert!(!scrubbed.contains("alice"), "{scrubbed}");
    }

    #[test]
    fn an_identity_key_is_scrubbed() {
        let key = "ab".repeat(32);
        let scrubbed = scrub(&format!("handshake failed for {key}"));

        assert!(!scrubbed.contains(&key), "{scrubbed}");
        assert!(scrubbed.contains("<key>"), "{scrubbed}");
    }

    #[test]
    fn an_email_address_is_scrubbed() {
        let scrubbed = scrub("account alice@example.org failed");
        assert!(!scrubbed.contains("alice@example.org"), "{scrubbed}");
    }

    #[test]
    fn the_source_location_survives_because_it_is_the_useful_part() {
        // A report with no idea where it happened is not worth keeping.
        let scrubbed = scrub("crates/gui/src/main.rs:412");
        assert!(scrubbed.contains("main.rs"), "{scrubbed}");
    }

    #[test]
    fn ordinary_words_are_left_alone() {
        let text = "index out of bounds: the len is 3 but the index is 7";
        assert_eq!(scrub(text), text);
    }

    #[test]
    fn the_timestamp_is_blunted_to_the_hour() {
        // A precise timestamp is a correlation handle.
        let report = build("boom", "x", "0.1.0", 1_700_003_661);
        assert_eq!(report.at_hour_secs % 3600, 0);
        assert!(report.at_hour_secs <= 1_700_003_661);
    }

    #[test]
    fn a_report_carries_no_username_machine_name_or_peer() {
        let report = build(
            r"panic at C:\Users\Dana\thing while talking to ab12cd34ef56ab12cd34ef56ab12cd34",
            "crates/gui/src/main.rs:9",
            "0.1.0",
            1_700_000_000,
        );
        let json = serde_json::to_string(&report).unwrap();

        assert!(!json.contains("Dana"), "{json}");
        assert!(!json.contains("ab12cd34ef56ab12cd34ef56ab12cd34"), "{json}");
        assert!(json.contains("main.rs"), "location should survive: {json}");
    }

    #[test]
    fn scrubbing_is_idempotent() {
        let once = scrub(r"C:\Users\Alice\file");
        assert_eq!(scrub(&once), once);
    }
}
