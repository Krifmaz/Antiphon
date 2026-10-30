//! Files to and from the people you call: the send queue, the one
//! transfer running, and received files saved where the user can find them.
//!
//! The transfer itself is the desktop's `file_transfer` module on the
//! call's runtime -- the offer, chunking, repair requests and hash check
//! are its, unchanged, which is what lets a phone and a desktop swap files.
//! What this adds is what a phone needs around it: picked files arrive as
//! a platform handle rather than a path and are read through it a window
//! at a time, transfers wait in a queue the user can reorder (and that
//! outlives the app, so a file picked for someone between calls goes at
//! their next one), and the data rules can hold them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use antiphon_net::file_transfer::{sha256_hex, sha256_hex_reader, CHUNK_SIZE};
use serde::{Deserialize, Serialize};

use crate::call::Live;
use crate::control::ControlMessage;
use crate::data_rules::{CellularMeter, Hold};
use crate::file_integrity::FileSource;
use crate::file_transfer;
use crate::platform::{self, PickedFile};
use crate::queue::{self, Entry, TransferQueue};
use crate::stream::Windowed;

/// The largest file sent when the platform cannot read it in ranges and
/// hands it over as one buffer instead (an Android provider that serves a
/// pipe rather than a file, say). Every other file is streamed and has no
/// limit beyond the protocol's.
const MAX_IN_MEMORY_BYTES: u64 = 256 * 1024 * 1024;

/// The largest picture read whole to make a thumbnail of. Decoding needs
/// it all in memory, and a preview is not worth holding a huge file for.
const MAX_THUMBNAIL_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// The file the send queue is kept in, under the app's private storage,
/// so files picked for someone survive the app being closed before their
/// next call.
const QUEUE_FILE: &str = "send-queue.json";

/// A file waiting to be sent, and who to. Saved with the queue, so every
/// field is something that still means the same after a restart: the
/// platform handle does, a path would not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    /// Who it is for: their identity key, hex. It waits for a call with
    /// them and goes to nobody else.
    pub peer_hex: String,
    /// The platform's handle for the picked file, passed back to
    /// [`platform::read_file_range`] when its turn comes.
    pub handle: String,
    /// The file's name, as offered to the receiver.
    pub name: String,
    /// Its size in bytes, if the picker reported one.
    pub size: Option<u64>,
}

/// A picked file measured and hashed, and ready to offer.
struct Prepared {
    /// Where the bytes come from as they are sent: read through the
    /// platform a window at a time, or, when it cannot do that, in memory.
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
    /// The queue as last written to [`QUEUE_FILE`], so it is written only
    /// when something changed.
    saved: Vec<(Outgoing, bool)>,
    /// The incoming offer's thumbnail decoded for the screen, by transfer
    /// id, so a picture is decoded once rather than on every redraw.
    offer_thumbnail: Option<(u64, Option<iced_core::image::Handle>)>,
}

impl FileManager {
    /// A manager with the queue saved in `dir`, which also saves received
    /// files under `dir` when the platform has no downloads folder. A
    /// missing or unreadable queue file starts an empty queue: losing the
    /// list of what was waiting is better than not starting.
    pub fn new(dir: PathBuf) -> Self {
        let saved = load_queue(&dir);
        Self {
            queue: TransferQueue::restore(saved.iter().cloned()),
            saved,
            offer_thumbnail: None,
            active: None,
            received: Vec::new(),
            last_line: None,
            dir,
            seen_offer: None,
            counted_incoming: (0, 0),
        }
    }

    /// Queues a picked file for `peer_hex`, to go during the call with them
    /// now or their next one, and saves the queue straight away so a file
    /// picked just before the app is closed is not forgotten.
    pub fn enqueue(&mut self, peer_hex: String, picked: PickedFile) {
        self.queue.push(Outgoing { peer_hex, handle: picked.handle, name: picked.name, size: picked.size });
        self.save_queue();
    }

    /// Writes the queue to [`QUEUE_FILE`] if it changed since the last
    /// write. The file being sent goes first, so a send cut short by the
    /// app closing is tried again first at the next call, as it would be
    /// had only the call ended. Called after every poll, which follows
    /// every change; a failure is logged and tried again at the next one.
    pub fn save_queue(&mut self) {
        let now: Vec<(Outgoing, bool)> =
            self.active.iter().map(|a| &a.entry).chain(self.queue.iter()).map(|e| (e.item.clone(), e.held)).collect();
        if now == self.saved {
            return;
        }
        match queue::save(&self.dir.join(QUEUE_FILE), &now) {
            Ok(()) => self.saved = now,
            Err(e) => log::warn!("could not save the send queue: {e}"),
        }
    }

