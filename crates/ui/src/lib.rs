//! Stoop's interface: one set of screens for the desktop and the phone.
//!
//! The shape is Encore's (an Elm-style state, message and view, a palette
//! of tokens read by plain style functions, a small widget toolbox), split
//! so the screens never touch an engine:
//!
//! - the app builds a [`Model`] from its own state,
//! - [`Ui::view`] draws it,
//! - [`Ui::update`] handles a [`Msg`], keeps what is purely the
//!   interface's own (which screen is open, what is typed, which dialog is
//!   up), and hands back the [`Action`]s the app must carry out.
//!
//! The same [`Ui`] lays itself out for the window it is in: the people
//! list, the conversation and a details panel side by side on a wide
//! window, and one screen at a time with a tab bar on a narrow one
//! ([`theme::NARROW`]), which is what a phone gets.

pub mod a11y;
pub mod avatars;
pub mod emoji;
pub mod icons;
pub mod markup;
pub mod model;
pub mod profile_layout;
pub mod theme;
pub mod theme_file;
pub mod widgets;

mod screens;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use iced_core::animation::{Animation, Easing};
use iced_core::{Point, Size};

pub use avatars::PROFILE_COLORS;
pub use model::*;
pub use profile_layout::{ProfileBlock, ProfileLayout, QrPlace};

/// An element of this interface: the default theme, and whichever renderer
/// the app's build of iced provides.
pub type El<'a, M> = iced_core::Element<'a, M, iced_core::Theme, iced_widget::Renderer>;

/// The composer's widget id, so an app can focus it when a conversation opens.
pub const COMPOSER: &str = "stoop-composer";
/// The message edit box's widget id, focused when an edit starts.
pub const EDIT_BOX: &str = "stoop-edit";
/// The people search box's widget id, for a find shortcut.
pub const SEARCH: &str = "stoop-search";

/// The shortest passphrase a backup accepts. The same number as
/// `antiphon_crypto::account_seal::MIN_PASSPHRASE_CHARS`, which the apps
/// check at compile time; repeated here so the interface can say so
/// without depending on the crypto crate.
pub const MIN_PASSPHRASE_CHARS: usize = 8;

/// A screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// The front step: your door, invite, people.
    Home,
    /// A conversation.
    Person(String),
    /// A part of Settings.
    Settings(Section),
    /// The phone's list of settings sections, grouped, each opening its
    /// [`Route::Settings`] page. A wide window has no such page: it shows
    /// the list beside a section, so it draws this as the first section.
    SettingsMenu,
    /// Group chats and communities: the list, invitations, and making one.
    Spaces,
    /// One channel of a group chat or community: (its id, the channel).
    Community(String, u32),
    /// A profile page: banner, picture, name, pronouns and About. `None`
    /// is my own, with the way into [`Route::ProfileEdit`]; `Some` is a
    /// contact's, by id. Both layouts show it as a page of its own: the
    /// phone full screen, a wide window in the main area.
    Profile(Option<String>),
    /// Editing my profile: picture, colour, name, pronouns, status and
    /// About (saved together), and the page's arrangement and QR code
    /// (saved as they change).
    ProfileEdit,
}

/// A part of Settings. The sections sit in [`Group`]s, as Discord's
/// settings do: a sidebar of grouped sections on a wide window, a grouped
/// list on a phone. Some sections split their cards into tabs (see
/// [`Section::tabs`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Your name, pronouns, status, About line, picture and colour.
    Profile,
    /// Keeping the account: your identity, backup files and the recovery
    /// phrase.
    Account,
    /// Moving the account to another device.
    Devices,
    /// Anonymity routing and what may act as you (the local API).
    Privacy,
    /// Which notifications Stoop shows.
    Notifications,
    /// Devices, how your voice is sent, processing and plugins.
    Voice,
    /// Theme, accent, reading font and motion.
    Appearance,
    /// Low data calls, and on a phone, battery and mobile data.
    Data,
    /// Whether people can reach you: the reachability check and the port.
    Network,
    /// The relay to fall back on and the mailbox nodes that hold mail.
    Relays,
    /// Looking for and installing newer versions.
    Updates,
    /// Version and where data is kept.
    About,
}

/// A heading that gathers related [`Section`]s in the settings navigation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    /// Who you are and how you keep your account.
    User,
    /// How the app looks, sounds and behaves on this device.
    App,
    /// How you reach people and they reach you.
    Connection,
    /// The app itself: updates and version.
    Stoop,
}

impl Group {
    /// Every group, in the order the navigation lists them.
    pub const ALL: [Group; 4] = [Group::User, Group::App, Group::Connection, Group::Stoop];

    /// The group's heading.
    pub fn label(self) -> &'static str {
        match self {
            Group::User => "User settings",
            Group::App => "App settings",
            Group::Connection => "Connection",
            Group::Stoop => "Stoop",
        }
    }

    /// The group's sections, in order.
    pub fn sections(self) -> impl Iterator<Item = Section> {
        Section::ALL.into_iter().filter(move |s| s.group() == self)
    }
}

impl Section {
    /// Every section, in the order the navigation lists them.
    pub const ALL: [Section; 12] = [
        Section::Profile,
        Section::Account,
        Section::Devices,
        Section::Privacy,
        Section::Notifications,
        Section::Voice,
        Section::Appearance,
        Section::Data,
        Section::Network,
        Section::Relays,
        Section::Updates,
        Section::About,
    ];

    /// The section's name as the navigation shows it.
    pub fn label(self) -> &'static str {
        match self {
            Section::Profile => "Profile",
            Section::Account => "Account & backup",
            Section::Devices => "Devices",
            Section::Privacy => "Privacy",
            Section::Notifications => "Notifications",
            Section::Voice => "Voice & video",
            Section::Appearance => "Appearance",
            Section::Data => "Data & battery",
            Section::Network => "Network",
            Section::Relays => "Relays & mailboxes",
            Section::Updates => "Updates",
            Section::About => "About",
        }
    }

    /// The icon beside the section's name.
    pub fn icon(self) -> icons::Icon {
        match self {
            Section::Profile => icons::Icon::Users,
            Section::Account => icons::Icon::Lock,
            Section::Devices => icons::Icon::Screen,
            Section::Privacy => icons::Icon::Shield,
            Section::Notifications => icons::Icon::Bell,
            Section::Voice => icons::Icon::Mic,
            Section::Appearance => icons::Icon::Palette,
            Section::Data => icons::Icon::Sliders,
            Section::Network => icons::Icon::Globe,
            Section::Relays => icons::Icon::Door,
            Section::Updates => icons::Icon::Download,
            Section::About => icons::Icon::Info,
        }
    }

    /// The group the section is listed under.
    pub fn group(self) -> Group {
        match self {
            Section::Profile | Section::Account | Section::Devices | Section::Privacy => Group::User,
            Section::Notifications | Section::Voice | Section::Appearance | Section::Data => Group::App,
            Section::Network | Section::Relays => Group::Connection,
            Section::Updates | Section::About => Group::Stoop,
        }
    }

    /// The tabs the section's cards are split into, by name; empty when
    /// the section is short enough to be one page. `desktop` adds the
    /// tabs only a computer has (audio plugins).
    pub fn tabs(self, desktop: bool) -> &'static [&'static str] {
        match self {
            Section::Account => &["Identity", "Backup file", "Recovery phrase"],
            Section::Voice if desktop => &["Input & output", "Processing", "Plugins"],
            Section::Voice => &["Input & output", "Processing"],
            Section::Appearance => &["Theme", "Text & motion"],
            Section::Relays => &["Relay", "Mailboxes"],
            _ => &[],
        }
    }
}

/// A dialog over the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dialog {
    /// Your invite: the QR code and link.
    Invite,
    /// Add someone from their invite.
    AddPerson,
    /// Are you sure: remove this contact.
    ConfirmRemove(String),
    /// Are you sure: block this contact.
    ConfirmBlock(String),
    /// Are you sure: replace this device's identity with the one the
    /// recovery phrase in the box restores.
    ConfirmRestore,
    /// Are you sure: replace this device's account with a backup file
    /// (opened with the passphrase in the box).
    ConfirmImport,
    /// Are you sure: replace this device's account with the one the move
    /// link in the box offers.
    ConfirmReceive,
    /// Are you sure: remove this device (by device key, hex) from the account.
    ConfirmRevokeDevice(String),
    /// Start a group chat or community.
    NewSpace,
    /// Invite one of your people into this group chat or community (by id).
    InviteToSpace(String),
    /// Add a channel to this community (by id).
    AddChannel(String),
    /// Who can read this channel of this community (by id).
    ChannelSettings(String, u32),
    /// This community's roles and who holds which (by id).
    Roles(String),
    /// Slow mode, reports and open invitations in this community (by id).
    Moderation(String),
    /// The spaces this community shares with other communities (by id).
    SharedSpaces(String),
    /// Find communities in the directories you added.
    Directory,
    /// Report a message (community id, channel, message id) and any others
    /// picked with it.
    Report(String, u32, u64),
    /// Are you sure: leave this group chat or community (by id).
    ConfirmLeave(String),
    /// Which messages notify you in this group chat or community (by id).
    SpaceNotify(String),
    /// Pick one of this community's emoji to put in the message (by id).
    EmojiPicker(String),
    /// This community's custom emoji: see, add and remove them (by id).
    SpaceEmoji(String),
    /// Every emoji to react to a message with, and the community's own
    /// when the message is in one.
    ReactPicker(ReactTarget),
}

/// The message a reaction picked in the picker goes on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReactTarget {
    /// A message in a conversation with one person.
    Chat {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
    },
    /// A message in a group chat or community channel.
    Post {
        /// The group chat or community's id.
        community: String,
        /// The channel the message is in.
        channel: u32,
        /// Which message.
        id: u64,
        /// Who wrote it (their key there, hex).
        author: String,
    },
}

