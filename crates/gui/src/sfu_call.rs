//! Group calls past the mesh (spec §3: "Beyond that -- SFU relay. Each
//! person uploads once; the relay forwards encrypted frames to everyone.
//! The relay holds no keys and cannot hear anything.").
//!
//! A mesh call stays a mesh up to [`crate::group::MAX_PEOPLE`]. When the
//! person adding a sixth finds the call full, they move it: they make a
//! fresh group secret, join a room on their relay's SFU (`antiphon-relay
//! --sfu`), and hand the SFU's address, the secret and the day to everyone
//! over the pairwise connections the call already has -- end to end
//! encrypted and authenticated, so the SFU never sees any of it. Each
//! member then joins the same room and sends their audio there once,
//! instead of once per person.
//!
//! Inside the room, every member:
//!
//! - makes a **sender key** of its own and seals each audio frame with it
//!   (`antiphon_net::media_seal`, the stopgap until MLS-derived SFrame keys
//!   replace it behind the same trait);
//! - **announces** that key, its identity key and name, signed by its
//!   identity and sealed under a key derived from the group secret, so
//!   only members can read it and nobody can speak as someone else;
//! - mixes everyone it hears through the same mixer the mesh uses.
//!
//! What the SFU sees: a room token that changes daily, a random tag per
//! member per call, and sealed bytes. Not a name, a key or a word.
//!
//! Known limits, said plainly: the group secret is shared by everyone in
//! the call, so it gives no post-compromise security (MLS will); and
//! someone removed from the call keeps the secret until the call ends.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::IdentityKeyPair;
use antiphon_net::media_seal::{generate_sender_key, MediaSealer, SenderKey, SenderKeySealer};
use antiphon_net::sfu::{room_token, new_member_tag, MemberTag, SfuClient, SfuEvent};
use antiphon_net::JitterBuffer;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::group::BlockCheck;

/// Most people in an SFU call, matching the relay's default room size.
pub const MAX_PEOPLE: usize = 50;

/// Mixer source ids for SFU members start here, clear of mesh legs.
pub const SOURCE_BASE: u64 = 1 << 32;

const KIND_MEDIA: u8 = 0;
const KIND_ANNOUNCE: u8 = 1;
const ANNOUNCE_EVERY: Duration = Duration::from_secs(5);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const JITTER_MAX_WAIT: Duration = Duration::from_millis(80);
const ANNOUNCE_CONTEXT: &[u8] = b"antiphon/sfu/announce/v1";

/// Where and how to join, as handed from member to member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub sfu: String,
    pub secret: [u8; 32],
    pub day: u64,
}

impl Invite {
    pub fn fresh(sfu: String, day: u64) -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS randomness is unavailable");
        Self { sfu, secret, day }
    }
}

/// Someone heard in the room, after their announcement checked out.
struct Member {
    id: u64,
    identity_key: [u8; 32],
    name: String,
    jitter: Arc<Mutex<JitterBuffer>>,
}

