//! The call itself: the desktop CLI's `run_call`, moved behind a handle
//! the interface can drive. The wire format, handshake, FEC and jitter
//! buffer are the shared crates', unchanged, so a phone calls a desktop.
//!
//! While a call is up, [`Live`] is what the interface uses to reach the
//! other side: chat goes out over the call's control channel, and file
//! transfers run through the desktop's own `file_transfer` module on this
//! engine's runtime, so the bytes on the wire are the desktop's.

use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use antiphon_audio::{start_capture, start_playback, AudioChain, ChainConfig, PlaybackSink};
use antiphon_crypto::{verification_code, IdentityKeyPair, Invite};
use antiphon_net::voice::{VoiceReceiver, VoiceSender};
use antiphon_net::{AppSocket, JitterBuffer, PeerConnection, PopResult, Received};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

use crate::control::{ControlMessage, Hello, ProfileCard};
use crate::file_transfer::{self, FileTransferState};
use crate::link_quality::{self, LinkQuality};
use crate::platform;

/// Same wait as the desktop builds, so both ends conceal at the same point.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(60);
/// How long to wait for a STUN server before listening without a public
/// address. Listening matters more than knowing the address.
const STUN_TIMEOUT: Duration = Duration::from_secs(4);
/// How often the interface is woken while a file is moving, so its
/// progress bar and queue keep up even when nothing else happens.
const TRANSFER_WAKE: Duration = Duration::from_millis(250);

/// Where the call is up to, as the call screen shows it.
#[derive(Clone, Debug, Default)]
pub enum Phase {
    /// Nothing running; also the state during a mic test.
    #[default]
    Idle,
    /// Waiting for an incoming call.
    Listening {
        /// `ip:port` on the local network, for someone nearby to dial.
        local: String,
        /// The address STUN reported the outside world sees, once known.
        public: Option<String>,
    },
    /// Resolving the target and running the handshake.
    Dialing {
        /// What the user dialled, as typed.
        remote: String,
    },
    /// The handshake succeeded and audio is flowing.
    Connected {
        /// The other side's `ip:port`.
        remote: String,
        /// The verification code to read aloud and compare.
        code: String,
        /// Their identity key, as proved in the handshake.
        peer_key: [u8; 32],
    },
    /// The last call or attempt is over.
    Ended {
        /// One sentence on how it ended, shown until the next call.
        message: String,
    },
}

/// What the audio and network tasks share with the interface.
#[derive(Default)]
struct Shared {
    /// Where the call is up to; read by the interface every frame, written
    /// only through [`Shared::set_phase`].
    phase: Mutex<Phase>,
    /// Peak level of the processed microphone signal, as f32 bits.
    level: AtomicU32,
    /// Push-to-talk held, or open mic on.
    transmitting: AtomicBool,
    /// The open-mic setting: transmit without holding the button.
    open_mic: AtomicBool,
    /// The push-to-talk button is held down.
    ptt_held: AtomicBool,
    /// Play through the loudspeaker rather than the earpiece. Read when a
    /// call connects, to put the audio session in the right mode.
    speaker: AtomicBool,
    /// Bumped by every new call or mic test, so a task that is still
    /// winding down cannot overwrite the phase of the one that replaced it.
    generation: AtomicU64,
    /// The connected call, while there is one.
    live: Mutex<Option<Arc<Live>>>,
    /// Calls that ended since the interface last looked, for the call
    /// history. Recorded here, where the outcome is known, rather than
    /// inferred from phase changes the interface may never see: a dial
    /// that fails at once can go from dialing to ended between two frames.
    finished: Mutex<Vec<CallSummary>>,
    /// The name sent to the other side when a call connects.
    display_name: Mutex<String>,
    /// The status line sent with it.
    status_message: Mutex<String>,
    /// The rest of the profile (picture, About, colour), sent when a call
    /// connects.
    card: Mutex<ProfileCard>,
    /// Proves calls come from this phone, one of the account's devices;
    /// `None` until the device sync is running.
    prover: Mutex<Option<Arc<crate::my_devices::Prover>>>,
}

impl Shared {
    /// Starts a new call or mic test and returns its generation, which
    /// retires every task belonging to the one before.
    fn begin(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Sets the phase, but only for the current generation: a task of a
    /// call that has since been replaced changes nothing.
    fn set_phase(&self, generation: u64, phase: Phase) {
        let mut current = self.phase.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) == generation {
            *current = phase;
        }
    }

