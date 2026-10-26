use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use device_query::{DeviceQuery, DeviceState, Keycode};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use std::sync::atomic::{AtomicU32, AtomicU64};

use antiphon_audio::{start_capture, start_playback, AudioChain, CaptureHandle, ChainConfig, EchoCanceller, PlaybackHandle, PlaybackSink};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::{JitterBuffer, PeerConnection, PopResult, Received};

/// How long the jitter buffer waits for a specific missing frame before
/// giving up and concealing it (spec §2 "Adaptive jitter buffer"). Fixed
/// rather than actually adaptive -- see `antiphon_net::JitterBuffer`'s doc comment.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(60);

/// How many recently sent audio frames to keep for answering a resend
/// request (spec section 2 retransmit).
///
/// Sized past the jitter buffer's own ceiling: anything older than that
/// would arrive too late to play, so keeping it would only waste memory.
const RESEND_CACHE_FRAMES: usize = 64;

use crate::control::{ControlMessage, Hello};
use crate::file_transfer::{self, FileTransferState};

/// Recently sent audio frames by sequence number, kept so a peer's
/// `AudioMissing` request can be answered (spec §2 retransmit).
type ResendCache = Arc<Mutex<std::collections::VecDeque<(u32, Vec<u8>)>>>;

/// How the mic decides when to actually transmit (spec §1 "Push-to-Talk /
/// Voice Activation"), and the settings for each mode. Lives in [`AppState`]
/// rather than [`ActiveCall`] because it's a standing user preference, not
/// something that resets between calls -- rebinding the PTT key or flipping
/// to voice activation should take effect immediately on a live call and
/// still be there for the next one.
/// How the microphone decides to open. Stored as a plain integer because
/// it crosses into `.slint` (which has no enums shared with Rust) and lives
/// in an atomic read by the audio send loop on every buffer.
pub const MODE_PTT: u32 = 0;
pub const MODE_VOICE_ACTIVATION: u32 = 1;
pub const MODE_OPEN_MIC: u32 = 2;

pub struct TransmitSettings {
    /// One of [`MODE_PTT`], [`MODE_VOICE_ACTIVATION`], [`MODE_OPEN_MIC`].
    pub transmit_mode: AtomicU32,
    /// Linear peak amplitude (0..1) the mic must cross to open in VA mode.
    pub va_threshold: Mutex<f32>,
    pub ptt_key: Mutex<Keycode>,

    /// Optional shortcuts, `None` when unbound. Polled by the same thread
    /// that watches the PTT key, so they only fire during a call -- which
    /// is the only time any of them would do anything anyway.
    pub mute_key: Mutex<Option<Keycode>>,
    pub deafen_key: Mutex<Option<Keycode>>,
    pub hangup_key: Mutex<Option<Keycode>>,
    pub replay_key: Mutex<Option<Keycode>>,

    /// Set by the key poller when a shortcut is pressed, cleared by the UI
    /// timer once it has acted. A flag rather than a direct call because
    /// the poller is a bare thread with no handle to the Slint window, and
    /// every one of these actions has to run on the UI thread.
    pub pending_mute: AtomicBool,
    pub pending_deafen: AtomicBool,
    pub pending_hangup: AtomicBool,
    pub pending_replay: AtomicBool,

    /// Spec section 1: "Config menu allows PTT or mute/deafen to be
    /// switched to toggle behavior per user." When true the PTT key latches
    /// on press instead of transmitting only while held.
    pub ptt_toggle: AtomicBool,
    /// Latched transmit state for toggle mode. Separate from `ptt_active`
    /// because that one is what the send loop reads, and it also has to
    /// reflect the release delay below.
    pub ptt_latched: AtomicBool,
    /// Spec section 1: "Release delay slider, so the tail of your last word
    /// is not cut off." Milliseconds the mic stays open after the key comes
    /// up.
    pub ptt_release_ms: AtomicU32,
    /// The other half of "PTT or mute/deafen switched to toggle behavior
    /// per user": mute and deafen toggle by default, and either can be
    /// switched to *hold* -- muted only while the key is down
    /// (push-to-mute), deafened only while it is down.
    pub mute_hold: AtomicBool,
    pub deafen_hold: AtomicBool,
}

impl TransmitSettings {
    pub fn new(ptt_key: Keycode, transmit_mode: u32, va_threshold: f32) -> Self {
        Self {
            transmit_mode: AtomicU32::new(transmit_mode),
            va_threshold: Mutex::new(va_threshold),
            ptt_key: Mutex::new(ptt_key),
            mute_key: Mutex::new(None),
            deafen_key: Mutex::new(None),
            hangup_key: Mutex::new(None),
            replay_key: Mutex::new(None),
            pending_mute: AtomicBool::new(false),
            pending_deafen: AtomicBool::new(false),
            pending_hangup: AtomicBool::new(false),
            pending_replay: AtomicBool::new(false),
            ptt_toggle: AtomicBool::new(false),
            ptt_latched: AtomicBool::new(false),
            ptt_release_ms: AtomicU32::new(0),
            mute_hold: AtomicBool::new(false),
            deafen_hold: AtomicBool::new(false),
        }
    }
}

/// Every key physically down right now. The polling backend's answer, plus
/// Caps Lock when the push-to-talk hook is swallowing it -- a swallowed key
/// never reaches the backend (see `caps_lock.rs`).
pub fn keys_down(device_state: &DeviceState) -> HashSet<Keycode> {
    let mut down: HashSet<Keycode> = device_state.get_keys().into_iter().collect();
    if crate::caps_lock::hooked() {
        if crate::caps_lock::held() {
            down.insert(Keycode::CapsLock);
        } else {
            down.remove(&Keycode::CapsLock);
        }
    }
    down
}