/// This machine's place in an SFU call.
pub struct SfuCall {
    pub invite: Invite,
    client: Arc<SfuClient>,
    sealer: Arc<Mutex<SenderKeySealer>>,
    members: Arc<Mutex<HashMap<MemberTag, Member>>>,
    /// The person this machine's call started with, left out of the
    /// names list because the call screen already shows them.
    primary_key: [u8; 32],
    tasks: Vec<JoinHandle<()>>,
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
    pub async fn join(
        invite: Invite,
        identity: Arc<IdentityKeyPair>,
        name: String,
        primary_key: [u8; 32],
        is_blocked: BlockCheck,
    ) -> anyhow::Result<Arc<SfuCall>> {
        let addr: SocketAddr = crate::state::resolve(&invite.sfu).await?;
        let socket = Arc::new(UdpSocket::bind(("0.0.0.0", 0)).await?);
        let me = new_member_tag();
        let room = room_token(&invite.secret, invite.day);
        let (client, mut events) = SfuClient::join(socket, addr, room, me, JOIN_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("the group relay at {} said no: {e}", invite.sfu))?;
        let client = Arc::new(client);
        let sender_key = generate_sender_key();
        let sealer = Arc::new(Mutex::new(SenderKeySealer::new(me, sender_key)));
        let members: Arc<Mutex<HashMap<MemberTag, Member>>> = Arc::default();
        let announce_cipher = announce_cipher(&invite.secret);
        let announcement = Arc::new(seal_announce(&announce_cipher, &announce_body(&identity, &room, &me, &sender_key, &name)));

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
            let (client, sealer, members) = (client.clone(), sealer.clone(), members.clone());
            let next_id = AtomicU64::new(0);
            tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        SfuEvent::Media { from, payload } => match payload.split_first() {
                            Some((&KIND_MEDIA, sealed)) => {
                                let plain = sealer.lock().unwrap().open(from, sealed);
                                let Some(plain) = plain else { continue };
                                let Some(decoded) = antiphon_net::fec::decode(&plain) else { continue };
                                if let Some(m) = members.lock().unwrap().get(&from) {
                                    m.jitter.lock().unwrap().push_with_backup(
                                        decoded.sequence as u64,
                                        decoded.primary,
                                        decoded.backup,
                                    );
                                }
                            }
                            Some((&KIND_ANNOUNCE, sealed)) => {
                                let Some(body) = open_announce(&announce_cipher, sealed) else { continue };
                                let Some(a) = parse_announce(&body, &room, &from) else { continue };
                                if is_blocked(&a.identity_key) {
                                    continue;
                                }
                                let mut members = members.lock().unwrap();
                                let known = members.get(&from).map(|m| m.identity_key);
                                match known {
                                    // A tag can't change hands mid-call.
                                    Some(k) if k != a.identity_key => continue,
                                    Some(_) => {
                                        if let Some(m) = members.get_mut(&from) {
                                            m.name = a.name;
                                        }
                                    }
                                    None => {
                                        sealer.lock().unwrap().add_sender(from, a.sender_key);
                                        members.insert(
                                            from,
                                            Member {
                                                id: next_id.fetch_add(1, Ordering::Relaxed),
                                                identity_key: a.identity_key,
                                                name: a.name,
                                                jitter: Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT))),
                                            },
                                        );
                                        // Say hello back at once, so a newcomer
                                        // doesn't wait out the timer to hear us.
                                        drop(members);
                                        let client = client.clone();
                                        let announcement = announcement.clone();
                                        tokio::spawn(async move {
                                            let _ = client.send_media(&announcement).await;
                                        });
                                    }
                                }
                            }
                            _ => {}
                        },
                        SfuEvent::Roster(present) => {
                            let mut members = members.lock().unwrap();
                            let gone: Vec<MemberTag> =
                                members.keys().filter(|t| !present.contains(t)).copied().collect();
                            for tag in gone {
                                members.remove(&tag);
                                sealer.lock().unwrap().remove_sender(&tag);
                            }
                        }
                        SfuEvent::Refused(_) => {}
                    }
                }
            })
        };

        Ok(Arc::new(SfuCall {
            invite,
            client,
            sealer,
            members,
            primary_key,
            tasks: vec![announcer, receiver],
        }))
    }

    /// Seals and sends one encoded audio frame, once, for everyone.
    pub async fn send_audio(&self, frame: &[u8]) {
        let mut out = vec![KIND_MEDIA];
        out.extend(self.sealer.lock().unwrap().seal(frame));
        let _ = self.client.send_media(&out).await;
    }

    pub fn people(&self) -> usize {
        self.members.lock().unwrap().len() + 1
    }

    /// Each member's mixer source id and jitter buffer.
    pub fn audio(&self) -> Vec<(u64, Arc<Mutex<JitterBuffer>>)> {
        self.members.lock().unwrap().values().map(|m| (SOURCE_BASE + m.id, m.jitter.clone())).collect()
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

    pub fn identity_keys(&self) -> Vec<[u8; 32]> {
        self.members.lock().unwrap().values().map(|m| m.identity_key).collect()
    }
}

fn announce_cipher(secret: &[u8; 32]) -> ChaCha20Poly1305 {
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(ANNOUNCE_CONTEXT), secret)
        .expand(b"announce key", &mut key)
        .expect("32 bytes is a valid HKDF length");
    ChaCha20Poly1305::new(&Key::from(key))
}

/// `[sender key 32][identity key 32][signature 64][name...]`, signed over
/// the room and tag too so an announcement can't be replayed elsewhere.
fn announce_body(identity: &IdentityKeyPair, room: &[u8; 32], tag: &MemberTag, key: &SenderKey, name: &str) -> Vec<u8> {
    let identity_key = identity.public().identity_key;
    let signature = identity.sign(&signed_part(room, tag, key, &identity_key, name.as_bytes()));
    let mut out = Vec::with_capacity(128 + name.len());
    out.extend_from_slice(key);
    out.extend_from_slice(&identity_key);
    out.extend_from_slice(&signature);
    out.extend_from_slice(name.as_bytes());
    out
}

fn signed_part(room: &[u8; 32], tag: &MemberTag, key: &SenderKey, identity_key: &[u8; 32], name: &[u8]) -> Vec<u8> {
    [ANNOUNCE_CONTEXT, room, tag, key, identity_key, name].concat()
}

struct Announcement {
    sender_key: SenderKey,
    identity_key: [u8; 32],
    name: String,
}

fn parse_announce(body: &[u8], room: &[u8; 32], tag: &MemberTag) -> Option<Announcement> {
    if body.len() < 128 {
        return None;
    }
    let sender_key: SenderKey = body[..32].try_into().ok()?;
    let identity_key: [u8; 32] = body[32..64].try_into().ok()?;
    let signature: [u8; 64] = body[64..128].try_into().ok()?;
    let name = &body[128..];
    antiphon_crypto::verify_signature(&identity_key, &signed_part(room, tag, &sender_key, &identity_key, name), &signature)
        .ok()?;
    let name: String = String::from_utf8_lossy(name).chars().take(64).collect();
    Some(Announcement { sender_key, identity_key, name })
}

