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
    pub me: Me,
    pub people: Vec<Person>,
    pub call: Call,
    /// The conversation with the person on screen, once the app has loaded
    /// it (after [`crate::Action::Open`]).
    pub conversation: Option<Conversation>,
    pub transfers: Transfers,
    pub network: Network,
    pub settings: Settings,
    /// A line across the top of the window: something the person should
    /// know now ("Your microphone is blocked").
    pub notice: Option<String>,
    /// The first-run screen, until the person has picked a name.
    pub first_run: bool,
    /// What this device can do.
    pub device: Device,
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
}

/// The person using the app.
#[derive(Default)]
pub struct Me {
    pub name: String,
    pub status: String,
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
    pub name: String,
    /// Where they were last reached.
    pub address: String,
    /// The fingerprint was compared out loud and matched.
    pub verified: bool,
    pub presence: Presence,
    /// The last message, one line.
    pub last: Option<String>,
    /// When it was, short ("14:02", "Tue").
    pub last_at: Option<String>,
    pub unread: u32,
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
}

/// What a person last said about being around.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Presence {
    Online,
    Away,
    Busy,
    /// Said nothing, or has not been reached this session.
    #[default]
    Unknown,
}

/// The call, if any.
#[derive(Clone, Debug, Default)]
pub struct Call {
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
    pub muted: bool,
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
}

/// Where a call is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Idle,
    /// Placing a call.
    Dialing,
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
    pub lines: Vec<Line>,
    /// Messages go live now (they are on a call).
    pub live: bool,
    /// Messages wait in the outbox and go when they are next reachable.
    pub queues: bool,
    /// Disappearing messages ("Messages vanish after 1 day").
    pub expiry: Option<String>,
}

/// One message.
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub id: u64,
    pub mine: bool,
    pub author: String,
    pub text: String,
    /// "14:02".
    pub time: String,
    /// Shown above the first message of a new day ("Today", "Monday 26 October").
    pub day: Option<String>,
    pub delivery: Delivery,
    pub edited: bool,
    pub pinned: bool,
    /// (emoji, how many).
    pub reactions: Vec<(String, u32)>,
    /// A file instead of text: (name, size).
    pub file: Option<(String, String)>,
}

/// How far a message of mine got.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Delivery {
    #[default]
    None,
    /// Waiting in the outbox for them to be reachable.
    Queued,
    Sent,
    Delivered,
    Failed,
}

/// File transfers.
#[derive(Clone, Debug, Default)]
pub struct Transfers {
    /// A file they offered, waiting for yes or no: (name, size).
    pub offer: Option<(String, String)>,
    pub active: Vec<Transfer>,
}

/// A file on its way.
#[derive(Clone, Debug, Default)]
pub struct Transfer {
    pub name: String,
    pub incoming: bool,
    /// 0..1.
    pub progress: f32,
    /// "2.1 MB of 8 MB · 1.2 MB/s".
    pub detail: String,
    pub paused: bool,
}

/// Whether people can reach this device.
#[derive(Clone, Debug, Default)]
pub struct Network {
    pub checks: Vec<Check>,
    pub port: u16,
    pub relay: String,
}

/// One line of the reachability check.
#[derive(Clone, Debug, Default)]
pub struct Check {
    pub label: String,
    pub state: CheckState,
    pub detail: String,
}

/// How a reachability check came out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CheckState {
    Good,
    Warn,
    Bad,
    #[default]
    Unknown,
}

/// The settings the screens show and change.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    pub theme: String,
    pub accent: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
    /// The chosen devices; empty is the system default.
    pub input: String,
    pub output: String,
    pub transmit: Transmit,
    /// The push-to-talk key, as a name ("Caps Lock").
    pub ptt_key: String,
    /// Waiting for a key press to bind push-to-talk.
    pub ptt_capturing: bool,
    pub noise_suppression: bool,
    pub echo_cancel: bool,
    pub auto_gain: bool,
    /// The voice preset in use ("Natural", "Broadcast", ...).
    pub preset: String,
    pub presets: Vec<String>,
    /// The microphone test is running, and its level.
    pub mic_test: Option<f32>,
    /// Master output volume, dB.
    pub output_db: f32,
    pub reduced_motion: bool,
    /// Show a notification for messages while the window is not focused.
    pub notify_messages: bool,
    /// Open the door (listen for calls) when the app starts.
    pub open_on_start: bool,
    /// The app's version, for About.
    pub version: String,
    /// Where settings and history are kept, for About.
    pub data_dir: String,
}

/// How the microphone decides to send.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transmit {
    PushToTalk,
    #[default]
    Voice,
    Open,
}

impl Transmit {
    pub const ALL: [Transmit; 3] = [Transmit::Voice, Transmit::PushToTalk, Transmit::Open];

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
