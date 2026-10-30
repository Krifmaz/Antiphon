//! What the app remembers between launches, as JSON in the app's private
//! storage. Same `ChainConfig` the desktop presets use, so a preset file
//! means the same thing on both.

use std::path::{Path, PathBuf};

use antiphon_audio::ChainConfig;
use serde::{Deserialize, Serialize};

/// Everything the phone keeps between launches. Missing fields take
/// their defaults, so a settings file written by an older version still
/// loads.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// The UDP port incoming calls are listened for on.
    pub listen_port: u16,
    /// The address last dialled, so the call screen opens with it filled in.
    pub last_remote: String,
    /// Push-to-talk is the default, as on desktop: a phone held to the
    /// ear in a busy room should not transmit until asked to.
    pub open_mic: bool,
    /// Play the call through the loudspeaker instead of the earpiece.
    pub speaker: bool,
    /// The microphone processing chain, in the desktop's preset format.
    pub chain: ChainConfig,
    /// The name sent to the people you call.
    pub display_name: String,
    /// The status line sent with it ("Around this evening").
    pub status_message: String,
    /// A line or two about you, sent to the people you call.
    pub bio: String,
    /// Your pronouns ("she/her"), sent with your picture and About line;
    /// empty for none.
    pub pronouns: String,
    /// The cards on your own profile page, in order, a hidden one written
    /// with a leading `-` (`antiphon_ui::ProfileLayout`). Empty is the
    /// standard arrangement. Arranges this phone's page only; never sent.
    pub page_blocks: Vec<String>,
    /// Where your invite's QR code sits on your profile page: `""` (not
    /// shown), `"banner"`, `"name"` or `"card"` (`antiphon_ui::QrPlace`).
    pub page_qr: String,
    /// Your avatar colour's id (`antiphon_ui::PROFILE_COLORS`); empty for
    /// the automatic one.
    pub color: String,
    /// Your profile picture: a small JPEG, base64, as
    /// `profile::avatar_from_image` makes it. Empty for none.
    pub avatar: String,
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
    /// Bytes counted in `cellular_month`; together they make the
    /// `CellularMeter` the transfer rules read.
    pub cellular_used_bytes: u64,
    /// The interface's theme id (`stoop`, `dark`, `oled`, `light`, `system`).
    pub theme: String,
    /// The interface's accent id, or `custom`.
    pub accent: String,
    /// The custom accent as `#rrggbb`, used when `accent` is `custom`.
    pub custom_accent: String,
    /// The reading font's id: `inter` (the default) or `opendyslexic`.
    pub font: String,
    /// Dialogs and panels appear at once instead of sliding.
    pub reduced_motion: bool,
    /// Mailbox nodes (`host[:port]`) this phone keeps in touch with the
    /// account's other devices through: notes to self, a removed device,
    /// a new mail key. Filled in from a computer's delivery routes when the
    /// account moves here from one.
    pub device_mailboxes: Vec<String>,
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
            status_message: String::new(),
            bio: String::new(),
            pronouns: String::new(),
            page_blocks: Vec::new(),
            page_qr: String::new(),
            color: String::new(),
            avatar: String::new(),
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
            theme: "stoop".into(),
            accent: "logo".into(),
            custom_accent: String::new(),
            font: "inter".into(),
            reduced_motion: false,
            device_mailboxes: Vec::new(),
        }
    }
}

/// The settings file in the app's private storage.
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

    /// Writes the settings as pretty JSON, so a user or a support request
    /// can read the file. A failure is logged and otherwise ignored: the
    /// app keeps working with what is in memory.
    pub fn save(&self, dir: &Path) {
        let result = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|json| std::fs::write(path(dir), json));
        if let Err(e) = result {
            log::warn!("could not save settings: {e}");
        }
    }
}
