//! App-layer file transfer (spec §5): owns the send/receive lifecycle and
//! shared progress state; `antiphon_net::file_transfer` owns the wire-level
//! chunk format, `crate::control::ControlMessage` owns the
//! offer/accept/missing/complete envelope. This module wires those two
//! together into something a call can actually run.
//!
//! Streamed at both ends, so a transfer is bounded by the disk rather than
//! by memory: the sender reads each chunk from the file as it goes
//! ([`FileSource`]) and the receiver writes each one straight to its place
//! in a partial file ([`DiskReassembly`]). Anything that reads a whole
//! file -- hashing it, verifying it -- runs on a blocking thread, never on
//! the task that is also carrying the call's audio.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_net::file_transfer::{chunk_count, encode_chunk, parse_chunk};
use antiphon_net::PeerConnection;

use crate::content_store::{DiskReassembly, MAX_REQUEST};
use crate::control::ControlMessage;
use crate::file_integrity::FileSource;
use crate::pacer::Pacer;

/// The most chunks a sender will hold queued for resending.
///
/// The queue is filled by the peer's requests, so it is bounded: a peer
/// asking for the same file a hundred times over should cost a bounded
/// amount of memory, not an unbounded one.
const MAX_REPAIR_QUEUE: usize = 65_536;

/// Monotonic and process-unique (not just per-call), so a stray late
/// message from an earlier transfer can never be mistaken for a new one
/// that happens to reuse an id.
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
static REPAIRS: AtomicU64 = AtomicU64::new(0);