impl ReactTarget {
    /// The action that puts `emoji` on this message, or takes mine off.
    pub fn action(&self, emoji: String) -> Action {
        match self {
            ReactTarget::Chat { to, id } => Action::React { to: to.clone(), id: *id, emoji },
            ReactTarget::Post { community, channel, id, author } => {
                Action::ReactPost { community: community.clone(), channel: *channel, id: *id, author: author.clone(), emoji }
            }
        }
    }

    /// The community whose custom emoji the picker offers, if any.
    pub fn community(&self) -> Option<&str> {
        match self {
            ReactTarget::Chat { .. } => None,
            ReactTarget::Post { community, .. } => Some(community),
        }
    }
}

/// How a toast is coloured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    /// Neutral news.
    Info,
    /// Something worked.
    Ok,
    /// Worth knowing, nothing broke.
    Warn,
    /// Something failed.
    Err,
}

/// A short message in the corner.
#[derive(Clone, Debug)]
pub struct Toast {
    /// Stable id, so a click can dismiss this toast and no other.
    pub id: u64,
    /// What it says.
    pub text: String,
    /// Its colour and icon.
    pub kind: ToastKind,
    /// When it appeared, for its time-out.
    pub at: Instant,
}

impl Toast {
    /// How long a toast stays; errors stay twice as long.
    fn life(&self) -> Duration {
        if self.kind == ToastKind::Err {
            Duration::from_millis(8000)
        } else {
            Duration::from_millis(4000)
        }
    }
}

/// What the app must do for the person.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// A conversation is on screen: load it and mark it read.
    Open(String),
    /// Open or close the door (listen for calls).
    Door(bool),
    /// Call a contact.
    Call(String),
    /// Bring a contact into the call that is running (spec §3 group calls).
    AddToCall(String),
    /// Call an address that is not a contact.
    CallAddress(String),
    /// Leave the call, or stop placing it.
    Hangup,
    /// Stop or resume sending my voice.
    Mute(bool),
    /// Stop or resume hearing the call.
    Deafen(bool),
    /// The person on the call: volume trim, dB (for someone not saved).
    PeerVolume(f32),
    /// How loud this contact is for you, dB; remembered for every call.
    Volume(String, f32),
    /// Silence this contact for you only, or undo it; remembered.
    MuteFor(String, bool),
    /// Send a message.
    Send {
        /// The contact it goes to.
        to: String,
        /// What it says.
        text: String,
        /// The message of theirs or mine it answers, by id, when it was
        /// written as a reply; drawn above it as a quote.
        reply_to: Option<u64>,
    },
    /// Change the words of a message you sent.
    Edit {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
        /// The new words.
        text: String,
    },
    /// Take back a message you sent, on both sides.
    Delete {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
    },
    /// Add an emoji reaction to a message, or take yours off.
    React {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
        /// The emoji.
        emoji: String,
    },
    /// Pin a message to the top of the conversation, or unpin it.
    Pin {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
        /// True to pin, false to unpin.
        pinned: bool,
    },
    /// Choose a file and offer it to the person on the call.
    SendFile,
    /// Start recording a voice message for this contact.
    RecordVoice(String),
    /// Stop recording: send it (true) or throw it away.
    FinishVoice {
        /// The contact it is for.
        to: String,
        /// Send it rather than drop it.
        send: bool,
    },
    /// Play a voice message in a conversation, or stop it if it is playing.
    PlayVoice {
        /// The contact the conversation is with.
        to: String,
        /// Which message.
        id: u64,
    },
    /// Take the file the person on the call offered.
    AcceptFile,
    /// Turn the offered file down.
    DeclineFile,
    /// Stop the file on its way, in either direction.
    CancelTransfer,
    /// Move a waiting file one place earlier (true) or later.
    QueueMove {
        /// Which file ([`QueuedFile::id`]).
        id: u64,
        /// Earlier rather than later.
        up: bool,
    },
    /// Hold a waiting file back (true), or let it go again.
    QueueHold {
        /// Which file.
        id: u64,
        /// Hold it.
        held: bool,
    },
    /// Take a file out of the queue without sending it.
    QueueRemove(u64),
    /// Add a contact from an invite link (or `host:port`).
    AddPerson {
        /// What to call them.
        name: String,
        /// Their invite link or `host:port`.
        invite: String,
    },
    /// Remove a contact (their history stays on disk).
    Remove(String),
    /// Block a contact: calls from them are refused and they are not told.
    Block(String),
    /// Mark a contact verified (the words matched), or take it back.
    Verify(String, bool),
    /// Give a contact a new name: (id, name).
    Rename(String, String),
    /// Save a private note about a contact: (id, note).
    Note(String, String),
    /// Make a new invite link (the old one stops working for new people).
    RefreshInvite,
    /// Put text on the clipboard.
    Copy(String),
    /// My display name, sent at the start of every call.
    SetName(String),
    /// My status line, sent at the start of every call.
    SetStatus(String),
    /// Choose a new profile picture (the app opens a file picker).
    PickAvatar,
    /// Go back to the initial-letter avatar.
    RemoveAvatar,
    /// Save my About line.
    SetBio(String),
    /// Save my pronouns ("she/her"), shown beside my name; empty for none.
    SetPronouns(String),
    /// Save how my own profile page is arranged: its cards' order, which
    /// are hidden, and where my QR code sits. Kept on this device only.
    SetProfileLayout(ProfileLayout),
    /// Open a web link from a message in the system's browser.
    OpenLink(String),
    /// Pick my avatar colour, by id from [`PROFILE_COLORS`]; empty for
    /// the automatic one.
    ProfileColor(String),
    /// Pick a theme by its id (see [`theme::THEMES`]).
    Theme(String),
    /// Pick an accent by its id (see [`theme::ACCENTS`]).
    Accent(String),
    /// Use this colour (`#rrggbb`) as a custom accent.
    CustomAccent(String),
    /// Pick the reading font by its id (see [`theme::FONTS`]).
    Font(String),
    /// Save the theme, accent and reading font to a theme file.
    ExportTheme,
    /// Load a theme file and use what it says.
    ImportTheme,
    /// Put the typing cursor in the text box with this widget id, or take
    /// it out of every box (`None`): keyboard focus moved.
    FocusInput(Option<&'static str>),
    /// Scroll the focused control into view: the one marked by
    /// [`a11y::mark`], or the text box with this widget id.
    RevealFocus(Option<&'static str>),
    /// Use this microphone; empty is the system default.
    Input(String),
    /// Use these speakers or headphones; empty is the system default.
    Output(String),
    /// How the microphone decides to send.
    Transmit(Transmit),
    /// Wait for the next key press and bind it to push-to-talk.
    BindPtt,
    /// Noise suppression on or off.
    NoiseSuppression(bool),
    /// Echo cancellation on or off.
    EchoCancel(bool),
    /// "Low data" calls on or off.
    LowData(bool),
    /// Sending a shared screen's sound on or off.
    ScreenAudio(bool),
    /// Look for plugins installed on this computer.
    FindPlugins,
    /// Add the found plugin at this index to the end of the chain.
    AddPlugin(usize),
    /// Take the chain's plugin at this index out.
    RemovePlugin(usize),
    /// Turn the chain's plugin at this index on or off.
    PluginOn(usize, bool),
    /// Move the chain's plugin at this index one place earlier.
    PluginEarlier(usize),
    /// Set a parameter (by id) of the chain's plugin at this index.
    PluginParam(usize, String, f32),
    /// Automatic level on or off.
    AutoGain(bool),
    /// Use a voice preset by name.
    Preset(String),
    /// Start or stop hearing my own microphone.
    MicTest(bool),
    /// Master output volume, dB.
    OutputVolume(f32),
    /// Turn animations off (or back on).
    ReducedMotion(bool),
    /// Notify about messages while the window is in the background.
    NotifyMessages(bool),
    /// Open the door when the app starts.
    OpenOnStart(bool),
    /// The relay to be reachable through; empty for none.
    Relay(String),
    /// Let contacts with no relay of their own listen at yours.
    LendRelay(bool),
    /// Route calls through anonymity-mode hops, or stop.
    Anonymity(bool),
    /// The hops for anonymity mode, comma-separated `host:port[#key]`.
    AnonymityHops(String),
    /// The local API for scripts and bots on or off.
    LocalApi(bool),
    /// The port the door listens on.
    Port(u16),
    /// Re-run the reachability check.
    CheckNetwork,
    /// The first-run screen is done: this is my name.
    Welcome(String),
    /// Open the folder where received files go.
    OpenDownloads,
    /// Look for a newer version of Stoop now.
    UpdateCheck,
    /// Download and install the newer version (or, where the app can't
    /// install it itself, open its release page).
    UpdateInstall,
    /// Not now: hide the newer version's banner until a newer one still.
    UpdateLater,
    /// Open the newer version's release page, to read about it.
    UpdatePage,
    /// Look for updates by itself (true), or only when asked.
    UpdateAuto(bool),
    /// Play the call through the loudspeaker (true) or the earpiece.
    Speaker(bool),
    /// Turn my camera on (true) or off.
    Camera(bool),
    /// Switch to the other camera, front or back. Only a phone shows the
    /// button: a desktop's webcam has no other side.
    FlipCamera,
    /// Start (true) or stop sharing my screen.
    ShareScreen(bool),
    /// The push-to-talk pad was pressed (true) or let go.
    Talk(bool),
    /// Ask the system for the microphone.
    RequestMic,
    /// Move files only on Wi-Fi.
    WifiOnly(bool),
    /// Turn the data saver on or off.
    DataSaver(bool),
    /// The monthly mobile data limit for files, in MB; 0 for none.
    CellularLimit(u32),
    /// Keep the door open in the background.
    KeepListening(bool),
    /// Keep listening in the background on battery saver too.
    ListenOnSaver(bool),
    /// Show (true) or hide the recovery phrase.
    ShowPhrase(bool),
    /// Restore the identity this recovery phrase (or these Shamir shares,
    /// one per line) makes, replacing this one. Contacts and settings stay.
    Restore(String),
    /// Seal the whole account under this passphrase and save it as a file.
    BackupSave {
        /// What the file opens with.
        passphrase: String,
        /// Include message history.
        history: bool,
    },
    /// Choose a backup file, open it with this passphrase, and make it
    /// this device's account (the dialog already asked).
    BackupOpen {
        /// What the file opens with.
        passphrase: String,
    },
    /// Offer this account to another device on the same network: show a
    /// link and QR code.
    MoveStart {
        /// Include message history.
        history: bool,
    },
    /// Close the offer.
    MoveStop,
    /// Fetch the account this move link offers and make it this device's
    /// (the dialog already asked).
    MoveReceive(String),
    /// Take this device (by device key, hex) off the account; it signs out
    /// the next time it hears from this one (the dialog already asked).
    RevokeDevice(String),
    /// The mailboxes your devices keep in touch through, as the list now
    /// stands (one added or taken off). Only where [`Account::mailboxes`]
    /// is offered.
    DeviceMailboxes(Vec<String>),
    /// A channel is on screen: load its messages and mark it read.
    OpenChannel {
        /// The group chat or community's id.
        community: String,
        /// The channel.
        channel: u32,
    },
    /// Start a group chat or community.
    CreateSpace {
        /// What to call it.
        name: String,
        /// A group chat rather than a community.
        group: bool,
    },
    /// Accept the invitation at this index in [`Model::space_invites`].
    AcceptSpaceInvite(usize),
    /// Turn down the invitation at this index.
    DeclineSpaceInvite(usize),
    /// Post a message to a channel.
    Post {
        /// The group chat or community's id.
        community: String,
        /// The channel.
        channel: u32,
        /// What it says.
        text: String,
    },
    /// Change the words of a message I posted.
    EditPost {
        /// The group chat or community's id.
        community: String,
        /// Which message.
        id: u64,
        /// The new words.
        text: String,
    },
    /// Put a reaction on a channel's message, or take mine off.
    ReactPost {
        /// The group chat or community's id.
        community: String,
        /// The channel the message is in.
        channel: u32,
        /// Which message.
        id: u64,
        /// Who wrote it (their key there, hex).
        author: String,
        /// The emoji: a Unicode emoji, or one of the community's as `:name:`.
        emoji: String,
    },
    /// Take a message down: my own, or someone's I'm allowed to remove.
    DeletePost {
        /// The group chat or community's id.
        community: String,
        /// Which message.
        id: u64,
        /// Who wrote it (their key there, hex).
        author: String,
    },
    /// Invite one of my people into a group chat or community.
    InviteToSpace {
        /// The group chat or community's id.
        community: String,
        /// The contact's id.
        person: String,
    },
    /// Add a channel to a community.
    AddChannel {
        /// The community's id.
        community: String,
        /// Its name, without the hash.
        name: String,
        /// A voice channel rather than a text one.
        voice: bool,
    },
    /// Leave a group chat or community (the dialog already asked).
    LeaveSpace(String),
    /// Add a mailbox node: a node link or a `host:port`.
    AddNode(String),
    /// Add a mailbox node by a picture of its QR code (a file is picked).
    AddNodeFromQr,
    /// Take a mailbox node off your routes.
    RemoveNode(String),
    /// Look for mailbox nodes on this network.
    FindNodes,
    /// Add a community directory (`host:port`) to browse.
    AddDirectory(String),
    /// Stop browsing a directory.
    RemoveDirectory(String),
    /// Read every directory again.
    BrowseDirectories,
    /// Ask to join a community from its listing (its join link).
    AskToJoin(String),
    /// Choose a picture to add as one of this community's emoji (by id);
    /// its name comes from the file's.
    AddEmoji(String),
    /// Take one of a community's emoji away.
    RemoveEmoji {
        /// The community's id.
        community: String,
        /// The emoji's name.
        name: String,
    },
    /// Choose which messages notify me in a space or one of its channels.
    SpaceNotify {
        /// The group chat or community's id.
        community: String,
        /// One channel, or `None` for the whole space.
        channel: Option<u32>,
        /// The setting; `None` for a channel follows the space's.
        level: Option<Notify>,
    },
    /// Change how a community is set up: its channels' readers, its
    /// roles, who holds which.
    ManageSpace {
        /// The community's id.
        community: String,
        /// What to change.
        change: SpaceChange,
    },
}

/// One change to how a community is set up, signed into its manifest by
/// whoever makes it (and refused by everyone else if they aren't allowed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpaceChange {
    /// Make a channel private (only its readers and the owners get it) or
    /// open to every member again.
    ChannelPrivate {
        /// The channel.
        channel: u32,
        /// Private rather than open.
        private: bool,
    },
    /// Let a member read a private channel, or stop them.
    ChannelReader {
        /// The channel.
        channel: u32,
        /// The member's id (hex).
        member: String,
        /// Let them rather than stop them.
        on: bool,
    },
    /// Add a role, just above the one newcomers get.
    AddRole(String),
    /// Remove the role at this place; whoever held it gets the newcomers'.
    RemoveRole(usize),
    /// Allow or disallow one thing (a place in [`Role::perms`]) for the
    /// role at a place.
    RolePerm {
        /// The role's place.
        role: usize,
        /// The thing's place in its list.
        perm: usize,
        /// Allow rather than disallow.
        on: bool,
    },
    /// Give a member the role at a place.
    MemberRole {
        /// The member's id (hex).
        member: String,
        /// The role's place.
        role: usize,
    },
    /// Set slow mode: the fewest seconds between one member's posts, 0 off.
    SlowMode(u32),
    /// Give it a board on this mailbox node (`host[:port]`), so a message
    /// for everyone is posted once and it can grow to thousands; or, with
    /// `None`, take the board away.
    Board(Option<String>),
    /// Open it to people asking to join from its join link, or close it.
    Listed(bool),
    /// Ask a directory (`host:port`) to list it, with this description.
    SubmitListing {
        /// The directory's address.
        directory: String,
        /// What it says about itself.
        description: String,
    },
    /// Ask a directory to take it off.
    WithdrawListing(String),
    /// Start a space shared with another community, with this name; a
    /// link for the other community's admin comes back.
    StartShared(String),
    /// Bring this community into the shared space a link from another
    /// community's admin names.
    JoinShared(String),
    /// Ask into a shared space this community lists (its id, hex).
    AskIntoShared(String),
    /// Take a shared space off this community's list (its id, hex).
    UnlistShared(String),
    /// Make another link for another community's admin to join this
    /// shared space, which you own.
    NewAllyLink,
    /// Withdraw the invitation you sent to someone (their id, hex).
    WithdrawInvite(String),
    /// Take a report off the list.
    DismissReport(u64),
    /// Remove a member (their id, hex), and with `ban` keep them out.
    RemoveMember {
        /// The member's id (hex).
        member: String,
        /// Ban rather than only remove.
        ban: bool,
    },
    /// Join a voice channel: say you're there, and be connected to the
    /// call of whoever else is.
    JoinVoice(u32),
    /// Leave the voice channel you are in.
    LeaveVoice(u32),
    /// Report messages in a channel to the moderators.
    Report {
        /// The channel.
        channel: u32,
        /// The messages picked.
        messages: Vec<u64>,
        /// Why.
        reason: String,
    },
}

