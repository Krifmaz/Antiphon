//! The phone app's state and rules: the call, chat, files, contacts,
//! voice, settings and identity.
//!
//! What is on screen is Stoop's shared interface (`antiphon-ui`), the same
//! screens as the desktop; [`stoop`] turns this state into the model it
//! draws and carries out what it asks for, mostly by sending a
//! [`Message`] handled here. [`crate::runner`] feeds it events, draws it,
//! and calls [`App::poll`] whenever it wakes so what the call delivered --
//! messages, file progress, the end of a call -- reaches the screen and
//! the disk.

mod stoop;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use antiphon_audio::{eq_preset, ChainConfig, GainConfig};
use antiphon_crypto::{BackupKey, IdentityKeyPair};
use iced_core::{Element, Length, Padding, Size, Theme};
use iced_widget::{container, qr_code};

use crate::call::{self, now_secs, CallEngine, Phase, Waker};
use crate::call_log::{self, CallRecord};
use crate::contacts::{hex, Contacts};
use crate::control::ControlMessage;
use crate::conversation::Conversation;
use crate::data_rules::{self, CellularMeter, DataSettings, Link};
use crate::files::{self, FileManager};
use crate::platform::{self, BackgroundWork, Network};
use crate::settings::Settings;

/// The renderer the shared interface is typed against: iced's wgpu one.
pub type Renderer = iced_widget::Renderer;

/// A stage of the microphone chain, for the switch that turns it on and
/// off on the Voice screen.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // the full voice chain screen; see the note on `Message`
pub enum Stage {
    /// Removes steady background noise, even under speech.
    NoiseSuppress,
    /// Mutes the microphone between words.
    NoiseGate,
    /// Cuts rumble below the voice.
    HighPass,
    /// Cuts hiss above the voice.
    LowPass,
    /// Softens sibilants.
    DeEsser,
    /// Evens out loud and quiet moments.
    Compressor,
    /// A hard ceiling against clipping.
    Limiter,
    /// Slowly turns a quiet microphone up and a loud one down.
    AutoGain,
    /// Raises or lowers the voice.
    PitchShift,
    /// Adds a room around the voice.
    Reverb,
}

/// A slider on the Voice screen, each setting one field of the chain.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // the full voice chain screen; see the note on `Message`
pub enum Param {
    /// How hard noise suppression works, from 0 to 1.
    SuppressStrength,
    /// The noise gate's threshold, in dB.
    GateThreshold,
    /// The high-pass filter's cutoff, in Hz.
    HighPassCutoff,
    /// The low-pass filter's cutoff, in Hz.
    LowPassCutoff,
    /// The compressor's threshold, in dB.
    CompressorThreshold,
    /// The compressor's ratio, as the `n` in `n:1`.
    CompressorRatio,
    /// The pitch shift, in semitones up (positive) or down.
    PitchSemitones,
    /// How much of the reverb is heard against the dry voice, from 0 to 1.
    ReverbMix,
    /// The reverb's room size, from 0 to 1.
    ReverbRoom,
}

/// Everything a tap or a keystroke can ask the interface to do.
///
/// The variants marked `allow(dead_code)` belong to phone screens the
/// shared interface does not have yet -- the file queue, recent calls,
/// the full voice chain, saving a caller, the message timer. Their rules
/// stay here, tested by use before, so those screens only need drawing. Handled
/// by [`App::update`] on the interface thread; the call, contacts and
/// settings ones change state and save it straight away.
#[derive(Debug, Clone)]
pub enum Message {
    /// The listening port box changed; only digits are kept.
    PortChanged(String),
    /// Start waiting for a call on the port in the box.
    Listen,
    /// Hang up, stop waiting, or cancel a dial.
    Stop,
    /// The push-to-talk pad was pressed.
    PttPressed,
    /// The push-to-talk pad was released.
    PttReleased,
    /// Switch open mic on or off.
    OpenMic(bool),
    /// Switch between loudspeaker and earpiece.
    Speaker(bool),
    /// Show or hide the recovery phrase.
    ShowPhrase(bool),
    /// Restore from the phrase in the box; the first press only arms it
    /// when it would replace a different identity.
    Restore,
    /// Show the system's microphone permission prompt.
    RequestMic,
    /// Start or stop the mic test.
    MicTest(bool),
    /// Load a named EQ preset into the chain.
    EqPreset(String),
    /// Turn a chain stage on or off.
    Toggle(Stage, bool),
    /// Move a chain slider to a new value, in that slider's units.
    #[allow(dead_code)] // see the note on `Message`
    Param(Param, f32),
    /// Put the whole chain back to the default preset.
    ResetChain,

    // Contacts.
    /// Call the contact with this key (hex).
    CallContact(String),
    /// The name box for saving the person on the call changed.
    #[allow(dead_code)] // see the note on `Message`
    SaveCallerName(String),
    /// Save the person on the call under the name in the box.
    #[allow(dead_code)] // see the note on `Message`
    SaveCaller,
    /// Mark the contact with this key (hex) verified, or not.
    MarkVerified(String, bool),

