//! Past calls: who, when, how long, and how each ended.
//!
//! Sealed with the same backup key as the chat history. Who you talk to
//! and when is as private as what you say, and a phone's private storage
//! is copied by backups the user may not think about.

use std::path::{Path, PathBuf};

use antiphon_crypto::BackupKey;
use serde::{Deserialize, Serialize};

use crate::call::CallSummary;
use crate::contacts::hex;

/// Enough to scroll back a few months; the whole list is rewritten on
/// every call, so it is bounded.
const MAX_RECORDS: usize = 500;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CallRecord {
    pub outgoing: bool,
    /// What was dialled, or the address an incoming call came from.
    pub remote: String,
    /// Who answered, hex, or empty when the call never connected.
    #[serde(default)]
    pub peer_key: String,
    pub started_secs: u64,
    /// How long it was connected, or `None` if it never was.
    #[serde(default)]
    pub duration_secs: Option<u64>,
    /// The line shown when it ended.
    #[serde(default)]
    pub outcome: String,
}

impl From<&CallSummary> for CallRecord {
    fn from(summary: &CallSummary) -> Self {
        Self {
            outgoing: summary.outgoing,
            remote: summary.remote.clone(),
            peer_key: summary.peer_key.as_ref().map(hex).unwrap_or_default(),
            started_secs: summary.started_secs,
            duration_secs: summary.connected_secs.map(|at| summary.ended_secs.saturating_sub(at)),
            outcome: summary.message.clone(),
        }
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("calls.log")
}

/// Newest first. A log that will not open reads as empty rather than as
/// an error the user can do nothing about, as with chat history.
pub fn load(dir: &Path, key: &BackupKey) -> Vec<CallRecord> {
    let Ok(sealed) = std::fs::read(path(dir)) else { return Vec::new() };
    let Ok(plain) = key.open_content(&sealed) else { return Vec::new() };
    serde_json::from_slice(&plain).unwrap_or_default()
}

pub fn save(dir: &Path, key: &BackupKey, log: &[CallRecord]) {
    let result = serde_json::to_vec(log)
        .map_err(|e| e.to_string())
        .and_then(|plain| key.seal_content(&plain).map_err(|e| e.to_string()))
        .and_then(|sealed| std::fs::write(path(dir), sealed).map_err(|e| e.to_string()));
    if let Err(e) = result {
        log::warn!("could not save the call history: {e}");
    }
}

/// Adds a call at the top, dropping the oldest past the cap.
pub fn record(log: &mut Vec<CallRecord>, record: CallRecord) {
    log.insert(0, record);
    log.truncate(MAX_RECORDS);
}