fn next_transfer_id() -> u64 {
    NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct OutgoingTransfer {
    pub transfer_id: u64,
    pub file_name: String,
    pub file_size: u64,
    pub accepted: bool,
    pub chunks_sent: u32,
    pub total_chunks: u32,
    pub done: bool,
    pub cancelled: bool,
    /// Held mid-transfer at the user's request (spec §5).
    ///
    /// Distinct from cancelled on purpose: a pause keeps the transfer,
    /// the receiver's partial file and the agreed chunk numbering, so
    /// resuming picks up where it stopped rather than starting over.
    pub paused: bool,
    pub error: Option<String>,
    /// Chunks the receiver asked for again, waiting to be resent.
    ///
    /// Served by the send loop itself, ahead of new chunks, rather than by
    /// a task of their own. That puts repairs under the same pacing, limit
    /// and pause as everything else -- a burst of 256 unpaced resends is
    /// exactly the kind of spike that caused the losses being repaired.
    pub repair_queue: VecDeque<u32>,
    /// Chunks reported lost since the send loop last looked, for the pacer.
    pub reported_lost: u64,
}

/// An offer that arrived from the peer, awaiting the user's Accept/Reject
/// click -- kept separate from `IncomingTransfer` since "someone wants to
/// send me a file" and "I'm actively receiving one" are different states
/// the UI needs to show differently (a prompt vs. a progress bar).
pub struct IncomingOffer {
    pub transfer_id: u64,
    pub file_name: String,
    pub file_size: u64,
    sha256: String,
    /// The sender's preview, already checked against
    /// `file_preview::accept_thumbnail` -- so anything reaching here is
    /// bounded and starts with a PNG header.
    pub thumbnail_png: Vec<u8>,
}

pub struct IncomingTransfer {
    pub transfer_id: u64,
    pub file_name: String,
    pub file_size: u64,
    pub chunks_received: u32,
    /// Chunks already on disk from an earlier, interrupted attempt. Shown,
    /// because a bar that starts at 60% with no explanation looks broken.
    pub resumed_chunks: u32,
    pub total_chunks: u32,
    /// Every chunk is in and the file is being checked against its hash.
    /// Its own state because on a large file that takes long enough to
    /// look like a hang if the bar simply sits at 100%.
    pub verifying: bool,
    pub done: bool,
    pub error: Option<String>,
}

/// Everything in flight for one call. A call supports at most one
/// outgoing and one incoming transfer at a time -- not a fundamental limit
/// of the wire protocol (transfer ids already disambiguate), just a
/// deliberately simple scope.
#[derive(Default)]
pub struct FileTransferState {
    pub outgoing: Option<OutgoingTransfer>,
    pub pending_offer: Option<IncomingOffer>,
    pub incoming: Option<IncomingTransfer>,
    reassembly: Option<DiskReassembly>,
    /// Set once a received file has been verified and stored: its name and
    /// where it sits in the content store. The UI timer notices this and
    /// offers a save dialog, off the main thread.
    pub ready_to_save: Option<(String, PathBuf)>,
    last_missing_request: Option<Instant>,
    /// Where received files are stored. `None` means the app's own config
    /// directory; tests point it somewhere disposable instead of at the
    /// real one.
    pub store_root: Option<PathBuf>,
}

impl FileTransferState {
    fn root(&self) -> Option<PathBuf> {
        self.store_root.clone().or_else(crate::config::config_dir)
    }
}

/// How often the receiver asks for missing chunks.
///
/// Frequent, because a request covers at most `MAX_REQUEST` chunks and a
/// damaged transfer needs many. Asking again for a chunk already asked for
/// is prevented separately -- see `DiskReassembly::chunks_to_request` --
/// which is what used to force this out to two seconds.
const MISSING_REQUEST_INTERVAL: Duration = Duration::from_millis(250);

/// How long the sender waits for confirmation after its last activity --
/// the end of its first pass, or the most recent repair request.
const CONFIRMATION_WAIT: Duration = Duration::from_secs(20);

/// Minimal base64, so a thumbnail can ride inside JSON.
///
/// Hand-rolled rather than a dependency: it is a table and two loops, and
/// the alternative is another crate parsing attacker-supplied input.
pub fn encode_b64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Decodes base64, returning `None` for anything malformed rather than
/// guessing at it.
pub fn decode_b64(text: &str) -> Option<Vec<u8>> {
    if text.is_empty() {
        return None;
    }
    let value = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };

    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|c| **c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= if *c == b'=' { 0 } else { value(*c)? } << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Whether a call is running, for the throttle.
///
/// A global rather than a parameter because `send_file` is spawned as its
/// own task and does not otherwise need to know about calls -- threading
/// an `AppState` through purely for a boolean would couple the two for no
/// other reason.
static CALL_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_call_active(active: bool) {
    CALL_ACTIVE.store(active, std::sync::atomic::Ordering::Relaxed);
}

fn in_call() -> bool {
    CALL_ACTIVE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Offers `source` to the peer, waits (up to 30s) for acceptance, then
/// streams it: every chunk once, and any the receiver reports missing
/// again, all through one pacer. Ends when the receiver confirms
/// `FileComplete`, or after a stretch with nothing to send and no word
/// from them. Meant to run as its own spawned task -- see
/// `state::send_file` for how it's launched.
pub async fn send_file(
    peer: Arc<PeerConnection>,
    state: Arc<Mutex<FileTransferState>>,
    file_name: String,
    source: Arc<FileSource>,
    sha256: String,
    // A small PNG preview the *sender* generated (spec §5), or empty.
    thumbnail_png_b64: String,
) {
    let transfer_id = next_transfer_id();
    let file_size = source.len();
    let total_chunks = chunk_count(file_size);

    {
        let mut s = state.lock().unwrap();
        s.outgoing = Some(OutgoingTransfer {
            transfer_id,
            file_name: file_name.clone(),
            file_size,
            accepted: false,
            chunks_sent: 0,
            total_chunks,
            done: false,
            cancelled: false,
            paused: false,
            error: None,
            repair_queue: VecDeque::new(),
            reported_lost: 0,
        });
    }

    let offer = ControlMessage::FileOffer {
        transfer_id,
        file_name,
        file_size,
        chunk_count: total_chunks,
        sha256,
        thumbnail_png_b64,
    };
    if peer.send_control(&offer.encode()).await.is_err() {
        fail_outgoing(&state, transfer_id, "Couldn't send the file offer");
        return;
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if Instant::now() > deadline {
            fail_outgoing(&state, transfer_id, "The other side didn't respond in time");
            return;
        }
        match outgoing_status(&state, transfer_id) {
            Some(OutgoingStatus::Accepted) => break,
            Some(OutgoingStatus::Gone) | None => return, // rejected/cancelled -- already cleared/marked by handle_control_message
            // Pausing before the other side has even accepted still just
            // waits: the acceptance deadline is about *them* not
            // answering, and holding your own send does not change that.
            Some(OutgoingStatus::Pending) | Some(OutgoingStatus::Paused) => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
        }
    }

    let mut pacer = Pacer::default();
    // The upload limit, re-read periodically rather than per chunk. Per
    // chunk meant reading and parsing the whole settings file every 3.8 KB
    // sent -- on a large file, tens of thousands of times. A quarter of a
    // second is still immediate as far as anyone moving a slider can tell.
    let mut limits = crate::config::load().transfer_limits;
    let mut limits_read_at = Instant::now();
    let mut next_new: u32 = 0;
    let mut idle_since: Option<Instant> = None;

    loop {
        enum Next {
            Wait,
            Idle,
            Send { index: u32, repair: bool },
        }
        let next = {
            let mut s = state.lock().unwrap();
            let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id) else { return };
            if o.done || o.cancelled || o.error.is_some() {
                return;
            }
            pacer.report_loss(std::mem::take(&mut o.reported_lost));
            if o.paused {
                // A pause is a wait, not an exit: the loop holds its place,
                // so resuming continues rather than re-sending everything.
                Next::Wait
            } else if let Some(index) = o.repair_queue.pop_front() {
                // Repairs first: the receiver is stuck behind them, while
                // new chunks only extend a file it can already hold.
                Next::Send { index, repair: true }
            } else if next_new < total_chunks {
                Next::Send { index: next_new, repair: false }
            } else {
                Next::Idle
            }
        };

        let (index, repair) = match next {
            Next::Wait => {
                idle_since = None;
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Next::Idle => {
                // Everything sent once and nothing queued: waiting for
                // FileComplete, or for more repair requests. Given up on
                // only after a stretch with neither, so a large transfer
                // still being patched up is never declared lost while it
                // is visibly still working.
                if idle_since.get_or_insert_with(Instant::now).elapsed() > CONFIRMATION_WAIT {
                    fail_outgoing(&state, transfer_id, "No confirmation from the other side -- it may not have received everything");
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            Next::Send { index, repair } => (index, repair),
        };
        idle_since = None;
        if !repair {
            next_new += 1;
        }

        let bytes = match source.read_chunk(index) {
            Ok(bytes) => bytes,
            Err(e) => {
                // Stopped rather than sent on: the file no longer matches
                // what was offered, and the receiver would only find out
                // at the very end. Telling them now saves the rest.
                fail_outgoing(&state, transfer_id, &format!("Stopped sending: {e}"));
                let _ = peer.send_control(&ControlMessage::FileCancel { transfer_id }.encode()).await;
                return;
            }
        };
        let chunk = encode_chunk(transfer_id, index, &bytes);
        if peer.send_file_chunk(&chunk).await.is_err() {
            fail_outgoing(&state, transfer_id, "Connection dropped mid-transfer");
            return;
        }
        if !repair {
            if let Ok(mut s) = state.lock() {
                if let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id) {
                    o.chunks_sent = (o.chunks_sent + 1).min(o.total_chunks);
                }
            }
        }

        // Spec §5 bandwidth controls: the user's cap, the automatic one
        // while a call is running, and -- with neither -- whatever rate
        // the link turns out to sustain. Starting a call mid-transfer
        // takes effect on the very next chunk (it is a flag, not a file).
        if limits_read_at.elapsed() >= Duration::from_millis(250) {
            limits = crate::config::load().transfer_limits;
            limits_read_at = Instant::now();
        }
        if let Some(wait) = pacer.after_send(chunk.len(), limits.limit_bytes_per_sec(in_call())) {
            tokio::time::sleep(wait).await;
        }
    }
}

enum OutgoingStatus {
    Pending,
    Accepted,
    Paused,
    Gone,
}

fn outgoing_status(state: &Mutex<FileTransferState>, transfer_id: u64) -> Option<OutgoingStatus> {
    let s = state.lock().unwrap();
    let o = s.outgoing.as_ref()?;
    if o.transfer_id != transfer_id || o.done || o.cancelled || o.error.is_some() {
        return Some(OutgoingStatus::Gone);
    }
    if !o.accepted {
        return Some(OutgoingStatus::Pending);
    }
    Some(if o.paused { OutgoingStatus::Paused } else { OutgoingStatus::Accepted })
}

fn fail_outgoing(state: &Mutex<FileTransferState>, transfer_id: u64, message: &str) {
    if let Ok(mut s) = state.lock() {
        if let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id) {
            o.error = Some(message.to_string());
        }
    }
}

/// Tells the sender the file arrived and verified.
///
/// Sent as soon as the file is verified and in the store -- not after the
/// user picks where to save a copy. The bytes are already safely on disk
/// at that point, and making the sender's confirmation wait on a save
/// dialog meant a receiver who took half a minute choosing a folder made
/// the sender report a failed transfer that had in fact succeeded.
fn send_complete(peer: &Arc<PeerConnection>, runtime: &tokio::runtime::Handle, transfer_id: u64) {
    let peer = peer.clone();
    runtime.spawn(async move {
        let _ = peer.send_control(&ControlMessage::FileComplete { transfer_id }.encode()).await;
    });
}

/// Reacts to a decoded `ControlMessage` that isn't `Hello` -- called
/// synchronously from the receive loop, which already runs on the tokio
/// runtime, so follow-up work is spawned on the current runtime rather
/// than awaited inline.
pub fn handle_control_message(msg: ControlMessage, state: &Arc<Mutex<FileTransferState>>, peer: &Arc<PeerConnection>) {
    match msg {
        // Hello, Chat and the friend-request messages are handled upstream
        // in `state::start_pipeline`'s receive loop; they only appear here
        // to keep the match exhaustive.
        ControlMessage::Hello(_)
        | ControlMessage::Chat { .. }
        | ControlMessage::ChatEdit { .. }
        | ControlMessage::ChatDelete { .. }
        | ControlMessage::ChatReact { .. }
        | ControlMessage::Introduce { .. }
        | ControlMessage::EscalateCall
        | ControlMessage::SilentCall
        | ControlMessage::AudioMissing { .. }
        | ControlMessage::LinkReport { .. }
        | ControlMessage::FriendRequest { .. }
        | ControlMessage::FriendAccept { .. }
        | ControlMessage::FriendReject
        | ControlMessage::GroupConnect { .. }
        | ControlMessage::GroupPunch { .. }
        | ControlMessage::GroupPunchAddr { .. }
        | ControlMessage::GroupPunchPeer { .. }
        | ControlMessage::GroupLeave
        | ControlMessage::GroupSfu { .. }
        | ControlMessage::Community { .. }
        | ControlMessage::Profile(_)
        | ControlMessage::MlsKeyPackage(_)
        | ControlMessage::MlsWelcome(_)
        | ControlMessage::MlsCommit(_)
        | ControlMessage::FileLink { .. } => {}
        ControlMessage::FileOffer {
            transfer_id,
            file_name,
            file_size,
            sha256,
            thumbnail_png_b64,
            ..
        } => {
            let mut s = state.lock().unwrap();
            // Only one incoming offer/transfer at a time -- a second offer
            // while one's already pending or in flight is ignored rather
            // than queued.
            if s.pending_offer.is_some() || s.incoming.is_some() {
                return;
            }
            // The sender chose these bytes, so they are checked before
            // anything decodes them -- an image decoder is the wrong place
            // to first discover a file is not what it claims.
            let thumbnail_png = decode_b64(&thumbnail_png_b64)
                .filter(|bytes| crate::file_preview::accept_thumbnail(bytes))
                .unwrap_or_default();
            let offer = IncomingOffer { transfer_id, file_name, file_size, sha256, thumbnail_png };

            // Spec §5: "a file you already have is never downloaded twice".
            // A cheap existence check decides whether to look; the real
            // check reads the whole stored file, so it runs on a blocking
            // thread rather than stalling the audio this task also carries.
            let held = s.root().filter(|root| crate::content_store::has(root, &offer.sha256));
            let Some(root) = held else {
                s.pending_offer = Some(offer);
                return;
            };
            // Holds the slot while checking, so a second offer arriving
            // meanwhile is turned away rather than overlapping this one.
            s.incoming = Some(IncomingTransfer {
                transfer_id,
                file_name: offer.file_name.clone(),
                file_size,
                chunks_received: 0,
                resumed_chunks: 0,
                total_chunks: chunk_count(file_size),
                verifying: true,
                done: false,
                error: None,
            });
            drop(s);

            let state = state.clone();
            let peer = peer.clone();
            let runtime = tokio::runtime::Handle::current();
            runtime.clone().spawn_blocking(move || {
                let verified = crate::content_store::verified_path(&root, &offer.sha256);
                let mut s = state.lock().unwrap();
                if !s.incoming.as_ref().is_some_and(|i| i.transfer_id == transfer_id) {
                    return; // cancelled while checking
                }
                match verified {
                    Some(path) => {
                        if let Some(i) = s.incoming.as_mut() {
                            i.verifying = false;
                            i.done = true;
                            i.chunks_received = i.total_chunks;
                        }
                        // Straight to the save dialog: the user still
                        // chooses where it goes, they just do not wait for
                        // a transfer that would reproduce bytes already
                        // present.
                        s.ready_to_save = Some((offer.file_name, path));
                        drop(s);
                        send_complete(&peer, &runtime, transfer_id);
                    }
                    None => {
                        // The stored copy has been damaged or edited. Fall
                        // back to asking, exactly as if it were not held.
                        s.incoming = None;
                        s.pending_offer = Some(offer);
                    }
                }
            });
        }
        ControlMessage::FileAccept { transfer_id } => {
            if let Ok(mut s) = state.lock() {
                if let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id) {
                    o.accepted = true;
                }
            }
        }
        ControlMessage::FileReject { transfer_id } => {
            if let Ok(mut s) = state.lock() {
                if s.outgoing.as_ref().is_some_and(|o| o.transfer_id == transfer_id) {
                    s.outgoing = None;
                }
            }
        }
        ControlMessage::FileCancel { transfer_id } => {
            if let Ok(mut s) = state.lock() {
                if s.outgoing.as_ref().is_some_and(|o| o.transfer_id == transfer_id) {
                    s.outgoing = None;
                }
                if s.incoming.as_ref().is_some_and(|i| i.transfer_id == transfer_id) {
                    s.incoming = None;
                    // Kept rather than discarded: *they* stopped, and if
                    // they offer the same file again it resumes from here.
                    if let Some(mut reassembly) = s.reassembly.take() {
                        let _ = reassembly.persist();
                    }
                }
                if s.pending_offer.as_ref().is_some_and(|o| o.transfer_id == transfer_id) {
                    s.pending_offer = None;
                }
            }
        }
        ControlMessage::FileComplete { transfer_id } => {
            if let Ok(mut s) = state.lock() {
                if let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id) {
                    o.done = true;
                }
            }
        }
        ControlMessage::FileMissing { transfer_id, indices } => {
            #[cfg(test)]
            REPAIRS.fetch_add(1, Ordering::Relaxed);
            let Ok(mut s) = state.lock() else { return };
            let Some(o) = s.outgoing.as_mut().filter(|o| o.transfer_id == transfer_id && !o.done) else {
                return;
            };
            // Only chunks already sent once count: asking for one the
            // first pass has not reached yet is not a loss, and resending
            // it early would send it twice. Capped here as well as on the
            // receiving side, because the list comes from the peer.
            let mut queued = 0;
            for index in indices.into_iter().take(MAX_REQUEST) {
                if index < o.chunks_sent
                    && o.repair_queue.len() < MAX_REPAIR_QUEUE
                    && !o.repair_queue.contains(&index)
                {
                    o.repair_queue.push_back(index);
                    queued += 1;
                }
            }
            o.reported_lost += queued;
        }
    }
}