    // Chat.
    /// Send the message box, or save it over the message being edited.
    SendChat,
    /// Delete one of our messages.
    DeleteChat(u64),
    /// Toggle our reaction on a message.
    React(u64, &'static str),
    /// Set the open conversation's disappearing-message timer.
    #[allow(dead_code)] // see the note on `Message`
    Expiry(Expiry),

    // Files.
    /// Open the system file picker to send a file to the person on the
    /// call.
    PickFile,
    /// Accept the file being offered.
    AcceptOffer,
    /// Decline the file being offered.
    DeclineOffer,
    /// Stop receiving the file arriving.
    CancelIncoming,
    /// Pause (`true`) or resume the file being sent.
    #[allow(dead_code)] // see the note on `Message`
    PauseSend(bool),
    /// Stop sending the file being sent.
    CancelSend,
    /// Move a queued file one place earlier.
    #[allow(dead_code)] // see the note on `Message`
    QueueUp(u64),
    /// Move a queued file one place later.
    #[allow(dead_code)] // see the note on `Message`
    QueueDown(u64),
    /// Hold (`true`) or release a queued file.
    #[allow(dead_code)] // see the note on `Message`
    QueueHold(u64, bool),
    /// Take a file out of the queue.
    #[allow(dead_code)] // see the note on `Message`
    QueueRemove(u64),

    // Recent calls.
    /// Call back the call at this index in the history, newest first.
    #[allow(dead_code)] // see the note on `Message`
    CallBack(usize),
    /// Clear the call history; the first press only arms it.
    #[allow(dead_code)] // see the note on `Message`
    ClearCalls,

    // Settings.
    /// The display name box changed.
    DisplayName(String),
    /// Switch "Files only on Wi-Fi".
    WifiOnly(bool),
    /// The monthly mobile data limit box changed, in MB; only digits are
    /// kept, and empty means no limit.
    CellularLimit(String),
    /// Switch the data saver.
    DataSaver(bool),
    /// Switch listening for calls in the background.
    KeepListening(bool),
    /// Switch listening in the background even on battery saver.
    ListenOnSaver(bool),

    /// Something happened in the shared interface: a tap, typing, a frame.
    Ui(antiphon_ui::Msg),
}

/// The disappearing-message choices offered. A short list, as on the
/// desktop, rather than a free duration nobody can reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// Messages are kept.
    Off,
    /// Five minutes.
    FiveMinutes,
    /// One hour.
    Hour,
    /// One day.
    Day,
    /// One week.
    Week,
}

impl Expiry {
    /// Every choice, in the order the picker shows them.
    pub const ALL: [Expiry; 5] = [Expiry::Off, Expiry::FiveMinutes, Expiry::Hour, Expiry::Day, Expiry::Week];

    /// The timer in seconds, or `None` to keep messages.
    pub fn secs(self) -> Option<u64> {
        match self {
            Expiry::Off => None,
            Expiry::FiveMinutes => Some(300),
            Expiry::Hour => Some(3_600),
            Expiry::Day => Some(86_400),
            Expiry::Week => Some(7 * 86_400),
        }
    }

    /// The choice matching a stored timer. A timer that is not one of the
    /// choices (set on a desktop, say) shows as Off; it is not changed
    /// unless the user picks something.
    pub fn from_secs(secs: Option<u64>) -> Self {
        Self::ALL.into_iter().find(|e| e.secs() == secs).unwrap_or(Expiry::Off)
    }
}

impl std::fmt::Display for Expiry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Expiry::Off => "Keep messages",
            Expiry::FiveMinutes => "5 minutes",
            Expiry::Hour => "1 hour",
            Expiry::Day => "1 day",
            Expiry::Week => "1 week",
        })
    }
}

/// The whole interface: its state, the call engine, and the files on
/// disk it keeps in step.
///
/// Owned by the [`Runner`](crate::runner::Runner) and used only on the
/// event-loop thread. The call runs on the engine's own threads and
/// reaches this through [`App::poll`].
pub struct App {
    /// The app's private storage, where every file below lives.
    dir: PathBuf,
    /// The settings as last saved; every change is written straight away.
    settings: Settings,
    /// This phone's identity, or why it could not be loaded, which is shown
    /// in place of anything that needs it.
    identity: Result<IdentityKeyPair, String>,
    /// Runs calls and the mic test.
    engine: CallEngine,
    /// The listening port box as typed, which may not be a valid port yet.
    port_text: String,
    /// The EQ preset last chosen, for the picker; `None` once the chain is
    /// reset.
    eq_choice: Option<String>,
    /// Whether the recovery phrase is on screen.
    phrase_visible: bool,
    /// The restore phrase box.
    restore_text: String,
    /// The first press of Restore has been made and a second will replace
    /// the identity.
    restore_armed: bool,
    /// One sentence shown above the screen; cleared by the next tap.
    notice: Option<String>,
    /// Whether the microphone may be used, as of the last start or resume.
    mic_permission: bool,
    /// Looked up on start and resume rather than every frame: it opens a
    /// socket to ask.
    local_ip: Option<String>,