/// Waits (up to `timeout`) for the next key press and returns it, for the
/// Settings tab's "click, then press a key" PTT rebind flow. Edge-triggered
/// on a key going from up to down, rather than "is any key down right now",
/// so a key the user happened to already be holding doesn't get captured.
pub fn capture_next_key(timeout: Duration) -> Option<Keycode> {
    let device_state = DeviceState::new();
    let start = Instant::now();
    let mut previously_down = keys_down(&device_state);
    while start.elapsed() < timeout {
        let now_down = keys_down(&device_state);
        if let Some(key) = now_down.iter().find(|k| !previously_down.contains(k)) {
            return Some(*key);
        }
        previously_down = now_down;
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// Everything a live call owns. Dropping this tears the call down: aborts
/// the send/receive tasks, stops the PTT poller, and (via the held handles)
/// stops audio capture/playback.
pub struct ActiveCall {
    /// The echo canceller, shared with `AppState` so the filter it learned
    /// outlives this call (spec §1).
    pub echo_canceller: Arc<Mutex<EchoCanceller>>,
    pub chain: Arc<Mutex<AudioChain>>,
    /// The device's actual sample rate -- can change on a hot-swap to a
    /// device with a different native rate, hence atomic rather than a
    /// plain field. `last_config` lets a swap rebuild the chain for the
    /// new rate without the caller needing to hand the config back in.
    pub sample_rate: AtomicU32,
    last_config: Mutex<ChainConfig>,
    pub muted: Arc<AtomicBool>,
    pub deafened: Arc<AtomicBool>,
    pub ptt_active: Arc<AtomicBool>,
    pub mic_level: Arc<Mutex<f32>>,
    /// The peer's display name/status, once their `Hello` control message
    /// arrives (spec §1 profile info isn't guaranteed to arrive instantly
    /// or at all -- older peers, or a lost UDP packet, just leave this `None`).
    pub peer_profile: Arc<Mutex<Option<Hello>>>,
    /// Kept around so the UI thread can re-send an updated `Hello` (e.g.
    /// after the user edits their profile mid-call) via `AppState::runtime`
    /// -- see `main::sync_transmit_from_ui`'s sibling for profile pushes.
    pub peer: Arc<PeerConnection>,
    /// Live Monitoring (spec §1 Utility): hear your own processed mic in
    /// your headphones. Toggling this doesn't open/close a stream -- the
    /// monitor playback stream runs for the whole call, this just decides
    /// whether the send loop feeds samples into it.
    pub monitor_enabled: Arc<AtomicBool>,
    /// Soundboard audio waiting to be mixed into the outgoing stream
    /// (spec §13). Drained by the send loop a frame at a time, so a clip
    /// rides along with the mic rather than interrupting it.
    pub soundboard_queue: Arc<Mutex<std::collections::VecDeque<f32>>>,
    ptt_stop: Arc<AtomicBool>,

    /// For spec §2's connection-quality readout: the jitter buffer (its
    /// delivered/concealed counters) and when the call went live.
    pub jitter: Arc<Mutex<JitterBuffer>>,
    pub started_at: Instant,
    /// Set when the peer marked this a silent call (spec §9). A request
    /// for less attention, honoured by lowering the outcome -- never a way
    /// to demand more.
    pub peer_wants_silent: Arc<AtomicBool>,
    /// Set when the peer withdrew an earlier knock (spec §9). Withdrawing
    /// their own silencing is all it does -- the receiver's rules are
    /// re-read here and still decide the outcome.
    pub peer_wants_ring: Arc<AtomicBool>,
    /// An incoming "add me" request awaiting the user's answer (spec §3),
    /// holding the requester's display name. `None` when there's nothing
    /// pending.
    pub pending_friend_request: Arc<Mutex<Option<String>>>,
    /// An introduction the peer sent, waiting for the UI thread to act on
    /// it (spec §4). Handled there rather than here because acting on it
    /// means touching the contact list, which outlives the call.
    pub pending_introduction: Arc<Mutex<Option<crate::introduction::Introduction>>>,
    /// Set when the *other* side accepts a request we sent -- the UI timer
    /// drains this and writes the contact to config, since contacts
    /// outlive any single call and so don't belong to `ActiveCall`.
    pub friend_accepted: Arc<Mutex<Option<String>>>,
    /// In-call text chat (spec §4). `chat_rev` bumps on every append so
    /// the UI timer can skip rebuilding its message model on the ~20
    /// ticks/second where nothing changed.
    pub chat_log: Arc<Mutex<Vec<ChatEntry>>>,
    pub chat_rev: Arc<AtomicU32>,
    /// Reactions on this conversation's messages (spec §13).
    pub reactions: Arc<Mutex<Vec<crate::reactions::Reaction>>>,
    /// Next id for a message *we* send. Starts past anything already in
    /// the loaded history so a resumed conversation cannot reuse an id and
    /// have an edit hit the wrong message.
    next_chat_id: Arc<AtomicU64>,
    /// Where this conversation is persisted, when the peer is a known
    /// contact. `None` for a stranger -- nothing is written to disk for
    /// someone you haven't chosen to keep.
    pub history: Option<Arc<crate::history::HistorySink>>,
    /// Messages that have not been confirmed sent (spec §8 outbox).
    ///
    /// Loaded from disk when the call opens, so a message queued in an
    /// earlier session goes out the moment this contact is reachable
    /// again -- which is the whole point of having one.
    pub outbox: Arc<Mutex<Vec<crate::outbox::Queued>>>,
    /// Rolling buffer of the most recent *received* audio
    /// (spec §2 "Replay") -- captured pre-deafen/pre-volume, so it holds
    /// what the peer actually sent, not what happened to be audible.
    replay: Arc<Mutex<std::collections::VecDeque<f32>>>,
    /// Clips cut around damaged moments (spec §2 replay history).
    replay_clips: Arc<Mutex<crate::replay_history::ReplayHistory>>,
    /// File transfer (spec §5) progress/offer state for this call -- see
    /// `crate::file_transfer` for the actual send/receive logic.
    pub file_transfer: Arc<Mutex<FileTransferState>>,

    // Device Hot-Swap (spec §1 Utility: "change microphone or headphones
    // mid-call without dropping"). The stream handles live behind a Mutex
    // so a swap can replace them; the send/recv tasks never own a stream
    // directly, only the channel/sink that survives a swap underneath them.
    capture: Mutex<CaptureHandle>,
    capture_tx: tokio::sync::mpsc::UnboundedSender<Vec<f32>>,
    playback: Mutex<PlaybackHandle>,
    playback_sink: Arc<Mutex<PlaybackSink>>,
    monitor_playback: Mutex<PlaybackHandle>,
    monitor_sink: Arc<Mutex<PlaybackSink>>,

    send_task: JoinHandle<()>,
    recv_task: JoinHandle<()>,
    drain_task: JoinHandle<()>,
}

/// Live Monitoring's playback buffer. Spec asks for "a readout of how much
/// delay you're adding" -- this is that delay: the buffer holds up to this
/// much audio, so worst case that's what's added between your mouth and
/// your ears through the monitor path.
pub const MONITOR_LATENCY_MS: u32 = 120;

/// Extra delay the output device adds on top of our own buffer.
///
/// A rough figure rather than a measurement: cpal does not report device
/// latency portably, and the honest options are an approximation labelled
/// as one, or a precise-looking number that is wrong. Typical shared-mode
/// WASAPI / CoreAudio output sits around here.
pub const DEVICE_LATENCY_ESTIMATE_MS: u32 = 20;

/// Roughly how much delay live monitoring adds, end to end (spec §1's
/// "readout of how much delay you're adding").
///
/// Deliberately reported as approximate. Someone deciding whether
/// monitoring is usable needs the order of magnitude, and a figure
/// presented as exact would be a false claim about hardware we cannot
/// measure.
pub fn monitor_added_latency_ms() -> u32 {
    MONITOR_LATENCY_MS + DEVICE_LATENCY_ESTIMATE_MS
}

impl Drop for ActiveCall {
    fn drop(&mut self) {
        self.send_task.abort();
        self.recv_task.abort();
        self.drain_task.abort();
        self.ptt_stop.store(true, Ordering::Relaxed);
    }
}

impl ActiveCall {
    /// The peer's verified long-term identity (spec §4). Verified by the
    /// handshake, so this is who they provably are -- not who they claim
    /// to be in their profile.
    pub fn peer_identity(&self) -> antiphon_crypto::PublicIdentity {
        self.peer.peer_identity()
    }

    pub fn remote_addr(&self) -> String {
        self.peer.remote_addr().to_string()
    }

    /// Swaps the microphone live: opens a new capture stream on `device`
    /// feeding the same processing/send pipeline, then drops the old
    /// stream. If the new device's native sample rate differs, rebuilds
    /// the DSP chain for it (filter cutoffs etc. are rate-dependent).
    pub fn swap_input_device(&self, device: Option<&str>) -> Result<()> {
        let tx = self.capture_tx.clone();
        let level = self.mic_level.clone();
        let new_capture = start_capture(device, move |samples: &[f32]| {
            let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
            if let Ok(mut lvl) = level.lock() {
                *lvl = peak;
            }
            let _ = tx.send(samples.to_vec());
        })?;

        let new_rate = new_capture.sample_rate;
        if new_rate != self.sample_rate.load(Ordering::Relaxed) {
            *self.chain.lock().unwrap() = AudioChain::from_config(new_rate, &self.last_config.lock().unwrap());
            self.sample_rate.store(new_rate, Ordering::Relaxed);
        }
        *self.capture.lock().unwrap() = new_capture; // old stream drops here, stopping it
        Ok(())
    }

    /// Swaps the output device live for both the call audio and (if
    /// running) the live-monitor stream.
    pub fn swap_output_device(&self, device: Option<&str>) -> Result<()> {
        // Spec §1: a different output device has a different buffer, so
        // the canceller's delay estimate is stale -- but the room has not
        // changed, so the filter it learned is still right. Telling it
        // apart is the difference between a moment of re-alignment and
        // several seconds of audible echo.
        self.echo_canceller.lock().unwrap().device_changed();
        let (new_playback, new_sink) = start_playback(device, 0.5)?;
        *self.playback_sink.lock().unwrap() = new_sink;
        *self.playback.lock().unwrap() = new_playback;

        let (new_monitor, new_monitor_sink) = start_playback(device, MONITOR_LATENCY_MS as f32 / 1000.0)?;
        *self.monitor_sink.lock().unwrap() = new_monitor_sink;
        *self.monitor_playback.lock().unwrap() = new_monitor;
        Ok(())
    }

    /// Rebuilds the DSP chain from a new config and remembers it, so a
    /// later device swap that changes sample rate can rebuild correctly
    /// without the caller re-supplying the config.
    pub fn apply_chain_config(&self, config: ChainConfig) {
        *self.chain.lock().unwrap() = AudioChain::from_config(self.sample_rate.load(Ordering::Relaxed), &config);
        *self.last_config.lock().unwrap() = config;
    }

    /// Starts sending `data` (already read from disk) to the peer as
    /// `file_name`, spawned onto `runtime` -- see `file_transfer::send_file`
    /// for the actual offer/blast/repair lifecycle.
    /// Offers a file (spec §5). `thumbnail_png_b64` is a sender-generated
    /// preview, or empty when there is none.
    /// Offers a file already opened and hashed -- see
    /// `file_integrity::open_and_hash`, which does both in one step so
    /// the hash describes exactly the bytes that will be sent.
    pub fn send_file(
        &self,
        runtime: &tokio::runtime::Handle,
        file_name: String,
        source: crate::file_integrity::FileSource,
        sha256: String,
        thumbnail_png_b64: String,
    ) {
        let peer = self.peer.clone();
        let ft = self.file_transfer.clone();
        runtime.spawn(file_transfer::send_file(
            peer,
            ft,
            file_name,
            Arc::new(source),
            sha256,
            thumbnail_png_b64,
        ));
    }

    /// Accepts the currently pending incoming offer (if any) and replies
    /// `FileAccept`.
    ///
    /// If there is nowhere to write the file, the offer is declined rather
    /// than accepted -- accepting would leave the sender streaming chunks
    /// at a receiver that is dropping every one of them. The reason is
    /// returned so the UI can say why.
    pub fn accept_file_offer(&self, runtime: &tokio::runtime::Handle) -> Option<String> {
        let outcome = file_transfer::accept_offer(&self.file_transfer, &self.peer, runtime)?;
        let peer = self.peer.clone();
        let (message, problem) = match outcome {
            Ok(transfer_id) => (ControlMessage::FileAccept { transfer_id }, None),
            Err((transfer_id, why)) => (ControlMessage::FileReject { transfer_id }, Some(why)),
        };
        runtime.spawn(async move {
            let _ = peer.send_control(&message.encode()).await;
        });
        problem
    }

    pub fn reject_file_offer(&self, runtime: &tokio::runtime::Handle) {
        let Some(transfer_id) = file_transfer::reject_offer(&self.file_transfer) else { return };
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::FileReject { transfer_id }.encode()).await;
        });
    }

    /// Appends to the local chat log and sends the message to the peer
    /// (spec §4). Blank/whitespace-only messages are dropped here rather
    /// than trusting every caller to pre-trim.
    /// Writes the current queue to disk, if this call has somewhere to
    /// write it.
    ///
    /// Errors are reported and swallowed, like history: losing a queue is
    /// bad, but taking down a live call over a disk write is worse.
    fn persist_outbox(&self) {
        let Some(history) = &self.history else { return };
        let queue = self.outbox.lock().unwrap();
        if let Err(e) =
            crate::outbox::save(&history.dir, &history.peer_hex, &history.key, &queue)
        {
            eprintln!("[outbox] could not save queue: {e}");
        }
    }

    /// One line saying what is still waiting, or `None` when nothing is.
    pub fn outbox_summary(&self, peer_name: &str) -> Option<String> {
        crate::outbox::summary(&self.outbox.lock().unwrap(), peer_name)
    }

    /// What to show against one of our own messages (spec §8).
    pub fn delivery_status(&self, id: u64) -> crate::outbox::Status {
        crate::outbox::status_of(&self.outbox.lock().unwrap(), id, true)
    }

    /// Sends everything queued for this contact (spec §8 outbox).
    ///
    /// Called once the call is up. Each message leaves the queue only when
    /// the transport accepts it, so a flush interrupted halfway leaves the
    /// rest to try again rather than losing them.
    pub fn flush_outbox(&self, runtime: &tokio::runtime::Handle) {
        let queued: Vec<crate::outbox::Queued> = self.outbox.lock().unwrap().clone();
        for message in queued {
            self.deliver(runtime, message.id, message.text);
        }
    }

    /// Hands one queued message to the transport and clears it on success.
    fn deliver(&self, runtime: &tokio::runtime::Handle, id: u64, text: String) {
        let peer = self.peer.clone();
        let outbox = self.outbox.clone();
        let history = self.history.clone();
        runtime.spawn(async move {
            if peer.send_control(&ControlMessage::Chat { id, text }.encode()).await.is_err() {
                // Left in the queue on purpose. Clearing it here is what
                // makes a sender's log and a receiver's log disagree.
                return;
            }
            let mut queue = outbox.lock().unwrap();
            if !crate::outbox::mark_delivered(&mut queue, id) {
                return;
            }
            if let Some(history) = history {
                if let Err(e) =
                    crate::outbox::save(&history.dir, &history.peer_hex, &history.key, &queue)
                {
                    eprintln!("[outbox] could not save queue: {e}");
                }
            }
        });
    }

    pub fn send_chat(&self, runtime: &tokio::runtime::Handle, text: String) {
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        let id = self.next_chat_id.fetch_add(1, Ordering::Relaxed);
        self.chat_log.lock().unwrap().push(ChatEntry {
            id,
            from_me: true,
            text: text.clone(),
            edited: false,
            deleted: false,
            pinned: false,
        });
        self.chat_rev.fetch_add(1, Ordering::Relaxed);
        if let Some(history) = &self.history {
            history.record(id, true, &text);
        }
        // Queued *before* the attempt, so a send into a connection that is
        // already dying leaves a record instead of vanishing with its
        // error discarded (spec §8).
        {
            let mut queue = self.outbox.lock().unwrap();
            let at_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            crate::outbox::enqueue(
                &mut queue,
                crate::outbox::Queued { id, text: text.clone(), queued_at_secs: at_secs, attempts: 1 },
            );
        }
        self.persist_outbox();
        self.deliver(runtime, id, text);
    }

    /// Revises one of *our own* messages (spec §13 "edit").
    ///
    /// Refuses to touch a message the peer sent: editing their words would
    /// not be an edit, it would be a forgery.
    pub fn edit_chat(&self, runtime: &tokio::runtime::Handle, id: u64, text: String) -> bool {
        let text = text.trim().to_string();
        if text.is_empty() {
            return false;
        }
        {
            let mut log = self.chat_log.lock().unwrap();
            let Some(entry) = log.iter_mut().find(|e| e.id == id && e.from_me && !e.deleted) else {
                return false;
            };
            entry.text = text.clone();
            entry.edited = true;
        }
        self.chat_rev.fetch_add(1, Ordering::Relaxed);
        if let Some(history) = &self.history {
            history.edit(id, true, &text);
        }
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::ChatEdit { id, text }.encode()).await;
        });
        true
    }

    /// Withdraws one of our own messages (spec §13 "delete").
    pub fn delete_chat(&self, runtime: &tokio::runtime::Handle, id: u64) -> bool {
        {
            let mut log = self.chat_log.lock().unwrap();
            let Some(entry) = log.iter_mut().find(|e| e.id == id && e.from_me) else {
                return false;
            };
            entry.deleted = true;
            entry.edited = false;
            entry.text.clear();
        }
        // Reactions are about a message; without it they are about
        // nothing, and leaving them under "(deleted)" would partly undo
        // the deletion.
        crate::reactions::forget_message(&mut self.reactions.lock().unwrap(), id);
        self.chat_rev.fetch_add(1, Ordering::Relaxed);
        if let Some(history) = &self.history {
            history.delete(id, true);
        }
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::ChatDelete { id }.encode()).await;
        });
        true
    }

    /// Passes a contact's key to the person on this call (spec §4).
    ///
    /// Vouching, not verifying: the receiver is expected to record where
    /// the key came from and still compare a code before trusting it.
    pub fn introduce(
        &self,
        runtime: &tokio::runtime::Handle,
        identity_key: String,
        petname: String,
        note: String,
    ) {
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer
                .send_control(
                    &ControlMessage::Introduce { identity_key, petname, note }.encode(),
                )
                .await;
        });
    }

    /// Adds or removes our reaction to a message (spec §13).
    pub fn react(&self, runtime: &tokio::runtime::Handle, message_id: u64, emoji: String) -> bool {
        if !crate::reactions::toggle(&mut self.reactions.lock().unwrap(), message_id, &emoji, true)
        {
            return false;
        }
        self.chat_rev.fetch_add(1, Ordering::Relaxed);

        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer
                .send_control(&ControlMessage::ChatReact { message_id, emoji }.encode())
                .await;
        });
        true
    }

    /// Pins or unpins a message on this device only (spec §13 "pinned").
    /// The peer is not told: a pin is a note to yourself, not a claim on
    /// their view of the conversation.
    pub fn set_chat_pinned(&self, id: u64, pinned: bool) -> bool {
        {
            let mut log = self.chat_log.lock().unwrap();
            let Some(entry) = log.iter_mut().find(|e| e.id == id) else { return false };
            entry.pinned = pinned;
        }
        self.chat_rev.fetch_add(1, Ordering::Relaxed);
        if let Some(history) = &self.history {
            history.pin(id, pinned);
        }
        true
    }

    /// Marks this call as silent for the other side (spec §9).
    ///
    /// Sent immediately after connecting, as a courtesy. It cannot make
    /// their app ring -- only ask it not to.
    /// Withdraws an earlier knock, asking for the receiver's normal rules
    /// to apply after all (spec §9).
    ///
    /// Does not restart the call: the connection, the keys and the audio
    /// all continue, and only the notification decision is revisited.
    pub fn escalate(&self, runtime: &tokio::runtime::Handle) {
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::EscalateCall.encode()).await;
        });
    }

    pub fn announce_silent(&self, runtime: &tokio::runtime::Handle) {
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::SilentCall.encode()).await;
        });
    }

    /// Asks the peer to add us (spec §3). `display_name` is our own.
    pub fn send_friend_request(&self, runtime: &tokio::runtime::Handle, display_name: String) {
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::FriendRequest { display_name }.encode()).await;
        });
    }

    /// Accepts the pending request, replying with our own display name so
    /// the requester can add us back. Returns the requester's name and
    /// address for the caller to save, or `None` if nothing was pending.
    pub fn accept_friend_request(&self, runtime: &tokio::runtime::Handle, my_display_name: String) -> Option<(String, String)> {
        let their_name = self.pending_friend_request.lock().unwrap().take()?;
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::FriendAccept { display_name: my_display_name }.encode()).await;
        });
        Some((their_name, self.peer.remote_addr().to_string()))
    }

    pub fn reject_friend_request(&self, runtime: &tokio::runtime::Handle) {
        if self.pending_friend_request.lock().unwrap().take().is_none() {
            return;
        }
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::FriendReject.encode()).await;
        });
    }

    /// Drains a "they accepted" notification into `(name, address)` for the
    /// caller to persist.
    pub fn take_friend_accepted(&self) -> Option<(String, String)> {
        let name = self.friend_accepted.lock().unwrap().take()?;
        Some((name, self.peer.remote_addr().to_string()))
    }

    /// A copy of the replay ring right now, oldest sample first, plus the
    /// sample rate to write it out at.
    /// Labels for the kept clips, newest first (spec §2 replay history).
    pub fn replay_clip_labels(&self) -> Vec<String> {
        self.replay_clips.lock().unwrap().clips().map(|c| c.label()).collect()
    }

    /// One kept clip's audio, for playing it back.
    pub fn replay_clip(&self, index: usize) -> Option<(u32, Vec<f32>)> {
        let history = self.replay_clips.lock().unwrap();
        history.get(index).map(|c| (c.sample_rate, c.samples.clone()))
    }

    /// How many clips this call has cut so far (spec §2 replay modes).
    pub fn replay_clips_cut(&self) -> u64 {
        self.replay_clips.lock().unwrap().cut()
    }

    pub fn clear_replay_clips(&self) {
        self.replay_clips.lock().unwrap().clear();
    }

    pub fn take_replay_snapshot(&self) -> (u32, Vec<f32>) {
        let samples: Vec<f32> = self.replay.lock().unwrap().iter().copied().collect();
        (self.sample_rate.load(Ordering::Relaxed), samples)
    }

    /// Cancels whichever file transfer is currently active and notifies
    /// the peer.
    pub fn cancel_file_transfer(&self, runtime: &tokio::runtime::Handle) {
        let Some(transfer_id) = file_transfer::cancel(&self.file_transfer) else { return };
        let peer = self.peer.clone();
        runtime.spawn(async move {
            let _ = peer.send_control(&ControlMessage::FileCancel { transfer_id }.encode()).await;
        });
    }
}

