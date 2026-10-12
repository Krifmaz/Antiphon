import io, os, re

# The repo root, wherever it is checked out.
B = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load(rel):
    return io.open(B + "\\" + rel, encoding="utf-8").read()


def save(rel, s):
    io.open(B + "\\" + rel, "w", encoding="utf-8", newline="").write(s)
    print("patched", rel)


def sub(s, old, new, label):
    assert s.count(old) == 1, "%s: found %d" % (label, s.count(old))
    return s.replace(old, new)


# ---------------------------------------------------------------- file_transfer.rs
rel = r"crates\gui\src\file_transfer.rs"
s = load(rel)

s = sub(s, """use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};""", """use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};""", "import")

s = sub(s, """use crate::file_integrity::FileSource;

/// What the sender keeps so it can answer repair requests: which transfer,
/// and where its bytes come from.
pub type ResendSource = Arc<Mutex<Option<(u64, Arc<FileSource>)>>>;
""", """use crate::file_integrity::FileSource;
use crate::pacer::Pacer;

/// The most chunks a sender will hold queued for resending.
///
/// The queue is filled by the peer's requests, so it is bounded: a peer
/// asking for the same file a hundred times over should cost a bounded
/// amount of memory, not an unbounded one.
const MAX_REPAIR_QUEUE: usize = 65_536;
""", "resend type")

s = sub(s, """    pub error: Option<String>,
    /// When the receiver last asked for chunks again. The sender waits for
    /// confirmation for a while after its last send; a large transfer
    /// being actively repaired is not one that has gone quiet.
    pub last_repair: Option<Instant>,
}""", """    pub error: Option<String>,
    /// Chunks the receiver asked for again, waiting to be resent.
    ///
    /// Served by the send loop itself, ahead of new chunks, rather than by
    /// a task of their own. That puts repairs under the same pacing, limit
    /// and pause as everything else -- a burst of 256 unpaced resends is
    /// exactly the kind of spike that caused the losses being repaired.
    pub repair_queue: VecDeque<u32>,
    /// Chunks reported lost since the send loop last looked, for the pacer.
    pub reported_lost: u64,
}""", "outgoing fields")

# send_file: signature + body up to the first-pass end.
start = s.index("/// Offers `source` to the peer")
end = s.index("enum OutgoingStatus {")
s = s[:start] + '''/// Offers `source` to the peer, waits (up to 30s) for acceptance, then
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

''' + s[end:]

# handle_control_message: drop the resend parameter, queue repairs instead.
s = sub(s, "pub fn handle_control_message(msg: ControlMessage, state: &Arc<Mutex<FileTransferState>>, peer: &Arc<PeerConnection>, resend: &ResendSource) {",
        "pub fn handle_control_message(msg: ControlMessage, state: &Arc<Mutex<FileTransferState>>, peer: &Arc<PeerConnection>) {", "sig")

start = s.index("        ControlMessage::FileMissing { transfer_id, indices } => {")
end = s.index("/// Feeds one decrypted `File` frame's plaintext")
s = s[:start] + '''        ControlMessage::FileMissing { transfer_id, indices } => {
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

''' + s[end:]

s = sub(s, "const MISSING_REQUEST_INTERVAL: Duration = Duration::from_secs(2);", """/// How often the receiver asks for missing chunks.
///
/// Frequent, because a request covers at most `MAX_REQUEST` chunks and a
/// damaged transfer needs many. Asking again for a chunk already asked for
/// is prevented separately -- see `DiskReassembly::chunks_to_request` --
/// which is what used to force this out to two seconds.
const MISSING_REQUEST_INTERVAL: Duration = Duration::from_millis(250);""", "interval")

# pause tests
s = sub(s, """                paused,
                error: None,
                last_repair: None,
            }),""", """                paused,
                error: None,
                repair_queue: VecDeque::new(),
                reported_lost: 0,
            }),""", "pause ctor")

# integration test plumbing
s = sub(s, """        state_a: Arc<Mutex<FileTransferState>>,
        resend_a: ResendSource,
        state_b""", """        state_a: Arc<Mutex<FileTransferState>>,
        state_b""", "pair struct")
s = sub(s, """        let resend_a: ResendSource = Arc::new(Mutex::new(None));
        let resend_b: ResendSource = Arc::new(Mutex::new(None));
""", "", "pair resend")
s = sub(s, """            let (peer, state, resend) = (peer_a.clone(), state_a.clone(), resend_a.clone());""",
        """            let (peer, state) = (peer_a.clone(), state_a.clone());""", "a loop")
s = sub(s, """                                handle_control_message(m, &state, &peer, &resend);""",
        """                                handle_control_message(m, &state, &peer);""", "a handle")
s = sub(s, """            let (peer, state, resend) = (peer_b.clone(), state_b.clone(), resend_b.clone());""",
        """            let (peer, state) = (peer_b.clone(), state_b.clone());""", "b loop")
s = sub(s, """                            handle_control_message(decoded, &state, &peer, &resend);""",
        """                            handle_control_message(decoded, &state, &peer);""", "b handle")
s = sub(s, """        Pair { peer_a, state_a, resend_a, state_b }""", """        Pair { peer_a, state_a, state_b }""", "pair ret")
s = sub(s, """            pair.state_a.clone(),
            pair.resend_a.clone(),
            "integration.bin".to_string(),""", """            pair.state_a.clone(),
            "integration.bin".to_string(),""", "run send")
save(rel, s)

