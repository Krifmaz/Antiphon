//! What the screens show: plain data the app fills in from its engine.
//!
//! The interface never reaches into the call engine, the contact list or
//! the settings file. Each app (desktop, phone) builds a [`Model`] from its
//! own state, the screens draw it, and what the person does comes back as
//! [`crate::Action`]s for the app to carry out. That split is what lets one
//! set of screens run on two very different engines.

use iced_widget::qr_code;

/// Everything the screens draw.
#[derive(Default)]
pub struct Model {
    /// The person using the app.
    pub me: Me,
    /// Contacts, in the order the list shows them.
    pub people: Vec<Person>,
    /// Notes to self: a conversation with yourself, synced between your
    /// devices. Listed above your people, never among them -- it can't be
    /// called, invited or blocked. `None` where the app doesn't offer it.
    pub notes: Option<Person>,
    /// The call, if any.
    pub call: Call,
    /// The conversation with the person on screen, once the app has loaded
    /// it (after [`crate::Action::Open`]).
    pub conversation: Option<Conversation>,
    /// Files on their way.
    pub transfers: Transfers,
    /// Reachability and connection settings.
    pub network: Network,
    /// Everything Settings shows.
    pub settings: Settings,
    /// A line across the top of the window: something the person should
    /// know now ("Your microphone is blocked").
    pub notice: Option<String>,
    /// The first-run screen, until the person has picked a name.
    pub first_run: bool,
    /// What this device can do.
    pub device: Device,
    /// What only a phone has: the loudspeaker, battery and data rules.
    /// `None` on a desktop, which hides all of it.
    pub phone: Option<Phone>,
    /// Keeping the account: the recovery phrase, backups, moving devices.
    pub account: Account,
    /// Group chats and communities you are in (or waiting to be let into),
    /// in the order the list shows them.
    pub communities: Vec<Community>,
    /// Invitations to group chats and communities, waiting for an answer.
    pub space_invites: Vec<SpaceInvite>,
    /// The channel on screen, once the app has loaded it (after
    /// [`crate::Action::OpenChannel`]).
    pub channel: Option<ChannelView>,
    /// The community directories you browse, and what they last listed.
    pub directory: Directory,
    /// A newer version of Stoop, and getting it (Settings > Updates, and a
    /// line across the top while one is waiting).
    pub update: Update,
    /// Live captions, where this device can run them (desktop only).
    pub captions: Option<Captions>,
}

impl Model {
    /// The conversation partner `id` names: one of your people, or your
    /// notes to self.
    pub fn person(&self, id: &str) -> Option<&Person> {
        self.people.iter().chain(&self.notes).find(|p| p.id == id)
    }
}

/// The in-app updater, as the screens show it. The app fills this in from
/// `antiphon-update`; the buttons come back as [`crate::Action::UpdateCheck`],
/// [`crate::Action::UpdateInstall`], [`crate::Action::UpdateLater`],
/// [`crate::Action::UpdatePage`] and [`crate::Action::UpdateAuto`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Update {
    /// This copy can look for updates at all. False (iOS, whose updates come
    /// from the App Store, and the demo) hides everything about updates.
    pub available: bool,
    /// Look for updates by itself, at start and every few hours.
    pub auto: bool,
    /// Where it stands.
    pub state: UpdateState,
    /// Show the newer version across the top of the window. False after
    /// "Later", until a newer one still comes out.
    pub banner: bool,
    /// The app can download and install it itself. When false, "Update"
    /// opens the release page instead (a Flatpak, or the plain tarball).
    pub can_install: bool,
}

/// Where an update stands, for [`Update`].
#[derive(Clone, Debug, Default, PartialEq)]
pub enum UpdateState {
    /// Nothing looked for yet.
    #[default]
    Idle,
    /// Looking.
    Checking,
    /// This is the newest version.
    UpToDate,
    /// This version is out ("0.3.1").
    Available(String),
    /// This version is downloading; how far, from 0 to 1, when known.
    Downloading(String, Option<f32>),
    /// Downloaded and being installed.
    Installing(String),
    /// What went wrong, in a sentence.
    Failed(String),
}

/// Community directories (spec §4 "Opt-in directories for public
/// communities only"): the ones you added, and what they list.
#[derive(Clone, Debug, Default)]
pub struct Directory {
    /// Each directory's address, as added.
    pub directories: Vec<String>,
    /// Everything they listed at the last look, in their order.
    pub listed: Vec<Listing>,
    /// What went wrong with any of them at the last look.
    pub problems: Vec<String>,
    /// A look is under way.
    pub browsing: bool,
}

/// One community a directory lists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Its name.
    pub name: String,
    /// What it says about itself.
    pub description: String,
    /// Its join link (or, from an older directory, its host's invite).
    pub link: String,
    /// The directory that listed it.
    pub directory: String,
}

