use serde::{Deserialize, Serialize};
use antiphon_audio::ChainConfig;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ProfileData {
    pub display_name: String,
    pub status_message: String,
    /// A line or two about you, shown to contacts (`crate::profile`).
    #[serde(default)]
    pub bio: String,
    /// Your avatar colour's id (`antiphon_ui::PROFILE_COLORS`), or empty
    /// for the one picked from your key.
    #[serde(default)]
    pub color: String,
    /// Your picture as sent to contacts: a small JPEG, base64, or empty.
    /// Kept here, in the config file, so a backup of the config carries it.
    #[serde(default)]
    pub avatar: String,
    /// Your pronouns ("she/her"), sent with your card; empty for none.
    #[serde(default)]
    pub pronouns: String,
    /// The cards on your own profile page, in order, a hidden one written
    /// with a leading `-` (`antiphon_ui::ProfileLayout`). Empty is the
    /// standard arrangement. Arranges this device's page only; never sent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_blocks: Vec<String>,
    /// Where your invite's QR code sits on your profile page: `""` (not
    /// shown), `"banner"`, `"name"` or `"card"` (`antiphon_ui::QrPlace`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub page_qr: String,
}

impl Default for ProfileData {
    fn default() -> Self {
        Self {
            display_name: "You".to_string(),
            status_message: "Online".to_string(),
            bio: String::new(),
            color: String::new(),
            avatar: String::new(),
            pronouns: String::new(),
            page_blocks: Vec::new(),
            page_qr: String::new(),
        }
    }
}

/// Remembered window geometry, restored on the next launch regardless of
/// `start_maximized`. `x`/`y` are optional since a fresh install has never
/// placed the window anywhere -- let the OS pick a default position.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct WindowState {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub maximized: bool,
}

/// Settings only the iced app reads. Kept in their own table so the Slint
/// app's fields keep their meaning while both front ends exist.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct StoopPrefs {
    /// Theme id (`stoop`, `dark`, `oled`, `light`, `system`).
    pub theme: String,
    /// Accent id, or `custom`.
    pub accent: String,
    /// The custom accent as `#rrggbb`, used when `accent` is `custom`.
    pub custom_accent: String,
    /// Reading-font id: `inter` (the default) or `opendyslexic`.
    pub font: String,
    /// Open the door (listen for calls) as soon as the app starts.
    pub door_on_start: bool,
    /// Notify about messages that arrive while the window is in the background.
    pub notify_messages: bool,
    /// The port the door listens on.
    pub port: u16,
}

