//! Holding sealed files (spec §5): what a seeder, a pinning device and a
//! server in "server-hosted mode" all do.
//!
//! Every file here is a list of chunks sealed under a key this holder
//! never has, checked on the way in against the file's id, so the store
//! is bytes it cannot read. The same [`FileHost`] serves:
//!
//! - **Seeding** from the app, with the user's seeding scope.
//! - **Pinning and server-hosted files** on `antiphon-relay --files`:
//!   uploads off by default; a members code for a community-only node;
//!   a maximum file size, a total quota and a per-uploader quota; and
//!   retention -- keep forever, expire after N days, or expire if nobody
//!   opened it within N days.
//!
//! The operator's view is [`FileHost::usage`] and [`FileStore::list`]:
//! sizes, ages and how often each was read -- never a name or a byte of
//! content, which the store doesn't have. [`FileStore::purge`] removes a
//! file, and from then on it answers "no longer shared".

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use antiphon_crypto::file_seal::{self, ChunkHash, FileId, CHUNK, TAG};
use antiphon_net::blob::{claim_bytes, Message, Refusal, HASHES_PER_PAGE, MAX_SEALED_CHUNK, PUT_INFO_ACK};
use serde::{Deserialize, Serialize};

/// Who a held file is handed to (spec §5 "Seeding scope").
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Only people it was shared with: they must show its read token.
    Conversation,
    /// Those people, and only if they prove they're a saved contact.
    Contacts,
    /// Those people, and only if they prove they're a member of this
    /// community (by its id), checked against the holder's copy of its
    /// signed member list at that moment -- so a ban stops serving them.
    Community([u8; 32]),
    /// Anyone who knows its id. They still can't read it.
    Anyone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retention {
    Forever,
    /// Removed this long after it arrived.
    AfterDays(u32),
    /// Removed if nobody has downloaded any of it this long after it
    /// arrived.
    UnopenedDays(u32),
}

#[derive(Clone, Debug)]
pub struct HostPolicy {
    /// Answer downloads at all.
    pub serve: bool,
    /// Accept uploads (pinning, server-hosted mode). Off by default.
    pub accept_uploads: bool,
    /// `Some` for a community-only node: uploading takes this code.
    pub upload_code: Option<Vec<u8>>,
    pub max_file_bytes: u64,
    pub total_quota_bytes: u64,
    pub per_uploader_quota_bytes: u64,
    pub retention: Retention,
}

impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            serve: true,
            accept_uploads: false,
            upload_code: None,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            total_quota_bytes: 20 * 1024 * 1024 * 1024,
            per_uploader_quota_bytes: 2 * 1024 * 1024 * 1024,
            retention: Retention::Forever,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Meta {
    pub size: u64,
    pub chunks: u32,
    #[serde(default)]
    pub read_token: Option<[u8; 32]>,
    pub scope: Scope,
    /// Per-node uploader label, for quotas. Zero for the app's own seeds.
    #[serde(default)]
    pub uploader: [u8; 32],
    pub created_secs: u64,
    #[serde(default)]
    pub reads: u64,
    #[serde(default)]
    pub last_read_secs: Option<u64>,
    /// Which chunks are present; complete once all are.
    pub have: Vec<bool>,
}

impl Meta {
    pub fn complete(&self) -> bool {
        self.have.iter().all(|h| *h)
    }

    pub fn stored_bytes(&self) -> u64 {
        self.size + self.chunks as u64 * TAG as u64
    }
}

/// Sealed files on disk, one directory each.
pub struct FileStore {
    root: PathBuf,
    metas: HashMap<FileId, Meta>,
    hashes: HashMap<FileId, Vec<ChunkHash>>,
    removed: HashSet<FileId>,
}

fn hex(b: &[u8]) -> String {
    antiphon_crypto::hex::encode(b)
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    antiphon_crypto::hex::decode_array(s)
}

pub fn sealed_len(size: u64, index: u32) -> usize {
    let start = index as u64 * CHUNK as u64;
    let plain = size.saturating_sub(start).min(CHUNK as u64) as usize;
    plain + TAG
}

