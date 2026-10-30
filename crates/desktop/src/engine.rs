//! The desktop's call engine: the shared client crates, driven for the
//! Stoop interface.
//!
//! Everything slow (connecting, STUN, the router, a key capture) runs on the
//! tokio runtime and reports back through an [`Event`] queue that the app
//! drains on its tick, so the window never waits on the network. The engine
//! turns its state into a [`ui::Model`] on demand and carries out the
//! [`ui::Action`]s the interface hands it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_audio::{ChainConfig, GainConfig};
use antiphon_client::control::{ControlMessage, Hello, ProfileCard};
use antiphon_client::device_sync::NoteOp;
use antiphon_client::transfer_queue::{self, Entry, TransferQueue};
use antiphon_client::voice_message::{self, VoiceLink};
use antiphon_client::state::{self, AppState, TransmitSettings, MODE_OPEN_MIC, MODE_PTT, MODE_VOICE_ACTIVATION};
use antiphon_client::{
    caps_lock, community, config, connection_mode, delivery, discovery, node_picker, fallback_relay, file_integrity, file_preview, file_transfer, history, key_capture,
    group, moderation, notify, outbox, presence, profile, push_wake, reachability, reactions, sharing, video_capture, vpn,
};
use antiphon_crypto::BackupKey;
use antiphon_ui::{self as ui, ToastKind};
use antiphon_video::Stream;
use device_query::Keycode;
use iced::widget::qr_code;

/// What the voice preset list calls "no EQ".
const NATURAL: &str = "Natural";
/// How long an ended call's reason stays on screen.
const ENDED_FOR: Duration = Duration::from_secs(3);

/// Something a background task finished.
enum Event {
    /// A line about how a call attempt is going ("Looking them up...").
    Status(String),
    /// The call is up and the audio is running.
    Connected {
        /// True when they called us.
        incoming: bool,
        /// Their contact id (public key, hex).
        peer: String,
        /// Their name, as saved or as they gave it.
        name: String,
        /// The words both sides read aloud to be sure.
        code: String,
        /// How the audio travels: direct, through the router, or relayed.
        mode: reachability::Mode,
        /// A trust warning to show, such as a key that changed.
        warning: Option<String>,
        /// Whether they are a saved contact.
        known: bool,
    },
    /// The call could not be made, or the audio would not start.
    Failed(String),
    /// Listening on the door's port failed.
    ListenFailed(String),
    /// The STUN check finished: our public address, or why not.
    Public(Result<String, String>),
    /// The relay check finished: the relay to use, if any answered, and the
    /// relays it said it peers with.
    RelayChosen(Option<fallback_relay::Choice>, Vec<String>),
    /// The router port mapping finished: the outside port, or why not.
    PortMap(Result<u16, String>),
    /// A push-to-talk key capture finished: the key's name, or why not.
    Key(Result<String, String>),
    /// A note to show the person.
    Toast(String, ToastKind),
    /// A picked profile picture, made small: the base64 to save, or why
    /// the file couldn't be used.
    Avatar(Result<String, String>),
    /// Files picked to send to this contact (id), for the queue.
    Queue(Vec<PathBuf>, String),
    /// The plugin search finished (spec §1 "Plugin Host").
    PluginsFound(Vec<antiphon_audio::plugins::PluginInfo>),
    /// A voice message was left (or why it couldn't be).
    VoiceSent(Result<(), String>),
    /// A picture picked to be a community's emoji: (community, file name,
    /// the picture made small), or why it couldn't be used.
    EmojiPicked(Result<(String, String, String), String>),
    /// A theme file was chosen and read: use what it says.
    Theme(ui::theme_file::ThemeFile),
    /// The directories were read: what they list, what went wrong, and
    /// the keys pinned for them.
    Directory(Vec<discovery::Listed>, Vec<String>, Vec<String>),
    /// The look for mailbox nodes on this network finished: their addresses.
    NodesFound(Vec<String>),
    /// A node read from a QR code picture, or why none was.
    NodePicked(Result<node_picker::NodeLink, String>),
}

/// What the app should do in the interface after the engine acted.
pub enum Out {
    /// Show a toast.
    Toast(String, ToastKind),
    /// Open this screen (an incoming call from a contact).
    Show(ui::Route),
    /// A contact was added: close the dialog and open them.
    Added(String),
    /// Adding a contact failed: keep the dialog open and say why.
    AddFailed(String),
    /// Put this text on the clipboard.
    Copy(String),
    /// A restore rewrote the account on disk: restart to load it.
    Relaunch,
    /// A group chat or community was made or joined: close the dialog and
    /// open it.
    SpaceCreated(String),
}

/// Whether the background task is placing a call or waiting for one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskKind {
    /// Waiting for someone to call in on the door's port.
    Listen,
    /// Placing a call out.
    Dial,
}

/// The call as the interface sees it, kept between ticks.
#[derive(Default)]
struct CallInfo {
    /// Dialing, connected, ended, or nothing.
    phase: ui::Phase,
    /// The other side's contact id, once known.
    peer: Option<String>,
    /// The other side's name.
    name: String,
    /// The verification words.
    code: String,
    /// Short name of the route ("Direct", "Relayed").
    mode: String,
    /// A sentence explaining the route.
    mode_detail: String,
    /// A trust warning from connecting, if any.
    warning: Option<String>,
    /// Progress while dialing, or why the call ended.
    status: Option<String>,
    /// When the call ended, to clear the reason after [`ENDED_FOR`].
    ended_at: Option<Instant>,
}

/// The rate voice messages are decoded and played at.
const CLIP_RATE: u32 = 48_000;

/// A voice message being recorded.
struct Recording {
    /// The contact it is for.
    to: String,
    /// Raised to stop recording.
    stop: Arc<AtomicBool>,
    /// Raised (before `stop`) to send what was recorded rather than drop it.
    send: Arc<AtomicBool>,
    /// When it started, for the counter and the length cap.
    started: Instant,
}

/// A voice message's clip, decoded: one I sent from the copy kept when it
/// was made, or one I received once its fetch has finished.
fn load_clip(link: &VoiceLink, mine: bool) -> Option<Vec<f32>> {
    if mine {
        let path = config::config_dir()?.join("voice-messages").join(&link.name);
        antiphon_net::voice::decode_clip(&std::fs::read(path).ok()?, CLIP_RATE)
    } else {
        voice_message::load_fetched(link, CLIP_RATE)
    }
}

/// Where the send queue is kept between runs, in the config directory.
const QUEUE_FILE: &str = "send-queue.json";

/// A file waiting in the send queue. Saved with the queue, so a restart
/// still sends it.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct QueuedSend {
    /// Where it is.
    path: PathBuf,
    /// Its name, as offered.
    name: String,
    /// Its size when picked, for the queue's line.
    size: u64,
    /// The contact it is for (id): it goes on a call with them.
    peer: String,
}

/// The send queue saved in `dir` by a previous run. One that can't be
/// read starts empty, with a line in the log: losing the list of what was
/// waiting is better than not starting.
fn load_queue(dir: &Path) -> Vec<(QueuedSend, bool)> {
    transfer_queue::load(&dir.join(QUEUE_FILE)).unwrap_or_else(|e| {
        eprintln!("[queue] the saved send queue could not be read, so it starts empty: {e}");
        Vec::new()
    })
}

/// The open conversation, built once per change rather than per frame.
struct ConvCache {
    /// Whose conversation it is.
    person: String,
    /// Whether it was built from the live call's chat (vs. saved history).
    live: bool,
    /// The live chat's revision counter when built; a change means rebuild.
    rev: u32,
    /// The built lines.
    lines: Vec<ui::Line>,
}

/// The call engine: owns the client state, the saved config, and whatever
/// background task is listening or dialing.
pub struct Engine {
    /// Shared state the audio pipeline and network tasks also hold.
    state: Arc<AppState>,
    /// The local API for scripts and bots, while it is on.
    api: Option<crate::api::Api>,
    /// Clients that asked for events.
    api_listeners: Vec<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
    /// The last message seen per contact, and the call's phase, so events
    /// go out once per change.
    api_seen: HashMap<String, (String, u64)>,
    api_phase: String,
    /// Plugins found on this computer, once looked for.
    plugins_found: Option<Vec<antiphon_audio::plugins::PluginInfo>>,
    /// The plugin search is running.
    plugins_scanning: bool,
    /// Results from background tasks, drained on each tick.
    events: Arc<Mutex<Vec<Event>>>,
    /// What the app should do in the interface; the app takes these.
    pub out: Vec<Out>,
    /// Saved settings and contacts.
    cfg: config::AppConfig,
    /// The background task listening or dialing, and which it is.
    task: Option<(TaskKind, tokio::task::JoinHandle<()>)>,
    /// The call as shown.
    call: CallInfo,
    /// Whether we are taking calls (listening on the port).
    door: bool,
    /// Our public address from STUN, once known.
    public: Option<String>,
    /// The relay in use when it is not simply ours: a fallback, one from
    /// the mesh, or a contact's lent relay. `None` until a check finds one.
    relay_choice: Option<fallback_relay::Choice>,
    /// What the checks found about being reachable.
    reach: reachability::Reachability,
    /// Our invite link.
    invite: String,
    /// The invite as a QR code, built once.
    qr: Option<Arc<qr_code::Data>>,
    /// The config folder, if there is one.
    dir: Option<PathBuf>,
    /// The key that seals saved history and backups.
    key: Option<BackupKey>,
    /// The open conversation, cached.
    conv: Option<ConvCache>,
    /// Last message per contact: (text, unix seconds).
    previews: HashMap<String, (String, u64)>,
    /// Unread message count per contact.
    unread: HashMap<String, u32>,
    /// How many chat lines the call had at the last look, to count new ones.
    seen_lines: usize,
    /// Microphone names, listed at start.
    inputs: Vec<String>,
    /// Speaker names, listed at start.
    outputs: Vec<String>,
    /// True while waiting for a key press to set push-to-talk.
    ptt_capturing: bool,
    /// The last delivery round handled; a new number means messages moved.
    delivery_seen: u64,
    /// The last LAN discovery count handled; a new number means new peers.
    discovery_seen: u64,
    /// The relay the peer last told us about, saved to their contact.
    peer_relay_saved: String,
    /// The VPN routes the peer last told us about, saved to their contact.
    peer_routes_saved: String,
    /// Mailbox nodes found on this network at the last look.
    found_nodes: Vec<String>,
    /// A look for mailbox nodes on this network is under way.
    finding_nodes: bool,
    /// The peer's meter level, decaying between frames.
    their_level: f32,
    /// True while a STUN check is running.
    checking: bool,
    /// The call's pictures as last drawn, by tile key, with the version
    /// each was built from: a tile is rebuilt (and its picture uploaded
    /// again) only when a newer frame has come.
    tiles: HashMap<String, (u64, ui::VideoTile)>,
    /// Group chats and communities, as the screens last saw them.
    spaces: crate::spaces::Spaces,
    /// The channel on screen, as (space id, channel): its messages are
    /// loaded and it counts as read. Set by the app before each model.
    viewing: Option<(String, u32)>,
    /// Files waiting to be sent, one at a time, each to its contact.
    send_queue: TransferQueue<QueuedSend>,
    /// The queued file being read or sent now, and a flag raised once
    /// that is over (sent, failed, declined or cancelled). Saved at the
    /// front of the queue until then, so one cut short by the app closing
    /// goes first next time.
    sending: Option<(Entry<QueuedSend>, Arc<AtomicBool>)>,
    /// The queue as last written to [`QUEUE_FILE`], so it is written only
    /// when something changed.
    queue_saved: Vec<(QueuedSend, bool)>,
    /// A voice message being recorded.
    recording: Option<Recording>,
    /// Voice messages seen in conversations, by (contact, message id):
    /// the link to the clip, and whether it is one I sent.
    voice_links: HashMap<(String, u64), (VoiceLink, bool)>,
    /// Received clips whose fetch has been started, by file id.
    voice_fetched: HashSet<String>,
    /// A clip asked to play before it had arrived: (contact, message id),
    /// and when it was last looked for. It plays once it is here.
    voice_waiting: Option<(String, u64, Instant)>,
    /// The clip playing: (contact, message id), the stream to keep
    /// alive, and when it ends.
    playing: Option<(String, u64, antiphon_audio::PlaybackHandle, Instant)>,
    /// Settings > Account & backup: phrase, backup files, moving devices.
    account: crate::account::AccountState,
    /// This computer as one of the account's devices, and notes to self.
    /// `None` without a data folder or history key to keep them in.
    devices: Option<crate::my_devices::MyDevices>,
    /// The voice channel we are in, if any.
    voice: Option<VoiceRoom>,
    /// What the community directories listed at the last look.
    directory: ui::Directory,
}

/// Being in a community's voice channel.
struct VoiceRoom {
    /// The community's id.
    community: String,
    /// The voice channel.
    channel: u32,
    /// When we last said we are here.
    said_at: Instant,
    /// Whom we last tried to put through, by main key (hex), and when.
    dialed: HashMap<String, Instant>,
}

/// How long before trying again to put someone through to a voice channel.
const VOICE_REDIAL: Duration = Duration::from_secs(45);

impl Engine {
    /// Builds the engine around a started [`AppState`].
    pub fn new(state: Arc<AppState>) -> Self {
        let cfg = config::load();
        let dir = config::config_dir();
        let key = dir.as_ref().and_then(|d| history::load_or_create_backup_key(d).ok());
        let saved_queue = dir.as_ref().map(|d| load_queue(d)).unwrap_or_default();
        let mut engine = Engine {
            state,
            events: Arc::new(Mutex::new(Vec::new())),
            out: Vec::new(),
            door: false,
            cfg,
            task: None,
            call: CallInfo::default(),
            public: None,
            relay_choice: None,
            reach: reachability::Reachability::default(),
            invite: String::new(),
            qr: None,
            dir,
            key,
            conv: None,
            previews: HashMap::new(),
            unread: HashMap::new(),
            seen_lines: 0,
            inputs: antiphon_audio::list_input_devices(),
            outputs: antiphon_audio::list_output_devices(),
            ptt_capturing: false,
            plugins_found: None,
            plugins_scanning: false,
            api: None,
            api_listeners: Vec::new(),
            api_seen: HashMap::new(),
            api_phase: String::new(),
            delivery_seen: 0,
            discovery_seen: 0,
            peer_relay_saved: String::new(),
            peer_routes_saved: String::new(),
            found_nodes: Vec::new(),
            finding_nodes: false,
            their_level: 0.0,
            checking: false,
            tiles: HashMap::new(),
            spaces: Default::default(),
            viewing: None,
            send_queue: TransferQueue::restore(saved_queue.iter().cloned()),
            sending: None,
            queue_saved: saved_queue,
            recording: None,
            voice_links: HashMap::new(),
            voice_fetched: HashSet::new(),
            voice_waiting: None,
            playing: None,
            account: Default::default(),
            devices: None,
            voice: None,
            directory: ui::Directory::default(),
        };
        if let (Some(dir), Some(key)) = (&engine.dir, &engine.key) {
            match crate::my_devices::MyDevices::start(&engine.state.runtime, dir, engine.state.identity.clone(), key.clone()) {
                Ok(d) => engine.devices = Some(d),
                Err(e) => eprintln!("[devices] {e}"),
            }
        }
        engine.load_previews();
        engine.api_seen = engine.previews.clone();
        engine.set_api(engine.cfg.local_api || crate::has_flag("--api"));
        engine.refresh_invite();
        if engine.cfg.stoop.door_on_start {
            engine.set_door(true);
        }
        engine.check_network();
        engine
    }

    /// Queues a background result for the next tick.
    fn push(events: &Arc<Mutex<Vec<Event>>>, e: Event) {
        events.lock().unwrap().push(e);
    }