impl Default for StoopPrefs {
    fn default() -> Self {
        StoopPrefs {
            theme: "stoop".into(),
            accent: "logo".into(),
            custom_accent: String::new(),
            font: "inter".into(),
            door_on_start: false,
            notify_messages: true,
            port: 6002,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AppConfig {
    /// The Stoop app's own preferences; absent in older configs.
    #[serde(default)]
    pub stoop: StoopPrefs,
    pub profile: ProfileData,
    pub chain: ChainConfig,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// A [`device_query::Keycode`]'s `Display`/`FromStr` string, e.g. "Space".
    pub ptt_key: String,
    /// 0 push-to-talk, 1 voice activation, 2 open mic. Replaced an earlier
    /// `voice_activation` boolean, which couldn't express open mic; an old
    /// config carrying that boolean simply falls back to push-to-talk.
    #[serde(default)]
    pub transmit_mode: i32,
    /// Optional shortcut keys, "" when unbound. Same `Keycode` string form
    /// as `ptt_key`.
    #[serde(default)]
    pub mute_key: String,
    #[serde(default)]
    pub deafen_key: String,
    #[serde(default)]
    pub hangup_key: String,
    #[serde(default)]
    pub replay_key: String,
    /// Linear peak amplitude (0..1) the mic must cross to open in VA mode.
    pub va_threshold: f32,
    /// Push-to-talk latches on press instead of transmitting only while
    /// held (spec section 1 Utility).
    #[serde(default)]
    pub ptt_toggle: bool,
    /// Mute / deafen only while their key is held, instead of toggling
    /// (spec §1: "PTT or mute/deafen switched to toggle behavior").
    #[serde(default)]
    pub mute_hold: bool,
    /// The relay this machine listens at, `host:port`; empty for none
    /// (spec §3). Self-hosted or a community's -- the app ships with no
    /// default, so nothing is relayed through anyone you didn't choose.
    #[serde(default)]
    pub relay_server: String,
    /// The secret behind the relay part of your invite link, hex. Made on
    /// first use; replacing it revokes every link handed out before.
    #[serde(default)]
    pub relay_invite_secret: String,
    /// Relays to fall back on when `relay_server` is empty or down, tried
    /// in order (spec §3 "Project-hosted relays exist as fallback"). See
    /// `crate::fallback_relay`.
    #[serde(default)]
    pub fallback_relays: Vec<String>,
    /// Whether the project's own relays are tried after yours.
    #[serde(default = "default_true")]
    pub use_project_relays: bool,
    /// Let contacts with no relay of their own listen at `relay_server`,
    /// so they can be rung too (spec §8 "Server-side opt-in": off by
    /// default). Said in invite links and at the start of every call.
    #[serde(default)]
    pub lend_relay: bool,
    /// Relays that relays you used said they peer with, kept so they are
    /// there to fall back on when those relays are down. Learned, never
    /// typed; see `crate::fallback_relay`.
    #[serde(default)]
    pub mesh_relays: Vec<String>,
    /// Anonymity mode (spec §3): relays to route calls through, `host:port`
    /// each, in order. Two or more turn it on. See `crate::anonymity`.
    #[serde(default)]
    pub anonymity_hops: Vec<String>,
    /// Route calls through `anonymity_hops`.
    #[serde(default)]
    pub anonymity_mode: bool,
    /// Your delivery routes (spec §8), in order -- your inbox
    /// configuration, published to contacts. See `crate::delivery`.
    #[serde(default = "crate::delivery::default_routes")]
    pub routes: Vec<crate::delivery::RouteConfig>,
    /// Fan out: leave each message at every node instead of the first
    /// that takes it.
    #[serde(default)]
    pub route_fan_out: bool,
    /// Strict mode: Direct only, nothing ever stored anywhere.
    #[serde(default)]
    pub strict_mode: bool,
    /// Bumped whenever the route list changes, so contacts replace the
    /// old one and can't be rolled back to it.
    #[serde(default)]
    pub route_sequence: u64,
    /// This device holds mail for your other devices (spec §8 "My other
    /// devices"). Off by default.
    #[serde(default)]
    pub hold_mail_for_devices: bool,
    #[serde(default = "default_hold_mail_port")]
    pub hold_mail_port: u16,
    /// On-device live captions in the call view (spec §7). Off by
    /// default; nothing is downloaded until the user asks.
    #[serde(default)]
    pub captions_enabled: bool,
    /// Caption your own voice as well as everyone else's.
    #[serde(default)]
    pub captions_own_voice: bool,
    /// Keep sealed copies of files you share or download and hand them
    /// out (spec §5 "Seeding"). Off by default.
    #[serde(default)]
    pub seeding: bool,
    /// Seed without ever showing your own address (spec §5 "Anonymous
    /// seeding through relays"). Instead of answering from this device,
    /// sealed copies are handed to your pin nodes over an onion circuit
    /// through `anonymity_hops`, and those nodes do the serving; your own
    /// downloads go through circuits too. Off by default. Needs at least
    /// two hop nodes and one pin node; without them nothing is sent,
    /// rather than sent in the open.
    #[serde(default)]
    pub anonymous_seeding: bool,
    /// Who seeded files go to: 0 the conversation it was shared in,
    /// 1 your contacts, 2 anyone with the link.
    #[serde(default)]
    pub seed_scope: i32,
    #[serde(default = "default_seed_port")]
    pub seed_port: u16,
    /// Nodes that keep a sealed copy of files you share (spec §5
    /// "Pinning"), `host[:port]`.
    #[serde(default)]
    pub pin_nodes: Vec<String>,
    /// The code a members-only pin node asks for.
    #[serde(default)]
    pub pin_code: String,
    /// Bring your own VPN (spec §6). Off by default. The configs
    /// themselves -- private keys -- are in their own sealed file (see
    /// `vpn.rs`), never here: settings export copies this file verbatim.
    #[serde(default)]
    pub vpn_enabled: bool,
    /// Id of the stored config to use.
    #[serde(default)]
    pub vpn_active: String,
    /// Which traffic goes through the VPN. Calls default to direct,
    /// because a tunnel adds latency and voice notices it first; files
    /// and offline messages default to the tunnel.
    #[serde(default)]
    pub vpn_route_voice: bool,
    #[serde(default = "default_true")]
    pub vpn_route_files: bool,
    #[serde(default = "default_true")]
    pub vpn_route_messages: bool,
    /// When the VPN is down, refuse to send the routed traffic rather
    /// than sending it from your real address. Off by default.
    #[serde(default)]
    pub vpn_kill_switch: bool,
    /// Publish where you are to the DHT and look contacts up there when
    /// their address changes (spec §4). Off by default.
    #[serde(default)]
    pub dht_enabled: bool,
    /// DHT nodes to join through, `host[:port]`. Empty by default: the
    /// app ships with none, so nothing is asked of anyone you didn't
    /// choose.
    #[serde(default)]
    pub dht_bootstrap: Vec<String>,
    /// Community directories to browse (spec §4 "Opt-in directories"),
    /// `host[:port]`. Empty by default.
    #[serde(default)]
    pub directories: Vec<String>,
    /// Each directory's key, `address=hex`, pinned the first time it
    /// answered.
    #[serde(default)]
    pub directory_pins: Vec<String>,
    #[serde(default)]
    pub deafen_hold: bool,
    /// Milliseconds the mic stays open after the push-to-talk key comes up,
    /// so the tail of the last word is not clipped.
    #[serde(default)]
    pub ptt_release_ms: f32,
    /// 0 = follow system, 1 = light, 2 = dark. `#[serde(default)]` on the
    /// fields below so a config saved before this setting existed still
    /// loads instead of falling back to defaults wholesale.
    #[serde(default)]
    pub theme_mode: i32,
    #[serde(default = "default_text_scale")]
    pub text_scale: f32,
    #[serde(default)]
    pub reduced_motion: bool,
    /// Start up already listening for an incoming call instead of waiting
    /// for the user to press "Listen" -- the closest honest meaning of
    /// "server mode" for a peer-to-peer call: always-on and reachable,
    /// not a client/server architecture change (there isn't one).
    #[serde(default)]
    pub server_mode: bool,
    /// Force-maximize on launch, regardless of the remembered window size.
    #[serde(default)]
    pub start_maximized: bool,
    #[serde(default)]
    pub window: WindowState,
    /// Master output (playback) volume in dB, applied to the peer's audio.
    #[serde(default)]
    pub output_gain_db: f32,
    /// Whether Settings' collapsible sections start expanded or collapsed --
    /// a single blanket preference (General tab) rather than remembering
    /// every individual section's state.
    #[serde(default = "default_sections_expanded")]
    pub sections_expanded_default: bool,
    /// "#rrggbb". The one user-choosable color; everything else in the UI
    /// derives from the OS/theme palette (see `global Theme` in app.slint).
    #[serde(default = "default_accent_hex")]
    pub accent_hex: String,
    /// A font family name (e.g. "Consolas"), or "" for the system default.
    /// Applied window-wide via `Window.default-font-family`, so it reaches
    /// native widget chrome too, not just this app's own text.
    #[serde(default)]
    pub font_family: String,
    /// Path to a user-imported font file (.ttf/.otf/.ttc). Registering a
    /// font with the text engine is in-memory only and doesn't survive a
    /// restart on its own -- this is what lets startup re-register it so
    /// `font_family` (just a family name string) resolves to something.
    #[serde(default)]
    pub custom_font_path: String,
    /// "#rrggbb" background for Appearance > Theme's "Custom" mode. Only
    /// takes effect when `theme_mode == 3`; System/Light/Dark keep using
    /// the OS-derived palette regardless of what this holds.
    #[serde(default = "default_theme_bg_hex")]
    pub theme_bg_hex: String,
    /// Path to a user-chosen background image, or "" for none. Same
    /// re-registration-on-startup story as `custom_font_path`: the loaded
    /// `slint::Image` isn't itself persisted, just the path to reload it from.
    #[serde(default)]
    pub theme_image_path: String,
    /// 0..1 opacity of the background image over the background color.
    #[serde(default = "default_theme_image_opacity")]
    pub theme_image_opacity: f32,
    /// 0..1 opacity of the whole background layer (color + image) against
    /// whatever's behind the window -- panels/text/buttons stay fully
    /// opaque regardless of this value.
    #[serde(default = "default_theme_opacity")]
    pub theme_opacity: f32,
    /// Display order of each tab's collapsible sections, set via "Edit
    /// layout"'s up/down controls. Sanitized against the canonical id list
    /// on load (see `sanitize_order`) so a stale or hand-edited value can't
    /// make a section silently disappear from the UI.
    #[serde(default = "default_audio_order")]
    pub audio_section_order: Vec<String>,
    #[serde(default = "default_appearance_order")]
    pub appearance_section_order: Vec<String>,
    #[serde(default = "default_profile_order")]
    pub profile_section_order: Vec<String>,
    #[serde(default = "default_call_order")]
    pub call_section_order: Vec<String>,

    /// Whether calling a raw `host:port` is offered at all (Call settings >
    /// Advanced).
    ///
    /// Off by default because a direct connection shows each side the
    /// other's IP address -- spec §3 makes relayed the safe default for
    /// anyone who isn't already a known contact. Turning this on is the
    /// user deciding they understand that trade, which is why it lives
    /// under Advanced rather than next to the call button.
    #[serde(default)]
    pub ip_direct_calling: bool,

    /// Start calls with screen sharing already on.
    ///
    /// Screen share is spec §11 deferred and not implemented, so this is
    /// a stored preference with nothing yet reading it.
    #[serde(default)]
    pub start_with_screen_share: bool,
    /// Start calls with the camera already on. Video is spec §11 deferred
    /// and not implemented; same story as above.
    #[serde(default)]
    pub start_with_camera: bool,
    /// Join calls with everyone else muted on your end, to be un-muted
    /// individually by clicking them.
    #[serde(default)]
    pub mute_others_by_default: bool,
    /// Join calls with your own mic muted.
    #[serde(default)]
    pub mute_self_on_join: bool,
    /// Settings sidebar width in logical pixels -- dragged via the resize
    /// handle between the sidebar and the category content.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: f32,
    /// How the background image is fitted into the window: 0 cover, 1
    /// contain (letterboxed), 2 stretch (fill, ignores aspect ratio), 3
    /// original (1:1 pixels, cropped to fit like `cover` with no scaling).
    #[serde(default)]
    pub theme_image_fit: i32,
    /// Degrees, -180..180.
    #[serde(default)]
    pub theme_image_rotation: f32,
    /// Percent (0..100) trimmed off each edge of the source image before
    /// fitting -- e.g. `theme_image_crop_left: 10.0` starts the visible
    /// region 10% of the way into the image from the left.
    #[serde(default)]
    pub theme_image_crop_left: f32,
    #[serde(default)]
    pub theme_image_crop_top: f32,
    #[serde(default)]
    pub theme_image_crop_right: f32,
    #[serde(default)]
    pub theme_image_crop_bottom: f32,
    /// Same fit/rotation/crop editor, same meaning, for the profile
    /// avatar image instead of the theme background.
    #[serde(default)]
    pub avatar_image_fit: i32,
    #[serde(default)]
    pub avatar_image_rotation: f32,
    #[serde(default)]
    pub avatar_image_crop_left: f32,
    #[serde(default)]
    pub avatar_image_crop_top: f32,
    #[serde(default)]
    pub avatar_image_crop_right: f32,
    #[serde(default)]
    pub avatar_image_crop_bottom: f32,
    /// Freeform positions/sizes of the Call tab's panels, set via its own
    /// "Edit layout" drag-and-resize mode -- the app-wide analogue of the
    /// section reordering above, but for a canvas of independently
    /// placeable/resizable panels rather than a fixed vertical list.
    /// Saved call-screen arrangements (spec §7). The live layout stays in
    /// the `call_*_rect` fields below; these are snapshots to switch
    /// between.
    #[serde(default)]
    pub saved_layouts: Vec<NamedLayout>,
    /// Soundboard clips (spec §13).
    #[serde(default)]
    pub sound_clips: Vec<crate::soundboard::SoundClip>,
    /// Default ring rule for anyone without their own (spec §9). Applies
    /// to strangers as well as contacts who have not been given one.
    /// Spec §13: crash reporting, opt-in only. Off unless the user says
    /// otherwise -- and off means nothing is written at all.
    /// How far through getting started the user is (spec §13).
    #[serde(default)]
    pub first_run: crate::first_run::FirstRunState,
    /// Completed calls, so a backup prompt can wait until there is
    /// something worth backing up.
    #[serde(default)]
    pub calls_completed: u32,
    /// People whose calls are refused (spec §9). Keyed by identity, so a
    /// block follows the person rather than an address they can change.
    #[serde(default)]
    pub blocked: Vec<crate::moderation::BlockedIdentity>,
    /// Locally recorded reports. These go nowhere -- see
    /// `crate::moderation`.
    #[serde(default)]
    pub reports: Vec<crate::moderation::Report>,
    /// Bandwidth limits for file transfer (spec §5).
    #[serde(default)]
    pub transfer_limits: crate::transfer_limits::TransferLimits,
    /// Presence as an integer (spec §7) -- see `crate::presence`.
    /// Seconds of received audio the Replay ring keeps (spec §2).
    #[serde(default = "default_replay_seconds")]
    pub replay_seconds: u32,
    /// What Replay does when a gap is recovered: 0 save on demand only,
    /// 1 ask, 2 play it back automatically.
    #[serde(default)]
    pub replay_mode: i32,
    /// How long an undelivered message keeps trying, in hours; 0 means
    /// never give up (spec §8: the sender controls the outbox and nothing
    /// else about delivery).
    ///
    /// Defaults to 0. A message you meant to send is still a message you
    /// meant to send a week later, and expiring by default would silently
    /// discard things people believe they have sent.
    #[serde(default)]
    pub outbox_give_up_hours: u32,
    /// Whether Shift+click on Call flips the knock switch (spec §9).
    ///
    /// 0 is off: the visible switch is the only way, which is this
    /// build's default because a courtesy nobody can find is not a
    /// courtesy. 1 adds the modifier for people who want it.
    ///
    /// Shift is deliberately a *flip* rather than a mode: it inverts the
    /// switch for one call and puts it back, so a modifier can never
    /// quietly change a standing preference. Inverting the default is the
    /// switch's own job -- leave it on, and every call knocks.
    #[serde(default)]
    pub silent_call_mode: i32,
    /// Whether calls start as a knock by default (spec §9). This is the
    /// "invert the default" the spec asks to be configurable.
    #[serde(default)]
    pub start_call_silently: bool,
    /// Cancel the echo of the other person out of your microphone
    /// (spec §1: "On by default, with a manual off switch for users on
    /// headphones"). A config saved before this field existed gets the
    /// spec's default too.
    #[serde(default = "default_true")]
    pub echo_cancel: bool,
    /// "Low data" calls (spec §2 automatic bandwidth): voice held at
    /// 8 kbit/s in 60 ms packets and no video, for metered or poor links.
    /// Off by default -- calls adapt by themselves anyway.
    #[serde(default)]
    pub low_data_calls: bool,
    /// Send a shared screen's sound along with it (spec §5). On by
    /// default, as people sharing a video expect it to be heard.
    #[serde(default = "default_true")]
    pub share_screen_audio: bool,
    /// Spec §11 "Bots / API, local-first": serve the local API for the
    /// user's own scripts (see the desktop's `api.rs`). Off by default.
    #[serde(default)]
    pub local_api: bool,
    /// How far to lower the other side while you talk, in dB (spec §1
    /// "Ducking"). Zero is off.
    #[serde(default)]
    pub duck_db: u32,
    #[serde(default)]
    pub presence: i32,
    #[serde(default)]
    pub crash_reporting: bool,
    #[serde(default)]
    pub default_ring_rule: i32,
    /// Do-not-disturb window (spec §13 "schedules, do-not-disturb").
    #[serde(default)]
    pub quiet_hours: crate::notify::QuietHours,
    #[serde(default = "default_call_connect_rect")]
    pub call_connect_rect: PanelRect,
    #[serde(default = "default_call_filetransfer_rect")]
    pub call_filetransfer_rect: PanelRect,
    /// Where the invite QR panel sits (spec §4: "QR position
    /// user-configurable via the UI editor, including hiding it
    /// entirely").
    #[serde(default = "default_call_invite_rect")]
    pub call_invite_rect: PanelRect,
    /// The profile widget's own rect -- no visibility field alongside it,
    /// since (unlike the rest) it's never removable.
    #[serde(default = "default_call_profile_rect")]
    pub call_profile_rect: PanelRect,
    #[serde(default = "default_call_chat_rect")]
    pub call_chat_rect: PanelRect,
    #[serde(default = "default_true")]
    pub call_chat_visible: bool,
    #[serde(default = "default_call_contacts_rect")]
    pub call_contacts_rect: PanelRect,
    #[serde(default = "default_true")]
    pub call_contacts_visible: bool,
    /// Named voice presets (spec §1). Kept alongside the live `chain`
    /// rather than replacing it: the live chain is what you're currently
    /// hearing, presets are snapshots you can jump between.
    #[serde(default)]
    pub voice_presets: Vec<VoicePreset>,
    /// Saved contacts and servers (spec §3 "Known contacts"). An address
    /// book, not an identity system -- see `ControlMessage`'s trust note.
    #[serde(default)]
    pub contacts: Vec<Contact>,
    /// Whether each Call tab panel is shown at all -- "Edit layout"'s
    /// add/remove-widgets toggles.
    #[serde(default = "default_true")]
    pub call_connect_visible: bool,
    #[serde(default = "default_true")]
    pub call_filetransfer_visible: bool,
    /// Off by default. The QR is also on the profile settings screen, and
    /// a code that identifies you should be shown because you chose to
    /// show it, not because it happened to be on by default while
    /// somebody was sharing their screen.
    #[serde(default)]
    pub call_invite_visible: bool,
    /// Path to a user-picked avatar image, or "" for the default
    /// initial-letter circle. Same re-registration-on-startup story as the
    /// theme background image: the loaded `slint::Image` isn't itself
    /// persisted, just the path to reload it from.
    #[serde(default)]
    pub profile_avatar_path: String,
    /// Which details the compact profile bar shows -- the popup (opened by
    /// clicking it) always shows everything regardless of these.
    #[serde(default = "default_true")]
    pub profile_widget_show_avatar: bool,
    #[serde(default = "default_true")]
    pub profile_widget_show_name: bool,
    #[serde(default = "default_true")]
    pub profile_widget_show_status: bool,
}

fn default_true() -> bool {
    true
}

/// A named snapshot of the whole audio chain (spec §1 "Voice Presets --
/// export it, share it, load someone else's"). Stores the chain only, not
/// devices or appearance: a preset is about how you *sound*, so loading a
/// friend's shouldn't move your microphone selection or repaint your UI.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct VoicePreset {
    pub name: String,
    pub chain: ChainConfig,
}

/// Writes one preset to an arbitrary path, for sharing.
pub fn save_preset_to(path: &std::path::Path, preset: &VoicePreset) -> anyhow::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(preset)?)?;
    Ok(())
}

