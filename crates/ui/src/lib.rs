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

pub mod icons;
pub mod model;
pub mod theme;
pub mod widgets;

mod screens;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use iced_core::animation::{Animation, Easing};
use iced_core::{Point, Size};

pub use model::*;

/// An element of this interface: the default theme, and whichever renderer
/// the app's build of iced provides.
pub type El<'a, M> = iced_core::Element<'a, M, iced_core::Theme, iced_widget::Renderer>;

/// The composer's widget id, so an app can focus it when a conversation opens.
pub const COMPOSER: &str = "stoop-composer";
/// The people search box's widget id, for a find shortcut.
pub const SEARCH: &str = "stoop-search";

/// A screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// The front step: your door, invite, people.
    Home,
    /// A conversation.
    Person(String),
    /// A part of Settings.
    Settings(Section),
}

/// A part of Settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Your name and status.
    Profile,
    /// Theme, accent and motion.
    Appearance,
    /// Devices, how your voice is sent, and processing.
    Voice,
    /// Whether people can reach you: port, relay, the reachability check.
    Network,
    /// Version and where data is kept.
    About,
    /// Battery, mobile data and the recovery phrase; phones only.
    Phone,
}

impl Section {
    /// Every section, in the order the navigation lists them.
    pub const ALL: [Section; 6] = [Section::Profile, Section::Appearance, Section::Voice, Section::Network, Section::Phone, Section::About];