/// Feeds one decrypted `File` frame's plaintext into whatever incoming
/// transfer it belongs to (dropped if it's for a transfer that isn't
/// currently being received -- e.g. a stray retransmit that arrived after
/// the transfer already finished or was cancelled).
pub fn handle_file_chunk(plaintext: &[u8], state: &Arc<Mutex<FileTransferState>>, peer: &Arc<PeerConnection>) {
    let Some((transfer_id, index, bytes)) = parse_chunk(plaintext) else { return };
    let mut s = state.lock().unwrap();
    if !s.incoming.as_ref().is_some_and(|i| i.transfer_id == transfer_id) {
        return;
    }
    let Some(reassembly) = s.reassembly.as_mut() else { return };
    if let Err(e) = reassembly.write_chunk(index, bytes) {
        // A disk that filled up, or a file something else locked. What
        // arrived so far is kept for a resume; the transfer stops here
        // rather than silently dropping every chunk after this one.
        if let Some(mut reassembly) = s.reassembly.take() {
            let _ = reassembly.persist();
        }
        if let Some(incoming) = s.incoming.as_mut() {
            incoming.error = Some(format!("Couldn't write the file: {e}"));
        }
        return;
    }
    let received = reassembly.received();
    let complete = reassembly.is_complete();
    // The periodic index save, off this path: it fsyncs, and an fsync in
    // the receive loop is a burst of lost datagrams. The job is handed a
    // snapshot, so holding the lock any longer is not needed either.
    if !complete && reassembly.persist_due() {
        match reassembly.persist_job() {
            Ok(job) => {
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = job() {
                        eprintln!("[files] could not save transfer progress: {e}");
                    }
                });
            }
            Err(e) => eprintln!("[files] could not save transfer progress: {e}"),
        }
    }
    if let Some(incoming) = s.incoming.as_mut() {
        incoming.chunks_received = received;
    }
    if complete {
        drop(s);
        start_verification(state, peer, &tokio::runtime::Handle::current(), transfer_id);
    }
}