/// Reads a shared preset file. Errors are surfaced rather than swallowed --
/// someone handed you this file, so a failure needs explaining.
pub fn load_preset_from(path: &std::path::Path) -> anyhow::Result<VoicePreset> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// One saved contact or server. `is_server` only changes how it's labeled
/// and grouped in the UI -- a "server" here is just an address that
/// happens to always be listening, not a different protocol.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Contact {
    /// Your petname for them (spec §4). Local to this device, and what
    /// you see -- never what they call themselves, so a display name
    /// can't be used to impersonate someone you already know.
    pub name: String,
    /// Where they were last reachable. Changes freely without changing
    /// who they are.
    pub address: String,
    #[serde(default)]
    pub is_server: bool,
    /// Their pinned Ed25519 identity key, hex. Empty until the first
    /// successful call, since an address alone proves nothing about who
    /// answers at it.
    ///
    /// This is what actually identifies the contact -- see
    /// `antiphon_crypto::contact` for the trust rules built on it.
    #[serde(default)]
    pub identity_key: String,
    /// Whether a verification code was compared out of band and matched.
    #[serde(default)]
    pub verified: bool,
    /// Unix seconds when the key was first pinned. 0 if never.
    #[serde(default)]
    pub first_seen_secs: u64,
    /// Per-person volume trim in dB (spec §1 "Per-User Volume"), on top of
    /// the master output level.
    ///
    /// Stored on the contact rather than globally because it is a fact
    /// about *them* -- a quiet microphone stays quiet across every call,
    /// and having to re-fix it each time is exactly the annoyance the
    /// setting exists to remove.
    #[serde(default)]
    pub volume_db: f32,
    /// Muted on your end. They are not told.
    #[serde(default)]
    pub muted: bool,
    /// Seconds after which messages with this contact delete themselves,
    /// or `None` to keep them (spec §13 "disappearing messages").
    ///
    /// Per contact rather than global: how long a conversation should
    /// linger is a property of who it is with.
    #[serde(default)]
    pub expire_after_secs: Option<u64>,
    /// Whether this person may ring you (spec §9 "Receiver settings always
    /// win"): 0 always, 1 silent only, 2 never. See `crate::notify`.
    #[serde(default)]
    pub ring_rule: i32,
    /// EQ applied to *their* audio on your end (spec §1 "Per-User EQ").
    ///
    /// Empty means none. Stored on the contact because a harsh microphone
    /// is a fact about them that outlives any one call.
    #[serde(default)]
    pub eq_bands: Vec<antiphon_audio::EqBandConfig>,
    /// Where this contact came from, when that is worth remembering --
    /// currently "Introduced by <name>" (spec §4).
    ///
    /// Provenance is not trust, and keeping it visible is what stops the
    /// two being confused later: an introduced contact reads as introduced
    /// until a verification code says otherwise.
    #[serde(default)]
    pub note: String,
    /// Their X25519 prekey, hex, learned from the first completed call.
    /// What makes them a *known* contact (spec §3) and what the pairwise
    /// relay token is derived from.
    #[serde(default)]
    pub prekey: String,
    /// The relay they listen at, `host:port`, from their invite link or
    /// from what they said on a call. Empty when unknown.
    #[serde(default)]
    pub relay: String,
    /// The secret from their invite link, hex, for ringing them through
    /// their relay before you have ever spoken.
    #[serde(default)]
    pub relay_secret: String,
    /// They let contacts with no relay of their own listen at `relay`, as
    /// their invite or their last call said. See `crate::fallback_relay`.
    #[serde(default)]
    pub lends_relay: bool,
    /// Per-conversation override (spec §3): 0 automatic, 1 direct, 2 relay.
    /// See `crate::connection_mode`.
    #[serde(default)]
    pub connection_mode: i32,
    /// Their signed route list (spec §8), hex, as they last sent it: where
    /// to leave mail for them while they're offline. They decide it.
    #[serde(default)]
    pub route_record: String,
    /// The sequence of `route_record`, so an older one can't replace it.
    #[serde(default)]
    pub route_sequence: Option<u64>,
    /// Per-contact override (spec §8): 0 use their routes, 1 direct only
    /// -- messages to them wait until you're both online.
    #[serde(default)]
    pub message_route: i32,
    /// The highest DHT record sequence accepted for them (spec §4), so a
    /// replayed older record can't move them back.
    #[serde(default)]
    pub dht_sequence: Option<u64>,
    /// Their About line, as they last sent it (`ControlMessage::Profile`).
    #[serde(default)]
    pub bio: String,
    /// Their avatar colour's id, as they last sent it; empty for automatic.
    #[serde(default)]
    pub color: String,
    /// Their picture, as they last sent it: a small JPEG, base64.
    #[serde(default)]
    pub avatar: String,
    /// Their pronouns, as they last sent them; empty for none given.
    #[serde(default)]
    pub pronouns: String,
}