/// One line of the in-call chat log.
#[derive(Clone)]
pub struct ChatEntry {
    /// The id the sender gave this message, so an edit or delete arriving
    /// later can find it (spec §13).
    pub id: u64,
    pub from_me: bool,
    pub text: String,
    pub edited: bool,
    pub deleted: bool,
    pub pinned: bool,
}

impl ChatEntry {
    /// What to show. A deleted message keeps its place with a marker
    /// rather than disappearing, and an edited one says so -- an edit that
    /// looks identical to the original is a way to put words in someone's
    /// mouth after the fact.
    pub fn display_text(&self) -> String {
        if self.deleted {
            "(deleted)".to_string()
        } else if self.edited {
            format!("{} (edited)", self.text)
        } else {
            self.text.clone()
        }
    }
}

/// How much received audio the Replay ring keeps. At 48kHz mono f32 this
/// is ~5.8MB -- cheap enough to always record rather than making Replay
/// something you have to remember to arm before the moment you wanted.
/// How far the other side is lowered while you are talking (spec §1
/// "Ducking"), in dB, and how fast that happens.
///
/// Ducking in a 1:1 call is not the group-call version the spec pictures,
/// but it solves the same problem in the case that exists: without
/// headphones, what you hear from them is picked up by your microphone
/// and sent back, so talking over each other turns into an echo. Lowering
/// them while you speak breaks that loop.
///
/// Ramped rather than switched: a gain that snaps produces an audible
/// click at the start of every word, which is worse than the problem.
pub const DUCK_ATTACK_PER_FRAME: f32 = 0.25;
pub const DUCK_RELEASE_PER_FRAME: f32 = 0.04;