/// Settings > Account: everything about not losing the account.
#[derive(Clone, Default)]
pub struct Account {
    /// The recovery phrase, while it is shown; `None` while hidden.
    pub phrase: Option<String>,
    /// A slow job is running ("Sealing your backup…"); the buttons that
    /// would start another wait until it is done.
    pub busy: Option<String>,
    /// An open offer to move this account to another device.
    pub moving: Option<Moving>,
    /// Whether this device can show a save or open dialog for a backup
    /// file. A phone without one still saves to Downloads.
    pub can_pick_file: bool,
    /// Everything can be exported as plain files into a folder (desktop
    /// only).
    pub can_export_data: bool,
    /// The account's devices, this one first. Empty until the list loads.
    pub devices: Vec<DeviceRow>,
    /// How this device keeps in touch with the others, in words: through
    /// which mailboxes, or what it needs before it can.
    pub device_sync: String,
    /// Devices can be removed from here: this device has a way to tell
    /// the account's others.
    pub can_remove_devices: bool,
    /// The mailboxes this device keeps in touch with the others through,
    /// editable here (`host:port`). `None` where they come from elsewhere
    /// (the desktop's Settings > Delivery).
    pub mailboxes: Option<Vec<String>>,
}

/// One device on the account, as Settings > Account lists it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceRow {
    /// Its device key, hex: what [`crate::Action::RevokeDevice`] names it by.
    pub id: String,
    /// What it is called ("Desktop", "Phone").
    pub name: String,
    /// When it joined, in words ("Added 3 March 2026").
    pub added: String,
    /// The device this is running on.
    pub this: bool,
    /// The device key's fingerprint, for telling two of the same name apart.
    pub fingerprint: String,
}

/// An open offer to move the account to another device.
#[derive(Clone)]
pub struct Moving {
    /// The link the new device opens.
    pub link: String,
    /// The same link as a QR code, for a phone's camera.
    pub qr: Option<std::sync::Arc<qr_code::Data>>,
    /// Where it stands, in words ("Waiting for your other device").
    pub status: String,
    /// The account has arrived on the other device.
    pub done: bool,
}

/// The phone's own settings and state.
#[derive(Clone, Debug, Default)]
pub struct Phone {
    /// Calls play through the loudspeaker rather than the earpiece.
    pub speaker: bool,
    /// Whether the microphone may be used; false shows a way to allow it.
    pub mic_allowed: bool,
    /// Files move only on Wi-Fi.
    pub wifi_only: bool,
    /// Lower quality and fewer background checks on mobile data.
    pub data_saver: bool,
    /// The monthly mobile data limit for files, in MB; 0 for none.
    pub cellular_limit_mb: u32,
    /// Mobile data used by files this month, as people read it ("12 MB").
    pub cellular_used: String,
    /// Keep the door open while the app is in the background.
    pub keep_listening: bool,
    /// Keep listening in the background even on battery saver.
    pub listen_on_saver: bool,
    /// The push-to-talk pad is held.
    pub talking: bool,
}

/// This device's abilities, so a screen offers only what works here.
#[derive(Clone, Debug, Default)]
pub struct Device {
    /// A phone: touch-sized targets, no hover-only controls.
    pub phone: bool,
    /// Whether a file picker exists.
    pub files: bool,
    /// A push-to-talk key can be bound (desktop keyboards).
    pub ptt_keys: bool,
    /// Group chats and communities work on this device; without them the
    /// screens for them are not offered.
    pub communities: bool,
    /// Voice messages can be recorded here.
    pub voice_messages: bool,
}

/// The person using the app.
#[derive(Default)]
pub struct Me {
    /// Display name.
    pub name: String,
    /// Status line ("Around this evening").
    pub status: String,
    /// A line or two about me, shown to contacts.
    pub bio: String,
    /// How to refer to me ("she/her"), shown beside my name; empty for
    /// none given.
    pub pronouns: String,
    /// How my own profile page is arranged, and where my QR code sits on it.
    pub layout: crate::ProfileLayout,
    /// My avatar colour's id ([`crate::PROFILE_COLORS`]); empty for automatic.
    pub color: String,
    /// Whether I have a profile picture (registered in [`crate::avatars`]).
    pub has_picture: bool,
    /// Their identity key, hex: the avatar colour seed.
    pub id: String,
    /// The key's fingerprint in groups, for reading aloud.
    pub fingerprint: String,
    /// The invite link others paste to add them.
    pub invite: String,
    /// The invite as a QR code, built by the app when the link changes.
    pub qr: Option<std::sync::Arc<qr_code::Data>>,
    /// Open door: listening for calls.
    pub door_open: bool,
    /// Where this device listens, as people on the same network reach it.
    pub local_addr: Option<String>,
    /// The address the internet sees, when known.
    pub public_addr: Option<String>,
    /// "Good evening", from the app's clock.
    pub greeting: String,
}