    /// Recomputes `transmitting` from the open-mic setting and the
    /// push-to-talk button.
    fn update_transmitting(&self) {
        let on = self.open_mic.load(Ordering::Relaxed) || self.ptt_held.load(Ordering::Relaxed);
        self.transmitting.store(on, Ordering::Relaxed);
    }
}

/// Wakes the interface's event loop. Called from the call's tasks,
/// which run on the engine's runtime, so it must be callable from any
/// thread.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A connected call, as the interface sees it.
pub struct Live {
    /// The encrypted link to the other side, shared with the call's tasks
    /// and with file transfer.
    pub peer: Arc<PeerConnection>,
    /// Their identity key, as proved in the handshake.
    pub peer_key: [u8; 32],
    /// The desktop's transfer state machine, driven by this call's receive
    /// loop and read by the interface for progress.
    pub files: Arc<Mutex<FileTransferState>>,
    /// Chat and profile messages that arrived, waiting for the interface,
    /// which owns the conversation and its history.
    inbox: Mutex<Vec<ControlMessage>>,
    /// A handle to the engine's runtime, for spawning sends and transfers
    /// from the interface thread.
    runtime: tokio::runtime::Handle,
    /// The other side's camera and screen, fed by the receive loop.
    pub video: Arc<crate::video::CallVideo>,
}

impl Live {
    /// Sends a control message, best effort, without waiting.
    pub fn send(&self, message: ControlMessage) {
        let peer = self.peer.clone();
        self.runtime.spawn(async move {
            let _ = peer.send_control(&message.encode()).await;
        });
    }

    /// Takes the chat and profile messages that arrived since the last call,
    /// oldest first. The interface calls it on every wake.
    pub fn take_inbox(&self) -> Vec<ControlMessage> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }

    /// The call's runtime, where file transfer runs its tasks next to the
    /// call's own.
    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

/// One finished call, for the call history.
#[derive(Clone, Debug)]
pub struct CallSummary {
    /// Whether this phone placed the call rather than answered it.
    pub outgoing: bool,
    /// What was dialled, or where an incoming call came from.
    pub remote: String,
    /// Who answered, as proved in the handshake, or `None` when the call
    /// never connected.
    pub peer_key: Option<[u8; 32]>,
    /// When the call began (dialling, or the incoming handshake), as Unix
    /// seconds.
    pub started_secs: u64,
    /// When it connected, if it did.
    pub connected_secs: Option<u64>,
    /// When the call ended, as Unix seconds.
    pub ended_secs: u64,
    /// The line shown when it ended.
    pub message: String,
}

/// The current time as Unix seconds, or 0 if the clock is set before
/// 1970.
pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// What `run` learned about a call, filled in as it goes so the summary
/// is right however the call ends.
#[derive(Default)]
struct Facts {
    /// What was dialled, or where the call came from.
    remote: String,
    /// Who answered, once the handshake proved it.
    peer_key: Option<[u8; 32]>,
    /// When it connected, as Unix seconds, if it did.
    connected_secs: Option<u64>,
}

/// Runs calls and the mic test for the interface: one at a time, each on
/// its own tasks on a small runtime of the engine's, so the audio never
/// waits on the interface thread.
///
/// Owned by the interface and driven from its thread; the tasks report
/// back through the shared state and the waker.
pub struct CallEngine {
    /// Two worker threads for the network, audio and transfer tasks; the
    /// microphone and speaker streams have a thread of their own.
    runtime: tokio::runtime::Runtime,
    /// State the tasks write and the interface reads.
    shared: Arc<Shared>,
    /// The current mic chain. Running calls and mic tests watch it, so a
    /// change applies mid-call.
    chain: watch::Sender<ChainConfig>,
    /// Called by the tasks whenever something the interface shows changed.
    wake: Waker,
    /// Ends the running call, listen, or mic test.
    stop: Option<oneshot::Sender<()>>,
    /// Whether the running thing is a mic test rather than a call.
    mic_test: bool,
}