/// Everything that can happen in the interface.
#[derive(Clone, Debug)]
pub enum Msg {
    /// The window's size changed.
    Resized(Size),
    /// Show this call picture large (by its tile key), or hand the
    /// choice back if it already is.
    FocusVideo(String),
    /// An animation frame (or a tick): advances animations and toasts.
    Frame(Instant),
    /// Go to a screen.
    Nav(Route),
    /// Pick a tab of the settings section on view, by index into
    /// [`Section::tabs`].
    SettingsTab(usize),
    /// Back: the phone layout's back arrow, the mouse's back button, Escape.
    Back,
    /// The people search box changed.
    Search(String),
    /// Open a dialog, or close the one that is open.
    Dialog(Option<Dialog>),
    /// Show or hide the details panel beside a conversation.
    TogglePanel,
    /// The composer's text changed.
    Draft(String),
    /// Send what is in the composer.
    Send,
    /// Start editing one of my messages: (id, its text).
    EditStart(u64, String),
    /// The text being edited changed.
    EditDraft(String),
    /// Save the edit.
    EditSave,
    /// Drop the edit.
    EditCancel,
    /// The add-person dialog's name box changed.
    AddName(String),
    /// The add-person dialog's invite box changed.
    AddInvite(String),
    /// Add the person from the invite typed in.
    AddSubmit,
    /// Call whatever is typed in the add-person box as an address.
    AddCall,
    /// My name is being edited in Settings.
    NameEdit(String),
    /// Save my name.
    NameSave,
    /// My status is being edited in Settings.
    StatusEdit(String),
    /// Save my status.
    StatusSave,
    /// My About line is being edited in Settings.
    BioEdit(String),
    /// Save my About line.
    BioSave,
    /// My pronouns are being edited (the profile screen or Settings).
    PronounsEdit(String),
    /// Save my pronouns as edited in Settings.
    PronounsSave,
    /// Save everything changed on the profile edit screen (name, pronouns,
    /// status, About) and go back to the profile.
    ProfileSave,
    /// Answer this message (by id) with the next one sent, or stop
    /// answering it (`None`).
    Reply(Option<u64>),
    /// The relay box is being edited.
    RelayEdit(String),
    /// Save the relay.
    RelaySave,
    /// The anonymity hop box is being edited.
    HopsEdit(String),
    /// Save the anonymity hops.
    HopsSave,
    /// The port box is being edited (digits only are kept).
    PortEdit(String),
    /// Save the port.
    PortSave,
    /// The note about the open person is being edited.
    NoteEdit(String),
    /// Save the note.
    NoteSave,
    /// The recovery phrase box changed.
    RestoreEdit(String),
    /// The backup passphrase box changed.
    BackupPass(String),
    /// The "type it again" box changed.
    BackupPass2(String),
    /// Include message history in a backup or move.
    IncludeHistory(bool),
    /// Save a backup with the passphrase typed (if both boxes agree).
    BackupSave,
    /// The passphrase box for opening a backup changed.
    ImportPass(String),
    /// The move link box changed.
    ReceiveLink(String),
    /// The box for adding a mailbox your devices share changed.
    MailboxDraft(String),
    /// Add what is in the mailbox box to the list your devices share.
    MailboxAdd,
    /// The mobile data limit box changed (digits only are kept).
    LimitEdit(String),
    /// Save the mobile data limit box.
    LimitSave,
    /// The first-run name box changed.
    FirstName(String),
    /// Finish first run with the name typed.
    FirstDone,
    /// The new group chat or community's name box changed.
    SpaceName(String),
    /// Make it a group chat (true) or a community.
    SpaceGroup(bool),
    /// Start the group chat or community named in the box.
    SpaceCreate,
    /// The new channel's name box changed.
    ChannelName(String),
    /// Add the channel named in the box to this community (by id).
    ChannelCreate(String),
    /// Make the new channel a voice channel, or a text one.
    ChannelVoice(bool),
    /// Show or hide the files waiting to be sent.
    ToggleQueue,
    /// Put `:name:` (a custom emoji) at the end of the composer's text.
    InsertEmoji(String),
    /// The box for a mailbox node to add changed.
    NodeEdit(String),
    /// Add the node typed in the box.
    NodeAdd,
    /// The directory search box changed.
    DirectoryQuery(String),
    /// The box for a directory to add changed.
    DirectoryAddress(String),
    /// Add the directory typed in the box.
    DirectoryAdd,
    /// The directory box in a community's listing settings changed.
    ListingDirectory(String),
    /// The node box in a community's board settings changed.
    BoardNode(String),
    /// The description box in a community's listing settings changed.
    ListingDescription(String),
    /// Send the listing typed in the boxes (community id).
    ListingSubmit(String),
    /// The name box for a new shared space changed.
    SharedName(String),
    /// The box for another community's shared space link changed.
    SharedLink(String),
    /// Start the shared space named in the box (community id).
    SharedStart(String),
    /// Join the shared space whose link is in the box (community id).
    SharedJoin(String),
    /// Pick or unpick a message to report with the others.
    ReportPick(u64),
    /// The report's reason box changed.
    ReportReason(String),
    /// Send the report being written (community id, channel).
    ReportSend(String, u32),
    /// The new role's name box changed.
    RoleName(String),
    /// Add the role named in the box to this community (by id).
    RoleCreate(String),
    /// Close a toast by its id.
    DismissToast(u64),
    /// Where the pointer is, so a right-click menu opens under it.
    Cursor(Point),
    /// Open the right-click menu for a person, or close it.
    Menu(Option<String>),
    /// Copy text and say so.
    Copy(String, &'static str),
    /// The custom accent box changed.
    AccentEdit(String),
    /// Use the colour in the custom accent box.
    AccentSave,
    /// Tab (1) or Shift+Tab (-1): move keyboard focus to the next or
    /// previous control.
    Focus(i32),
    /// Enter or Space: press the control with keyboard focus.
    Activate,
    /// An arrow key: step the focused slider or choice up (1) or down (-1).
    Adjust(i32),
    /// Shift+F10 or the menu key: open the focused control's right-click menu.
    ContextMenu,
    /// A screen reader asked for this to be done to the node with this
    /// AccessKit id.
    Reader(u64, a11y::ReaderAction),
    /// An action with nothing for the interface itself to do.
    Do(Action),
}

/// The interface's own state: what is open and what is typed.
pub struct Ui {
    /// The screen on view.
    route: Route,
    /// The dialog that is open, if any.
    dialog: Option<Dialog>,
    /// Kept while the dialog animates out, so it can still be drawn.
    dialog_shown: Option<Dialog>,
    /// Rises the dialog in and sinks it out.
    dialog_anim: Animation<bool>,
    /// Whether the details panel should be open.
    panel_open: bool,
    /// Slides the details panel open and shut.
    panel_anim: Animation<bool>,
    /// What is typed in the people search box.
    search: String,
    /// Unsent composer text, per conversation ([`Ui::draft_key`]), kept
    /// when switching conversations.
    drafts: HashMap<String, String>,
    /// The message being edited: (id, text so far).
    editing: Option<(u64, String)>,
    /// The add-person dialog's name box.
    add_name: String,
    /// The add-person dialog's invite (or address) box.
    add_invite: String,
    /// Why adding failed, shown under the invite box.
    add_error: String,
    /// My name while it is being edited; `None` shows the saved one.
    name_edit: Option<String>,
    /// My status while it is being edited.
    status_edit: Option<String>,
    /// My About line while it is being edited.
    bio_edit: Option<String>,
    /// My pronouns as being edited, until saved.
    pronouns_edit: Option<String>,
    /// The message the composer is answering, by id, while a reply is
    /// being written; shown over the composer with a way to drop it.
    replying: Option<u64>,
    /// The relay while it is being edited.
    relay_edit: Option<String>,
    /// The anonymity hop list while it is being edited.
    hops_edit: Option<String>,
    /// The port while it is being edited.
    port_edit: Option<String>,
    /// The open person's note while it is being edited: (id, text).
    note_edit: Option<(String, String)>,
    /// The first-run name box.
    first_name: String,
    /// The recovery phrase box, for restoring an identity.
    restore_edit: String,
    /// The passphrase a new backup is sealed with.
    backup_pass: String,
    /// The same passphrase typed again, to catch a typo before it locks
    /// the file for good.
    backup_pass2: String,
    /// Why the passphrase cannot be used yet, once Save was pressed.
    backup_error: String,
    /// Whether backups and moves bring message history.
    include_history: bool,
    /// The passphrase a backup file is opened with.
    import_pass: String,
    /// The move link pasted from another device.
    receive_link: String,
    /// A mailbox address being typed, to add to the list your devices share.
    mailbox_draft: String,
    /// The mobile data limit box while it is being edited.
    limit_edit: Option<String>,
    /// The new group chat or community's name box.
    space_name: String,
    /// The new one is a group chat rather than a community.
    space_group: bool,
    /// The new channel's name box.
    channel_name: String,
    /// The new channel is a voice channel.
    channel_voice: bool,
    /// The list of files waiting to be sent is open (it starts folded,
    /// so a long queue never pushes the messages off screen).
    queue_open: bool,
    /// The new role's name box.
    role_name: String,
    /// The box for a mailbox node to add.
    node_edit: String,
    /// The directory search box.
    directory_query: String,
    /// The box for a directory to add.
    directory_address: String,
    /// The directory a community is to be listed in.
    listing_directory: String,
    /// The mailbox node a community's board is to be on.
    board_node: String,
    /// What the listing is to say.
    listing_description: String,
    /// The name for a new shared space.
    shared_name: String,
    /// Another community's link for a shared space.
    shared_link: String,
    /// The messages picked to report.
    report_pick: Vec<u64>,
    /// Why they are being reported.
    report_reason: String,
    /// Toasts on screen, oldest first.
    toasts: Vec<Toast>,
    /// The id the next toast gets.
    next_toast: u64,
    /// Where the pointer last was, in window coordinates.
    cursor: Point,
    /// The person whose right-click menu is open, and where it opened.
    menu: Option<(String, Point)>,
    /// The window's size, which picks the wide or phone layout.
    size: Size,
    /// The time of the latest frame, which drives every animation.
    now: Instant,
    /// When the app started, for the looping pulses.
    epoch: Instant,
    /// Animations off: everything jumps to its end state.
    reduced_motion: bool,
    /// The strips the phone's system draws over, in logical pixels, as
    /// (top, bottom): the status bar and camera cutout, and the gesture
    /// bar (or the keyboard while it is up). The layout keeps every
    /// control out of them, while backgrounds still run underneath.
    insets: (f32, f32),
    /// The call picture chosen to be shown large, by its tile key. `None`
    /// lets the stage choose: their screen, then their camera, then mine.
    video_focus: Option<String>,
    /// The custom accent box while it is being edited.
    accent_edit: Option<String>,
    /// The key of the control with keyboard focus (see [`a11y`]); `None`
    /// until Tab is first pressed.
    focus: Option<String>,
    /// The message (by id) whose tools are in the keyboard's list, because
    /// focus is on it or on one of them.
    active_message: Option<u64>,
    /// The tab on view in the settings section on screen, by index into
    /// [`Section::tabs`]; back to the first whenever the screen changes.
    settings_tab: usize,
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui {
    /// A fresh interface on the front step.
    pub fn new() -> Self {
        let now = Instant::now();
        Ui {
            route: Route::Home,
            settings_tab: 0,
            dialog: None,
            dialog_shown: None,
            dialog_anim: Animation::new(false).duration(Duration::from_millis(180)).easing(Easing::EaseOutCubic),
            panel_open: true,
            panel_anim: Animation::new(true).duration(Duration::from_millis(220)).easing(Easing::EaseOutCubic),
            search: String::new(),
            drafts: HashMap::new(),
            editing: None,
            add_name: String::new(),
            add_invite: String::new(),
            add_error: String::new(),
            name_edit: None,
            status_edit: None,
            bio_edit: None,
            pronouns_edit: None,
            replying: None,
            relay_edit: None,
            hops_edit: None,
            port_edit: None,
            note_edit: None,
            first_name: String::new(),
            restore_edit: String::new(),
            backup_pass: String::new(),
            backup_pass2: String::new(),
            backup_error: String::new(),
            include_history: true,
            import_pass: String::new(),
            receive_link: String::new(),
            mailbox_draft: String::new(),
            limit_edit: None,
            space_name: String::new(),
            space_group: false,
            channel_name: String::new(),
            channel_voice: false,
            queue_open: false,
            role_name: String::new(),
            report_pick: Vec::new(),
            directory_query: String::new(),
            node_edit: String::new(),
            directory_address: String::new(),
            listing_directory: String::new(),
            board_node: String::new(),
            shared_name: String::new(),
            shared_link: String::new(),
            listing_description: String::new(),
            report_reason: String::new(),
            toasts: Vec::new(),
            next_toast: 1,
            cursor: Point::ORIGIN,
            menu: None,
            size: Size::new(1280.0, 820.0),
            now,
            epoch: now,
            reduced_motion: false,
            video_focus: None,
            insets: (0.0, 0.0),
            accent_edit: None,
            focus: None,
            active_message: None,
        }
    }

    /// The screen on view.
    pub fn route(&self) -> &Route {
        &self.route
    }

    /// The contact whose conversation is on screen.
    pub fn open_person(&self) -> Option<&str> {
        match &self.route {
            Route::Person(id) => Some(id),
            _ => None,
        }
    }

    /// The group chat or community channel on screen, as (id, channel).
    pub fn open_channel(&self) -> Option<(&str, u32)> {
        match &self.route {
            Route::Community(id, channel) => Some((id, *channel)),
            _ => None,
        }
    }

    /// Where the composer's text is kept for the screen on view: a
    /// contact's id, or the channel's own key. `None` off a conversation.
    fn draft_key(&self) -> Option<String> {
        match &self.route {
            Route::Person(id) => Some(id.clone()),
            // A hash can't start a contact id (hex), so the two never meet.
            Route::Community(id, channel) => Some(format!("#{id}/{channel}")),
            _ => None,
        }
    }

    /// Closes the new group chat dialog after the app made one, and opens it.
    pub fn space_created(&mut self, id: String) -> Vec<Action> {
        self.space_name.clear();
        self.set_dialog(None);
        self.go(Route::Community(id, 0))
    }

    /// The system's insets for this frame, in logical pixels: what the
    /// status bar and the gesture bar (or the keyboard while it is up)
    /// cover, as (top, bottom). Desktops leave them at zero.
    pub fn set_insets(&mut self, top: f32, bottom: f32) {
        self.insets = (top.max(0.0), bottom.max(0.0));
    }

    /// Whether the phone layout is in use.
    pub fn narrow(&self) -> bool {
        self.size.width < theme::NARROW
    }

    /// Whether something is moving, so the app should send [`Msg::Frame`]
    /// on every frame rather than on its slower tick.
    pub fn animating(&self) -> bool {
        self.dialog_anim.is_animating(self.now) || self.panel_anim.is_animating(self.now)
    }

    /// Whether the app should keep sending [`Msg::Frame`]: something is
    /// animating, or a toast is up and will need taking down on time.
    pub fn needs_frames(&self) -> bool {
        self.animating() || !self.toasts.is_empty()
    }

    /// Turns animations off (or back on) without waiting for a model.
    pub fn set_reduced_motion(&mut self, on: bool) {
        self.reduced_motion = on;
    }

    /// Shows a toast. Apps use it for the outcome of an action ("Added Robin").
    pub fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        let id = self.next_toast;
        self.next_toast += 1;
        self.toasts.push(Toast { id, text: text.into(), kind, at: self.now.max(Instant::now()) });
        // Never a wall of them.
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
    }