    /// Asks the app to show a toast.
    fn toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.out.push(Out::Toast(text.into(), kind));
    }

    /// Writes the config to disk.
    fn save(&self) {
        config::save(&self.cfg);
    }

    /// Whether a call is up or being placed, so the app ticks faster.
    pub fn busy(&self) -> bool {
        self.call.phase != ui::Phase::Idle
            || self.state.loopback.lock().unwrap().is_some()
            || self.ptt_capturing
            || self.recording.is_some()
            || self.playing.is_some()
            || self.voice_waiting.is_some()
    }

    // ---------------------------------------------------------------- tick

    /// Catches up with everything that happened since the last tick.
    /// `open` is the conversation on screen, which counts as read.
    pub fn tick(&mut self, open: Option<&str>) {
        let outcomes = self.account.tick();
        self.account_outcomes(outcomes);
        // Another of your devices changed the notes or the device list, or
        // removed this one.
        if let Some(changes) = self.devices.as_mut().map(|d| d.poll()) {
            if changes.removed {
                self.devices = None;
                self.toast("This computer was removed from your account on another device. Signing out…", ToastKind::Warn);
                self.out.push(Out::Relaunch);
                return;
            }
            if changes.changed {
                self.load_previews();
                if self.conv.as_ref().is_some_and(|c| !c.live) {
                    self.conv = None;
                }
            }
        }
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        for e in events {
            self.on_event(e);
        }
        self.serve_api(open);

        // The far end hung up, or the connection died under us.
        let ended = self.state.call.lock().unwrap().as_ref().is_some_and(|a| !a.is_live());
        if ended {
            let name = self.call.name.clone();
            self.teardown(format!("{name} left the call"));
        }
        if let (ui::Phase::Ended(_), Some(at)) = (&self.call.phase, self.call.ended_at) {
            if at.elapsed() >= ENDED_FOR {
                self.call = CallInfo::default();
            }
        }

        // An open door with nothing on the line goes back to listening.
        if (self.door || self.voice.is_some()) && self.task.as_ref().is_none_or(|(_, t)| t.is_finished()) && self.state.call.lock().unwrap().is_none() && self.call.phase != ui::Phase::Dialing {
            self.task = None;
            self.listen();
        }

        // Spec §8: the courier ran; mail may have come in or gone out.
        let round = delivery::rounds();
        if round != self.delivery_seen {
            self.delivery_seen = round;
            for notice in delivery::take_notices() {
                self.toast(notice, ToastKind::Info);
            }
            self.cfg = config::load();
            self.mail_arrived(open);
        }
        discovery::set_local(discovery::Local {
            listen_port: if self.door { self.cfg.stoop.port } else { 0 },
            mapped_port: self.state.port_map.lock().unwrap().as_ref().map(state::PortMapHandle::current_external_port),
            stun_public: self.public.clone(),
            relay: self.my_relay(),
        });
        let found = discovery::revision();
        if found != self.discovery_seen {
            self.discovery_seen = found;
            self.cfg = config::load();
        }

        // Keyboard shortcuts raised by the key poller thread.
        let t = &self.state.transmit;
        let (mute, deafen, hangup) =
            (t.pending_mute.swap(false, Ordering::Relaxed), t.pending_deafen.swap(false, Ordering::Relaxed), t.pending_hangup.swap(false, Ordering::Relaxed));
        if mute {
            let now = !self.flag(|a| a.muted.load(Ordering::Relaxed));
            self.act(ui::Action::Mute(now), open);
        }
        if deafen {
            let now = !self.flag(|a| a.deafened.load(Ordering::Relaxed));
            self.act(ui::Action::Deafen(now), open);
        }
        if hangup {
            self.act(ui::Action::Hangup, open);
        }

        // A camera unplugged, or sharing stopped from the system's own
        // controls: say so, and the button goes back to off by itself.
        for (_, why) in self.with_call(|a| a.video.take_failures()).unwrap_or_default() {
            self.toast(why, ToastKind::Warn);
        }

        self.watch_call(open);
        self.group_upkeep();
        self.voice_room_upkeep();
        self.pump_queue();
        self.voice_upkeep();
    }

    // ---------------------------------------------------------- group calls

    /// What acting on group-call messages needs (spec §3), for the call
    /// that is running.
    fn group_context(&self, active: &state::ActiveCall) -> group::Context {
        let relay = self.my_relay();
        group::Context {
            group: active.group.clone(),
            primary: active.peer.clone(),
            identity: self.state.identity.clone(),
            hello: self.hello(),
            relay: (!relay.is_empty()).then_some(relay),
            is_blocked: Arc::new(|key: &[u8; 32]| moderation::is_blocked(&config::load().blocked, &connection_mode::encode_hex32(key))),
        }
    }

    /// Spec §3 group calls, on every tick of a call: act on introductions,
    /// keep the MLS keys current, and say who left.
    fn group_upkeep(&mut self) {
        let (ctx, notices) = {
            let call = self.state.call.lock().unwrap();
            let Some(a) = call.as_ref() else { return };
            let busy = !a.group.inbox.lock().unwrap().is_empty() || ((a.group.people() >= 3 || a.group.keys.in_group()) && a.group.keys.poll_due());
            let mut notices: Vec<String> = a.group.remove_departed().into_iter().map(|(_, name)| format!("{name} left the call")).collect();
            let notice = std::mem::take(&mut *a.group.notice.lock().unwrap());
            if !notice.is_empty() {
                notices.push(notice);
            }
            (busy.then(|| self.group_context(a)), notices)
        };
        if let Some(ctx) = ctx {
            ctx.poll(&self.state.runtime);
        }
        for n in notices {
            self.toast(n, ToastKind::Info);
        }
    }

    /// Brings `contact` into the running call, reaching them the way a
    /// call to them on its own would; everyone else in it is then
    /// introduced to them (or the call moves to the relay's SFU past five).
    fn add_to_call(&mut self, contact: config::Contact) {
        let ctx = {
            let call = self.state.call.lock().unwrap();
            let Some(a) = call.as_ref() else { return };
            let has_relay = !self.my_relay().is_empty();
            if a.group.is_full() && (a.group.sfu_call().is_some() || !has_relay) {
                let most = if a.group.sfu_call().is_some() { antiphon_client::sfu_call::MAX_PEOPLE } else { group::MAX_PEOPLE };
                drop(call);
                self.toast(format!("A group call holds up to {most} people"), ToastKind::Warn);
                return;
            }
            self.group_context(a)
        };
        if moderation::is_blocked(&self.cfg.blocked, &contact.identity_key) {
            self.toast(format!("{} is blocked", contact.name), ToastKind::Warn);
            return;
        }
        let plan = match connection_mode::plan(&contact, &self.state.identity, now_secs()) {
            Ok(plan) => plan,
            Err(reason) => {
                self.toast(reason, ToastKind::Warn);
                return;
            }
        };
        let identity = self.state.identity.clone();
        let events = self.events.clone();
        let name = contact.name;
        self.state.runtime.spawn(async move {
            let connected = match plan {
                connection_mode::Plan::Direct { address } => match state::resolve(&address).await {
                    Ok(addr) => state::establish_call(addr, 0, identity).await.map(|p| (p, false)),
                    Err(e) => Err(e),
                },
                connection_mode::Plan::Relay { relay, token } => async {
                    let addr = state::resolve(&relay).await?;
                    state::establish_relayed_call(addr, token, 0, identity).await
                }
                .await
                .map(|p| (p, true)),
            };
            let (text, kind) = match connected {
                Ok((peer, relayed)) => match ctx.add_person(peer, relayed).await {
                    Ok(m) => (m, ToastKind::Ok),
                    Err(e) => (e, ToastKind::Warn),
                },
                Err(e) => (format!("Couldn't add {name}: {e}"), ToastKind::Warn),
            };
            Self::push(&events, Event::Toast(text, kind));
        });
    }

    /// Everyone on the running call, by main key (hex): whoever it started
    /// with, the mesh legs, and the people in its SFU room.
    fn on_call_keys(&self) -> HashSet<String> {
        let call = self.state.call.lock().unwrap();
        let Some(a) = call.as_ref() else { return HashSet::new() };
        let mut keys: HashSet<String> = a.group.leg_keys().iter().map(connection_mode::encode_hex32).collect();
        keys.insert(connection_mode::encode_hex32(&a.peer.peer_identity().identity_key));
        if let Some(sfu) = a.group.sfu_call() {
            keys.extend(sfu.member_keys().iter().map(connection_mode::encode_hex32));
        }
        keys
    }

    // ---------------------------------------------------------- mailboxes

    /// Puts a mailbox node on the delivery routes and says where it went.
    fn add_node(&mut self, node: &node_picker::NodeLink) {
        match node_picker::add(&mut self.cfg.routes, node) {
            Ok(kind) => {
                self.found_nodes.retain(|a| a != &node.address);
                self.save_routes();
                self.toast(format!("Added {} as {}", node.address, kind.title().to_lowercase()), ToastKind::Ok);
            }
            Err(why) => self.toast(why, ToastKind::Warn),
        }
    }

    /// Saves a changed route list: bumps its sequence so contacts take the
    /// new one over the old, refreshes the cards members see, and wakes
    /// the courier.
    fn save_routes(&mut self) {
        delivery::normalize(&mut self.cfg.routes);
        self.cfg.route_sequence = (self.cfg.route_sequence + 1).max(now_secs());
        self.save();
        std::thread::spawn(community::refresh_cards);
        delivery::kick();
    }

    /// Asks for a picture of a node's QR code and reads it, off the
    /// window's thread.
    fn node_from_qr(&self) {
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Choose a picture of the node's QR code")
                .add_filter("Pictures", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                .pick_file()
            else {
                return;
            };
            Engine::push(&events, Event::NodePicked(node_picker::read_qr(&path)));
        });
    }

    /// Looks for mailbox nodes answering on this network, in the background.
    fn find_nodes(&mut self) {
        if self.finding_nodes {
            return;
        }
        self.finding_nodes = true;
        let events = self.events.clone();
        self.state.runtime.spawn(async move {
            let found = delivery::discover(Duration::from_secs(2)).await;
            Self::push(&events, Event::NodesFound(found.into_iter().map(|a| a.to_string()).collect()));
        });
    }

    /// The mailbox nodes on the routes, with how each answered last.
    fn mailboxes(&self) -> Vec<ui::Mailbox> {
        let report = delivery::node_report();
        self.cfg
            .routes
            .iter()
            .flat_map(|r| r.nodes.iter().map(move |n| (r.kind, n)))
            .map(|(kind, address)| ui::Mailbox {
                address: address.clone(),
                route: kind.title().to_string(),
                status: report.iter().find(|(a, _)| a == address).map(|(_, s)| s.clone()).unwrap_or_default(),
            })
            .collect()
    }

    // ---------------------------------------------------------- directories

    /// Reads every directory added, in the background.
    fn browse_directories(&mut self) {
        if self.directory.browsing || self.cfg.directories.is_empty() {
            return;
        }
        self.directory.browsing = true;
        let (dirs, mut pins) = (self.cfg.directories.clone(), self.cfg.directory_pins.clone());
        let events = self.events.clone();
        self.state.runtime.spawn(async move {
            let (listed, problems) = discovery::browse(&dirs, &mut pins).await;
            Self::push(&events, Event::Directory(listed, problems, pins));
        });
    }

    /// Asks `directory` to list a community with `description` (`None`
    /// takes it off), signed by the key we are in it as, which alone can
    /// change it later.
    fn submit_listing(&mut self, community: String, directory: String, description: Option<String>) {
        let made = self.spaces.listing(&community, description.as_deref());
        let (entry, identity) = match made {
            Ok(made) => made,
            Err(e) => {
                self.toast(e, ToastKind::Warn);
                return;
            }
        };
        let events = self.events.clone();
        let listing = description.is_some();
        self.state.runtime.spawn(async move {
            let (text, kind) = match discovery::submit(&directory, entry, &identity).await {
                Ok(()) if listing => (format!("Listed in {directory}"), ToastKind::Ok),
                Ok(()) => (format!("Taken off {directory}"), ToastKind::Ok),
                Err(e) => (e, ToastKind::Warn),
            };
            Self::push(&events, Event::Toast(text, kind));
        });
    }

    // -------------------------------------------------------- voice channels

    /// Joins a voice channel: says so to its readers, with our invite link
    /// so whoever is there can put us through, and starts taking calls.
    fn join_voice(&mut self, community: String, channel: u32) {
        if self.voice.as_ref().is_some_and(|r| r.community == community && r.channel == channel) {
            return;
        }
        if self.voice.is_none() && self.state.call.lock().unwrap().is_some() {
            self.toast("Hang up first: you're on a call", ToastKind::Warn);
            return;
        }
        if let Some(room) = self.voice.take() {
            self.leave_voice(&room);
            self.teardown("Left the voice channel".into());
        }
        self.refresh_invite();
        match self.spaces.voice_join(&community, channel, &self.invite) {
            Ok(()) => {
                self.voice = Some(VoiceRoom { community, channel, said_at: Instant::now(), dialed: HashMap::new() });
                self.spaces.set_voice_room(self.voice.as_ref().map(|r| (r.community.clone(), r.channel)));
            }
            Err(e) => self.toast(e, ToastKind::Warn),
        }
    }

    /// Says we left a voice channel.
    fn leave_voice(&mut self, room: &VoiceRoom) {
        if let Err(e) = self.spaces.voice_leave(&room.community, room.channel) {
            self.toast(e, ToastKind::Warn);
        }
        self.spaces.set_voice_room(None);
    }

    /// While in a voice channel: say we still are now and then, and put
    /// people through. One person does the dialing so two don't call each
    /// other at once: whoever has the lowest key among those there. They
    /// call the first person, then add each one after to the call, which
    /// connects everyone to everyone (or moves to the relay's SFU).
    fn voice_room_upkeep(&mut self) {
        let Some(room) = self.voice.as_mut() else { return };
        let (community, channel) = (room.community.clone(), room.channel);
        if room.said_at.elapsed() >= Duration::from_secs(community::VOICE_HEARTBEAT_SECS) {
            room.said_at = Instant::now();
            let invite = self.invite.clone();
            if let Err(e) = self.spaces.voice_join(&community, channel, &invite) {
                self.toast(e, ToastKind::Warn);
            }
        }
        if self.call.phase == ui::Phase::Dialing {
            return;
        }
        let me = connection_mode::encode_hex32(&self.state.identity.public().identity_key);
        let there: Vec<antiphon_crypto::Invite> = self
            .spaces
            .voice_here(&community, channel)
            .iter()
            .filter_map(|p| antiphon_crypto::Invite::parse(&p.invite).ok())
            .filter(|i| connection_mode::encode_hex32(&i.identity_key) != me)
            .collect();
        let lowest = there.iter().map(|i| connection_mode::encode_hex32(&i.identity_key)).min();
        if lowest.is_some_and(|k| k < me) {
            return; // theirs to dial
        }
        let on_call = self.on_call_keys();
        let Some(room) = self.voice.as_mut() else { return };
        let next = there.into_iter().find(|i| {
            let key = connection_mode::encode_hex32(&i.identity_key);
            !on_call.contains(&key) && room.dialed.get(&key).is_none_or(|at| at.elapsed() >= VOICE_REDIAL)
        });
        let Some(invite) = next else { return };
        let key = connection_mode::encode_hex32(&invite.identity_key);
        room.dialed.insert(key.clone(), Instant::now());
        let name = self.cfg.contacts.iter().find(|c| c.identity_key == key).map(|c| c.name.clone()).unwrap_or_else(|| "someone in the voice channel".into());
        let contact = config::Contact {
            name,
            address: invite.address.clone(),
            identity_key: key,
            relay: invite.relay.clone(),
            relay_secret: invite.relay_secret.as_ref().map(connection_mode::encode_hex32).unwrap_or_default(),
            lends_relay: invite.lends_relay,
            ..Default::default()
        };
        if self.state.call.lock().unwrap().is_some() {
            self.add_to_call(contact);
        } else {
            self.dial(contact);
        }
    }

    /// Reads a yes/no from the running call; false when there is none.
    fn flag(&self, read: impl Fn(&state::ActiveCall) -> bool) -> bool {
        self.state.call.lock().unwrap().as_ref().map(read).unwrap_or(false)
    }

    /// Per-tick upkeep of a running call: the peer's profile, new messages,
    /// a file that finished arriving.
    fn watch_call(&mut self, open: Option<&str>) {
        let mut save_relay = None;
        let mut save_routes = None;
        let mut new_lines = 0usize;
        let ready;
        let card;
        {
            let call = self.state.call.lock().unwrap();
            let Some(active) = call.as_ref() else {
                self.their_level = 0.0;
                return;
            };
            let level = active.peer_level.lock().map(|l| *l).unwrap_or(0.0);
            // A meter that falls gently reads as speech; one that snaps to
            // zero between frames flickers.
            self.their_level = level.max(self.their_level * 0.82);
            if let Some(peer) = active.peer_profile.lock().unwrap().as_ref() {
                if self.call.peer.is_none() && !peer.display_name.is_empty() {
                    self.call.name = peer.display_name.clone();
                }
                let said = format!("{}|{}", peer.relay, peer.lends_relay);
                if !peer.relay.is_empty() && said != self.peer_relay_saved {
                    save_relay = Some((peer.relay.clone(), peer.lends_relay, said));
                }
                if !peer.routes.is_empty() && peer.routes != self.peer_routes_saved {
                    save_routes = Some(peer.routes.clone());
                }
            }
            let log = active.chat_log.lock().unwrap();
            if log.len() > self.seen_lines {
                new_lines = log[self.seen_lines..].iter().filter(|e| !e.from_me).count();
                self.seen_lines = log.len();
            }
            ready = active.file_transfer.lock().unwrap().ready_to_save.take();
            card = active.peer_card.lock().unwrap().take();
        }
        let key = self.call.peer.clone();
        if let Some(card) = card {
            self.save_card(key.as_deref(), card);
        }
        if let Some((relay, lends, said)) = save_relay {
            self.peer_relay_saved = said;
            if let Some(c) = key.as_deref().and_then(|k| self.cfg.contacts.iter_mut().find(|c| c.identity_key == k)) {
                if c.relay != relay || c.lends_relay != lends {
                    c.relay = relay;
                    c.lends_relay = lends;
                    self.save();
                }
            }
        }
        if let Some(routes) = save_routes {
            self.peer_routes_saved = routes.clone();
            if let Some(k) = key.as_deref().and_then(delivery::unhex32) {
                save_peer_routes(&k, &routes);
                self.cfg = config::load();
            }
        }
        if new_lines > 0 {
            if let Some(id) = key.as_deref() {
                if open != Some(id) {
                    *self.unread.entry(id.to_string()).or_default() += new_lines as u32;
                }
            }
        }
        if let Some((name, stored)) = ready {
            self.save_received(name, stored);
        }
    }

    /// Keeps the card the person on the call sent: saved to their contact
    /// when they are one (so it shows between calls, and is in backups),
    /// and shown on the call either way.
    fn save_card(&mut self, key: Option<&str>, card: ProfileCard) {
        let avatar = if profile::decode_avatar(&card.avatar).is_some() { card.avatar } else { String::new() };
        let bio = profile::clean_bio(&card.bio);
        let color = if ui::avatars::named_color(&card.color).is_some() { card.color } else { String::new() };
        let pronouns = profile::clean_pronouns(&card.pronouns);
        match key.and_then(|k| contact_mut(&mut self.cfg, k)) {
            Some(c) => {
                if (&c.avatar, &c.bio, &c.color, &c.pronouns) != (&avatar, &bio, &color, &pronouns) {
                    (c.avatar, c.bio, c.color, c.pronouns) = (avatar, bio, color, pronouns);
                    self.save();
                }
            }
            // Someone not saved yet: their avatar on the call is keyed by
            // the name shown.
            None => ui::avatars::set(&self.call.name, &avatar, &color, profile::decode_avatar),
        }
    }

    /// Saves the theme, accent and reading font to a theme file the person
    /// picks, off the window's thread.
    fn export_theme(&mut self) {
        let s = &self.cfg.stoop;
        let file = ui::theme_file::ThemeFile::new(&s.theme, &s.accent, &s.custom_accent, &s.font);
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Export theme")
                .set_file_name(ui::theme_file::FILE_NAME)
                .add_filter("Stoop theme", &[ui::theme_file::EXTENSIONS[0]])
                .save_file()
            else {
                return;
            };
            let (text, kind) = match std::fs::write(&path, file.to_markup()) {
                Ok(()) => (format!("Theme saved to {}", path.display()), ToastKind::Ok),
                Err(e) => (format!("Couldn't save the theme: {e}"), ToastKind::Err),
            };
            Self::push(&events, Event::Toast(text, kind));
        });
    }

    /// Asks for a theme file, reads and checks it off the window's thread,
    /// and hands it back as [`Event::Theme`]. Nothing in it is run: it is
    /// parsed as data and every value checked (see `theme_file`).
    fn import_theme(&mut self) {
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new().set_title("Import theme").add_filter("Stoop theme", &ui::theme_file::EXTENSIONS).pick_file() else { return };
            // Refuse anything too big before reading it into memory.
            let too_big = std::fs::metadata(&path).map(|m| m.len() > ui::theme_file::MAX_BYTES as u64).unwrap_or(false);
            let parsed = if too_big {
                Err("That file is too big to be a theme file.".to_string())
            } else {
                std::fs::read_to_string(&path).map_err(|e| format!("Couldn't read the theme: {e}")).and_then(|t| ui::theme_file::ThemeFile::parse(&t))
            };
            match parsed {
                Ok(file) => Self::push(&events, Event::Theme(file)),
                Err(why) => Self::push(&events, Event::Toast(why, ToastKind::Err)),
            }
        });
    }

    /// Offers a save dialog for a received file, off the window's thread.
    fn save_received(&mut self, name: String, stored: PathBuf) {
        let state = self.state.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            let clear = || {
                if let Some(active) = state.call.lock().unwrap().as_ref() {
                    active.file_transfer.lock().unwrap().incoming = None;
                }
            };
            let Some(path) = rfd::FileDialog::new().set_title("Save received file").set_file_name(&name).save_file() else {
                clear();
                return;
            };
            match std::fs::copy(&stored, &path) {
                Ok(_) => {
                    clear();
                    Self::push(&events, Event::Toast(format!("Saved {name}"), ToastKind::Ok));
                }
                Err(e) => Self::push(&events, Event::Toast(format!("Got the file but couldn't save it: {e}"), ToastKind::Err)),
            }
        });
    }

    /// Mail came in or went out while offline: refresh previews and count
    /// what is new from people not on screen.
    fn mail_arrived(&mut self, open: Option<&str>) {
        let before = self.previews.clone();
        self.load_previews();
        for (id, (_, at)) in &self.previews {
            let newer = before.get(id).is_none_or(|(_, was)| at > was);
            let theirs = self.last_from_them(id);
            if newer && theirs && open != Some(id.as_str()) && !self.call_with(id) {
                *self.unread.entry(id.clone()).or_default() += 1;
            }
        }
        // The open conversation may have changed underneath.
        if self.conv.as_ref().is_some_and(|c| !c.live) {
            self.conv = None;
        }
    }

    /// Whether the newest saved message with `id` came from them.
    fn last_from_them(&self, id: &str) -> bool {
        let (Some(dir), Some(key)) = (&self.dir, &self.key) else { return false };
        history::load(dir, id, key).last().is_some_and(|m| !m.from_me)
    }

    /// Whether we are connected to `id` right now.
    fn call_with(&self, id: &str) -> bool {
        self.call.peer.as_deref() == Some(id) && self.call.phase == ui::Phase::Connected
    }

    /// Reads the last saved message of every contact, for the list previews.
    fn load_previews(&mut self) {
        let (Some(dir), Some(key)) = (&self.dir, &self.key) else { return };
        let notes = self.devices.as_ref().map(|d| d.notes_id());
        for id in self.cfg.contacts.iter().map(|c| &c.identity_key).filter(|k| !k.is_empty()).chain(&notes) {
            let log = history::load(dir, id, key);
            if let Some(m) = log.iter().rev().find(|m| !m.deleted) {
                // Every note is yours; "You:" in front of each would say nothing.
                let text = if m.from_me && notes.as_ref() != Some(id) { format!("You: {}", m.text) } else { m.text.clone() };
                self.previews.insert(id.clone(), (text, m.at_secs));
            }
        }
    }

    /// Applies one background result.
    fn on_event(&mut self, e: Event) {
        match e {
            Event::Status(s) => self.call.status = Some(s),
            Event::Connected { incoming, peer, name, code, mode, warning, known } => {
                self.task = None;
                self.seen_lines = self.state.call.lock().unwrap().as_ref().map(|a| a.chat_log.lock().unwrap().len()).unwrap_or(0);
                self.peer_relay_saved.clear();
                self.peer_routes_saved.clear();
                self.conv = None;
                self.call = CallInfo {
                    phase: ui::Phase::Connected,
                    peer: known.then(|| peer.clone()),
                    name: name.clone(),
                    code,
                    mode: mode.label().to_string(),
                    mode_detail: mode.explain().to_string(),
                    warning,
                    status: None,
                    ended_at: None,
                };
                // Our picture, About line and colour go out at the start
                // of every call, so they always see the current ones.
                self.send_card();
                if incoming {
                    let ring = delivery::unhex32(&peer).map(|k| ring_outcome(&k, false)).unwrap_or(notify::Outcome::Ring);
                    if ring != notify::Outcome::Ignore {
                        self.toast(format!("{name} came by"), ToastKind::Info);
                    }
                    if known {
                        self.out.push(Out::Show(ui::Route::Person(peer)));
                    }
                }
            }
            Event::Failed(why) => {
                self.task = None;
                if self.call.phase == ui::Phase::Dialing {
                    self.end(why.clone());
                    self.toast(why, ToastKind::Err);
                }
            }
            Event::ListenFailed(why) => {
                self.task = None;
                self.door = false;
                self.toast(format!("Couldn't open your door: {why}"), ToastKind::Err);
            }
            Event::Public(r) => {
                self.checking = false;
                match r {
                    Ok(ip) => {
                        self.reach.public_address_known = true;
                        self.public = Some(ip);
                    }
                    Err(e) => {
                        self.reach.public_address_known = false;
                        self.public = None;
                        self.toast(format!("Couldn't find your public address: {e}"), ToastKind::Warn);
                    }
                }
                self.refresh_invite();
            }
            Event::RelayChosen(choice, mesh) => {
                if !mesh.is_empty() {
                    let merged = fallback_relay::merge_mesh(&self.cfg.mesh_relays, &mesh);
                    if merged != self.cfg.mesh_relays {
                        self.cfg.mesh_relays = merged;
                        self.save();
                    }
                }
                if choice != self.relay_choice {
                    if let Some(fallback_relay::Choice { source: fallback_relay::Source::Borrowed, lender: Some(who), .. }) = &choice {
                        self.toast(format!("You have no relay of your own, so you can be reached through {who}'s"), ToastKind::Info);
                    }
                    self.relay_choice = choice;
                    self.invite.clear();
                    self.refresh_invite();
                    self.relisten();
                }
            }
            Event::PortMap(r) => match r {
                Ok(port) => {
                    self.reach.port_forwarded = true;
                    self.reach.port_forward_refused = false;
                    if port != self.cfg.stoop.port {
                        self.toast(format!("Your router forwarded port {port} instead of {}", self.cfg.stoop.port), ToastKind::Info);
                    }
                    self.refresh_invite();
                }
                Err(_) => {
                    self.reach.port_forwarded = false;
                    self.reach.port_forward_refused = true;
                }
            },
            Event::Key(r) => {
                self.ptt_capturing = false;
                match r {
                    Ok(name) => {
                        self.cfg.ptt_key = name.clone();
                        self.sync_transmit();
                        self.save();
                        self.toast(format!("Push to talk is now {name}"), ToastKind::Ok);
                    }
                    Err(why) => self.toast(why, ToastKind::Warn),
                }
            }
            Event::Toast(text, kind) => self.toast(text, kind),
            Event::NodesFound(found) => {
                self.finding_nodes = false;
                if found.is_empty() {
                    self.toast("No mailbox nodes answered on this network", ToastKind::Warn);
                }
                self.found_nodes = found;
            }
            Event::NodePicked(Ok(node)) => self.add_node(&node),
            Event::NodePicked(Err(why)) => self.toast(why, ToastKind::Err),
            Event::Directory(listed, problems, pins) => {
                self.cfg.directory_pins = pins;
                self.save();
                self.directory.browsing = false;
                self.directory.problems = problems;
                self.directory.listed = listed
                    .into_iter()
                    .map(|l| ui::Listing { name: l.entry.name, description: l.entry.description, link: l.entry.invite, directory: l.directory })
                    .collect();
            }
            Event::PluginsFound(found) => {
                self.plugins_scanning = false;
                self.plugins_found = Some(found);
            }
            Event::Theme(file) => {
                self.cfg.stoop.theme = file.theme;
                self.cfg.stoop.accent = file.accent;
                if !file.custom_accent.is_empty() {
                    self.cfg.stoop.custom_accent = file.custom_accent;
                }
                self.cfg.stoop.font = file.font;
                self.save();
                self.toast("Theme loaded", ToastKind::Ok);
            }
            Event::Avatar(Ok(avatar)) => {
                self.cfg.profile.avatar = avatar;
                self.profile_changed();
                self.toast("Picture updated", ToastKind::Ok);
            }
            Event::Avatar(Err(why)) => self.toast(why, ToastKind::Err),
            Event::VoiceSent(Ok(())) => {
                // It is in their history now; the conversation reloads.
                self.conv = None;
                self.toast("Voice message sent", ToastKind::Ok);
            }
            Event::VoiceSent(Err(why)) => self.toast(why, ToastKind::Err),
            Event::EmojiPicked(Ok((community, file, image))) => match crate::spaces::add_emoji(&community, &file, image) {
                Ok(name) => self.toast(format!("Added :{name}:"), ToastKind::Ok),
                Err(why) => self.toast(why, ToastKind::Err),
            },
            Event::EmojiPicked(Err(why)) => self.toast(why, ToastKind::Err),
            Event::Queue(paths, peer) => {
                for path in paths {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    self.send_queue.push(QueuedSend { path, name, size, peer: peer.clone() });
                }
                self.pump_queue();
            }
        }
    }

    // ---------------------------------------------------------------- profile

    /// Our profile card as contacts get it.
    fn card(&self) -> ProfileCard {
        let p = &self.cfg.profile;
        ProfileCard { bio: p.bio.clone(), color: p.color.clone(), avatar: p.avatar.clone(), pronouns: p.pronouns.clone() }
    }

    /// Sends our card to the person on the call, if there is one. Said
    /// three times a little apart: it is one datagram, and a lost one would
    /// leave them with our old picture until the next call.
    fn send_card(&self) {
        let call = self.state.call.lock().unwrap();
        let Some(a) = call.as_ref() else { return };
        let (peer, message) = (a.peer.clone(), ControlMessage::Profile(self.card()).encode());
        self.state.runtime.spawn(async move {
            for _ in 0..3 {
                let _ = peer.send_control(&message).await;
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        });
    }

    /// Saves a changed profile and tells the person on the call. The name
    /// and status ride in `Hello`, which goes out again too.
    fn profile_changed(&mut self) {
        self.save();
        self.send_card();
        let hello = self.hello();
        self.with_call(|a| a.send_control(&self.state.runtime, ControlMessage::Hello(hello)));
    }

    /// Asks for a picture file off the window's thread, and makes it small
    /// there too (decoding a large photo takes a moment).
    fn pick_avatar(&self) {
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(path) = rfd::FileDialog::new()
                .set_title("Choose a profile picture")
                .add_filter("Pictures", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
                .pick_file()
            else {
                return;
            };
            let made = std::fs::read(&path).map_err(|e| format!("Couldn't open that file: {e}")).and_then(|b| profile::avatar_from_image(&b));
            Engine::push(&events, Event::Avatar(made));
        });
    }

    // ---------------------------------------------------------------- calls

    /// What we say about ourselves at the start of every call.
    fn hello(&self) -> Hello {
        let chosen = presence::Presence::from_i32(self.cfg.presence);
        Hello {
            display_name: self.cfg.profile.display_name.clone(),
            status_message: self.cfg.profile.status_message.clone(),
            presence: chosen.should_publish().then(|| chosen.to_i32()),
            wants_silent: false,
            relay: self.my_relay(),
            lends_relay: self.lends_relay(),
            routes: delivery::my_record_hex(),
        }
    }

    /// Everything a connect task needs, cloned out of the engine.
    fn pipeline_args(&self) -> Connect {
        Connect {
            state: self.state.clone(),
            events: self.events.clone(),
            chain: self.cfg.chain.clone(),
            input: self.cfg.input_device.clone(),
            output: self.cfg.output_device.clone(),
            hello: self.hello(),
        }
    }

    /// Cancels the listen or dial task, if one is running.
    fn stop_task(&mut self) {
        if let Some((_, t)) = self.task.take() {
            t.abort();
        }
    }

    /// Waits for a call on the door's port.
    fn listen(&mut self) {
        let port = self.cfg.stoop.port;
        let args = self.pipeline_args();
        let relay = self.listen_relay();
        let identity = self.state.identity.clone();
        let handle = self.state.runtime.spawn(async move {
            let relay = match relay {
                Some((address, tokens)) => match state::resolve(&address).await {
                    Ok(addr) => {
                        *args.state.relay_addr.lock().unwrap() = Some(addr);
                        Some((addr, tokens))
                    }
                    Err(_) => None,
                },
                None => {
                    *args.state.relay_addr.lock().unwrap() = None;
                    None
                }
            };
            match state::establish_listen(port, identity, relay).await {
                Ok(peer) => args.finish(Ok(peer), true),
                Err(e) => Self::push(&args.events, Event::ListenFailed(e.to_string())),
            }
        });
        self.task = Some((TaskKind::Listen, handle));
    }

    /// What to register at our relay while listening, if there is one.
    fn listen_relay(&self) -> Option<(String, state::RelayTokens)> {
        let relay = self.my_relay();
        if relay.is_empty() {
            return None;
        }
        let identity = self.state.identity.clone();
        let secret = relay_invite_secret();
        Some((relay, Box::new(move || connection_mode::listening_tokens(&config::load().contacts, &identity, Some(secret), now_secs()))))
    }

    /// The relay to listen at and tell people about: the one the last check
    /// chose, or yours unchecked, or none (empty).
    fn my_relay(&self) -> String {
        match &self.relay_choice {
            Some(choice) => choice.relay.clone(),
            None => self.cfg.relay_server.trim().to_string(),
        }
    }

    /// Whether we tell contacts they may listen at our relay: only when we
    /// said so, and only for a relay that is ours -- a borrowed or fallback
    /// relay is not ours to lend.
    fn lends_relay(&self) -> bool {
        let own = self.cfg.relay_server.trim();
        self.cfg.lend_relay && !own.is_empty() && self.my_relay() == own
    }

    /// Works out, in the background, which relay to use: yours, your
    /// fallbacks, the mesh's, or a contact's lent relay -- and learns the
    /// mesh from whichever answers.
    fn choose_relay(&mut self) {
        let cfg = self.cfg.clone();
        let events = self.events.clone();
        self.state.runtime.spawn(async move {
            let choice = fallback_relay::choose(&cfg).await;
            let mesh = match &choice {
                Some(c) => fallback_relay::learn_mesh(&c.relay).await,
                None => Vec::new(),
            };
            Self::push(&events, Event::RelayChosen(choice, mesh));
        });
    }

    /// Opens or closes the door: listen and map a router port, or stop.
    fn set_door(&mut self, open: bool) {
        self.door = open;
        if open {
            // Being called needs a way in through the router.
            if self.state.port_map.lock().unwrap().is_none() {
                self.map_port();
            }
        } else if self.task.as_ref().is_some_and(|(k, _)| *k == TaskKind::Listen) {
            self.stop_task();
        }
    }

    /// Asks the router to forward our port inward, in the background.
    fn map_port(&mut self) {
        let port = self.cfg.stoop.port;
        let state = self.state.clone();
        let events = self.events.clone();
        self.state.runtime.spawn(async move {
            match state::open_port_automatically(port).await {
                Ok(handle) => {
                    let external = handle.external_port;
                    *state.port_map.lock().unwrap() = Some(handle);
                    Self::push(&events, Event::PortMap(Ok(external)));
                }
                Err(e) => Self::push(&events, Event::PortMap(Err(e.to_string()))),
            }
        });
    }

    /// Runs the STUN check for our public address, unless one is running.
    fn check_network(&mut self) {
        if self.checking {
            return;
        }
        self.checking = true;
        let events = self.events.clone();
        self.state.runtime.spawn(async move {
            // Port 0: the door may be holding the listening port, and the
            // address the world sees is what matters here.
            let r = state::discover_public_address(0).await.map(|(addr, _)| addr.ip().to_string()).map_err(|e| e.to_string());
            Self::push(&events, Event::Public(r));
        });
        self.choose_relay();
        if self.door && self.state.port_map.lock().unwrap().is_none() {
            self.map_port();
        }
    }

    /// The invite link and its QR code, from the address known now.
    fn refresh_invite(&mut self) {
        let port = self.state.port_map.lock().unwrap().as_ref().map(state::PortMapHandle::current_external_port).unwrap_or(self.cfg.stoop.port);
        let address = self.public.as_ref().map(|ip| if ip.contains(':') { format!("[{ip}]:{port}") } else { format!("{ip}:{port}") }).unwrap_or_default();
        let mut invite = antiphon_crypto::Invite::new(self.state.identity.public().identity_key, address);
        let relay = self.my_relay();
        if !relay.is_empty() {
            invite.relay = relay;
            invite.relay_secret = Some(relay_invite_secret());
            invite.lends_relay = self.lends_relay();
        }
        let link = invite.to_link();
        if link != self.invite {
            self.qr = qr_code::Data::new(link.as_bytes()).ok().map(Arc::new);
            self.invite = link;
        }
    }

    /// Calls a saved contact, by the route their contact says (direct, DHT lookup or relay).
    fn call_contact(&mut self, id: &str) {
        if self.call.phase == ui::Phase::Connected || self.call.phase == ui::Phase::Dialing {
            self.toast("You're already on a call", ToastKind::Warn);
            return;
        }
        let Some(contact) = find_contact(&self.cfg, id).cloned() else { return };
        self.dial(contact);
    }

    /// Calls `contact` (saved, or made up from an invite link) by the route
    /// it says: direct, DHT lookup or relay.
    fn dial(&mut self, contact: config::Contact) {
        let id = contact.identity_key.clone();
        if moderation::is_blocked(&self.cfg.blocked, &contact.identity_key) {
            self.toast(format!("{} is blocked", contact.name), ToastKind::Warn);
            return;
        }
        let plan = match connection_mode::plan(&contact, &self.state.identity, now_secs()) {
            Ok(plan) => plan,
            Err(reason) => {
                self.toast(reason, ToastKind::Warn);
                return;
            }
        };
        self.stop_task();
        self.call = CallInfo { phase: ui::Phase::Dialing, peer: Some(id), name: contact.name.clone(), ..CallInfo::default() };
        let args = self.pipeline_args();
        let identity = self.state.identity.clone();
        let lookup = self.cfg.dht_enabled && !contact.identity_key.is_empty();
        let handle = self.state.runtime.spawn(async move {
            // A phone asleep in a pocket isn't listening; if the dial has
            // not connected within a moment, knock through its push
            // service so it wakes and starts listening while we keep
            // trying. Aborted below once the dial is over either way.
            let wake = push_wake::wake_later(identity.clone(), &contact.identity_key, &contact.route_record, Vec::new());
            *args.state.relay_addr.lock().unwrap() = None;
            let result = match plan {
                connection_mode::Plan::Direct { address } if lookup => call_with_lookup(&contact, address, identity, &args).await,
                connection_mode::Plan::Direct { address } => async {
                    let remote = state::resolve(&address).await?;
                    state::establish_call(remote, 0, identity).await
                }
                .await,
                connection_mode::Plan::Relay { relay, token } => async {
                    Self::push(&args.events, Event::Status(format!("Trying {} through a relay...", contact.name)));
                    let addr = state::resolve(&relay).await?;
                    *args.state.relay_addr.lock().unwrap() = Some(addr);
                    state::establish_relayed_call(addr, token, 0, identity).await
                }
                .await,
            };
            if let Some(wake) = wake {
                wake.abort();
            }
            let result = result.map_err(|e| anyhow::anyhow!("Couldn't reach {}: {e}", contact.name));
            args.finish(result, false);
        });
        self.task = Some((TaskKind::Dial, handle));
    }

    /// Calls a bare `host:port` someone typed.
    fn call_address(&mut self, address: String) {
        if self.call.phase == ui::Phase::Connected || self.call.phase == ui::Phase::Dialing {
            self.toast("You're already on a call", ToastKind::Warn);
            return;
        }
        self.stop_task();
        self.call = CallInfo { phase: ui::Phase::Dialing, peer: None, name: address.clone(), ..CallInfo::default() };
        let args = self.pipeline_args();
        let identity = self.state.identity.clone();
        let handle = self.state.runtime.spawn(async move {
            *args.state.relay_addr.lock().unwrap() = None;
            let result = async {
                let remote = state::resolve(&address).await?;
                state::establish_call(remote, 0, identity).await
            }
            .await
            .map_err(|e| anyhow::anyhow!("Couldn't reach {address}: {e}"));
            args.finish(result, false);
        });
        self.task = Some((TaskKind::Dial, handle));
    }

    /// Ends the call (or the attempt) from this side.
    fn hangup(&mut self) {
        // Hanging up in a voice channel is leaving it.
        if let Some(room) = self.voice.take() {
            self.leave_voice(&room);
        }
        if self.call.phase == ui::Phase::Dialing {
            self.stop_task();
            self.end("Stopped calling".into());
            return;
        }
        self.teardown("Call ended".into());
    }

    /// Drops the running call and puts everything back.
    fn teardown(&mut self, why: String) {
        let active = self.state.call.lock().unwrap().take();
        let Some(active) = active else { return };
        file_transfer::save_partial_if_interrupted(&active.file_transfer);
        active.say_goodbye(&self.state.runtime);
        drop(active);
        self.cfg.calls_completed = self.cfg.calls_completed.saturating_add(1);
        self.save();
        delivery::kick();
        file_transfer::set_call_active(false);
        if let Some(peer) = self.call.peer.clone() {
            self.conv = None;
            self.load_previews();
            self.unread.remove(&peer);
        }
        self.end(why);
    }

    /// Marks the call ended, with the reason to show for a moment.
    fn end(&mut self, why: String) {
        self.call.phase = ui::Phase::Ended(why);
        self.call.ended_at = Some(Instant::now());
        self.call.status = None;
        self.their_level = 0.0;
    }

    /// Runs `f` on the running call, if there is one.
    fn with_call<R>(&self, f: impl FnOnce(&state::ActiveCall) -> R) -> Option<R> {
        self.state.call.lock().unwrap().as_ref().map(f)
    }

    // ---------------------------------------------------------------- actions

    /// Carries out what the person asked for.
    pub fn act(&mut self, action: ui::Action, open: Option<&str>) {
        use ui::Action as A;
        let rt = self.state.runtime.clone();
        match action {
            A::Open(id) => {
                self.unread.remove(&id);
            }
            A::Door(on) => {
                self.set_door(on);
                self.cfg.stoop.door_on_start = on;
                self.save();
            }
            A::Call(id) => self.call_contact(&id),
            A::AddToCall(id) => {
                if let Some(contact) = find_contact(&self.cfg, &id).cloned() {
                    self.toast(format!("Adding {}...", contact.name), ToastKind::Info);
                    self.add_to_call(contact);
                }
            }
            A::CallAddress(address) => self.call_address(address),
            A::Hangup => self.hangup(),
            A::Mute(on) => {
                self.with_call(|a| a.muted.store(on, Ordering::Relaxed));
            }
            A::Deafen(on) => {
                self.with_call(|a| a.deafened.store(on, Ordering::Relaxed));
            }
            A::PeerVolume(db) => {
                *self.state.peer_volume_db.lock().unwrap() = db;
                if let Some(c) = self.call.peer.clone().and_then(|k| self.cfg.contacts.iter_mut().find(|c| c.identity_key == k)) {
                    c.volume_db = db;
                    self.save();
                }
            }
            A::Volume(id, db) => {
                let live = self.call.peer.as_deref() == Some(id.as_str());
                if let Some(c) = contact_mut(&mut self.cfg, &id) {
                    c.volume_db = db;
                    self.save();
                }
                if live {
                    *self.state.peer_volume_db.lock().unwrap() = db;
                }
            }
            A::MuteFor(id, muted) => {
                let live = self.call.peer.as_deref() == Some(id.as_str());
                if let Some(c) = contact_mut(&mut self.cfg, &id) {
                    c.muted = muted;
                    let name = c.name.clone();
                    self.save();
                    self.toast(if muted { format!("{name} is muted for you") } else { format!("You can hear {name} again") }, ToastKind::Info);
                }
                if live {
                    self.state.peer_muted.store(muted, Ordering::Relaxed);
                }
            }
            // A reply carries the id it answers, live or by mailbox; an id
            // the history doesn't hold is dropped on the way and the
            // message goes plain.
            A::Send { to, text, reply_to } => {
                let notes = self.devices.as_ref().filter(|d| d.notes_id() == to);
                let preview = if notes.is_some() { text.clone() } else { format!("You: {text}") };
                if let Some(d) = notes {
                    match d.add_note(&text) {
                        Ok(()) => self.conv = None,
                        Err(e) => self.toast(format!("Couldn't save the note: {e}"), ToastKind::Err),
                    }
                } else if self.call_with(&to) {
                    self.with_call(|a| a.send_reply(&rt, text, reply_to));
                } else if let Some(c) = find_contact(&self.cfg, &to) {
                    match delivery::leave_reply(&c.identity_key, &text, c.expire_after_secs, reply_to) {
                        Ok(()) => self.conv = None,
                        Err(e) => self.toast(e, ToastKind::Err),
                    }
                }
                self.previews.insert(to, (preview, now_secs()));
            }
            A::Edit { to, id, text } => {
                if self.call_with(&to) {
                    self.with_call(|a| a.edit_chat(&rt, id, text));
                } else if let (Some(dir), Some(key)) = (&self.dir, &self.key) {
                    history::apply_edit(dir, &to, key, id, true, &text);
                    self.conv = None;
                    self.share_note(&to, NoteOp::Edit { id, text });
                }
            }
            A::Delete { to, id } => {
                if self.call_with(&to) {
                    self.with_call(|a| a.delete_chat(&rt, id));
                } else if let (Some(dir), Some(key)) = (&self.dir, &self.key) {
                    history::apply_delete(dir, &to, key, id, true);
                    self.conv = None;
                    self.share_note(&to, NoteOp::Delete { id });
                }
            }
            A::React { to, id, emoji } => {
                if self.call_with(&to) {
                    self.with_call(|a| a.react(&rt, id, emoji));
                } else {
                    self.toast("Reactions go through while you're on a call together", ToastKind::Info);
                }
            }
            A::Pin { to, id, pinned } => {
                if self.call_with(&to) {
                    self.with_call(|a| a.set_chat_pinned(id, pinned));
                } else if let (Some(dir), Some(key)) = (&self.dir, &self.key) {
                    history::set_pinned(dir, &to, key, id, pinned);
                    self.conv = None;
                    self.share_note(&to, NoteOp::Pin { id, pinned });
                }
            }
            A::SendFile => self.send_file(),
            A::RecordVoice(to) => self.record_voice(to),
            A::AddEmoji(community) => {
                let events = self.events.clone();
                std::thread::spawn(move || {
                    let Some(path) = rfd::FileDialog::new().set_title("Choose an emoji picture").add_filter("Pictures", &["png", "jpg", "jpeg", "gif", "webp"]).pick_file() else {
                        return;
                    };
                    let file = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let made = std::fs::read(&path).map_err(|e| format!("Couldn't open that file: {e}")).and_then(|b| profile::emoji_from_image(&b));
                    Engine::push(&events, Event::EmojiPicked(made.map(|image| (community, file, image))));
                });
            }
            A::FinishVoice { to, send } => self.finish_voice(&to, send),
            A::PlayVoice { to, id } => self.play_voice(to, id),
            A::AcceptFile => {
                if let Some(Some(why)) = self.with_call(|a| a.accept_file_offer(&rt)) {
                    self.toast(why, ToastKind::Warn);
                }
            }
            A::DeclineFile => {
                self.with_call(|a| a.reject_file_offer(&rt));
            }
            A::CancelTransfer => {
                self.with_call(|a| a.cancel_file_transfer(&rt));
            }
            A::QueueMove { id, up } => {
                if up {
                    self.send_queue.move_up(id);
                } else {
                    self.send_queue.move_down(id);
                }
                self.save_queue();
            }
            A::QueueHold { id, held } => {
                self.send_queue.set_held(id, held);
                self.pump_queue();
            }
            A::QueueRemove(id) => {
                self.send_queue.remove(id);
                self.save_queue();
            }
            A::AddPerson { name, invite } => self.add_person(name, invite),
            A::Remove(id) => {
                if let Some(i) = contact_index(&self.cfg, &id) {
                    let gone = self.cfg.contacts.remove(i);
                    self.save();
                    self.toast(format!("Removed {}", gone.name), ToastKind::Ok);
                }
            }
            A::Block(id) => {
                let Some(c) = find_contact(&self.cfg, &id).cloned() else { return };
                moderation::block(&mut self.cfg.blocked, &c.identity_key, &c.name, "", now_secs());
                if let Some(i) = contact_index(&self.cfg, &id) {
                    self.cfg.contacts.remove(i);
                }
                self.save();
                if self.call.peer.as_deref() == Some(id.as_str()) {
                    self.teardown("Blocked".into());
                }
                self.toast(format!("Blocked {}. They aren't told.", c.name), ToastKind::Ok);
            }
            A::Verify(id, on) => {
                if let Some(c) = contact_mut(&mut self.cfg, &id) {
                    c.verified = on;
                    if c.first_seen_secs == 0 {
                        c.first_seen_secs = now_secs();
                    }
                    let name = c.name.clone();
                    self.save();
                    if on {
                        self.toast(format!("{name} is verified"), ToastKind::Ok);
                    }
                }
            }
            A::Rename(id, name) => {
                if let Some(c) = contact_mut(&mut self.cfg, &id) {
                    c.name = name;
                    self.save();
                }
            }
            A::Note(id, note) => {
                if let Some(c) = contact_mut(&mut self.cfg, &id) {
                    c.note = note;
                    self.save();
                }
            }
            A::RefreshInvite => {
                let mut cfg = config::load();
                cfg.relay_invite_secret.clear();
                config::save(&cfg);
                self.cfg = cfg;
                self.invite.clear();
                self.refresh_invite();
            }
            A::Copy(text) => self.out.push(Out::Copy(text)),
            A::SetName(name) => {
                self.cfg.profile.display_name = name;
                self.profile_changed();
            }
            A::SetStatus(status) => {
                self.cfg.profile.status_message = status;
                self.profile_changed();
            }
            A::PickAvatar => self.pick_avatar(),
            A::AddNode(text) => match node_picker::parse(&text) {
                Some(node) => self.add_node(&node),
                None => self.toast("That isn't a node address (host:port) or node link", ToastKind::Err),
            },
            A::AddNodeFromQr => self.node_from_qr(),
            A::RemoveNode(address) => {
                node_picker::remove(&mut self.cfg.routes, &address);
                self.save_routes();
            }
            A::FindNodes => self.find_nodes(),
            A::RemoveAvatar => {
                self.cfg.profile.avatar.clear();
                self.profile_changed();
            }
            A::SetBio(bio) => {
                self.cfg.profile.bio = profile::clean_bio(&bio);
                self.profile_changed();
            }
            A::SetPronouns(pronouns) => {
                self.cfg.profile.pronouns = profile::clean_pronouns(&pronouns);
                self.profile_changed();
            }
            // Only this device's page changes, so nothing goes to anyone.
            A::SetProfileLayout(layout) => {
                self.cfg.profile.page_blocks = layout.saved_blocks();
                self.cfg.profile.page_qr = layout.qr.id().to_string();
                self.save();
            }
            A::OpenLink(url) => open_link(&url),
            A::ProfileColor(color) => {
                self.cfg.profile.color = color;
                self.profile_changed();
            }
            A::Theme(theme) => {
                self.cfg.stoop.theme = theme;
                self.save();
            }
            A::Accent(accent) => {
                self.cfg.stoop.accent = accent;
                self.save();
            }
            A::CustomAccent(hex) => {
                self.cfg.stoop.accent = "custom".into();
                self.cfg.stoop.custom_accent = hex;
                self.save();
            }
            A::Font(font) => {
                self.cfg.stoop.font = font;
                self.save();
            }
            A::ExportTheme => self.export_theme(),
            A::ImportTheme => self.import_theme(),
            // Moving the typing cursor is the window's job; the app does it.
            A::FocusInput(_) | A::RevealFocus(_) => {}
            A::Input(name) => {
                let device = (!name.is_empty()).then_some(name);
                if let Some(Err(e)) = self.with_call(|a| a.swap_input_device(device.as_deref())) {
                    self.toast(format!("Couldn't switch microphones: {e}"), ToastKind::Err);
                }
                self.cfg.input_device = device;
                self.save();
            }
            A::Output(name) => {
                let device = (!name.is_empty()).then_some(name);
                if let Some(Err(e)) = self.with_call(|a| a.swap_output_device(device.as_deref())) {
                    self.toast(format!("Couldn't switch speakers: {e}"), ToastKind::Err);
                }
                self.cfg.output_device = device;
                self.save();
            }
            A::Transmit(mode) => {
                self.cfg.transmit_mode = match mode {
                    ui::Transmit::PushToTalk => 0,
                    ui::Transmit::Voice => 1,
                    ui::Transmit::Open => 2,
                };
                self.sync_transmit();
                self.save();
            }
            A::BindPtt => self.bind_ptt(),
            A::NoiseSuppression(on) => {
                self.cfg.chain.noise_suppress.enabled = on;
                self.chain_changed();
            }
            A::EchoCancel(on) => {
                self.cfg.echo_cancel = on;
                if !on && self.state.echo_cancel.swap(false, Ordering::Relaxed) {
                    self.state.echo_canceller.lock().unwrap().reset();
                } else {
                    self.state.echo_cancel.store(on, Ordering::Relaxed);
                }
                self.save();
            }
            A::LowData(on) => {
                self.cfg.low_data_calls = on;
                antiphon_client::link_quality::set_low_data(on);
                self.save();
            }
            A::FindPlugins => self.find_plugins(),
            A::AddPlugin(i) => self.add_plugin(i),
            A::RemovePlugin(i) => {
                if i < self.cfg.chain.plugins.len() {
                    self.cfg.chain.plugins.remove(i);
                    self.chain_changed();
                }
            }
            A::PluginOn(i, on) => {
                if let Some(p) = self.cfg.chain.plugins.get_mut(i) {
                    p.enabled = on;
                    self.chain_changed();
                }
            }
            A::PluginEarlier(i) => {
                if i > 0 && i < self.cfg.chain.plugins.len() {
                    self.cfg.chain.plugins.swap(i - 1, i);
                    self.chain_changed();
                }
            }
            A::PluginParam(i, id, value) => {
                if let Some(p) = self.cfg.chain.plugins.get_mut(i) {
                    p.set_param(&id, value);
                    self.chain_changed();
                }
            }
            A::ScreenAudio(on) => {
                self.cfg.share_screen_audio = on;
                antiphon_client::screen_audio::set_enabled(on);
                self.save();
            }
            A::AutoGain(on) => {
                self.cfg.chain.gain = if on { GainConfig::default() } else { GainConfig::Fixed { enabled: false, gain_db: 0.0 } };
                self.chain_changed();
            }
            A::Preset(name) => {
                self.cfg.chain.eq_bands = if name == NATURAL { Vec::new() } else { antiphon_audio::eq_preset(&name).unwrap_or_default() };
                self.cfg.chain.name = name;
                self.chain_changed();
            }
            A::MicTest(on) => self.mic_test(on),
            A::OutputVolume(db) => {
                self.cfg.output_gain_db = db;
                *self.state.output_gain_db.lock().unwrap() = db;
                self.save();
            }
            A::ReducedMotion(on) => {
                self.cfg.reduced_motion = on;
                self.save();
            }
            A::NotifyMessages(on) => {
                self.cfg.stoop.notify_messages = on;
                self.save();
            }
            A::OpenOnStart(on) => {
                self.cfg.stoop.door_on_start = on;
                self.save();
            }
            A::LocalApi(on) => {
                self.cfg.local_api = on;
                self.save();
                self.set_api(on);
            }
            A::Anonymity(on) => {
                self.cfg.anonymity_mode = on;
                self.save();
                if on && self.cfg.anonymity_hops.len() < antiphon_net::onion::MIN_HOPS {
                    self.toast("Add at least two hops for anonymity mode to take effect", ToastKind::Warn);
                }
                self.relisten();
            }
            A::AnonymityHops(text) => {
                self.cfg.anonymity_hops = fallback_relay::parse_list(&text);
                self.save();
                self.relisten();
            }
            A::LendRelay(on) => {
                self.cfg.lend_relay = on;
                self.save();
                self.invite.clear();
                self.refresh_invite();
            }
            A::Relay(relay) => {
                self.cfg.relay_server = relay;
                self.relay_choice = None;
                self.save();
                self.choose_relay();
                self.invite.clear();
                self.refresh_invite();
                self.relisten();
            }
            A::Port(port) => {
                self.cfg.stoop.port = port;
                self.save();
                if let Some(h) = self.state.port_map.lock().unwrap().take() {
                    h.close(&self.state.runtime);
                }
                self.reach.port_forwarded = false;
                self.relisten();
                if self.door {
                    self.map_port();
                }
                self.invite.clear();
                self.refresh_invite();
            }
            A::CheckNetwork => self.check_network(),
            A::Welcome(name) => {
                self.cfg.profile.display_name = name;
                self.cfg.first_run.welcomed = true;
                self.save();
            }
            // The app handles updates itself (`App::update_action`), so these
            // never reach the engine.
            A::UpdateCheck | A::UpdateInstall | A::UpdateLater | A::UpdatePage | A::UpdateAuto(_) => {}
            A::OpenDownloads => {
                if let Some(dir) = dirs_downloads() {
                    open_folder(&dir);
                }
            }
            A::Camera(on) => self.set_video(Stream::Camera, on),
            A::ShareScreen(on) => self.set_video(Stream::Screen, on),
            // Only the phone shows the controls that send these.
            A::Speaker(_)
            | A::FlipCamera
            | A::Talk(_)
            | A::RequestMic
            | A::WifiOnly(_)
            | A::DataSaver(_)
            | A::CellularLimit(_)
            | A::KeepListening(_)
            | A::ListenOnSaver(_) => {}
            A::OpenChannel { community, channel } => self.viewing = Some((community, channel)),
            A::ManageSpace { community, change: ui::SpaceChange::JoinVoice(channel) } => self.join_voice(community, channel),
            A::ManageSpace { community, change: ui::SpaceChange::SubmitListing { directory, description } } => self.submit_listing(community, directory, Some(description)),
            A::ManageSpace { community, change: ui::SpaceChange::WithdrawListing(directory) } => self.submit_listing(community, directory, None),
            A::ManageSpace {
                community,
                change: change @ (ui::SpaceChange::StartShared(_) | ui::SpaceChange::JoinShared(_) | ui::SpaceChange::AskIntoShared(_) | ui::SpaceChange::UnlistShared(_) | ui::SpaceChange::NewAllyLink),
            } => {
                let (text, kind) = match self.spaces.shared(&community, change) {
                    Ok(said) => (said, ToastKind::Ok),
                    Err(e) => (e, ToastKind::Warn),
                };
                self.toast(text, kind);
                delivery::kick();
            }
            A::AddDirectory(address) => {
                if !self.cfg.directories.iter().any(|d| d.eq_ignore_ascii_case(&address)) {
                    self.cfg.directories.push(address);
                    self.save();
                }
            }
            A::RemoveDirectory(address) => {
                self.cfg.directories.retain(|d| *d != address);
                self.directory.listed.retain(|l| l.directory != address);
                self.save();
            }
            A::BrowseDirectories => self.browse_directories(),
            A::AskToJoin(link) => match community::Store::app().ok_or_else(|| "Your identity isn't loaded yet".to_string()).and_then(|s| s.knock(&link)) {
                Ok(()) => self.toast("Asked to join. Its invitation shows up under Invitations once its host's app answers.", ToastKind::Ok),
                Err(e) => self.toast(e, ToastKind::Warn),
            },
            A::ManageSpace { change: ui::SpaceChange::LeaveVoice(_), .. } => {
                if let Some(room) = self.voice.take() {
                    self.leave_voice(&room);
                    self.teardown("Left the voice channel".into());
                }
            }
            a @ (A::CreateSpace { .. }
            | A::AcceptSpaceInvite(_)
            | A::DeclineSpaceInvite(_)
            | A::Post { .. }
            | A::EditPost { .. }
            | A::DeletePost { .. }
            | A::ReactPost { .. }
            | A::InviteToSpace { .. }
            | A::AddChannel { .. }
            | A::SpaceNotify { .. }
            | A::RemoveEmoji { .. }
            | A::ManageSpace { .. }
            | A::LeaveSpace(_)) => {
                let invite = matches!(a, A::InviteToSpace { .. });
                match self.spaces.act(a) {
                    Ok(Some(id)) => self.out.push(Out::SpaceCreated(id)),
                    Ok(None) if invite => self.toast("Invitation sent. They join once they accept it.", ToastKind::Ok),
                    Ok(None) => {}
                    Err(why) => self.toast(why, ToastKind::Err),
                }
            }
            A::RevokeDevice(id) => match self.devices.as_mut().map(|d| d.revoke(&id)) {
                Some(Ok(name)) => self.toast(format!("{name} was removed from your account. It signs out when it next hears from your devices."), ToastKind::Ok),
                Some(Err(e)) => self.toast(e, ToastKind::Err),
                None => self.toast("Your device list didn't load on this computer.", ToastKind::Err),
            },
            a @ (A::ShowPhrase(_)
            | A::Restore(_)
            | A::BackupSave { .. }
            | A::BackupOpen { .. }
            | A::MoveStart { .. }
            | A::MoveStop
            | A::MoveReceive(_)) => {
                let in_call = self.busy();
                let device = self.devices.as_ref().map(|d| d.device());
                let ctx = crate::account::Ctx { dir: self.dir.as_deref(), identity: &self.state.identity, key: self.key.as_ref(), device, in_call };
                let outcomes = self.account.act(a, ctx);
                self.account_outcomes(outcomes);
            }
        }
        let _ = open;
    }

    /// Turns the camera or the screen share on or off. Opening a device
    /// can block -- a camera warming up, macOS asking permission, Wayland
    /// asking which screen -- so it happens off the interface's thread and
    /// the stream starts on the call when it is ready.
    fn set_video(&mut self, stream: Stream, on: bool) {
        if !on {
            self.with_call(|a| a.video.stop(stream));
            return;
        }
        if self.state.call.lock().unwrap().is_none() {
            self.toast("Video starts once you're on a call", ToastKind::Info);
            return;
        }
        let (state, events) = (self.state.clone(), self.events.clone());
        self.state.runtime.spawn_blocking(move || {
            let opened = match stream {
                Stream::Camera => video_capture::open_camera(),
                Stream::Screen => video_capture::open_screen(),
            };
            match opened {
                Ok(source) => {
                    let call = state.call.lock().unwrap();
                    // The call may have ended while the device opened;
                    // the source is then dropped, and the device let go.
                    if let Some(a) = call.as_ref() {
                        a.video.start(stream, source, a.peer.clone(), state.runtime.clone());
                    }
                }
                Err(why) => Engine::push(&events, Event::Toast(why, ToastKind::Err)),
            }
        });
    }

    /// Restarts listening so a changed port or relay takes effect.
    fn relisten(&mut self) {
        if self.task.as_ref().is_some_and(|(k, _)| *k == TaskKind::Listen) {
            self.stop_task();
        }
    }

    /// Applies the edited voice chain to a running call and saves it.
    /// Set up for running with no window: the API on and the door open, so
    /// the bot can be reached and driven. Says where the API's details are.
    pub fn headless(&mut self) {
        self.set_api(true);
        self.set_door(true);
        match (&self.api, &self.dir) {
            (Some(_), Some(dir)) => println!("Local API on; port and key in {}", dir.join("api.json").display()),
            _ => println!("The local API didn't start"),
        }
    }

    /// Starts or stops the local API (see `api.rs`).
    fn set_api(&mut self, on: bool) {
        if !on {
            self.api = None;
            self.api_listeners.clear();
            return;
        }
        if self.api.is_some() {
            return;
        }
        let Some(dir) = self.dir.clone() else { return };
        match crate::api::start(&dir, &self.state.runtime) {
            Ok(api) => self.api = Some(api),
            Err(e) => self.toast(format!("The local API couldn't start: {e}"), ToastKind::Err),
        }
    }

    /// Answers waiting API commands and sends events to subscribers.
    fn serve_api(&mut self, open: Option<&str>) {
        let mut requests = Vec::new();
        if let Some(api) = &mut self.api {
            while let Ok(r) = api.requests.try_recv() {
                requests.push(r);
            }
        }
        for r in requests {
            let answer = self.api_call(&r.body, open);
            if r.body.get("cmd").and_then(|c| c.as_str()) == Some("subscribe") && answer.is_ok() {
                self.api_listeners.push(r.events);
            }
            let _ = r.reply.send(answer);
        }
        if self.api_listeners.is_empty() {
            self.api_seen = self.previews.clone();
            return;
        }
        let mut events = Vec::new();
        let changed: Vec<String> =
            self.previews.iter().filter(|(id, p)| self.api_seen.get(*id) != Some(p)).map(|(id, _)| id.clone()).collect();
        for id in changed {
            if self.last_from_them(&id) {
                let (Some(dir), Some(key)) = (&self.dir, &self.key) else { break };
                if let Some(m) = history::load(dir, &id, key).last() {
                    let name = self.cfg.contacts.iter().find(|c| c.identity_key == id).map(|c| c.name.clone()).unwrap_or_default();
                    events.push(serde_json::json!({ "event": "message", "from": id, "name": name, "text": m.text, "at": m.at_secs }));
                }
            }
        }
        self.api_seen = self.previews.clone();
        let phase = format!("{:?}", self.call.phase);
        if phase != self.api_phase {
            events.push(serde_json::json!({ "event": "call", "phase": api_phase(&self.call.phase), "with": self.call.peer }));
            self.api_phase = phase;
        }
        for e in events {
            self.api_listeners.retain(|l| l.send(e.clone()).is_ok());
        }
    }

    /// One API command, done the way the matching button does it.
    fn api_call(&mut self, body: &serde_json::Value, open: Option<&str>) -> Result<serde_json::Value, String> {
        use crate::api::arg;
        use serde_json::json;
        let known = |e: &Self, id: &str| e.cfg.contacts.iter().any(|c| c.identity_key == id);
        match arg(body, "cmd")? {
            "me" => {
                let me = self.me();
                Ok(json!({ "id": me.id, "name": me.name, "status": me.status, "invite": me.invite, "door_open": me.door_open }))
            }
            "contacts" => Ok(self
                .people()
                .iter()
                .map(|p| {
                    json!({
                        "id": p.id, "name": p.name, "online": p.presence == ui::Presence::Online,
                        "verified": p.verified, "unread": p.unread, "last": p.last,
                    })
                })
                .collect()),
            "messages" => {
                let with = arg(body, "with")?;
                if !known(self, with) {
                    return Err("no contact with that id".into());
                }
                let limit = body.get("limit").and_then(serde_json::Value::as_u64).unwrap_or(50) as usize;
                let lines = self.stored_lines(with);
                let start = lines.len().saturating_sub(limit);
                Ok(lines[start..]
                    .iter()
                    .filter(|l| !l.deleted)
                    .map(|l| json!({ "id": l.id, "mine": l.mine, "author": l.author, "text": l.text, "time": l.time, "edited": l.edited }))
                    .collect())
            }
            "send" => {
                let (to, text) = (arg(body, "to")?.to_string(), arg(body, "text")?.to_string());
                if !known(self, &to) {
                    return Err("no contact with that id".into());
                }
                if text.trim().is_empty() {
                    return Err("nothing to send".into());
                }
                self.act(ui::Action::Send { to, text, reply_to: None }, open);
                Ok(serde_json::Value::Null)
            }
            "call" => {
                let to = arg(body, "to")?.to_string();
                if !known(self, &to) {
                    return Err("no contact with that id".into());
                }
                self.act(ui::Action::Call(to), open);
                Ok(serde_json::Value::Null)
            }
            "hangup" => {
                self.act(ui::Action::Hangup, open);
                Ok(serde_json::Value::Null)
            }
            "mute" => {
                let on = body.get("on").and_then(serde_json::Value::as_bool).ok_or("'on' is missing")?;
                self.act(ui::Action::Mute(on), open);
                Ok(serde_json::Value::Null)
            }
            "door" => {
                let on = body.get("open").and_then(serde_json::Value::as_bool).ok_or("'open' is missing")?;
                self.act(ui::Action::Door(on), open);
                Ok(serde_json::Value::Null)
            }
            "status" => Ok(json!({ "phase": api_phase(&self.call.phase), "with": self.call.peer, "name": self.call.name, "code": self.call.code })),
            "subscribe" => Ok(serde_json::Value::Null),
            other => Err(format!("unknown command '{other}'")),
        }
    }

    /// Looks for installed plugins in the background (it opens every
    /// plugin library, which can take a while).
    fn find_plugins(&mut self) {
        if self.plugins_scanning {
            return;
        }
        self.plugins_scanning = true;
        let events = self.events.clone();
        self.state.runtime.spawn_blocking(move || {
            Self::push(&events, Event::PluginsFound(antiphon_audio::plugins::scan()));
        });
    }

    /// Adds found plugin `i` to the end of the chain, if it starts.
    fn add_plugin(&mut self, i: usize) {
        let Some(info) = self.plugins_found.as_ref().and_then(|f| f.get(i)) else { return };
        let config = info.to_config();
        // Tried once here, so a plugin that won't start says so now rather
        // than leaving the chain silently without it.
        if let Err(e) = antiphon_audio::plugins::load(&config, 48_000) {
            self.toast(format!("{} couldn't start: {e}", info.name), ToastKind::Err);
            return;
        }
        self.toast(format!("{} added", info.name), ToastKind::Ok);
        self.cfg.chain.plugins.push(config);
        self.chain_changed();
    }

    /// The plugin list as Settings shows it.
    fn plugins_model(&self) -> ui::Plugins {
        let chain = self
            .cfg
            .chain
            .plugins
            .iter()
            .map(|p| ui::PluginRow {
                name: p.name.clone(),
                format: p.format.label().to_string(),
                enabled: p.enabled,
                problem: (!p.path.exists()).then(|| format!("{} isn't installed any more", p.path.display())),
                params: p
                    .described
                    .iter()
                    .map(|d| ui::PluginParam {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        min: d.min,
                        max: d.max,
                        value: p.param(&d.id).unwrap_or(d.default),
                        toggle: d.toggle,
                    })
                    .collect(),
            })
            .collect();
        let found = self.plugins_found.as_ref().map(|f| f.iter().map(|p| format!("{} · {}", p.name, p.format.label())).collect());
        ui::Plugins { chain, found, scanning: self.plugins_scanning }
    }

    fn chain_changed(&mut self) {
        let chain = self.cfg.chain.clone();
        self.with_call(|a| a.apply_chain_config(chain));
        self.save();
    }

    /// Copies the saved transmit mode and push-to-talk key into the live state.
    fn sync_transmit(&self) {
        let t = &self.state.transmit;
        let mode = transmit_mode(self.cfg.transmit_mode);
        t.transmit_mode.store(mode, Ordering::Relaxed);
        if let Ok(key) = Keycode::from_str(&self.cfg.ptt_key) {
            *t.ptt_key.lock().unwrap() = key;
        }
        caps_lock::set_bound(*t.ptt_key.lock().unwrap() == Keycode::CapsLock && mode == MODE_PTT);
    }

    /// Waits, in the background, for the next key press and makes it the push-to-talk key.
    fn bind_ptt(&mut self) {
        if self.ptt_capturing {
            return;
        }
        self.ptt_capturing = true;
        let events = self.events.clone();
        self.state.runtime.spawn_blocking(move || {
            let r = match state::capture_next_key(Duration::from_secs(6)) {
                None => Err("No key came through. Another program may have it.".to_string()),
                Some(key) => {
                    let name = key.to_string();
                    key_capture::check(&name).map(|_| name.clone()).map_err(|why| why.explain(&name))
                }
            };
            Self::push(&events, Event::Key(r));
        });
    }

    /// Starts or stops hearing your own microphone.
    fn mic_test(&mut self, on: bool) {
        let mut loopback = self.state.loopback.lock().unwrap();
        if !on {
            *loopback = None;
            return;
        }
        if self.state.call.lock().unwrap().is_some() {
            drop(loopback);
            self.toast("Not while a call is using your microphone", ToastKind::Warn);
            return;
        }
        match state::start_loopback(self.cfg.input_device.as_deref(), self.cfg.output_device.as_deref(), &self.cfg.chain) {
            Ok(handle) => *loopback = Some(handle),
            Err(e) => {
                drop(loopback);
                self.toast(format!("Couldn't start the test: {e}"), ToastKind::Err);
            }
        }
    }

    // ---------------------------------------------------------------- voice messages

    /// Starts recording a voice message for contact `to`, until
    /// [`Engine::finish_voice`] or the length cap.
    fn record_voice(&mut self, to: String) {
        if self.recording.is_some() {
            return;
        }
        let (stop, send) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (t_stop, t_send) = (stop.clone(), send.clone());
        let (input, chain, rt, events, who) = (self.cfg.input_device.clone(), self.cfg.chain.clone(), self.state.runtime.clone(), self.events.clone(), to.clone());
        std::thread::spawn(move || {
            let (rate, mut samples) = match state::record_until(input.as_deref(), t_stop, voice_message::MAX_SECONDS) {
                Ok(recorded) => recorded,
                Err(e) => {
                    Engine::push(&events, Event::VoiceSent(Err(format!("Couldn't record: {e}"))));
                    return;
                }
            };
            if !t_send.load(Ordering::SeqCst) {
                return;
            }
            // Through the same chain as a call, so it sounds like you do
            // live, then without the silence before and after.
            voice_message::process_recording(&mut samples, rate, &chain);
            let clip = voice_message::trim_silence(&samples, 0.01).to_vec();
            let sent = voice_message::leave(&rt, &who, &clip, rate).map(|_| ());
            Engine::push(&events, Event::VoiceSent(sent));
        });
        self.recording = Some(Recording { to, stop, send, started: Instant::now() });
    }

    /// Stops the recording for `to`: sends it, or throws it away.
    fn finish_voice(&mut self, to: &str, send: bool) {
        let Some(r) = self.recording.take_if(|r| r.to == to) else { return };
        r.send.store(send, Ordering::SeqCst);
        r.stop.store(true, Ordering::SeqCst);
    }

    /// Plays a voice message, or stops it if it is the one playing.
    fn play_voice(&mut self, to: String, id: u64) {
        if self.playing.as_ref().is_some_and(|(p, i, ..)| *p == to && *i == id) {
            self.playing = None;
            return;
        }
        let Some((link, mine)) = self.voice_links.get(&(to.clone(), id)).cloned() else { return };
        match load_clip(&link, mine) {
            Some(samples) => self.start_clip(to, id, &samples),
            None if mine => self.toast("That recording isn't on this device any more", ToastKind::Warn),
            None => {
                if self.voice_fetched.insert(link.file_id.clone()) {
                    voice_message::fetch(&self.state.runtime, &link, &to);
                }
                self.voice_waiting = Some((to, id, Instant::now()));
            }
        }
    }

    /// Plays decoded samples through the chosen speakers.
    fn start_clip(&mut self, to: String, id: u64, samples: &[f32]) {
        match state::play_clip(self.cfg.output_device.as_deref(), CLIP_RATE, samples) {
            Ok(handle) => {
                let ends = Instant::now() + Duration::from_secs_f32(voice_message::duration_secs(samples, CLIP_RATE) + 0.3);
                self.playing = Some((to, id, handle, ends));
            }
            Err(e) => self.toast(format!("Couldn't play it: {e}"), ToastKind::Err),
        }
    }

    /// Per-tick upkeep: the length cap on a recording, a clip that finished
    /// playing, and a clip waited for that may have arrived.
    fn voice_upkeep(&mut self) {
        if let Some(r) = &self.recording {
            if r.started.elapsed() >= Duration::from_secs(voice_message::MAX_SECONDS as u64) {
                let to = r.to.clone();
                self.finish_voice(&to, true);
            }
        }
        if self.playing.as_ref().is_some_and(|(.., ends)| Instant::now() >= *ends) {
            self.playing = None;
        }
        // Looked for once a second: decoding is not free.
        let Some((to, id, last)) = self.voice_waiting.clone() else { return };
        if last.elapsed() < Duration::from_secs(1) {
            return;
        }
        match self.voice_links.get(&(to.clone(), id)).cloned().and_then(|(link, mine)| load_clip(&link, mine)) {
            Some(samples) => {
                self.voice_waiting = None;
                self.start_clip(to, id, &samples);
            }
            None => self.voice_waiting = Some((to, id, Instant::now())),
        }
    }

    /// Turns the voice messages among `lines` into players: remembers each
    /// clip's link, starts fetching received ones, and shows a short line
    /// instead of the link's text.
    fn find_voice(&mut self, person: &str, lines: &mut [ui::Line]) {
        for line in lines.iter_mut() {
            let Some(link) = voice_message::parse_link(&line.text) else { continue };
            line.text = format!("Voice message ({}:{:02})", link.seconds / 60, link.seconds % 60);
            line.voice = Some(ui::VoiceClip { seconds: link.seconds, ..Default::default() });
            // Fetched as it comes on screen, so it is ready by the time
            // it is pressed.
            if !line.mine && self.voice_fetched.insert(link.file_id.clone()) {
                voice_message::fetch(&self.state.runtime, &link, person);
            }
            self.voice_links.insert((person.to_string(), line.id), (link, line.mine));
        }
    }

    /// Asks for files to send to the person on the call. They join the
    /// queue, which sends them one at a time.
    fn send_file(&mut self) {
        let Some(peer) = self.with_call(|a| delivery::hex(&a.peer_identity().identity_key)) else {
            self.toast("Files go to the person you're on a call with", ToastKind::Info);
            return;
        };
        let events = self.events.clone();
        std::thread::spawn(move || {
            let Some(paths) = rfd::FileDialog::new().set_title("Send files").pick_files() else { return };
            if !paths.is_empty() {
                Self::push(&events, Event::Queue(paths, peer));
            }
        });
    }

    /// Starts the next queued file for the person on the call, once
    /// nothing else is going out, then saves the queue if that or anything
    /// else changed it. Called whenever the queue or the call may have
    /// moved on, which includes every tick.
    fn pump_queue(&mut self) {
        self.start_next_queued();
        self.save_queue();
    }

    /// Writes the queue, with the file being sent at its front, to
    /// [`QUEUE_FILE`] if it changed since the last write. A failure is
    /// logged and tried again at the next change or tick.
    fn save_queue(&mut self) {
        let Some(dir) = &self.dir else { return };
        let now: Vec<(QueuedSend, bool)> =
            self.sending.iter().map(|(e, _)| e).chain(self.send_queue.iter()).map(|e| (e.item.clone(), e.held)).collect();
        if now == self.queue_saved {
            return;
        }
        match transfer_queue::save(&dir.join(QUEUE_FILE), &now) {
            Ok(()) => self.queue_saved = now,
            Err(e) => eprintln!("[queue] could not save the send queue: {e}"),
        }
    }

    /// [`pump_queue`](Self::pump_queue)'s first half: retires a finished
    /// send and starts the next one that fits the call.
    fn start_next_queued(&mut self) {
        if let Some((_, flag)) = &self.sending {
            if !flag.load(Ordering::SeqCst) {
                return;
            }
            self.sending = None;
        }
        let Some((peer, busy)) = self.with_call(|a| {
            // A send started by hand (or an older one still finishing)
            // keeps the link; the queue waits behind it.
            let ft = a.file_transfer.lock().unwrap();
            let busy = ft.outgoing.as_ref().is_some_and(|o| !o.done && o.error.is_none());
            (delivery::hex(&a.peer_identity().identity_key), busy)
        }) else {
            return;
        };
        if busy {
            return;
        }
        let Some(entry) = self.send_queue.take_next(|q| q.peer == peer) else { return };
        let flag = Arc::new(AtomicBool::new(false));
        self.sending = Some((entry.clone(), flag.clone()));
        let (state, events) = (self.state.clone(), self.events.clone());
        // Reading and hashing a large file takes a while: off the window's
        // thread, and off the call's runtime.
        std::thread::spawn(move || {
            let QueuedSend { path, name, peer, .. } = entry.item;
            let (source, sha256) = match file_integrity::open_and_hash(&path) {
                Ok(opened) => opened,
                Err(reason) => {
                    Self::push(&events, Event::Toast(reason.explain(&name), ToastKind::Err));
                    flag.store(true, Ordering::SeqCst);
                    return;
                }
            };
            let thumbnail = file_preview::generate(&path).map(|png| file_transfer::encode_b64(&png)).unwrap_or_default();
            {
                let call = state.call.lock().unwrap();
                // The call ended, or is now with someone else, while the
                // file was read: it waits for next time.
                let Some(active) = call.as_ref().filter(|a| delivery::hex(&a.peer_identity().identity_key) == peer) else {
                    flag.store(true, Ordering::SeqCst);
                    return;
                };
                let (link, ft, done) = (active.peer.clone(), active.file_transfer.clone(), flag.clone());
                let name = name.clone();
                state.runtime.spawn(async move {
                    file_transfer::send_file(link, ft, name, Arc::new(source), sha256, thumbnail).await;
                    done.store(true, Ordering::SeqCst);
                });
            }
            if let Some(link) = sharing::share(&state.runtime, &path, &name, &peer) {
                if let Some(active) = state.call.lock().unwrap().as_ref() {
                    active.send_control(&state.runtime, link);
                }
            }
        });
    }

    /// Adds a contact from an invite link typed or pasted as `text`.
    fn add_person(&mut self, name: String, text: String) {
        let invite = match antiphon_crypto::Invite::parse(text.trim()) {
            Ok(invite) => invite,
            Err(_) => {
                self.out.push(Out::AddFailed("That isn't an invite link. Ask them to copy it again from their Invite button.".into()));
                return;
            }
        };
        let key = connection_mode::encode_hex32(&invite.identity_key);
        if key == connection_mode::encode_hex32(&self.state.identity.public().identity_key) {
            self.out.push(Out::AddFailed("That's your own invite.".into()));
            return;
        }
        let secret = invite.relay_secret.as_ref().map(connection_mode::encode_hex32).unwrap_or_default();
        if let Some(existing) = self.cfg.contacts.iter_mut().find(|c| c.identity_key == key) {
            if !invite.address.is_empty() {
                existing.address = invite.address.clone();
            }
            if !invite.relay.is_empty() {
                existing.relay = invite.relay.clone();
                existing.relay_secret = secret;
                existing.lends_relay = invite.lends_relay;
            }
            let who = existing.name.clone();
            self.save();
            self.toast(format!("{who} was already here; their details are up to date"), ToastKind::Info);
            self.out.push(Out::Added(key));
            return;
        }
        let name = if name.trim().is_empty() { format!("Friend {}", &key[..6]) } else { name.trim().to_string() };
        self.cfg.contacts.push(config::Contact {
            name: name.clone(),
            address: invite.address.clone(),
            identity_key: key.clone(),
            first_seen_secs: now_secs(),
            relay: invite.relay.clone(),
            relay_secret: secret,
            lends_relay: invite.lends_relay,
            ..Default::default()
        });
        self.save();
        self.toast(format!("Added {name}. Compare the words on your first call."), ToastKind::Ok);
        self.out.push(Out::Added(key));
    }

    // ---------------------------------------------------------------- model

    /// Everything the screens draw, now.
    /// Tells the engine which channel is on screen, before [`Engine::model`].
    pub fn set_viewing(&mut self, channel: Option<(&str, u32)>) {
        self.viewing = channel.map(|(id, ch)| (id.to_string(), ch));
    }

    /// Everything the screens draw, built from the engine's state.
    pub fn model(&mut self, open: Option<&str>) -> ui::Model {
        let viewing = self.viewing.clone();
        let (communities, space_invites, channel) = self.spaces.model(viewing.as_ref().map(|(id, ch)| (id.as_str(), *ch)));
        let me = self.me();
        let people = self.people();
        let notes = self.notes();
        let mut call = self.call_model();
        call.video = self.video_tiles();
        let conversation = open.and_then(|id| self.conversation(id));
        let mut account = self.account.model(&self.state.identity);
        if let Some(d) = &self.devices {
            account.devices = d.rows();
            account.device_sync = crate::my_devices::MyDevices::sync_line(&self.cfg);
        }
        ui::Model {
            me,
            people,
            notes,
            call,
            conversation,
            transfers: self.transfers(),
            network: self.network(),
            settings: self.settings(),
            notice: None,
            // A fresh install's name is the placeholder "You", not empty,
            // so first run is also anyone never welcomed who still has it.
            first_run: {
                let name = self.cfg.profile.display_name.trim();
                name.is_empty() || (!self.cfg.first_run.welcomed && name == config::ProfileData::default().display_name)
            },
            device: ui::Device { phone: false, files: true, ptt_keys: true, communities: true, voice_messages: true },
            phone: None,
            account,
            communities,
            space_invites,
            channel,
            directory: ui::Directory { directories: self.cfg.directories.clone(), ..self.directory.clone() },
            // The app's updater fills this in (`App::stamp_update`).
            update: ui::Update::default(),
        }
    }

    /// Sends a note change made here to the account's other devices, when
    /// `to` is the notes conversation.
    fn share_note(&self, to: &str, op: NoteOp) {
        if let Some(d) = self.devices.as_ref().filter(|d| d.notes_id() == to) {
            d.share(op);
        }
    }

    /// Carries out what the account section asked for.
    fn account_outcomes(&mut self, outcomes: Vec<crate::account::Outcome>) {
        for o in outcomes {
            match o {
                crate::account::Outcome::Toast(text, kind) => self.toast(text, kind),
                crate::account::Outcome::Relaunch => self.out.push(Out::Relaunch),
                crate::account::Outcome::DevicesChanged => {
                    if let Some(d) = self.devices.as_mut() {
                        d.reload();
                    }
                }
            }
        }
    }

    /// Your card: name, status, invite, addresses and greeting.
    fn me(&self) -> ui::Me {
        let key = self.state.identity.public().identity_key;
        let hour = chrono::Timelike::hour(&chrono::Local::now());
        let greeting = match hour {
            5..=11 => "Good morning",
            12..=17 => "Good afternoon",
            18..=22 => "Good evening",
            _ => "Up late",
        };
        let name = self.cfg.profile.display_name.clone();
        let id = connection_mode::encode_hex32(&key);
        let p = &self.cfg.profile;
        ui::avatars::set(&id, &p.avatar, &p.color, profile::decode_avatar);
        ui::Me {
            greeting: greeting.to_string(),
            name,
            status: self.cfg.profile.status_message.clone(),
            bio: self.cfg.profile.bio.clone(),
            pronouns: self.cfg.profile.pronouns.clone(),
            layout: ui::ProfileLayout::from_saved(&self.cfg.profile.page_blocks, &self.cfg.profile.page_qr),
            color: self.cfg.profile.color.clone(),
            has_picture: !self.cfg.profile.avatar.is_empty(),
            id,
            fingerprint: antiphon_crypto::fingerprint(&key),
            invite: self.invite.clone(),
            qr: self.qr.clone(),
            door_open: self.door,
            local_addr: local_ip().map(|ip| format!("{ip}:{}", self.cfg.stoop.port)),
            public_addr: self.public.clone(),
        }
    }

    /// Every contact as the interface shows them, most recent first.
    fn people(&self) -> Vec<ui::Person> {
        let mut people: Vec<ui::Person> = self
            .cfg
            .contacts
            .iter()
            .map(|c| {
                let id = contact_id(c);
                let (last, last_at) = match self.previews.get(&id) {
                    Some((text, at)) => (Some(text.clone()), Some(short_time(*at))),
                    None => (None, None),
                };
                let presence = if self.call_with(&id) { ui::Presence::Online } else { ui::Presence::Unknown };
                ui::avatars::set(&id, &c.avatar, &c.color, profile::decode_avatar);
                ui::Person {
                    name: c.name.clone(),
                    address: c.address.clone(),
                    verified: c.verified,
                    presence,
                    last,
                    last_at,
                    unread: self.unread.get(&id).copied().unwrap_or(0),
                    fingerprint: delivery::unhex32(&c.identity_key).map(|k| antiphon_crypto::fingerprint(&k)).unwrap_or_default(),
                    note: c.note.clone(),
                    ring_rule: match notify::RingRule::from_i32(c.ring_rule) {
                        notify::RingRule::Always => "Rings out loud",
                        notify::RingRule::SilentOnly => "Quietly",
                        notify::RingRule::Never => "Never",
                    }
                    .into(),
                    route: match connection_mode::Override::from_i32(c.connection_mode) {
                        connection_mode::Override::Auto => "Automatic",
                        connection_mode::Override::Direct => "Direct only",
                        connection_mode::Override::Relay => "Through a relay",
                    }
                    .into(),
                    volume_db: c.volume_db,
                    muted: c.muted,
                    pronouns: c.pronouns.clone(),
                    bio: c.bio.clone(),
                    note_to_self: false,
                    id,
                }
            })
            .collect();
        // Most recent conversation first, then by name.
        people.sort_by(|a, b| {
            let at = |p: &ui::Person| self.previews.get(&p.id).map(|(_, t)| *t).unwrap_or(0);
            at(b).cmp(&at(a)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        people
    }

    /// Notes to self as the list shows them, when this computer keeps them.
    fn notes(&self) -> Option<ui::Person> {
        let id = self.devices.as_ref()?.notes_id();
        let (last, last_at) = match self.previews.get(&id) {
            Some((text, at)) => (Some(text.clone()), Some(short_time(*at))),
            None => (None, None),
        };
        Some(ui::Person { name: crate::my_devices::NOTES_NAME.into(), last, last_at, note_to_self: true, id, ..Default::default() })
    }

    /// Whether `id` is the notes-to-self conversation.
    fn is_notes(&self, id: &str) -> bool {
        self.devices.as_ref().is_some_and(|d| d.notes_id() == id)
    }

    /// The call as the interface shows it.
    fn call_model(&self) -> ui::Call {
        let c = &self.call;
        let mut m = ui::Call {
            phase: c.phase.clone(),
            peer: c.peer.clone(),
            peer_name: c.name.clone(),
            code: c.code.clone(),
            mode: c.mode.clone(),
            mode_detail: c.mode_detail.clone(),
            warning: c.warning.clone(),
            peer_volume_db: *self.state.peer_volume_db.lock().unwrap(),
            ..ui::Call::default()
        };
        if c.phase == ui::Phase::Dialing {
            m.talk_hint = c.status.clone();
        }
        let call = self.state.call.lock().unwrap();
        if let Some(a) = call.as_ref() {
            m.seconds = a.started_at.elapsed().as_secs();
            m.camera_on = a.video.sending(Stream::Camera);
            m.sharing = a.video.sending(Stream::Screen);
            m.can_camera = true;
            m.can_share = true;
            m.muted = a.muted.load(Ordering::Relaxed);
            m.deafened = a.deafened.load(Ordering::Relaxed);
            m.my_level = a.mic_level.lock().map(|l| *l).unwrap_or(0.0).min(1.0);
            m.sending = a.ptt_active.load(Ordering::Relaxed);
            m.their_level = (self.their_level * 2.5).min(1.0);
            let (delivered, concealed) = a.jitter.lock().unwrap().stats();
            let total = delivered + concealed;
            m.quality = Some(if total < 50 {
                "Settling".to_string()
            } else {
                let loss = concealed as f64 * 100.0 / total as f64;
                if loss < 2.0 { "Clear".to_string() } else { format!("{loss:.0}% lost") }
            });
            m.talk_hint = Some(match transmit_mode(self.cfg.transmit_mode) {
                MODE_PTT => format!("Hold {} to talk", key_label(&self.cfg.ptt_key)),
                MODE_OPEN_MIC => "Mic always on".into(),
                _ => if m.sending { "Talking".into() } else { "Speak to talk".into() },
            });
        }
        m
    }

    /// The call's pictures: theirs, then my previews. A tile whose picture
    /// has not changed since the last frame is reused as it was, so iced
    /// keeps the texture it already has.
    fn video_tiles(&mut self) -> Vec<ui::VideoTile> {
        let state = self.state.clone();
        let call = state.call.lock().unwrap();
        let Some(a) = call.as_ref() else {
            self.tiles.clear();
            return Vec::new();
        };
        let first = self.call.name.split_whitespace().next().unwrap_or("Their").to_string();
        let mut tiles = Vec::new();
        for (stream, version) in a.video.remote_versions() {
            let (key, label) = match stream {
                Stream::Camera => ("their-camera", first.clone()),
                Stream::Screen => ("their-screen", format!("{first}'s screen")),
            };
            let tile = self.tile(key, version, || {
                let p = a.video.remote_picture(stream)?;
                Some(ui::VideoTile {
                    key: key.into(),
                    label,
                    image: iced::widget::image::Handle::from_rgba(p.width, p.height, p.rgba),
                    width: p.width,
                    height: p.height,
                    mine: false,
                    screen: stream == Stream::Screen,
                })
            });
            tiles.extend(tile);
        }
        // Everyone else in a group call, a tile per picture they send.
        for (leg, stream, version) in a.video.leg_versions() {
            let who = a.group.leg_name(leg).unwrap_or_else(|| "Someone".into());
            let (key, label) = match stream {
                Stream::Camera => (format!("leg-{leg}-camera"), who),
                Stream::Screen => (format!("leg-{leg}-screen"), format!("{who}'s screen")),
            };
            let tile = self.tile(&key, version, || {
                let p = a.video.leg_picture(leg, stream)?;
                Some(ui::VideoTile {
                    key: key.clone(),
                    label,
                    image: iced::widget::image::Handle::from_rgba(p.width, p.height, p.rgba),
                    width: p.width,
                    height: p.height,
                    mine: false,
                    screen: stream == Stream::Screen,
                })
            });
            tiles.extend(tile);
        }
        for stream in [Stream::Camera, Stream::Screen] {
            let Some((n, frame)) = a.video.preview(stream) else { continue };
            let (key, label) = match stream {
                Stream::Camera => ("my-camera", "You"),
                Stream::Screen => ("my-screen", "Your screen"),
            };
            let tile = self.tile(key, n, || {
                Some(ui::VideoTile {
                    key: key.into(),
                    label: label.into(),
                    image: iced::widget::image::Handle::from_rgba(frame.width, frame.height, frame.rgba.clone()),
                    width: frame.width,
                    height: frame.height,
                    mine: true,
                    screen: stream == Stream::Screen,
                })
            });
            tiles.extend(tile);
        }
        self.tiles.retain(|k, _| tiles.iter().any(|t| &t.key == k));
        tiles
    }

    /// The tile `key` at `version`: the cached one if it is that version,
    /// else a fresh one from `build`, cached for next time.
    fn tile(&mut self, key: &str, version: u64, build: impl FnOnce() -> Option<ui::VideoTile>) -> Option<ui::VideoTile> {
        if let Some((v, tile)) = self.tiles.get(key) {
            if *v == version {
                return Some(tile.clone());
            }
        }
        let tile = build()?;
        self.tiles.insert(key.into(), (version, tile.clone()));
        Some(tile)
    }

    /// Files on their way in either direction.
    fn transfers(&self) -> ui::Transfers {
        let mut t = ui::Transfers::default();
        let call = self.state.call.lock().unwrap();
        let Some(a) = call.as_ref() else {
            drop(call);
            t.queued = self.queued_files(None);
            return t;
        };
        let ft = a.file_transfer.lock().unwrap();
        if let Some(offer) = ft.pending_offer.as_ref() {
            t.offer = Some((offer.file_name.clone(), human_size(offer.file_size)));
        }
        if let Some(o) = ft.outgoing.as_ref() {
            let progress = if o.total_chunks == 0 { 0.0 } else { o.chunks_sent as f32 / o.total_chunks as f32 };
            let detail = match (&o.error, o.done, o.accepted) {
                (Some(e), _, _) => e.clone(),
                (None, true, _) => "Sent, and they have it".into(),
                (None, false, false) => format!("{} · waiting for them to accept", human_size(o.file_size)),
                _ => format!("{} of {}", human_size((o.file_size as f64 * progress as f64) as u64), human_size(o.file_size)),
            };
            t.active.push(ui::Transfer { name: o.file_name.clone(), incoming: false, progress, detail, paused: o.paused });
        }
        if let Some(i) = ft.incoming.as_ref() {
            let progress = if i.total_chunks == 0 { 0.0 } else { i.chunks_received as f32 / i.total_chunks as f32 };
            let detail = match (&i.error, i.verifying) {
                (Some(e), _) => e.clone(),
                (None, true) => "Checking it arrived intact".into(),
                _ => format!("{} of {}", human_size((i.file_size as f64 * progress as f64) as u64), human_size(i.file_size)),
            };
            t.active.push(ui::Transfer { name: i.file_name.clone(), incoming: true, progress, detail, paused: false });
        }
        drop(ft);
        let on_call_with = delivery::hex(&a.peer_identity().identity_key);
        drop(call);
        t.queued = self.queued_files(Some(&on_call_with));
        t
    }

    /// The send queue as the screens show it. Files for someone other
    /// than `on_call_with` say who they wait for.
    fn queued_files(&self, on_call_with: Option<&str>) -> Vec<ui::QueuedFile> {
        self.send_queue
            .iter()
            .map(|e| {
                let q = &e.item;
                let detail = if Some(q.peer.as_str()) == on_call_with {
                    human_size(q.size)
                } else {
                    let who = find_contact(&self.cfg, &q.peer).map(|c| c.name.clone()).unwrap_or_else(|| "them".into());
                    format!("{} · for {who}, on your next call", human_size(q.size))
                };
                ui::QueuedFile { id: e.id, name: q.name.clone(), detail, held: e.held }
            })
            .collect()
    }

    /// The reachability checks and network settings.
    fn network(&self) -> ui::Network {
        let checks = self
            .reach
            .rows()
            .into_iter()
            .map(|(label, answer, detail)| ui::Check {
                label: label.to_string(),
                state: match answer.as_str() {
                    "Works" | "Found" => ui::CheckState::Good,
                    "Needs a hand" => ui::CheckState::Warn,
                    _ if self.checking => ui::CheckState::Unknown,
                    _ => ui::CheckState::Unknown,
                },
                detail,
            })
            .collect();
        let relay_note = match &self.relay_choice {
            Some(c) => match (c.source, &c.lender) {
                (fallback_relay::Source::Own, _) => String::new(),
                (fallback_relay::Source::Borrowed, Some(who)) => format!("Yours isn't set, so you're reachable through {who}'s relay ({}).", c.relay),
                (fallback_relay::Source::Mesh, _) => format!("Using {}, a relay your relay peers with.", c.relay),
                _ => format!("Using the fallback relay {}.", c.relay),
            },
            None if self.cfg.relay_server.trim().is_empty() => String::new(),
            None => "Your relay hasn't answered a check yet.".to_string(),
        };
        ui::Network {
            checks,
            port: self.cfg.stoop.port,
            relay: self.cfg.relay_server.clone(),
            relay_note,
            lend_relay: self.cfg.lend_relay,
            anonymity: self.cfg.anonymity_mode,
            hops: self.cfg.anonymity_hops.join(", "),
            mailboxes: self.mailboxes(),
            found_nodes: self.found_nodes.iter().filter(|a| !self.cfg.routes.iter().any(|r| r.nodes.contains(a))).cloned().collect(),
            finding_nodes: self.finding_nodes,
            local_api: self.api.is_some(),
            api_file: self.dir.as_ref().map(|d| d.join("api.json").display().to_string()).unwrap_or_default(),
        }
    }

    /// The settings screen's values.
    fn settings(&self) -> ui::Settings {
        let mut presets = vec![NATURAL.to_string()];
        presets.extend(antiphon_audio::eq_preset_names().iter().map(ToString::to_string));
        let preset = if self.cfg.chain.eq_bands.is_empty() || !presets.contains(&self.cfg.chain.name) { NATURAL.to_string() } else { self.cfg.chain.name.clone() };
        ui::Settings {
            theme: self.cfg.stoop.theme.clone(),
            accent: self.cfg.stoop.accent.clone(),
            custom_accent: self.cfg.stoop.custom_accent.clone(),
            font: self.cfg.stoop.font.clone(),
            inputs: self.inputs.clone(),
            outputs: self.outputs.clone(),
            input: self.cfg.input_device.clone().unwrap_or_default(),
            output: self.cfg.output_device.clone().unwrap_or_default(),
            transmit: match transmit_mode(self.cfg.transmit_mode) {
                MODE_PTT => ui::Transmit::PushToTalk,
                MODE_OPEN_MIC => ui::Transmit::Open,
                _ => ui::Transmit::Voice,
            },
            ptt_key: key_label(&self.cfg.ptt_key),
            ptt_capturing: self.ptt_capturing,
            noise_suppression: self.cfg.chain.noise_suppress.enabled,
            echo_cancel: self.cfg.echo_cancel,
            low_data: self.cfg.low_data_calls,
            screen_audio: self.cfg.share_screen_audio,
            plugins: self.plugins_model(),
            auto_gain: matches!(self.cfg.chain.gain, GainConfig::Auto { enabled: true, .. }),
            preset,
            presets,
            mic_test: self.state.loopback.lock().unwrap().as_ref().map(|l| l.level().min(1.0)),
            output_db: self.cfg.output_gain_db,
            reduced_motion: self.cfg.reduced_motion,
            notify_messages: self.cfg.stoop.notify_messages,
            open_on_start: self.cfg.stoop.door_on_start,
            version: env!("CARGO_PKG_VERSION").to_string(),
            data_dir: self.dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
        }
    }

    /// The conversation with `id`, rebuilt only when something changed.
    fn conversation(&mut self, id: &str) -> Option<ui::Conversation> {
        if self.is_notes(id) {
            let stale = self.conv.as_ref().is_none_or(|c| c.person != id || c.live);
            if stale {
                self.conv = Some(ConvCache { person: id.to_string(), live: false, rev: 0, lines: self.stored_lines(id) });
            }
            let lines = self.conv.as_ref().map(|c| c.lines.clone()).unwrap_or_default();
            // Always "queues": a note is written here at once, and reaches
            // the other devices through the mailboxes.
            return Some(ui::Conversation { person: id.to_string(), lines, queues: true, ..Default::default() });
        }
        let contact = find_contact(&self.cfg, id)?.clone();
        let live = self.call_with(id);
        let rev = if live { self.with_call(|a| a.chat_rev.load(Ordering::Relaxed)).unwrap_or(0) } else { 0 };
        let stale = self.conv.as_ref().is_none_or(|c| c.person != id || c.live != live || c.rev != rev);
        if stale {
            let mut lines = if live { self.live_lines() } else { self.stored_lines(&contact.identity_key) };
            self.find_voice(id, &mut lines);
            if let Some(last) = lines.iter().rev().find(|l| l.file.is_none()) {
                let text = if last.mine { format!("You: {}", last.text) } else { last.text.clone() };
                let at = self.previews.get(id).map(|(_, at)| *at).unwrap_or_else(now_secs);
                self.previews.insert(id.to_string(), (text, at));
            }
            self.conv = Some(ConvCache { person: id.to_string(), live, rev, lines });
        }
        let mut lines = self.conv.as_ref().map(|c| c.lines.clone()).unwrap_or_default();
        for line in &mut lines {
            if let Some(clip) = line.voice.as_mut() {
                clip.playing = self.playing.as_ref().is_some_and(|(p, i, ..)| p == id && *i == line.id);
                clip.fetching = self.voice_waiting.as_ref().is_some_and(|(p, i, _)| p == id && *i == line.id);
            }
        }
        Some(ui::Conversation {
            person: id.to_string(),
            lines,
            live,
            queues: !live && !contact.identity_key.is_empty(),
            expiry: contact.expire_after_secs.map(expiry_label),
            recording: self.recording.as_ref().filter(|r| r.to == id).map(|r| r.started.elapsed().as_secs() as u32),
        })
    }

    /// Lines from saved history, with anything still in the outbox marked.
    fn stored_lines(&self, key_hex: &str) -> Vec<ui::Line> {
        let (Some(dir), Some(key)) = (&self.dir, &self.key) else { return Vec::new() };
        let now = now_secs();
        let log: Vec<history::StoredMessage> = history::load(dir, key_hex, key).into_iter().filter(|m| !m.is_expired(now)).collect();
        let queued = outbox::load(dir, key_hex, key);
        let mut lines = Vec::with_capacity(log.len());
        let mut last_day = None;
        for m in &log {
            let day = day_label(m.at_secs);
            let mut line = ui::Line {
                id: m.id,
                mine: m.from_me,
                author: String::new(),
                text: if m.deleted { "Message deleted".into() } else { m.text.clone() },
                time: clock_time(m.at_secs),
                day: (last_day.as_ref() != Some(&day)).then(|| day.clone()),
                delivery: if !m.from_me {
                    ui::Delivery::None
                } else if queued.iter().any(|q| q.id == m.id) {
                    ui::Delivery::Queued
                } else {
                    ui::Delivery::Delivered
                },
                edited: m.edited,
                pinned: m.pinned,
                reply_to: m.reply_to,
                ..ui::Line::default()
            };
            if m.deleted {
                line.delivery = ui::Delivery::None;
            }
            last_day = Some(day);
            lines.push(line);
        }
        lines
    }

    /// Lines from the running call's chat, which already holds history.
    fn live_lines(&self) -> Vec<ui::Line> {
        let times: HashMap<(u64, bool), u64> = match (&self.dir, &self.key, &self.call.peer) {
            (Some(dir), Some(key), Some(peer)) => history::load(dir, peer, key).into_iter().map(|m| ((m.id, m.from_me), m.at_secs)).collect(),
            _ => HashMap::new(),
        };
        let call = self.state.call.lock().unwrap();
        let Some(a) = call.as_ref() else { return Vec::new() };
        let log = a.chat_log.lock().unwrap().clone();
        let all_reactions = a.reactions.lock().unwrap().clone();
        let mut lines = Vec::with_capacity(log.len());
        let mut last_day = None;
        for e in &log {
            let at = times.get(&(e.id, e.from_me)).copied().unwrap_or_else(now_secs);
            let day = day_label(at);
            lines.push(ui::Line {
                id: e.id,
                mine: e.from_me,
                author: String::new(),
                text: if e.deleted { "Message deleted".into() } else { e.text.clone() },
                time: clock_time(at),
                day: (last_day.as_ref() != Some(&day)).then(|| day.clone()),
                delivery: if e.from_me && !e.deleted {
                    match a.delivery_status(e.id) {
                        outbox::Status::Sending => ui::Delivery::Sent,
                        outbox::Status::Waiting => ui::Delivery::Queued,
                        outbox::Status::Delivered => ui::Delivery::Delivered,
                    }
                } else {
                    ui::Delivery::None
                },
                edited: e.edited,
                pinned: e.pinned,
                reactions: reactions::counts(&all_reactions, e.id),
                reacted: all_reactions.iter().filter(|r| r.message_id == e.id && r.from_me).map(|r| r.emoji.clone()).collect(),
                file: None,
                reply_to: e.reply_to,
                ..ui::Line::default()
            });
            last_day = Some(day);
        }
        lines
    }

    /// Hands the router its port back on the way out.
    pub fn shutdown(&mut self) {
        self.teardown("Closing".into());
        let closing = self.state.port_map.lock().unwrap().take();
        if let Some(handle) = closing {
            handle.close_before_exit(&self.state.runtime);
        }
    }
}

/// What finishing a connection needs, captured before the task starts.
struct Connect {
    /// The shared client state the call is set up in.
    state: Arc<AppState>,
    /// Where to report how it went.
    events: Arc<Mutex<Vec<Event>>>,
    /// The voice chain to start the microphone with.
    chain: ChainConfig,
    /// The microphone to use; `None` for the system default.
    input: Option<String>,
    /// The speakers to use; `None` for the system default.
    output: Option<String>,
    /// Our name and details, sent to the other side first.
    hello: Hello,
}

impl Connect {
    /// Turns a connection into a running call: trust, blocks, the
    /// per-person audio, and the pipeline.
    fn finish(self, result: anyhow::Result<antiphon_net::PeerConnection>, incoming: bool) {
        let state = self.state;
        let peer = match result {
            Ok(peer) => peer,
            Err(e) => {
                Engine::push(&self.events, Event::Failed(e.to_string()));
                return;
            }
        };
        let identity = peer.peer_identity();
        let key = identity.identity_key;
        let hex = connection_mode::encode_hex32(&key);
        let cfg = config::load();
        if moderation::is_blocked(&cfg.blocked, &hex) {
            // Refused here, before anything is shown: there is no server to
            // enforce a block, so if this does not hang up nothing does.
            if !incoming {
                Engine::push(&self.events, Event::Failed("They're blocked".into()));
            }
            return;
        }
        let code = antiphon_crypto::verification_code(&state.identity.public().identity_key, &key);
        let remote_addr = peer.remote_addr().to_string();
        let relayed = *state.relay_addr.lock().unwrap() == Some(peer.remote_addr());
        let mode = if relayed { reachability::Mode::Relayed } else { reachability::Mode::Direct };
        let warning = evaluate_peer_trust(&remote_addr, &identity, relayed);
        let cfg = config::load();
        let contact = cfg.contacts.iter().find(|c| c.identity_key == hex);
        let (volume, muted) = contact.map(|c| (c.volume_db, c.muted)).unwrap_or((0.0, false));
        *state.peer_volume_db.lock().unwrap() = volume;
        state.peer_muted.store(muted, Ordering::Relaxed);
        let bands = contact.map(|c| c.eq_bands.clone()).unwrap_or_default();
        *state.peer_eq.lock().unwrap() = peer_eq_chain(&bands);
        let name = contact.map(|c| c.name.clone()).unwrap_or_else(|| remote_addr.clone());
        let known = contact.is_some();
        let history = history_sink_for(&cfg, &hex);
        let started = state::start_pipeline(
            peer,
            self.chain,
            self.input,
            self.output,
            state.transmit.clone(),
            state.output_gain_db.clone(),
            state.replay_seconds.clone(),
            state.duck_db.clone(),
            state.peer_eq.clone(),
            state.echo_cancel.clone(),
            state.echo_canceller.clone(),
            state.peer_volume_db.clone(),
            state.peer_muted.clone(),
            self.hello,
            history,
        );
        match started {
            Ok(active) => {
                active.flush_outbox(&state.runtime);
                *state.call.lock().unwrap() = Some(active);
                file_transfer::set_call_active(true);
                Engine::push(&self.events, Event::Connected { incoming, peer: hex, name, code, mode, warning, known });
            }
            Err(e) => Engine::push(&self.events, Event::Failed(format!("Your audio wouldn't start: {e}"))),
        }
    }
}

/// Spec §4: a direct call that looks the contact up in the DHT when their
/// saved address doesn't answer, then tries their relay (automatic mode).
async fn call_with_lookup(
    contact: &config::Contact,
    saved: String,
    identity: Arc<antiphon_crypto::IdentityKeyPair>,
    args: &Connect,
) -> anyhow::Result<antiphon_net::PeerConnection> {
    let attempt = |address: String| {
        let identity = identity.clone();
        async move {
            let remote = state::resolve(&address).await?;
            match tokio::time::timeout(discovery::DIRECT_TRY, state::establish_call(remote, 0, identity)).await {
                Ok(result) => result,
                Err(_) => anyhow::bail!("{address} didn't answer"),
            }
        }
    };
    let say = |text: String| Engine::push(&args.events, Event::Status(text));
    let mut last = match attempt(saved.clone()).await {
        Ok(peer) => return Ok(peer),
        Err(e) => e,
    };
    say(format!("Looking {} up...", contact.name));
    if let Some(fresh) = discovery::find_contact_now(&contact.identity_key, &saved).await {
        say(format!("Found {} somewhere new. Calling...", contact.name));
        match attempt(fresh).await {
            Ok(peer) => return Ok(peer),
            Err(e) => last = e,
        }
    }
    let refreshed = config::load().contacts.into_iter().find(|c| c.identity_key == contact.identity_key);
    let contact = refreshed.as_ref().unwrap_or(contact);
    if connection_mode::Override::from_i32(contact.connection_mode) == connection_mode::Override::Auto {
        if let Some(connection_mode::Plan::Relay { relay, token }) = connection_mode::relay_only(contact, &identity, now_secs()) {
            say(format!("Trying {} through their relay...", contact.name));
            let addr = state::resolve(&relay).await?;
            *args.state.relay_addr.lock().unwrap() = Some(addr);
            return state::establish_relayed_call(addr, token, 0, identity).await;
        }
    }
    Err(last)
}

/// Spec §4 trust on first use, against what is pinned for this person.
/// Returns a warning only when there is something to warn about.
fn evaluate_peer_trust(remote_addr: &str, peer: &antiphon_crypto::PublicIdentity, relayed: bool) -> Option<String> {
    let mut cfg = config::load();
    let key_hex = connection_mode::encode_hex32(&peer.identity_key);
    let index = cfg
        .contacts
        .iter()
        .position(|c| c.identity_key == key_hex)
        .or_else(|| (!relayed).then(|| cfg.contacts.iter().position(|c| c.identity_key.is_empty() && c.address == remote_addr)).flatten())?;
    let prekey_hex = connection_mode::encode_hex32(&peer.prekey);
    if cfg.contacts[index].identity_key == key_hex && cfg.contacts[index].prekey != prekey_hex {
        cfg.contacts[index].prekey = prekey_hex.clone();
        config::save(&cfg);
    }
    let contact = &cfg.contacts[index];
    let pinned = connection_mode::decode_hex32(&contact.identity_key).map(|identity_key| antiphon_crypto::PinnedContact {
        identity_key,
        petname: contact.name.clone(),
        trust: if contact.verified { antiphon_crypto::TrustLevel::Verified } else { antiphon_crypto::TrustLevel::Unverified },
        first_seen_secs: contact.first_seen_secs,
    });
    let name = contact.name.clone();
    match antiphon_crypto::evaluate(pinned.as_ref(), peer) {
        antiphon_crypto::TrustDecision::FirstContact => {
            cfg.contacts[index].identity_key = key_hex;
            cfg.contacts[index].first_seen_secs = now_secs();
            cfg.contacts[index].prekey = prekey_hex;
            config::save(&cfg);
            None
        }
        antiphon_crypto::TrustDecision::Unchanged { .. } => None,
        antiphon_crypto::TrustDecision::KeyChanged { was_verified, .. } => Some(if was_verified {
            format!("{name}'s key changed, and you had verified the old one. Compare the words before you trust this call.")
        } else {
            format!("{name}'s key changed since last time. Usually a reinstall; compare the words to be sure.")
        }),
    }
}

/// What an incoming call from this identity may do (spec §9).
fn ring_outcome(key: &[u8; 32], caller_wants_silent: bool) -> notify::Outcome {
    let hex = connection_mode::encode_hex32(key);
    let cfg = config::load();
    let rule = cfg.contacts.iter().find(|c| c.identity_key == hex).map(|c| c.ring_rule).unwrap_or(cfg.default_ring_rule);
    notify::decide(notify::RingRule::from_i32(rule), cfg.quiet_hours, notify::current_minute_of_day(), caller_wants_silent)
}

/// This contact's EQ, built once at connect.
fn peer_eq_chain(bands: &[antiphon_audio::EqBandConfig]) -> Option<antiphon_audio::AudioChain> {
    if bands.iter().all(|b| !b.enabled) {
        return None;
    }
    let mut chain = ChainConfig::default();
    chain.noise_gate.enabled = false;
    chain.high_pass.enabled = false;
    chain.low_pass.enabled = false;
    chain.de_esser.enabled = false;
    chain.compressor.enabled = false;
    chain.limiter.enabled = false;
    chain.pitch_shift.enabled = false;
    chain.reverb.enabled = false;
    chain.gain = GainConfig::Fixed { enabled: false, gain_db: 0.0 };
    chain.eq_bands = bands.to_vec();
    Some(antiphon_audio::AudioChain::from_config(48_000, &chain))
}

/// A history sink for a saved contact; strangers get none, so nothing about
/// them is written down.
fn history_sink_for(cfg: &config::AppConfig, hex: &str) -> Option<Arc<history::HistorySink>> {
    let contact = cfg.contacts.iter().find(|c| c.identity_key == hex)?;
    let dir = config::config_dir()?;
    let key = history::load_or_create_backup_key(&dir).ok()?;
    Some(Arc::new(history::HistorySink { dir, peer_hex: hex.to_string(), key, expire_after_secs: contact.expire_after_secs }))
}

/// Stores a contact's route list if it is theirs and newer than ours.
fn save_peer_routes(identity_key: &[u8; 32], encoded: &str) {
    let mut cfg = config::load();
    let key = connection_mode::encode_hex32(identity_key);
    let Some(contact) = cfg.contacts.iter_mut().find(|c| c.identity_key == key) else { return };
    if contact.route_record == encoded {
        return;
    }
    let Some(record) = delivery::accept_record(encoded, identity_key, contact.route_sequence) else { return };
    contact.route_record = encoded.to_string();
    contact.route_sequence = Some(record.record.sequence);
    config::save(&cfg);
}

/// The secret behind the relay part of the invite link (spec §3).
fn relay_invite_secret() -> [u8; 32] {
    let mut cfg = config::load();
    if let Some(secret) = connection_mode::decode_hex32(&cfg.relay_invite_secret) {
        return secret;
    }
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("OS randomness is unavailable");
    cfg.relay_invite_secret = connection_mode::encode_hex32(&secret);
    config::save(&cfg);
    secret
}

/// Builds the shared state the call engine runs on, as the classic app did.
pub fn start_state(runtime: tokio::runtime::Handle) -> anyhow::Result<Arc<AppState>> {
    let loaded = config::load();
    antiphon_client::link_quality::set_low_data(loaded.low_data_calls);
    antiphon_client::screen_audio::set_enabled(loaded.share_screen_audio);
    let ptt_key = Keycode::from_str(&loaded.ptt_key).or_else(|_| Keycode::from_str(caps_lock::default_ptt_key())).unwrap_or(Keycode::Space);
    let mode = transmit_mode(loaded.transmit_mode);
    let transmit = Arc::new(TransmitSettings::new(ptt_key, mode, loaded.va_threshold));
    caps_lock::set_bound(ptt_key == Keycode::CapsLock && mode == MODE_PTT);
    let dir = config::config_dir().ok_or_else(|| anyhow::anyhow!("could not locate a config directory for the identity key"))?;
    let identity = Arc::new(antiphon_crypto::store::load_or_create(&antiphon_crypto::store::identity_path(&dir))?);
    let state = Arc::new(AppState {
        runtime,
        call: Mutex::new(None),
        transmit,
        output_gain_db: Arc::new(Mutex::new(loaded.output_gain_db)),
        loopback: Mutex::new(None),
        voice_stop: Arc::new(AtomicBool::new(false)),
        replay_seconds: Arc::new(std::sync::atomic::AtomicU32::new(loaded.replay_seconds)),
        duck_db: Arc::new(std::sync::atomic::AtomicU32::new(loaded.duck_db)),
        peer_eq: Arc::new(Mutex::new(None)),
        echo_cancel: Arc::new(AtomicBool::new(loaded.echo_cancel)),
        echo_canceller: Arc::new(Mutex::new(antiphon_audio::EchoCanceller::new(48_000))),
        peer_volume_db: Arc::new(Mutex::new(0.0)),
        peer_muted: Arc::new(AtomicBool::new(false)),
        punch_socket: Mutex::new(None),
        port_map: Mutex::new(None),
        identity,
        relay_addr: Mutex::new(None),
    });

    delivery::set_identity(state.identity.clone());
    {
        let weak = Arc::downgrade(&state);
        delivery::start(&state.runtime, move || {
            let state = weak.upgrade()?;
            let call = state.call.lock().unwrap();
            call.as_ref().map(|a| a.peer_identity().identity_key)
        });
    }
    delivery::set_hosting(&state.runtime, loaded.hold_mail_for_devices, loaded.hold_mail_port);
    sharing::set_seeding(&state.runtime, loaded.seeding, loaded.seed_port);
    vpn::apply(
        &state.runtime,
        loaded.vpn_enabled,
        &loaded.vpn_active,
        antiphon_net::vpn::Routes { voice: loaded.vpn_route_voice, files: loaded.vpn_route_files, messages: loaded.vpn_route_messages },
        loaded.vpn_kill_switch,
    );
    discovery::start(&state.runtime, state.identity.clone());
    Ok(state)
}

// ---------------------------------------------------------------- helpers

/// The live transmit mode for the saved number (0 push to talk, 1 voice, 2 open mic).
fn transmit_mode(stored: i32) -> u32 {
    match stored {
        1 => MODE_VOICE_ACTIVATION,
        2 => MODE_OPEN_MIC,
        _ => MODE_PTT,
    }
}

/// "CapsLock" as a person writes it: "Caps Lock".
fn key_label(name: &str) -> String {
    let name = if name.is_empty() { caps_lock::default_ptt_key() } else { name };
    let mut out = String::new();
    for (i, ch) in name.chars().enumerate() {
        if i > 0 && ch.is_uppercase() && !out.ends_with(' ') {
            out.push(' ');
        }
        out.push(ch);
    }
    out
}

/// A contact's id: their key, or their address for one saved before keys.
fn contact_id(c: &config::Contact) -> String {
    if c.identity_key.is_empty() { c.address.clone() } else { c.identity_key.clone() }
}

/// Where contact `id` is in the list.
fn contact_index(cfg: &config::AppConfig, id: &str) -> Option<usize> {
    cfg.contacts.iter().position(|c| contact_id(c) == id)
}

/// Contact `id`, if saved.
fn find_contact<'a>(cfg: &'a config::AppConfig, id: &str) -> Option<&'a config::Contact> {
    contact_index(cfg, id).map(|i| &cfg.contacts[i])
}

