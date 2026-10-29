//! Group calls past the mesh (spec §3: "Beyond that -- SFU relay. Each
//! person uploads once; the relay forwards encrypted frames to everyone.
//! The relay holds no keys and cannot hear anything.").
//!
//! A mesh call stays a mesh up to [`crate::group::MAX_PEOPLE`]. When the
//! person adding a sixth finds the call full, they move it: they make a
//! fresh room secret, join a room on their relay's SFU (`antiphon-relay
//! --sfu`), and hand the SFU's address, the secret and the day to everyone
//! over the pairwise connections the call already has -- end to end
//! encrypted and authenticated, so the SFU never sees any of it. Each
//! member then joins the same room and sends their audio there once,
//! instead of once per person.
//!
//! Inside the room:
//!
//! - **Audio is SFrame**, under the call's MLS keys (`crate::group_keys`):
//!   the same epoch key the mesh uses, so a member removed from the MLS
//!   group can't read what follows. A frame's KID names the sender's MLS
//!   leaf, and it is only opened if that leaf belongs to the identity that
//!   announced the room tag it came under. Until someone is keyed in, they
//!   are silent in the room rather than sending anything in the clear.
//! - **The room is a link for MLS.** People past the mesh are connected
//!   to nobody but the person who added them, so KeyPackages, Welcomes and
//!   Commits travel through the room too, addressed to one member and
//!   sealed under the room secret. That keeps the "lowest linked leaf
//!   commits" rule consistent for everyone in the room.
//! - **Announcements** say which identity is behind a room tag and its
//!   name, signed by that identity and sealed under the room secret, so
//!   only members can read them and nobody can speak as someone else.
//!
//! What the SFU sees: a room token that changes daily, a random tag per
//! member per call, and sealed bytes. Not a name, a key or a word.
//!
//! The room secret only hides who is in the room from the SFU and from
//! strangers; it is not what keeps the audio private. MLS is.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use antiphon_crypto::IdentityKeyPair;
use antiphon_net::media_seal::MediaSealer;
use antiphon_net::sfu::{new_member_tag, room_token, MemberTag, SfuClient, SfuEvent};
use antiphon_net::JitterBuffer;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

use crate::control::ControlMessage;
use crate::group::{BlockCheck, Group, Link};
use crate::group_keys::SFRAME_AUDIO_TAG;

/// Most people in an SFU call, matching the relay's default room size.
pub const MAX_PEOPLE: usize = 50;

/// Mixer source ids for SFU members start here, clear of mesh legs.
pub const SOURCE_BASE: u64 = 1 << 32;

/// Room payload kind: an SFrame-sealed audio frame.
const KIND_MEDIA: u8 = 0;
/// Room payload kind: a signed announcement of who is behind a tag.
const KIND_ANNOUNCE: u8 = 1;
/// Room payload kind: an MLS control message for one member.
const KIND_CONTROL: u8 = 2;
/// Room payload kind: how much of whose audio this member lost (spec §2
/// automatic bandwidth), sealed under the room secret.
const KIND_REPORT: u8 = 3;
/// Room payload kind: an SFrame-sealed video datagram.
const KIND_VIDEO: u8 = 4;
/// How often a member reports loss to the room. Only senders it lost
/// audio from are listed, and nothing goes out when there are none, so a
/// healthy room carries no reports at all.
const REPORT_EVERY: Duration = Duration::from_secs(2);
/// Domain separation for loss reports.
const REPORT_CONTEXT: &[u8] = b"antiphon/sfu/report/v1";
/// How often this member re-announces, so late joiners learn who it is.
const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);
/// How long joining the SFU room may take.
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Most a member's jitter buffer waits for a late frame.
const JITTER_MAX_WAIT: Duration = Duration::from_millis(80);
/// Domain separation for announcements and the room key, versioned so a
/// changed announcement layout cannot be mistaken for an older one.
const ANNOUNCE_CONTEXT: &[u8] = b"antiphon/sfu/announce/v2";
/// Domain separation for control messages, so neither kind can pass for
/// the other.
const CONTROL_CONTEXT: &[u8] = b"antiphon/sfu/control/v1";

/// Where and how to join, as handed from member to member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    /// The SFU's `host:port`.
    pub sfu: String,
    /// The room secret: shared only over the call's end-to-end links, never
    /// with the SFU.
    pub secret: [u8; 32],
    /// The day number the room token is derived for, picked by the inviter.
    pub day: u64,
}