    /// Shows the add-person dialog's error ("That invite is not valid").
    pub fn add_failed(&mut self, error: impl Into<String>) {
        self.add_error = error.into();
    }

    /// Closes the add-person dialog after the app added someone, and opens
    /// their conversation.
    pub fn added(&mut self, id: String) -> Vec<Action> {
        self.add_name.clear();
        self.add_invite.clear();
        self.add_error.clear();
        self.set_dialog(None);
        self.go(Route::Person(id))
    }

    /// Opens a screen from outside (a notification, a tray item).
    pub fn show(&mut self, route: Route) -> Vec<Action> {
        self.go(route)
    }

    /// Opens or closes a dialog, starting its animation.
    fn set_dialog(&mut self, dialog: Option<Dialog>) {
        let at = self.now.max(Instant::now());
        self.now = at;
        if dialog.is_some() {
            self.dialog_shown = dialog.clone();
        }
        self.dialog_anim.go_mut(dialog.is_some(), at);
        self.dialog = dialog;
    }

    /// Moves to a screen, dropping per-screen state (an edit, a menu).
    fn go(&mut self, route: Route) -> Vec<Action> {
        self.editing = None;
        self.menu = None;
        self.replying = None;
        // A new screen (or another section) opens on its first tab.
        if self.route != route {
            self.settings_tab = 0;
        }
        let open = match &route {
            Route::Person(id) => vec![Action::Open(id.clone())],
            Route::Community(id, channel) => vec![Action::OpenChannel { community: id.clone(), channel: *channel }],
            _ => Vec::new(),
        };
        self.route = route;
        open
    }