/// Checks a finished transfer against its hash and stores it, on a
/// blocking thread -- it reads the whole file.
fn start_verification(
    state: &Arc<Mutex<FileTransferState>>,
    peer: &Arc<PeerConnection>,
    runtime: &tokio::runtime::Handle,
    transfer_id: u64,
) {
    let reassembly = {
        let mut s = state.lock().unwrap();
        let Some(incoming) = s.incoming.as_mut().filter(|i| i.transfer_id == transfer_id) else { return };
        incoming.verifying = true;
        s.reassembly.take()
    };
    let Some(reassembly) = reassembly else { return };

    let state = state.clone();
    let peer = peer.clone();
    let runtime2 = runtime.clone();
    runtime.spawn_blocking(move || {
        let outcome = reassembly.finish();
        let mut s = state.lock().unwrap();
        let file_name = {
            let Some(incoming) = s.incoming.as_mut().filter(|i| i.transfer_id == transfer_id) else { return };
            incoming.verifying = false;
            match &outcome {
                Ok(Some(_)) => incoming.done = true,
                Ok(None) => {
                    incoming.error = Some(
                        "The received data didn't match its checksum -- the file is corrupted or incomplete"
                            .to_string(),
                    )
                }
                Err(e) => incoming.error = Some(format!("Couldn't store the received file: {e}")),
            }
            incoming.file_name.clone()
        };
        if let Ok(Some(path)) = outcome {
            s.ready_to_save = Some((file_name, path));
            drop(s);
            send_complete(&peer, &runtime2, transfer_id);
        }
    });
}