impl CallEngine {
    /// Builds the engine and its runtime with the saved mic chain and
    /// switches. Fails only if the runtime's threads cannot be started.
    pub fn new(chain: ChainConfig, open_mic: bool, speaker: bool, wake: Waker) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let shared = Arc::new(Shared::default());
        shared.open_mic.store(open_mic, Ordering::Relaxed);
        shared.speaker.store(speaker, Ordering::Relaxed);
        shared.update_transmitting();
        let (chain, _) = watch::channel(chain);
        Ok(Self { runtime, shared, chain, wake, stop: None, mic_test: false })
    }

    /// Where the call is up to now.
    pub fn phase(&self) -> Phase {
        self.shared.phase.lock().unwrap().clone()
    }

    /// The connected call, if there is one.
    pub fn live(&self) -> Option<Arc<Live>> {
        self.shared.live.lock().unwrap().clone()
    }

    /// Calls that ended since the last look.
    pub fn take_finished(&self) -> Vec<CallSummary> {
        std::mem::take(&mut *self.shared.finished.lock().unwrap())
    }

    /// A handle to the engine's runtime, for the account's device sync to
    /// run beside the calls.
    pub fn handle(&self) -> tokio::runtime::Handle {
        self.runtime.handle().clone()
    }

    /// Sets the name sent to the other side in the next call's greeting.
    /// Surrounding whitespace is dropped.
    pub fn set_display_name(&self, name: &str) {
        *self.shared.display_name.lock().unwrap() = name.trim().to_string();
    }

    /// Sets what proves the next calls come from this phone.
    pub fn set_prover(&self, prover: Option<Arc<crate::my_devices::Prover>>) {
        *self.shared.prover.lock().unwrap() = prover;
    }

    /// This phone's proof for a Hello to `to`, hex; empty without one.
    pub fn hello_proof(&self, to: &[u8; 32]) -> String {
        self.shared.prover.lock().unwrap().as_ref().map(|p| p.hello_proof(to)).unwrap_or_default()
    }

    /// Sets the status line and profile card sent when the next call
    /// connects. Sending them to a call already up is the interface's job.
    pub fn set_profile(&self, status: &str, card: ProfileCard) {
        *self.shared.status_message.lock().unwrap() = status.trim().to_string();
        *self.shared.card.lock().unwrap() = card;
    }

    /// Turns the data saver on or off for the running call and the next
    /// ones: on, calls switch to "Low data" (spec §2 automatic bandwidth:
    /// 8 kbit/s voice in 60 ms packets, no video), applied within a packet.
    pub fn set_data_saver(&self, on: bool) {
        link_quality::set_low_data(on);
    }

    /// Whether a call is waiting, dialling or connected, so a new one would
    /// replace it.
    pub fn is_busy(&self) -> bool {
        matches!(self.phase(), Phase::Listening { .. } | Phase::Dialing { .. } | Phase::Connected { .. })
    }

    /// Whether the mic test is running.
    pub fn mic_test_running(&self) -> bool {
        self.mic_test
    }

    /// Peak level of the processed microphone signal in the last frame, as
    /// a linear sample amplitude from 0 to 1, for the level meter. Zero
    /// while not transmitting.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    /// Whether the microphone is being sent right now.
    pub fn transmitting(&self) -> bool {
        self.shared.transmitting.load(Ordering::Relaxed)
    }

    /// Records the push-to-talk button being pressed or released.
    pub fn set_ptt(&self, held: bool) {
        self.shared.ptt_held.store(held, Ordering::Relaxed);
        self.shared.update_transmitting();
    }

    /// Switches open mic on or off.
    pub fn set_open_mic(&self, on: bool) {
        self.shared.open_mic.store(on, Ordering::Relaxed);
        self.shared.update_transmitting();
    }

    /// Switches between loudspeaker and earpiece, at once if a call is
    /// connected.
    pub fn set_speaker(&self, on: bool) {
        self.shared.speaker.store(on, Ordering::Relaxed);
        if matches!(self.phase(), Phase::Connected { .. }) {
            platform::set_call_audio(true, on);
        }
    }

    /// Applies to a running call or mic test immediately, not just the
    /// next one.
    pub fn set_chain(&self, chain: ChainConfig) {
        self.chain.send_replace(chain);
    }

    /// Ends whatever is running.
    pub fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.mic_test = false;
    }

    /// Waits for a call on `port`.
    pub fn listen(&mut self, port: u16, identity: IdentityKeyPair) {
        self.stop();
        self.start(Mode::Listen { port }, identity);
    }

    /// Calls `target`: `ip:port`, `host:port`, or an invite link carrying
    /// an address. An invite also pins who must answer.
    pub fn call(&mut self, target: &str, identity: IdentityKeyPair) {
        self.stop();
        self.start(Mode::Call { target: target.trim().to_string() }, identity);
    }

    /// Plays the microphone, through the chain, back to the speaker.
    pub fn start_mic_test(&mut self) {
        self.stop();
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stop = Some(stop_tx);
        self.mic_test = true;
        let generation = self.shared.begin();
        self.shared.set_phase(generation, Phase::Idle);
        let shared = self.shared.clone();
        let chain = self.chain.subscribe();
        let wake = self.wake.clone();
        self.runtime.spawn(async move {
            if let Err(e) = mic_test(shared.clone(), chain, stop_rx).await {
                shared.set_phase(generation, Phase::Ended { message: format!("Mic test stopped: {e}") });
            }
            shared.level.store(0, Ordering::Relaxed);
            wake();
        });
    }

    /// Spawns the task that runs one call (listening or dialling) and,
    /// when it ends for any reason, records its summary and final phase.
    fn start(&mut self, mode: Mode, identity: IdentityKeyPair) {
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stop = Some(stop_tx);
        let generation = self.shared.begin();
        let shared = self.shared.clone();
        let chain = self.chain.subscribe();
        let wake = self.wake.clone();
        self.runtime.spawn(async move {
            let outgoing = matches!(mode, Mode::Call { .. });
            let started_secs = now_secs();
            let mut facts = Facts::default();
            let message = match run(&shared, generation, &wake, mode, identity, chain, stop_rx, &mut facts).await {
                Ok(message) => message,
                Err(e) => e,
            };
            if shared.generation.load(Ordering::SeqCst) == generation {
                platform::set_call_audio(false, false);
                shared.level.store(0, Ordering::Relaxed);
            }
            // Waiting for a call that never came is not a call; anything
            // dialled, or anything that connected, is.
            if outgoing || facts.connected_secs.is_some() {
                shared.finished.lock().unwrap().push(CallSummary {
                    outgoing,
                    remote: facts.remote,
                    peer_key: facts.peer_key,
                    started_secs,
                    connected_secs: facts.connected_secs,
                    ended_secs: now_secs(),
                    message: message.clone(),
                });
            }
            shared.set_phase(generation, Phase::Ended { message });
            wake();
        });
    }
}