/// Contact `id`, to edit.
fn contact_mut<'a>(cfg: &'a mut config::AppConfig, id: &str) -> Option<&'a mut config::Contact> {
    contact_index(cfg, id).map(move |i| &mut cfg.contacts[i])
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Unix seconds as local time.
fn local(secs: u64) -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::from_timestamp(secs as i64, 0).unwrap_or_default().with_timezone(&chrono::Local)
}

/// "14:02".
pub fn clock_time(secs: u64) -> String {
    local(secs).format("%H:%M").to_string()
}

/// "Today", "Yesterday", "Monday 26 October".
pub fn day_label(secs: u64) -> String {
    let day = local(secs).date_naive();
    let today = chrono::Local::now().date_naive();
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => day.format("%A %-d %B").to_string(),
    }
}

/// A list timestamp: the time today, the weekday this week, else the date.
pub fn short_time(secs: u64) -> String {
    let at = local(secs);
    let days = (chrono::Local::now().date_naive() - at.date_naive()).num_days();
    match days {
        0 => at.format("%H:%M").to_string(),
        1..=6 => at.format("%a").to_string(),
        _ => at.format("%-d %b").to_string(),
    }
}

/// How long messages last, as a sentence ("Messages vanish after 2 hours").
fn expiry_label(secs: u64) -> String {
    let span = match secs {
        s if s < 3600 => format!("{} minutes", s / 60),
        s if s < 86_400 => format!("{} hours", s / 3600),
        s if s < 2 * 86_400 => "1 day".to_string(),
        s => format!("{} days", s / 86_400),
    };
    format!("Messages vanish after {span}")
}