/// A freeform panel's position and size in logical pixels, relative to its
/// tab's own canvas origin (top-left of the content area, not the window).
/// A whole call-screen arrangement, saved under a name (spec §7 "Save
/// named arrangements").
///
/// Holds only the call canvas -- panel positions, sizes and visibility --
/// and deliberately not colours, fonts or audio settings. Those are
/// separate axes of taste, and folding them together would mean switching
/// to a compact layout also changed your theme.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NamedLayout {
    pub name: String,
    pub connect_rect: PanelRect,
    pub filetransfer_rect: PanelRect,
    pub profile_rect: PanelRect,
    pub chat_rect: PanelRect,
    pub contacts_rect: PanelRect,
    /// The invite QR (spec §4). Defaulted rather than required, so a
    /// layout saved before this panel existed still loads -- with the
    /// panel where a fresh install puts it, not at 0,0 under another one.
    #[serde(default = "default_call_invite_rect")]
    pub invite_rect: PanelRect,
    pub connect_visible: bool,
    pub filetransfer_visible: bool,
    pub chat_visible: bool,
    pub contacts_visible: bool,
    /// Hidden by default in a saved layout that predates it: someone who
    /// arranged their panels once should not find a new one dropped on
    /// top of the arrangement they made.
    #[serde(default)]
    pub invite_visible: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct PanelRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