/// How a call starts.
enum Mode {
    /// Wait for someone to call in.
    Listen {
        /// The UDP port to listen on.
        port: u16,
    },
    /// Dial out.
    Call {
        /// What the user typed: an address or an invite link.
        target: String,
    },
}

/// Keeps the microphone and speaker open on a thread of their own, so the
/// cpal streams (which are not `Send` on every platform) never cross an
/// `.await`. Dropping it closes both.
struct AudioIo {
    /// Dropping this ends the audio thread, which closes both streams.
    _stop: std_mpsc::Sender<()>,
    /// The rate the microphone captures at, in Hz; playback runs at the
    /// same rate.
    sample_rate: u32,
    /// Where decoded audio goes to be played.
    sink: PlaybackSink,
    /// Microphone frames as they are captured, in samples from -1 to 1.
    frames: mpsc::UnboundedReceiver<Vec<f32>>,
}

/// Opens the microphone and speaker on a thread of their own and waits
/// until both are running, or returns the reason one would not open.
fn open_audio() -> Result<AudioIo, String> {
    let (frames_tx, frames) = mpsc::unbounded_channel::<Vec<f32>>();
    let (ready_tx, ready_rx) = std_mpsc::channel();
    let (stop, stop_rx) = std_mpsc::channel::<()>();
    std::thread::spawn(move || {
        let capture = match start_capture(None, move |samples: &[f32]| {
            let _ = frames_tx.send(samples.to_vec());
        }) {
            Ok(capture) => capture,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("could not open the microphone: {e}")));
                return;
            }
        };
        let (playback, sink) = match start_playback(None, 0.5) {
            Ok(pair) => pair,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("could not open the speaker: {e}")));
                return;
            }
        };
        let _ = ready_tx.send(Ok((capture.sample_rate, sink)));
        // Returns when AudioIo drops its sender.
        let _ = stop_rx.recv();
        drop(capture);
        drop(playback);
    });
    let (sample_rate, sink) = ready_rx.recv().map_err(|_| "the audio thread died".to_string())??;
    Ok(AudioIo { _stop: stop, sample_rate, sink, frames })
}

