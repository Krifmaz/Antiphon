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
    /// (emoji, how many).
    pub reactions: Vec<(String, u32)>,
    /// A file instead of text: (name, size).
    pub file: Option<(String, String)>,
    /// A voice message instead of text.
    pub voice: Option<VoiceClip>,
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
    /// Waiting for a key press to bind push-to-talk.
    pub ptt_capturing: bool,
    /// Noise suppression is on.
    pub noise_suppression: bool,
    /// Echo cancellation is on.
    pub echo_cancel: bool,
    /// "Low data" calls (spec §2 automatic bandwidth): voice held at
    /// 8 kbit/s and no video. On a phone, the data saver.
    pub low_data: bool,
    /// A shared screen's sound is sent with it (desktop only).
    pub screen_audio: bool,
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