impl Invite {
    /// A new invite with a fresh random room secret.
    pub fn fresh(sfu: String, day: u64) -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS randomness is unavailable");
        Self { sfu, secret, day }
    }
}

/// Someone heard in the room, after their announcement checked out.
struct Member {
    /// This machine's local handle for the member, for links and mixer ids.
    id: u64,
    /// The identity that signed the member's announcement.
    identity_key: [u8; 32],
    /// The announced display name, at most 64 characters.
    name: String,
    /// Where the member's audio waits to be mixed.
    jitter: Arc<Mutex<JitterBuffer>>,
    /// The member's voice decoder: Opus keeps state between packets, so
    /// one per person.
    voice: antiphon_net::voice::VoiceReceiver,
}

/// Room members by tag, shared by the receiver task, the sealer and the call.
type Members = Arc<Mutex<HashMap<MemberTag, Member>>>;

/// SFrame through the room, as a [`MediaSealer`]: the call's MLS keys,
/// with each room tag tied to the identity that announced it.
pub struct RoomSealer {
    /// The call's group, for its MLS keys; weak so the room does not keep a
    /// finished call alive.
    group: Weak<Group>,
    /// Who is behind each tag, so a frame opens only as its announcer's.
    members: Members,
}

impl MediaSealer for RoomSealer {
    fn seal(&mut self, plain: &[u8]) -> Option<Vec<u8>> {
        self.group.upgrade()?.keys.seal_audio(plain)
    }

    fn open(&mut self, sender: MemberTag, sealed: &[u8]) -> Option<Vec<u8>> {
        // Only SFrame: the room never carries audio in the clear.
        if sealed.first() != Some(&SFRAME_AUDIO_TAG) {
            return None;
        }
        let identity = self.members.lock().unwrap().get(&sender)?.identity_key;
        self.group.upgrade()?.keys.open_audio(&identity, sealed)
    }
}