/// The largest absolute sample in a frame: the level the meter shows.
fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Plays the processed microphone straight back to the speaker until
/// stopped, picking up chain changes as they are made.
async fn mic_test(
    shared: Arc<Shared>,
    mut chain_rx: watch::Receiver<ChainConfig>,
    mut stop: oneshot::Receiver<()>,
) -> Result<(), String> {
    let AudioIo { _stop, sample_rate, mut sink, mut frames } = open_audio()?;
    let mut chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
    loop {
        tokio::select! {
            _ = &mut stop => return Ok(()),
            frame = frames.recv() => {
                let Some(mut samples) = frame else { return Err("the microphone stopped".into()) };
                if chain_rx.has_changed().unwrap_or(false) {
                    chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
                }
                chain.process(&mut samples);
                shared.level.store(peak(&samples).to_bits(), Ordering::Relaxed);
                sink.push(&samples);
            }
        }
    }
}

/// This phone's address on the local network, found by asking the OS
/// which interface it would route to the internet through. Sends nothing.
pub fn local_ip() -> Option<String> {
    let socket = StdUdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:53").ok()?;
    Some(socket.local_addr().ok()?.ip().to_string())
}

/// Resolves what the user typed into an address and, for an invite, the
/// identity that must answer.
async fn resolve(target: &str) -> Result<(SocketAddr, Option<[u8; 32]>), String> {
    let (address, expected) = if target.starts_with("antiphon://") || target.starts_with("voip://") || target.contains('@') {
        let invite = Invite::parse(target).map_err(|e| format!("That invite link is not valid: {e}"))?;
        if invite.address.is_empty() {
            return Err("That invite has no address in it. Ask them for one with their address, or type ip:port.".into());
        }
        (invite.address, Some(invite.identity_key))
    } else {
        (target.to_string(), None)
    };
    if let Ok(addr) = address.parse::<SocketAddr>() {
        return Ok((addr, expected));
    }
    let mut found = tokio::net::lookup_host(address.as_str())
        .await
        .map_err(|e| format!("Could not find {address}: {e}"))?;
    let addr = found
        .find(SocketAddr::is_ipv4)
        .ok_or_else(|| format!("{address} has no IPv4 address"))?;
    Ok((addr, expected))
}

/// Connects, runs the call until hung up, and returns the line to show
/// afterwards. Errors are the same: a line to show.
#[expect(clippy::too_many_arguments, reason = "each argument is a distinct handle the call task owns or borrows")]
async fn run(
    shared: &Arc<Shared>,
    generation: u64,
    wake: &Waker,
    mode: Mode,
    identity: IdentityKeyPair,
    chain_rx: watch::Receiver<ChainConfig>,
    mut stop: oneshot::Receiver<()>,
    facts: &mut Facts,
) -> Result<String, String> {
    if let Mode::Call { target } = &mode {
        facts.remote.clone_from(target);
    }
    let (peer, expected) = match mode {
        Mode::Listen { port } => {
            let socket = Arc::new(AppSocket::from(
                UdpSocket::bind(("0.0.0.0", port)).await.map_err(|e| format!("Could not open port {port}: {e}"))?,
            ));
            let local = format!("{}:{port}", local_ip().unwrap_or_else(|| "0.0.0.0".into()));
            shared.set_phase(generation, Phase::Listening { local: local.clone(), public: None });
            wake();
            if let Ok(Ok(public)) = tokio::time::timeout(STUN_TIMEOUT, antiphon_net::stun::discover_public_addr_default(&socket)).await {
                shared.set_phase(generation, Phase::Listening { local, public: Some(public.to_string()) });
                wake();
            }
            tokio::select! {
                _ = &mut stop => return Ok("Stopped waiting.".into()),
                peer = PeerConnection::listen(socket, &identity) => (peer.map_err(|e| format!("Handshake failed: {e}"))?, None),
            }
        }
        Mode::Call { target } => {
            shared.set_phase(generation, Phase::Dialing { remote: target.clone() });
            wake();
            let (remote, expected) = resolve(&target).await?;
            let socket = Arc::new(AppSocket::from(
                UdpSocket::bind(("0.0.0.0", 0)).await.map_err(|e| format!("Could not open a socket: {e}"))?,
            ));
            tokio::select! {
                _ = &mut stop => return Ok("Call cancelled.".into()),
                peer = PeerConnection::connect(socket, remote, &identity) => (peer.map_err(|e| format!("Could not reach {remote}: {e}"))?, expected),
            }
        }
    };

    let peer_key = peer.peer_identity().identity_key;
    facts.peer_key = Some(peer_key);
    if facts.remote.is_empty() {
        facts.remote = peer.remote_addr().to_string();
    }
    // An invite names who should answer. Someone else answering at that
    // address is exactly the attack the invite's key exists to catch.
    if let Some(expected) = expected {
        if expected != peer_key {
            return Err("Hung up: a different identity answered than the one in the invite. Someone may be intercepting the call.".into());
        }
    }
    let code = verification_code(&identity.public().identity_key, &peer_key);
    shared.set_phase(generation, Phase::Connected { remote: peer.remote_addr().to_string(), code, peer_key });
    facts.connected_secs = Some(now_secs());
    platform::set_call_audio(true, shared.speaker.load(Ordering::Relaxed));
    // CallKit keeps showing "Calling..." (or keeps ringing an incoming
    // call answered in the app) until told the call connected.
    platform::call_connected();
    wake();

    let peer = Arc::new(peer);
    let live = Arc::new(Live {
        peer: peer.clone(),
        peer_key,
        files: Arc::new(Mutex::new(FileTransferState::default())),
        inbox: Mutex::new(Vec::new()),
        runtime: tokio::runtime::Handle::current(),
        video: Arc::new(crate::video::CallVideo::new()),
    });
    *shared.live.lock().unwrap() = Some(live.clone());
    // The transfer pacer slows down while a call shares the link.
    file_transfer::set_call_active(true);
    let result = talk(shared, &live, wake, chain_rx, stop).await;
    file_transfer::set_call_active(false);
    // Whatever arrived of an interrupted download stays on disk with a
    // record of which chunks it is, so offering it again resumes it.
    file_transfer::save_partial_if_interrupted(&live.files);
    live.video.stop_all();
    *shared.live.lock().unwrap() = None;
    result
}