/// How much far-end audio the echo canceller is kept supplied with
/// (spec §1).
///
/// A second at 48 kHz. The reference has to reach back at least as far as
/// the canceller's own delay search, and a little slack past that absorbs
/// the two loops -- capture and playback -- drifting relative to each
/// other. Bounded because a queue nobody drains is a leak: if playback
/// runs ahead the oldest samples are dropped, since the echo of audio a
/// second old is long gone.
const AEC_REFERENCE_SAMPLES: usize = 48_000;

/// When catch-up playback starts, in buffered frames, and how hard it
/// pushes (spec §2: "recovered audio plays slightly sped up so you rejoin
/// live").
///
/// The threshold is what the jitter buffer is already willing to hold at
/// its ceiling plus a margin: below that the backlog is the buffer doing
/// its job, and speeding through it would fight the smoothing it exists
/// to provide. Above it, the backlog is latency nobody asked for -- a
/// stall that already happened, and that you carry for the rest of the
/// call unless something drains it.
///
/// The speed ramps with how far behind you are rather than switching on
/// at full tilt, so a small overshoot is corrected imperceptibly and only
/// a real stall pushes toward the cap.
const CATCH_UP_START_FRAMES: usize = 8;
const CATCH_UP_FULL_FRAMES: usize = 25;

/// How fast to play, given how many frames are waiting.
///
/// Ramped rather than switched so a backlog that is barely over the line
/// is corrected at a speed nobody can hear, and only a genuine stall
/// reaches the cap.
fn catch_up_speed(backlog_frames: usize) -> f32 {
    if backlog_frames <= CATCH_UP_START_FRAMES {
        return 1.0;
    }
    let over = (backlog_frames - CATCH_UP_START_FRAMES) as f32;
    let span = (CATCH_UP_FULL_FRAMES - CATCH_UP_START_FRAMES) as f32;
    1.0 + (antiphon_audio::MAX_SPEED - 1.0) * (over / span).min(1.0)
}

/// Default Replay window. The user can change it within the bounds below;
/// this is where a fresh install starts.
pub const REPLAY_SECONDS: u32 = 30;

/// Bounds on the user-set replay window (spec §2's "user-configurable
/// buffer window").
///
/// Capped rather than open-ended because the ring is uncompressed f32 in
/// memory: at 48 kHz mono that is ~192 KB a second, so a window someone
/// sets to an hour would quietly cost most of a gigabyte.
pub const REPLAY_SECONDS_MIN: u32 = 10;
pub const REPLAY_SECONDS_MAX: u32 = 60;

/// Samples kept for a given window length.
pub fn replay_cap(seconds: u32) -> usize {
    seconds.clamp(REPLAY_SECONDS_MIN, REPLAY_SECONDS_MAX) as usize * 48_000
}

/// Shared application state, reachable from every UI callback closure.
pub struct AppState {
    pub runtime: tokio::runtime::Handle,
    pub call: Mutex<Option<ActiveCall>>,
    pub transmit: Arc<TransmitSettings>,
    /// Master output volume in dB, applied to the peer's audio right
    /// before it plays -- a standing preference like `transmit`, not tied
    /// to any one call, so it's set once here and read live by whichever
    /// call happens to be running.
    pub output_gain_db: Arc<Mutex<f32>>,
    /// Volume trim and mute for the person on the current call (spec §1
    /// "Per-User Volume"), applied after the master level.
    pub peer_volume_db: Arc<Mutex<f32>>,
    pub peer_muted: Arc<AtomicBool>,
    /// The live mic loopback, when running. `None` when off.
    pub loopback: Mutex<Option<LoopbackHandle>>,
    /// Set to stop an in-progress voice-message recording (spec §13).
    pub voice_stop: Arc<AtomicBool>,
    /// Replay window in seconds (spec §2), shared with the running call.
    pub replay_seconds: Arc<AtomicU32>,
    /// Ducking depth in dB (spec §1), shared with the running call. Zero
    /// is off.
    pub duck_db: Arc<AtomicU32>,
    /// Per-contact EQ applied to received audio (spec §1). `None` when the
    /// contact has none configured.
    pub peer_eq: Arc<Mutex<Option<AudioChain>>>,
    /// Whether to cancel the echo of the far end out of the microphone
    /// (spec §1). Shared with the running call, so the switch applies to
    /// the call it is flipped during.
    pub echo_cancel: Arc<AtomicBool>,
    /// The canceller itself, kept across device changes so a converged
    /// filter is not thrown away every time somebody swaps headphones.
    pub echo_canceller: Arc<Mutex<EchoCanceller>>,
    /// The socket STUN discovery ran on, held so the follow-up punch can
    /// reuse it -- see `establish_punch`. Cleared once a call starts.
    pub punch_socket: Mutex<Option<Arc<UdpSocket>>>,
    /// The running port-forward renewal task (spec §10), if one is up.
    ///
    /// Held so it can be stopped -- and the mapping released -- when the
    /// user switches it off. A renewal loop nobody can stop would keep a
    /// hole open in their router for as long as the app runs.
    pub port_map: Mutex<Option<PortMapHandle>>,
    /// This machine's long-term identity (spec §4). Loaded once at
    /// startup and shared by every call, so the user is the same person
    /// across connections rather than a fresh stranger each time.
    pub identity: Arc<IdentityKeyPair>,
}

pub async fn establish_listen(port: u16, identity: Arc<IdentityKeyPair>) -> Result<PeerConnection> {
    let socket = Arc::new(UdpSocket::bind(("0.0.0.0", port)).await?);
    Ok(PeerConnection::listen(socket, &identity).await?)
}

pub async fn establish_call(
    remote: SocketAddr,
    local_port: u16,
    identity: Arc<IdentityKeyPair>,
) -> Result<PeerConnection> {
    let socket = Arc::new(UdpSocket::bind(("0.0.0.0", local_port)).await?);
    Ok(PeerConnection::connect(socket, remote, &identity).await?)
}

/// How long both sides keep punching before giving up. Generous, because
/// the two people are pressing buttons by hand and won't be in sync to the
/// second -- the window has to tolerate that.
pub const PUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Binds `local_port` and asks a STUN server what the outside world sees
/// that socket as. The socket is returned alongside the answer because the
/// discovered mapping belongs to *this* socket -- calling on a different
/// one would get a different (and useless) public port.
pub async fn discover_public_address(local_port: u16) -> Result<(SocketAddr, Arc<UdpSocket>)> {
    let socket = Arc::new(UdpSocket::bind(("0.0.0.0", local_port)).await?);
    let public = antiphon_net::stun::discover_public_addr_default(&socket).await?;
    Ok((public, socket))
}

/// A live automatic port forward, and the task keeping it alive.
pub struct PortMapHandle {
    pub gateway: std::net::Ipv4Addr,
    pub internal_port: u16,
    pub external_port: u16,
    renewal: tokio::task::JoinHandle<()>,
}

impl PortMapHandle {
    /// Stops renewing, and says what needs releasing.
    ///
    /// Split from the release itself because the two callers need
    /// opposite things: the button can spawn and carry on, while the exit
    /// path has to wait or the packet never leaves.
    fn stop(self) -> (std::net::Ipv4Addr, u16) {
        self.renewal.abort();
        (self.gateway, self.internal_port)
    }

    /// Stops renewing and asks the router to drop the mapping, without
    /// waiting for it.
    ///
    /// Best effort: if the release does not land, the mapping expires on
    /// its own within the hour, which is exactly why the requested
    /// lifetime is an hour and not a day.
    pub fn close(self, runtime: &tokio::runtime::Handle) {
        let (gateway, port) = self.stop();
        runtime.spawn(async move {
            antiphon_net::port_map::release_mapping(gateway, port).await;
        });
    }

    /// The same, but waited on -- for shutdown, where a spawned task would
    /// simply never run.
    ///
    /// Capped, because quitting must not hang on a router that has stopped
    /// answering.
    pub fn close_before_exit(self, runtime: &tokio::runtime::Handle) {
        let (gateway, port) = self.stop();
        runtime.block_on(async move {
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(900),
                antiphon_net::port_map::release_mapping(gateway, port),
            )
            .await;
        });
    }
}