fn seal_announce(cipher: &ChaCha20Poly1305, body: &[u8]) -> Vec<u8> {
    let mut nonce = [0u8; 12];
    getrandom::fill(&mut nonce).expect("OS randomness is unavailable");
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), Payload { msg: body, aad: ANNOUNCE_CONTEXT })
        .expect("sealing never fails for in-memory data");
    [&[KIND_ANNOUNCE][..], &nonce, &sealed].concat()
}

fn open_announce(cipher: &ChaCha20Poly1305, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < 12 {
        return None;
    }
    let (nonce, ct) = sealed.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().ok()?;
    cipher.decrypt(&Nonce::from(nonce), Payload { msg: ct, aad: ANNOUNCE_CONTEXT }).ok()
}

/// The control-message form of an invite: hex secret, so it rides in JSON.
pub fn to_message(invite: &Invite) -> crate::control::ControlMessage {
    crate::control::ControlMessage::GroupSfu {
        sfu: invite.sfu.clone(),
        secret: crate::connection_mode::encode_hex32(&invite.secret),
        day: invite.day,
    }
}

pub fn from_message(sfu: &str, secret: &str, day: u64) -> Option<Invite> {
    Some(Invite { sfu: sfu.to_string(), secret: crate::connection_mode::decode_hex32(secret)?, day })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_announcement_round_trips_and_binds_room_and_tag() {
        let me = IdentityKeyPair::generate();
        let (room, tag, key) = ([1u8; 32], [2u8; 16], [3u8; 32]);
        let cipher = announce_cipher(&[9; 32]);
        let sealed = seal_announce(&cipher, &announce_body(&me, &room, &tag, &key, "Ana"));
        let body = open_announce(&cipher, &sealed[1..]).unwrap();
        let a = parse_announce(&body, &room, &tag).unwrap();
        assert_eq!((a.sender_key, a.identity_key, a.name.as_str()), (key, me.public().identity_key, "Ana"));
        // Replayed under another tag or room, it no longer checks out.
        assert!(parse_announce(&body, &room, &[7u8; 16]).is_none());
        assert!(parse_announce(&body, &[8u8; 32], &tag).is_none());
    }

    #[test]
    fn an_announcement_is_unreadable_without_the_group_secret() {
        let me = IdentityKeyPair::generate();
        let sealed = seal_announce(&announce_cipher(&[9; 32]), &announce_body(&me, &[1; 32], &[2; 16], &[3; 32], "Ana"));
        assert!(open_announce(&announce_cipher(&[4; 32]), &sealed[1..]).is_none());
    }

    #[test]
    fn invites_survive_the_control_message() {
        let invite = Invite::fresh("relay.example:3479".into(), 20_000);
        let crate::control::ControlMessage::GroupSfu { sfu, secret, day } = to_message(&invite) else { panic!() };
        assert_eq!(from_message(&sfu, &secret, day), Some(invite));
    }

    #[tokio::test]
    async fn six_people_hear_each_other_through_one_sfu() {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let relay = Arc::new(Mutex::new(antiphon_relay::Relay::default()));
        let sfu = Arc::new(Mutex::new(antiphon_relay::sfu::Sfu::new(Default::default())));
        let node = tokio::spawn(antiphon_relay::run_node_with_sfu(socket, relay, None, None, Some(sfu)));

        let invite = Invite::fresh(addr.to_string(), 1);
        let never: BlockCheck = Arc::new(|_| false);
        let mut calls = Vec::new();
        for i in 0..6 {
            let id = Arc::new(IdentityKeyPair::generate());
            calls.push(SfuCall::join(invite.clone(), id, format!("P{i}"), [0; 32], never.clone()).await.unwrap());
        }
        // Everyone learns everyone from the announcements.
        for _ in 0..50 {
            if calls.iter().all(|c| c.people() == 6) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(calls.iter().all(|c| c.people() == 6), "{:?}", calls.iter().map(|c| c.people()).collect::<Vec<_>>());

        let frame = antiphon_net::fec::encode(0, &vec![0.25f32; 960], None);
        calls[0].send_audio(&frame).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        for c in &calls[1..] {
            let heard: usize = c.audio().iter().map(|(_, j)| j.lock().unwrap().backlog_frames()).sum();
            assert_eq!(heard, 1);
        }
        // A blocked person's voice is never mixed.
        let blocked_id = Arc::new(IdentityKeyPair::generate());
        let blocked_key = blocked_id.public().identity_key;
        let blocks: BlockCheck = Arc::new(move |k| *k == blocked_key);
        let wary = SfuCall::join(invite.clone(), Arc::new(IdentityKeyPair::generate()), "W".into(), [0; 32], blocks).await.unwrap();
        let _blocked = SfuCall::join(invite, blocked_id, "B".into(), [0; 32], never).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!wary.identity_keys().contains(&blocked_key));
        node.abort();
    }
}
