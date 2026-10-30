//! Group calls (spec §3 "Group calls": "Up to ~5 people -- direct mesh, no
//! infrastructure").
//!
//! A call starts as 1:1 and stays that shape underneath: the person you
//! called or who called you is the *primary* connection, and everything
//! a 1:1 call does -- chat, files, verification -- rides on it unchanged.
//! Each further person is a **leg**: a connection of its own, carrying
//! audio and a profile, mixed into what you hear by `crate::mixer`.
//!
//! **Mesh**, not a star: when someone adds a person, every existing member
//! connects to the newcomer directly, so nobody's audio passes through
//! anyone else's machine. The person who did the adding only introduces
//! them, in one of two ways:
//!
//! - **Through a relay**, when they have one: both members get the same
//!   one-off token; one registers it and listens, the other dials it.
//!   Neither learns the other's IP address, which is the right default
//!   for two people who may be strangers to each other (spec §3
//!   "everyone else -> relay").
//! - **Punching**, when there is no relay: each member finds its public
//!   address on a fresh socket, the introducer swaps the two, and both
//!   punch through at once.
//!
//! Either way the newcomer has to *prove* to be the identity named in the
//! introduction -- checked against the handshake -- so a token that leaked
//! cannot be used to join a call.
//!
//! Once three people are in, the members also run **MLS** over these same
//! links and seal their audio with **SFrame** under its per-epoch key, so
//! anyone who leaves can't decrypt what follows -- see `crate::group_keys`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::IdentityKeyPair;
use antiphon_net::{JitterBuffer, PeerConnection, Received};
use antiphon_net::vpn::TrafficKind;
use antiphon_net::AppSocket;
use tokio::task::JoinHandle;

use crate::connection_mode::{decode_hex32, encode_hex32};
use crate::control::{ControlMessage, Hello};

/// Including you. Past this a mesh costs every member one upload per
/// person, and the spec hands that size to an SFU.
pub const MAX_PEOPLE: usize = 5;