/// Someone in the contact list.
#[derive(Clone, Debug, Default)]
pub struct Person {
    /// Their identity key, hex. Stable: what every action names them by.
    pub id: String,
    /// The name you gave them.
    pub name: String,
    /// Where they were last reached.
    pub address: String,
    /// The fingerprint was compared out loud and matched.
    pub verified: bool,
    /// What they last said about being around.
    pub presence: Presence,
    /// The last message, one line.
    pub last: Option<String>,
    /// When it was, short ("14:02", "Tue").
    pub last_at: Option<String>,
    /// Messages from them not seen yet.
    pub unread: u32,
    /// Their key's fingerprint in groups, for reading aloud.
    pub fingerprint: String,
    /// A private note about them.
    pub note: String,
    /// When they may ring through ("Always", "Contacts only", ...).
    pub ring_rule: String,
    /// How a call to them is placed ("Automatic", "Direct only", ...).
    pub route: String,
    /// How loud they are for you, dB (0 is unchanged). Remembered per person.
    pub volume_db: f32,
    /// Silenced for you only; they are not told.
    pub muted: bool,
    /// What they say about themselves, as they last sent it.
    pub bio: String,
    /// Their pronouns, as they last sent them; empty for none given.
    pub pronouns: String,
    /// Not a contact: your own notes, kept and synced between your devices.
    /// No calls, files or profile, just messages to yourself.
    pub note_to_self: bool,
}

/// What a person last said about being around.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    /// Around.
    Online,
    /// Around, but not paying attention.
    Away,
    /// Asking not to be interrupted.
    Busy,
    /// Said nothing, or has not been reached this session.
    #[default]
    Unknown,
}

/// The call, if any.
#[derive(Clone, Debug, Default)]
pub struct Call {
    /// Where the call is.
    pub phase: Phase,
    /// The contact on the call, when they are one.
    pub peer: Option<String>,
    /// Their name, or the address for a stranger.
    pub peer_name: String,
    /// The words both sides compare to rule out a listener in the middle.
    pub code: String,
    /// "Direct", "Relayed", "Through your VPN".
    pub mode: String,
    /// One line on what the mode means.
    pub mode_detail: String,
    /// Seconds since it connected.
    pub seconds: u64,
    /// My microphone is muted.
    pub muted: bool,
    /// I am not hearing the call.
    pub deafened: bool,
    /// Loudness of my microphone after processing, 0..1.
    pub my_level: f32,
    /// Whether my microphone is open (push-to-talk held, voice detected, open mic).
    pub sending: bool,
    /// Loudness of what they send, 0..1.
    pub their_level: f32,
    /// Their volume trim, dB.
    pub peer_volume_db: f32,
    /// A one-line quality reading ("Good", "12% lost").
    pub quality: Option<String>,
    /// How to talk in this transmit mode ("Hold Caps Lock to talk").
    pub talk_hint: Option<String>,
    /// Trust problems found at connect ("This key is new for this address").
    pub warning: Option<String>,
    /// The pictures on the call: theirs first, then my own previews.
    /// Empty for a voice-only call, which then looks as it always has.
    pub video: Vec<VideoTile>,
    /// My camera is on.
    pub camera_on: bool,
    /// My screen is being shared.
    pub sharing: bool,
    /// What this device can send: a camera button and a share button
    /// appear only for what it can actually do.
    pub can_camera: bool,
    /// This device can share its screen.
    pub can_share: bool,
    /// Moments the network damaged badly enough to be worth a second
    /// listen, as labels, newest first (spec §2 replay history; desktop
    /// only).
    pub replay: Vec<String>,
    /// A newly damaged moment is being offered ("Ask me" replay mode).
    pub replay_offer: bool,
    /// The soundboard's clip names, to play into the call (desktop only).
    pub sounds: Vec<String>,
}

/// One picture on a call: someone's camera or screen.
#[derive(Clone, Debug)]
pub struct VideoTile {
    /// Stable for the life of the picture ("their-screen", "my-camera"),
    /// so a picture chosen to be shown large stays chosen as frames come.
    pub key: String,
    /// Whose it is and what ("Sam's screen", "You").
    pub label: String,
    /// The picture. Built by the app only when a new frame arrives, so a
    /// redraw with nothing new uploads nothing.
    pub image: iced_core::image::Handle,
    /// Its width in pixels, for laying tiles out at the right shape.
    pub width: u32,
    /// Its height in pixels.
    pub height: u32,
    /// Mine, rather than theirs. The app mirrors my own camera before it
    /// gets here, the way a mirror shows a face.
    pub mine: bool,
    /// A screen rather than a camera: shown whole, never cropped.
    pub screen: bool,
}

/// Where a call is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    /// No call.
    Idle,
    /// Placing a call.
    Dialing,
    /// The call is up.
    Connected,
    /// It ended; why, for a moment.
    Ended(String),
}

impl Call {
    /// A call is up or being placed.
    pub fn active(&self) -> bool {
        matches!(self.phase, Phase::Dialing | Phase::Connected)
    }

    /// The call is with this contact.
    pub fn with(&self, id: &str) -> bool {
        self.active() && self.peer.as_deref() == Some(id)
    }
}

/// A conversation's messages, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    /// The contact it is with.
    pub person: String,
    /// The messages, oldest first.
    pub lines: Vec<Line>,
    /// Messages go live now (they are on a call).
    pub live: bool,
    /// Messages wait in the outbox and go when they are next reachable.
    pub queues: bool,
    /// Disappearing messages ("Messages vanish after 1 day").
    pub expiry: Option<String>,
    /// A voice message being recorded for them: seconds so far.
    pub recording: Option<u32>,
}