# ---------------------------------------------------------------- state.rs
rel = r"crates\gui\src\state.rs"
s = load(rel)
s = sub(s, """    /// Where the active outgoing transfer's bytes come from, kept so a
    /// `FileMissing` repair request can read them again -- see
    /// `file_transfer::send_file`.
    file_resend_data: file_transfer::ResendSource,
""", "", "field")
s = sub(s, """    let file_resend_data: file_transfer::ResendSource = Arc::new(Mutex::new(None));
""", "", "init")
s = sub(s, """        let resend = self.file_resend_data.clone();
        runtime.spawn(file_transfer::send_file(
            peer,
            ft,
            resend,
            file_name,""", """        runtime.spawn(file_transfer::send_file(
            peer,
            ft,
            file_name,""", "send")
s = sub(s, "file_transfer::handle_control_message(other, &recv_ft_state, &recv_ft_peer, &recv_ft_resend)",
        "file_transfer::handle_control_message(other, &recv_ft_state, &recv_ft_peer)", "handle")
# remaining references to the resend slot
s = re.sub(r"\n[ \t]*let recv_ft_resend = file_resend_data\.clone\(\);", "", s)
s = re.sub(r"\n[ \t]*file_resend_data,", "", s)
assert "file_resend_data" not in s and "recv_ft_resend" not in s, "resend leftovers"
save(rel, s)

# ---------------------------------------------------------------- transfer_limits.rs
rel = r"crates\gui\src\transfer_limits.rs"
s = load(rel)
start = s.index("    /// How long to wait after sending `chunk_bytes` to stay within the cap.")
end = s.index("    /// A human description of what is currently in force")
s = s[:start] + '''    /// The cap that applies right now, in bytes per second, or `None`
    /// when there is none. Handed to the pacer, which turns it into send
    /// times -- see `crate::pacer` for why per-chunk sleeps did not work.
    pub fn limit_bytes_per_sec(&self, in_call: bool) -> Option<f64> {
        match self.effective_kbps(in_call) {
            0 => None,
            kbps => Some(kbps as f64 * 1024.0),
        }
    }

''' + s[end:]
save(rel, s)

# ---------------------------------------------------------------- content_store.rs
rel = r"crates\gui\src\content_store.rs"
s = load(rel)
s = sub(s, """use std::fs::{File, OpenOptions};""", """use std::collections::HashMap;
use std::fs::{File, OpenOptions};""", "import")
s = sub(s, """const QUIET_BEFORE_TAIL_REQUEST: std::time::Duration = std::time::Duration::from_secs(2);""",
        """const QUIET_BEFORE_TAIL_REQUEST: std::time::Duration = std::time::Duration::from_secs(2);

/// How long after asking for a chunk before asking for it again.
///
/// Long enough for the request to cross, the sender to queue it behind
/// whatever it is already resending, and the chunk to come back. Without
/// this every request round repeated the previous one's list, so the
/// sender resent the same chunks over and over while the ones actually
/// still missing waited their turn.
const REREQUEST_AFTER: std::time::Duration = std::time::Duration::from_secs(1);""", "const")
s = sub(s, """    highest_seen: Option<u32>,
    last_chunk_at: Instant,
}""", """    highest_seen: Option<u32>,
    last_chunk_at: Instant,
    /// When each still-missing chunk was last asked for.
    requested: HashMap<u32, Instant>,
}""", "field")
s = sub(s, """            highest_seen: None,
            last_chunk_at: Instant::now(),
        };""", """            highest_seen: None,
            last_chunk_at: Instant::now(),
            requested: HashMap::new(),
        };""", "init")
s = sub(s, """        self.mark(index);
        self.highest_seen""", """        self.mark(index);
        self.requested.remove(&index);
        self.highest_seen""", "clear requested")
s = sub(s, """    /// tail is fair game too.
    pub fn chunks_to_request(&self) -> Vec<u32> {
        let quiet = self.last_chunk_at.elapsed() >= QUIET_BEFORE_TAIL_REQUEST;
        let limit = if quiet {
            self.chunk_count
        } else {
            self.highest_seen.unwrap_or(0)
        };
        (0..limit).filter(|&i| !self.has_chunk(i)).take(MAX_REQUEST).collect()
    }""", """    /// tail is fair game too. A chunk asked for in the last
    /// [`REREQUEST_AFTER`] is skipped: its answer is probably on the way.
    pub fn chunks_to_request(&mut self) -> Vec<u32> {
        let now = Instant::now();
        let quiet = self.last_chunk_at.elapsed() >= QUIET_BEFORE_TAIL_REQUEST;
        let limit = if quiet {
            self.chunk_count
        } else {
            self.highest_seen.unwrap_or(0)
        };
        let wanted: Vec<u32> = (0..limit)
            .filter(|&i| !self.has_chunk(i))
            .filter(|i| self.requested.get(i).is_none_or(|at| now.duration_since(*at) >= REREQUEST_AFTER))
            .take(MAX_REQUEST)
            .collect();
        for &i in &wanted {
            self.requested.insert(i, now);
        }
        wanted
    }""", "request")
s = sub(s, """        let r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |i| i != 2 && i <= 5);""", """        let mut r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();
        feed(&mut r, &data, |i| i != 2 && i <= 5);""", "gaps mut") if 'let r = DiskReassembly::open(&dir, &hash, "f", data.len() as u64).unwrap();\n        feed(&mut r, &data, |i| i != 2 && i <= 5);' in s else s
s = sub(s, """        assert_eq!(r.chunks_to_request(), vec![2]);
    }""", """        assert_eq!(r.chunks_to_request(), vec![2]);
        // Asked for once; not asked for again while the answer is likely
        // still on its way.
        assert!(r.chunks_to_request().is_empty());
        r.requested.insert(2, Instant::now() - REREQUEST_AFTER);
        assert_eq!(r.chunks_to_request(), vec![2], "never asked again after the wait");
    }""", "gaps test")
save(rel, s)
