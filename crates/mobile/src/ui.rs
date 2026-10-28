//! The interface, in iced: the call, chat, files, contacts, recent calls,
//! voice, settings and identity, as tabs.
//!
//! Plain Elm-style state, messages and view, so it reads like any other
//! iced app; [`crate::runner`] is what feeds it events and draws it, and
//! calls [`App::poll`] whenever it wakes so what the call delivered --
//! messages, file progress, the end of a call -- reaches the screen and
//! the disk. The newer screens' views are in [`views`].

mod views;

use std::path::PathBuf;

use antiphon_audio::{eq_preset, eq_preset_names, ChainConfig, GainConfig, THIN_VOICE_WARNING_STRENGTH};
use antiphon_crypto::{BackupKey, IdentityKeyPair, Invite};
use iced_core::text::Wrapping;
use iced_core::{Alignment, Element, Font, Length, Padding, Theme};
use iced_widget::{
    button, column, container, mouse_area, pick_list, progress_bar, row, scrollable, slider, space, text, text_input,
    toggler,
};

use crate::call::{self, now_secs, CallEngine, Phase, Waker};
use crate::call_log::{self, CallRecord};
use crate::contacts::{hex, Contacts};
use crate::control::ControlMessage;
use crate::conversation::Conversation;
use crate::data_rules::{self, CellularMeter, DataSettings, Link};
use crate::files::{self, FileManager};
use crate::platform::{self, BackgroundWork, Network};
use crate::settings::Settings;

pub type Renderer = iced_wgpu::Renderer;

/// The face bundled through iced's `fira-sans` feature, named so cosmic-text
/// never falls back to whatever the vendor ships as sans-serif.
pub const FONT: Font = Font::with_name("Fira Sans");


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Call,
    Chat,
    Files,
    Contacts,
    Recent,
    Voice,
    Settings,
    Identity,
}

#[derive(Debug, Clone, Copy)]
pub enum Stage {
    NoiseSuppress,
    NoiseGate,
    HighPass,
    LowPass,
    DeEsser,
    Compressor,
    Limiter,
    AutoGain,
    PitchShift,
    Reverb,
}

#[derive(Debug, Clone, Copy)]
pub enum Param {
    SuppressStrength,
    GateThreshold,
    HighPassCutoff,
    LowPassCutoff,
    CompressorThreshold,
    CompressorRatio,
    PitchSemitones,
    ReverbMix,
    ReverbRoom,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tab(Tab),
    PortChanged(String),
    RemoteChanged(String),
    PasteRemote,
    Listen,
    Call,
    Stop,
    PttPressed,
    PttReleased,
    OpenMic(bool),
    Speaker(bool),
    Copy(String),
    ShowPhrase(bool),
    RestoreChanged(String),
    PasteRestore,
    Restore,
    CancelRestore,
    RequestMic,
    MicTest(bool),
    EqPreset(String),
    Toggle(Stage, bool),
    Param(Param, f32),
    ResetChain,

    // Contacts.
    ContactName(String),
    ContactInvite(String),
    PasteContactInvite,
    AddContact,
    CallContact(String),
    RemoveContact(String),
    SaveCallerName(String),
    SaveCaller,
    MarkVerified(String, bool),

    // Chat.
    OpenChat(String),
    CloseChat,
    Draft(String),
    SendChat,
    SelectChat(Option<u64>),
    EditChat(u64),
    CancelEdit,
    DeleteChat(u64),
    React(u64, &'static str),
    Expiry(Expiry),

    // Files.
    PickFile,
    AcceptOffer,
    DeclineOffer,
    CancelIncoming,
    PauseSend(bool),
    CancelSend,
    QueueUp(u64),
    QueueDown(u64),
    QueueHold(u64, bool),
    QueueRemove(u64),

    // Recent calls.
    CallBack(usize),
    ClearCalls,

    // Settings.
    DisplayName(String),
    WifiOnly(bool),
    CellularLimit(String),
    DataSaver(bool),
    KeepListening(bool),
    ListenOnSaver(bool),
}

/// The disappearing-message choices offered. A short list, as on the
/// desktop, rather than a free duration nobody can reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Off,
    FiveMinutes,
    Hour,
    Day,
    Week,
}

impl Expiry {
    pub const ALL: [Expiry; 5] = [Expiry::Off, Expiry::FiveMinutes, Expiry::Hour, Expiry::Day, Expiry::Week];