    /// Handles one message; returns what the app must do.
    pub fn update(&mut self, msg: Msg, model: &Model) -> Vec<Action> {
        self.reduced_motion = model.settings.reduced_motion;
        match msg {
            Msg::Resized(size) => {
                self.size = size;
                Vec::new()
            }
            Msg::Frame(now) => {
                self.now = now;
                self.toasts.retain(|t| now.saturating_duration_since(t.at) < t.life());
                if self.dialog.is_none() && !self.dialog_anim.is_animating(now) {
                    self.dialog_shown = None;
                }
                Vec::new()
            }
            Msg::Nav(route) => self.go(route),
            Msg::SettingsTab(i) => {
                self.settings_tab = i;
                Vec::new()
            }
            Msg::Back => {
                if self.menu.is_some() {
                    self.menu = None;
                } else if self.dialog.is_some() {
                    self.set_dialog(None);
                } else if self.editing.is_some() {
                    self.editing = None;
                } else if self.replying.is_some() {
                    self.replying = None;
                } else if matches!(self.route, Route::Community(..)) {
                    // Out of a channel is back to the list it was opened from.
                    self.route = Route::Spaces;
                } else if self.route == Route::ProfileEdit {
                    // Out of editing is back to the profile, unsaved.
                    self.name_edit = None;
                    self.status_edit = None;
                    self.bio_edit = None;
                    self.pronouns_edit = None;
                    self.route = Route::Profile(None);
                } else if matches!(self.route, Route::Settings(_)) && self.narrow() {
                    // A phone opens a section from the settings list, so
                    // back is that list.
                    return self.go(Route::SettingsMenu);
                } else if let Route::Profile(Some(id)) = &self.route {
                    // A contact's profile is opened from their conversation.
                    let id = id.clone();
                    return self.go(Route::Person(id));
                } else if self.route != Route::Home {
                    self.route = Route::Home;
                }
                Vec::new()
            }
            Msg::Search(s) => {
                self.search = s;
                Vec::new()
            }
            Msg::Dialog(d) => {
                if d == Some(Dialog::AddPerson) {
                    self.add_error.clear();
                }
                if let Some(Dialog::Report(_, _, id)) = &d {
                    self.report_pick = vec![*id];
                    self.report_reason.clear();
                }
                // Opening the directories reads them again.
                let browse = d == Some(Dialog::Directory);
                self.set_dialog(d);
                if browse {
                    return vec![Action::BrowseDirectories];
                }
                Vec::new()
            }
            Msg::TogglePanel => {
                self.panel_open = !self.panel_open;
                let at = self.now.max(Instant::now());
                self.now = at;
                self.panel_anim.go_mut(self.panel_open, at);
                Vec::new()
            }
            Msg::Draft(s) => {
                if let Some(key) = self.draft_key() {
                    self.drafts.insert(key, s);
                }
                Vec::new()
            }
            Msg::Send => {
                let Some(key) = self.draft_key() else { return Vec::new() };
                let text = self.drafts.remove(&key).unwrap_or_default();
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                let reply_to = self.replying.take();
                match &self.route {
                    Route::Community(id, channel) => vec![Action::Post { community: id.clone(), channel: *channel, text }],
                    _ => vec![Action::Send { to: key, text, reply_to }],
                }
            }
            Msg::EditStart(id, text) => {
                self.editing = Some((id, text));
                // Straight into the box, whether Edit was clicked or pressed.
                self.focus = Some(format!("{:?}|Edit message|0", a11y::Role::Input));
                vec![Action::FocusInput(Some(EDIT_BOX))]
            }
            Msg::EditDraft(s) => {
                if let Some((_, text)) = self.editing.as_mut() {
                    *text = s;
                }
                Vec::new()
            }
            Msg::EditSave => {
                let edit = match self.editing.take() {
                    Some((id, text)) if !text.trim().is_empty() => (id, text.trim().to_string()),
                    _ => return Vec::new(),
                };
                match &self.route {
                    Route::Person(to) => vec![Action::Edit { to: to.clone(), id: edit.0, text: edit.1 }],
                    Route::Community(community, _) => vec![Action::EditPost { community: community.clone(), id: edit.0, text: edit.1 }],
                    _ => Vec::new(),
                }
            }
            Msg::EditCancel => {
                self.editing = None;
                Vec::new()
            }
            Msg::AddName(s) => {
                self.add_name = s;
                Vec::new()
            }
            Msg::AddInvite(s) => {
                self.add_invite = s;
                self.add_error.clear();
                Vec::new()
            }
            Msg::AddSubmit => {
                let invite = self.add_invite.trim().to_string();
                if invite.is_empty() {
                    self.add_error = "Paste their invite link first.".into();
                    return Vec::new();
                }
                vec![Action::AddPerson { name: self.add_name.trim().to_string(), invite }]
            }
            Msg::AddCall => {
                let address = self.add_invite.trim().to_string();
                if address.is_empty() {
                    self.add_error = "Type an address like 192.168.1.20:7000 first.".into();
                    return Vec::new();
                }
                self.set_dialog(None);
                vec![Action::CallAddress(address)]
            }
            Msg::NameEdit(s) => {
                self.name_edit = Some(s);
                Vec::new()
            }
            Msg::NameSave => match self.name_edit.take() {
                Some(n) if !n.trim().is_empty() && n.trim() != model.me.name => vec![Action::SetName(n.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::StatusEdit(s) => {
                self.status_edit = Some(s);
                Vec::new()
            }
            Msg::StatusSave => match self.status_edit.take() {
                Some(s) if s.trim() != model.me.status => vec![Action::SetStatus(s.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::PronounsEdit(s) => {
                self.pronouns_edit = Some(s);
                Vec::new()
            }
            Msg::PronounsSave => match self.pronouns_edit.take() {
                Some(s) if s.trim() != model.me.pronouns => vec![Action::SetPronouns(s.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::ProfileSave => {
                // Each field that changed becomes its own action, so the
                // app's rules for each (trimming, length) still apply.
                let mut out = Vec::new();
                if let Some(n) = self.name_edit.take().map(|n| n.trim().to_string()) {
                    if !n.is_empty() && n != model.me.name {
                        out.push(Action::SetName(n));
                    }
                }
                if let Some(s) = self.pronouns_edit.take().map(|s| s.trim().to_string()) {
                    if s != model.me.pronouns {
                        out.push(Action::SetPronouns(s));
                    }
                }
                if let Some(s) = self.status_edit.take().map(|s| s.trim().to_string()) {
                    if s != model.me.status {
                        out.push(Action::SetStatus(s));
                    }
                }
                if let Some(s) = self.bio_edit.take().map(|s| s.trim().to_string()) {
                    if s != model.me.bio {
                        out.push(Action::SetBio(s));
                    }
                }
                self.route = Route::Profile(None);
                out
            }
            Msg::Reply(id) => {
                self.replying = id;
                if id.is_some() {
                    // Straight into the composer, as a tap on Reply means
                    // "I am about to type".
                    return vec![Action::FocusInput(Some(COMPOSER))];
                }
                Vec::new()
            }
            Msg::BioEdit(s) => {
                self.bio_edit = Some(s);
                Vec::new()
            }
            Msg::BioSave => match self.bio_edit.take() {
                Some(s) if s.trim() != model.me.bio => vec![Action::SetBio(s.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::RelayEdit(s) => {
                self.relay_edit = Some(s);
                Vec::new()
            }
            Msg::HopsEdit(s) => {
                self.hops_edit = Some(s);
                Vec::new()
            }
            Msg::HopsSave => match self.hops_edit.take() {
                Some(s) if s.trim() != model.network.hops => vec![Action::AnonymityHops(s.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::RelaySave => match self.relay_edit.take() {
                Some(s) if s.trim() != model.network.relay => vec![Action::Relay(s.trim().to_string())],
                _ => Vec::new(),
            },
            Msg::PortEdit(s) => {
                self.port_edit = Some(s.chars().filter(char::is_ascii_digit).take(5).collect());
                Vec::new()
            }
            Msg::PortSave => match self.port_edit.take().and_then(|s| s.parse::<u16>().ok()) {
                Some(p) if p != 0 && p != model.network.port => vec![Action::Port(p)],
                _ => Vec::new(),
            },
            Msg::NoteEdit(s) => {
                if let Some(id) = self.open_person() {
                    self.note_edit = Some((id.to_string(), s));
                }
                Vec::new()
            }
            Msg::NoteSave => match self.note_edit.take() {
                Some((id, note)) => vec![Action::Note(id, note.trim().to_string())],
                None => Vec::new(),
            },
            Msg::RestoreEdit(s) => {
                self.restore_edit = s;
                Vec::new()
            }
            Msg::BackupPass(s) => {
                self.backup_pass = s;
                self.backup_error.clear();
                Vec::new()
            }
            Msg::BackupPass2(s) => {
                self.backup_pass2 = s;
                self.backup_error.clear();
                Vec::new()
            }
            Msg::IncludeHistory(on) => {
                self.include_history = on;
                Vec::new()
            }
            Msg::BackupSave => {
                // Checked here, not only in the engine, so a typo is caught
                // while the person is still looking at the boxes.
                let min = MIN_PASSPHRASE_CHARS;
                if self.backup_pass.chars().count() < min {
                    self.backup_error = format!("Use at least {min} characters. A short sentence is easy to remember and hard to guess.");
                    return Vec::new();
                }
                if self.backup_pass != self.backup_pass2 {
                    self.backup_error = "The two passphrases don't match.".into();
                    return Vec::new();
                }
                let passphrase = std::mem::take(&mut self.backup_pass);
                self.backup_pass2.clear();
                vec![Action::BackupSave { passphrase, history: self.include_history }]
            }
            Msg::ImportPass(s) => {
                self.import_pass = s;
                Vec::new()
            }
            Msg::ReceiveLink(s) => {
                self.receive_link = s;
                Vec::new()
            }
            Msg::MailboxDraft(s) => {
                self.mailbox_draft = s;
                Vec::new()
            }
            Msg::MailboxAdd => {
                let node = self.mailbox_draft.trim().to_string();
                let Some(list) = model.account.mailboxes.as_ref().filter(|l| !node.is_empty() && !l.contains(&node)) else { return Vec::new() };
                self.mailbox_draft.clear();
                vec![Action::DeviceMailboxes(list.iter().cloned().chain([node]).collect())]
            }
            Msg::LimitEdit(s) => {
                self.limit_edit = Some(s.chars().filter(char::is_ascii_digit).take(6).collect());
                Vec::new()
            }
            Msg::FocusVideo(key) => {
                // Choosing the picture already large hands the choice back.
                self.video_focus = if self.video_focus.as_ref() == Some(&key) { None } else { Some(key) };
                Vec::new()
            }
            Msg::LimitSave => match self.limit_edit.take() {
                // Empty means no limit.
                Some(s) => vec![Action::CellularLimit(s.parse().unwrap_or(0))],
                None => Vec::new(),
            },
            Msg::FirstName(s) => {
                self.first_name = s;
                Vec::new()
            }
            Msg::FirstDone => {
                let name = self.first_name.trim().to_string();
                if name.is_empty() {
                    return Vec::new();
                }
                vec![Action::Welcome(name)]
            }
            Msg::Cursor(at) => {
                self.cursor = at;
                Vec::new()
            }
            Msg::Menu(who) => {
                self.menu = who.map(|id| (id, self.cursor));
                Vec::new()
            }
            Msg::SpaceName(s) => {
                self.space_name = s;
                Vec::new()
            }
            Msg::SpaceGroup(on) => {
                self.space_group = on;
                Vec::new()
            }
            Msg::SpaceCreate => {
                let name = self.space_name.trim().to_string();
                if name.is_empty() {
                    return Vec::new();
                }
                vec![Action::CreateSpace { name, group: self.space_group }]
            }
            Msg::ChannelVoice(on) => {
                self.channel_voice = on;
                Vec::new()
            }
            Msg::ChannelName(s) => {
                // Channel names read like words joined by dashes, as people
                // expect from other apps: no spaces, no hash, short.
                self.channel_name = s.to_lowercase().replace(' ', "-").chars().filter(|c| *c != '#').take(32).collect();
                Vec::new()
            }
            Msg::ChannelCreate(community) => {
                let name = self.channel_name.trim_matches('-').to_string();
                if name.is_empty() {
                    return Vec::new();
                }
                self.channel_name.clear();
                self.set_dialog(None);
                let voice = std::mem::take(&mut self.channel_voice);
                vec![Action::AddChannel { community, name, voice }]
            }
            Msg::InsertEmoji(name) => {
                if let Some(key) = self.draft_key() {
                    let draft = self.drafts.entry(key).or_default();
                    if !draft.is_empty() && !draft.ends_with(' ') {
                        draft.push(' ');
                    }
                    draft.push_str(&format!(":{name}: "));
                }
                self.set_dialog(None);
                Vec::new()
            }
            Msg::ToggleQueue => {
                self.queue_open = !self.queue_open;
                Vec::new()
            }
            Msg::NodeEdit(s) => {
                self.node_edit = s;
                Vec::new()
            }
            Msg::NodeAdd => {
                let text = std::mem::take(&mut self.node_edit).trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Action::AddNode(text)]
            }
            Msg::DirectoryQuery(s) => {
                self.directory_query = s;
                Vec::new()
            }
            Msg::DirectoryAddress(s) => {
                self.directory_address = s.trim().to_string();
                Vec::new()
            }
            Msg::DirectoryAdd => {
                let address = std::mem::take(&mut self.directory_address);
                if address.is_empty() {
                    return Vec::new();
                }
                vec![Action::AddDirectory(address), Action::BrowseDirectories]
            }
            Msg::ListingDirectory(s) => {
                self.listing_directory = s.trim().to_string();
                Vec::new()
            }
            Msg::BoardNode(s) => {
                self.board_node = s.trim().to_string();
                Vec::new()
            }
            Msg::SharedName(s) => {
                self.shared_name = s.chars().take(80).collect();
                Vec::new()
            }
            Msg::SharedLink(s) => {
                self.shared_link = s.trim().to_string();
                Vec::new()
            }
            Msg::SharedStart(community) => {
                let name = std::mem::take(&mut self.shared_name).trim().to_string();
                if name.is_empty() {
                    return Vec::new();
                }
                vec![Action::ManageSpace { community, change: SpaceChange::StartShared(name) }]
            }
            Msg::SharedJoin(community) => {
                let link = std::mem::take(&mut self.shared_link);
                if link.is_empty() {
                    return Vec::new();
                }
                vec![Action::ManageSpace { community, change: SpaceChange::JoinShared(link) }]
            }
            Msg::ListingDescription(s) => {
                self.listing_description = s.chars().take(240).collect();
                Vec::new()
            }
            Msg::ListingSubmit(community) => {
                if self.listing_directory.is_empty() {
                    return Vec::new();
                }
                let change = SpaceChange::SubmitListing { directory: self.listing_directory.clone(), description: self.listing_description.trim().to_string() };
                vec![Action::ManageSpace { community, change }]
            }
            Msg::ReportPick(id) => {
                if let Some(at) = self.report_pick.iter().position(|x| *x == id) {
                    self.report_pick.remove(at);
                } else if self.report_pick.len() < 10 {
                    self.report_pick.push(id);
                }
                Vec::new()
            }
            Msg::ReportReason(s) => {
                self.report_reason = s.chars().take(500).collect();
                Vec::new()
            }
            Msg::ReportSend(community, channel) => {
                if self.report_pick.is_empty() {
                    return Vec::new();
                }
                let messages = std::mem::take(&mut self.report_pick);
                let reason = std::mem::take(&mut self.report_reason).trim().to_string();
                self.set_dialog(None);
                self.toast("Reported to the moderators", ToastKind::Ok);
                vec![Action::ManageSpace { community, change: SpaceChange::Report { channel, messages, reason } }]
            }
            Msg::RoleName(s) => {
                self.role_name = s.chars().take(40).collect();
                Vec::new()
            }
            Msg::RoleCreate(community) => {
                let name = self.role_name.trim().to_string();
                if name.is_empty() {
                    return Vec::new();
                }
                self.role_name.clear();
                vec![Action::ManageSpace { community, change: SpaceChange::AddRole(name) }]
            }
            Msg::DismissToast(id) => {
                self.toasts.retain(|t| t.id != id);
                Vec::new()
            }
            Msg::Copy(text, what) => {
                self.toast(format!("{what} copied"), ToastKind::Ok);
                vec![Action::Copy(text)]
            }
            Msg::AccentEdit(s) => {
                self.accent_edit = Some(s);
                vec![]
            }
            Msg::AccentSave => match self.accent_edit.take() {
                Some(s) if theme::parse_hex(&s).is_some() => vec![Action::CustomAccent(theme::to_hex(theme::hex(&s)))],
                Some(_) => {
                    self.toast("A colour looks like #8b5cf6", ToastKind::Warn);
                    vec![]
                }
                None => vec![],
            },
            Msg::Focus(step) => {
                let nodes = a11y::nodes();
                let next = a11y::next_stop(&nodes, self.focus.as_deref(), step);
                self.move_focus(next, &nodes)
            }
            Msg::Activate => match self.focused().and_then(|n| n.activate) {
                Some(msg) => self.update(msg, model),
                None => vec![],
            },
            Msg::Adjust(step) => match self.focused().and_then(|n| if step > 0 { n.increase } else { n.decrease }) {
                Some(msg) => self.update(msg, model),
                None => vec![],
            },
            Msg::ContextMenu => match self.focused().and_then(|n| n.menu) {
                Some(msg) => self.update(msg, model),
                None => vec![],
            },
            Msg::Reader(id, act) => {
                let Some(node) = a11y::nodes().into_iter().find(|n| n.id().0 == id) else { return vec![] };
                let mut actions = Vec::new();
                if node.role.focusable() {
                    actions.extend(self.move_focus(Some(node.key.clone()), &a11y::nodes()));
                }
                let msg = match act {
                    a11y::ReaderAction::Focus => None,
                    a11y::ReaderAction::Click => node.activate,
                    a11y::ReaderAction::Increase => node.increase,
                    a11y::ReaderAction::Decrease => node.decrease,
                    a11y::ReaderAction::Menu => node.menu,
                };
                if let Some(msg) = msg {
                    actions.extend(self.update(msg, model));
                }
                actions
            }
            Msg::Do(action) => {
                match &action {
                    // A reaction picked in the picker closes it; one tapped
                    // on a message had nothing open to close.
                    Action::React { .. } | Action::ReactPost { .. } if matches!(self.dialog, Some(Dialog::ReactPicker(_))) => self.set_dialog(None),
                    Action::Call(_) | Action::AddToCall(_) => self.menu = None,
                    Action::Remove(_) | Action::Block(_) => {
                        self.set_dialog(None);
                        self.route = Route::Home;
                    }
                    Action::RefreshInvite => self.toast("New invite link made", ToastKind::Ok),
                    Action::Restore(_) => {
                        self.set_dialog(None);
                        self.restore_edit.clear();
                    }
                    Action::BackupOpen { .. } => {
                        self.set_dialog(None);
                        self.import_pass.clear();
                    }
                    Action::MoveReceive(_) => {
                        self.set_dialog(None);
                        self.receive_link.clear();
                    }
                    Action::LeaveSpace(_) => {
                        self.set_dialog(None);
                        self.route = Route::Spaces;
                    }
                    Action::InviteToSpace { .. } => self.set_dialog(None),
                    _ => {}
                }
                vec![action]
            }
        }
    }

    /// Draws the interface for `model`.
    pub fn view<'a>(&'a self, model: &'a Model) -> El<'a, Msg> {
        // The controls register themselves as the view is built; see `a11y`.
        a11y::begin(self.focus.as_deref());
        screens::view(self, model)
    }

    /// Puts keyboard focus on the node with key `key` (or nowhere): the
    /// typing cursor follows into or out of text boxes, the control is
    /// scrolled into view, and a message keeps its tools while focus is on
    /// it or them.
    fn move_focus(&mut self, key: Option<String>, nodes: &[a11y::Node]) -> Vec<Action> {
        let node = key.as_deref().and_then(|k| nodes.iter().find(|n| n.key == k));
        let input = node.and_then(|n| n.input);
        self.active_message = node.and_then(|n| n.group);
        self.focus = key;
        vec![Action::FocusInput(input), Action::RevealFocus(input)]
    }

    /// The control with keyboard focus, as the last frame drew it.
    fn focused(&self) -> Option<a11y::Node> {
        let key = self.focus.as_deref()?;
        a11y::nodes().into_iter().find(|n| n.key == key)
    }

    /// The key of the control with keyboard focus, if any.
    pub fn focus_key(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// How far open the dialog is, 0..1.
    fn dialog_rise(&self) -> f32 {
        if self.reduced_motion {
            return if self.dialog.is_some() { 1.0 } else { 0.0 };
        }
        self.dialog_anim.interpolate(0.0, 1.0, self.now)
    }

    /// How far open the details panel is, 0..1.
    fn panel_open_amount(&self) -> f32 {
        if self.reduced_motion {
            return if self.panel_open { 1.0 } else { 0.0 };
        }
        self.panel_anim.interpolate(0.0, 1.0, self.now)
    }

    /// A slow 0..1..0 pulse for "calling..." and similar waiting states.
    fn pulse(&self) -> f32 {
        if self.reduced_motion {
            return 0.5;
        }
        let t = self.now.saturating_duration_since(self.epoch).as_secs_f32();
        (1.0 - (t * std::f32::consts::TAU / 1.6).cos()) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_a_person_asks_for_their_conversation() {
        let mut ui = Ui::new();
        let actions = ui.update(Msg::Nav(Route::Person("ab".into())), &Model::default());
        assert_eq!(actions, vec![Action::Open("ab".into())]);
        assert_eq!(ui.open_person(), Some("ab"));
    }

    #[test]
    fn send_takes_the_draft_and_skips_blank_ones() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Nav(Route::Person("ab".into())), &m);
        ui.update(Msg::Draft("   ".into()), &m);
        assert!(ui.update(Msg::Send, &m).is_empty());
        ui.update(Msg::Draft(" hi ".into()), &m);
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "ab".into(), text: "hi".into(), reply_to: None }]);
        assert!(ui.update(Msg::Send, &m).is_empty(), "the draft is cleared once sent");
    }

    #[test]
    fn a_reply_goes_with_the_next_message_only() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Nav(Route::Person("ab".into())), &m);
        ui.update(Msg::Reply(Some(4)), &m);
        ui.update(Msg::Draft("same".into()), &m);
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "ab".into(), text: "same".into(), reply_to: Some(4) }]);
        ui.update(Msg::Draft("and another".into()), &m);
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "ab".into(), text: "and another".into(), reply_to: None }]);
    }

    #[test]
    fn the_profile_editor_saves_only_what_changed() {
        let mut ui = Ui::new();
        let mut m = Model::default();
        m.me.name = "Azule".into();
        ui.update(Msg::Nav(Route::Profile(None)), &m);
        ui.update(Msg::Nav(Route::ProfileEdit), &m);
        ui.update(Msg::NameEdit("Azule".into()), &m);
        ui.update(Msg::PronounsEdit(" they/them ".into()), &m);
        assert_eq!(ui.update(Msg::ProfileSave, &m), vec![Action::SetPronouns("they/them".into())]);
        assert_eq!(ui.route(), &Route::Profile(None));
        // Back out of an edit drops it.
        ui.update(Msg::Nav(Route::ProfileEdit), &m);
        ui.update(Msg::BioEdit("never saved".into()), &m);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.route(), &Route::Profile(None));
        assert!(ui.update(Msg::ProfileSave, &m).is_empty());
    }

    #[test]
    fn back_from_a_contacts_profile_is_their_conversation() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Nav(Route::Profile(Some("ab".into()))), &m);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.open_person(), Some("ab"));
    }