impl FileStore {
    pub fn open(root: &Path) -> std::io::Result<Self> {
        fs::create_dir_all(root)?;
        let mut store =
            Self { root: root.to_path_buf(), metas: HashMap::new(), hashes: HashMap::new(), removed: HashSet::new() };
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().and_then(unhex32) else { continue };
            let dir = entry.path();
            let Ok(meta) = fs::read(dir.join("meta.json")).map_err(|_| ()).and_then(|b| {
                serde_json::from_slice::<Meta>(&b).map_err(|_| ())
            }) else {
                continue;
            };
            let Ok(raw) = fs::read(dir.join("hashes.bin")) else { continue };
            let hashes: Vec<ChunkHash> = raw.chunks_exact(32).map(|c| c.try_into().expect("32")).collect();
            if hashes.len() != meta.chunks as usize {
                continue;
            }
            store.metas.insert(id, meta);
            store.hashes.insert(id, hashes);
        }
        if let Ok(text) = fs::read_to_string(root.join("removed.txt")) {
            store.removed = text.lines().filter_map(unhex32).collect();
        }
        Ok(store)
    }

    fn dir(&self, id: &FileId) -> PathBuf {
        self.root.join(hex(id))
    }

    pub fn meta(&self, id: &FileId) -> Option<&Meta> {
        self.metas.get(id)
    }

    pub fn list(&self) -> Vec<(FileId, Meta)> {
        let mut out: Vec<_> = self.metas.iter().map(|(k, v)| (*k, v.clone())).collect();
        out.sort_by_key(|(_, m)| m.created_secs);
        out
    }

    pub fn used_bytes(&self) -> u64 {
        self.metas.values().map(Meta::stored_bytes).sum()
    }

    fn used_by(&self, uploader: &[u8; 32]) -> u64 {
        self.metas.values().filter(|m| &m.uploader == uploader).map(Meta::stored_bytes).sum()
    }

    /// Starts holding a file whose hashes are known and checked.
    #[expect(clippy::too_many_arguments)]
    pub fn begin(
        &mut self,
        id: FileId,
        size: u64,
        hashes: Vec<ChunkHash>,
        scope: Scope,
        read_token: Option<[u8; 32]>,
        uploader: [u8; 32],
        now_secs: u64,
    ) -> std::io::Result<()> {
        if file_seal::file_id(size, &hashes) != id {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "hashes don't match the id"));
        }
        if self.metas.contains_key(&id) {
            return Ok(());
        }
        let dir = self.dir(&id);
        fs::create_dir_all(&dir)?;
        let raw: Vec<u8> = hashes.iter().flatten().copied().collect();
        fs::write(dir.join("hashes.bin"), raw)?;
        let meta = Meta {
            size,
            chunks: hashes.len() as u32,
            read_token,
            scope,
            uploader,
            created_secs: now_secs,
            reads: 0,
            last_read_secs: None,
            have: vec![false; hashes.len()],
        };
        self.removed.remove(&id);
        self.metas.insert(id, meta);
        self.hashes.insert(id, hashes);
        self.save_meta(&id)
    }

    fn save_meta(&self, id: &FileId) -> std::io::Result<()> {
        let meta = self.metas.get(id).expect("meta exists");
        fs::write(self.dir(id).join("meta.json"), serde_json::to_vec(meta)?)
    }

    /// Stores one sealed chunk, if it matches its hash.
    pub fn put_chunk(&mut self, id: &FileId, index: u32, data: &[u8]) -> Result<(), Refusal> {
        let hashes = self.hashes.get(id).ok_or(Refusal::NotHere)?;
        let expected = hashes.get(index as usize).ok_or(Refusal::BadData)?;
        let meta = self.metas.get(id).ok_or(Refusal::NotHere)?;
        if data.len() != sealed_len(meta.size, index) || &file_seal::chunk_hash(data) != expected {
            return Err(Refusal::BadData);
        }
        if meta.have[index as usize] {
            return Ok(());
        }
        let mut f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir(id).join("chunks.bin"))
            .map_err(|_| Refusal::Full)?;
        f.seek(SeekFrom::Start(index as u64 * MAX_SEALED_CHUNK as u64)).map_err(|_| Refusal::Full)?;
        f.write_all(data).map_err(|_| Refusal::Full)?;
        let meta = self.metas.get_mut(id).expect("checked");
        meta.have[index as usize] = true;
        // Saved at the end and every so often, not per chunk.
        if meta.complete() || index.is_multiple_of(256) {
            let _ = self.save_meta(id);
        }
        Ok(())
    }

    pub fn read_chunk(&self, id: &FileId, index: u32) -> Option<Vec<u8>> {
        let meta = self.metas.get(id)?;
        if !*meta.have.get(index as usize)? {
            return None;
        }
        let mut f = File::open(self.dir(id).join("chunks.bin")).ok()?;
        f.seek(SeekFrom::Start(index as u64 * MAX_SEALED_CHUNK as u64)).ok()?;
        let mut buf = vec![0u8; sealed_len(meta.size, index)];
        f.read_exact(&mut buf).ok()?;
        // Disk can rot; never hand out a chunk that no longer matches.
        (file_seal::chunk_hash(&buf) == self.hashes[id][index as usize]).then_some(buf)
    }

    pub fn hashes(&self, id: &FileId) -> Option<&[ChunkHash]> {
        self.hashes.get(id).map(Vec::as_slice)
    }

    /// Removes a file. From then on it answers "no longer shared".
    pub fn purge(&mut self, id: &FileId) -> bool {
        let existed = self.metas.remove(id).is_some();
        self.hashes.remove(id);
        let _ = fs::remove_dir_all(self.dir(id));
        if existed && self.removed.insert(*id) {
            let mut text = self.removed.iter().map(|r| hex(r)).collect::<Vec<_>>().join("\n");
            text.push('\n');
            let _ = fs::write(self.root.join("removed.txt"), text);
        }
        existed
    }

    pub fn was_removed(&self, id: &FileId) -> bool {
        self.removed.contains(id)
    }

    fn note_read(&mut self, id: &FileId, now_secs: u64) {
        if let Some(m) = self.metas.get_mut(id) {
            let first = m.reads == 0;
            m.reads += 1;
            m.last_read_secs = Some(now_secs);
            if first {
                let _ = self.save_meta(id);
            }
        }
    }

    /// Applies a retention policy. Returns how many files went.
    pub fn sweep(&mut self, retention: Retention, now_secs: u64) -> usize {
        let day = 86_400u64;
        let expired: Vec<FileId> = self
            .metas
            .iter()
            .filter(|(_, m)| match retention {
                Retention::Forever => false,
                Retention::AfterDays(d) => now_secs >= m.created_secs + d as u64 * day,
                Retention::UnopenedDays(d) => m.reads == 0 && now_secs >= m.created_secs + d as u64 * day,
            })
            .map(|(k, _)| *k)
            .collect();
        for id in &expired {
            self.purge(id);
        }
        expired.len()
    }
}

