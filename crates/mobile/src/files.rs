//! Files during a call: the send queue, the one transfer running, and
//! received files saved where the user can find them.
//!
//! The transfer itself is the desktop's `file_transfer` module on the
//! call's runtime -- the offer, chunking, repair requests and hash check
//! are its, unchanged, which is what lets a phone and a desktop swap files.
//! What this adds is what a phone needs around it: picked files arrive as
//! a platform handle rather than a path, transfers wait in a queue the
//! user can reorder, and the data rules can hold them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use antiphon_net::file_transfer::{sha256_hex, CHUNK_SIZE};

use crate::call::Live;
use crate::control::ControlMessage;
use crate::data_rules::{CellularMeter, Hold};
use crate::file_integrity::FileSource;
use crate::file_transfer;
use crate::platform::{self, PickedFile};
use crate::queue::{Entry, TransferQueue};

/// The largest file sent from a phone. The platform hands over a picked
/// file as one buffer (there is no path to stream from), so this bounds
/// the memory a send takes rather than the protocol, which has no limit.
const MAX_SEND_BYTES: u64 = 256 * 1024 * 1024;

/// A file waiting to be sent, and who to.
#[derive(Clone, Debug)]
pub struct Outgoing {
    /// Who it is for: their identity key, hex. It waits for a call with
    /// them and goes to nobody else.
    pub peer_hex: String,
    /// The platform's handle for the picked file, passed back to
    /// [`platform::read_file`] when its turn comes.
    pub handle: String,
    /// The file's name, as offered to the receiver.
    pub name: String,
    /// Its size in bytes, if the picker reported one.
    pub size: Option<u64>,
}

/// A picked file read into memory and ready to offer.
struct Prepared {
    /// The whole file, in memory.
    source: FileSource,
    /// Its SHA-256, hex, which the receiver checks the reassembled file
    /// against.
    sha256: String,
    /// A PNG preview, base64, or empty when there is none or thumbnails are
    /// off to save data.
    thumbnail_b64: String,
}

/// Where the blocking preparation leaves its result for
/// [`FileManager::poll`] to collect: empty until it is done. Shared with
/// the blocking thread, hence the mutex.
type Slot = Arc<Mutex<Option<Result<Prepared, String>>>>;

/// How far the active send has got.
enum Stage {
    /// Reading and hashing on a blocking thread; the result lands in the
    /// slot.
    Preparing(Slot),
    /// Handed to `file_transfer::send_file`. The flag is raised when that
    /// task returns, which is the only reliable sign it is over: its state
    /// is briefly empty before the task first runs, too.
    Sending(Arc<AtomicBool>),
}

/// The one file being sent now.
pub struct Active {
    /// The queue entry it came from, kept whole so an interrupted send goes
    /// back to the front with its id and held state.
    pub entry: Entry<Outgoing>,
    /// Whether it is still being read or already with `file_transfer`.
    stage: Stage,
    /// The user pressed pause.
    pub user_paused: bool,
    /// The data rules paused it; lifted by the rules, not by the user.
    pub held: Option<Hold>,
    /// Chunks already charged to the cellular meter, so each poll charges
    /// only what moved since the last one.
    counted_chunks: u32,
}

impl Active {
    /// Whether the file is still being read and hashed, before any offer has
    /// gone to the other side.
    pub fn preparing(&self) -> bool {
        matches!(self.stage, Stage::Preparing(_))
    }
}

/// Something the user may want a notification about.
pub enum Event {
    /// They offered a file, waiting for an answer.
    Offer {
        /// The file's name as they offered it.
        name: String,
    },
    /// A received file passed its hash check and was saved.
    Saved {
        /// The file's name as they offered it.
        name: String,
    },
    /// They confirmed a file we sent arrived intact.
    Sent {
        /// The file's name as we offered it.
        name: String,
    },
    /// A transfer either way stopped short.
    Failed {
        /// One sentence saying which file and why.
        line: String,
    },
}

/// The phone's side of file transfer: the send queue, the one send
/// running, and received files saved where the user keeps downloads.
///
/// Owned by the interface and polled on its thread. The transfer itself
/// runs on the call's runtime and is reached only through
/// [`Live::files`](crate::call::Live::files).
pub struct FileManager {
    /// Files waiting their turn, in the order the user set.
    pub queue: TransferQueue<Outgoing>,
    /// The file being sent now, if any.
    pub active: Option<Active>,
    /// Where each received file went, newest first, for the Files screen.
    pub received: Vec<(String, String)>,
    /// The last thing that finished or failed, in one sentence.
    pub last_line: Option<String>,
    /// The app's private storage, where received files go when the platform
    /// has no downloads folder.
    dir: PathBuf,
    /// The transfer id of the last offer notified about, so one offer makes
    /// one notification however often the screen polls.
    seen_offer: Option<u64>,
    /// The incoming transfer's id and the chunks already charged to the
    /// cellular meter, as `counted_chunks` is for the outgoing one.
    counted_incoming: (u64, u32),
}