fn default_call_connect_rect() -> PanelRect {
    PanelRect { x: 0.0, y: 0.0, width: 540.0, height: 190.0 }
}

fn default_call_filetransfer_rect() -> PanelRect {
    PanelRect { x: 550.0, y: 0.0, width: 320.0, height: 340.0 }
}

fn default_call_invite_rect() -> PanelRect {
    PanelRect { x: 550.0, y: 350.0, width: 260.0, height: 300.0 }
}

fn default_call_profile_rect() -> PanelRect {
    PanelRect { x: 550.0, y: 350.0, width: 320.0, height: 130.0 }
}

fn default_call_chat_rect() -> PanelRect {
    PanelRect { x: 550.0, y: 500.0, width: 320.0, height: 220.0 }
}

fn default_call_contacts_rect() -> PanelRect {
    PanelRect { x: 900.0, y: 0.0, width: 320.0, height: 300.0 }
}

fn default_sidebar_width() -> f32 {
    148.0
}

/// The canonical, exhaustive id list for each tab's reorderable sections --
/// `sanitize_order` uses these to repair a saved order that's missing an id,
/// has a stale one, or (from hand-edited JSON) a duplicate. "font" lives
/// inside "textmotion" (Appearance > Text & motion), not its own section.
/// Sections the Audio tab renders inside its reorderable loop.
///
/// "presets" was missing here while `app.slint` rendered it, and
/// `sanitize_order` drops ids it doesn't recognise -- so the Voice presets
/// section could never appear at all. Conversely "miclevel" and "mictest"
/// were listed here but no section renders them: that UI is a fixed block
/// at the top of the tab, outside the loop, so they only ever occupied
/// reorderable slots that drew nothing.
pub const AUDIO_SECTION_IDS: &[&str] =
    &["presets", "callcontrols", "aec", "denoise", "gate", "hpf", "lpf", "deesser", "eq", "comp", "limiter", "pitch", "reverb"];