/// User accepted the currently pending incoming offer: opens the partial
/// file it will be written into -- adopting whatever an earlier attempt
/// left there -- and returns the transfer id to reply `FileAccept` with.
///
/// `Err` carries the id and a reason when the file cannot be opened, so
/// the caller can decline rather than leave the sender waiting on a
/// transfer that has nowhere to go.
pub fn accept_offer(
    state: &Arc<Mutex<FileTransferState>>,
    peer: &Arc<PeerConnection>,
    runtime: &tokio::runtime::Handle,
) -> Option<Result<u64, (u64, String)>> {
    let mut s = state.lock().unwrap();
    let offer = s.pending_offer.take()?;
    let total_chunks = chunk_count(offer.file_size);

    // Spec §5: "transfers resume after a disconnect instead of starting
    // over". A partial held under this same content hash is by definition
    // part of *this* file, so its chunks are adopted rather than
    // re-fetched -- `DiskReassembly::open` does that.
    let opened = s
        .root()
        .ok_or_else(|| "there is no settings folder to store it in".to_string())
        .and_then(|root| {
            DiskReassembly::open(&root, &offer.sha256, &offer.file_name, offer.file_size)
                .map_err(|e| e.to_string())
        });
    let reassembly = match opened {
        Ok(r) => r,
        Err(why) => return Some(Err((offer.transfer_id, format!("Couldn't receive {}: {why}", offer.file_name)))),
    };

    let already_have = reassembly.received();
    let resumed_chunks = reassembly.resumed();
    let complete = reassembly.is_complete();
    s.reassembly = Some(reassembly);
    s.incoming = Some(IncomingTransfer {
        transfer_id: offer.transfer_id,
        file_name: offer.file_name,
        file_size: offer.file_size,
        chunks_received: already_have,
        resumed_chunks,
        total_chunks,
        verifying: false,
        done: false,
        error: None,
    });
    drop(s);
    // An empty file -- or one fully recovered from an earlier attempt that
    // died just before verifying -- has nothing left to receive.
    if complete {
        start_verification(state, peer, runtime, offer.transfer_id);
    }
    Some(Ok(offer.transfer_id))
}