    pub fn secs(self) -> Option<u64> {
        match self {
            Expiry::Off => None,
            Expiry::FiveMinutes => Some(300),
            Expiry::Hour => Some(3_600),
            Expiry::Day => Some(86_400),
            Expiry::Week => Some(7 * 86_400),
        }
    }

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

pub struct App {
    dir: PathBuf,
    settings: Settings,
    identity: Result<IdentityKeyPair, String>,
    engine: CallEngine,
    tab: Tab,
    port_text: String,
    eq_choice: Option<String>,
    phrase_visible: bool,
    restore_text: String,
    restore_armed: bool,
    notice: Option<String>,
    mic_permission: bool,
    /// Looked up on start and resume rather than every frame: it opens a
    /// socket to ask.
    local_ip: Option<String>,

    /// Seals chat history and the call log. The desktop's key file, made
    /// the desktop's way, in this app's private storage.
    backup_key: Result<BackupKey, String>,
    contacts: Contacts,
    calls: Vec<CallRecord>,
    clear_calls_armed: bool,
    contact_name: String,
    contact_invite: String,
    remove_armed: Option<String>,
    save_name: String,
    /// What this phone dialled for the call in progress, or `None` when
    /// the call came in. Kept for "save as contact".
    dialled: Option<String>,

    /// The conversation on screen: the person on the call, or one opened
    /// from contacts to read back.
    chat: Option<Conversation>,
    draft: String,
    editing: Option<u64>,
    selected: Option<u64>,
    unread: usize,

    files: FileManager,
    /// Who the connected call is with, hex, as of the last poll: noticing
    /// it change is how a call's start and end are handled once.
    live_peer: Option<String>,
    /// What they call themselves, from their Hello.
    peer_display_name: Option<String>,

    /// Whether the app is on screen; notifications are only for when it
    /// is not.
    foreground: bool,
    network: Network,
    battery_saver: bool,
    meter: CellularMeter,
    limit_text: String,
    background_work: BackgroundWork,
}

fn load_identity(dir: &std::path::Path) -> Result<IdentityKeyPair, String> {
    let path = antiphon_crypto::store::identity_path(dir);
    antiphon_crypto::store::load_or_create(&path).map_err(|e| format!("Could not load the identity at {}: {e}", path.display()))
}

impl App {
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
        Ok(Self {
            port_text: settings.listen_port.to_string(),
            limit_text: if settings.cellular_limit_mb == 0 { String::new() } else { settings.cellular_limit_mb.to_string() },
            contacts: Contacts::load(&dir),
            files: FileManager::new(dir.clone()),
            dir,
            settings,
            identity,
            engine,
            tab: Tab::Call,
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
            contact_name: String::new(),
            contact_invite: String::new(),
            remove_armed: None,
            save_name: String::new(),
            dialled: None,
            chat: None,
            draft: String::new(),
            editing: None,
            selected: None,
            unread: 0,
            live_peer: None,
            peer_display_name: None,
            foreground: true,
            network: platform::network(),
            battery_saver: platform::battery_saver(),
            meter,
            background_work: BackgroundWork::default(),
        })
    }

    /// Something is moving on screen -- a call or mic test's level meter,
    /// and with a call, any file's progress bar -- so the runner should
    /// keep redrawing rather than wait for input.
    pub fn is_animating(&self) -> bool {
        self.engine.mic_test_running() || matches!(self.engine.phase(), Phase::Connected { .. })
    }