pub const APPEARANCE_SECTION_IDS: &[&str] = &["accent", "theme", "textmotion", "importexport"];
pub const PROFILE_SECTION_IDS: &[&str] = &["profile", "about"];
pub const CALL_SECTION_IDS: &[&str] =
    &["callstart", "notifications", "soundboard", "transferlimits", "sharedfiles", "captions", "discovery", "callparticipants", "outbox", "calladvanced", "vpn"];

fn default_audio_order() -> Vec<String> {
    AUDIO_SECTION_IDS.iter().map(ToString::to_string).collect()
}

fn default_appearance_order() -> Vec<String> {
    APPEARANCE_SECTION_IDS.iter().map(ToString::to_string).collect()
}

fn default_profile_order() -> Vec<String> {
    PROFILE_SECTION_IDS.iter().map(ToString::to_string).collect()
}

fn default_replay_seconds() -> u32 {
    // Deferred to `state` so the default and the audio ring that honours
    // it cannot drift apart.
    crate::state::REPLAY_SECONDS
}

fn default_call_order() -> Vec<String> {
    CALL_SECTION_IDS.iter().map(ToString::to_string).collect()
}

/// Keeps `loaded` from ever being able to drop a section out of the UI:
/// unknown ids are dropped, duplicates are collapsed to their first
/// occurrence, and any canonical id missing from `loaded` is appended (in
/// canonical order) at the end -- so a stale save from a version with fewer
/// sections just puts the new ones last, rather than hiding them.
pub fn sanitize_order(loaded: Vec<String>, canonical: &[&str]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(canonical.len());
    for id in loaded {
        if canonical.contains(&id.as_str()) && !result.contains(&id) {
            result.push(id);
        }
    }
    for id in canonical {
        if !result.iter().any(|r| r == id) {
            result.push(id.to_string());
        }
    }
    result
}

fn default_theme_bg_hex() -> String {
    "#202020".to_string()
}

fn default_theme_image_opacity() -> f32 {
    0.5
}

fn default_theme_opacity() -> f32 {
    1.0
}

fn default_sections_expanded() -> bool {
    true
}

fn default_accent_hex() -> String {
    // Encore's cream, so the two apps share a palette.
    "#f0ead6".to_string()
}

fn default_text_scale() -> f32 {
    1.0
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            stoop: StoopPrefs::default(),
            profile: ProfileData::default(),
            chain: ChainConfig::default_preset(),
            input_device: None,
            output_device: None,
            ptt_key: crate::caps_lock::default_ptt_key().to_string(),
            transmit_mode: 0,
            mute_key: String::new(),
            deafen_key: String::new(),
            hangup_key: String::new(),
            replay_key: String::new(),
            va_threshold: 0.05,
            theme_mode: 0,
            text_scale: 1.0,
            reduced_motion: false,
            server_mode: false,
            start_maximized: false,
            window: WindowState::default(),
            output_gain_db: 0.0,
            sections_expanded_default: true,
            accent_hex: default_accent_hex(),
            font_family: String::new(),
            custom_font_path: String::new(),
            theme_bg_hex: default_theme_bg_hex(),
            theme_image_path: String::new(),
            theme_image_opacity: default_theme_image_opacity(),
            theme_opacity: default_theme_opacity(),
            audio_section_order: default_audio_order(),
            appearance_section_order: default_appearance_order(),
            profile_section_order: default_profile_order(),
            call_section_order: default_call_order(),
            saved_layouts: Vec::new(),
            sound_clips: Vec::new(),
            first_run: crate::first_run::FirstRunState::default(),
            calls_completed: 0,
            transfer_limits: crate::transfer_limits::TransferLimits::default(),
            blocked: Vec::new(),
            reports: Vec::new(),
            replay_seconds: default_replay_seconds(),
            replay_mode: 0,
            outbox_give_up_hours: 0,
            silent_call_mode: 0,
            start_call_silently: false,
            echo_cancel: true,
            low_data_calls: false,
            share_screen_audio: true,
            local_api: false,
            duck_db: 0,
            presence: 0,
            crash_reporting: false,
            default_ring_rule: 0,
            quiet_hours: crate::notify::QuietHours::default(),
            ptt_toggle: false,
            mute_hold: false,
            relay_server: String::new(),
            relay_invite_secret: String::new(),
            fallback_relays: Vec::new(),
            use_project_relays: true,
            lend_relay: false,
            mesh_relays: Vec::new(),
            anonymity_hops: Vec::new(),
            anonymity_mode: false,
            routes: crate::delivery::default_routes(),
            route_fan_out: false,
            strict_mode: false,
            route_sequence: 0,
            hold_mail_for_devices: false,
            hold_mail_port: default_hold_mail_port(),
            captions_enabled: false,
            captions_own_voice: false,
            seeding: false,
            anonymous_seeding: false,
            seed_scope: 0,
            seed_port: default_seed_port(),
            pin_nodes: Vec::new(),
            pin_code: String::new(),
            vpn_enabled: false,
            vpn_active: String::new(),
            vpn_route_voice: false,
            vpn_route_files: true,
            vpn_route_messages: true,
            vpn_kill_switch: false,
            dht_enabled: false,
            dht_bootstrap: Vec::new(),
            directories: Vec::new(),
            directory_pins: Vec::new(),
            deafen_hold: false,
            ptt_release_ms: 0.0,
            ip_direct_calling: false,
            start_with_screen_share: false,
            start_with_camera: false,
            mute_others_by_default: false,
            mute_self_on_join: false,
            sidebar_width: default_sidebar_width(),
            theme_image_fit: 0,
            theme_image_rotation: 0.0,
            theme_image_crop_left: 0.0,
            theme_image_crop_top: 0.0,
            theme_image_crop_right: 0.0,
            theme_image_crop_bottom: 0.0,
            avatar_image_fit: 0,
            avatar_image_rotation: 0.0,
            avatar_image_crop_left: 0.0,
            avatar_image_crop_top: 0.0,
            avatar_image_crop_right: 0.0,
            avatar_image_crop_bottom: 0.0,
            call_connect_rect: default_call_connect_rect(),
            call_filetransfer_rect: default_call_filetransfer_rect(),
            call_invite_rect: default_call_invite_rect(),
            call_profile_rect: default_call_profile_rect(),
            call_chat_rect: default_call_chat_rect(),
            call_chat_visible: true,
            call_contacts_rect: default_call_contacts_rect(),
            call_contacts_visible: true,
            voice_presets: Vec::new(),
            contacts: Vec::new(),
            call_connect_visible: true,
            call_filetransfer_visible: true,
            call_invite_visible: false,
            profile_avatar_path: String::new(),
            profile_widget_show_avatar: true,
            profile_widget_show_name: true,
            profile_widget_show_status: true,
        }
    }
}