pub fn reject_offer(state: &Arc<Mutex<FileTransferState>>) -> Option<u64> {
    let mut s = state.lock().unwrap();
    s.pending_offer.take().map(|o| o.transfer_id)
}

/// Holds or releases the outgoing transfer (spec §5). Returns the new
/// state, or `None` when there is nothing running to pause.
pub fn set_paused(state: &Arc<Mutex<FileTransferState>>, paused: bool) -> Option<bool> {
    let mut s = state.lock().ok()?;
    let outgoing = s.outgoing.as_mut()?;
    if outgoing.done || outgoing.cancelled || outgoing.error.is_some() {
        return None;
    }
    outgoing.paused = paused;
    Some(paused)
}

/// Cancels whichever transfer is currently active -- outgoing takes
/// priority if somehow both are -- and returns its id for the caller to
/// notify the peer with. `None` if nothing was in flight.
///
/// A receive cancelled by *this* user throws its partial away: they said
/// they did not want it, and keeping a half-file around for a resume
/// nobody asked for would just be disk they did not agree to spend.
pub fn cancel(state: &Arc<Mutex<FileTransferState>>) -> Option<u64> {
    let mut s = state.lock().unwrap();
    if let Some(o) = s.outgoing.take() {
        return Some(o.transfer_id);
    }
    if let Some(i) = s.incoming.take() {
        if let Some(reassembly) = s.reassembly.take() {
            reassembly.discard();
        }
        return Some(i.transfer_id);
    }
    None
}

/// Periodic housekeeping, called from `state::start_pipeline`'s drain
/// task: asks the sender to resend chunks that are genuinely missing,
/// throttled so a request is not repeated while the last one is still
/// being answered. See `DiskReassembly::chunks_to_request` for which
/// chunks count as missing.
pub fn request_missing_chunks_if_needed(state: &Arc<Mutex<FileTransferState>>, peer: &Arc<PeerConnection>) {
    let mut s = state.lock().unwrap();
    let Some(incoming) = s.incoming.as_ref() else { return };
    if incoming.done || incoming.verifying || incoming.error.is_some() {
        return;
    }
    let transfer_id = incoming.transfer_id;
    let due = s.last_missing_request.is_none_or(|t| t.elapsed() >= MISSING_REQUEST_INTERVAL);
    if !due {
        return;
    }
    let Some(missing) = s.reassembly.as_mut().map(|r| r.chunks_to_request()) else { return };
    if missing.is_empty() {
        return;
    }
    s.last_missing_request = Some(Instant::now());
    drop(s);

    let peer = peer.clone();
    tokio::spawn(async move {
        let _ = peer.send_control(&ControlMessage::FileMissing { transfer_id, indices: missing }.encode()).await;
    });
}

/// Saves where an interrupted incoming transfer got to, so it can resume
/// (spec §5 "transfers resume after a disconnect instead of starting
/// over").
///
/// The bytes are already on disk -- they were written as they arrived --
/// so all that is left to save is the record of which chunks they are.
pub fn save_partial_if_interrupted(state: &Arc<Mutex<FileTransferState>>) {
    let mut s = state.lock().unwrap();
    if s.incoming.as_ref().is_some_and(|i| i.done) {
        return;
    }
    if let Some(reassembly) = s.reassembly.as_mut() {
        if let Err(e) = reassembly.persist() {
            eprintln!("[files] could not save the interrupted transfer: {e}");
        }
    }
}

#[cfg(test)]
mod pause_tests {
    use super::*;

    fn sending(paused: bool) -> Arc<Mutex<FileTransferState>> {
        Arc::new(Mutex::new(FileTransferState {
            outgoing: Some(OutgoingTransfer {
                transfer_id: 1,
                file_name: "notes.txt".into(),
                file_size: 100,
                accepted: true,
                chunks_sent: 3,
                total_chunks: 10,
                done: false,
                cancelled: false,
                paused,
                error: None,
                repair_queue: VecDeque::new(),
                reported_lost: 0,
            }),
            ..Default::default()
        }))
    }