    /// Seals chat history and the call log. The desktop's key file, made
    /// the desktop's way, in this app's private storage.
    backup_key: Result<BackupKey, String>,
    /// The address book.
    contacts: Contacts,
    /// Past calls, newest first.
    calls: Vec<CallRecord>,
    /// The first press of "Clear call history" has been made.
    clear_calls_armed: bool,
    /// The name box for saving the person on the call. Filled from their
    /// Hello when it arrives and the box is still empty.
    save_name: String,
    /// What this phone dialled for the call in progress, or `None` when
    /// the call came in. Kept for "save as contact".
    dialled: Option<String>,

    /// The conversation on screen: the person on the call, or one opened
    /// from contacts to read back.
    chat: Option<Conversation>,
    /// The message box.
    draft: String,
    /// The id of our message being edited, if the message box holds an
    /// edit.
    editing: Option<u64>,
    /// Messages from them that arrived while the Chat screen was not
    /// showing, for the tab's badge.
    unread: usize,

    /// The send queue and the transfers running.
    files: FileManager,
    /// Who the connected call is with, hex, as of the last poll: noticing
    /// it change is how a call's start and end are handled once.
    live_peer: Option<String>,
    /// What they call themselves, from their Hello.
    peer_display_name: Option<String>,

    /// Whether the app is on screen; notifications are only for when it
    /// is not.
    foreground: bool,
    /// The network as last reported by the platform.
    network: Network,
    /// Whether battery saver was on when last checked.
    battery_saver: bool,
    /// Mobile data used by transfers this month, copied into the settings
    /// now and then by [`save_meter`](Self::save_meter).
    meter: CellularMeter,
    /// The mobile data limit box as typed.
    limit_text: String,
    /// What the platform was last told must keep running, so it is told
    /// only when that changes.
    background_work: BackgroundWork,

    /// The shared interface's own state: the screen, dialogs, drafts.
    stoop: antiphon_ui::Ui,
    /// What the interface draws, rebuilt by [`refresh`](Self::refresh)
    /// after every change and before every frame.
    model: antiphon_ui::Model,
    /// The theme and accent the palette was last built for.
    look: (String, String),
    /// The invite link and its QR code, rebuilt only when the link changes.
    qr: Option<(String, Arc<qr_code::Data>)>,
    /// The last message with each person, hex key to (text, unix seconds),
    /// for the people list.
    previews: HashMap<String, (String, u64)>,
    /// Muted from the call screen: nothing is sent, however the mic is keyed.
    muted: bool,
    /// The push-to-talk pad is held.
    talking: bool,
    /// When the call on now connected, for its clock.
    connected_at: Option<Instant>,
    /// The window's logical size as last told to the interface.
    size: Size,
}

/// Loads this phone's identity from `dir`, creating one on first start.
fn load_identity(dir: &std::path::Path) -> Result<IdentityKeyPair, String> {
    let path = antiphon_crypto::store::identity_path(dir);
    antiphon_crypto::store::load_or_create(&path).map_err(|e| format!("Could not load the identity at {}: {e}", path.display()))
}

impl App {
    /// Loads everything from `dir` and starts the call engine. `wake` is
    /// how the engine wakes the runner's event loop. Fails only when the
    /// engine cannot start; a missing identity or backup key is shown on
    /// screen instead.
    pub fn new(dir: PathBuf, wake: Waker) -> Result<Self, String> {
        let settings = Settings::load(&dir);
        let identity = load_identity(&dir);
        let engine = CallEngine::new(settings.chain.clone(), settings.open_mic, settings.speaker, wake)
            .map_err(|e| format!("could not start the call engine: {e}"))?;
        engine.set_display_name(&settings.display_name);
        // Received files are stored, and partial ones kept for resuming,
        // under the same folder the desktop's modules would use.
        crate::config::set_dir(dir.clone());
        let backup_key = crate::history::load_or_create_backup_key(&dir)
            .map_err(|e| format!("Could not open the key that protects your messages: {e}"));
        let calls = backup_key.as_ref().map(|key| call_log::load(&dir, key)).unwrap_or_default();
        let meter = CellularMeter { month: settings.cellular_month, used_bytes: settings.cellular_used_bytes };
        let contacts = Contacts::load(&dir);
        // The people list shows each person's last message; read once here
        // and kept up to date as messages come and go.
        let mut previews = HashMap::new();
        if let Ok(key) = &backup_key {
            for c in &contacts.list {
                if let Some(last) = crate::history::load(&dir, &c.identity_key, key).into_iter().rev().find(|m| !m.deleted) {
                    let text = if last.from_me { format!("You: {}", last.text) } else { last.text };
                    previews.insert(c.identity_key.clone(), (text, last.at_secs));
                }
            }
        }
        let mut app = Self {
            port_text: settings.listen_port.to_string(),
            limit_text: if settings.cellular_limit_mb == 0 { String::new() } else { settings.cellular_limit_mb.to_string() },
            contacts,
            files: FileManager::new(dir.clone()),
            dir,
            settings,
            identity,
            engine,
            eq_choice: None,
            phrase_visible: false,
            restore_text: String::new(),
            restore_armed: false,
            notice: None,
            mic_permission: platform::has_mic_permission(),
            local_ip: call::local_ip(),
            backup_key,
            calls,
            clear_calls_armed: false,
            save_name: String::new(),
            dialled: None,
            chat: None,
            draft: String::new(),
            editing: None,
            unread: 0,
            live_peer: None,
            peer_display_name: None,
            foreground: true,
            network: platform::network(),
            battery_saver: platform::battery_saver(),
            meter,
            background_work: BackgroundWork::default(),
            stoop: antiphon_ui::Ui::new(),
            model: antiphon_ui::Model::default(),
            look: Default::default(),
            qr: None,
            previews,
            muted: false,
            talking: false,
            connected_at: None,
            size: Size::new(0.0, 0.0),
        };
        app.refresh();
        Ok(app)
    }