/// A byte count people can read ("3.4 MB").
pub fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64 / 1024.0;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// This machine's address on the local network, found by asking the OS
/// which interface would route outward (nothing is sent).
fn local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// The Downloads folder in your home, if it exists.
fn dirs_downloads() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Downloads")).filter(|p| p.exists())
}

/// Opens a web link from a message in the default browser. Only `http`
/// and `https`, so a message can't have the system open anything else.
fn open_link(url: &str) {
    if url.starts_with("https://") || url.starts_with("http://") {
        open_folder(std::path::Path::new(url));
    }
}

/// Opens `path` in the file manager (or, for a web link, the browser).
fn open_folder(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// A call phase as the API names it.
fn api_phase(phase: &ui::Phase) -> &'static str {
    match phase {
        ui::Phase::Idle => "idle",
        ui::Phase::Dialing => "dialing",
        ui::Phase::Connected => "connected",
        ui::Phase::Ended(_) => "ended",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_read_like_words() {
        assert_eq!(key_label("CapsLock"), "Caps Lock");
        assert_eq!(key_label("F13"), "F13");
        assert_eq!(key_label("LControl"), "L Control");
    }

    #[test]
    fn sizes_are_short() {
        assert_eq!(human_size(900), "900 B");
        assert_eq!(human_size(2048), "2.0 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn expiry_is_said_plainly() {
        assert_eq!(expiry_label(86_400), "Messages vanish after 1 day");
        assert_eq!(expiry_label(7 * 86_400), "Messages vanish after 7 days");
    }
}