    /// The thumbnail the sender made for the offer waiting, decoded for the
    /// image widget, or `None` when they sent none. Decoded once per offer;
    /// its size was checked on arrival (`file_preview::accept_thumbnail`),
    /// so this only has to turn PNG into pixels.
    pub fn offer_thumbnail(&mut self, transfer_id: u64, png: &[u8]) -> Option<iced_core::image::Handle> {
        match &self.offer_thumbnail {
            Some((id, handle)) if *id == transfer_id => handle.clone(),
            _ => {
                let handle = decode_thumbnail(png);
                self.offer_thumbnail = Some((transfer_id, handle.clone()));
                handle
            }
        }
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
            self.offer_thumbnail = None;
            self.save_queue();
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
        self.save_queue();
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
        // Hashing a large file takes a while, and the call's audio runs on
        // the same runtime's async threads.
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
    pub fn decline_offer(&self, live: &Live) {
        if let Some(transfer_id) = file_transfer::reject_offer(&live.files) {
            live.send(ControlMessage::FileReject { transfer_id });
        }
    }

    /// Stops a file being received. Only offered while nothing is being
    /// sent: the desktop's `cancel` takes the outgoing transfer first.
    pub fn cancel_incoming(&self, live: &Live) {
        if live.files.lock().unwrap().outgoing.is_some() {
            return;
        }
        if let Some(transfer_id) = file_transfer::cancel(&live.files) {
            live.send(ControlMessage::FileCancel { transfer_id });
        }
    }
}

/// Measures, hashes and (for a picture, when allowed) thumbnails a picked
/// file, on a blocking thread, reading it through the platform a window at
/// a time so a file of any size costs only a window of memory.
///
/// The length is measured before and after hashing, as the desktop's
/// `file_integrity::open_and_hash` does with a file on disk: if it moved,
/// something is still writing the file and there is no one version of it
/// to offer. A platform that cannot read in ranges hands the whole file
/// over instead, up to [`MAX_IN_MEMORY_BYTES`].
fn prepare(handle: &str, name: &str, thumbnails: bool) -> Result<Prepared, String> {
    let unreadable = |e: std::io::Error| format!("Could not read {name}: {e}");
    let len = match platform::file_size(handle) {
        Ok(len) => len,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return prepare_in_memory(handle, name, thumbnails),
        Err(e) => return Err(unreadable(e)),
    };
    let owned = handle.to_string();
    let file = Windowed::new(len, move |offset, len| platform::read_file_range(&owned, offset, len));
    let sha256 = sha256_hex_reader(file.reader()).map_err(unreadable)?;
    if platform::file_size(handle).map_err(unreadable)? != len {
        return Err(format!(
            "{name} changed while it was being read. Wait for whatever is writing it to finish, then send it again."
        ));
    }
    let previewable = thumbnails && crate::file_preview::is_previewable(name) && len <= MAX_THUMBNAIL_SOURCE_BYTES;
    let thumbnail_b64 = previewable
        .then(|| file.read_at(0, len as usize).ok())
        .flatten()
        .and_then(|bytes| thumbnail(&bytes))
        .map(|png| file_transfer::encode_b64(&png))
        .unwrap_or_default();
    let source = FileSource::Ranged { len, read: Box::new(move |offset, len| file.read_at(offset, len)) };
    Ok(Prepared { source, sha256, thumbnail_b64 })
}

/// [`prepare`] for a file the platform can only hand over whole, which
/// has to fit in memory.
fn prepare_in_memory(handle: &str, name: &str, thumbnails: bool) -> Result<Prepared, String> {
    let bytes = platform::read_file(handle).map_err(|e| format!("Could not read {name}: {e}"))?;
    if bytes.len() as u64 > MAX_IN_MEMORY_BYTES {
        return Err(format!(
            "{name} is too big to send: the app that holds it only hands it over whole, and the limit for that is 256 MB."
        ));
    }
    let sha256 = sha256_hex(&bytes);
    let thumbnail_b64 = if thumbnails && crate::file_preview::is_previewable(name) {
        thumbnail(&bytes).map(|png| file_transfer::encode_b64(&png)).unwrap_or_default()
    } else {
        String::new()
    };
    Ok(Prepared { source: FileSource::Memory(bytes), sha256, thumbnail_b64 })
}

/// The send queue saved in `dir`, front first, with each file's held
/// state, or nothing when there is none or it cannot be read.
fn load_queue(dir: &Path) -> Vec<(Outgoing, bool)> {
    match queue::load(&dir.join(QUEUE_FILE)) {
        Ok(saved) => saved,
        Err(e) => {
            log::warn!("the saved send queue could not be read, so it starts empty: {e}");
            Vec::new()
        }
    }
}

/// A sender's PNG thumbnail as pixels for the image widget. Decoded here
/// rather than handed over as PNG so a bad picture is found once, now, and
/// shows as no picture rather than failing inside the renderer.
fn decode_thumbnail(png: &[u8]) -> Option<iced_core::image::Handle> {
    if png.is_empty() {
        return None;
    }
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.into_rgba8();
    let (width, height) = image.dimensions();
    Some(iced_core::image::Handle::from_rgba(width, height, image.into_raw()))
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