impl FileManager {
    /// An empty manager that saves received files under `dir` when the
    /// platform has no downloads folder.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            queue: TransferQueue::default(),
            active: None,
            received: Vec::new(),
            last_line: None,
            dir,
            seen_offer: None,
            counted_incoming: (0, 0),
        }
    }

    /// Queues a picked file for `peer_hex`. Refuses one the picker says is
    /// over [`MAX_SEND_BYTES`] now, rather than after the user waited for
    /// its turn.
    pub fn enqueue(&mut self, peer_hex: String, picked: PickedFile) -> Result<(), String> {
        if picked.size.is_some_and(|size| size > MAX_SEND_BYTES) {
            return Err(format!(
                "{} is too big to send from a phone (the limit is 256 MB, because the phone hands the app the whole file at once).",
                picked.name
            ));
        }
        self.queue.push(Outgoing { peer_hex, handle: picked.handle, name: picked.name, size: picked.size });
        Ok(())
    }

    /// How many transfers are going or waiting for this call, for the
    /// platform's background work.
    pub fn transfer_count(&self, live: Option<&Live>) -> usize {
        let Some(live) = live else { return 0 };
        let peer = crate::contacts::hex(&live.peer_key);
        let waiting = self.queue.iter().filter(|e| e.item.peer_hex == peer).count();
        let incoming = live.files.lock().unwrap().incoming.as_ref().is_some_and(|i| !i.done && i.error.is_none());
        waiting + usize::from(self.active.is_some()) + usize::from(incoming)
    }

    /// Moves everything along: finishes, starts, holds and saves. Called
    /// whenever the interface wakes. `metered_bytes` is charged with what
    /// moved over mobile data.
    pub fn poll(
        &mut self,
        live: Option<&Live>,
        hold: Option<Hold>,
        thumbnails: bool,
        metered: Option<(&mut CellularMeter, u64)>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        let Some(live) = live else {
            // The call ended. Anything not finished goes back to the front
            // of the queue for the next call with them; the receiver keeps
            // its partial file, so that send resumes rather than restarts.
            if let Some(active) = self.active.take() {
                self.queue.push_front(active.entry);
            }
            self.seen_offer = None;
            self.counted_incoming = (0, 0);
            return events;
        };

        let mut charged = 0u64;
        self.poll_active(live, hold, &mut events, &mut charged);
        if self.active.is_none() && hold.is_none() {
            self.start_next(live, thumbnails);
        }
        self.poll_incoming(live, &mut events, &mut charged);
        if let Some((meter, now)) = metered {
            if charged > 0 {
                meter.add(charged, now);
            }
        }
        events
    }

    /// Collects a finished preparation and hands it to `file_transfer`,
    /// or, while sending, charges new chunks to the meter, applies the
    /// pause wanted by the user and the rules, and notices when the send is
    /// over.
    fn poll_active(&mut self, live: &Live, hold: Option<Hold>, events: &mut Vec<Event>, charged: &mut u64) {
        let Some(active) = self.active.as_mut() else { return };
        match &active.stage {
            Stage::Preparing(slot) => {
                let Some(result) = slot.lock().unwrap().take() else { return };
                match result {
                    Ok(prepared) => {
                        let finished = Arc::new(AtomicBool::new(false));
                        let flag = finished.clone();
                        let peer = live.peer.clone();
                        let files = live.files.clone();
                        let name = active.entry.item.name.clone();
                        live.runtime().spawn(async move {
                            file_transfer::send_file(
                                peer,
                                files,
                                name,
                                Arc::new(prepared.source),
                                prepared.sha256,
                                prepared.thumbnail_b64,
                            )
                            .await;
                            flag.store(true, Ordering::SeqCst);
                        });
                        active.stage = Stage::Sending(finished);
                        active.counted_chunks = 0;
                    }
                    Err(why) => {
                        events.push(Event::Failed { line: why.clone() });
                        self.last_line = Some(why);
                        self.active = None;
                    }
                }
            }
            Stage::Sending(finished) => {
                let finished = finished.load(Ordering::SeqCst);
                let mut files = live.files.lock().unwrap();
                if let Some(outgoing) = files.outgoing.as_ref() {
                    *charged += (outgoing.chunks_sent.saturating_sub(active.counted_chunks) as u64) * CHUNK_SIZE as u64;
                    active.counted_chunks = outgoing.chunks_sent;
                }
                if !finished {
                    // The rules pause and resume; a user's pause outranks
                    // them in both directions.
                    let want_paused = active.user_paused || hold.is_some();
                    active.held = hold;
                    if let Some(outgoing) = files.outgoing.as_mut() {
                        if outgoing.paused != want_paused && !outgoing.done && outgoing.error.is_none() {
                            outgoing.paused = want_paused;
                        }
                    }
                    return;
                }
                let name = active.entry.item.name.clone();
                let line = match files.outgoing.take() {
                    Some(o) if o.done => {
                        events.push(Event::Sent { name: name.clone() });
                        format!("Sent {name}.")
                    }
                    Some(o) => {
                        let why = o.error.unwrap_or_else(|| "it stopped".into());
                        events.push(Event::Failed { line: format!("{name} was not sent: {why}") });
                        format!("{name} was not sent: {why}")
                    }
                    None => format!("{name} was declined or cancelled."),
                };
                drop(files);
                self.last_line = Some(line);
                self.active = None;
            }
        }
    }

    /// Starts reading the first queued file meant for whoever is on the
    /// call, on a blocking thread.
    fn start_next(&mut self, live: &Live, thumbnails: bool) {
        let peer = crate::contacts::hex(&live.peer_key);
        let Some(entry) = self.queue.take_next(|item| item.peer_hex == peer) else { return };
        let slot: Slot = Arc::new(Mutex::new(None));
        let result = slot.clone();
        let handle = entry.item.handle.clone();
        let name = entry.item.name.clone();
        // Reading and hashing a large file takes a while, and the call's
        // audio runs on the same runtime's async threads.
        live.runtime().spawn_blocking(move || {
            *result.lock().unwrap() = Some(prepare(&handle, &name, thumbnails));
        });
        self.active =
            Some(Active { entry, stage: Stage::Preparing(slot), user_paused: false, held: None, counted_chunks: 0 });
    }

    /// Notices a new offer and a failed incoming transfer, charges received
    /// chunks to the meter, and saves a file that has arrived and passed its
    /// hash check.
    fn poll_incoming(&mut self, live: &Live, events: &mut Vec<Event>, charged: &mut u64) {
        let ready = {
            let mut files = live.files.lock().unwrap();
            if let Some(offer) = files.pending_offer.as_ref() {
                if self.seen_offer != Some(offer.transfer_id) {
                    self.seen_offer = Some(offer.transfer_id);
                    events.push(Event::Offer { name: offer.file_name.clone() });
                }
            }
            if let Some(incoming) = files.incoming.as_ref() {
                let (id, counted) = self.counted_incoming;
                let counted = if id == incoming.transfer_id { counted } else { incoming.chunks_received };
                *charged += (incoming.chunks_received.saturating_sub(counted) as u64) * CHUNK_SIZE as u64;
                self.counted_incoming = (incoming.transfer_id, incoming.chunks_received);
                if let Some(error) = incoming.error.clone() {
                    let line = format!("{} did not arrive: {error}", incoming.file_name);
                    events.push(Event::Failed { line: line.clone() });
                    self.last_line = Some(line);
                    // Cleared so the next offer is not turned away as
                    // overlapping one that is already over.
                    files.incoming = None;
                }
            }
            let ready = files.ready_to_save.take();
            if ready.is_some() {
                files.incoming = None;
            }
            ready
        };
        if let Some((name, stored)) = ready {
            let line = match self.save_received(&name, &stored) {
                Ok(location) => {
                    self.received.insert(0, (name.clone(), location.clone()));
                    self.received.truncate(50);
                    events.push(Event::Saved { name: name.clone() });
                    format!("Received {name}. Saved to {location}.")
                }
                Err(e) => format!("Received {name} but could not save it: {e}"),
            };
            self.last_line = Some(line);
        }
    }

    /// Copies a verified file out of the store to where the user keeps
    /// downloads, or to the app's own folder when the platform has none.
    /// A copy, as on the desktop: the store keeps its own, so the same
    /// file offered again needs no transfer at all.
    fn save_received(&self, name: &str, stored: &Path) -> std::io::Result<String> {
        let dir = platform::downloads_dir().unwrap_or_else(|| self.dir.join("received"));
        std::fs::create_dir_all(&dir)?;
        let target = unique_path(&dir, &safe_name(name));
        std::fs::copy(stored, &target)?;
        Ok(target.display().to_string())
    }

    /// Pauses or resumes the file being sent at the user's request. A
    /// resume does not override a hold from the data rules; the next poll
    /// releases it when the rules allow.
    pub fn set_user_paused(&mut self, live: Option<&Live>, paused: bool) {
        let Some(active) = self.active.as_mut() else { return };
        active.user_paused = paused;
        if let (Some(live), Stage::Sending(_)) = (live, &active.stage) {
            if active.held.is_none() || paused {
                file_transfer::set_paused(&live.files, paused);
            }
        }
    }

    /// Stops the file being sent and tells the other side.
    pub fn cancel_active(&mut self, live: Option<&Live>) {
        let Some(active) = self.active.take() else { return };
        if let (Some(live), Stage::Sending(_)) = (live, &active.stage) {
            let id = live.files.lock().unwrap().outgoing.take().map(|o| o.transfer_id);
            if let Some(transfer_id) = id {
                live.send(ControlMessage::FileCancel { transfer_id });
            }
        }
        self.last_line = Some(format!("Stopped sending {}.", active.entry.item.name));
    }

    /// Accepts the pending offer and tells the sender, or declines it with
    /// the reason shown when there is nowhere to put the file.
    pub fn accept_offer(&mut self, live: &Live) {
        match file_transfer::accept_offer(&live.files, &live.peer, live.runtime()) {
            Some(Ok(transfer_id)) => live.send(ControlMessage::FileAccept { transfer_id }),
            Some(Err((transfer_id, why))) => {
                // Declined rather than left hanging, so the sender is not
                // kept waiting on a file with nowhere to go.
                live.send(ControlMessage::FileReject { transfer_id });
                self.last_line = Some(why);
            }
            None => {}
        }
    }

    /// Declines the pending offer and tells the sender.
    pub fn decline_offer(&mut self, live: &Live) {
        if let Some(transfer_id) = file_transfer::reject_offer(&live.files) {
            live.send(ControlMessage::FileReject { transfer_id });
        }
    }

    /// Stops a file being received. Only offered while nothing is being
    /// sent: the desktop's `cancel` takes the outgoing transfer first.
    pub fn cancel_incoming(&mut self, live: &Live) {
        if live.files.lock().unwrap().outgoing.is_some() {
            return;
        }
        if let Some(transfer_id) = file_transfer::cancel(&live.files) {
            live.send(ControlMessage::FileCancel { transfer_id });
        }
    }
}