const JITTER_MAX_WAIT: Duration = Duration::from_millis(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Where a group message came from, so a reply goes back the same way.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Link {
    Primary,
    Leg(u64),
    /// Someone reached only through the call's SFU room, by the room's id
    /// for them (`crate::sfu_call`). Carries MLS and nothing else.
    Room(u64),
}

/// One more person in the call.
pub struct Leg {
    pub id: u64,
    pub peer: Arc<PeerConnection>,
    pub jitter: Arc<Mutex<JitterBuffer>>,
    pub profile: Arc<Mutex<Option<Hello>>>,
    pub relayed: bool,
    left: Arc<AtomicBool>,
    recv_task: JoinHandle<()>,
}

impl Drop for Leg {
    fn drop(&mut self) {
        self.recv_task.abort();
    }
}

impl Leg {
    pub fn name(&self) -> String {
        match self.profile.lock().unwrap().as_ref() {
            Some(h) if !h.display_name.is_empty() => h.display_name.clone(),
            _ => antiphon_crypto::fingerprint(&self.peer.peer_identity().identity_key),
        }
    }
}

/// A pair the introducer is matching up without a relay.
struct PunchPair {
    a: (Link, [u8; 32], Option<String>),
    b: (Link, [u8; 32], Option<String>),
}

/// Everything about the call beyond the primary connection.
#[derive(Default)]
pub struct Group {
    legs: Mutex<Vec<Leg>>,
    next_id: AtomicU64,
    /// Group messages waiting for the UI thread to act on.
    pub inbox: Mutex<Vec<(Link, ControlMessage)>>,
    /// Fresh sockets that reported a public address, awaiting the other
    /// member's, by pair.
    punch_sockets: Mutex<HashMap<u64, Arc<AppSocket>>>,
    /// Pairs this machine introduced without a relay.
    punch_pairs: Mutex<HashMap<u64, PunchPair>>,
    /// The latest thing worth telling the user, cleared once shown.
    pub notice: Mutex<String>,
    /// Set once the call has moved to an SFU (`crate::sfu_call`).
    pub sfu: Mutex<Option<Arc<crate::sfu_call::SfuCall>>>,
    /// Spec §3 MLS and SFrame: the call's group keys.
    pub keys: crate::group_keys::GroupKeys,
    /// Automatic bandwidth for the call (spec §2): every link's reports,
    /// and the plan the voice and video senders follow.
    pub link: crate::link_quality::LinkQuality,
    /// The sound card's sample rate, which decoded voice is produced at;
    /// 0 until the call sets it, read as 48 kHz.
    pub device_rate: std::sync::atomic::AtomicU32,
    /// The call's video, so each leg's pictures reach it (spec §3 group
    /// calls); set once the call is up.
    pub video: Mutex<Option<Arc<crate::video::CallVideo>>>,
}

impl Group {
    pub fn people(&self) -> usize {
        let mesh = self.legs.lock().unwrap().len() + 2;
        match self.sfu_call() {
            // Everyone the room has heard from, plus anyone on a leg who
            // hasn't joined it yet.
            Some(sfu) => {
                let in_room = sfu.identity_keys();
                let outside = self.leg_keys().iter().filter(|k| !in_room.contains(k)).count();
                sfu.people().max(2) + outside
            }
            None => mesh,
        }
    }

    /// Full for a mesh at [`MAX_PEOPLE`]; once the call is on an SFU, at
    /// the SFU's size instead.
    pub fn is_full(&self) -> bool {
        match self.sfu_call() {
            Some(_) => self.people() >= crate::sfu_call::MAX_PEOPLE,
            None => self.people() >= MAX_PEOPLE,
        }
    }

    /// The SFU room this call has moved to, if it has.
    pub fn sfu_call(&self) -> Option<Arc<crate::sfu_call::SfuCall>> {
        self.sfu.lock().unwrap().clone()
    }

    /// Everyone's names, for the call screen.
    /// Spec §3: how each is connected is shown too, since a relayed leg
    /// hides IP addresses and a direct one does not.
    pub fn names(&self) -> Vec<String> {
        self.legs
            .lock()
            .unwrap()
            .iter()
            .map(|l| if l.relayed { format!("{} (relayed)", l.name()) } else { l.name() })
            .chain(self.sfu_call().map(|s| s.names(&self.leg_keys())).unwrap_or_default())
            .collect()
    }

    /// One person's name by leg id, for labelling their captions (spec §7)
    /// and their video tiles. Ids from [`crate::sfu_call::SOURCE_BASE`] up
    /// are people in the group relay's room.
    pub fn leg_name(&self, id: u64) -> Option<String> {
        if id >= crate::sfu_call::SOURCE_BASE {
            return self.sfu_call()?.name_of(id);
        }
        self.legs.lock().unwrap().iter().find(|l| l.id == id).map(Leg::name)
    }

    /// The rate decoded voice comes out at: the sound card's.
    pub fn device_rate(&self) -> u32 {
        match self.device_rate.load(Ordering::Relaxed) {
            0 => 48_000,
            rate => rate,
        }
    }

    /// Each leg with the source number its link reports go under (spec
    /// §2 automatic bandwidth): its id + 1, as in the mixer, since the
    /// primary connection is 0.
    pub fn leg_sources(&self) -> Vec<(u64, Arc<PeerConnection>)> {
        self.legs.lock().unwrap().iter().map(|l| (l.id + 1, l.peer.clone())).collect()
    }

    pub fn leg_peers(&self) -> Vec<Arc<PeerConnection>> {
        self.legs.lock().unwrap().iter().map(|l| l.peer.clone()).collect()
    }

    pub fn leg_keys(&self) -> Vec<[u8; 32]> {
        self.legs.lock().unwrap().iter().map(|l| l.peer.peer_identity().identity_key).collect()
    }

    /// Each leg's id and jitter buffer, for the playback loop.
    pub fn leg_audio(&self) -> Vec<(u64, Arc<Mutex<JitterBuffer>>)> {
        let mut out: Vec<_> = self.legs.lock().unwrap().iter().map(|l| (l.id, l.jitter.clone())).collect();
        if let Some(sfu) = self.sfu_call() {
            out.extend(sfu.audio());
        }
        out
    }

    /// Every link we have and whose identity is at its far end, leaving
    /// out legs whose person said they were going.
    pub fn linked(&self, primary: &Arc<PeerConnection>) -> Vec<(Link, [u8; 32])> {
        std::iter::once((Link::Primary, primary.peer_identity().identity_key))
            .chain(
                self.legs
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|l| !l.left.load(Ordering::Relaxed))
                    .map(|l| (Link::Leg(l.id), l.peer.peer_identity().identity_key)),
            )
            .collect::<Vec<_>>()
            .pipe_room(self.sfu_call())
    }

    /// Sends one audio payload to every leg: SFrame-sealed (`sealed`) to
    /// those in our MLS group, plain to anyone not keyed in yet.
    pub async fn send_audio_to_legs(&self, plain: &[u8], sealed: Option<&[u8]>) {
        for leg in self.leg_peers() {
            let frame = self.keys.frame_for(&leg.peer_identity().identity_key, plain, sealed);
            let _ = leg.send_audio(frame).await;
        }
    }

    fn peer_for(&self, link: Link, primary: &Arc<PeerConnection>) -> Option<Arc<PeerConnection>> {
        match link {
            Link::Primary => Some(primary.clone()),
            Link::Leg(id) => self.legs.lock().unwrap().iter().find(|l| l.id == id).map(|l| l.peer.clone()),
            Link::Room(_) => None,
        }
    }

    fn key_for(&self, link: Link, primary: &Arc<PeerConnection>) -> Option<[u8; 32]> {
        match link {
            Link::Room(id) => self.sfu_call()?.identity_of(id),
            _ => self.peer_for(link, primary).map(|p| p.peer_identity().identity_key),
        }
    }

    /// Drops legs whose person said they were leaving. Returns their ids,
    /// so the mixer can forget them too.
    pub fn remove_departed(&self) -> Vec<(u64, String)> {
        let mut legs = self.legs.lock().unwrap();
        let gone: Vec<(u64, String)> =
            legs.iter().filter(|l| l.left.load(Ordering::Relaxed)).map(|l| (l.id, l.name())).collect();
        legs.retain(|l| !l.left.load(Ordering::Relaxed));
        for (id, _) in &gone {
            self.link.forget(id + 1);
        }
        gone
    }

    /// Adds a connected person as a leg and starts receiving from them.
    pub fn add_leg(self: &Arc<Self>, peer: PeerConnection, relayed: bool, hello: Hello) -> Option<u64> {
        let key = peer.peer_identity().identity_key;
        if self.leg_keys().contains(&key) || self.is_full() {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let peer = Arc::new(peer);
        let jitter = Arc::new(Mutex::new(JitterBuffer::new(JITTER_MAX_WAIT)));
        let profile = Arc::new(Mutex::new(None));
        let left = Arc::new(AtomicBool::new(false));

        // Their voice is decoded as it arrives, and its losses counted
        // for our reports back to them (spec §2 automatic bandwidth).
        let mut voice = antiphon_net::voice::VoiceReceiver::new(self.device_rate());
        self.link.watch(id + 1, voice.loss());
        let recv_task = {
            let (jitter, profile, left, group) = (jitter.clone(), profile.clone(), left.clone(), self.clone());
            let recv_peer = peer.clone();
            tokio::spawn(async move {
                let _ = recv_peer
                    .recv_loop(move |message| match message {
                        Received::Audio { plaintext, .. } => {
                            let Some(plaintext) = group.keys.open_audio(&key, &plaintext) else { return };
                            if let Some(decoded) = voice.decode(&plaintext) {
                                jitter.lock().unwrap().push_with_backup(
                                    decoded.sequence as u64,
                                    decoded.primary,
                                    decoded.backup,
                                );
                            }
                        }
                        Received::Control(bytes) => match ControlMessage::decode(&bytes) {
                            Some(ControlMessage::Hello(h)) => *profile.lock().unwrap() = Some(h),
                            Some(ControlMessage::GroupLeave) => left.store(true, Ordering::Relaxed),
                            Some(ControlMessage::LinkReport { loss_pct, sent_ms, echo_ms, held_ms }) => {
                                group.link.on_report(id + 1, loss_pct, sent_ms, echo_ms, held_ms);
                            }
                            Some(m) if is_group_message(&m) => group.inbox.lock().unwrap().push((Link::Leg(id), m)),
                            _ => {}
                        },
                        Received::Video(bytes) => {
                            if let Some(video) = group.video.lock().unwrap().as_ref() {
                                video.receive_from(id, &bytes);
                            }
                        }
                        // Files and chat stay on the primary connection.
                        Received::File(_) => {}
                    })
                    .await;
            })
        };

        {
            let peer = peer.clone();
            tokio::spawn(async move {
                let _ = peer.send_control(&ControlMessage::Hello(hello).encode()).await;
            });
        }
        self.legs.lock().unwrap().push(Leg { id, peer, jitter, profile, relayed, left, recv_task });
        Some(id)
    }

    /// Tells everyone we're going. Best effort, and spawned so it outlives
    /// the call being torn down.
    pub fn announce_leaving(&self, primary: &Arc<PeerConnection>) {
        let mut peers = self.leg_peers();
        if peers.is_empty() {
            return;
        }
        peers.push(primary.clone());
        tokio::spawn(async move {
            for p in peers {
                let _ = p.send_control(&ControlMessage::GroupLeave.encode()).await;
            }
        });
    }
}

pub fn is_group_message(m: &ControlMessage) -> bool {
    matches!(
        m,
        ControlMessage::GroupConnect { .. }
            | ControlMessage::GroupPunch { .. }
            | ControlMessage::GroupPunchAddr { .. }
            | ControlMessage::GroupPunchPeer { .. }
            | ControlMessage::GroupSfu { .. }
    ) || crate::group_keys::is_mls_message(m)
}

/// Adds the SFU room's members to a list of links, for anyone not
/// already reached over a real one: MLS must see each person once.
trait PipeRoom {
    /// Adds the room's members, skipping anyone already linked.
    fn pipe_room(self, sfu: Option<Arc<crate::sfu_call::SfuCall>>) -> Self;
}

impl PipeRoom for Vec<(Link, [u8; 32])> {
    fn pipe_room(mut self, sfu: Option<Arc<crate::sfu_call::SfuCall>>) -> Self {
        if let Some(sfu) = sfu {
            for (link, key) in sfu.links() {
                if !self.iter().any(|(_, k)| *k == key) {
                    self.push((link, key));
                }
            }
        }
        self
    }
}

/// Whether an identity key is blocked.
pub type BlockCheck = Arc<dyn Fn(&[u8; 32]) -> bool + Send + Sync>;

/// What acting on group messages needs from the rest of the app.
#[derive(Clone)]
pub struct Context {
    pub group: Arc<Group>,
    pub primary: Arc<PeerConnection>,
    pub identity: Arc<IdentityKeyPair>,
    pub hello: Hello,
    /// Our relay, `host:port`, if there is one.
    pub relay: Option<String>,
    /// Spec §9: blocked people are refused here as everywhere else.
    pub is_blocked: BlockCheck,
}

impl Context {
    fn admit(&self, peer: PeerConnection, expected: &[u8; 32], relayed: bool) -> Result<(), String> {
        let key = peer.peer_identity().identity_key;
        if &key != expected {
            return Err("someone other than who was introduced answered, so they were not let in".into());
        }
        if (self.is_blocked)(&key) {
            return Err("a person you blocked was added to the call; you are not connected to them".into());
        }
        self.group.add_leg(peer, relayed, self.hello.clone()).map(|_| ()).ok_or_else(|| "the call is full".into())
    }

    fn say(&self, text: impl Into<String>) {
        *self.group.notice.lock().unwrap() = text.into();
    }

    async fn send(&self, link: Link, message: ControlMessage) {
        if let Link::Room(id) = link {
            if let Some(sfu) = self.group.sfu_call() {
                sfu.send_control(id, &message).await;
            }
            return;
        }
        if let Some(peer) = self.group.peer_for(link, &self.primary) {
            let _ = peer.send_control(&message.encode()).await;
        }
    }

    /// Adds `peer`, already connected, then introduces them to everyone
    /// else in the call so the mesh closes.
    pub async fn add_person(&self, peer: PeerConnection, relayed: bool) -> Result<String, String> {
        let new_key = peer.peer_identity().identity_key;
        if new_key == self.primary.peer_identity().identity_key || self.group.leg_keys().contains(&new_key) {
            return Err("They're already in the call.".into());
        }
        // Spec §3: past the mesh, the call moves to an SFU rather than
        // turning someone away.
        if self.group.sfu_call().is_none() && self.group.people() >= MAX_PEOPLE {
            self.move_to_sfu().await?;
        }
        if let Some(sfu) = self.group.sfu_call() {
            let Some(new_id) = self.group.add_leg(peer, relayed, self.hello.clone()) else {
                return Err(format!("A group call holds up to {} people.", crate::sfu_call::MAX_PEOPLE));
            };
            self.send(Link::Leg(new_id), crate::sfu_call::to_message(&sfu.invite)).await;
            return Ok(format!("Added through the group relay. {} people in the call.", self.group.people()));
        }
        let others: Vec<(Link, [u8; 32])> = std::iter::once((Link::Primary, self.primary.peer_identity().identity_key))
            .chain(self.group.legs.lock().unwrap().iter().map(|l| (Link::Leg(l.id), l.peer.peer_identity().identity_key)))
            .collect();
        // Spec §3 MLS: the moment a 1:1 call becomes a group, whoever
        // brought in the third person starts the MLS group.
        if self.group.people() == 2 {
            self.group.keys.ensure(&self.identity);
            self.group.keys.start_group();
        }
        let Some(new_id) = self.group.add_leg(peer, relayed, self.hello.clone()) else {
            return Err(format!("A group call holds up to {MAX_PEOPLE} people."));
        };
        let new_link = Link::Leg(new_id);

        for (link, key) in others {
            let pair = random_u64();
            match &self.relay {
                Some(relay) => {
                    let token = encode_hex32(&random_32());
                    // The newcomer listens and the existing member dials:
                    // the member is already in a call, so it is the one
                    // whose socket is free to dial out from.
                    self.send(new_link, ControlMessage::GroupConnect {
                        pair,
                        peer_key: encode_hex32(&key),
                        relay: relay.clone(),
                        token: token.clone(),
                        dial: false,
                    })
                    .await;
                    self.send(link, ControlMessage::GroupConnect {
                        pair,
                        peer_key: encode_hex32(&new_key),
                        relay: relay.clone(),
                        token,
                        dial: true,
                    })
                    .await;
                }
                None => {
                    self.group.punch_pairs.lock().unwrap().insert(
                        pair,
                        PunchPair { a: (new_link, new_key, None), b: (link, key, None) },
                    );
                    self.send(new_link, ControlMessage::GroupPunch { pair, peer_key: encode_hex32(&key) }).await;
                    self.send(link, ControlMessage::GroupPunch { pair, peer_key: encode_hex32(&new_key) }).await;
                }
            }
        }
        Ok(format!("Added. {} people in the call.", self.group.people()))
    }

    /// Starts an SFU room on our relay and asks everyone in the call to
    /// join it.
    async fn move_to_sfu(&self) -> Result<(), String> {
        let Some(relay) = self.relay.clone() else {
            return Err(format!(
                "A direct group call holds up to {MAX_PEOPLE} people. Past that it needs a relay that \
                 forwards group calls (antiphon-relay --sfu), and none is set."
            ));
        };
        let day = antiphon_crypto::relay_token::day_number(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        );
        let invite = crate::sfu_call::Invite::fresh(relay, day);
        self.join_sfu(invite.clone()).await?;
        let message = crate::sfu_call::to_message(&invite);
        self.send(Link::Primary, message.clone()).await;
        for peer in self.group.leg_peers() {
            let _ = peer.send_control(&message.encode()).await;
        }
        Ok(())
    }

    /// Joins the SFU room `invite` names and records it on the group.
    async fn join_sfu(&self, invite: crate::sfu_call::Invite) -> Result<(), String> {
        let call = crate::sfu_call::SfuCall::join(
            invite,
            &self.group,
            self.identity.clone(),
            self.hello.display_name.clone(),
            self.primary.peer_identity().identity_key,
            self.is_blocked.clone(),
        )
        .await
        .map_err(|e| format!("Couldn't move the call to the group relay: {e}"))?;
        *self.group.sfu.lock().unwrap() = Some(call);
        Ok(())
    }

    /// Called on every tick of the call screen: acts on what arrived for
    /// the group and keeps the MLS keys current.
    ///
    /// MLS messages are handled right here, in arrival order, because
    /// commits have to be applied in the order they were made; connecting
    /// to people takes seconds, so those messages go to their own tasks.
    pub fn poll(&self, runtime: &tokio::runtime::Handle) {
        let inbox: Vec<_> = std::mem::take(&mut *self.group.inbox.lock().unwrap());
        let linked = self.group.linked(&self.primary);
        let mut out = Vec::new();
        if inbox.iter().any(|(_, m)| crate::group_keys::is_mls_message(m)) || self.group.people() >= 3 {
            self.group.keys.ensure(&self.identity);
        }
        for (link, message) in inbox {
            if crate::group_keys::is_mls_message(&message) {
                if let Some(from) = self.group.key_for(link, &self.primary) {
                    out.extend(self.group.keys.handle(link, from, message, &linked));
                }
            } else {
                let ctx = self.clone();
                runtime.spawn(async move { ctx.handle(link, message).await });
            }
        }
        out.extend(self.group.keys.upkeep(self.group.people(), &linked));
        if !out.is_empty() {
            let ctx = self.clone();
            runtime.spawn(async move {
                let _order = ctx.group.keys.send_serial.lock().await;
                for (link, message) in out {
                    ctx.send(link, message).await;
                }
            });
        }
    }

    /// Acts on one group message from `link`.
    pub async fn handle(&self, link: Link, message: ControlMessage) {
        match message {
            ControlMessage::GroupSfu { sfu, secret, day } => {
                if self.group.sfu_call().is_some() {
                    return;
                }
                let Some(invite) = crate::sfu_call::from_message(&sfu, &secret, day) else { return };
                match self.join_sfu(invite).await {
                    Ok(()) => self.say("The call grew past five, so it moved to a group relay. It still can't hear anything."),
                    Err(e) => self.say(e),
                }
            }
            ControlMessage::GroupConnect { pair: _, peer_key, relay, token, dial } => {
                let (Some(expected), Some(token)) = (decode_hex32(&peer_key), decode_hex32(&token)) else { return };
                if self.group.is_full() {
                    return;
                }
                let result = async {
                    let relay_addr = crate::state::resolve(&relay).await.map_err(|e| e.to_string())?;
                    // Spec §3 anonymity mode: a leg goes through a circuit
                    // like a 1:1 relayed call, so the relay (and through
                    // it, the other member) sees only the last hop.
                    let (socket, relay_addr) = match crate::anonymity::configured_hops() {
                        Some(hops) => crate::anonymity::route_to(&hops, relay_addr).await.map_err(|e| e.to_string())?,
                        None => (Arc::new(crate::vpn::bind(TrafficKind::Voice, 0).await.map_err(|e| e.to_string())?), relay_addr),
                    };
                    let peer = if dial {
                        dial_with_retry(&socket, relay_addr, token).await?;
                        tokio::time::timeout(CONNECT_TIMEOUT, PeerConnection::connect(socket, relay_addr, &self.identity))
                            .await
                            .map_err(|_| "they never answered".to_string())?
                            .map_err(|e| e.to_string())?
                    } else {
                        let weak = Arc::downgrade(&socket);
                        tokio::spawn(async move {
                            // Registered until connected (the socket is
                            // then owned by the leg) or given up on.
                            for _ in 0..(CONNECT_TIMEOUT.as_secs() / 2) {
                                let Some(s) = weak.upgrade() else { break };
                                let _ = antiphon_net::relay::register(&s, relay_addr, &[token]).await;
                                drop(s);
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                        });
                        tokio::time::timeout(CONNECT_TIMEOUT, PeerConnection::listen(socket, &self.identity))
                            .await
                            .map_err(|_| "they never arrived".to_string())?
                            .map_err(|e| e.to_string())?
                    };
                    self.admit(peer, &expected, true)
                }
                .await;
                if let Err(e) = result {
                    self.say(format!("Couldn't connect to someone in the group: {e}"));
                }
            }
            ControlMessage::GroupPunch { pair, peer_key: _ } => {
                if self.group.is_full() {
                    return;
                }
                // A punched leg is a direct connection: it would hand this
                // machine's address to the other member. Anonymity mode
                // refuses it rather than quietly connecting in the open.
                if crate::anonymity::requested() {
                    self.say("Anonymity mode is on, so nobody joins this call directly. The person adding them needs a relay in Settings > Call.");
                    return;
                }
                match crate::state::discover_public_address(0).await {
                    Ok((addr, socket)) => {
                        self.group.punch_sockets.lock().unwrap().insert(pair, socket);
                        self.send(link, ControlMessage::GroupPunchAddr { pair, addr: addr.to_string() }).await;
                    }
                    Err(e) => self.say(format!("Couldn't find this machine's public address for the group: {e}")),
                }
            }
            ControlMessage::GroupPunchAddr { pair, addr } => {
                // Only the introducer, matching a pair it set up, and only
                // from one of the two members it asked.
                let Some(from_key) = self.group.key_for(link, &self.primary) else { return };
                let ready = {
                    let mut pairs = self.group.punch_pairs.lock().unwrap();
                    let Some(p) = pairs.get_mut(&pair) else { return };
                    if p.a.1 == from_key {
                        p.a.2 = Some(addr);
                    } else if p.b.1 == from_key {
                        p.b.2 = Some(addr);
                    } else {
                        return;
                    }
                    match (&p.a.2, &p.b.2) {
                        (Some(_), Some(_)) => pairs.remove(&pair),
                        _ => None,
                    }
                };
                if let Some(PunchPair { a, b }) = ready {
                    let (a_addr, b_addr) = (a.2.unwrap_or_default(), b.2.unwrap_or_default());
                    self.send(a.0, ControlMessage::GroupPunchPeer { pair, peer_key: encode_hex32(&b.1), addr: b_addr }).await;
                    self.send(b.0, ControlMessage::GroupPunchPeer { pair, peer_key: encode_hex32(&a.1), addr: a_addr }).await;
                }
            }
            ControlMessage::GroupPunchPeer { pair, peer_key, addr } => {
                let Some(expected) = decode_hex32(&peer_key) else { return };
                let Some(socket) = self.group.punch_sockets.lock().unwrap().remove(&pair) else { return };
                let Ok(addr) = addr.parse::<SocketAddr>() else { return };
                match PeerConnection::punch(socket, addr, CONNECT_TIMEOUT, &self.identity).await {
                    Ok(peer) => {
                        if let Err(e) = self.admit(peer, &expected, false) {
                            self.say(format!("Couldn't add someone to the group: {e}"));
                        }
                    }
                    Err(e) => self.say(format!(
                        "Couldn't reach someone in the group directly ({e}). A relay in Settings > Call fixes this."
                    )),
                }
            }
            _ => {}
        }
    }
}

async fn dial_with_retry(socket: &AppSocket, relay: SocketAddr, token: [u8; 32]) -> Result<(), String> {
    // The listener may register a moment after we're told to dial.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match antiphon_net::relay::dial(socket, relay, token, Duration::from_secs(3)).await {
            Ok(()) => return Ok(()),
            Err(_) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(500)).await,
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn random_32() -> [u8; 32] {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    b
}

fn random_u64() -> u64 {
    u64::from_le_bytes(random_32()[..8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UdpSocket;

    fn hello(name: &str) -> Hello {
        Hello {
            display_name: name.into(),
            status_message: String::new(),
            presence: None,
            wants_silent: false,
            relay: String::new(),
            lends_relay: false,
            routes: String::new(),
            device: String::new(),
        }
    }

    async fn connected_pair() -> (PeerConnection, PeerConnection) {
        let a_id = IdentityKeyPair::generate();
        let b_id = IdentityKeyPair::generate();
        let a_sock = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let b_sock = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let b_addr = b_sock.local_addr().unwrap();
        let listen = tokio::spawn(async move { PeerConnection::listen(b_sock, &b_id).await.unwrap() });
        let a = PeerConnection::connect(a_sock, b_addr, &a_id).await.unwrap();
        (a, listen.await.unwrap())
    }

    #[tokio::test]
    async fn a_group_holds_at_most_five_people() {
        let group = Arc::new(Group::default());
        let mut keep = Vec::new();
        for i in 0..MAX_PEOPLE {
            let (a, b) = connected_pair().await;
            let added = group.add_leg(a, false, hello("me"));
            if i < MAX_PEOPLE - 2 {
                assert!(added.is_some(), "leg {i}");
            } else {
                assert!(added.is_none(), "leg {i} should not fit");
            }
            keep.push(b);
        }
        assert_eq!(group.people(), MAX_PEOPLE);
        // The far ends were only kept so their connections stayed up.
        drop(keep);
    }

    #[tokio::test]
    async fn the_same_person_is_not_added_twice() {
        let group = Arc::new(Group::default());
        let (a, _b) = connected_pair().await;
        let key = a.peer_identity();
        assert!(group.add_leg(a, false, hello("me")).is_some());
        assert_eq!(group.leg_keys(), vec![key.identity_key]);
    }

    #[tokio::test]
    async fn a_leg_carries_audio_and_a_leave_removes_it() {
        let group = Arc::new(Group::default());
        let (mine, theirs) = connected_pair().await;
        let id = group.add_leg(mine, false, hello("me")).unwrap();
        let theirs = Arc::new(theirs);
        let frame = antiphon_net::fec::encode(0, &[0.25; 960], None);
        theirs.send_audio(&frame).await.unwrap();
        theirs.send_control(&ControlMessage::Hello(hello("Carol")).encode()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(group.names(), vec!["Carol".to_string()]);
        let (leg_id, jitter) = group.leg_audio().remove(0);
        assert_eq!(leg_id, id);
        assert!(jitter.lock().unwrap().backlog_frames() > 0 || !matches!(jitter.lock().unwrap().pop_ready(), antiphon_net::PopResult::Empty));

        theirs.send_control(&ControlMessage::GroupLeave.encode()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(group.remove_departed(), vec![(id, "Carol".to_string())]);
        assert_eq!(group.people(), 2);
    }

    /// Feeds group messages arriving on a primary connection into that
    /// member's inbox, as `state::start_pipeline` does in the app.
    fn pump_primary(peer: Arc<PeerConnection>, group: Arc<Group>) {
        tokio::spawn(peer.recv_loop(move |m| {
            if let Received::Control(bytes) = m {
                if let Some(m) = ControlMessage::decode(&bytes).filter(is_group_message) {
                    group.inbox.lock().unwrap().push((Link::Primary, m));
                }
            }
        }));
    }

    async fn pair_between(a: &Arc<IdentityKeyPair>, b: &Arc<IdentityKeyPair>) -> (PeerConnection, PeerConnection) {
        let a_sock = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let b_sock = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
        let b_addr = b_sock.local_addr().unwrap();
        let b2 = b.clone();
        let listen = tokio::spawn(async move { PeerConnection::listen(b_sock, &b2).await.unwrap() });
        let a_conn = PeerConnection::connect(a_sock, b_addr, a).await.unwrap();
        (a_conn, listen.await.unwrap())
    }

    fn context(group: Arc<Group>, primary: Arc<PeerConnection>, identity: Arc<IdentityKeyPair>, relay: Option<String>, name: &str) -> Context {
        Context { group, primary, identity, hello: hello(name), relay, is_blocked: Arc::new(|_| false) }
    }

    #[tokio::test]
    async fn adding_a_third_person_closes_the_mesh_through_a_relay() {
        let relay_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay_addr = relay_socket.local_addr().unwrap();
        tokio::spawn(antiphon_relay::run(
            relay_socket,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(antiphon_relay::Limits::default()))),
        ));

        let (alice, bob, carol) = (
            Arc::new(IdentityKeyPair::generate()),
            Arc::new(IdentityKeyPair::generate()),
            Arc::new(IdentityKeyPair::generate()),
        );
        // Alice is on a call with Bob, then calls Carol to add her.
        let (a_b, b_a) = pair_between(&alice, &bob).await;
        let (a_c, c_a) = pair_between(&alice, &carol).await;
        let (g_a, g_b, g_c) = (Arc::new(Group::default()), Arc::new(Group::default()), Arc::new(Group::default()));
        let (b_a, c_a) = (Arc::new(b_a), Arc::new(c_a));
        pump_primary(b_a.clone(), g_b.clone());
        pump_primary(c_a.clone(), g_c.clone());

        let ctx_a = context(g_a.clone(), Arc::new(a_b), alice.clone(), Some(relay_addr.to_string()), "Alice");
        let ctx_b = context(g_b.clone(), b_a, bob.clone(), None, "Bob");
        let ctx_c = context(g_c.clone(), c_a, carol.clone(), None, "Carol");

        ctx_a.add_person(a_c, false).await.unwrap();
        assert_eq!(g_a.people(), 3);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while (g_b.people() < 3 || g_c.people() < 3) && tokio::time::Instant::now() < deadline {
            for (ctx, g) in [(&ctx_b, &g_b), (&ctx_c, &g_c)] {
                for (link, m) in std::mem::take(&mut *g.inbox.lock().unwrap()) {
                    let ctx = ctx.clone();
                    tokio::spawn(async move { ctx.handle(link, m).await });
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(g_b.leg_keys(), vec![carol.public().identity_key], "Bob should reach Carol directly");
        assert_eq!(g_c.leg_keys(), vec![bob.public().identity_key], "Carol should reach Bob directly");
        assert!(g_b.names()[0].contains("relayed"), "{:?}", g_b.names());
    }

    /// Spec §3 MLS + SFrame over a real three-person mesh: the members key
    /// themselves in over their links, audio between them is SFrame-sealed,
    /// and when one leaves the others move to an epoch the leaver can't
    /// read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_group_call_keys_itself_with_mls_and_rekeys_when_someone_leaves() {
        let relay_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let relay_addr = relay_socket.local_addr().unwrap();
        tokio::spawn(antiphon_relay::run(
            relay_socket,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(antiphon_relay::Limits::default()))),
        ));
        let (alice, bob, carol) = (
            Arc::new(IdentityKeyPair::generate()),
            Arc::new(IdentityKeyPair::generate()),
            Arc::new(IdentityKeyPair::generate()),
        );
        let keys = [alice.public().identity_key, bob.public().identity_key, carol.public().identity_key];
        let (a_b, b_a) = pair_between(&alice, &bob).await;
        let (a_c, c_a) = pair_between(&alice, &carol).await;
        let (g_a, g_b, g_c) = (Arc::new(Group::default()), Arc::new(Group::default()), Arc::new(Group::default()));
        let (a_b, b_a, c_a) = (Arc::new(a_b), Arc::new(b_a), Arc::new(c_a));
        pump_primary(a_b.clone(), g_a.clone());
        pump_primary(b_a.clone(), g_b.clone());
        pump_primary(c_a.clone(), g_c.clone());

        let ctx_a = context(g_a.clone(), a_b, alice.clone(), Some(relay_addr.to_string()), "Alice");
        let ctx_b = context(g_b.clone(), b_a, bob.clone(), None, "Bob");
        let ctx_c = context(g_c.clone(), c_a.clone(), carol.clone(), None, "Carol");
        ctx_a.add_person(a_c, false).await.unwrap();

        let runtime = tokio::runtime::Handle::current();

        // Everyone polls, as the call screen does, until all three share
        // one MLS epoch with all three in it.
        let all = {
            let mut k = keys.to_vec();
            k.sort_unstable();
            k
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            for ctx in [&ctx_a, &ctx_b, &ctx_c] {
                ctx.poll(&runtime);
            }
            let epochs = [g_a.keys.epoch(), g_b.keys.epoch(), g_c.keys.epoch()];
            let keyed = [&g_a, &g_b, &g_c].iter().all(|g| g.keys.members() == all);
            if keyed && epochs[0].is_some() && epochs.iter().all(|e| *e == epochs[0]) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "MLS never converged: {epochs:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let keyed_epoch = g_a.keys.epoch().unwrap();

        // Bob's audio reaches Carol over their own leg, SFrame-sealed.
        let frame = antiphon_net::fec::encode(7, &[0.25; 960], None);
        let sealed = g_b.keys.seal_audio(&frame).unwrap();
        assert_eq!(sealed[0], crate::group_keys::SFRAME_AUDIO_TAG);
        assert_eq!(g_b.keys.frame_for(&keys[2], &frame, Some(&sealed)), &sealed[..]);
        g_b.send_audio_to_legs(&frame, Some(&sealed)).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let (_, jitter) = g_c.leg_audio().remove(0);
        assert!(jitter.lock().unwrap().backlog_frames() > 0, "Carol should hear Bob's sealed frame");
        // Alice can open Bob's frame too (one seal, every member).
        assert_eq!(g_a.keys.open_audio(&keys[1], &sealed).unwrap(), frame);
        // And the plain frame from a keyed member is refused.
        assert!(g_a.keys.open_audio(&keys[1], &frame).is_none());

        // Carol leaves. Alice, the committer, removes her: a new epoch.
        g_c.announce_leaving(&c_a);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            for ctx in [&ctx_a, &ctx_b] {
                ctx.poll(&runtime);
            }
            let (ea, eb) = (g_a.keys.epoch(), g_b.keys.epoch());
            if ea > Some(keyed_epoch) && ea == eb && g_b.keys.members().len() == 2 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no rekey after Carol left: {ea:?} {eb:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(!g_a.keys.members().contains(&keys[2]));

        // Alice's new audio opens for Bob, not for Carol's old keys.
        let sealed = g_a.keys.seal_audio(&frame).unwrap();
        assert_eq!(g_b.keys.open_audio(&keys[0], &sealed).unwrap(), frame);
        assert!(g_c.keys.open_audio(&keys[0], &sealed).is_none());
    }
}