/// This machine's place in an SFU call.
pub struct SfuCall {
    /// Where the call is, to hand on to anyone added later.
    pub invite: Invite,
    /// This member's connection to the SFU room.
    client: Arc<SfuClient>,
    /// Seals this machine's audio for the room.
    sealer: Mutex<RoomSealer>,
    /// Seals announcements and control messages under the room secret.
    room_cipher: ChaCha20Poly1305,
    /// Everyone else heard in the room.
    members: Members,
    /// The person this machine's call started with, left out of the
    /// names list because the call screen already shows them.
    primary_key: [u8; 32],
    /// The announcer and receiver tasks, stopped when the call is dropped.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for SfuCall {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl SfuCall {
    /// Joins the room `invite` names and starts announcing and receiving.
    /// MLS messages from the room land in `group`'s inbox as
    /// [`Link::Room`].
    pub async fn join(
        invite: Invite,
        group: &Arc<Group>,
        identity: Arc<IdentityKeyPair>,
        name: String,
        primary_key: [u8; 32],
        is_blocked: BlockCheck,
    ) -> anyhow::Result<Arc<SfuCall>> {
        let addr: SocketAddr = crate::state::resolve(&invite.sfu).await?;
        // Spec §3 anonymity mode: the SFU sees the circuit's last hop.
        let (socket, addr) = match crate::anonymity::configured_hops() {
            Some(hops) => crate::anonymity::route_to(&hops, addr).await?,
            None => (Arc::new(crate::vpn::bind(antiphon_net::vpn::TrafficKind::Voice, 0).await?), addr),
        };
        let me = new_member_tag();
        let room = room_token(&invite.secret, invite.day);
        let (client, mut events) = SfuClient::join(socket, addr, room, me, JOIN_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("the group relay at {} said no: {e}", invite.sfu))?;
        let client = Arc::new(client);
        let members: Members = Arc::default();
        let room_cipher = room_cipher(&invite.secret);
        let announcement =
            Arc::new(seal(&room_cipher, KIND_ANNOUNCE, ANNOUNCE_CONTEXT, &announce_body(&identity, &room, &me, &name)));

        let announcer = {
            let (client, announcement) = (client.clone(), announcement.clone());
            tokio::spawn(async move {
                loop {
                    let _ = client.send_media(&announcement).await;
                    tokio::time::sleep(ANNOUNCE_EVERY).await;
                }
            })
        };

        let receiver = {
            let (client, members, cipher) = (client.clone(), members.clone(), room_cipher.clone());
            let group = Arc::downgrade(group);
            let mut sealer = RoomSealer { group: group.clone(), members: members.clone() };
            let next_id = AtomicU64::new(0);
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        SfuEvent::Media { from, payload } => match payload.split_first() {
                            Some((&KIND_MEDIA, sealed)) => {
                                let Some(plain) = sealer.open(from, sealed) else { continue };
                                if let Some(m) = members.lock().unwrap().get_mut(&from) {
                                    let Some(decoded) = m.voice.decode(&plain) else { continue };
                                    m.jitter.lock().unwrap().push_with_backup(
                                        decoded.sequence as u64,
                                        decoded.primary,
                                        decoded.backup,
                                    );
                                }
                            }
                            Some((&KIND_ANNOUNCE, sealed)) => {
                                let Some(body) = open(&cipher, ANNOUNCE_CONTEXT, sealed) else { continue };
                                let Some(a) = parse_announce(&body, &room, &from) else { continue };
                                if is_blocked(&a.identity_key) {
                                    continue;
                                }
                                let mut members = members.lock().unwrap();
                                match members.get_mut(&from) {
                                    // A tag can't change hands mid-call.
                                    Some(m) if m.identity_key != a.identity_key => {}
                                    Some(m) => m.name = a.name,
                                    None => {
                                        members.insert(
                                            from,
                                            Member {
                                                id: next_id.fetch_add(1, Ordering::Relaxed),
                                                identity_key: a.identity_key,
                                                name: a.name,
                                                jitter: Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT))),
                                                voice: antiphon_net::voice::VoiceReceiver::new(
                                                    group.upgrade().map_or(48_000, |g| g.device_rate()),
                                                ),
                                            },
                                        );
                                        // A newcomer needs a whole picture, not
                                        // patches against one they never saw.
                                        if let Some(video) = group.upgrade().and_then(|g| g.video.lock().unwrap().clone()) {
                                            video.request_keyframe();
                                        }
                                        // Say hello back at once, so a newcomer
                                        // doesn't wait out the timer to hear us.
                                        let (client, announcement) = (client.clone(), announcement.clone());
                                        tokio::spawn(async move {
                                            let _ = client.send_media(&announcement).await;
                                        });
                                    }
                                }
                            }
                            Some((&KIND_VIDEO, sealed)) => {
                                let Some(plain) = sealer.open(from, sealed) else { continue };
                                let Some(id) = members.lock().unwrap().get(&from).map(|m| m.id) else { continue };
                                if let Some(group) = group.upgrade() {
                                    if let Some(video) = group.video.lock().unwrap().as_ref() {
                                        video.receive_from(SOURCE_BASE + id, &plain);
                                    }
                                }
                            }
                            Some((&KIND_REPORT, sealed)) => {
                                // Entries are `[tag: 16][loss: u8]`; only
                                // the one about us matters here.
                                let Some(body) = open(&cipher, REPORT_CONTEXT, sealed) else { continue };
                                let Some(loss) = body.chunks_exact(17).find(|e| e[..16] == me[..]).map(|e| e[16]) else {
                                    continue;
                                };
                                let Some(id) = members.lock().unwrap().get(&from).map(|m| m.id) else { continue };
                                if let Some(group) = group.upgrade() {
                                    group.link.on_loss(SOURCE_BASE + id, loss);
                                }
                            }
                            Some((&KIND_CONTROL, sealed)) => {
                                let Some(body) = open(&cipher, CONTROL_CONTEXT, sealed) else { continue };
                                let Some((to, json)) = body.split_at_checked(16) else { continue };
                                if to != me {
                                    continue;
                                }
                                let Some(id) = members.lock().unwrap().get(&from).map(|m| m.id) else { continue };
                                // Only MLS travels through the room; calls
                                // and introductions stay on real links.
                                let Some(m) = ControlMessage::decode(json).filter(crate::group_keys::is_mls_message)
                                else {
                                    continue;
                                };
                                if let Some(group) = group.upgrade() {
                                    group.inbox.lock().unwrap().push((Link::Room(id), m));
                                }
                            }
                            _ => {}
                        },
                        SfuEvent::Roster(present) => {
                            members.lock().unwrap().retain(|t, _| present.contains(t));
                        }
                        SfuEvent::Refused(_) => {}
                    }
                }
            })
        };

        // Spec §2 automatic bandwidth in the room: who we lost audio from,
        // so their senders can step down.
        let reporter = {
            let (client, members, cipher) = (client.clone(), members.clone(), room_cipher.clone());
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(REPORT_EVERY).await;
                    let body: Vec<u8> = members
                        .lock()
                        .unwrap()
                        .iter()
                        .filter_map(|(tag, m)| m.voice.loss().take_percent().filter(|l| *l > 0).map(|l| (tag, l)))
                        .flat_map(|(tag, loss)| tag.iter().copied().chain([loss]))
                        .collect();
                    if !body.is_empty() {
                        let _ = client.send_media(&seal(&cipher, KIND_REPORT, REPORT_CONTEXT, &body)).await;
                    }
                }
            })
        };

        // Our video into the room: the call's video hands datagrams over a
        // channel, sealed here with SFrame like the audio and sent once.
        let video_sender = {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
            if let Some(video) = group.video.lock().unwrap().as_ref() {
                video.set_room(Some(tx));
            }
            let client = client.clone();
            let mut sealer = RoomSealer { group: Arc::downgrade(group), members: members.clone() };
            tokio::spawn(async move {
                while let Some(datagram) = rx.recv().await {
                    let Some(sealed) = sealer.seal(&datagram) else { continue };
                    let _ = client.send_media(&[&[KIND_VIDEO][..], &sealed].concat()).await;
                }
            })
        };

        Ok(Arc::new(SfuCall {
            invite,
            client,
            sealer: Mutex::new(RoomSealer { group: Arc::downgrade(group), members: members.clone() }),
            room_cipher,
            members,
            primary_key,
            tasks: vec![announcer, receiver, reporter, video_sender],
        }))
    }

    /// Seals one encoded audio frame with SFrame and sends it once, for
    /// everyone. Nothing goes out until MLS has keyed us in.
    pub async fn send_audio(&self, frame: &[u8]) {
        let Some(sealed) = self.sealer.lock().unwrap().seal(frame) else { return };
        let mut out = Vec::with_capacity(sealed.len() + 1);
        out.push(KIND_MEDIA);
        out.extend(sealed);
        let _ = self.client.send_media(&out).await;
    }

    /// Sends a control message to the room member behind `id`.
    pub async fn send_control(&self, id: u64, message: &ControlMessage) {
        let Some(tag) = self.members.lock().unwrap().iter().find(|(_, m)| m.id == id).map(|(t, _)| *t) else {
            return;
        };
        let body = [&tag[..], &message.encode()].concat();
        let _ = self.client.send_media(&seal(&self.room_cipher, KIND_CONTROL, CONTROL_CONTEXT, &body)).await;
    }

    /// Everyone in the room, this machine included.
    pub fn people(&self) -> usize {
        self.members.lock().unwrap().len() + 1
    }

    /// Each member's mixer source id and jitter buffer.
    pub fn audio(&self) -> Vec<(u64, Arc<Mutex<JitterBuffer>>)> {
        self.members.lock().unwrap().values().map(|m| (SOURCE_BASE + m.id, m.jitter.clone())).collect()
    }

    /// Room members as links, for MLS, by identity key.
    pub fn links(&self) -> Vec<(Link, [u8; 32])> {
        self.members.lock().unwrap().values().map(|m| (Link::Room(m.id), m.identity_key)).collect()
    }

    /// The identity behind room member `id`.
    pub fn identity_of(&self, id: u64) -> Option<[u8; 32]> {
        self.members.lock().unwrap().values().find(|m| m.id == id).map(|m| m.identity_key)
    }

    /// Names of members not already shown: not the primary, not `skip`.
    pub fn names(&self, skip: &[[u8; 32]]) -> Vec<String> {
        let mut names: Vec<String> = self
            .members
            .lock()
            .unwrap()
            .values()
            .filter(|m| m.identity_key != self.primary_key && !skip.contains(&m.identity_key))
            .map(|m| {
                let name = if m.name.is_empty() { antiphon_crypto::fingerprint(&m.identity_key) } else { m.name.clone() };
                format!("{name} (group relay)")
            })
            .collect();
        names.sort();
        names
    }

    /// Room member `id`'s display name (`id` as in [`SfuCall::audio`],
    /// counted from [`SOURCE_BASE`]), for their video tile.
    pub fn name_of(&self, id: u64) -> Option<String> {
        let members = self.members.lock().unwrap();
        let m = members.values().find(|m| SOURCE_BASE + m.id == id)?;
        Some(if m.name.is_empty() { antiphon_crypto::fingerprint(&m.identity_key) } else { m.name.clone() })
    }

    /// The identity keys of everyone else in the room.
    pub fn identity_keys(&self) -> Vec<[u8; 32]> {
        self.members.lock().unwrap().values().map(|m| m.identity_key).collect()
    }
}

