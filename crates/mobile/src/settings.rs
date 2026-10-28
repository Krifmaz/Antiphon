//! What the app remembers between launches, as JSON in the app's private
//! storage. Same `ChainConfig` the desktop presets use, so a preset file
//! means the same thing on both.

use std::path::{Path, PathBuf};

use antiphon_audio::ChainConfig;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub listen_port: u16,
    pub last_remote: String,
    /// Push-to-talk is the default, as on desktop: a phone held to the
    /// ear in a busy room should not transmit until asked to.
    pub open_mic: bool,
    pub speaker: bool,
    pub chain: ChainConfig,
    /// The name sent to the people you call.
    pub display_name: String,
    /// Hold file transfers while on mobile data.
    pub wifi_only_transfers: bool,
    /// Megabytes of mobile data file transfers may use per month; 0 for
    /// no limit. Only matters with `wifi_only_transfers` off.
    pub cellular_limit_mb: u32,
    /// On mobile data: no spare audio copy, no thumbnails, slower uploads.
    pub data_saver: bool,
    /// Keep waiting for calls after switching away from the app.
    pub keep_listening_in_background: bool,
    /// Keep listening in the background even with battery saver on.
    pub listen_on_battery_saver: bool,
    /// Mobile data used by transfers this month, kept across restarts so
    /// the limit means a month rather than a session.
    pub cellular_month: u32,
    pub cellular_used_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen_port: 5000,
            last_remote: String::new(),
            open_mic: false,
            speaker: false,
            chain: ChainConfig::default_preset(),
            display_name: String::new(),
            // On by default: a large file quietly eating a data plan is
            // the surprise people do not forgive, and the queue says
            // plainly why a file is waiting.
            wifi_only_transfers: true,
            cellular_limit_mb: 0,
            data_saver: true,
            keep_listening_in_background: true,
            listen_on_battery_saver: false,
            cellular_month: 0,
            cellular_used_bytes: 0,
        }
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("settings.json")
}

impl Settings {
    /// Reads saved settings, or the defaults when there are none or they
    /// do not parse -- unlike the identity, nothing here is worth refusing
    /// to start over.
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(path(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) {
        let result = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(path(dir), json));
        if let Err(e) = result {
            log::warn!("could not save settings: {e}");
        }
    }
}