/// One message.
#[derive(Clone, Debug, Default)]
pub struct Line {
    /// The message's id within the conversation.
    pub id: u64,
    /// I sent it.
    pub mine: bool,
    /// Who wrote it; empty means the conversation's person (or me).
    pub author: String,
    /// The author's id (identity key, hex) where a conversation has more
    /// than two people in it; their avatar and taking a message down use
    /// it. Empty in a conversation with one person.
    pub author_id: String,
    /// What it says.
    pub text: String,
    /// "14:02".
    pub time: String,
    /// Shown above the first message of a new day ("Today", "Monday 26 October").
    pub day: Option<String>,
    /// How far it got, for my own messages.
    pub delivery: Delivery,
    /// Its sender changed it after sending.
    pub edited: bool,
    /// Kept at hand (pinned on this device only).
    pub pinned: bool,
    /// Taken down by its author or a moderator; drawn as a faint note.
    pub deleted: bool,
    /// (emoji, how many), in the order the row shows them. An emoji is a
    /// Unicode emoji, or a community's custom one written `:name:`.
    pub reactions: Vec<(String, u32)>,
    /// The emoji among [`reactions`](Self::reactions) that I used, so
    /// their chips show as mine and a tap takes mine off.
    pub reacted: Vec<String>,
    /// A file instead of text: (name, size).
    pub file: Option<(String, String)>,
    /// A voice message instead of text.
    pub voice: Option<VoiceClip>,
    /// The message this one answers, by id within the conversation; the
    /// screen quotes it above this one when it is still there.
    pub reply_to: Option<u64>,
}

/// A voice message in a conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceClip {
    /// How long it is.
    pub seconds: u32,
    /// Playing now.
    pub playing: bool,
    /// Still on its way from where it was left; it plays once here.
    pub fetching: bool,
}

/// How far a message of mine got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    /// Not mine, or nothing to say.
    None,
    /// Waiting in the outbox for them to be reachable.
    Queued,
    /// Handed to the connection, not confirmed yet.
    Sent,
    /// They have it.
    Delivered,
    /// It could not be sent.
    Failed,
}

/// File transfers.
#[derive(Clone, Debug, Default)]
pub struct Transfers {
    /// A file they offered, waiting for yes or no: (name, size).
    pub offer: Option<(String, String)>,
    /// The picture the sender made of the offered file, already decoded,
    /// shown beside the offer so a photo can be judged before taking it.
    /// `None` when they sent none.
    pub offer_thumbnail: Option<iced_core::image::Handle>,
    /// Transfers under way.
    pub active: Vec<Transfer>,
    /// Files waiting their turn, in the order they will go. One goes at a
    /// time: the call and the files share one connection.
    pub queued: Vec<QueuedFile>,
    /// Files can be chosen for a person with no call up: they join
    /// [`queued`](Self::queued) and go at the next call with them. The
    /// phone app keeps its queue across restarts by file handle; the
    /// desktop offers files only during a call.
    pub pick_anytime: bool,
}

/// A file waiting to be sent.
#[derive(Clone, Debug, Default)]
pub struct QueuedFile {
    /// Its place in the queue, stable while it waits.
    pub id: u64,
    /// The file's name.
    pub name: String,
    /// Its size, and who it is for when that isn't the person on screen
    /// ("4.2 MB · for Robin, on your next call").
    pub detail: String,
    /// Held back: skipped until let go, without losing its place.
    pub held: bool,
}

/// A file on its way.
#[derive(Clone, Debug, Default)]
pub struct Transfer {
    /// The file's name.
    pub name: String,
    /// Coming to me rather than going out.
    pub incoming: bool,
    /// 0..1.
    pub progress: f32,
    /// "2.1 MB of 8 MB · 1.2 MB/s".
    pub detail: String,
    /// Paused by the sender.
    pub paused: bool,
}

/// Whether people can reach this device.
#[derive(Clone, Debug, Default)]
pub struct Network {
    /// The reachability check's answers.
    pub checks: Vec<Check>,
    /// The port the door listens on.
    pub port: u16,
    /// The relay to be reachable through; empty for none.
    pub relay: String,
    /// A line on the relay actually in use when it is not simply yours
    /// (a fallback, the mesh's, a contact's); empty when there is nothing to say.
    pub relay_note: String,
    /// Whether contacts with no relay may listen at yours.
    pub lend_relay: bool,
    /// Whether anonymity mode is on.
    pub anonymity: bool,
    /// Its hops, comma-separated `host:port[#key]`.
    pub hops: String,
    /// The mailbox nodes on your delivery routes, in the order mail
    /// tries them.
    pub mailboxes: Vec<Mailbox>,
    /// Mailbox nodes found on this network at the last look, by address.
    pub found_nodes: Vec<String>,
    /// A look for nodes on this network is under way.
    pub finding_nodes: bool,
    /// The local API for scripts and bots is on (desktop only).
    pub local_api: bool,
    /// Where a script finds its port and token, while it is on.
    pub api_file: String,
}