/// The room key for announcements and control messages, derived from the
/// room secret so only members can read them.
fn room_cipher(secret: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(ANNOUNCE_CONTEXT), secret)
        .expand(b"room key", &mut key)
        .expect("32 bytes is a valid HKDF length");
    ChaCha20Poly1305::new(&Key::from(key))
}

/// `[identity key 32][signature 64][name...]`, signed over the room and
/// tag too so an announcement can't be replayed elsewhere.
fn announce_body(identity: &IdentityKeyPair, room: &[u8; 32], tag: &MemberTag, name: &str) -> Vec<u8> {
    let identity_key = identity.public().identity_key;
    let signature = identity.sign(&signed_part(room, tag, &identity_key, name.as_bytes()));
    [&identity_key[..], &signature, name.as_bytes()].concat()
}

/// What an announcement signs: context, room, tag, identity and name.
fn signed_part(room: &[u8; 32], tag: &MemberTag, identity_key: &[u8; 32], name: &[u8]) -> Vec<u8> {
    [ANNOUNCE_CONTEXT, room, tag, identity_key, name].concat()
}

/// A checked announcement: who is behind a tag.
struct Announcement {
    /// The identity whose signature checked out.
    identity_key: [u8; 32],
    /// Their name, lossily decoded and cut to 64 characters.
    name: String,
}