    #[test]
    fn a_running_transfer_can_be_held_and_released() {
        let state = sending(false);
        assert_eq!(set_paused(&state, true), Some(true));
        assert!(matches!(outgoing_status(&state, 1), Some(OutgoingStatus::Paused)));
        assert_eq!(set_paused(&state, false), Some(false));
        assert!(matches!(outgoing_status(&state, 1), Some(OutgoingStatus::Accepted)));
    }

    #[test]
    fn pausing_keeps_the_progress_already_made() {
        // The difference between pause and cancel: resuming continues
        // from where it stopped rather than re-sending everything.
        let state = sending(false);
        set_paused(&state, true);
        set_paused(&state, false);
        let s = state.lock().unwrap();
        assert_eq!(s.outgoing.as_ref().unwrap().chunks_sent, 3);
    }

    #[test]
    fn a_finished_transfer_cannot_be_paused() {
        let state = sending(false);
        state.lock().unwrap().outgoing.as_mut().unwrap().done = true;
        assert_eq!(set_paused(&state, true), None);
    }

    #[test]
    fn a_cancelled_transfer_cannot_be_paused_back_to_life() {
        let state = sending(false);
        state.lock().unwrap().outgoing.as_mut().unwrap().cancelled = true;
        assert_eq!(set_paused(&state, false), None);
    }

    #[test]
    fn a_failed_transfer_cannot_be_paused() {
        let state = sending(false);
        state.lock().unwrap().outgoing.as_mut().unwrap().error = Some("gone".into());
        assert_eq!(set_paused(&state, true), None);
    }

    #[test]
    fn there_is_nothing_to_pause_when_nothing_is_sending() {
        let state = Arc::new(Mutex::new(FileTransferState::default()));
        assert_eq!(set_paused(&state, true), None);
    }

    #[test]
    fn a_paused_transfer_is_still_the_same_transfer() {
        // Cancelled and paused must not be confused: `Gone` ends the send
        // loop for good, `Paused` makes it wait.
        let state = sending(true);
        assert!(!matches!(outgoing_status(&state, 1), Some(OutgoingStatus::Gone)));
    }
}

#[cfg(test)]
mod integration_tests {
    //! Two real `PeerConnection`s over loopback UDP running the actual
    //! protocol: handshake, offer, accept, chunks streamed from a file on
    //! disk, written to a partial file on the other side, verified, stored,
    //! and confirmed back to the sender -- the same code path a real call
    //! uses, without a GUI or two machines.
    use super::*;
    use tokio::net::UdpSocket;
    use antiphon_net::file_transfer::{sha256_hex, CHUNK_SIZE};
    use antiphon_net::Received;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("antiphon-ft-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Pair {
        peer_a: Arc<PeerConnection>,
        state_a: Arc<Mutex<FileTransferState>>,
        state_b: Arc<Mutex<FileTransferState>>,
    }