/// The active profile, or `None` for the default one.
///
/// A "profile" is a separate identity plus its own settings on the same
/// machine -- what `--profile alice` selects. It exists because identity
/// here is a keypair tied to a config directory, so without this every
/// instance on a machine is literally the same person, and two windows
/// side by side can't call each other meaningfully.
///
/// This is emphatically *not* an account system: there is no server, no
/// login, and no name in anyone's identity. It is one machine holding
/// more than one keypair.
static PROFILE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Chooses the profile for this process. Call once, before any config or
/// identity access.
///
/// Returns the sanitized name actually used, which may differ from what
/// was asked for -- the name becomes a directory, so anything that isn't
/// a plain identifier character is stripped rather than trusted. A name
/// that sanitizes to nothing falls back to the default profile instead of
/// creating a directory with a surprising path.
pub fn set_profile(requested: Option<&str>) -> Option<String> {
    let cleaned = requested.and_then(sanitize_profile);
    // A second call is ignored rather than panicking: losing the race
    // would mean some config had already been read under the other name,
    // and swapping underneath it would be worse than the mismatch.
    let _ = PROFILE.set(cleaned);
    PROFILE.get().cloned().flatten()
}

/// Strips a requested profile name down to characters that are safe as a
/// single path component. Separators and dots are dropped rather than
/// escaped, so no name can climb out of the profiles directory.
fn sanitize_profile(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

/// The profile chosen for this process, if any.
pub fn profile() -> Option<String> {
    PROFILE.get().cloned().flatten()
}

/// The directory holding this app's config. Also where the identity key
/// lives -- see `antiphon_crypto::store`, and note that the key is
/// deliberately a separate file so settings export can't sweep it up.
///
/// The default profile keeps the original path exactly, so an existing
/// install's settings and identity are found where they already are.
/// Named profiles nest under `profiles/`.
pub fn config_dir() -> Option<std::path::PathBuf> {
    let mut dir = antiphon_crypto::store::app_data_dir(&dirs::config_dir()?);
    if let Some(name) = profile() {
        dir.push("profiles");
        dir.push(name);
    }
    Some(dir)
}

fn config_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("config.json"))
}

/// Loads the saved config, if any. Missing file or parse failure both just
/// mean "start from defaults" -- a corrupt settings file shouldn't block
/// the app from opening.
fn default_hold_mail_port() -> u16 {
    3480
}

fn default_seed_port() -> u16 {
    3481
}

pub fn load() -> AppConfig {
    let Some(path) = config_path() else { return AppConfig::default() };
    let Ok(text) = std::fs::read_to_string(&path) else { return AppConfig::default() };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save(config: &AppConfig) {
    let Some(path) = config_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(path, text);
    }
}

/// Writes `config` to an arbitrary path (spec §7 "Export & import" / §1
/// "Voice Presets -- export it, share it, load someone else's"), unlike
/// [`save`] which always targets the app's own config file.
pub fn save_to(path: &std::path::Path, config: &AppConfig) -> anyhow::Result<()> {
    let text = serde_json::to_string_pretty(config)?;
    std::fs::write(path, text)?;
    Ok(())
}