/// Checks an announcement's signature for this room and tag; `None` if it
/// does not verify.
fn parse_announce(body: &[u8], room: &[u8; 32], tag: &MemberTag) -> Option<Announcement> {
    if body.len() < 96 {
        return None;
    }
    let identity_key: [u8; 32] = body[..32].try_into().ok()?;
    let signature: [u8; 64] = body[32..96].try_into().ok()?;
    let name = &body[96..];
    antiphon_crypto::verify_signature(&identity_key, &signed_part(room, tag, &identity_key, name), &signature).ok()?;
    let name: String = String::from_utf8_lossy(name).chars().take(64).collect();
    Some(Announcement { identity_key, name })
}

/// `[kind][nonce 12][ciphertext]` under the room key, with `context` as
/// associated data. Random nonces, since many members share the key.
fn seal(cipher: &ChaCha20Poly1305, kind: u8, context: &[u8], body: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: body, aad: context })
        .expect("sealing never fails for in-memory data");
    [&[kind][..], &nonce, &sealed].concat()
}

/// Opens what [`seal`] made (without its kind byte); `None` on any failure.
fn open(cipher: &ChaCha20Poly1305, context: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
    let (nonce, ct) = sealed.split_at_checked(12)?;
    let nonce: [u8; 12] = nonce.try_into().ok()?;
    cipher.decrypt(&Nonce::from(nonce), Payload { msg: ct, aad: context }).ok()
}

/// The control-message form of an invite: hex secret, so it rides in JSON.
pub fn to_message(invite: &Invite) -> ControlMessage {
    ControlMessage::GroupSfu {
        sfu: invite.sfu.clone(),
        secret: crate::connection_mode::encode_hex32(&invite.secret),
        day: invite.day,
    }
}