    /// Two connected peers with receive loops running. B auto-accepts
    /// offers, and `drop_chunk` decides which of the first-pass chunks B
    /// pretends never arrived -- to exercise the repair path for real.
    async fn connected_pair(store: PathBuf, drop_chunk: fn(u32) -> bool) -> Pair {
        let socket_a = Arc::new(antiphon_net::AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let socket_b = Arc::new(antiphon_net::AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let addr_b = socket_b.local_addr().unwrap();

        // Two distinct identities, so this exercises the same authenticated
        // handshake a real call uses rather than a special test path.
        let id_a = antiphon_crypto::IdentityKeyPair::generate();
        let id_b = antiphon_crypto::IdentityKeyPair::generate();
        let (peer_a, peer_b) = tokio::join!(
            PeerConnection::connect(socket_a, addr_b, &id_a),
            PeerConnection::listen(socket_b, &id_b)
        );
        let peer_a = Arc::new(peer_a.expect("handshake (connect side)"));
        let peer_b = Arc::new(peer_b.expect("handshake (listen side)"));

        let state_a = Arc::new(Mutex::new(FileTransferState::default()));
        let state_b = Arc::new(Mutex::new(FileTransferState { store_root: Some(store), ..Default::default() }));

        {
            let (peer, state) = (peer_a.clone(), state_a.clone());
            tokio::spawn(async move {
                let peer2 = peer.clone();
                let _ = peer2
                    .recv_loop(move |msg| match msg {
                        Received::Control(bytes) => {
                            if let Some(m) = ControlMessage::decode(&bytes) {
                                handle_control_message(m, &state, &peer);
                            }
                        }
                        Received::File(bytes) => handle_file_chunk(&bytes, &state, &peer),
                        Received::Audio { .. } | Received::Video(_) => {}
                    })
                    .await;
            });
        }
        {
            let (peer, state) = (peer_b.clone(), state_b.clone());
            let seen: Arc<Mutex<std::collections::HashSet<u32>>> = Arc::default();
            tokio::spawn(async move {
                let peer2 = peer.clone();
                let _ = peer2
                    .recv_loop(move |msg| match msg {
                        Received::Control(bytes) => {
                            let Some(decoded) = ControlMessage::decode(&bytes) else { return };
                            let is_offer = matches!(decoded, ControlMessage::FileOffer { .. });
                            handle_control_message(decoded, &state, &peer);
                            if is_offer {
                                let runtime = tokio::runtime::Handle::current();
                                if let Some(Ok(transfer_id)) = accept_offer(&state, &peer, &runtime) {
                                    let peer = peer.clone();
                                    tokio::spawn(async move {
                                        let _ = peer.send_control(&ControlMessage::FileAccept { transfer_id }.encode()).await;
                                    });
                                }
                            }
                        }
                        Received::File(bytes) => {
                            let index = parse_chunk(&bytes).map(|(_, i, _)| i).unwrap_or(0);
                            // Dropped only the first time it is seen, so a
                            // resend gets through.
                            if drop_chunk(index) && seen.lock().unwrap().insert(index) {
                                return;
                            }
                            handle_file_chunk(&bytes, &state, &peer)
                        }
                        Received::Audio { .. } | Received::Video(_) => {}
                    })
                    .await;
            });
        }
        // The repair requests come from the drain task in the real app.
        {
            let (peer, state) = (peer_b.clone(), state_b.clone());
            tokio::spawn(async move {
                loop {
                    request_missing_chunks_if_needed(&state, &peer);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
        Pair { peer_a, state_a, state_b }
    }

    async fn run_transfer(pair: &Pair, path: &std::path::Path, seconds: u64) -> PathBuf {
        let (source, hash) = crate::file_integrity::open_and_hash(path).unwrap();
        tokio::spawn(send_file(
            pair.peer_a.clone(),
            pair.state_a.clone(),
            "integration.bin".to_string(),
            Arc::new(source),
            hash,
            String::new(),
        ));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(seconds);
        let (name, stored) = loop {
            assert!(tokio::time::Instant::now() <= deadline, "file transfer didn't complete in {seconds}s");
            if let Some(err) = pair.state_b.lock().unwrap().incoming.as_ref().and_then(|i| i.error.clone()) {
                panic!("receiver reported: {err}");
            }
            if let Some(saved) = pair.state_b.lock().unwrap().ready_to_save.take() {
                break saved;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert_eq!(name, "integration.bin");

        // The sender hears FileComplete without anyone having saved a copy
        // anywhere -- the verified file in the store is what counts.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            assert!(tokio::time::Instant::now() <= deadline, "sender never saw FileComplete");
            if pair.state_a.lock().unwrap().outgoing.as_ref().is_some_and(|o| o.done) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stored
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_file_streams_from_disk_to_disk_over_real_sockets() {
        let dir = temp_dir("clean");
        let data: Vec<u8> = (0..(CHUNK_SIZE * 3 + 500)).map(|i| (i % 251) as u8).collect();
        let source = dir.join("outgoing.bin");
        std::fs::write(&source, &data).unwrap();

        let pair = connected_pair(dir.join("store"), |_| false).await;
        let stored = run_transfer(&pair, &source, 15).await;

        let received = std::fs::read(&stored).unwrap();
        assert_eq!(received, data);
        assert_eq!(sha256_hex(&received), sha256_hex(&data));
    }

    /// A large transfer, timed. Ignored by default because of its size;
    /// run with `cargo test -p antiphon large_file -- --ignored --nocapture`.
    #[ignore]
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_large_file_streams_through() {
        let dir = temp_dir("large");
        let len = 64 * 1024 * 1024;
        let data: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        let source = dir.join("outgoing.bin");
        std::fs::write(&source, &data).unwrap();
        drop(data);

        let pair = connected_pair(dir.join("store"), |_| false).await;
        let started = std::time::Instant::now();
        let stored = run_transfer(&pair, &source, 600).await;
        let secs = started.elapsed().as_secs_f64();
        println!("64 MB in {secs:.1}s = {:.1} MB/s", 64.0 / secs);
        println!("repair requests answered: {}", REPAIRS.load(Ordering::Relaxed));
        assert_eq!(std::fs::metadata(&stored).unwrap().len(), len as u64);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn lost_chunks_are_asked_for_again_and_the_file_still_arrives_whole() {
        // Every third chunk lost on the first pass. The repair path is the
        // part of this protocol that only ever runs when something went
        // wrong, which is exactly why it needs a test that makes it run.
        let dir = temp_dir("lossy");
        let data: Vec<u8> = (0..(CHUNK_SIZE * 40 + 77)).map(|i| (i * 7 % 253) as u8).collect();
        let source = dir.join("outgoing.bin");
        std::fs::write(&source, &data).unwrap();

        let pair = connected_pair(dir.join("store"), |i| i % 3 == 1).await;
        let stored = run_transfer(&pair, &source, 30).await;

        assert_eq!(std::fs::read(&stored).unwrap(), data);
    }
}