    /// Something is moving on screen -- a call or mic test's level meter,
    /// and with a call, any file's progress bar -- so the runner should
    /// keep redrawing rather than wait for input.
    pub fn is_animating(&self) -> bool {
        self.engine.mic_test_running()
            || matches!(self.engine.phase(), Phase::Connected { .. } | Phase::Dialing { .. })
            || self.stoop.needs_frames()
    }

    /// The activity came back to the front, possibly from the permission
    /// prompt, whose answer only shows up this way.
    pub fn resumed(&mut self) {
        self.foreground = true;
        self.mic_permission = platform::has_mic_permission();
        self.local_ip = call::local_ip();
        self.network = platform::network();
        self.battery_saver = platform::battery_saver();
        if self.live_peer.is_some() && self.stoop.open_person() == self.live_peer.as_deref() {
            self.unread = 0;
        }
        self.refresh();
    }

    /// The app went to the background. Waiting for calls keeps a socket
    /// open and the phone awake, so it stops here unless the user wants it
    /// kept and battery saver does not say otherwise; a call or a file in
    /// progress carries on regardless, since stopping either would lose
    /// something the user started.
    pub fn suspended(&mut self) {
        self.foreground = false;
        self.battery_saver = platform::battery_saver();
        let keep = data_rules::listen_in_background(
            self.settings.keep_listening_in_background,
            self.battery_saver,
            self.settings.listen_on_battery_saver,
        );
        if matches!(self.engine.phase(), Phase::Listening { .. }) && !keep {
            self.engine.stop();
            self.notice = Some(if self.settings.keep_listening_in_background {
                "Stopped waiting for calls when you left the app, because battery saver is on. Settings can change this.".into()
            } else {
                "Stopped waiting for calls when you left the app. Settings can keep it listening.".into()
            });
        }
        self.save_meter();
        self.poll();
    }

    /// Something the phone reported from outside the interface: a picked
    /// file, a notification button, a network change.
    pub fn platform_event(&mut self, event: platform::PlatformEvent) {
        use platform::{CallAction, PlatformEvent};
        match event {
            PlatformEvent::FilePicked(picked) => {
                let Some(peer) = self.live_peer.clone() else {
                    self.notice = Some("The call ended before the file was picked. Send it during your next call.".into());
                    return;
                };
                if let Err(e) = self.files.enqueue(peer, picked) {
                    self.notice = Some(e);
                }
            }
            PlatformEvent::FilePickCancelled => {}
            PlatformEvent::NetworkChanged(network) => self.network = network,
            PlatformEvent::CallAction(CallAction::Answer) => platform::ring(None),
            PlatformEvent::CallAction(CallAction::HangUp | CallAction::Decline) => {
                platform::ring(None);
                self.engine.stop();
            }
            PlatformEvent::CallAction(CallAction::Mute(muted)) => {
                // Muting from outside the app means "stop transmitting",
                // whichever way the mic was keyed; unmuting puts back what
                // the saved setting says, without changing it.
                self.engine.set_ptt(false);
                self.engine.set_open_mic(!muted && self.settings.open_mic);
            }
            other => log::info!("platform event: {other:?}"),
        }
    }

    /// The network in the terms the data rules use.
    fn link(&self) -> Link {
        match self.network {
            Network::Unknown => Link::Unknown,
            Network::None => Link::Offline,
            Network::Wifi => Link::Wifi,
            Network::Metered => Link::Metered,
        }
    }

    /// The data settings in the terms the data rules use.
    fn data_settings(&self) -> DataSettings {
        DataSettings {
            wifi_only: self.settings.wifi_only_transfers,
            cellular_limit_mb: self.settings.cellular_limit_mb,
            data_saver: self.settings.data_saver,
        }
    }

    /// Whether file transfers must wait right now, and why.
    fn transfer_hold(&self) -> Option<data_rules::Hold> {
        data_rules::transfer_hold(self.data_settings(), self.link(), self.meter.used(now_secs()))
    }

    /// Copies the data meter into the settings file. Not on every chunk --
    /// that would be a file write a few times a second -- but on leaving
    /// the app and at each megabyte.
    fn save_meter(&mut self) {
        if self.settings.cellular_month != self.meter.month || self.settings.cellular_used_bytes != self.meter.used_bytes {
            self.settings.cellular_month = self.meter.month;
            self.settings.cellular_used_bytes = self.meter.used_bytes;
            self.save();
        }
    }

    /// The name to show for a key: yours for them if saved, otherwise the
    /// name they sent if they are on the call, otherwise a fingerprint.
    fn display_name_for(&self, key_hex: &str) -> String {
        match (self.contacts.by_key(key_hex), &self.peer_display_name) {
            (Some(contact), _) => contact.name.clone(),
            (None, Some(theirs)) if self.live_peer.as_deref() == Some(key_hex) => format!("{theirs} (unsaved)"),
            _ => self.contacts.name_for(key_hex),
        }
    }