/// Asks the router to forward `local_port` inbound, and keeps it forwarded
/// (spec §10 "built-in NAT-PMP client, auto-renewing").
///
/// The renewal loop is the part that matters: NAT-PMP mappings expire by
/// design, so a one-shot request would leave the user reachable for an
/// hour and then quietly not, with nothing on screen changing.
pub async fn open_port_automatically(local_port: u16) -> Result<PortMapHandle> {
    let local = antiphon_net::port_map::local_ipv4()
        .await
        .ok_or_else(|| anyhow::anyhow!("this machine has no IPv4 address on a local network"))?;
    let (gateway, mapping) = antiphon_net::port_map::open_port(local, local_port).await?;

    let renewal = tokio::spawn(async move {
        let mut interval = mapping.renew_after();
        loop {
            tokio::time::sleep(interval).await;
            match antiphon_net::port_map::request_mapping(gateway, local_port).await {
                Ok(renewed) => interval = renewed.renew_after(),
                // Kept trying rather than given up on: a router that was
                // rebooting a moment ago will answer the next one, and
                // stopping here would silently end the user's
                // reachability. Backed off so a router that is simply
                // gone is not hammered.
                Err(_) => interval = interval.max(std::time::Duration::from_secs(60)),
            }
        }
    });

    Ok(PortMapHandle {
        gateway,
        internal_port: mapping.internal_port,
        external_port: mapping.external_port,
        renewal,
    })
}

/// Hole-punches to `remote`, reusing an already-bound socket when one is
/// supplied. Reuse matters: if the user just pressed "Find my address",
/// that socket is the one whose public mapping they shared, so punching
/// from a fresh socket would aim at the right place from the wrong port.
pub async fn establish_punch(
    remote: SocketAddr,
    local_port: u16,
    existing: Option<Arc<UdpSocket>>,
    identity: Arc<IdentityKeyPair>,
) -> Result<PeerConnection> {
    let socket = match existing {
        Some(s) => s,
        None => Arc::new(UdpSocket::bind(("0.0.0.0", local_port)).await?),
    };
    Ok(PeerConnection::punch(socket, remote, PUNCH_TIMEOUT, &identity).await?)
}