    #[test]
    fn drafts_are_kept_per_person() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Nav(Route::Person("a".into())), &m);
        ui.update(Msg::Draft("for a".into()), &m);
        ui.update(Msg::Nav(Route::Person("b".into())), &m);
        assert!(ui.update(Msg::Send, &m).is_empty());
        ui.update(Msg::Nav(Route::Person("a".into())), &m);
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "a".into(), text: "for a".into(), reply_to: None }]);
    }

    #[test]
    fn back_closes_a_dialog_before_leaving_the_screen() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Nav(Route::Person("a".into())), &m);
        ui.update(Msg::Dialog(Some(Dialog::Invite)), &m);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.open_person(), Some("a"));
        ui.update(Msg::Back, &m);
        assert_eq!(ui.route(), &Route::Home);
    }

    #[test]
    fn adding_needs_an_invite() {
        let mut ui = Ui::new();
        let m = Model::default();
        assert!(ui.update(Msg::AddSubmit, &m).is_empty());
        assert!(!ui.add_error.is_empty());
        ui.update(Msg::AddInvite("stoop://abc".into()), &m);
        assert_eq!(ui.update(Msg::AddSubmit, &m), vec![Action::AddPerson { name: String::new(), invite: "stoop://abc".into() }]);
    }

    #[test]
    fn the_port_box_takes_digits_only() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::PortEdit("7a0b00".into()), &m);
        assert_eq!(ui.update(Msg::PortSave, &m), vec![Action::Port(7000)]);
    }

    #[test]
    fn right_click_opens_a_menu_where_the_pointer_is() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::Cursor(Point::new(40.0, 300.0)), &m);
        ui.update(Msg::Menu(Some("ab".into())), &m);
        assert_eq!(ui.menu, Some(("ab".into(), Point::new(40.0, 300.0))));
        ui.update(Msg::Back, &m);
        assert!(ui.menu.is_none(), "Escape closes the menu first");
    }

    #[test]
    fn a_channel_has_its_own_draft_and_posts_there() {
        let mut ui = Ui::new();
        let m = Model::default();
        let actions = ui.update(Msg::Nav(Route::Community("c1".into(), 2)), &m);
        assert_eq!(actions, vec![Action::OpenChannel { community: "c1".into(), channel: 2 }]);
        ui.update(Msg::Draft("hello all".into()), &m);
        ui.update(Msg::Nav(Route::Person("ab".into())), &m);
        assert!(ui.update(Msg::Send, &m).is_empty(), "the channel's draft stays with the channel");
        ui.update(Msg::Nav(Route::Community("c1".into(), 2)), &m);
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Post { community: "c1".into(), channel: 2, text: "hello all".into() }]);
        ui.update(Msg::EditStart(9, "hello all".into()), &m);
        ui.update(Msg::EditDraft("hello everyone".into()), &m);
        assert_eq!(ui.update(Msg::EditSave, &m), vec![Action::EditPost { community: "c1".into(), id: 9, text: "hello everyone".into() }]);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.route(), &Route::Spaces, "back from a channel is the list");
    }

    #[test]
    fn channel_names_read_like_words_with_dashes() {
        let mut ui = Ui::new();
        let m = Model::default();
        ui.update(Msg::ChannelName("#Band Practice".into()), &m);
        assert_eq!(ui.update(Msg::ChannelCreate("c1".into()), &m), vec![Action::AddChannel { community: "c1".into(), name: "band-practice".into(), voice: false }]);
        assert!(ui.update(Msg::ChannelCreate("c1".into()), &m).is_empty(), "the box is cleared once used");
        ui.update(Msg::RoleName("  Helper ".into()), &m);
        let add = Action::ManageSpace { community: "c1".into(), change: SpaceChange::AddRole("Helper".into()) };
        assert_eq!(ui.update(Msg::RoleCreate("c1".into()), &m), vec![add]);
        assert!(ui.update(Msg::RoleCreate("c1".into()), &m).is_empty(), "the role box is cleared once used");
    }

    #[test]
    fn the_keyboard_alone_opens_a_dialog_and_stays_inside_it() {
        // Reduced motion, so the dialog is fully up the moment it opens.
        let mut m = Model::default();
        m.settings.reduced_motion = true;
        let mut ui = Ui::new();
        let _ = ui.view(&m);
        // Tab to the invite button in the people list, press it.
        let stops = a11y::nodes();
        let invite = a11y::tab_stops(&stops).iter().position(|n| n.label == "Your invite").expect("the invite button is reachable");
        for _ in 0..=invite {
            ui.update(Msg::Focus(1), &m);
        }
        assert!(ui.focus_key().is_some_and(|k| k.contains("Your invite")));
        ui.update(Msg::Activate, &m);
        assert_eq!(ui.dialog, Some(Dialog::Invite));
        // Now Tab only walks the dialog, and Escape closes it.
        let _ = ui.view(&m);
        let behind = ui.focus_key().map(str::to_string);
        ui.update(Msg::Focus(1), &m);
        let _ = ui.view(&m);
        let layer = a11y::nodes().iter().find(|n| Some(n.key.as_str()) == ui.focus_key()).map(|n| n.layer);
        assert_eq!(layer, Some(1), "focus moved into the dialog, away from {behind:?}");
        ui.update(Msg::Back, &m);
        assert_eq!(ui.dialog, None);
    }

    #[test]
    fn focusing_a_message_brings_its_tools_into_reach() {
        let mut m = Model::default();
        m.people.push(Person { id: "ab".into(), name: "Robin".into(), ..Default::default() });
        m.conversation = Some(Conversation {
            person: "ab".into(),
            lines: vec![Line { id: 5, mine: true, text: "hello".into(), ..Default::default() }],
            ..Default::default()
        });
        let mut ui = Ui::new();
        ui.update(Msg::Nav(Route::Person("ab".into())), &m);
        let _ = ui.view(&m);
        let has_edit = |ui: &Ui| a11y::tab_stops(&a11y::nodes()).iter().any(|n| n.label == "Edit" && n.group == Some(5)) && ui.focus_key().is_some();
        assert!(!a11y::nodes().iter().any(|n| n.label == "Edit"), "tools stay out until the message has focus");
        let at = a11y::tab_stops(&a11y::nodes()).iter().position(|n| n.role == a11y::Role::Message).expect("the message is a stop");
        for _ in 0..=at {
            ui.update(Msg::Focus(1), &m);
        }
        let actions = ui.update(Msg::Focus(0), &m);
        assert!(actions.contains(&Action::RevealFocus(None)), "a focus move scrolls it into view");
        let _ = ui.view(&m);
        assert!(has_edit(&ui), "its tools follow it");
        // Tab to Edit and press it: editing starts with the cursor in the box.
        while !ui.focus_key().is_some_and(|k| k.contains("|Edit|")) {
            ui.update(Msg::Focus(1), &m);
        }
        assert_eq!(ui.update(Msg::Activate, &m), vec![Action::FocusInput(Some(EDIT_BOX))]);
    }

    #[test]
    fn arrows_step_the_focused_switch_and_slider_messages() {
        let m = Model::default();
        let mut ui = Ui::new();
        ui.update(Msg::Nav(Route::Settings(Section::Appearance)), &m);
        let _ = ui.view(&m);
        let stops = a11y::nodes();
        let motion = a11y::tab_stops(&stops).iter().position(|n| n.label == "Reduce motion").expect("the switch is reachable");
        for _ in 0..=motion {
            ui.update(Msg::Focus(1), &m);
        }
        assert_eq!(ui.update(Msg::Activate, &m), vec![Action::ReducedMotion(true)]);
    }

    #[test]
    fn narrow_windows_get_the_phone_layout() {
        let mut ui = Ui::new();
        ui.update(Msg::Resized(Size::new(400.0, 800.0)), &Model::default());
        assert!(ui.narrow());
        ui.update(Msg::Resized(Size::new(1400.0, 800.0)), &Model::default());
        assert!(!ui.narrow());
    }

    #[test]
    fn every_settings_section_is_in_exactly_one_group() {
        let listed: Vec<Section> = Group::ALL.into_iter().flat_map(Group::sections).collect();
        assert_eq!(listed, Section::ALL, "the groups list every section once, in order");
    }

    #[test]
    fn settings_tabs_reset_on_a_new_section() {
        let (mut ui, m) = (Ui::new(), Model::default());
        ui.update(Msg::Nav(Route::Settings(Section::Account)), &m);
        ui.update(Msg::SettingsTab(2), &m);
        assert_eq!(ui.settings_tab, 2);
        ui.update(Msg::Nav(Route::Settings(Section::Account)), &m);
        assert_eq!(ui.settings_tab, 2, "the same section keeps its tab");
        ui.update(Msg::Nav(Route::Settings(Section::Voice)), &m);
        assert_eq!(ui.settings_tab, 0);
    }

    #[test]
    fn phone_back_from_a_settings_section_is_the_list() {
        let (mut ui, m) = (Ui::new(), Model::default());
        ui.update(Msg::Resized(Size::new(400.0, 800.0)), &m);
        ui.update(Msg::Nav(Route::SettingsMenu), &m);
        ui.update(Msg::Nav(Route::Settings(Section::Privacy)), &m);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.route(), &Route::SettingsMenu);
        ui.update(Msg::Back, &m);
        assert_eq!(ui.route(), &Route::Home);
    }
}