/// Aggregate counters for the operator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub files: usize,
    pub stored_bytes: u64,
    pub chunks_served: u64,
    pub bytes_served: u64,
    pub refused: u64,
}

/// Is this identity someone the holder saved as a contact?
pub type ContactCheck = Arc<dyn Fn(&[u8; 32]) -> bool + Send + Sync>;

/// Is this identity a member of this community (by id), as far as the
/// holder's copy of its signed manifest says?
pub type MemberCheck = Arc<dyn Fn(&[u8; 32], &[u8; 32]) -> bool + Send + Sync>;

/// How long a contact's claim lets their address download a file.
pub const CLAIM_LIFETIME: Duration = Duration::from_secs(600);

struct Pending {
    size: u64,
    uploader: [u8; 32],
    read_token: Option<[u8; 32]>,
    hashes: Vec<Option<ChunkHash>>,
}

pub struct FileHost {
    pub store: FileStore,
    pub policy: HostPolicy,
    contacts: ContactCheck,
    members: MemberCheck,
    claims: HashMap<(SocketAddr, FileId), Instant>,
    pending: HashMap<FileId, Pending>,
    usage: Usage,
}

impl FileHost {
    pub fn new(store: FileStore, policy: HostPolicy, contacts: ContactCheck) -> Self {
        Self {
            store,
            policy,
            contacts,
            members: Arc::new(|_, _| false),
            claims: HashMap::new(),
            pending: HashMap::new(),
            usage: Usage::default(),
        }
    }

    /// Serves "this community" scope (spec §5) with `members` as the
    /// membership check. Without it nobody passes one.
    pub fn with_members(mut self, members: MemberCheck) -> Self {
        self.members = members;
        self
    }

