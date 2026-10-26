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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            listen_port: 5000,
            last_remote: String::new(),
            open_mic: false,
            speaker: false,
            chain: ChainConfig::default_preset(),
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
