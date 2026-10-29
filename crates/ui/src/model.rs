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
    /// Display name.
    pub name: String,
    /// Status line ("Around this evening").
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
    /// (emoji, how many).
    pub reactions: Vec<(String, u32)>,
    /// A file instead of text: (name, size).
    pub file: Option<(String, String)>,
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
    /// Transfers under way.
    pub active: Vec<Transfer>,
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
    /// The accent's id.
    pub accent: String,
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
    /// Waiting for a key press to bind push-to-talk.
    pub ptt_capturing: bool,
    /// Noise suppression is on.
    pub noise_suppression: bool,
    /// Echo cancellation is on.
    pub echo_cancel: bool,
    /// Automatic level is on.
    pub auto_gain: bool,
    /// The voice preset in use ("Natural", "Broadcast", ...).
    pub preset: String,
    /// The voice presets to choose from.
    pub presets: Vec<String>,
    /// The microphone test is running, and its level.
    pub mic_test: Option<f32>,
    /// Master output volume, dB.
    pub output_db: f32,
    /// Animations are off.
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