/// A mailbox node on your delivery routes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mailbox {
    /// `host:port`.
    pub address: String,
    /// Which route it is on ("A node you trust", "Community node").
    pub route: String,
    /// How it answered at the last courier round, if it has.
    pub status: String,
}

/// One line of the reachability check.
#[derive(Clone, Debug, Default)]
pub struct Check {
    /// The question ("Being called").
    pub label: String,
    /// How it came out.
    pub state: CheckState,
    /// One line on what it means.
    pub detail: String,
}

/// How a reachability check came out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckState {
    /// Works.
    Good,
    /// Works with a caveat, or needs a hand.
    Warn,
    /// Does not work.
    Bad,
    #[default]
    /// Not checked yet.
    Unknown,
}

/// The settings the screens show and change.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// The theme's id.
    pub theme: String,
    /// The accent's id, or `custom`.
    pub accent: String,
    /// The custom accent as `#rrggbb`, used when `accent` is `custom`.
    pub custom_accent: String,
    /// The reading font's id (see [`crate::theme::FONTS`]).
    pub font: String,
    /// Microphones found.
    pub inputs: Vec<String>,
    /// Speakers and headphones found.
    pub outputs: Vec<String>,
    /// The chosen devices; empty is the system default.
    pub input: String,
    /// The chosen output device; empty is the system default.
    pub output: String,
    /// How the microphone decides to send.
    pub transmit: Transmit,
    /// The push-to-talk key, as a name ("Caps Lock").
    pub ptt_key: String,
    /// The mute shortcut's key, as a name; empty when unbound.
    pub mute_key: String,
    /// The deafen shortcut's key, as a name; empty when unbound.
    pub deafen_key: String,
    /// The hang-up shortcut's key, as a name; empty when unbound.
    pub hangup_key: String,
    /// Mute only while its key is held, rather than toggling.
    pub mute_hold: bool,
    /// Deafen only while its key is held, rather than toggling.
    pub deafen_hold: bool,
    /// The save-replay shortcut's key, as a name; empty when unbound.
    pub replay_key: String,
    /// The shortcut waiting for a key press to bind it.
    pub binding: Option<Hotkey>,
    /// What happens when their audio is damaged and recovered (desktop
    /// only).
    pub replay_mode: ReplayMode,
    /// The soundboard's clips (desktop only).
    pub sound_clips: Vec<SoundRow>,
    /// Noise suppression is on.
    pub noise_suppression: bool,
    /// Echo cancellation is on.
    pub echo_cancel: bool,
    /// "Low data" calls (spec §2 automatic bandwidth): voice held at
    /// 8 kbit/s and no video. On a phone, the data saver.
    pub low_data: bool,
    /// A shared screen's sound is sent with it (desktop only).
    pub screen_audio: bool,
    /// The plugin host's list (desktop only).
    pub plugins: Plugins,
    /// Automatic level is on.
    pub auto_gain: bool,
    /// The voice preset in use ("Natural", "Broadcast", ...).
    pub preset: String,
    /// The voice presets to choose from.
    pub presets: Vec<String>,
    /// Every stage of the microphone chain with its settings, in the
    /// order the sound runs through them, for Voice's "Fine-tune" tab.
    pub stages: Vec<StageRow>,
    /// The EQ's bands, for the same tab.
    pub bands: Vec<BandRow>,
    /// Voice presets can be saved to and loaded from files (desktop only).
    pub preset_files: bool,
    /// The microphone test is running, and its level.
    pub mic_test: Option<f32>,
    /// Master output volume, dB.
    pub output_db: f32,
    /// Animations are off.
    pub reduced_motion: bool,
    /// How large the whole interface is drawn, 1 being normal (desktop
    /// only; a phone follows the system's text size). `0` before it is
    /// known, which draws at normal size.
    pub text_scale: f32,
    /// Show a notification for messages while the window is not focused.
    pub notify_messages: bool,
    /// Open the door (listen for calls) when the app starts.
    pub open_on_start: bool,
    /// The app's version, for About.
    pub version: String,
    /// Where settings and history are kept, for About.
    pub data_dir: String,
}

/// Spec §1 "Plugin Host", as Settings shows it (desktop only).
#[derive(Clone, Debug, Default)]
pub struct Plugins {
    /// The plugins in the microphone chain, in the order they run.
    pub chain: Vec<PluginRow>,
    /// What was found on this computer, as "Name · Format", once looked
    /// for; `None` before.
    pub found: Option<Vec<String>>,
    /// Looking is under way.
    pub scanning: bool,
}

/// One plugin in the chain.
#[derive(Clone, Debug, Default)]
pub struct PluginRow {
    pub name: String,
    /// "LADSPA", "LV2" or "VST3".
    pub format: String,
    pub enabled: bool,
    /// Why it isn't running, when it isn't (gone, failed to start).
    pub problem: Option<String>,
    pub params: Vec<PluginParam>,
}

