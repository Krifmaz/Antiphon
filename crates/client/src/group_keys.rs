//! Keys for a group call: MLS over the call's own links, and SFrame on
//! its audio (spec §3 "Groups -- MLS (RFC 9420)" and "Media through a
//! relay -- SFrame").
//!
//! In today's mesh every frame already travels inside a 1:1 Double
//! Ratchet session per link, so SFrame here adds one thing: audio is
//! sealed **once**, under a key held by exactly the current members, the
//! shape a relay forwarding to many (an SFU) needs, and a membership
//! change -- someone joining or leaving -- moves everyone to a new epoch
//! whose key the leaver never gets.
//!
//! # How the group agrees, with no server
//!
//! MLS needs commits applied in one order. One member commits: the one
//! at the **lowest MLS leaf index among the members this machine is
//! connected to** (counting itself). Everyone computes the same answer
//! from the same tree, so there is never a vote, and when the committer
//! leaves the next one takes over by the same rule.
//!
//! - When a 1:1 call becomes a group, whoever adds the third person
//!   creates the MLS group and so starts as the committer.
//! - Anyone in the call but not in the MLS group sends a KeyPackage over
//!   each of its links every couple of seconds until a Welcome arrives.
//!   Only the committer acts on one, and only if the KeyPackage is for the
//!   identity at the other end of the link it came over -- a member can't
//!   smuggle in someone else's key.
//! - The committer adds them, sends them the Welcome, and sends the Commit
//!   to every other member it is linked to. Everyone moves to the new
//!   epoch's SFrame key.
//! - A member who left (said `GroupLeave`, so its link is gone) is removed
//!   by the committer within a second, which is a new epoch too.
//! - A member that misses a commit (UDP loses it) notices -- the next
//!   commit fails, or SFrame frames from members keep failing -- drops its
//!   group and rejoins with a fresh KeyPackage; the committer swaps its old
//!   leaf out.
//!
//! Audio from someone in our MLS group must arrive SFrame-sealed; plain
//! frames are accepted only from people not (yet) in it, so the first
//! second of a newcomer is still heard.
//!
//! Not handled: two members adding people at the same instant as a 1:1
//! call first becomes a group would create two MLS groups; one side's
//! members then never receive a Welcome they can use and their audio stays
//! on the per-link encryption only.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use antiphon_crypto::group::{GroupMember, Processed};
use antiphon_crypto::sframe::SframeContext;
use antiphon_crypto::IdentityKeyPair;

use crate::control::{ControlMessage, MlsPart};
use crate::file_transfer::{decode_b64, encode_b64};
use crate::group::Link;

/// First byte of an SFrame-sealed audio payload. `antiphon_net::fec`
/// frames start with their version byte, 1, so the two never collide.
pub const SFRAME_AUDIO_TAG: u8 = 0xA5;

/// Bytes of MLS message per control datagram, before base64: well under
/// the 4 KiB datagram once it is base64 and JSON.
const PART_BYTES: usize = 1800;
const MAX_PARTS: u16 = 32;
const MAX_PENDING_MESSAGES: usize = 64;
const KEY_PACKAGE_EVERY: Duration = Duration::from_secs(2);
const UPKEEP_EVERY: Duration = Duration::from_millis(500);
/// How long frames from the previous epoch are still opened after a
/// change, for audio sealed just before it.
const OLD_EPOCH_GRACE: Duration = Duration::from_secs(3);
/// Consecutive SFrame frames from members that fail to open before we
/// decide we missed a commit and rejoin. 100 frames is two seconds.
const DESYNC_FRAMES: u32 = 100;

/// Who is at the other end of each link we have, by identity key.
pub type Linked = [(Link, [u8; 32])];

struct Mls {
    member: GroupMember,
    me: [u8; 32],
    pending_key_package: Option<Vec<u8>>,
    last_key_package: Option<Instant>,
    last_upkeep: Option<Instant>,
    epoch_changed: Option<Instant>,
    parts: HashMap<(Link, u64), Vec<Option<Vec<u8>>>>,
}

/// The group call's MLS member and SFrame keys.
#[derive(Default)]
pub struct GroupKeys {
    mls: Mutex<Option<Mls>>,
    sframe: Mutex<Option<SframeContext>>,
    /// Members' leaf indices, by identity key, for SFrame's sender check.
    index: Mutex<HashMap<[u8; 32], u32>>,
    desync: AtomicU32,
    last_poll: Mutex<Option<Instant>>,
    /// Held while sending MLS messages, so commits go out in the order
    /// they were made.
    pub send_serial: tokio::sync::Mutex<()>,
}

#[derive(Clone, Copy)]
enum Kind {
    KeyPackage,
    Welcome,
    Commit,
}