    /// The activity came back to the front, possibly from the permission
    /// prompt, whose answer only shows up this way.
    pub fn resumed(&mut self) {
        self.foreground = true;
        self.mic_permission = platform::has_mic_permission();
        self.local_ip = call::local_ip();
        self.network = platform::network();
        self.battery_saver = platform::battery_saver();
        if self.tab == Tab::Chat {
            self.unread = 0;
        }
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

    fn link(&self) -> Link {
        match self.network {
            Network::Unknown => Link::Unknown,
            Network::None => Link::Offline,
            Network::Wifi => Link::Wifi,
            Network::Metered => Link::Metered,
        }
    }

    fn data_settings(&self) -> DataSettings {
        DataSettings {
            wifi_only: self.settings.wifi_only_transfers,
            cellular_limit_mb: self.settings.cellular_limit_mb,
            data_saver: self.settings.data_saver,
        }
    }

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

    fn display_name_for(&self, key_hex: &str) -> String {
        match (self.contacts.by_key(key_hex), &self.peer_display_name) {
            (Some(contact), _) => contact.name.clone(),
            (None, Some(theirs)) if self.live_peer.as_deref() == Some(key_hex) => format!("{theirs} (unsaved)"),
            _ => self.contacts.name_for(key_hex),
        }
    }

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
        self.selected = None;
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
                    if !self.foreground || self.tab != Tab::Chat {
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
        self.engine.set_ptt(false);
    }

    fn save(&self) {
        self.settings.save(&self.dir);
    }

    fn chain_changed(&mut self) {
        self.engine.set_chain(self.settings.chain.clone());
        self.save();
    }

    pub fn update(&mut self, message: Message) {
        if !matches!(message, Message::PttPressed | Message::PttReleased | Message::Draft(_)) {
            self.notice = None;
        }
        if !matches!(message, Message::ClearCalls) {
            self.clear_calls_armed = false;
        }
        if !matches!(message, Message::RemoveContact(_)) {
            self.remove_armed = None;
        }
        match message {
            Message::Tab(tab) => {
                self.tab = tab;
                if tab == Tab::Chat {
                    self.unread = 0;
                    // During a call, Chat means the person on it.
                    if let Some(peer) = self.live_peer.clone() {
                        self.open_chat(&peer);
                    }
                }
            }
            Message::PortChanged(value) => {
                self.port_text = value.chars().filter(char::is_ascii_digit).take(5).collect();
                if let Ok(port) = self.port_text.parse::<u16>() {
                    self.settings.listen_port = port;
                    self.save();
                }
            }
            Message::RemoteChanged(value) => {
                self.settings.last_remote = value;
                self.save();
            }
            Message::PasteRemote => {
                if let Some(text) = platform::read_clipboard() {
                    self.settings.last_remote = text.trim().to_string();
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
            Message::Call => {
                let target = self.settings.last_remote.trim().to_string();
                if target.is_empty() {
                    self.notice = Some("Enter their address (ip:port) or paste their invite link.".into());
                    return;
                }
                self.start_call(&target);
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
            Message::Copy(value) => {
                platform::copy_to_clipboard(&value);
                self.notice = Some("Copied.".into());
            }
            Message::ShowPhrase(show) => self.phrase_visible = show,
            Message::RestoreChanged(value) => {
                self.restore_text = value;
                self.restore_armed = false;
            }
            Message::PasteRestore => {
                if let Some(text) = platform::read_clipboard() {
                    self.restore_text = text;
                    self.restore_armed = false;
                }
            }
            Message::Restore => self.restore(),
            Message::CancelRestore => {
                self.restore_armed = false;
                self.restore_text.clear();
            }
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
            self.notice = Some("Antiphon needs the microphone for calls. Allow it, then try again.".into());
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

    fn start_call(&mut self, target: &str) {
        if let Some(identity) = self.call_identity() {
            self.dialled = Some(target.to_string());
            self.engine.call(target, identity);
            self.tab = Tab::Call;
        }
    }

    /// Contacts, chat, files, recent calls and settings.
    fn update_messaging(&mut self, message: Message) {
        let live = self.engine.live();
        let live_here = |chat: &Conversation| live.as_ref().filter(|l| hex(&l.peer_key) == chat.peer_hex).cloned();
        match message {
            Message::ContactName(value) => self.contact_name = value,
            Message::ContactInvite(value) => self.contact_invite = value,
            Message::PasteContactInvite => {
                if let Some(text) = platform::read_clipboard() {
                    self.contact_invite = text.trim().to_string();
                }
            }
            Message::AddContact => {
                match self.contacts.add_from_invite(&self.contact_name, &self.contact_invite, now_secs()) {
                    Ok(line) => {
                        self.contacts.save(&self.dir);
                        self.contact_name.clear();
                        self.contact_invite.clear();
                        self.notice = Some(line);
                    }
                    Err(e) => self.notice = Some(e),
                }
            }
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
            Message::RemoveContact(key) => {
                // Two presses, like restoring an identity: a contact is a
                // pinned key, and removing it means the next call from that
                // key is a stranger's.
                if self.remove_armed.as_deref() == Some(&key) {
                    self.contacts.remove(&key);
                    self.contacts.save(&self.dir);
                    self.remove_armed = None;
                } else {
                    self.remove_armed = Some(key);
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

            Message::OpenChat(key) => {
                self.open_chat(&key);
                self.tab = Tab::Chat;
            }
            Message::CloseChat => {
                self.chat = None;
                self.editing = None;
                self.selected = None;
            }
            Message::Draft(value) => self.draft = value,
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
            Message::SelectChat(id) => self.selected = id,
            Message::EditChat(id) => {
                if let Some(message) = self.chat.as_ref().and_then(|c| c.messages.iter().find(|m| m.id == id && m.from_me)) {
                    self.draft = message.text.clone();
                    self.editing = Some(id);
                    self.selected = None;
                }
            }
            Message::CancelEdit => {
                self.editing = None;
                self.draft.clear();
            }
            Message::DeleteChat(id) => {
                if let Some(chat) = self.chat.as_mut() {
                    let here = live_here(chat);
                    chat.delete(id, here.as_deref());
                }
                self.selected = None;
            }
            Message::React(id, emoji) => {
                if let Some(chat) = self.chat.as_mut() {
                    if let Some(live) = live_here(chat) {
                        chat.react(id, emoji, &live);
                    }
                }
                self.selected = None;
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

    /// `top` and `bottom` are the logical pixels the system draws over.
    pub fn view(&self, top: f32, bottom: f32) -> Element<'_, Message, Theme, Renderer> {
        // Eight tabs in two rows of four: one row of eight is too narrow to
        // tap on a phone held upright, and a menu hides where things are.
        let tab_button = |label: String, tab: Tab| {
            button(text(label).size(14).width(Length::Fill).align_x(Alignment::Center))
                .width(Length::Fill)
                .padding([10, 2])
                .style(if self.tab == tab { button::primary } else { button::secondary })
                .on_press(Message::Tab(tab))
        };
        let chat_label = if self.unread > 0 { format!("Chat ({})", self.unread) } else { "Chat".to_string() };
        let offer_waiting = self.engine.live().is_some_and(|l| l.files.lock().unwrap().pending_offer.is_some());
        let files_label = if offer_waiting { "Files (!)".to_string() } else { "Files".to_string() };
        let tabs = column![
            row![
                tab_button("Call".into(), Tab::Call),
                tab_button(chat_label, Tab::Chat),
                tab_button(files_label, Tab::Files),
                tab_button("Contacts".into(), Tab::Contacts),
            ]
            .spacing(6),
            row![
                tab_button("Recent".into(), Tab::Recent),
                tab_button("Voice".into(), Tab::Voice),
                tab_button("Settings".into(), Tab::Settings),
                tab_button("Identity".into(), Tab::Identity),
            ]
            .spacing(6),
        ]
        .spacing(6);

        let body = match self.tab {
            Tab::Call => self.call_view(),
            Tab::Chat => self.chat_view(),
            Tab::Files => self.files_view(),
            Tab::Contacts => self.contacts_view(),
            Tab::Recent => self.recent_view(),
            Tab::Voice => self.voice_view(),
            Tab::Settings => self.settings_view(),
            Tab::Identity => self.identity_view(),
        };

        let mut page = column![tabs].spacing(16);
        if let Some(notice) = &self.notice {
            page = page.push(container(text(notice.as_str())).padding(10).width(Length::Fill).style(container::rounded_box));
        }
        // An open conversation scrolls its messages itself, so its input
        // stays put at the bottom instead of scrolling away with them.
        if self.tab == Tab::Chat && self.chat.is_some() {
            page = page.push(container(body).height(Length::Fill));
        } else {
            page = page.push(scrollable(container(body).padding(Padding { right: 12.0, ..Padding::ZERO })).height(Length::Fill));
        }

        container(page)
            .padding(Padding { top: top + 12.0, bottom: bottom + 12.0, left: 16.0, right: 16.0 })
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn mic_banner(&self) -> Option<Element<'_, Message, Theme, Renderer>> {
        (!self.mic_permission).then(|| {
            container(
                column![
                    text("Microphone access is off. Calls and the mic test need it."),
                    button("Allow microphone").on_press(Message::RequestMic),
                ]
                .spacing(8),
            )
            .padding(12)
            .width(Length::Fill)
            .style(container::rounded_box)
            .into()
        })
    }

    fn call_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let mut content = column![].spacing(14);
        if let Some(banner) = self.mic_banner() {
            content = content.push(banner);
        }

        match self.engine.phase() {
            Phase::Connected { remote, code, peer_key } => {
                return self.connected_view(remote, code, peer_key);
            }
            Phase::Listening { local, public } => {
                content = content.push(text("Waiting for a call").size(24));
                content = content.push(text(format!("On your network: {local}")));
                match &public {
                    Some(public) => content = content.push(text(format!("From the internet: {public}"))),
                    None => content = content.push(text("Looking up your internet address...")),
                }
                if let Ok(identity) = &self.identity {
                    let address = public.unwrap_or(local);
                    let link = Invite::new(identity.public().identity_key, address).to_link();
                    content = content.push(text("Send them this invite. It carries your address and who you are, so nobody else can answer in your place."));
                    content = content.push(text(link.clone()).size(13).wrapping(Wrapping::Glyph));
                    content = content.push(button("Copy invite").on_press(Message::Copy(link)));
                }
                content = content.push(button("Stop waiting").style(button::danger).on_press(Message::Stop));
                return content.into();
            }
            Phase::Dialing { remote } => {
                content = content.push(text("Calling...").size(24));
                content = content.push(text(remote).wrapping(Wrapping::Glyph));
                content = content.push(button("Cancel").style(button::danger).on_press(Message::Stop));
                return content.into();
            }
            Phase::Ended { message } => {
                content = content.push(container(text(message)).padding(10).width(Length::Fill).style(container::rounded_box));
            }
            Phase::Idle => {}
        }

        content = content.push(text("Call someone").size(22));
        content = content.push(text("Their address as ip:port, or the invite link they sent you."));
        content = content.push(
            text_input("192.168.1.20:5000 or antiphon://...", &self.settings.last_remote)
                .on_input(Message::RemoteChanged)
                .on_submit(Message::Call)
                .padding(10),
        );
        content = content.push(
            row![
                button("Paste").style(button::secondary).on_press(Message::PasteRemote),
                button("Call").style(button::success).on_press(Message::Call),
            ]
            .spacing(10),
        );

        content = content.push(space().height(8));
        content = content.push(text("Wait for a call").size(22));
        content = content.push(text("Opens this port and shows an invite to send. Over the internet, your router may need to forward the port to this phone."));
        content = content.push(
            row![
                text_input("5000", &self.port_text)
                    .on_input(Message::PortChanged)
                    .on_submit(Message::Listen)
                    .padding(10)
                    .width(Length::Fixed(110.0)),
                button("Wait for a call").on_press(Message::Listen),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        if let Some(ip) = &self.local_ip {
            content = content.push(text(format!("This phone on your network: {ip}")).size(14));
        }
        content.into()
    }

    fn connected_view(&self, remote: String, code: String, peer_key: [u8; 32]) -> Element<'_, Message, Theme, Renderer> {
        let transmitting = self.engine.transmitting();
        let key_hex = hex(&peer_key);
        let mut content = column![
            text(format!("In a call with {}", self.display_name_for(&key_hex))).size(24),
            text(remote).size(14),
            text("Verification code").size(14),
            text(code).size(30),
            text("Read it to each other out loud. If it differs, someone is in the middle of this call: hang up."),
        ]
        .spacing(10);
        content = content.push(self.caller_card(&key_hex));
        content = content.push(
            row![
                button(text(if self.unread > 0 { format!("Chat ({})", self.unread) } else { "Chat".into() }))
                    .style(button::secondary)
                    .on_press(Message::Tab(Tab::Chat)),
                button("Send a file").style(button::secondary).on_press(Message::PickFile),
                button("Files").style(button::secondary).on_press(Message::Tab(Tab::Files)),
            ]
            .spacing(10),
        );
        content = content.push(progress_bar(0.0..=1.0, self.engine.level().min(1.0)).girth(10));

        if self.settings.open_mic {
            content = content.push(text("Open mic: you are always transmitting."));
        } else {
            let label = if transmitting { "Talking..." } else { "Hold to talk" };
            let pad = container(text(label).size(26))
                .center(Length::Fill)
                .height(Length::Fixed(160.0))
                .style(if transmitting { container::success } else { container::secondary });
            content = content.push(mouse_area(pad).on_press(Message::PttPressed).on_release(Message::PttReleased));
        }

        content = content
            .push(toggler(self.settings.open_mic).label("Open mic").on_toggle(Message::OpenMic))
            .push(toggler(self.settings.speaker).label("Speaker").on_toggle(Message::Speaker))
            .push(
                button(text("Hang up").width(Length::Fill).align_x(Alignment::Center))
                    .width(Length::Fill)
                    .padding(14)
                    .style(button::danger)
                    .on_press(Message::Stop),
            );
        content.into()
    }

    fn voice_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let chain = &self.settings.chain;
        let mut content = column![
            text("Your voice").size(22),
            text("Every stage runs on this phone before anything is sent. Changes apply straight away, even mid-call."),
        ]
        .spacing(12);
        if let Some(banner) = self.mic_banner() {
            content = content.push(banner);
        }

        let testing = self.engine.mic_test_running();
        content = content.push(toggler(testing).label("Hear yourself (use headphones)").on_toggle(Message::MicTest));
        if testing {
            content = content.push(progress_bar(0.0..=1.0, self.engine.level().min(1.0)).girth(10));
        }

        let mut presets: Vec<String> = vec!["Flat".to_string()];
        presets.extend(eq_preset_names().iter().map(|s| s.to_string()));
        content = content.push(
            row![
                text("EQ"),
                pick_list(presets, self.eq_choice.clone(), Message::EqPreset).placeholder("Choose a starting EQ"),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );

        let stage = |label: &'static str, help: &'static str, on: bool, which: Stage| {
            column![toggler(on).label(label).on_toggle(move |v| Message::Toggle(which, v)), text(help).size(13)].spacing(4)
        };
        let knob = |label: String, range: std::ops::RangeInclusive<f32>, value: f32, step: f32, which: Param| {
            column![text(label).size(14), slider(range, value, move |v| Message::Param(which, v)).step(step)].spacing(4)
        };

        let mut suppress = stage(
            "Noise suppression",
            "Removes steady background sound -- fans, hiss, air conditioning -- even under your voice.",
            chain.noise_suppress.enabled,
            Stage::NoiseSuppress,
        );
        if chain.noise_suppress.enabled {
            suppress = suppress.push(knob(
                format!("Strength {:.0}%", chain.noise_suppress.strength * 100.0),
                0.0..=1.0,
                chain.noise_suppress.strength,
                0.05,
                Param::SuppressStrength,
            ));
            if chain.noise_suppress.strength > THIN_VOICE_WARNING_STRENGTH {
                suppress = suppress.push(text("This strong, it starts removing parts of your voice too: speech and background share frequencies.").size(13));
            }
        }
        content = content.push(suppress);

        let mut gate = stage("Noise gate", "Mutes the mic between words, so the room is not heard while you are quiet.", chain.noise_gate.enabled, Stage::NoiseGate);
        if chain.noise_gate.enabled {
            gate = gate.push(knob(format!("Threshold {:.0} dB", chain.noise_gate.threshold_db), -80.0..=-10.0, chain.noise_gate.threshold_db, 1.0, Param::GateThreshold));
        }
        content = content.push(gate);

        let mut high = stage("High-pass filter", "Cuts rumble and handling noise below your voice.", chain.high_pass.enabled, Stage::HighPass);
        if chain.high_pass.enabled {
            high = high.push(knob(format!("Cutoff {:.0} Hz", chain.high_pass.cutoff_hz), 20.0..=300.0, chain.high_pass.cutoff_hz, 5.0, Param::HighPassCutoff));
        }
        content = content.push(high);

        let mut low = stage("Low-pass filter", "Cuts hiss above your voice.", chain.low_pass.enabled, Stage::LowPass);
        if chain.low_pass.enabled {
            low = low.push(knob(format!("Cutoff {:.0} Hz", chain.low_pass.cutoff_hz), 3000.0..=20000.0, chain.low_pass.cutoff_hz, 250.0, Param::LowPassCutoff));
        }
        content = content.push(low);

        content = content.push(stage("De-esser", "Softens harsh s and sh sounds.", chain.de_esser.enabled, Stage::DeEsser));

        let mut comp = stage("Compressor", "Evens out loud and quiet moments so you stay at one level.", chain.compressor.enabled, Stage::Compressor);
        if chain.compressor.enabled {
            comp = comp
                .push(knob(format!("Threshold {:.0} dB", chain.compressor.threshold_db), -50.0..=0.0, chain.compressor.threshold_db, 1.0, Param::CompressorThreshold))
                .push(knob(format!("Ratio {:.1}:1", chain.compressor.ratio), 1.0..=10.0, chain.compressor.ratio, 0.5, Param::CompressorRatio));
        }
        content = content.push(comp);

        content = content.push(stage("Limiter", "A hard ceiling, so a shout or a bump never clips.", chain.limiter.enabled, Stage::Limiter));

        let gain_on = match chain.gain {
            GainConfig::Fixed { enabled, .. } | GainConfig::Auto { enabled, .. } => enabled,
        };
        content = content.push(stage("Auto gain", "Turns a quiet mic up and a loud one down, slowly.", gain_on, Stage::AutoGain));

        let mut pitch = stage("Pitch shift", "Raises or lowers your voice, keeping it sounding like speech.", chain.pitch_shift.enabled, Stage::PitchShift);
        if chain.pitch_shift.enabled {
            pitch = pitch.push(knob(format!("{:+.0} semitones", chain.pitch_shift.semitones), -12.0..=12.0, chain.pitch_shift.semitones, 1.0, Param::PitchSemitones));
        }
        content = content.push(pitch);

        let mut reverb = stage("Reverb", "Adds a room around your voice.", chain.reverb.enabled, Stage::Reverb);
        if chain.reverb.enabled {
            reverb = reverb
                .push(knob(format!("Room {:.0}%", chain.reverb.room_size * 100.0), 0.0..=1.0, chain.reverb.room_size, 0.05, Param::ReverbRoom))
                .push(knob(format!("Mix {:.0}%", chain.reverb.wet_dry * 100.0), 0.0..=1.0, chain.reverb.wet_dry, 0.05, Param::ReverbMix));
        }
        content = content.push(reverb);

        content = content.push(button("Reset to defaults").style(button::secondary).on_press(Message::ResetChain));
        content.into()
    }

    fn identity_view(&self) -> Element<'_, Message, Theme, Renderer> {
        let mut content = column![
            text("Your identity").size(22),
            text("A key made on this phone. There is no account and no server: this key is who you are to your contacts."),
        ]
        .spacing(12);

        match &self.identity {
            Ok(identity) => {
                let key = hex(&identity.public().identity_key);
                let link = Invite::new(identity.public().identity_key, String::new()).to_link();
                content = content
                    .push(text(key.clone()).size(14).wrapping(Wrapping::Glyph))
                    .push(
                        row![
                            button("Copy key").style(button::secondary).on_press(Message::Copy(key)),
                            button("Copy invite").style(button::secondary).on_press(Message::Copy(link)),
                        ]
                        .spacing(10),
                    );

                content = content.push(space().height(8)).push(text("Recovery phrase").size(20));
                if self.phrase_visible {
                    let phrase = antiphon_crypto::mnemonic::phrase_from_seed(&identity.master_seed());
                    content = content
                        .push(text("Write these 24 words down and keep them somewhere safe. Anyone who reads them becomes you, and nobody can reset your identity if you lose them."))
                        .push(container(text(phrase).size(15)).padding(12).width(Length::Fill).style(container::rounded_box))
                        .push(button("Hide").style(button::secondary).on_press(Message::ShowPhrase(false)));
                } else {
                    content = content
                        .push(text("24 words that rebuild this identity on a new phone."))
                        .push(button("Show recovery phrase").on_press(Message::ShowPhrase(true)));
                }
            }
            Err(e) => {
                content = content.push(text(e.as_str()));
            }
        }

        content = content
            .push(space().height(8))
            .push(text("Restore from a phrase").size(20))
            .push(text("Replaces this phone's identity with the one the phrase belongs to."))
            .push(
                text_input("24 words, separated by spaces", &self.restore_text)
                    .on_input(Message::RestoreChanged)
                    .padding(10),
            );
        if self.restore_armed {
            content = content
                .push(text("This phone already has a different identity. Every contact who knows it will see a key-change warning. Replace it?"))
                .push(
                    row![
                        button("Yes, replace it").style(button::danger).on_press(Message::Restore),
                        button("Cancel").style(button::secondary).on_press(Message::CancelRestore),
                    ]
                    .spacing(10),
                );
        } else {
            content = content.push(
                row![
                    button("Paste").style(button::secondary).on_press(Message::PasteRestore),
                    button("Restore").on_press(Message::Restore),
                ]
                .spacing(10),
            );
        }
        content.into()
    }
}