/// One of a plugin's parameters.
#[derive(Clone, Debug, Default)]
pub struct PluginParam {
    pub id: String,
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub value: f32,
    /// An on/off switch rather than a range.
    pub toggle: bool,
}

/// One stage of the microphone chain, as the "Fine-tune" tab shows it.
#[derive(Clone, Debug, Default)]
pub struct StageRow {
    /// The stage's id, handed back in [`crate::Action::StageOn`].
    pub id: &'static str,
    /// Its name on screen.
    pub name: &'static str,
    /// What it does, in a sentence.
    pub help: &'static str,
    /// It is on.
    pub enabled: bool,
    /// Its settings, in the order they are shown.
    pub params: Vec<ParamRow>,
    /// How far it is turning the voice down right now, in dB, while a
    /// call is running; `None` for stages with nothing to show or with no
    /// call.
    pub reduction_db: Option<f32>,
}

/// One setting of a chain stage or EQ band.
#[derive(Clone, Debug, Default)]
pub struct ParamRow {
    /// The setting's id, handed back in [`crate::Action::StageParam`] or
    /// [`crate::Action::BandParam`].
    pub id: &'static str,
    /// The slider's label.
    pub name: &'static str,
    /// The sentence behind it.
    pub help: &'static str,
    /// The unit beside the value ("dB", "Hz", "ms", or empty).
    pub unit: &'static str,
    /// The slider's lowest value.
    pub min: f32,
    /// The slider's highest value.
    pub max: f32,
    /// How far one step moves.
    pub step: f32,
    /// The value now.
    pub value: f32,
}

/// One band of the EQ.
#[derive(Clone, Debug, Default)]
pub struct BandRow {
    /// Its shape, as a name ("Bell", "Low shelf", ...).
    pub shape: &'static str,
    /// It is on.
    pub enabled: bool,
    /// Its settings.
    pub params: Vec<ParamRow>,
}

/// On-device live captions (spec §7), as Settings and the call view show
/// them.
#[derive(Clone, Debug, Default)]
pub struct Captions {
    /// Captions are switched on (they run only during a call).
    pub enabled: bool,
    /// Your own voice is captioned too.
    pub own_voice: bool,
    /// The speech model is on this computer.
    pub installed: bool,
    /// This build can fetch the model itself (it carries the model's
    /// checksums).
    pub can_download: bool,
    /// How big the download is and where from, as words.
    pub size: String,
    /// How far the download is, 0 to 1, while it runs.
    pub downloading: Option<f32>,
    /// The outcome of the last download, import or removal.
    pub message: String,
    /// What the recogniser is doing, when there is something to say.
    pub status: String,
    /// The recogniser is running.
    pub live: bool,
    /// The last few captions, oldest first, for the call view.
    pub lines: Vec<CaptionLine>,
    /// This call has captions to save.
    pub have_transcript: bool,
}

/// One caption in the call view.
#[derive(Clone, Debug, Default)]
pub struct CaptionLine {
    /// Who said it.
    pub who: String,
    /// What they said.
    pub text: String,
}

/// What the person asks of live captions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptionsDo {
    /// Switch captions on or off.
    On(bool),
    /// Caption your own voice too, or not.
    OwnVoice(bool),
    /// Fetch the speech model.
    Download,
    /// Stop fetching it.
    CancelDownload,
    /// Install the model from files already on this computer.
    Import,
    /// Delete the model from this computer.
    RemoveModel,
    /// Save this call's captions to a text file.
    SaveTranscript,
}

/// What happens when the network damages their audio badly enough that
/// a moment is worth hearing again (spec §2 replay modes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplayMode {
    /// Keep the moments to play from the call, and say nothing.
    #[default]
    Off,
    /// Offer to play each one.
    Ask,
    /// Play each one straight away.
    Auto,
}

impl ReplayMode {
    /// Every mode, in the order Settings offers them.
    pub const ALL: [ReplayMode; 3] = [ReplayMode::Off, ReplayMode::Ask, ReplayMode::Auto];

    /// The mode as Settings names it.
    pub fn label(self) -> &'static str {
        match self {
            ReplayMode::Off => "Just keep them",
            ReplayMode::Ask => "Ask me",
            ReplayMode::Auto => "Play at once",
        }
    }
}

/// One soundboard clip, as Settings shows it.
#[derive(Clone, Debug, Default)]
pub struct SoundRow {
    /// Its name (the file's, when added).
    pub name: String,
    /// How much louder or quieter it plays, dB.
    pub gain_db: f32,
}

/// Something to do with the replay history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayDo {
    /// Choose what happens when a moment is damaged.
    Mode(ReplayMode),
    /// Play the kept moment at this index (newest first).
    Play(usize),
    /// Save what you just heard to a sound file.
    Save,
    /// Forget the kept moments.
    Clear,
    /// Close the offer to play a new one.
    Dismiss,
}