/// Reads a config from an arbitrary path, surfacing errors -- unlike
/// [`load`], a bad *imported* file should tell the user it failed rather
/// than silently falling back to defaults.
pub fn load_from(path: &std::path::Path) -> anyhow::Result<AppConfig> {
    let text = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout(name: &str) -> NamedLayout {
        let r = |x: f32| PanelRect { x, y: 10.0, width: 200.0, height: 120.0 };
        NamedLayout {
            name: name.to_string(),
            connect_rect: r(1.0),
            filetransfer_rect: r(2.0),
            profile_rect: r(3.0),
            chat_rect: r(4.0),
            contacts_rect: r(5.0),
            invite_rect: r(6.0),
            connect_visible: true,
            filetransfer_visible: false,
            chat_visible: true,
            contacts_visible: false,
            invite_visible: true,
        }
    }

    #[test]
    fn saved_layouts_round_trip_through_json() {
        let config = AppConfig {
            saved_layouts: vec![sample_layout("Compact"), sample_layout("Everything")],
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(back.saved_layouts, config.saved_layouts);
    }

    #[test]
    fn a_config_saved_before_named_layouts_existed_still_loads() {
        // `saved_layouts` is defaulted, so an older config file opens with
        // no arrangements rather than failing to parse and resetting
        // everything else the user had configured.
        let mut value: serde_json::Value =
            serde_json::to_value(AppConfig::default()).unwrap();
        value.as_object_mut().unwrap().remove("saved_layouts");

        let back: AppConfig = serde_json::from_value(value).unwrap();
        assert!(back.saved_layouts.is_empty());
    }

    #[test]
    fn a_layout_keeps_panel_visibility_as_well_as_position() {
        // Position alone is not an arrangement: a layout that reopened
        // every panel you had hidden would not be the layout you saved.
        let layout = sample_layout("Compact");
        assert!(layout.connect_visible);
        assert!(!layout.filetransfer_visible);
        assert!(!layout.contacts_visible);
    }

    #[test]
    fn round_trips_through_json() {
        let cfg = AppConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let back: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.ptt_key, back.ptt_key);
        assert_eq!(cfg.theme_mode, back.theme_mode);
        assert_eq!(cfg.text_scale, back.text_scale);
    }

    /// A config saved before `theme_mode`/`text_scale`/`reduced_motion`/
    /// `server_mode` existed must still load -- `#[serde(default)]` is what
    /// makes that true, and it's easy to add a field later and forget it.
    #[test]
    fn old_config_without_newer_fields_still_loads() {
        let old_json = serde_json::json!({
            "profile": { "display_name": "You", "status_message": "Online" },
            "chain": ChainConfig::default_preset(),
            "input_device": null,
            "output_device": null,
            "ptt_key": "Space",
            "va_threshold": 0.05,
        })
        .to_string();

        let cfg: AppConfig = serde_json::from_str(&old_json).expect("old config shape should still parse");
        assert_eq!(cfg.theme_mode, 0);
        assert_eq!(cfg.text_scale, 1.0);
        assert!(!cfg.start_maximized);
        assert!(cfg.window.width.is_none());
        assert!(!cfg.reduced_motion);
        assert!(!cfg.server_mode);
        assert_eq!(cfg.transmit_mode, 0, "an old config without a transmit mode falls back to push-to-talk");
        assert_eq!(cfg.mute_key, "");
        assert_eq!(cfg.output_gain_db, 0.0);
        assert!(cfg.sections_expanded_default, "sections should default to expanded, not silently collapse everything on upgrade");
        assert_eq!(cfg.accent_hex, "#f0ead6");
        assert_eq!(cfg.font_family, "");
        assert_eq!(cfg.custom_font_path, "");
        assert_eq!(cfg.theme_bg_hex, "#202020");
        assert_eq!(cfg.theme_image_path, "");
        assert_eq!(cfg.theme_image_opacity, 0.5);
        assert_eq!(cfg.theme_opacity, 1.0);
        assert_eq!(cfg.audio_section_order, default_audio_order());
        assert_eq!(cfg.appearance_section_order, default_appearance_order());
        assert_eq!(cfg.profile_section_order, default_profile_order());
        assert_eq!(cfg.sidebar_width, 148.0);
        assert_eq!(cfg.theme_image_fit, 0);
        assert_eq!(cfg.theme_image_rotation, 0.0);
        assert_eq!(cfg.theme_image_crop_left, 0.0);
        assert_eq!(cfg.avatar_image_fit, 0);
        assert_eq!(cfg.avatar_image_rotation, 0.0);
        assert_eq!(cfg.avatar_image_crop_left, 0.0);
        assert_eq!(cfg.call_connect_rect, default_call_connect_rect());
        assert_eq!(cfg.call_filetransfer_rect, default_call_filetransfer_rect());
        assert_eq!(cfg.call_invite_rect, default_call_invite_rect());
        assert_eq!(cfg.call_profile_rect, default_call_profile_rect());
        assert_eq!(cfg.call_chat_rect, default_call_chat_rect());
        assert!(cfg.call_chat_visible);
        assert!(cfg.contacts.is_empty(), "an upgrade shouldn't invent contacts");
        assert!(cfg.voice_presets.is_empty(), "an upgrade shouldn't invent presets");
        assert!(cfg.call_contacts_visible);
        assert!(cfg.call_connect_visible);
        assert!(cfg.call_filetransfer_visible);
        // A panel showing a code that identifies you is opt-in.
        assert!(!cfg.call_invite_visible);
        assert_eq!(cfg.profile_avatar_path, "");
        assert!(cfg.profile_widget_show_avatar);
        assert!(cfg.profile_widget_show_name);
        assert!(cfg.profile_widget_show_status);
    }

    #[test]
    fn a_preset_round_trips_through_a_shared_file() {
        let preset = VoicePreset { name: "Radio".to_string(), chain: ChainConfig::default_preset() };
        let path = std::env::temp_dir().join("antiphon_preset_test.json");
        save_preset_to(&path, &preset).unwrap();
        let back = load_preset_from(&path).unwrap();
        assert_eq!(back.name, "Radio");
        assert_eq!(back.chain.noise_gate.threshold_db, preset.chain.noise_gate.threshold_db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_malformed_preset_file_reports_an_error_instead_of_a_default() {
        let path = std::env::temp_dir().join("antiphon_preset_bad.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(load_preset_from(&path).is_err(), "someone handed you this file -- a failure has to be explainable");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sanitize_order_drops_unknown_ids_and_appends_missing_ones() {
        let canonical = &["a", "b", "c"];
        let loaded = vec!["c".to_string(), "stale".to_string(), "c".to_string()];
        assert_eq!(sanitize_order(loaded, canonical), vec!["c", "a", "b"]);
    }

    #[test]
    fn sanitize_order_leaves_a_valid_permutation_untouched() {
        let canonical = &["a", "b", "c"];
        let loaded = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        assert_eq!(sanitize_order(loaded, canonical), vec!["c", "a", "b"]);
    }

    #[test]
    fn a_profile_name_is_stripped_to_something_safe_to_use_as_a_directory() {
        // The name becomes a path component, so traversal and separators
        // must not survive it.
        assert_eq!(sanitize_profile("alice"), Some("alice".to_string()));
        assert_eq!(sanitize_profile("../../etc"), Some("etc".to_string()));
        assert_eq!(sanitize_profile(r"a/b\c"), Some("abc".to_string()));
        assert_eq!(sanitize_profile("test-2_b"), Some("test-2_b".to_string()));
    }

    #[test]
    fn a_profile_name_with_nothing_usable_falls_back_to_the_default() {
        assert_eq!(sanitize_profile("///"), None);
        assert_eq!(sanitize_profile(""), None);
    }

    /// The low-pass stage was in the DSP chain long before it had any UI,
    /// so a config saved by an older build has an audio order with no
    /// "lpf" in it. That must gain the section rather than keep hiding it
    /// -- the whole point of exposing it was that an unreachable stage
    /// violates spec §0 "User choice".
    #[test]
    fn an_audio_order_saved_before_the_low_pass_section_existed_gains_it() {
        let pre_lpf: Vec<String> = AUDIO_SECTION_IDS
            .iter()
            .filter(|id| **id != "lpf")
            .map(ToString::to_string)
            .collect();
        let merged = sanitize_order(pre_lpf, AUDIO_SECTION_IDS);
        assert!(merged.contains(&"lpf".to_string()));
        assert_eq!(merged.len(), AUDIO_SECTION_IDS.len());
    }
}