    /// Opens the conversation with `key_hex`, unless it is already the one
    /// open. Shows why instead when there is no backup key to read it with.
    fn open_chat(&mut self, key_hex: &str) {
        let Ok(key) = &self.backup_key else {
            self.notice = self.backup_key.as_ref().err().cloned();
            return;
        };
        if self.chat.as_ref().is_some_and(|c| c.peer_hex == key_hex) {
            return;
        }
        let expiry = self.contacts.by_key(key_hex).and_then(|c| c.expire_after_secs);
        self.chat = Some(Conversation::open(&self.dir, key, key_hex, expiry));
        self.editing = None;
        self.draft.clear();
    }

    /// Takes in what the call delivered since the last look: ended calls,
    /// chat, file progress. Returns whether anything changed on screen.
    /// Called by the runner every time it wakes, so it stays cheap when
    /// nothing happened.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;

        for summary in self.engine.take_finished() {
            if let Ok(key) = &self.backup_key {
                call_log::record(&mut self.calls, CallRecord::from(&summary));
                call_log::save(&self.dir, key, &self.calls);
            }
            changed = true;
        }

        let live = self.engine.live();
        let live_peer = live.as_ref().map(|l| hex(&l.peer_key));
        if live_peer != self.live_peer {
            self.live_peer = live_peer.clone();
            self.peer_display_name = None;
            self.unread = 0;
            self.connected_at = live_peer.as_ref().map(|_| Instant::now());
            self.muted = false;
            self.talking = false;
            // A call that starts brings up their conversation, with the
            // call at the top of it.
            if let Some(peer) = &live_peer {
                for action in self.stoop.show(antiphon_ui::Route::Person(peer.clone())) {
                    self.act(action);
                }
            }
            if let (Some(peer), Some(live)) = (&live_peer, &live) {
                // A call starts: their conversation comes up, and anything
                // typed to them while they were away goes now.
                self.chat = None;
                self.open_chat(peer);
                if let Some(chat) = self.chat.as_mut() {
                    chat.flush_outbox(live);
                }
                self.save_name = String::new();
            }
            changed = true;
        }

        if let Some(live) = &live {
            let peer = hex(&live.peer_key);
            for message in live.take_inbox() {
                changed = true;
                if let ControlMessage::Hello(hello) = message {
                    let name = hello.display_name.trim().to_string();
                    if !name.is_empty() {
                        if self.save_name.is_empty() {
                            self.save_name = name.clone();
                        }
                        self.peer_display_name = Some(name);
                    }
                    continue;
                }
                // Their messages always land in their history, even while
                // another conversation is open on screen.
                let new_text = match self.chat.as_mut().filter(|c| c.peer_hex == peer) {
                    Some(chat) => chat.receive(message),
                    None => match &self.backup_key {
                        Ok(key) => {
                            let expiry = self.contacts.by_key(&peer).and_then(|c| c.expire_after_secs);
                            Conversation::open(&self.dir, key, &peer, expiry).receive(message)
                        }
                        Err(_) => None,
                    },
                };
                if let Some(text) = new_text {
                    self.previews.insert(peer.clone(), (text.clone(), now_secs()));
                    if !self.foreground || self.stoop.open_person() != Some(peer.as_str()) {
                        self.unread += 1;
                    }
                    if !self.foreground {
                        platform::notify(&self.display_name_for(&peer), &text);
                    }
                }
            }
        }

        // The data rules, re-applied every time: the network can change
        // under a running transfer.
        let link = self.link();
        let saver = data_rules::data_saver_active(self.data_settings(), link);
        self.engine.set_data_saver(saver);
        crate::config::set_transfer_limits(crate::transfer_limits::TransferLimits {
            upload_kbps: data_rules::upload_cap_kbps(self.data_settings(), link),
            ..Default::default()
        });
        let hold = self.transfer_hold();
        let before_mb = self.meter.used_bytes / (1024 * 1024);
        let metered = (link == Link::Metered).then_some((&mut self.meter, now_secs()));
        let events = self.files.poll(live.as_deref(), hold, !saver, metered);
        if self.meter.used_bytes / (1024 * 1024) != before_mb {
            self.save_meter();
        }
        for event in events {
            changed = true;
            if self.foreground {
                continue;
            }
            let who = self.live_peer.as_deref().map(|p| self.display_name_for(p)).unwrap_or_default();
            match event {
                files::Event::Offer { name } => platform::notify(&who, &format!("wants to send you {name}")),
                files::Event::Saved { name } => platform::notify("File received", &name),
                files::Event::Sent { name } => platform::notify("File sent", &name),
                files::Event::Failed { line } => platform::notify("File transfer", &line),
            }
        }

        if let Some(chat) = self.chat.as_mut() {
            changed |= chat.prune_expired();
        }