/// Something to do with the soundboard.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SoundDo {
    /// Add a clip from a WAV file.
    Add,
    /// Take the clip at this index off the soundboard.
    Remove(usize),
    /// Hear the clip at this index yourself.
    Preview(usize),
    /// Play the clip at this index into the call.
    Send(usize),
    /// Set how much louder or quieter the clip at this index plays, dB.
    Gain(usize, f32),
}

/// A keyboard shortcut that works while Stoop is in the background
/// (desktop only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hotkey {
    /// Held to talk, in push-to-talk.
    Talk,
    /// Mutes or unmutes your microphone in a call.
    Mute,
    /// Mutes or unmutes everyone else in a call.
    Deafen,
    /// Leaves the call.
    HangUp,
    /// Saves what you just heard to a sound file.
    Replay,
}

impl Hotkey {
    /// The shortcuts besides push to talk, in the order Settings lists
    /// them.
    pub const OTHERS: [Hotkey; 4] = [Hotkey::Mute, Hotkey::Deafen, Hotkey::HangUp, Hotkey::Replay];

    /// The shortcut as Settings names it.
    pub fn label(self) -> &'static str {
        match self {
            Hotkey::Talk => "Talk key",
            Hotkey::Mute => "Mute",
            Hotkey::Deafen => "Deafen",
            Hotkey::HangUp => "Hang up",
            Hotkey::Replay => "Save replay",
        }
    }

    /// What the shortcut does, in a sentence.
    pub fn help(self) -> &'static str {
        match self {
            Hotkey::Talk => "Hold it to talk. It works while Stoop is in the background.",
            Hotkey::Mute => "Turns your microphone off or back on during a call.",
            Hotkey::Deafen => "Stops you hearing the call, and them hearing you, or brings both back.",
            Hotkey::HangUp => "Leaves the call.",
            Hotkey::Replay => "Saves what you just heard in the call to a sound file.",
        }
    }
}

impl Settings {
    /// The key bound to `hotkey`, as a name; empty when unbound.
    pub fn key(&self, hotkey: Hotkey) -> &str {
        match hotkey {
            Hotkey::Talk => &self.ptt_key,
            Hotkey::Mute => &self.mute_key,
            Hotkey::Deafen => &self.deafen_key,
            Hotkey::HangUp => &self.hangup_key,
            Hotkey::Replay => &self.replay_key,
        }
    }

    /// Whether `hotkey` acts only while held: `None` for shortcuts that
    /// have no such choice.
    pub fn hold(&self, hotkey: Hotkey) -> Option<bool> {
        match hotkey {
            Hotkey::Mute => Some(self.mute_hold),
            Hotkey::Deafen => Some(self.deafen_hold),
            Hotkey::Talk | Hotkey::HangUp | Hotkey::Replay => None,
        }
    }
}

/// How the microphone decides to send.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transmit {
    /// Only while a key is held.
    PushToTalk,
    #[default]
    /// When the microphone hears speech.
    Voice,
    /// All the time.
    Open,
}

impl Transmit {
    /// Every mode, in the order Settings offers them.
    pub const ALL: [Transmit; 3] = [Transmit::Voice, Transmit::PushToTalk, Transmit::Open];

    /// The mode as Settings names it.
    pub fn label(self) -> &'static str {
        match self {
            Transmit::PushToTalk => "Push to talk",
            Transmit::Voice => "When I speak",
            Transmit::Open => "Always on",
        }
    }
}

impl std::fmt::Display for Transmit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// A group chat or a community, as the list and its screen show it.
#[derive(Clone, Debug, Default)]
pub struct Community {
    /// Its id (hex), stable for its whole life.
    pub id: String,
    /// Its name.
    pub name: String,
    /// A group chat (one conversation, anyone may invite) rather than a
    /// community (channels and roles).
    pub group: bool,
    /// A line under the name ("Community · 6 members").
    pub detail: String,
    /// Its channels, in order; a group chat has the one.
    pub channels: Vec<Channel>,
    /// Who is in it.
    pub members: Vec<Member>,
    /// Messages from others not read yet.
    pub unread: u32,
    /// Why it can't be used yet, or any more ("Waiting to be let in",
    /// "You're no longer in this"); `None` while you're in it.
    pub waiting: Option<String>,
    /// You may post in it.
    pub can_post: bool,
    /// You may invite people.
    pub can_invite: bool,
    /// You may add channels, and make one private or open.
    pub can_manage_channels: bool,
    /// Its roles, most powerful first; the last is the one newcomers get.
    /// Empty for a group chat.
    pub roles: Vec<Role>,
    /// You may change what the roles are and what each may do (owners).
    pub can_edit_roles: bool,
    /// You may give members roles below your own.
    pub can_assign_roles: bool,
    /// Slow mode: the fewest seconds between one member's posts; 0 is off.
    pub slow_mode_secs: u32,
    /// You may turn slow mode on or off (the community's settings).
    pub can_set_slow_mode: bool,
    /// The mailbox node its board is on, if it has one (spec §11
    /// "Communities at full scale").
    pub board: Option<String>,
    /// You can act on reports: an owner, or allowed to remove messages or
    /// people.
    pub moderator: bool,
    /// Reports sent to you as a moderator, oldest first.
    pub reports: Vec<Report>,
    /// Invitations you sent that haven't been answered.
    pub pending_invites: Vec<PendingInvite>,
    /// The voice channel of this community you are in, if any.
    pub voice_room: Option<u32>,
    /// Anyone with its join link may ask to join (it can be listed).
    pub listed: bool,
    /// Its join link, when it is listed and you may invite people.
    pub join_link: Option<String>,
    /// Which of its messages notify you.
    pub notify: Notify,
    /// Its custom emoji's names, written `:name:`; their pictures are
    /// registered in [`crate::avatars`] under [`crate::avatars::emoji_id`].
    pub emoji: Vec<String>,
    /// You may change its settings, its emoji among them.
    pub can_manage: bool,
    /// The spaces it shares with other communities.
    pub shared_spaces: Vec<SharedSpace>,
    /// On a shared space: how many communities share it; 0 for an
    /// ordinary community.
    pub sharing: usize,
    /// The last link made for another community's admin to bring their
    /// community into a space this one shares.
    pub ally_link: Option<String>,
}

