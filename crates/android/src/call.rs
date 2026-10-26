//! The call itself: the desktop CLI's `run_call`, moved behind a handle
//! the interface can drive. The wire format, handshake, FEC and jitter
//! buffer are the shared crates', unchanged, so a phone calls a desktop.

use std::net::{SocketAddr, UdpSocket as StdUdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use antiphon_audio::{start_capture, start_playback, AudioChain, ChainConfig, PlaybackSink};
use antiphon_crypto::{verification_code, IdentityKeyPair, Invite};
use antiphon_net::{JitterBuffer, PeerConnection, PopResult};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, watch};

use crate::platform;

/// Same wait as the desktop builds, so both ends conceal at the same point.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(60);
/// How long to wait for a STUN server before listening without a public
/// address. Listening matters more than knowing the address.
const STUN_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, Default)]
pub enum Phase {
    #[default]
    Idle,
    Listening { local: String, public: Option<String> },
    Dialing { remote: String },
    Connected { remote: String, code: String },
    Ended { message: String },
}

/// What the audio and network tasks share with the interface.
#[derive(Default)]
struct Shared {
    phase: Mutex<Phase>,
    /// Peak level of the processed microphone signal, as f32 bits.
    level: AtomicU32,
    /// Push-to-talk held, or open mic on.
    transmitting: AtomicBool,
    open_mic: AtomicBool,
    ptt_held: AtomicBool,
    speaker: AtomicBool,
    /// Bumped by every new call or mic test, so a task that is still
    /// winding down cannot overwrite the phase of the one that replaced it.
    generation: AtomicU64,
}

impl Shared {
    fn begin(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn set_phase(&self, generation: u64, phase: Phase) {
        let mut current = self.phase.lock().unwrap();
        if self.generation.load(Ordering::SeqCst) == generation {
            *current = phase;
        }
    }

    fn update_transmitting(&self) {
        let on = self.open_mic.load(Ordering::Relaxed) || self.ptt_held.load(Ordering::Relaxed);
        self.transmitting.store(on, Ordering::Relaxed);
    }
}

pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct CallEngine {
    runtime: tokio::runtime::Runtime,
    shared: Arc<Shared>,
    chain: watch::Sender<ChainConfig>,
    wake: Waker,
    /// Ends the running call, listen, or mic test.
    stop: Option<oneshot::Sender<()>>,
    mic_test: bool,
}

impl CallEngine {
    pub fn new(chain: ChainConfig, open_mic: bool, speaker: bool, wake: Waker) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let shared = Arc::new(Shared::default());
        shared.open_mic.store(open_mic, Ordering::Relaxed);
        shared.speaker.store(speaker, Ordering::Relaxed);
        shared.update_transmitting();
        let (chain, _) = watch::channel(chain);
        Ok(Self { runtime, shared, chain, wake, stop: None, mic_test: false })
    }

    pub fn phase(&self) -> Phase {
        self.shared.phase.lock().unwrap().clone()
    }

    pub fn is_busy(&self) -> bool {
        matches!(self.phase(), Phase::Listening { .. } | Phase::Dialing { .. } | Phase::Connected { .. })
    }

    pub fn mic_test_running(&self) -> bool {
        self.mic_test
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed))
    }

    pub fn transmitting(&self) -> bool {
        self.shared.transmitting.load(Ordering::Relaxed)
    }

    pub fn set_ptt(&self, held: bool) {
        self.shared.ptt_held.store(held, Ordering::Relaxed);
        self.shared.update_transmitting();
    }

    pub fn set_open_mic(&self, on: bool) {
        self.shared.open_mic.store(on, Ordering::Relaxed);
        self.shared.update_transmitting();
    }

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

    fn start(&mut self, mode: Mode, identity: IdentityKeyPair) {
        let (stop_tx, stop_rx) = oneshot::channel();
        self.stop = Some(stop_tx);
        let generation = self.shared.begin();
        let shared = self.shared.clone();
        let chain = self.chain.subscribe();
        let wake = self.wake.clone();
        self.runtime.spawn(async move {
            let message = match run(&shared, generation, &wake, mode, identity, chain, stop_rx).await {
                Ok(message) => message,
                Err(e) => e,
            };
            if shared.generation.load(Ordering::SeqCst) == generation {
                platform::set_call_audio(false, false);
                shared.level.store(0, Ordering::Relaxed);
            }
            shared.set_phase(generation, Phase::Ended { message });
            wake();
        });
    }
}