    pub fn usage(&self) -> Usage {
        Usage { files: self.store.metas.len(), stored_bytes: self.store.used_bytes(), ..self.usage }
    }

    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant, now_secs: u64) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(request) = Message::decode(bytes) else { return Vec::new() };
        let reply = self.answer(from, request, now, now_secs);
        let Some(reply) = reply else { return Vec::new() };
        match &reply {
            Message::Refused(_) => self.usage.refused += 1,
            Message::ChunkData { data, .. } => {
                self.usage.chunks_served += 1;
                self.usage.bytes_served += data.len() as u64;
            }
            _ => {}
        }
        vec![(from, reply.encode())]
    }

    /// Whether this request may read `id`; the refusal if not.
    fn may_read(&self, from: SocketAddr, id: &FileId, token: &[u8; 32], now: Instant) -> Result<(), Refusal> {
        if !self.policy.serve {
            return Err(Refusal::Disabled);
        }
        let Some(meta) = self.store.meta(id) else {
            return Err(if self.store.was_removed(id) { Refusal::NoLongerShared } else { Refusal::NotHere });
        };
        let token_ok = match &meta.read_token {
            Some(t) => constant_time_eq(t, token),
            None => true,
        };
        match meta.scope {
            Scope::Anyone => Ok(()),
            Scope::Conversation if token_ok => Ok(()),
            Scope::Contacts | Scope::Community(_) if token_ok => {
                let claimed = self.claims.get(&(from, *id)).is_some_and(|t| now.duration_since(*t) < CLAIM_LIFETIME);
                if claimed {
                    Ok(())
                } else {
                    Err(Refusal::NotAContact)
                }
            }
            _ => Err(Refusal::Forbidden),
        }
    }

    fn answer(&mut self, from: SocketAddr, request: Message, now: Instant, now_secs: u64) -> Option<Message> {
        let refuse = |r| Some(Message::Refused(r));
        match request {
            Message::Info { file, token } => match self.may_read(from, &file, &token, now) {
                Err(r) => refuse(r),
                Ok(()) => {
                    let m = self.store.meta(&file)?;
                    if !m.complete() {
                        return refuse(Refusal::NotHere);
                    }
                    Some(Message::InfoReply { size: m.size, chunks: m.chunks })
                }
            },
            Message::Hashes { file, token, page } => match self.may_read(from, &file, &token, now) {
                Err(r) => refuse(r),
                Ok(()) => {
                    let all = self.store.hashes(&file)?;
                    let start = page as usize * HASHES_PER_PAGE;
                    let hashes = all.get(start..(start + HASHES_PER_PAGE).min(all.len()))?.to_vec();
                    Some(Message::HashPage { page, hashes })
                }
            },
            Message::Chunk { file, token, index } => match self.may_read(from, &file, &token, now) {
                Err(r) => refuse(r),
                Ok(()) => match self.store.read_chunk(&file, index) {
                    Some(data) => {
                        if index == 0 {
                            self.store.note_read(&file, now_secs);
                        }
                        Some(Message::ChunkData { index, data })
                    }
                    None => refuse(Refusal::NotHere),
                },
            },
            Message::Claim { file, identity, at_secs, signature } => {
                // A claim more than ten minutes either side of our clock
                // is refused, so a captured one can't be replayed later.
                if at_secs.abs_diff(now_secs) > CLAIM_LIFETIME.as_secs() {
                    return refuse(Refusal::NotAContact);
                }
                // The same claim proves either; which group they must be in
                // is the file's scope, not anything the asker says.
                let belongs = match self.store.meta(&file).map(|m| m.scope) {
                    Some(Scope::Community(community)) => (self.members)(&community, &identity),
                    _ => (self.contacts)(&identity),
                };
                if antiphon_crypto::verify_signature(&identity, &claim_bytes(&file, at_secs), &signature).is_err() || !belongs {
                    return refuse(Refusal::NotAContact);
                }
                self.claims.retain(|_, t| now.duration_since(*t) < CLAIM_LIFETIME);
                self.claims.insert((from, file), now);
                Some(Message::ClaimOk)
            }
            Message::PutInfo { file, size, uploader, read_token, code } => {
                if !self.policy.accept_uploads {
                    return refuse(Refusal::Disabled);
                }
                if let Some(expected) = &self.policy.upload_code {
                    if !constant_time_eq(expected, &code) {
                        return refuse(Refusal::Forbidden);
                    }
                }
                if self.store.meta(&file).is_some_and(Meta::complete) {
                    return Some(Message::PutOk { which: PUT_INFO_ACK });
                }
                let stored = size + file_seal::chunk_count(size) as u64 * TAG as u64;
                if size > self.policy.max_file_bytes {
                    return refuse(Refusal::TooBig);
                }
                if self.store.used_bytes() + stored > self.policy.total_quota_bytes {
                    return refuse(Refusal::Full);
                }
                if self.store.used_by(&uploader) + stored > self.policy.per_uploader_quota_bytes {
                    return refuse(Refusal::OverQuota);
                }
                let chunks = file_seal::chunk_count(size) as usize;
                let read_token = (read_token != [0; 32]).then_some(read_token);
                self.pending.entry(file).or_insert(Pending { size, uploader, read_token, hashes: vec![None; chunks] });
                Some(Message::PutOk { which: PUT_INFO_ACK })
            }
            Message::PutHashes { file, uploader, page, hashes } => {
                if self.store.meta(&file).is_some() {
                    return Some(Message::PutOk { which: page });
                }
                let Some(p) = self.pending.get_mut(&file).filter(|p| p.uploader == uploader) else {
                    return refuse(Refusal::NotHere);
                };
                let start = page as usize * HASHES_PER_PAGE;
                for (i, h) in hashes.into_iter().enumerate() {
                    match p.hashes.get_mut(start + i) {
                        Some(slot) => *slot = Some(h),
                        None => return refuse(Refusal::BadData),
                    }
                }
                if p.hashes.iter().all(Option::is_some) {
                    let p = self.pending.remove(&file).expect("present");
                    let hashes: Vec<ChunkHash> = p.hashes.into_iter().map(|h| h.expect("all some")).collect();
                    let scope = if p.read_token.is_some() { Scope::Conversation } else { Scope::Anyone };
                    if self.store.begin(file, p.size, hashes, scope, p.read_token, p.uploader, now_secs).is_err() {
                        return refuse(Refusal::BadData);
                    }
                }
                Some(Message::PutOk { which: page })
            }
            Message::PutChunk { file, uploader, index, data } => {
                match self.store.meta(&file) {
                    Some(m) if m.uploader == uploader => {}
                    Some(_) => return refuse(Refusal::Forbidden),
                    None => return refuse(Refusal::NotHere),
                }
                match self.store.put_chunk(&file, index, &data) {
                    Ok(()) => Some(Message::PutOk { which: index }),
                    Err(r) => refuse(r),
                }
            }
            _ => None,
        }
    }

    pub fn sweep(&mut self, now_secs: u64) -> usize {
        self.store.sweep(self.policy.retention, now_secs)
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::IdentityKeyPair;

    const FROM: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9);

    fn temp_dir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        let mut r = [0u8; 8];
        getrandom::fill(&mut r).unwrap();
        d.push(format!("antiphon-files-{tag}-{}", hex(&r)));
        d
    }

    fn host(policy: HostPolicy, contacts: ContactCheck) -> FileHost {
        FileHost::new(FileStore::open(&temp_dir("t")).unwrap(), policy, contacts)
    }

    fn ask(h: &mut FileHost, m: Message, now_secs: u64) -> Message {
        let out = h.handle(FROM, &m.encode(), Instant::now(), now_secs);
        Message::decode(&out[0].1).unwrap()
    }

    fn seed(h: &mut FileHost, plain: &[u8], scope: Scope) -> (FileId, [u8; 32], Vec<Vec<u8>>) {
        let key = file_seal::new_key();
        let (id, chunks) = file_seal::seal_all(&key, plain);
        let hashes = chunks.iter().map(|c| file_seal::chunk_hash(c)).collect();
        let token = file_seal::read_token(&key);
        h.store.begin(id, plain.len() as u64, hashes, scope, Some(token), [0; 32], 0).unwrap();
        for (i, c) in chunks.iter().enumerate() {
            h.store.put_chunk(&id, i as u32, c).unwrap();
        }
        (id, token, chunks)
    }

    #[test]
    fn a_seeded_file_is_served_to_people_with_the_token_only() {
        let mut h = host(HostPolicy::default(), Arc::new(|_| false));
        let (id, token, chunks) = seed(&mut h, &[5u8; 3000], Scope::Conversation);
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 0), Message::InfoReply { size: 3000, chunks: 3 });
        assert_eq!(ask(&mut h, Message::Chunk { file: id, token, index: 2 }, 0), Message::ChunkData { index: 2, data: chunks[2].clone() });
        assert_eq!(ask(&mut h, Message::Chunk { file: id, token: [0; 32], index: 2 }, 0), Message::Refused(Refusal::Forbidden));
    }

    #[test]
    fn community_scope_needs_a_signed_claim_from_a_member_of_that_community() {
        let member = IdentityKeyPair::generate();
        let contact = IdentityKeyPair::generate();
        let (member_key, contact_key) = (member.public().identity_key, contact.public().identity_key);
        let choir = [7u8; 32];
        let mut h = host(HostPolicy::default(), Arc::new(move |k| *k == contact_key))
            .with_members(Arc::new(move |c, k| *c == choir && *k == member_key));
        let (id, token, _) = seed(&mut h, b"rehearsal notes", Scope::Community(choir));
        let (other, other_token, _) = seed(&mut h, b"another community's", Scope::Community([8; 32]));
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 100), Message::Refused(Refusal::NotAContact));
        // Being a contact is not being a member.
        let claim = |who: &IdentityKeyPair, file| Message::Claim { file, identity: who.public().identity_key, at_secs: 100, signature: who.sign(&claim_bytes(&file, 100)) };
        assert_eq!(ask(&mut h, claim(&contact, id), 100), Message::Refused(Refusal::NotAContact));
        // Nor is a member of one community a member of another.
        assert_eq!(ask(&mut h, claim(&member, other), 100), Message::Refused(Refusal::NotAContact));
        assert_eq!(ask(&mut h, Message::Info { file: other, token: other_token }, 100), Message::Refused(Refusal::NotAContact));
        assert_eq!(ask(&mut h, claim(&member, id), 100), Message::ClaimOk);
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 100), Message::InfoReply { size: 15, chunks: 1 });
        // The read token is still needed on top.
        assert_eq!(ask(&mut h, Message::Info { file: id, token: [0; 32] }, 100), Message::Refused(Refusal::Forbidden));
    }

    #[test]
    fn contacts_scope_needs_a_signed_claim_from_a_contact() {
        let friend = IdentityKeyPair::generate();
        let stranger = IdentityKeyPair::generate();
        let friend_key = friend.public().identity_key;
        let mut h = host(HostPolicy::default(), Arc::new(move |k| *k == friend_key));
        let (id, token, _) = seed(&mut h, b"hello", Scope::Contacts);
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 100), Message::Refused(Refusal::NotAContact));
        let bad = Message::Claim {
            file: id,
            identity: stranger.public().identity_key,
            at_secs: 100,
            signature: stranger.sign(&claim_bytes(&id, 100)),
        };
        assert_eq!(ask(&mut h, bad, 100), Message::Refused(Refusal::NotAContact));
        let stale = Message::Claim { file: id, identity: friend_key, at_secs: 100, signature: friend.sign(&claim_bytes(&id, 100)) };
        assert_eq!(ask(&mut h, stale, 100 + 3600), Message::Refused(Refusal::NotAContact), "replayed an hour later");
        let good = Message::Claim { file: id, identity: friend_key, at_secs: 100, signature: friend.sign(&claim_bytes(&id, 100)) };
        assert_eq!(ask(&mut h, good, 100), Message::ClaimOk);
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 100), Message::InfoReply { size: 5, chunks: 1 });
    }

    #[test]
    fn hashes_come_in_pages() {
        let mut h = host(HostPolicy::default(), Arc::new(|_| false));
        let (id, token, chunks) = seed(&mut h, &vec![1u8; CHUNK * 40], Scope::Conversation);
        let Message::HashPage { hashes, .. } = ask(&mut h, Message::Hashes { file: id, token, page: 1 }, 0) else { panic!() };
        assert_eq!(hashes.len(), 8);
        assert_eq!(hashes[0], file_seal::chunk_hash(&chunks[32]));
    }

    #[test]
    fn a_pinned_upload_is_checked_chunk_by_chunk_and_quotas_hold() {
        let policy = HostPolicy { accept_uploads: true, per_uploader_quota_bytes: 10_000, ..Default::default() };
        let mut h = host(policy, Arc::new(|_| false));
        let key = file_seal::new_key();
        let plain = vec![3u8; 2500];
        let (id, chunks) = file_seal::seal_all(&key, &plain);
        let hashes: Vec<_> = chunks.iter().map(|c| file_seal::chunk_hash(c)).collect();
        let token = file_seal::read_token(&key);
        let up = [9u8; 32];
        assert_eq!(
            ask(&mut h, Message::PutInfo { file: id, size: 2500, uploader: up, read_token: token, code: vec![] }, 0),
            Message::PutOk { which: PUT_INFO_ACK }
        );
        assert_eq!(ask(&mut h, Message::PutHashes { file: id, uploader: up, page: 0, hashes }, 0), Message::PutOk { which: 0 });
        let mut forged = chunks[1].clone();
        forged[0] ^= 1;
        assert_eq!(
            ask(&mut h, Message::PutChunk { file: id, uploader: up, index: 1, data: forged }, 0),
            Message::Refused(Refusal::BadData)
        );
        for (i, c) in chunks.iter().enumerate() {
            ask(&mut h, Message::PutChunk { file: id, uploader: up, index: i as u32, data: c.clone() }, 0);
        }
        assert!(h.store.meta(&id).unwrap().complete());
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 0), Message::InfoReply { size: 2500, chunks: 3 });

        let big = Message::PutInfo { file: [7; 32], size: 9_000, uploader: up, read_token: [0; 32], code: vec![] };
        assert_eq!(ask(&mut h, big, 0), Message::Refused(Refusal::OverQuota));
    }

    #[test]
    fn uploads_are_off_unless_switched_on_and_codes_are_checked() {
        let mut h = host(HostPolicy::default(), Arc::new(|_| false));
        let m = Message::PutInfo { file: [1; 32], size: 10, uploader: [0; 32], read_token: [0; 32], code: vec![] };
        assert_eq!(ask(&mut h, m.clone(), 0), Message::Refused(Refusal::Disabled));
        let mut h = host(
            HostPolicy { accept_uploads: true, upload_code: Some(b"guild".to_vec()), ..Default::default() },
            Arc::new(|_| false),
        );
        assert_eq!(ask(&mut h, m, 0), Message::Refused(Refusal::Forbidden));
    }

    #[test]
    fn purged_and_expired_files_say_so() {
        let mut h = host(HostPolicy { retention: Retention::UnopenedDays(1), ..Default::default() }, Arc::new(|_| false));
        let (id, token, _) = seed(&mut h, b"short-lived", Scope::Conversation);
        let (kept, kept_token, _) = seed(&mut h, b"opened", Scope::Conversation);
        ask(&mut h, Message::Chunk { file: kept, token: kept_token, index: 0 }, 10);
        assert_eq!(h.sweep(86_400 + 1), 1);
        assert_eq!(ask(&mut h, Message::Info { file: id, token }, 0), Message::Refused(Refusal::NoLongerShared));
        assert!(h.store.meta(&kept).is_some());
        assert!(h.store.purge(&kept));
        assert_eq!(ask(&mut h, Message::Info { file: kept, token: kept_token }, 0), Message::Refused(Refusal::NoLongerShared));
        assert_eq!(ask(&mut h, Message::Info { file: [3; 32], token }, 0), Message::Refused(Refusal::NotHere));
    }

    #[test]
    fn the_store_survives_a_restart() {
        let dir = temp_dir("restart");
        let mut h = FileHost::new(FileStore::open(&dir).unwrap(), HostPolicy::default(), Arc::new(|_| false));
        let (id, _, chunks) = seed(&mut h, &[8u8; 1500], Scope::Anyone);
        drop(h);
        let store = FileStore::open(&dir).unwrap();
        assert!(store.meta(&id).unwrap().complete());
        assert_eq!(store.read_chunk(&id, 1).unwrap(), chunks[1]);
    }
}