    /// The section's name as the navigation shows it.
    pub fn label(self) -> &'static str {
        match self {
            Section::Profile => "Profile",
            Section::Appearance => "Appearance",
            Section::Voice => "Voice & audio",
            Section::Network => "Network",
            Section::About => "About",
            Section::Phone => "Battery & data",
        }
    }

    /// The icon beside the section's name.
    pub fn icon(self) -> icons::Icon {
        match self {
            Section::Profile => icons::Icon::Users,
            Section::Appearance => icons::Icon::Palette,
            Section::Voice => icons::Icon::Sliders,
            Section::Network => icons::Icon::Globe,
            Section::About => icons::Icon::Info,
            Section::Phone => icons::Icon::Phone,
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
    /// Take the file the person on the call offered.
    AcceptFile,
    /// Turn the offered file down.
    DeclineFile,
    /// Stop the file on its way, in either direction.
    CancelTransfer,
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
    /// Pick a theme by its id (see [`theme::THEMES`]).
    Theme(String),
    /// Pick an accent by its id (see [`theme::ACCENTS`]).
    Accent(String),
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
    /// The port the door listens on.
    Port(u16),
    /// Re-run the reachability check.
    CheckNetwork,
    /// The first-run screen is done: this is my name.
    Welcome(String),
    /// Open the folder where received files go.
    OpenDownloads,
    /// Play the call through the loudspeaker (true) or the earpiece.
    Speaker(bool),
    /// Turn my camera on (true) or off.
    Camera(bool),
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
    /// Restore the identity this recovery phrase makes, replacing this one.
    Restore(String),
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
    /// The relay box is being edited.
    RelayEdit(String),
    /// Save the relay.
    RelaySave,
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
    /// The mobile data limit box changed (digits only are kept).
    LimitEdit(String),
    /// Save the mobile data limit box.
    LimitSave,
    /// The first-run name box changed.
    FirstName(String),
    /// Finish first run with the name typed.
    FirstDone,
    /// Close a toast by its id.
    DismissToast(u64),
    /// Where the pointer is, so a right-click menu opens under it.
    Cursor(Point),
    /// Open the right-click menu for a person, or close it.
    Menu(Option<String>),
    /// Copy text and say so.
    Copy(String, &'static str),
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
    /// Unsent composer text, per contact id, kept when switching conversations.
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
    /// The relay while it is being edited.
    relay_edit: Option<String>,
    /// The port while it is being edited.
    port_edit: Option<String>,
    /// The open person's note while it is being edited: (id, text).
    note_edit: Option<(String, String)>,
    /// The first-run name box.
    first_name: String,
    /// The recovery phrase box, for restoring an identity on a phone.
    restore_edit: String,
    /// The mobile data limit box while it is being edited.
    limit_edit: Option<String>,
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
    /// The call picture chosen to be shown large, by its tile key. `None`
    /// lets the stage choose: their screen, then their camera, then mine.
    video_focus: Option<String>,
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
            relay_edit: None,
            port_edit: None,
            note_edit: None,
            first_name: String::new(),
            restore_edit: String::new(),
            limit_edit: None,
            toasts: Vec::new(),
            next_toast: 1,
            cursor: Point::ORIGIN,
            menu: None,
            size: Size::new(1280.0, 820.0),
            now,
            epoch: now,
            reduced_motion: false,
            video_focus: None,
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
        let open = match &route {
            Route::Person(id) => vec![Action::Open(id.clone())],
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
            Msg::Back => {
                if self.menu.is_some() {
                    self.menu = None;
                } else if self.dialog.is_some() {
                    self.set_dialog(None);
                } else if self.editing.is_some() {
                    self.editing = None;
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
                self.set_dialog(d);
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
                if let Some(id) = self.open_person() {
                    self.drafts.insert(id.to_string(), s);
                }
                Vec::new()
            }
            Msg::Send => {
                let Some(id) = self.open_person().map(str::to_string) else { return Vec::new() };
                let text = self.drafts.remove(&id).unwrap_or_default();
                let text = text.trim().to_string();
                if text.is_empty() {
                    return Vec::new();
                }
                vec![Action::Send { to: id, text }]
            }
            Msg::EditStart(id, text) => {
                self.editing = Some((id, text));
                Vec::new()
            }
            Msg::EditDraft(s) => {
                if let Some((_, text)) = self.editing.as_mut() {
                    *text = s;
                }
                Vec::new()
            }
            Msg::EditSave => {
                let Some(to) = self.open_person().map(str::to_string) else { return Vec::new() };
                match self.editing.take() {
                    Some((id, text)) if !text.trim().is_empty() => vec![Action::Edit { to, id, text: text.trim().to_string() }],
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
            Msg::RelayEdit(s) => {
                self.relay_edit = Some(s);
                Vec::new()
            }
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
            Msg::DismissToast(id) => {
                self.toasts.retain(|t| t.id != id);
                Vec::new()
            }
            Msg::Copy(text, what) => {
                self.toast(format!("{what} copied"), ToastKind::Ok);
                vec![Action::Copy(text)]
            }
            Msg::Do(action) => {
                match &action {
                    Action::Call(_) => self.menu = None,
                    Action::Remove(_) | Action::Block(_) => {
                        self.set_dialog(None);
                        self.route = Route::Home;
                    }
                    Action::RefreshInvite => self.toast("New invite link made", ToastKind::Ok),
                    Action::Restore(_) => {
                        self.set_dialog(None);
                        self.restore_edit.clear();
                    }
                    _ => {}
                }
                vec![action]
            }
        }
    }

    /// Draws the interface for `model`.
    pub fn view<'a>(&'a self, model: &'a Model) -> El<'a, Msg> {
        screens::view(self, model)
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
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "ab".into(), text: "hi".into() }]);
        assert!(ui.update(Msg::Send, &m).is_empty(), "the draft is cleared once sent");
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
        assert_eq!(ui.update(Msg::Send, &m), vec![Action::Send { to: "a".into(), text: "for a".into() }]);
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
    fn narrow_windows_get_the_phone_layout() {
        let mut ui = Ui::new();
        ui.update(Msg::Resized(Size::new(400.0, 800.0)), &Model::default());
        assert!(ui.narrow());
        ui.update(Msg::Resized(Size::new(1400.0, 800.0)), &Model::default());
        assert!(!ui.narrow());
    }
}