/// The call's audio: the desktop CLI's loop, with push-to-talk read from
/// the on-screen button instead of a keyboard. Control and file frames go
/// to the same handlers the desktop uses.
async fn talk(
    shared: &Arc<Shared>,
    live: &Arc<Live>,
    wake: &Waker,
    mut chain_rx: watch::Receiver<ChainConfig>,
    mut stop: oneshot::Receiver<()>,
) -> Result<String, String> {
    let AudioIo { _stop, sample_rate, mut sink, mut frames } = open_audio()?;
    let peer = live.peer.clone();

    // Who we are, for the other side's screen. Sent a few times a beat
    // apart, as the desktop does, in case the first datagram is lost
    // before their receive loop is up. The account's device list goes
    // first, so it is there when the Hello's device proof is checked.
    {
        let prover = shared.prover.lock().unwrap().clone();
        let hello = Hello {
            display_name: shared.display_name.lock().unwrap().clone(),
            status_message: shared.status_message.lock().unwrap().clone(),
            presence: None,
            wants_silent: false,
            relay: String::new(),
            lends_relay: false,
            routes: crate::push::published_routes(),
            device: prover.as_ref().map(|p| p.hello_proof(&live.peer_key)).unwrap_or_default(),
        };
        let bytes = ControlMessage::Hello(hello).encode();
        let devices = prover.map(|p| p.devices_hex()).filter(|l| !l.is_empty()).map(|list| ControlMessage::Devices { list }.encode());
        // The picture and About line follow in their own message, which
        // an older peer that does not know it simply ignores.
        let card = ControlMessage::Profile(shared.card.lock().unwrap().clone()).encode();
        let peer = peer.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                if let Some(devices) = &devices {
                    let _ = peer.send_control(devices).await;
                }
                let _ = peer.send_control(&bytes).await;
                let _ = peer.send_control(&card).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    // Spec §2 automatic bandwidth: the reports both ways and the plan
    // the encoder and video follow, as on the desktop.
    let link = Arc::new(LinkQuality::default());
    let send_peer = peer.clone();
    let send_shared = shared.clone();
    let send_link = link.clone();
    let send_task = tokio::spawn(async move {
        let mut chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
        // Opus at the tier the link calls for; see `antiphon_net::voice`.
        let mut voice = VoiceSender::new(sample_rate);
        while let Some(mut samples) = frames.recv().await {
            if chain_rx.has_changed().unwrap_or(false) {
                chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
            }
            chain.process(&mut samples);
            if send_shared.transmitting.load(Ordering::Relaxed) {
                send_shared.level.store(peak(&samples).to_bits(), Ordering::Relaxed);
                send_link.tune(&mut voice);
                for (_, bytes) in voice.push(&samples) {
                    if let Err(e) = send_peer.send_audio(&bytes).await {
                        log::warn!("send error: {e}");
                    }
                }
            } else {
                send_shared.level.store(0, Ordering::Relaxed);
                voice.pause();
            }
        }
    });

    let jitter = Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT)));
    let recv_jitter = jitter.clone();
    let recv_live = live.clone();
    let recv_wake = wake.clone();
    let mut recv_voice = VoiceReceiver::new(sample_rate);
    link.watch(0, recv_voice.loss());
    let recv_link = link.clone();
    let mut recv_task = tokio::spawn(async move {
        let files = recv_live.files.clone();
        let peer = recv_live.peer.clone();
        peer.clone()
            .recv_loop(move |msg| match msg {
                Received::Audio { plaintext, .. } => {
                    if let Some(decoded) = recv_voice.decode(&plaintext) {
                        recv_jitter.lock().unwrap().push_with_backup(decoded.sequence as u64, decoded.primary, decoded.backup);
                    }
                }
                Received::Control(bytes) => match ControlMessage::decode(&bytes) {
                    Some(ControlMessage::LinkReport { loss_pct, sent_ms, echo_ms, held_ms }) => {
                        recv_link.on_report(0, loss_pct, sent_ms, echo_ms, held_ms);
                    }
                    // The conversation and its history belong to the
                    // interface, so these wait for it there.
                    Some(
                        message @ (ControlMessage::Hello(_)
                        | ControlMessage::Devices { .. }
                        | ControlMessage::Profile(_)
                        | ControlMessage::Chat { .. }
                        | ControlMessage::ChatEdit { .. }
                        | ControlMessage::ChatDelete { .. }
                        | ControlMessage::ChatReact { .. }),
                    ) => {
                        recv_live.inbox.lock().unwrap().push(message);
                        recv_wake();
                    }
                    // Everything about files is the desktop's state
                    // machine; anything else it does not know (group
                    // calls, introductions) it ignores, as a phone with no
                    // screen for them should.
                    Some(other) => {
                        file_transfer::handle_control_message(other, &files, &peer);
                        recv_wake();
                    }
                    None => {}
                },
                Received::File(bytes) => file_transfer::handle_file_chunk(&bytes, &files, &peer),
                // No wake: a connected call already draws every frame,
                // and each one picks up the newest picture.
                Received::Video(bytes) => recv_live.video.receive(&bytes),
            })
            .await
    });

    let tick_files = live.files.clone();
    let tick_peer = live.peer.clone();
    let tick_wake = wake.clone();
    let tick_live = live.clone();
    let playback_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        let mut last_wake = std::time::Instant::now();
        let mut report_ticks = 0u32;
        loop {
            tick.tick().await;
            report_ticks += 1;
            if report_ticks >= link_quality::REPORT_EVERY_TICKS {
                report_ticks = 0;
                tick_live.video.set_share(link.plan().video_percent);
                let bytes = link.report(0).encode();
                let peer = tick_peer.clone();
                tokio::spawn(async move {
                    let _ = peer.send_control(&bytes).await;
                });
            }
            // Self-throttled inside; riding the audio tick, as the desktop
            // does, saves a timer of its own.
            file_transfer::request_missing_chunks_if_needed(&tick_files, &tick_peer);
            if last_wake.elapsed() >= TRANSFER_WAKE {
                let moving = {
                    let files = tick_files.lock().unwrap();
                    files.outgoing.is_some() || files.incoming.is_some() || files.ready_to_save.is_some()
                };
                if moving {
                    last_wake = std::time::Instant::now();
                    tick_wake();
                }
            }
            loop {
                let popped = jitter.lock().unwrap().pop_ready();
                match popped {
                    PopResult::Frame(samples) | PopResult::Recovered(samples) | PopResult::Concealed(samples) => {
                        sink.push(&samples);
                    }
                    PopResult::Empty => break,
                }
            }
        }
    });

    let message = tokio::select! {
        _ = &mut stop => "Call ended.".to_string(),
        result = &mut recv_task => match result {
            Ok(Err(e)) => format!("Call dropped: {e}"),
            _ => "Call ended by the other side.".to_string(),
        },
    };
    send_task.abort();
    recv_task.abort();
    playback_task.abort();
    Ok(message)
}