/// Captures audio until `stop` is set, for voice messages (spec §13).
///
/// Returns the raw samples and the device's rate; the caller runs them
/// through the mic chain afterwards rather than here, so the same
/// recording can be re-processed if settings change.
///
/// Blocks, so call it from `spawn_blocking` -- never the UI thread.
pub fn record_until(
    input_device: Option<&str>,
    stop: Arc<AtomicBool>,
    max_seconds: u32,
) -> Result<(u32, Vec<f32>)> {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorded_cb = recorded.clone();
    let capture = start_capture(input_device, move |samples: &[f32]| {
        if let Ok(mut buf) = recorded_cb.lock() {
            buf.extend_from_slice(samples);
        }
    })?;
    let sample_rate = capture.sample_rate;
    let cap = sample_rate as usize * max_seconds as usize;

    // Polled rather than woken: the cap has to be enforced from here too,
    // and a recording that ran past it because nobody was watching would
    // be a surprise at send time.
    while !stop.load(Ordering::Relaxed) {
        if recorded.lock().map(|b| b.len() >= cap).unwrap_or(true) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(capture);

    let mut samples = std::mem::take(&mut *recorded.lock().unwrap());
    samples.truncate(cap);
    Ok((sample_rate, samples))
}

/// Watches for `key` being pressed, up to `timeout`.
///
/// Used right after binding a push-to-talk key to confirm the app can
/// actually see it (spec §1). Some keys bind fine and are then swallowed
/// by another program or the window manager, and the resulting silence is
/// blamed on the microphone rather than the binding.
///
/// Blocks, so call it from `spawn_blocking`.
pub fn watch_for_key(key: Keycode, timeout: Duration) -> bool {
    let device_state = DeviceState::new();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if keys_down(&device_state).contains(&key) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Plays a buffer once through `output_device`.
///
/// The returned handle must be kept alive until playback finishes --
/// dropping it stops the stream immediately, cutting the sound off.
pub fn play_clip(output_device: Option<&str>, sample_rate: u32, samples: &[f32]) -> Result<PlaybackHandle> {
    let seconds = samples.len() as f32 / sample_rate.max(1) as f32 + 0.5;
    let (handle, mut sink) = start_playback(output_device, seconds)?;
    sink.push(samples);
    Ok(handle)
}

/// A live microphone loopback: what you say, processed by the chain, back
/// in your own ears (spec §1 "Live Monitoring").
///
/// Deliberately standalone rather than reusing the call pipeline's monitor
/// tap. That tap only exists while a call is running, and the whole point
/// of this control is checking your microphone *before* you call anyone.
///
/// Dropping the handle stops both streams and the worker thread.
pub struct LoopbackHandle {
    _capture: CaptureHandle,
    _playback: PlaybackHandle,
    stop: Arc<AtomicBool>,
    level: Arc<Mutex<f32>>,
}

impl LoopbackHandle {
    /// Current mic peak (0..1), so the level meter keeps working when
    /// there is no call to read it from.
    pub fn level(&self) -> f32 {
        self.level.lock().map(|l| *l).unwrap_or(0.0)
    }
}

impl Drop for LoopbackHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts a live loopback through `config`'s chain.
///
/// Same structure as the call pipeline: the capture callback does nothing
/// but hand samples to a channel, and a worker thread does the DSP. Audio
/// callbacks run on a realtime thread, so doing filter work (or taking a
/// lock someone else might hold) inside one is how you get dropouts.
/// How long the spec §1 mic test records for.
pub const MIC_TEST_SECONDS: u64 = 5;

/// Records `seconds` of microphone and returns it processed through the
/// chain, exactly as a call would send it (spec §1 "Mic Test & Record
/// Preview": "Record five seconds and play it back so you hear exactly
/// what everyone else hears").
///
/// The chain runs over the recording after the fact rather than live, so
/// nothing is heard while recording -- which is the point: you listen back
/// once, without your own voice in your ears confusing the comparison.
/// Blocks for the whole recording, so call it from `spawn_blocking`.
pub fn record_mic_test(input_device: Option<&str>, config: &ChainConfig, seconds: u64) -> Result<(u32, Vec<f32>)> {
    let recorded = Arc::new(Mutex::new(Vec::<f32>::new()));
    let sink = recorded.clone();
    let capture = start_capture(input_device, move |samples: &[f32]| {
        if let Ok(mut buf) = sink.lock() {
            buf.extend_from_slice(samples);
        }
    })?;
    let sample_rate = capture.sample_rate;
    std::thread::sleep(Duration::from_secs(seconds));
    drop(capture);

    let mut samples = std::mem::take(&mut *recorded.lock().unwrap());
    let mut chain = AudioChain::from_config(sample_rate, config);
    // Same 10 ms blocks a device callback hands the chain, so stages with
    // per-block state (meters, envelopes) behave as they do live.
    let block = (sample_rate as usize / 100).max(1);
    for chunk in samples.chunks_mut(block) {
        chain.process(chunk);
    }
    Ok((sample_rate, samples))
}

pub fn start_loopback(
    input_device: Option<&str>,
    output_device: Option<&str>,
    config: &ChainConfig,
) -> Result<LoopbackHandle> {
    let (playback, mut sink) = start_playback(output_device, MONITOR_LATENCY_MS as f32 / 1000.0)?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
    let level = Arc::new(Mutex::new(0.0f32));
    let level_cb = level.clone();

    let capture = start_capture(input_device, move |samples: &[f32]| {
        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if let Ok(mut lvl) = level_cb.lock() {
            *lvl = peak;
        }
        let _ = tx.send(samples.to_vec());
    })?;

    // Only known once the device is open, which is why the chain is built
    // here rather than before the capture starts.
    let sample_rate = capture.sample_rate;
    let mut chain = AudioChain::from_config(sample_rate, config);

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    std::thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(mut buf) => {
                    chain.process(&mut buf);
                    sink.push(&buf);
                }
                // A timeout just means a quiet moment; the sender going
                // away means the capture stream is gone, so stop.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    Ok(LoopbackHandle { _capture: capture, _playback: playback, stop, level })
}

/// Wires a connected [`PeerConnection`] up to the mic chain and audio
/// devices and starts the call running. Mirrors `antiphon-cli`'s `run_call`,
/// but returns live handles instead of blocking, since the GUI needs to
/// keep going (settings can change mid-call, mute/deafen can toggle, etc).
// One call site, and every argument is a distinct live handle the call
// needs -- a parameter struct would only move the list.
#[allow(clippy::too_many_arguments)]
pub fn start_pipeline(
    peer: PeerConnection,
    config: ChainConfig,
    input_device: Option<String>,
    output_device: Option<String>,
    transmit: Arc<TransmitSettings>,
    output_gain_db: Arc<Mutex<f32>>,
    // How many seconds of received audio the Replay ring keeps (spec §2).
    // Owned by `AppState` so a change applies to the running call.
    replay_seconds: Arc<AtomicU32>,
    // How far to duck the other side while you talk (spec §1), in dB.
    // Owned by `AppState` so a change applies to the running call.
    duck_db: Arc<AtomicU32>,
    // A chain applied to the *received* audio (spec §1 "Per-User EQ"),
    // or `None` when this contact has none. Owned by `AppState` so it can
    // be rebuilt mid-call.
    peer_eq: Arc<Mutex<Option<AudioChain>>>,
    // Spec §1 echo cancellation. Both come from `AppState` so the switch
    // and the learned filter outlive any one call.
    echo_cancel: Arc<AtomicBool>,
    echo_canceller: Arc<Mutex<EchoCanceller>>,
    // Per-person volume trim and mute (spec §1). Owned by `AppState` so
    // they outlive any single call; passed in here purely so the playback
    // loop can read them on every frame.
    peer_volume_db: Arc<Mutex<f32>>,
    peer_muted: Arc<AtomicBool>,
    local_hello: Hello,
    history: Option<Arc<crate::history::HistorySink>>,
) -> Result<ActiveCall> {
    let peer = Arc::new(peer);

    let muted = Arc::new(AtomicBool::new(false));
    let deafened = Arc::new(AtomicBool::new(false));
    let ptt_active = Arc::new(AtomicBool::new(false));
    let ptt_stop = Arc::new(AtomicBool::new(false));
    let mic_level = Arc::new(Mutex::new(0.0f32));
    let peer_profile: Arc<Mutex<Option<Hello>>> = Arc::new(Mutex::new(None));
    let monitor_enabled = Arc::new(AtomicBool::new(false));
    let soundboard_queue: Arc<Mutex<std::collections::VecDeque<f32>>> =
        Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let file_transfer_state = Arc::new(Mutex::new(FileTransferState::default()));
    // Anything queued for this contact in an earlier session (spec §8).
    // Loaded before the pipeline starts so a flush has something to send
    // the moment the call is up.
    let outbox: Arc<Mutex<Vec<crate::outbox::Queued>>> = Arc::new(Mutex::new({
        let mut queue = history
            .as_ref()
            .map(|h| crate::outbox::load(&h.dir, &h.peer_hex, &h.key))
            .unwrap_or_default();
        // The give-up window is applied on read, like history's expiry, so
        // a window that ran out while the app was closed takes effect the
        // moment anyone looks -- not whenever a timer next fires.
        let give_up = match crate::config::load().outbox_give_up_hours {
            0 => crate::outbox::GiveUp::Never,
            hours => crate::outbox::GiveUp::AfterSecs(hours as u64 * 3600),
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dropped = crate::outbox::drop_expired(&mut queue, give_up, now);
        if !dropped.is_empty() {
            // Said out loud rather than dropped quietly: a message
            // vanishing without a word is what this module exists to
            // prevent, and the give-up window is no exception.
            eprintln!(
                "[outbox] gave up on {} undelivered message(s) past the retry window",
                dropped.len()
            );
            if let Some(h) = history.as_ref() {
                let _ = crate::outbox::save(&h.dir, &h.peer_hex, &h.key, &queue);
            }
        }
        queue
    }));

    // Past messages with this contact, so the chat panel opens where the
    // last conversation left off instead of blank.
    let previous: Vec<ChatEntry> = history
        .as_ref()
        .map(|h| {
            h.load_all()
                .into_iter()
                .map(|m| ChatEntry {
                    id: m.id,
                    from_me: m.from_me,
                    text: m.text,
                    edited: m.edited,
                    deleted: m.deleted,
                    pinned: m.pinned,
                })
                .collect()
        })
        .unwrap_or_default();
    // Past ids must never be reused: an edit naming a recycled id would
    // silently rewrite an older message instead of the intended one.
    let next_chat_id = Arc::new(AtomicU64::new(
        previous.iter().map(|e| e.id).max().map(|m| m + 1).unwrap_or(0),
    ));
    let chat_log: Arc<Mutex<Vec<ChatEntry>>> = Arc::new(Mutex::new(previous));
    let chat_rev = Arc::new(AtomicU32::new(0));
    let reactions: Arc<Mutex<Vec<crate::reactions::Reaction>>> = Arc::new(Mutex::new(Vec::new()));
    let peer_wants_silent = Arc::new(AtomicBool::new(false));
    let peer_wants_ring = Arc::new(AtomicBool::new(false));
    let pending_introduction: Arc<Mutex<Option<crate::introduction::Introduction>>> =
        Arc::new(Mutex::new(None));
    let pending_friend_request: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let friend_accepted: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let replay: Arc<Mutex<std::collections::VecDeque<f32>>> = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let replay_clips: Arc<Mutex<crate::replay_history::ReplayHistory>> =
        Arc::new(Mutex::new(crate::replay_history::ReplayHistory::default()));
    // Recently sent audio, so a peer's resend request can be answered.
    let resend_cache: ResendCache = Arc::new(Mutex::new(std::collections::VecDeque::new()));

    let (monitor_playback, monitor_sink) = start_playback(output_device.as_deref(), MONITOR_LATENCY_MS as f32 / 1000.0)?;
    let monitor_sink = Arc::new(Mutex::new(monitor_sink));

    // Best-effort: sent a few times a beat apart in case the first UDP
    // datagram is lost before the peer's socket is even listening yet.
    {
        let hello_peer = peer.clone();
        tokio::spawn(async move {
            let bytes = ControlMessage::Hello(local_hello).encode();
            for _ in 0..3 {
                let _ = hello_peer.send_control(&bytes).await;
                tokio::time::sleep(Duration::from_millis(400)).await;
            }
        });
    }

    {
        let flag = ptt_active.clone();
        let stop = ptt_stop.clone();
        let transmit = transmit.clone();
        std::thread::spawn(move || {
            let device_state = DeviceState::new();
            let mut previously_down: HashSet<Keycode> = HashSet::new();
            // When the PTT key came up, while the release tail is still
            // running. `None` when not in a release.
            let mut release_pending_since: Option<Instant> = None;
            while !stop.load(Ordering::Relaxed) {
                let down = keys_down(&device_state);

                let ptt = *transmit.ptt_key.lock().unwrap();
                let held = down.contains(&ptt);
                // Caps Lock as push-to-talk flips the lock on platforms
                // where it can't be swallowed; put it back on release.
                if ptt == Keycode::CapsLock && !held && previously_down.contains(&ptt) {
                    crate::caps_lock::after_release();
                }

                if transmit.ptt_toggle.load(Ordering::Relaxed) {
                    // Toggle mode: latch on the press edge. Level-triggering
                    // here would flip the latch every poll for as long as
                    // the key was down.
                    if held && !previously_down.contains(&ptt) {
                        let now = !transmit.ptt_latched.load(Ordering::Relaxed);
                        transmit.ptt_latched.store(now, Ordering::Relaxed);
                    }
                    flag.store(transmit.ptt_latched.load(Ordering::Relaxed), Ordering::Relaxed);
                    release_pending_since = None;
                } else if held {
                    // Held: transmit, and cancel any release tail in flight.
                    transmit.ptt_latched.store(false, Ordering::Relaxed);
                    flag.store(true, Ordering::Relaxed);
                    release_pending_since = None;
                } else {
                    // Let go: keep transmitting for the release delay so the
                    // tail of the last word is not clipped off mid-syllable.
                    let release =
                        Duration::from_millis(transmit.ptt_release_ms.load(Ordering::Relaxed) as u64);
                    if release.is_zero() {
                        flag.store(false, Ordering::Relaxed);
                    } else {
                        let since = *release_pending_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= release {
                            flag.store(false, Ordering::Relaxed);
                            release_pending_since = None;
                        }
                    }
                }

                // The shortcuts are edge-triggered instead: holding mute
                // should toggle once, not flap every 10ms.
                let newly_pressed = |k: &Mutex<Option<Keycode>>| {
                    k.lock().unwrap().is_some_and(|key| down.contains(&key) && !previously_down.contains(&key))
                };
                // Hold mode toggles on both edges: press mutes, release
                // unmutes, through the same pending flag the UI already acts
                // on, so the two modes cannot drift apart.
                let newly_released = |k: &Mutex<Option<Keycode>>| {
                    k.lock().unwrap().is_some_and(|key| !down.contains(&key) && previously_down.contains(&key))
                };
                if newly_pressed(&transmit.mute_key)
                    || (transmit.mute_hold.load(Ordering::Relaxed) && newly_released(&transmit.mute_key))
                {
                    transmit.pending_mute.store(true, Ordering::Relaxed);
                }
                if newly_pressed(&transmit.deafen_key)
                    || (transmit.deafen_hold.load(Ordering::Relaxed) && newly_released(&transmit.deafen_key))
                {
                    transmit.pending_deafen.store(true, Ordering::Relaxed);
                }
                if newly_pressed(&transmit.hangup_key) {
                    transmit.pending_hangup.store(true, Ordering::Relaxed);
                }
                if newly_pressed(&transmit.replay_key) {
                    transmit.pending_replay.store(true, Ordering::Relaxed);
                }

                previously_down = down;
                std::thread::sleep(Duration::from_millis(10));
            }
        });
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<f32>>();
    let capture_tx = tx.clone();
    let level_for_capture = mic_level.clone();
    let capture = start_capture(input_device.as_deref(), move |samples: &[f32]| {
        let peak = samples.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        if let Ok(mut lvl) = level_for_capture.lock() {
            *lvl = peak;
        }
        let _ = tx.send(samples.to_vec());
    })?;
    let sample_rate = capture.sample_rate;
    let chain = Arc::new(Mutex::new(AudioChain::from_config(sample_rate, &config)));

    let send_peer = peer.clone();
    let send_muted = muted.clone();
    let send_ptt = ptt_active.clone();
    let send_chain = chain.clone();
    let send_level = mic_level.clone();
    let send_transmit = transmit.clone();
    let send_monitor = monitor_enabled.clone();
    let send_resend_cache = resend_cache.clone();
    // Spec §1: far-end audio waiting to be used as the echo canceller's
    // reference. Shared between the two loops because the echo of what
    // playback just produced arrives in a microphone block captured later.
    let aec_far: Arc<Mutex<std::collections::VecDeque<f32>>> =
        Arc::new(Mutex::new(std::collections::VecDeque::new()));

    let send_soundboard = soundboard_queue.clone();
    let send_aec = echo_canceller.clone();
    let send_aec_far = aec_far.clone();
    let send_echo_cancel = echo_cancel.clone();
    let send_monitor_sink = monitor_sink.clone();
    let send_task = tokio::spawn(async move {
        // The last frame actually transmitted, carried as a backup copy on
        // the next one (spec section 2 forward error correction).
        let mut previous_frame: Option<Vec<f32>> = None;
        // Application frame sequence, independent of the crypto counter.
        let mut sequence: u32 = 0;
        while let Some(mut samples) = rx.recv().await {
            // Spec §1 echo cancellation, before the mic chain: the
            // gate and compressor are tuned for a voice, and running them
            // over a signal that still has the far end mixed into it
            // would have them reacting to the other person's speech.
            if send_echo_cancel.load(Ordering::Relaxed) {
                let far = {
                    let mut queue = send_aec_far.lock().unwrap();
                    // Zero-padded when playback has produced less than the
                    // microphone consumed: nothing was played, so there is
                    // no echo to cancel, and that is exactly what silence
                    // in the reference means.
                    let mut block = Vec::with_capacity(samples.len());
                    for _ in 0..samples.len() {
                        block.push(queue.pop_front().unwrap_or(0.0));
                    }
                    block
                };
                if let Ok(mut aec) = send_aec.lock() {
                    aec.process(&mut samples, &far);
                }
            }
            if let Ok(mut c) = send_chain.lock() {
                c.process(&mut samples);
            }
            // Soundboard clips are mixed in *after* the mic chain, so a
            // clip is not gated, compressed or pitch-shifted by settings
            // meant for a voice -- it should sound the way the file does.
            {
                let mut queue = send_soundboard.lock().unwrap();
                if !queue.is_empty() {
                    for sample in samples.iter_mut() {
                        let Some(clip) = queue.pop_front() else { break };
                        *sample = (*sample + clip).clamp(-1.0, 1.0);
                    }
                }
            }
            if send_monitor.load(Ordering::Relaxed) {
                send_monitor_sink.lock().unwrap().push(&samples);
            }
            let should_send = match send_transmit.transmit_mode.load(Ordering::Relaxed) {
                MODE_VOICE_ACTIVATION => {
                    let level = send_level.lock().map(|l| *l).unwrap_or(0.0);
                    level > *send_transmit.va_threshold.lock().unwrap()
                }
                MODE_OPEN_MIC => true,
                _ => send_ptt.load(Ordering::Relaxed),
            };
            if should_send && !send_muted.load(Ordering::Relaxed) {
                // Spec §2, layer one: every frame carries a low-quality
                // copy of the one before it, so a single lost frame is
                // repaired with no round trip and no added delay.
                let bytes = antiphon_net::fec::encode(sequence, &samples, previous_frame.as_deref());
                previous_frame = Some(samples.clone());
                // Kept so a resend request can be answered without
                // re-running the mic chain.
                {
                    let mut cache = send_resend_cache.lock().unwrap();
                    cache.push_back((sequence, bytes.clone()));
                    while cache.len() > RESEND_CACHE_FRAMES {
                        cache.pop_front();
                    }
                }
                sequence = sequence.wrapping_add(1);
                if let Err(e) = send_peer.send_audio(&bytes).await {
                    eprintln!("[net] send error: {e}");
                }
            } else {
                // Cleared while not transmitting, so the first frame after
                // a pause doesn't offer a backup copy of audio from before
                // the silence -- repairing a gap with unrelated speech is
                // worse than leaving it.
                previous_frame = None;
            }
        }
    });

    let (playback, playback_sink) = start_playback(output_device.as_deref(), 0.5)?;
    let playback_sink = Arc::new(Mutex::new(playback_sink));

    // Frames land here in *arrival* order (UDP doesn't guarantee delivery
    // order); the drain task below releases them in *sequence* order,
    // reordering short-range jitter and concealing gaps that never fill in
    // (spec §2 "Adaptive jitter buffer" + "Replay").
    let jitter = Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT)));

    let recv_peer = peer.clone();
    let recv_jitter = jitter.clone();
    let recv_profile = peer_profile.clone();
    let recv_ft_state = file_transfer_state.clone();
    let recv_ft_peer = peer.clone();
    let recv_chat_log = chat_log.clone();
    let recv_chat_rev = chat_rev.clone();
    let recv_reactions = reactions.clone();
    let recv_history = history.clone();
    let recv_resend_cache = resend_cache.clone();
    let recv_resend_peer = peer.clone();
    let recv_silent = peer_wants_silent.clone();
    let recv_ring = peer_wants_ring.clone();
    let recv_introduction = pending_introduction.clone();
    let recv_friend_pending = pending_friend_request.clone();
    let recv_friend_accepted = friend_accepted.clone();
    let recv_task = tokio::spawn(async move {
        let result = recv_peer
            .recv_loop(move |msg| match msg {
                Received::Audio { counter: _, plaintext } => {
                    // A frame that won't parse is dropped like a lost one;
                    // the buffer already knows how to handle a gap.
                    //
                    // Ordered by the *application* sequence inside the
                    // payload, not the crypto counter outside it, so a
                    // resend lands in the slot it belongs to.
                    if let Some(decoded) = antiphon_net::fec::decode(&plaintext) {
                        recv_jitter.lock().unwrap().push_with_backup(
                            decoded.sequence as u64,
                            decoded.primary,
                            decoded.backup,
                        );
                    }
                }
                Received::Control(bytes) => match ControlMessage::decode(&bytes) {
                    Some(ControlMessage::Hello(hello)) => {
                        // Spec §9: the caller said up front that this is a
                        // knock. Handled here, on the same flag the
                        // separate `SilentCall` message raises, so both
                        // paths reach the one place that can only *lower*
                        // the receiver's outcome.
                        if hello.wants_silent {
                            recv_silent.store(true, Ordering::Relaxed);
                        }
                        *recv_profile.lock().unwrap() = Some(hello);
                    }
                    Some(ControlMessage::AudioMissing { sequences }) => {
                        // Best effort: anything already aged out of the
                        // cache is simply not resent, and the receiver's
                        // concealment covers it.
                        let to_send: Vec<Vec<u8>> = {
                            let cache = recv_resend_cache.lock().unwrap();
                            sequences
                                .iter()
                                .filter_map(|seq| {
                                    cache.iter().find(|(s, _)| s == seq).map(|(_, b)| b.clone())
                                })
                                .collect()
                        };
                        let peer = recv_resend_peer.clone();
                        tokio::spawn(async move {
                            for bytes in to_send {
                                let _ = peer.send_audio(&bytes).await;
                            }
                        });
                    }
                    Some(ControlMessage::Introduce { identity_key, petname, note }) => {
                        *recv_introduction.lock().unwrap() =
                            Some(crate::introduction::Introduction {
                                identity_key,
                                petname,
                                note,
                            });
                    }
                    Some(ControlMessage::EscalateCall) => {
                        recv_ring.store(true, Ordering::Relaxed);
                    }
                    Some(ControlMessage::SilentCall) => {
                        recv_silent.store(true, Ordering::Relaxed);
                    }
                    Some(ControlMessage::Chat { id, text }) => {
                        if let Some(history) = &recv_history {
                            history.record(id, false, &text);
                        }
                        recv_chat_log.lock().unwrap().push(ChatEntry {
                            id,
                            from_me: false,
                            text,
                            edited: false,
                            deleted: false,
                            pinned: false,
                        });
                        recv_chat_rev.fetch_add(1, Ordering::Relaxed);
                    }
                    // An edit or delete may only touch a message the *peer*
                    // sent. `from_me: false` is the whole check: without it,
                    // a peer could rewrite our own words in our own log.
                    Some(ControlMessage::ChatEdit { id, text }) => {
                        let mut log = recv_chat_log.lock().unwrap();
                        if let Some(entry) =
                            log.iter_mut().find(|e| e.id == id && !e.from_me && !e.deleted)
                        {
                            entry.text = text.clone();
                            entry.edited = true;
                            drop(log);
                            if let Some(history) = &recv_history {
                                history.edit(id, false, &text);
                            }
                            recv_chat_rev.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Some(ControlMessage::ChatReact { message_id, emoji }) => {
                        // `from_me: false` -- this is their reaction, and a
                        // toggle only ever affects the sender's own.
                        if crate::reactions::toggle(
                            &mut recv_reactions.lock().unwrap(),
                            message_id,
                            &emoji,
                            false,
                        ) {
                            recv_chat_rev.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Some(ControlMessage::ChatDelete { id }) => {
                        let mut log = recv_chat_log.lock().unwrap();
                        if let Some(entry) = log.iter_mut().find(|e| e.id == id && !e.from_me) {
                            entry.deleted = true;
                            entry.edited = false;
                            entry.text.clear();
                            drop(log);
                            crate::reactions::forget_message(
                                &mut recv_reactions.lock().unwrap(),
                                id,
                            );
                            if let Some(history) = &recv_history {
                                history.delete(id, false);
                            }
                            recv_chat_rev.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Some(ControlMessage::FriendRequest { display_name }) => {
                        *recv_friend_pending.lock().unwrap() = Some(display_name);
                    }
                    Some(ControlMessage::FriendAccept { display_name }) => {
                        *recv_friend_accepted.lock().unwrap() = Some(display_name);
                    }
                    Some(ControlMessage::FriendReject) => {}
                    Some(other) => file_transfer::handle_control_message(other, &recv_ft_state, &recv_ft_peer),
                    None => {}
                },
                Received::File(bytes) => file_transfer::handle_file_chunk(&bytes, &recv_ft_state, &recv_ft_peer),
            })
            .await;
        if let Err(e) = result {
            eprintln!("[net] receive loop ended: {e}");
        }
    });

    let drain_sink = playback_sink.clone();
    let drain_deafened = deafened.clone();
    let drain_ft_state = file_transfer_state.clone();
    let drain_ft_peer = peer.clone();
    let drain_replay = replay.clone();
    let drain_replay_clips = replay_clips.clone();
    // Spec §2 catch-up playback. Held across frames because a splice
    // spans them: it needs the tail of the last frame to line up against.
    let mut catch_up = antiphon_audio::TimeCompressor::new(sample_rate);
    let mut catch_up_out: Vec<f32> = Vec::new();
    let drain_aec_far = aec_far.clone();
    let mut clip_cutter = crate::replay_history::ClipCutter::default();
    // Samples drained since the call started, which is what a clip's
    // timestamp is measured against. Counted here rather than read from a
    // wall clock so a clip's label matches the audio it contains even if
    // the stream stalled.
    let mut drained_samples: u64 = 0;
    let stats_jitter = jitter.clone();
    let drain_nack_peer = peer.clone();
    // Current ducking amount in dB, ramped toward its target each frame.
    let mut duck_level = 0.0f32;
    let drain_peer_volume = peer_volume_db.clone();
    let drain_replay_seconds = replay_seconds.clone();
    let drain_duck_db = duck_db.clone();
    let drain_peer_eq = peer_eq.clone();
    let drain_ptt = ptt_active.clone();
    let drain_peer_muted = peer_muted.clone();
    let drain_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(10));
        loop {
            tick.tick().await;
            // Cheap no-op most ticks (a single mutex check, internally
            // throttled to once every couple of seconds) -- riding the
            // existing 10ms tick instead of a dedicated timer.
            file_transfer::request_missing_chunks_if_needed(&drain_ft_state, &drain_ft_peer);

            // Spec §2 retransmit: ask for frames the buffer is stuck
            // behind, before it gives up and conceals them. Riding the
            // existing playback tick rather than a timer of its own, and
            // the buffer itself guarantees each gap is asked for once.
            let missing = jitter.lock().unwrap().take_retransmit_requests();
            if !missing.is_empty() {
                let sequences: Vec<u32> = missing.iter().map(|c| *c as u32).collect();
                let peer = drain_nack_peer.clone();
                tokio::spawn(async move {
                    let bytes = ControlMessage::AudioMissing { sequences }.encode();
                    let _ = peer.send_control(&bytes).await;
                });
            }
            loop {
                let popped = jitter.lock().unwrap().pop_ready();
                let damaged = matches!(popped, PopResult::Recovered(_) | PopResult::Concealed(_));
                let mut samples = match popped {
                    PopResult::Frame(s) | PopResult::Recovered(s) | PopResult::Concealed(s) => s,
                    PopResult::Empty => break,
                };
                drained_samples += samples.len() as u64;
                {
                    // Replay ring: record everything received, before the
                    // deafen gate and volume scaling below, so "save the
                    // last 30 seconds" means what the peer sent.
                    let mut ring = drain_replay.lock().unwrap();
                    ring.extend(samples.iter().copied());
                    let cap = replay_cap(drain_replay_seconds.load(Ordering::Relaxed));
                    while ring.len() > cap {
                        ring.pop_front();
                    }

                    // Spec §2 replay history. Cut here, while the ring is
                    // already locked and still holds the moment -- a
                    // second later the window may have rolled past it.
                    if let Some(damaged_frames) = clip_cutter.observe(damaged) {
                        let bounds = crate::replay_history::clip_bounds(
                            ring.len(),
                            sample_rate,
                            damaged_frames,
                        );
                        let clip_samples: Vec<f32> =
                            ring.iter().copied().skip(bounds.start).take(bounds.len()).collect();
                        // The damage happened a settle window ago, not now.
                        let settled_ago = (crate::replay_history::SETTLE_FRAMES as u64
                            + damaged_frames as u64)
                            * 20
                            * sample_rate as u64
                            / 1000;
                        let at_seconds = drained_samples.saturating_sub(settled_ago)
                            / sample_rate.max(1) as u64;
                        drain_replay_clips.lock().unwrap().push(
                            crate::replay_history::Clip {
                                at_seconds: at_seconds as u32,
                                damaged_frames,
                                sample_rate,
                                samples: clip_samples,
                            },
                        );
                    }
                }
                if !drain_deafened.load(Ordering::Relaxed) {
                    // Read live so the Settings slider applies to this call
                    // immediately, not just the next one.
                    // Per-person trim rides on top of the master level, and a
                    // per-person mute wins outright -- silence is silence
                    // regardless of what either slider says.
                    // Spec §1 ducking. Moved toward the target a step per
                    // frame rather than jumped to it: a gain that snaps
                    // clicks at the start of every word, which is more
                    // annoying than the echo it is fixing.
                    let duck_db = drain_duck_db.load(Ordering::Relaxed) as f32;
                    let target = if duck_db > 0.0 && drain_ptt.load(Ordering::Relaxed) {
                        -duck_db
                    } else {
                        0.0
                    };
                    // Attack faster than release: get out of the way
                    // quickly, come back gently.
                    let step = if target < duck_level {
                        DUCK_ATTACK_PER_FRAME
                    } else {
                        DUCK_RELEASE_PER_FRAME
                    };
                    duck_level += (target - duck_level).clamp(-step * 24.0, step * 24.0);

                    // Spec §1 "Per-User EQ": fix someone's harsh microphone
                    // on *your* end. Applied before the volume trim so the
                    // trim still means what the slider says -- an EQ that
                    // changed the overall level would make the two
                    // controls fight each other.
                    if let Ok(mut eq) = drain_peer_eq.lock() {
                        if let Some(chain) = eq.as_mut() {
                            chain.process(&mut samples);
                        }
                    }

                    let gain_lin = if drain_peer_muted.load(Ordering::Relaxed) {
                        0.0
                    } else {
                        let master = *output_gain_db.lock().unwrap();
                        let trim = *drain_peer_volume.lock().unwrap();
                        10f32.powf((master + trim + duck_level) / 20.0)
                    };
                    if gain_lin != 1.0 {
                        for s in &mut samples {
                            *s *= gain_lin;
                        }
                    }

                    // Spec §2 catch-up playback, applied last so it
                    // shortens exactly what is about to be heard --
                    // compressing before the EQ would feed the filters a
                    // signal with splices in it.
                    //
                    // Off unless the backlog is real: at speed 1.0 this is
                    // a passthrough, so a healthy call pays nothing.
                    let backlog = jitter.lock().unwrap().backlog_frames();
                    let speed = catch_up_speed(backlog);
                    catch_up_out.clear();
                    catch_up.process(&samples, speed, &mut catch_up_out);
                    drain_sink.lock().unwrap().push(&catch_up_out);

                    // The echo canceller's reference is what actually
                    // reaches the speaker -- after the gain, the ducking
                    // and the EQ -- because that is what the room hears
                    // and therefore what comes back. Feeding it the raw
                    // received audio instead would have it hunting for an
                    // echo of a signal nobody played.
                    {
                        let mut far = drain_aec_far.lock().unwrap();
                        far.extend(catch_up_out.iter().copied());
                        while far.len() > AEC_REFERENCE_SAMPLES {
                            far.pop_front();
                        }
                    }
                }
            }
        }
    });

    Ok(ActiveCall {
        echo_canceller,
        chain,
        sample_rate: AtomicU32::new(sample_rate),
        last_config: Mutex::new(config),
        muted,
        deafened,
        ptt_active,
        mic_level,
        peer_profile,
        peer,
        monitor_enabled,
        soundboard_queue,
        ptt_stop,
        jitter: stats_jitter,
        started_at: Instant::now(),
        peer_wants_silent,
        peer_wants_ring,
        pending_introduction,
        pending_friend_request,
        friend_accepted,
        chat_log,
        chat_rev,
        reactions,
        next_chat_id,
        history,
        outbox,
        replay,
        replay_clips,
        file_transfer: file_transfer_state,
        capture: Mutex::new(capture),
        capture_tx,
        playback: Mutex::new(playback),
        playback_sink,
        monitor_playback: Mutex::new(monitor_playback),
        monitor_sink,
        send_task,
        recv_task,
        drain_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_healthy_call_never_speeds_up() {
        // A backlog under the threshold is the jitter buffer doing its
        // job. Draining it fast would fight the smoothing it exists for.
        for backlog in 0..=CATCH_UP_START_FRAMES {
            assert_eq!(catch_up_speed(backlog), 1.0, "sped up at {backlog} frames");
        }
    }

    #[test]
    fn catching_up_ramps_in_rather_than_switching_on() {
        // One frame over the line should be inaudible, not a lurch.
        let gentle = catch_up_speed(CATCH_UP_START_FRAMES + 1);
        assert!(gentle > 1.0 && gentle < 1.03, "{gentle}");
    }

    #[test]
    fn a_real_stall_pushes_to_the_cap_and_no_further() {
        assert_eq!(catch_up_speed(CATCH_UP_FULL_FRAMES), antiphon_audio::MAX_SPEED);
        // A catastrophic backlog must not ask for a speed the compressor
        // would have to clamp anyway -- the limit belongs in one place.
        assert_eq!(catch_up_speed(10_000), antiphon_audio::MAX_SPEED);
    }

    #[test]
    fn the_further_behind_you_are_the_faster_it_plays() {
        let mut previous = 0.0;
        for backlog in CATCH_UP_START_FRAMES..=CATCH_UP_FULL_FRAMES {
            let speed = catch_up_speed(backlog);
            assert!(speed >= previous, "speed dropped at {backlog} frames");
            previous = speed;
        }
    }
}