        let phase = self.engine.phase();
        let work = BackgroundWork {
            call: match &phase {
                Phase::Connected { peer_key, .. } => Some(self.display_name_for(&hex(peer_key))),
                Phase::Dialing { remote } => Some(remote.clone()),
                _ => None,
            },
            listening: matches!(phase, Phase::Listening { .. }),
            transfers: self.files.transfer_count(live.as_deref()),
        };
        if work != self.background_work {
            // Someone called in while the app was out of sight. Listening
            // answers by itself, so the call is already up; the alert is
            // what tells the person holding the phone, and Decline hangs up.
            let call_arrived = self.background_work.listening && self.background_work.call.is_none();
            match &work.call {
                Some(name) if call_arrived && !self.foreground => platform::ring(Some(name)),
                None if self.background_work.call.is_some() => platform::ring(None),
                _ => {}
            }
            platform::set_background_work(&work);
            self.background_work = work;
            changed = true;
        }
        changed
    }

    /// Every finger is off the screen. Push-to-talk must never stay keyed
    /// because the release landed somewhere the button did not see.
    pub fn touch_ended(&mut self) {
        self.talking = false;
        self.engine.set_ptt(false);
    }

    /// Writes the settings file.
    fn save(&self) {
        self.settings.save(&self.dir);
    }

    /// Hands the changed chain to the engine, which applies it at once, and
    /// saves it.
    fn chain_changed(&mut self) {
        self.engine.set_chain(self.settings.chain.clone());
        self.save();
    }

    /// Applies one message. The call, identity and voice ones are handled
    /// here and the rest in [`update_messaging`](Self::update_messaging).
    /// Any message but the ones a held button or typing send clears the
    /// notice, and anything else disarms a pending two-press action.
    pub fn update(&mut self, message: Message) {
        let clears_notice = match &message {
            // Frames, typing and the like leave it; a tap that does
            // something, or moves to another screen, clears it.
            Message::Ui(m) => matches!(m, antiphon_ui::Msg::Do(_) | antiphon_ui::Msg::Nav(_) | antiphon_ui::Msg::Back),
            Message::PttPressed | Message::PttReleased => false,
            _ => true,
        };
        if clears_notice {
            self.notice = None;
        }
        if let Message::Ui(msg) = message {
            let actions = self.stoop.update(msg, &self.model);
            for action in actions {
                self.act(action);
            }
            self.refresh();
            return;
        }
        if !matches!(message, Message::ClearCalls) {
            self.clear_calls_armed = false;
        }
        match message {
            Message::PortChanged(value) => {
                self.port_text = value.chars().filter(char::is_ascii_digit).take(5).collect();
                if let Ok(port) = self.port_text.parse::<u16>() {
                    self.settings.listen_port = port;
                    self.save();
                }
            }
            Message::Listen => {
                let Some(identity) = self.call_identity() else { return };
                let Ok(port) = self.port_text.parse::<u16>() else {
                    self.notice = Some("Enter a port number between 1 and 65535.".into());
                    return;
                };
                self.dialled = None;
                self.engine.listen(port, identity);
            }
            Message::Stop => self.engine.stop(),
            Message::PttPressed => self.engine.set_ptt(true),
            Message::PttReleased => self.engine.set_ptt(false),
            Message::OpenMic(on) => {
                self.settings.open_mic = on;
                self.engine.set_open_mic(on);
                self.save();
            }
            Message::Speaker(on) => {
                self.settings.speaker = on;
                self.engine.set_speaker(on);
                self.save();
            }
            Message::ShowPhrase(show) => self.phrase_visible = show,
            Message::Restore => self.restore(),
            Message::RequestMic => platform::request_mic_permission(),
            Message::MicTest(on) => {
                if on {
                    if !self.mic_permission {
                        platform::request_mic_permission();
                        return;
                    }
                    self.engine.start_mic_test();
                } else {
                    self.engine.stop();
                }
            }
            Message::EqPreset(name) => {
                self.settings.chain.eq_bands = eq_preset(&name).unwrap_or_default();
                self.eq_choice = Some(name);
                self.chain_changed();
            }
            Message::Toggle(stage, on) => {
                let chain = &mut self.settings.chain;
                match stage {
                    Stage::NoiseSuppress => chain.noise_suppress.enabled = on,
                    Stage::NoiseGate => chain.noise_gate.enabled = on,
                    Stage::HighPass => chain.high_pass.enabled = on,
                    Stage::LowPass => chain.low_pass.enabled = on,
                    Stage::DeEsser => chain.de_esser.enabled = on,
                    Stage::Compressor => chain.compressor.enabled = on,
                    Stage::Limiter => chain.limiter.enabled = on,
                    Stage::AutoGain => match &mut chain.gain {
                        GainConfig::Fixed { enabled, .. } | GainConfig::Auto { enabled, .. } => *enabled = on,
                    },
                    Stage::PitchShift => chain.pitch_shift.enabled = on,
                    Stage::Reverb => chain.reverb.enabled = on,
                }
                self.chain_changed();
            }
            Message::Param(param, value) => {
                let chain = &mut self.settings.chain;
                match param {
                    Param::SuppressStrength => chain.noise_suppress.strength = value,
                    Param::GateThreshold => chain.noise_gate.threshold_db = value,
                    Param::HighPassCutoff => chain.high_pass.cutoff_hz = value,
                    Param::LowPassCutoff => chain.low_pass.cutoff_hz = value,
                    Param::CompressorThreshold => chain.compressor.threshold_db = value,
                    Param::CompressorRatio => chain.compressor.ratio = value,
                    Param::PitchSemitones => chain.pitch_shift.semitones = value,
                    Param::ReverbMix => chain.reverb.wet_dry = value,
                    Param::ReverbRoom => chain.reverb.room_size = value,
                }
                self.chain_changed();
            }
            Message::ResetChain => {
                self.settings.chain = ChainConfig::default_preset();
                self.eq_choice = None;
                self.chain_changed();
            }
            other => self.update_messaging(other),
        }
    }

    /// The identity a call runs as, or `None` with the reason on screen.
    /// Checks the microphone first, since a call without it is no call.
    fn call_identity(&mut self) -> Option<IdentityKeyPair> {
        if !self.mic_permission {
            platform::request_mic_permission();
            self.notice = Some("Stoop needs the microphone for calls. Allow it, then try again.".into());
            return None;
        }
        match &self.identity {
            // Not Clone, deliberately; the seed rebuilds an equal copy
            // for the call task to own.
            Ok(identity) => Some(IdentityKeyPair::from_master_seed(identity.master_seed())),
            Err(e) => {
                self.notice = Some(e.clone());
                None
            }
        }
    }

    /// Dials `target` and shows the Call screen, once the microphone and
    /// identity are in order.
    fn start_call(&mut self, target: &str) {
        if let Some(identity) = self.call_identity() {
            self.dialled = Some(target.to_string());
            self.engine.call(target, identity);
        }
    }

    /// Contacts, chat, files, recent calls and settings.
    fn update_messaging(&mut self, message: Message) {
        let live = self.engine.live();
        let live_here = |chat: &Conversation| live.as_ref().filter(|l| hex(&l.peer_key) == chat.peer_hex).cloned();
        match message {
            Message::CallContact(key) => {
                let Some(contact) = self.contacts.by_key(&key) else { return };
                match contact.dial_target() {
                    Some(target) => self.start_call(&target),
                    None => {
                        self.notice = Some(format!(
                            "There is no address saved for {}. Ask them for an invite with their address and add it again.",
                            contact.name
                        ))
                    }
                }
            }
            Message::SaveCallerName(value) => self.save_name = value,
            Message::SaveCaller => {
                let Phase::Connected { peer_key, .. } = self.engine.phase() else { return };
                let name = self.save_name.trim().to_string();
                if name.is_empty() {
                    self.notice = Some("Give them a name first. It is only for you; they never see it.".into());
                    return;
                }
                // Only an outgoing call's target is somewhere they listen;
                // an incoming call's source port is not.
                let dialled = self
                    .calls_in_progress_target()
                    .unwrap_or_default();
                self.contacts.add_from_call(&name, &peer_key, &dialled, now_secs());
                self.contacts.save(&self.dir);
                self.notice = Some(format!("Saved {name}."));
            }
            Message::MarkVerified(key, verified) => {
                if let Some(contact) = self.contacts.by_key_mut(&key) {
                    contact.verified = verified;
                    self.contacts.save(&self.dir);
                }
            }

            Message::SendChat => {
                let Some(chat) = self.chat.as_mut() else { return };
                let here = live_here(chat);
                match self.editing.take() {
                    Some(id) => {
                        if !chat.edit(id, &self.draft, here.as_deref()) {
                            self.notice = Some("That message can no longer be edited.".into());
                        }
                    }
                    None => chat.send(&self.draft, here.as_deref()),
                }
                self.draft.clear();
            }
            Message::DeleteChat(id) => {
                if let Some(chat) = self.chat.as_mut() {
                    let here = live_here(chat);
                    chat.delete(id, here.as_deref());
                }
                    }
            Message::React(id, emoji) => {
                if let Some(chat) = self.chat.as_mut() {
                    if let Some(live) = live_here(chat) {
                        chat.react(id, emoji, &live);
                    }
                }
                    }
            Message::Expiry(expiry) => {
                let Some(chat) = self.chat.as_mut() else { return };
                chat.set_expiry(expiry.secs());
                match self.contacts.by_key_mut(&chat.peer_hex) {
                    Some(contact) => {
                        contact.expire_after_secs = expiry.secs();
                        self.contacts.save(&self.dir);
                    }
                    // Kept for this conversation only while it is open: the
                    // setting lives on a contact, as on the desktop.
                    None => self.notice = Some("Save them as a contact to keep this setting after the call.".into()),
                }
            }

            Message::PickFile => {
                if live.is_none() {
                    self.notice = Some("Files are sent during a call. Call them first.".into());
                    return;
                }
                platform::pick_file();
            }
            Message::AcceptOffer => {
                if let Some(live) = &live {
                    self.files.accept_offer(live);
                }
            }
            Message::DeclineOffer => {
                if let Some(live) = &live {
                    self.files.decline_offer(live);
                }
            }
            Message::CancelIncoming => {
                if let Some(live) = &live {
                    self.files.cancel_incoming(live);
                }
            }
            Message::PauseSend(paused) => self.files.set_user_paused(live.as_deref(), paused),
            Message::CancelSend => self.files.cancel_active(live.as_deref()),
            Message::QueueUp(id) => {
                self.files.queue.move_up(id);
            }
            Message::QueueDown(id) => {
                self.files.queue.move_down(id);
            }
            Message::QueueHold(id, held) => {
                self.files.queue.set_held(id, held);
            }
            Message::QueueRemove(id) => {
                self.files.queue.remove(id);
            }

            Message::CallBack(index) => {
                let Some(record) = self.calls.get(index) else { return };
                let target = self
                    .contacts
                    .by_key(&record.peer_key)
                    .and_then(|c| c.dial_target())
                    .or_else(|| record.outgoing.then(|| record.remote.clone()));
                match target {
                    Some(target) => self.start_call(&target),
                    None => self.notice = Some("They called you, so there is no address to call back. Save them as a contact with their invite.".into()),
                }
            }
            Message::ClearCalls => {
                if self.clear_calls_armed {
                    self.calls.clear();
                    if let Ok(key) = &self.backup_key {
                        call_log::save(&self.dir, key, &self.calls);
                    }
                    self.clear_calls_armed = false;
                } else {
                    self.clear_calls_armed = true;
                }
            }

            Message::DisplayName(value) => {
                self.settings.display_name = value;
                self.engine.set_display_name(&self.settings.display_name);
                self.save();
            }
            Message::WifiOnly(on) => {
                self.settings.wifi_only_transfers = on;
                self.save();
            }
            Message::CellularLimit(value) => {
                self.limit_text = value.chars().filter(char::is_ascii_digit).take(6).collect();
                self.settings.cellular_limit_mb = self.limit_text.parse().unwrap_or(0);
                self.save();
            }
            Message::DataSaver(on) => {
                self.settings.data_saver = on;
                self.save();
            }
            Message::KeepListening(on) => {
                // Without the exemption, Doze cuts the network a few
                // minutes after the screen goes off, and the setting would
                // quietly stop working.
                if on {
                    platform::request_battery_exemption();
                }
                self.settings.keep_listening_in_background = on;
                self.save();
            }
            Message::ListenOnSaver(on) => {
                self.settings.listen_on_battery_saver = on;
                self.save();
            }
            _ => {}
        }
    }

    /// What the call in progress was dialled with, if this phone dialled.
    fn calls_in_progress_target(&self) -> Option<String> {
        // The engine's phase only keeps the resolved address; the dial box
        // or the contact holds what was typed, which is what to save.
        let Phase::Connected { peer_key, .. } = self.engine.phase() else { return None };
        if let Some(contact) = self.contacts.by_key(&hex(&peer_key)) {
            return Some(contact.address.clone());
        }
        self.dialled.clone()
    }

    /// Restoring takes two presses: replacing an identity makes this phone
    /// a different person to every contact who pinned the old key, so it
    /// should not happen because someone pasted a phrase to see what it did.
    fn restore(&mut self) {
        if self.engine.is_busy() {
            self.notice = Some("Hang up before restoring an identity.".into());
            return;
        }
        let seed = match antiphon_crypto::mnemonic::seed_from_phrase(self.restore_text.trim()) {
            Ok(seed) => seed,
            Err(e) => {
                self.notice = Some(format!("That phrase is not valid: {e}"));
                return;
            }
        };
        let restored = IdentityKeyPair::from_master_seed(seed);
        if let Ok(current) = &self.identity {
            if current.public().identity_key == restored.public().identity_key {
                self.notice = Some("That phrase already matches this phone's identity. Nothing to do.".into());
                self.restore_text.clear();
                return;
            }
            if !self.restore_armed {
                self.restore_armed = true;
                return;
            }
        }
        let path = antiphon_crypto::store::identity_path(&self.dir);
        match antiphon_crypto::store::save(&path, &restored) {
            Ok(()) => {
                self.identity = Ok(restored);
                self.notice = Some("Identity restored.".into());
                self.restore_text.clear();
                self.restore_armed = false;
                self.phrase_visible = false;
            }
            Err(e) => self.notice = Some(format!("Could not save the identity: {e}")),
        }
    }

    /// The whole screen. `top` and `bottom` are the logical pixels the
    /// system draws over (status bar, gesture bar or keyboard), kept clear.
    pub fn view(&self, top: f32, bottom: f32) -> Element<'_, Message, Theme, Renderer> {
        container(self.stoop.view(&self.model).map(Message::Ui))
            .padding(Padding { top, bottom, ..Padding::ZERO })
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(iced_core::Background::Color(antiphon_ui::theme::look().bg)),
                ..Default::default()
            })
            .into()
    }

    /// The iced theme for the current palette, which also gives the
    /// colour behind the system bars.
    pub fn theme(&self) -> Theme {
        antiphon_ui::theme::iced_theme(&antiphon_ui::theme::look())
    }

    /// The window's size changed, in logical pixels: the interface picks
    /// its phone or tablet layout from it.
    pub fn resized(&mut self, size: Size) {
        if size != self.size {
            self.size = size;
            let _ = self.stoop.update(antiphon_ui::Msg::Resized(size), &self.model);
        }
    }

    /// A frame is about to be drawn: moves animations and timers on, and
    /// brings the model up to date (meters, the call clock).
    pub fn frame(&mut self) {
        let _ = self.stoop.update(antiphon_ui::Msg::Frame(Instant::now()), &self.model);
        self.refresh();
    }
}