/// An invite back from a [`ControlMessage::GroupSfu`]; `None` if the
/// secret is not 32 bytes of hex.
pub fn from_message(sfu: &str, secret: &str, day: u64) -> Option<Invite> {
    Some(Invite { sfu: sfu.to_string(), secret: crate::connection_mode::decode_hex32(secret)?, day })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_announcement_round_trips_and_binds_room_and_tag() {
        let me = IdentityKeyPair::generate();
        let (room, tag) = ([1u8; 32], [2u8; 16]);
        let cipher = room_cipher(&[9; 32]);
        let sealed = seal(&cipher, KIND_ANNOUNCE, ANNOUNCE_CONTEXT, &announce_body(&me, &room, &tag, "Ana"));
        let body = open(&cipher, ANNOUNCE_CONTEXT, &sealed[1..]).unwrap();
        let a = parse_announce(&body, &room, &tag).unwrap();
        assert_eq!((a.identity_key, a.name.as_str()), (me.public().identity_key, "Ana"));
        // Replayed under another tag or room, it no longer checks out.
        assert!(parse_announce(&body, &room, &[7u8; 16]).is_none());
        assert!(parse_announce(&body, &[8u8; 32], &tag).is_none());
        // An announcement can't be passed off as a control message.
        assert!(open(&cipher, CONTROL_CONTEXT, &sealed[1..]).is_none());
    }

    #[test]
    fn an_announcement_is_unreadable_without_the_room_secret() {
        let me = IdentityKeyPair::generate();
        let sealed = seal(&room_cipher(&[9; 32]), KIND_ANNOUNCE, ANNOUNCE_CONTEXT, &announce_body(&me, &[1; 32], &[2; 16], "Ana"));
        assert!(open(&room_cipher(&[4; 32]), ANNOUNCE_CONTEXT, &sealed[1..]).is_none());
    }

    #[test]
    fn invites_survive_the_control_message() {
        let invite = Invite::fresh("relay.example:3479".into(), 20_000);
        let ControlMessage::GroupSfu { sfu, secret, day } = to_message(&invite) else { panic!() };
        assert_eq!(from_message(&sfu, &secret, day), Some(invite));
    }

    /// One round of what `Context::poll` does, over the room alone: MLS
    /// in from the inbox, out through the room.
    async fn pump_mls(group: &Arc<Group>, sfu: &SfuCall) {
        let linked = sfu.links();
        let inbox: Vec<_> = std::mem::take(&mut *group.inbox.lock().unwrap());
        let mut out = Vec::new();
        for (link, m) in inbox {
            let Link::Room(id) = link else { continue };
            if let Some(from) = sfu.identity_of(id) {
                out.extend(group.keys.handle(link, from, m, &linked));
            }
        }
        out.extend(group.keys.upkeep(sfu.people(), &linked));
        for (link, m) in out {
            if let Link::Room(id) = link {
                sfu.send_control(id, &m).await;
            }
        }
    }

    #[tokio::test]
    async fn six_people_key_in_over_mls_and_hear_each_other_as_sframe() {
        let socket = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let relay = Arc::new(Mutex::new(antiphon_relay::Relay::default()));
        let sfu = Arc::new(Mutex::new(antiphon_relay::sfu::Sfu::new(Default::default())));
        let node = tokio::spawn(antiphon_relay::run_node_with_sfu(socket, relay, None, None, Some(sfu)));

        let invite = Invite::fresh(addr.to_string(), 1);
        let never: BlockCheck = Arc::new(|_| false);
        let mut people = Vec::new();
        for i in 0..6 {
            let id = Arc::new(IdentityKeyPair::generate());
            let group = Arc::new(Group::default());
            group.keys.ensure(&id);
            if i == 0 {
                group.keys.start_group();
            }
            let call = SfuCall::join(invite.clone(), &group, id, format!("P{i}"), [0; 32], never.clone()).await.unwrap();
            people.push((group, call));
        }
        // Announcements, then MLS through the room until everyone shares
        // one epoch.
        let mut keyed = false;
        for _ in 0..200 {
            for (g, c) in &people {
                pump_mls(g, c).await;
            }
            let epochs: Vec<_> = people.iter().map(|(g, _)| g.keys.epoch()).collect();
            if people.iter().all(|(_, c)| c.people() == 6) && epochs.iter().all(|e| e.is_some() && *e == epochs[0]) {
                keyed = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(keyed, "{:?}", people.iter().map(|(g, c)| (c.people(), g.keys.epoch())).collect::<Vec<_>>());

        let frame = antiphon_net::fec::encode(0, &[0.25f32; 960], None);
        people[3].1.send_audio(&frame).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        for (i, (_, c)) in people.iter().enumerate() {
            let heard: usize = c.audio().iter().map(|(_, j)| j.lock().unwrap().backlog_frames()).sum();
            assert_eq!(heard, usize::from(i != 3), "person {i}");
        }
        node.abort();
    }

    #[test]
    fn the_room_never_opens_audio_in_the_clear() {
        let group = Arc::new(Group::default());
        let members: Members = Arc::default();
        let key = IdentityKeyPair::generate().public().identity_key;
        members.lock().unwrap().insert(
            [1; 16],
            Member { id: 0, identity_key: key, name: String::new(), jitter: Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT))), voice: antiphon_net::voice::VoiceReceiver::new(48_000) },
        );
        let mut sealer = RoomSealer { group: Arc::downgrade(&group), members };
        let plain = antiphon_net::fec::encode(0, &[0.1f32; 960], None);
        assert!(sealer.open([1; 16], &plain).is_none());
        // ...and with no MLS keys yet, there is nothing to send.
        assert!(sealer.seal(&plain).is_none());
    }
}