/// Reads, hashes and (for a picture, when allowed) thumbnails a picked
/// file, on a blocking thread.
fn prepare(handle: &str, name: &str, thumbnails: bool) -> Result<Prepared, String> {
    let bytes = platform::read_file(handle).map_err(|e| format!("Could not read {name}: {e}"))?;
    if bytes.len() as u64 > MAX_SEND_BYTES {
        return Err(format!("{name} is too big to send from a phone (the limit is 256 MB)."));
    }
    let sha256 = sha256_hex(&bytes);
    let thumbnail_b64 = if thumbnails && crate::file_preview::is_previewable(name) {
        thumbnail(&bytes).map(|png| file_transfer::encode_b64(&png)).unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Prepared { source: FileSource::Memory(bytes), sha256, thumbnail_b64 })
}

/// The desktop's `file_preview::generate`, from bytes instead of a path,
/// with its size rules: a picked file on a phone has no path to open.
fn thumbnail(bytes: &[u8]) -> Option<Vec<u8>> {
    use crate::file_preview::{MAX_THUMBNAIL_BYTES, THUMBNAIL_EDGE};
    let image = image::load_from_memory(bytes).ok()?;
    let thumb = image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE);
    let mut encoded = Vec::new();
    thumb.write_to(&mut std::io::Cursor::new(&mut encoded), image::ImageFormat::Png).ok()?;
    (encoded.len() <= MAX_THUMBNAIL_BYTES).then_some(encoded)
}

/// The sender chose this name, so only its last component is used, and
/// never one that would climb out of the folder.
fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let cleaned: String = last.chars().filter(|c| !c.is_control()).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "received file".into()
    } else {
        cleaned
    }
}

/// `name`, or `name (2)`, `name (3)` ... before the extension, whichever
/// is free: a second file with the same name must not overwrite the first.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some numbered name is free")
}