fn split(kind: Kind, bytes: &[u8]) -> Vec<ControlMessage> {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).expect("OS randomness is unavailable");
    let msg = u64::from_le_bytes(id);
    let chunks: Vec<&[u8]> = bytes.chunks(PART_BYTES).collect();
    let parts = chunks.len() as u16;
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let p = MlsPart { msg, part: i as u16, parts, data: encode_b64(c) };
            match kind {
                Kind::KeyPackage => ControlMessage::MlsKeyPackage(p),
                Kind::Welcome => ControlMessage::MlsWelcome(p),
                Kind::Commit => ControlMessage::MlsCommit(p),
            }
        })
        .collect()
}

pub fn is_mls_message(m: &ControlMessage) -> bool {
    matches!(m, ControlMessage::MlsKeyPackage(_) | ControlMessage::MlsWelcome(_) | ControlMessage::MlsCommit(_))
}

type Out = Vec<(Link, ControlMessage)>;

fn to(link: Link, kind: Kind, bytes: &[u8], out: &mut Out) {
    out.extend(split(kind, bytes).into_iter().map(|m| (link, m)));
}

impl GroupKeys {
    /// Whether periodic group work is due -- at most every quarter second,
    /// so an idle group call doesn't build a full context 20 times a
    /// second for nothing.
    pub fn poll_due(&self) -> bool {
        let mut last = self.last_poll.lock().unwrap();
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(250)) {
            return false;
        }
        *last = Some(now);
        true
    }

    /// Sets up the MLS member for this identity, once.
    pub fn ensure(&self, identity: &IdentityKeyPair) {
        let mut mls = self.mls.lock().unwrap();
        if mls.is_none() {
            *mls = Some(Mls {
                member: GroupMember::new(identity),
                me: identity.public().identity_key,
                pending_key_package: None,
                last_key_package: None,
                last_upkeep: None,
                epoch_changed: None,
                parts: HashMap::new(),
            });
        }
    }

    /// Creates the MLS group, making us its first committer. For the
    /// moment a 1:1 call becomes a group, by whoever adds the third person.
    pub fn start_group(&self) {
        let mut guard = self.mls.lock().unwrap();
        let Some(mls) = guard.as_mut() else { return };
        if !mls.member.in_group() && mls.member.create().is_ok() {
            self.refresh(mls);
        }
    }

    pub fn in_group(&self) -> bool {
        self.mls.lock().unwrap().as_ref().is_some_and(|m| m.member.in_group())
    }

    /// The MLS epoch we're in, if any.
    pub fn epoch(&self) -> Option<u64> {
        self.mls.lock().unwrap().as_ref().and_then(|m| m.member.epoch().ok())
    }

    #[cfg(test)]
    /// Identity keys of everyone in our MLS group, us included.
    pub fn members(&self) -> Vec<[u8; 32]> {
        let mut keys: Vec<_> = self.index.lock().unwrap().keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// After every epoch change: new SFrame key, new index map.
    fn refresh(&self, mls: &mut Mls) {
        let members = mls.member.members().unwrap_or_default();
        *self.index.lock().unwrap() = members.into_iter().map(|(i, k)| (k, i)).collect();
        let mut sframe = self.sframe.lock().unwrap();
        match sframe.as_mut() {
            Some(ctx) => {
                let _ = mls.member.update_sframe(ctx);
            }
            None => *sframe = mls.member.sframe_context().ok(),
        }
        self.desync.store(0, Ordering::Relaxed);
        mls.epoch_changed = Some(Instant::now());
        mls.pending_key_package = None;
    }

    /// Forgets the group, to rejoin it with a fresh KeyPackage.
    fn drop_group(&self, mls: &mut Mls) {
        mls.member.reset();
        mls.pending_key_package = None;
        mls.last_key_package = None;
        *self.sframe.lock().unwrap() = None;
        self.index.lock().unwrap().clear();
        self.desync.store(0, Ordering::Relaxed);
    }

    fn is_committer(mls: &Mls, linked: &Linked) -> bool {
        let Ok(own) = mls.member.own_index() else { return false };
        let members = mls.member.members().unwrap_or_default();
        members
            .iter()
            .filter(|(_, k)| *k == mls.me || linked.iter().any(|(_, l)| l == k))
            .map(|(i, _)| *i)
            .min()
            == Some(own)
    }

    /// Sends `commit` to every linked member except `skip`.
    fn broadcast_commit(mls: &Mls, commit: &[u8], skip: &[[u8; 32]], linked: &Linked, out: &mut Out) {
        let members = mls.member.members().unwrap_or_default();
        for (link, key) in linked {
            if !skip.contains(key) && members.iter().any(|(_, k)| k == key) {
                to(*link, Kind::Commit, commit, out);
            }
        }
    }

    /// Acts on one MLS control message from `link`, whose far end is
    /// `from`. Returns what to send.
    pub fn handle(&self, link: Link, from: [u8; 32], message: ControlMessage, linked: &Linked) -> Out {
        let mut out = Vec::new();
        let mut guard = self.mls.lock().unwrap();
        let Some(mls) = guard.as_mut() else { return out };
        let (kind, part) = match message {
            ControlMessage::MlsKeyPackage(p) => (Kind::KeyPackage, p),
            ControlMessage::MlsWelcome(p) => (Kind::Welcome, p),
            ControlMessage::MlsCommit(p) => (Kind::Commit, p),
            _ => return out,
        };
        let Some(bytes) = Self::reassemble(mls, link, part) else { return out };

        match kind {
            Kind::KeyPackage => {
                if !Self::is_committer(mls, linked) || mls.member.key_package_identity(&bytes).ok() != Some(from) {
                    return out;
                }
                // Already a member: they lost track and are rejoining,
                // so their old leaf goes first.
                if mls.member.members().unwrap_or_default().iter().any(|(_, k)| *k == from) {
                    match mls.member.remove(&[from]) {
                        Ok(c) => Self::broadcast_commit(mls, &c.commit, &[from], linked, &mut out),
                        Err(_) => return out,
                    }
                }
                if let Ok(c) = mls.member.add(&[&bytes]) {
                    Self::broadcast_commit(mls, &c.commit, &[from], linked, &mut out);
                    if let Some(w) = &c.welcome {
                        to(link, Kind::Welcome, w, &mut out);
                    }
                }
                self.refresh(mls);
            }
            Kind::Welcome => {
                if mls.member.in_group() {
                    return out;
                }
                if mls.member.join(&bytes).is_ok() {
                    // The Welcome has to come from someone in the group it
                    // invites us to.
                    if mls.member.members().unwrap_or_default().iter().any(|(_, k)| *k == from) {
                        self.refresh(mls);
                    } else {
                        self.drop_group(mls);
                    }
                }
            }
            Kind::Commit => {
                if !mls.member.in_group() {
                    return out;
                }
                match mls.member.process(&bytes) {
                    Ok(Processed::NewEpoch { removed_self: true, .. }) => self.drop_group(mls),
                    Ok(Processed::NewEpoch { .. }) => self.refresh(mls),
                    Ok(_) => {}
                    // A commit we can't apply means we missed one before
                    // it: start over rather than stay on a dead epoch.
                    Err(_) => self.drop_group(mls),
                }
            }
        }
        out
    }

    fn reassemble(mls: &mut Mls, link: Link, part: MlsPart) -> Option<Vec<u8>> {
        if part.parts == 0 || part.parts > MAX_PARTS || part.part >= part.parts {
            return None;
        }
        let data = decode_b64(&part.data)?;
        if part.parts == 1 {
            return Some(data);
        }
        if mls.parts.len() >= MAX_PENDING_MESSAGES {
            mls.parts.clear();
        }
        let slots = mls.parts.entry((link, part.msg)).or_insert_with(|| vec![None; part.parts as usize]);
        if slots.len() != part.parts as usize {
            return None;
        }
        slots[part.part as usize] = Some(data);
        if slots.iter().all(Option::is_some) {
            let slots = mls.parts.remove(&(link, part.msg))?;
            return Some(slots.into_iter().flatten().flatten().collect());
        }
        None
    }

    /// Periodic work: offer a KeyPackage while outside the group, remove
    /// members who left while committer, and retire the previous epoch's
    /// key once its frames have drained. `people` counts everyone in the
    /// call, us included.
    pub fn upkeep(&self, people: usize, linked: &Linked) -> Out {
        let mut out = Vec::new();
        let mut guard = self.mls.lock().unwrap();
        let Some(mls) = guard.as_mut() else { return out };
        let now = Instant::now();
        if mls.last_upkeep.is_some_and(|t| now.duration_since(t) < UPKEEP_EVERY) {
            return out;
        }
        mls.last_upkeep = Some(now);

        if !mls.member.in_group() {
            let due = mls.last_key_package.is_none_or(|t| now.duration_since(t) >= KEY_PACKAGE_EVERY);
            if people >= 3 && due {
                if mls.pending_key_package.is_none() {
                    mls.pending_key_package = mls.member.key_package().ok();
                }
                if let Some(kp) = mls.pending_key_package.clone() {
                    for (link, _) in linked {
                        to(*link, Kind::KeyPackage, &kp, &mut out);
                    }
                    mls.last_key_package = Some(now);
                }
            }
            return out;
        }

        if self.desync.load(Ordering::Relaxed) >= DESYNC_FRAMES {
            self.drop_group(mls);
            return out;
        }

        if Self::is_committer(mls, linked) {
            let gone: Vec<[u8; 32]> = mls
                .member
                .members()
                .unwrap_or_default()
                .into_iter()
                .map(|(_, k)| k)
                .filter(|k| *k != mls.me && !linked.iter().any(|(_, l)| l == k))
                .collect();
            if !gone.is_empty() {
                if let Ok(c) = mls.member.remove(&gone) {
                    Self::broadcast_commit(mls, &c.commit, &[], linked, &mut out);
                    self.refresh(mls);
                }
            }
        }

        if mls.epoch_changed.is_some_and(|t| now.duration_since(t) >= OLD_EPOCH_GRACE) {
            mls.epoch_changed = None;
            if let (Ok(epoch), Some(ctx)) = (mls.member.epoch(), self.sframe.lock().unwrap().as_mut()) {
                ctx.retain_only(epoch);
            }
        }
        out
    }

    /// Seals one audio payload under our SFrame key, once for everyone,
    /// or `None` while we have no key.
    pub fn seal_audio(&self, payload: &[u8]) -> Option<Vec<u8>> {
        let mut sframe = self.sframe.lock().unwrap();
        let ctx = sframe.as_mut()?;
        let mut out = Vec::with_capacity(payload.len() + 32);
        out.push(SFRAME_AUDIO_TAG);
        out.extend_from_slice(&ctx.seal(payload));
        Some(out)
    }

    /// Which of the two to send `to`: the sealed frame if they're in our
    /// MLS group and so can open it, the plain one otherwise.
    pub fn frame_for<'a>(&self, to: &[u8; 32], plain: &'a [u8], sealed: Option<&'a [u8]>) -> &'a [u8] {
        match sealed {
            Some(s) if self.index.lock().unwrap().contains_key(to) => s,
            _ => plain,
        }
    }

    /// Opens an audio payload from `from`, or `None` to drop it.
    pub fn open_audio(&self, from: &[u8; 32], payload: &[u8]) -> Option<Vec<u8>> {
        if payload.first() == Some(&SFRAME_AUDIO_TAG) {
            let index = *self.index.lock().unwrap().get(from)?;
            let opened = self.sframe.lock().unwrap().as_mut()?.open(index, &payload[1..]);
            return match opened {
                Ok(p) => {
                    self.desync.store(0, Ordering::Relaxed);
                    Some(p)
                }
                Err(_) => {
                    self.desync.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
        }
        // Plain audio from a member of our group would be a downgrade;
        // from anyone else it's someone not keyed in yet.
        if self.sframe.lock().unwrap().is_some() && self.index.lock().unwrap().contains_key(from) {
            return None;
        }
        Some(payload.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn big_messages_split_and_reassemble_in_any_order() {
        let id = IdentityKeyPair::generate();
        let keys = GroupKeys::default();
        keys.ensure(&id);
        let big: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let mut parts = split(Kind::Commit, &big);
        assert_eq!(parts.len(), 3);
        parts.reverse();
        for m in &parts {
            assert!(m.encode().len() < 3000);
        }
        let mut guard = keys.mls.lock().unwrap();
        let mls = guard.as_mut().unwrap();
        let mut got = None;
        for m in parts {
            let ControlMessage::MlsCommit(p) = m else { panic!() };
            got = Mls::reassemble_for_test(mls, p).or(got);
        }
        assert_eq!(got.unwrap(), big);
    }

    impl Mls {
        fn reassemble_for_test(&mut self, p: MlsPart) -> Option<Vec<u8>> {
            GroupKeys::reassemble(self, Link::Primary, p)
        }
    }

    #[test]
    fn plain_audio_from_a_member_is_refused_once_keyed() {
        let id = IdentityKeyPair::generate();
        let keys = GroupKeys::default();
        keys.ensure(&id);
        let me = id.public().identity_key;
        let stranger = [9u8; 32];
        assert_eq!(keys.open_audio(&stranger, &[1, 2, 3]).unwrap(), vec![1, 2, 3]);
        keys.start_group();
        assert!(keys.in_group());
        // We are the only member; a frame claiming to be from us in the
        // clear is refused, a non-member's is not.
        assert!(keys.open_audio(&me, &[1, 2, 3]).is_none());
        assert!(keys.open_audio(&stranger, &[1, 2, 3]).is_some());
        let sealed = keys.seal_audio(&[1, 2, 3]).unwrap();
        assert_eq!(sealed[0], SFRAME_AUDIO_TAG);
        assert_eq!(keys.frame_for(&stranger, &[1, 2, 3], Some(&sealed)), &[1, 2, 3]);
        assert_eq!(keys.frame_for(&me, &[1, 2, 3], Some(&sealed)), &sealed[..]);
    }
}