/// A space a community shares with other communities (spec §12 Q4).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SharedSpace {
    /// Its id (hex).
    pub id: String,
    /// What it is called.
    pub name: String,
    /// You are in it.
    pub joined: bool,
}

/// Messages a member reported to the moderators.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Its id, for dismissing it.
    pub id: u64,
    /// Who reported it.
    pub from: String,
    /// Which channel, without the hash.
    pub channel: String,
    /// What they said about it.
    pub reason: String,
    /// When ("2 h ago").
    pub when: String,
    /// The messages, as the reporter's app read them.
    pub excerpts: Vec<Excerpt>,
}

/// One reported message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Excerpt {
    /// The message's id.
    pub id: u64,
    /// Who wrote it.
    pub author: String,
    /// Their member id (hex).
    pub author_id: String,
    /// What it said, by the reporter's copy.
    pub text: String,
    /// Your own copy says the same (`Some(true)`), something else
    /// (`Some(false)`), or you don't have it (`None`).
    pub verified: Option<bool>,
    /// You may take it down.
    pub can_remove: bool,
    /// You may remove its author from the community.
    pub can_kick: bool,
    /// You may ban its author.
    pub can_ban: bool,
}

/// An invitation you sent that is still open.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PendingInvite {
    /// Their id (hex).
    pub id: String,
    /// What you call them.
    pub name: String,
    /// When it was sent ("3 d ago").
    pub when: String,
}

/// One role of a community, as the roles dialog shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Role {
    /// What it is called ("Moderator").
    pub name: String,
    /// Each thing a role can be allowed, in a fixed order, with whether
    /// this one is: ("Invite people", true).
    pub perms: Vec<(String, bool)>,
    /// How many members hold it.
    pub holders: usize,
}

/// Which messages notify you, in a space or one channel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Notify {
    /// Every message.
    #[default]
    All,
    /// Only those that mention you by name.
    Mentions,
    /// None (they are still counted as unread).
    Nothing,
}

/// One channel of a community.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    /// Its number within the community.
    pub id: u32,
    /// Its name, without the hash.
    pub name: String,
    /// What it is for, if anyone said.
    pub topic: String,
    /// Its own notification setting; `None` follows the space's.
    pub notify: Option<Notify>,
    /// Only the members in [`Channel::readers`] (and the owners) get its
    /// messages; everyone else doesn't see it at all.
    pub private: bool,
    /// Who can read a private channel, by member id (hex); empty for an
    /// open one.
    pub readers: Vec<String>,
    /// A voice channel: a standing call people drop into.
    pub voice: bool,
    /// Who is in it now, by name, for a voice channel.
    pub in_voice: Vec<String>,
}

/// Someone in a group chat or community.
#[derive(Clone, Debug, Default)]
pub struct Member {
    /// Their key there (hex): their avatar seed.
    pub id: String,
    /// The name they go by there.
    pub name: String,
    /// Their status line there.
    pub status: String,
    /// Their role ("Owner", "Moderator", "Member").
    pub role: String,
    /// It is me.
    pub me: bool,
    /// Their role's place in [`Community::roles`]; `None` for an owner.
    pub role_index: Option<usize>,
    /// They hold the role newcomers get, which says nothing about them, so
    /// no badge shows it.
    pub plain: bool,
    /// The roles (places in [`Community::roles`]) you may give them;
    /// empty when you can't change their role.
    pub assignable: Vec<usize>,
}

/// An invitation to a group chat or community.
#[derive(Clone, Debug, Default)]
pub struct SpaceInvite {
    /// What it is called.
    pub name: String,
    /// Who invited you, and when ("Community · from Robin · 2 h ago").
    pub detail: String,
}

/// The messages of one channel.
#[derive(Clone, Debug, Default)]
pub struct ChannelView {
    /// The community's id.
    pub community: String,
    /// The channel's number.
    pub channel: u32,
    /// The messages, oldest first; [`Line::author`] and
    /// [`Line::author_id`] say who wrote each.
    pub lines: Vec<Line>,
}