enum Mode {
    Listen { port: u16 },
    Call { target: String },
}

/// Keeps the microphone and speaker open on a thread of their own, so the
/// cpal streams (which are not `Send` on every platform) never cross an
/// `.await`. Dropping it closes both.
struct AudioIo {
    _stop: std_mpsc::Sender<()>,
    sample_rate: u32,
    sink: PlaybackSink,
    frames: mpsc::UnboundedReceiver<Vec<f32>>,
}

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

fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

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
async fn run(
    shared: &Arc<Shared>,
    generation: u64,
    wake: &Waker,
    mode: Mode,
    identity: IdentityKeyPair,
    chain_rx: watch::Receiver<ChainConfig>,
    mut stop: oneshot::Receiver<()>,
) -> Result<String, String> {
    let (peer, expected) = match mode {
        Mode::Listen { port } => {
            let socket = Arc::new(
                UdpSocket::bind(("0.0.0.0", port)).await.map_err(|e| format!("Could not open port {port}: {e}"))?,
            );
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
            let socket = Arc::new(UdpSocket::bind(("0.0.0.0", 0)).await.map_err(|e| format!("Could not open a socket: {e}"))?);
            tokio::select! {
                _ = &mut stop => return Ok("Call cancelled.".into()),
                peer = PeerConnection::connect(socket, remote, &identity) => (peer.map_err(|e| format!("Could not reach {remote}: {e}"))?, expected),
            }
        }
    };

    let peer_key = peer.peer_identity().identity_key;
    // An invite names who should answer. Someone else answering at that
    // address is exactly the attack the invite's key exists to catch.
    if let Some(expected) = expected {
        if expected != peer_key {
            return Err("Hung up: a different identity answered than the one in the invite. Someone may be intercepting the call.".into());
        }
    }
    let code = verification_code(&identity.public().identity_key, &peer_key);
    shared.set_phase(generation, Phase::Connected { remote: peer.remote_addr().to_string(), code });
    platform::set_call_audio(true, shared.speaker.load(Ordering::Relaxed));
    wake();

    talk(shared, Arc::new(peer), chain_rx, stop).await
}

/// The call's audio: the desktop CLI's loop, with push-to-talk read from
/// the on-screen button instead of a keyboard.
async fn talk(
    shared: &Arc<Shared>,
    peer: Arc<PeerConnection>,
    mut chain_rx: watch::Receiver<ChainConfig>,
    mut stop: oneshot::Receiver<()>,
) -> Result<String, String> {
    let AudioIo { _stop, sample_rate, mut sink, mut frames } = open_audio()?;

    let send_peer = peer.clone();
    let send_shared = shared.clone();
    let send_task = tokio::spawn(async move {
        let mut chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
        let mut previous_frame: Option<Vec<f32>> = None;
        let mut sequence: u32 = 0;
        while let Some(mut samples) = frames.recv().await {
            if chain_rx.has_changed().unwrap_or(false) {
                chain = AudioChain::from_config(sample_rate, &chain_rx.borrow_and_update());
            }
            chain.process(&mut samples);
            if send_shared.transmitting.load(Ordering::Relaxed) {
                send_shared.level.store(peak(&samples).to_bits(), Ordering::Relaxed);
                let bytes = antiphon_net::fec::encode(sequence, &samples, previous_frame.as_deref());
                previous_frame = Some(samples);
                sequence = sequence.wrapping_add(1);
                if let Err(e) = send_peer.send_audio(&bytes).await {
                    log::warn!("send error: {e}");
                }
            } else {
                send_shared.level.store(0, Ordering::Relaxed);
                previous_frame = None;
            }
        }
    });

    let jitter = Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT)));
    let recv_jitter = jitter.clone();
    let mut recv_task = tokio::spawn(async move {
        peer.recv_loop(move |msg| {
            if let antiphon_net::Received::Audio { plaintext, .. } = msg {
                if let Some(decoded) = antiphon_net::fec::decode(&plaintext) {
                    recv_jitter.lock().unwrap().push_with_backup(decoded.sequence as u64, decoded.primary, decoded.backup);
                }
            }
            // Profile updates, chat and file offers have no screen here
            // yet, so they are dropped, as the CLI drops them.
        })
        .await
    });

    let playback_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            loop {
                let popped = jitter.lock().unwrap().pop_ready();
                match popped {
                    PopResult::Frame(samples) | PopResult::Recovered(samples) | PopResult::Concealed(samples) => {
                        sink.push(&samples)
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
