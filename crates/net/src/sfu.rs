//! Group calls through a selective forwarding unit (spec §3 "Group calls":
//! "Beyond that -- SFU relay forwarding encrypted frames", and "Community
//! owners can self-host their own SFU").
//!
//! A direct mesh stops scaling at a handful of people: every member sends
//! its audio to every other member, so upload grows with the room. An SFU
//! turns that around -- each member sends one copy to the SFU, and the SFU
//! fans it out to everyone else in the room. What the SFU learns while
//! doing that is the whole privacy story, so it is listed here:
//!
//! - **A room token**: 32 random-looking bytes that only people holding
//!   the group's secret can compute ([`room_token`]), rotating daily. Never
//!   a group id, a name or a key, so the operator cannot tell which
//!   community or which people a room belongs to, nor link today's room
//!   to tomorrow's.
//! - **Member tags**: 16 random bytes each member picks afresh for every
//!   call ([`new_member_tag`]). They let members tell each other's frames
//!   apart; they are not identities and are never reused, so the operator
//!   cannot follow anyone from one call to the next.
//! - **Socket addresses**, because forwarding needs them, held only while
//!   the member keeps refreshing.
//! - **Nothing else.** Media payloads are sealed end to end by the members
//!   ([`crate::media_seal`]) with keys the SFU never sees; it forwards them
//!   byte for byte without parsing them.
//!
//! Joining needs one round trip proving the joiner can receive at the
//! address it claims (a stateless cookie, as in DTLS/QUIC). Without it a
//! spoofed JOIN could make the SFU fan a room's traffic at a victim.
//!
//! ```text
//! JOIN      MAGIC 0x01 [room; 32] [member; 16] [cookie; 16]?
//! LEAVE     MAGIC 0x02 [member; 16]
//! MEDIA     MAGIC 0x03 [member; 16] payload...      forwarded verbatim
//! CHALLENGE MAGIC 0x80 [cookie; 16]                   "send JOIN again with this"
//! JOINED    MAGIC 0x81 [members: u16]
//! ROSTER    MAGIC 0x82 [member; 16]*                  everyone in the room, joiner included
//! REFUSED   MAGIC 0x83 [reason: u8]
//! ```
//!
//! A member that is already in a room refreshes by sending JOIN again
//! (every [`JOIN_INTERVAL`]) without a cookie; the reply carries the
//! roster, so a ROSTER lost or reordered on the way heals at the next
//! refresh.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hkdf::Hkdf;
use sha2::Sha256;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch};

use crate::error::NetError;

/// `0xA7` is not protocol version 1, so `antiphon_protocol::decode`
/// rejects every SFU datagram outright, and a node tells SFU traffic from
/// relay, mailbox and blob traffic by this prefix alone.
pub const MAGIC: [u8; 4] = [0xA7, b'S', b'F', b'1'];

pub const ROOM_LEN: usize = 32;
pub const MEMBER_LEN: usize = 16;
pub const COOKIE_LEN: usize = 16;

pub type RoomToken = [u8; ROOM_LEN];
pub type MemberTag = [u8; MEMBER_LEN];
pub type Cookie = [u8; COOKIE_LEN];

/// Largest media payload carried. Calls send 20 ms of 48 kHz audio as
/// 32-bit samples plus an FEC backup (`crate::fec`), about 4.2 KB once
/// sealed, so this leaves headroom for that and nothing much larger.
pub const MAX_MEDIA_PAYLOAD: usize = 6000;

/// Most members a ROSTER can list, which keeps one inside the 4 KB every
/// receive buffer in this project is sized for. An SFU's per-room limit
/// is clamped to this.
pub const MAX_ROSTER: usize = 200;

/// How often a member re-joins. Well inside the SFU's idle expiry, and
/// short enough to keep a NAT mapping to the SFU open while muted.
pub const JOIN_INTERVAL: Duration = Duration::from_secs(10);

const OP_JOIN: u8 = 0x01;
const OP_LEAVE: u8 = 0x02;
const OP_MEDIA: u8 = 0x03;
const OP_CHALLENGE: u8 = 0x80;
const OP_JOINED: u8 = 0x81;
const OP_ROSTER: u8 = 0x82;
const OP_REFUSED: u8 = 0x83;

const ROOM_SALT: &[u8] = b"antiphon/sfu/room/v1";

/// The room a group meets in at an SFU, for `day`
/// (`antiphon_crypto::relay_token::day_number`). `group_secret` is shared
/// by the group's members and never sent anywhere; the operator sees only
/// the output, which changes daily.
///
/// Everyone in one call must use the same `day`: the caller who starts a
/// call should pick it and carry it in the call invitation, so a call
/// that runs past midnight -- or a late joiner whose clock disagrees --
/// stays in one room.
pub fn room_token(group_secret: &[u8], day: u64) -> RoomToken {
    let hk = Hkdf::<Sha256>::new(Some(ROOM_SALT), group_secret);
    let mut out = [0u8; ROOM_LEN];
    hk.expand(&day.to_be_bytes(), &mut out)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    out
}

/// A fresh member tag for one call. Never reuse one across calls: a
/// stable tag would be a pseudonym the operator could follow.
pub fn new_member_tag() -> MemberTag {
    let mut tag = [0u8; MEMBER_LEN];
    getrandom::fill(&mut tag).expect("OS randomness is unavailable");
    tag
}

/// Why an SFU said no.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The room already has as many members as this SFU allows.
    RoomFull,
    /// The SFU holds as many rooms as it allows.
    NodeFull,
    /// This address is already in as many rooms as one address may be.
    TooManyRooms,
    /// Another live member holds this tag. Pick a new one.
    TagTaken,
    /// The operator hasn't switched the SFU on.
    Disabled,
}

impl Refusal {
    fn code(self) -> u8 {
        match self {
            Refusal::RoomFull => 1,
            Refusal::NodeFull => 2,
            Refusal::TooManyRooms => 3,
            Refusal::TagTaken => 4,
            Refusal::Disabled => 5,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        Some(match c {
            1 => Refusal::RoomFull,
            2 => Refusal::NodeFull,
            3 => Refusal::TooManyRooms,
            4 => Refusal::TagTaken,
            5 => Refusal::Disabled,
            _ => return None,
        })
    }

    pub fn describe(self) -> &'static str {
        match self {
            Refusal::RoomFull => "the call is full on this server",
            Refusal::NodeFull => "the group-call server is full",
            Refusal::TooManyRooms => "this connection is already in too many calls on that server",
            Refusal::TagTaken => "that call slot is in use; try joining again",
            Refusal::Disabled => "this server doesn't host group calls",
        }
    }
}

/// A parsed SFU datagram.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Join { room: RoomToken, member: MemberTag, cookie: Option<Cookie> },
    Leave { member: MemberTag },
    Media { member: MemberTag, payload: Vec<u8> },
    Challenge { cookie: Cookie },
    Joined { members: u16 },
    Roster(Vec<MemberTag>),
    Refused(Refusal),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        match self {
            Message::Join { room, member, cookie } => {
                out.push(OP_JOIN);
                out.extend_from_slice(room);
                out.extend_from_slice(member);
                if let Some(c) = cookie {
                    out.extend_from_slice(c);
                }
            }
            Message::Leave { member } => {
                out.push(OP_LEAVE);
                out.extend_from_slice(member);
            }
            Message::Media { member, payload } => return encode_media(member, payload),
            Message::Challenge { cookie } => {
                out.push(OP_CHALLENGE);
                out.extend_from_slice(cookie);
            }
            Message::Joined { members } => {
                out.push(OP_JOINED);
                out.extend_from_slice(&members.to_be_bytes());
            }
            Message::Roster(tags) => {
                out.push(OP_ROSTER);
                for t in tags {
                    out.extend_from_slice(t);
                }
            }
            Message::Refused(r) => {
                out.push(OP_REFUSED);
                out.push(r.code());
            }
        }
        out
    }

    /// `None` for anything that is not a well-formed SFU datagram --
    /// including every ordinary protocol frame and every relay, mailbox
    /// and blob datagram.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, body) = rest.split_first()?;
        match op {
            OP_JOIN => {
                let (room, rest) = body.split_at_checked(ROOM_LEN)?;
                let (member, cookie) = rest.split_at_checked(MEMBER_LEN)?;
                let cookie = match cookie.len() {
                    0 => None,
                    COOKIE_LEN => Some(cookie.try_into().ok()?),
                    _ => return None,
                };
                Some(Message::Join { room: room.try_into().ok()?, member: member.try_into().ok()?, cookie })
            }
            OP_LEAVE => Some(Message::Leave { member: body.try_into().ok()? }),
            OP_MEDIA => {
                let (member, payload) = body.split_at_checked(MEMBER_LEN)?;
                if payload.is_empty() || payload.len() > MAX_MEDIA_PAYLOAD {
                    return None;
                }
                Some(Message::Media { member: member.try_into().ok()?, payload: payload.to_vec() })
            }
            OP_CHALLENGE => Some(Message::Challenge { cookie: body.try_into().ok()? }),
            OP_JOINED => Some(Message::Joined { members: u16::from_be_bytes(body.try_into().ok()?) }),
            OP_ROSTER => {
                if body.len() % MEMBER_LEN != 0 || body.len() / MEMBER_LEN > MAX_ROSTER {
                    return None;
                }
                let tags = body.chunks_exact(MEMBER_LEN).map(|c| c.try_into().expect("chunk is MEMBER_LEN")).collect();
                Some(Message::Roster(tags))
            }
            OP_REFUSED => match body {
                [code] => Some(Message::Refused(Refusal::from_code(*code)?)),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is_sfu_datagram(bytes: &[u8]) -> bool {
        bytes.starts_with(&MAGIC)
    }

    /// The member tag of a MEDIA datagram, without copying its payload --
    /// what an SFU needs to route one.
    pub fn media_sender(bytes: &[u8]) -> Option<MemberTag> {
        let rest = bytes.strip_prefix(&MAGIC[..])?;
        let (&op, body) = rest.split_first()?;
        if op != OP_MEDIA || body.len() <= MEMBER_LEN || body.len() - MEMBER_LEN > MAX_MEDIA_PAYLOAD {
            return None;
        }
        body[..MEMBER_LEN].try_into().ok()
    }
}

/// A MEDIA datagram, built without an intermediate `Message`.
pub fn encode_media(member: &MemberTag, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(MAGIC.len() + 1 + MEMBER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.push(OP_MEDIA);
    out.extend_from_slice(member);
    out.extend_from_slice(payload);
    out
}

/// What a joined [`SfuClient`] hands its owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SfuEvent {
    /// A sealed frame from another member. Open it with the call's
    /// [`crate::media_seal::MediaSealer`]; the SFU never could.
    Media { from: MemberTag, payload: Vec<u8> },
    /// Everyone else now in the room (this member left out).
    Roster(Vec<MemberTag>),
    /// A refresh was refused -- typically the SFU restarted and has no
    /// room for us any more. The call through this SFU is over.
    Refused(Refusal),
}

/// Events queued for a slow consumer before media starts being dropped.
/// Audio that cannot be played on time is worthless, so dropping is the
/// right thing; a roster is never lost for good, since
/// [`SfuClient::roster`] always holds the latest.
pub const EVENT_QUEUE: usize = 512;

/// One member's connection to an SFU room.
///
/// Owns its socket's receive side: give it a socket of its own. Dropping
/// it stops the refreshes, and the SFU forgets the member after its idle
/// expiry; [`SfuClient::leave`] says goodbye straight away.
pub struct SfuClient {
    socket: Arc<UdpSocket>,
    sfu: SocketAddr,
    room: RoomToken,
    member: MemberTag,
    roster: watch::Receiver<Vec<MemberTag>>,
    task: tokio::task::JoinHandle<()>,
}

impl SfuClient {
    /// Joins `room` at `sfu` as `member`, and returns once the SFU has
    /// confirmed and sent the roster. Events arrive on the returned
    /// receiver until the client is dropped or leaves.
    pub async fn join(
        socket: Arc<UdpSocket>,
        sfu: SocketAddr,
        room: RoomToken,
        member: MemberTag,
        timeout: Duration,
    ) -> Result<(Self, mpsc::Receiver<SfuEvent>), NetError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut cookie = None;
        let mut joined = false;
        let mut roster: Option<Vec<MemberTag>> = None;
        let mut buf = vec![0u8; 8192];
        'outer: while tokio::time::Instant::now() < deadline {
            socket.send_to(&Message::Join { room, member, cookie }.encode(), sfu).await?;
            let wait_until = (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline);
            while let Ok(Ok((n, from))) = tokio::time::timeout_at(wait_until, socket.recv_from(&mut buf)).await {
                if from != sfu {
                    continue;
                }
                match Message::decode(&buf[..n]) {
                    Some(Message::Challenge { cookie: c }) => {
                        cookie = Some(c);
                        continue 'outer;
                    }
                    Some(Message::Joined { .. }) => joined = true,
                    Some(Message::Roster(tags)) => roster = Some(tags),
                    Some(Message::Refused(r)) => return Err(NetError::SfuRefused(r)),
                    _ => {}
                }
                if joined && roster.is_some() {
                    break 'outer;
                }
            }
        }
        let (true, Some(roster)) = (joined, roster) else { return Err(NetError::SfuTimeout) };

        let (roster_tx, roster_rx) = watch::channel(others(&roster, &member));
        let (events_tx, events_rx) = mpsc::channel(EVENT_QUEUE);
        let task = tokio::spawn(run_member(socket.clone(), sfu, room, member, roster_tx, events_tx));
        Ok((Self { socket, sfu, room, member, roster: roster_rx, task }, events_rx))
    }

    /// Sends one sealed frame to everyone else in the room. `sealed` must
    /// already be sealed ([`crate::media_seal::MediaSealer::seal`]) and no
    /// longer than [`MAX_MEDIA_PAYLOAD`].
    pub async fn send_media(&self, sealed: &[u8]) -> Result<(), NetError> {
        if sealed.is_empty() || sealed.len() > MAX_MEDIA_PAYLOAD {
            return Err(NetError::SfuPayloadSize(sealed.len()));
        }
        self.socket.send_to(&encode_media(&self.member, sealed), self.sfu).await?;
        Ok(())
    }

    pub fn member(&self) -> MemberTag {
        self.member
    }

    pub fn room(&self) -> RoomToken {
        self.room
    }

    pub fn sfu_addr(&self) -> SocketAddr {
        self.sfu
    }

    /// Everyone else in the room, as of the latest roster.
    pub fn roster(&self) -> Vec<MemberTag> {
        self.roster.borrow().clone()
    }

    /// A handle that wakes on every roster change.
    pub fn roster_watch(&self) -> watch::Receiver<Vec<MemberTag>> {
        self.roster.clone()
    }

    /// Tells the SFU to forget this member now, and stops. Best effort:
    /// if the datagram is lost, the SFU forgets the member at idle expiry.
    pub async fn leave(self) {
        self.task.abort();
        let _ = self.socket.send_to(&Message::Leave { member: self.member }.encode(), self.sfu).await;
    }
}

impl Drop for SfuClient {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn others(roster: &[MemberTag], me: &MemberTag) -> Vec<MemberTag> {
    roster.iter().filter(|t| *t != me).copied().collect()
}

/// The background half of a member: refreshes, answers challenges after
/// an SFU restart, and turns datagrams into events.
async fn run_member(
    socket: Arc<UdpSocket>,
    sfu: SocketAddr,
    room: RoomToken,
    member: MemberTag,
    roster_tx: watch::Sender<Vec<MemberTag>>,
    events: mpsc::Sender<SfuEvent>,
) {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + JOIN_INTERVAL, JOIN_INTERVAL);
    let mut buf = vec![0u8; 8192];
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // No cookie: a live member refreshes without one, and an
                // SFU that has forgotten us answers with a fresh challenge.
                let _ = socket.send_to(&Message::Join { room, member, cookie: None }.encode(), sfu).await;
            }
            received = socket.recv_from(&mut buf) => {
                let Ok((n, from)) = received else { continue };
                if from != sfu {
                    continue;
                }
                match Message::decode(&buf[..n]) {
                    Some(Message::Media { member: from, payload }) if from != member => {
                        let _ = events.try_send(SfuEvent::Media { from, payload });
                    }
                    Some(Message::Roster(tags)) => {
                        let list = others(&tags, &member);
                        let changed = *roster_tx.borrow() != list;
                        if changed {
                            roster_tx.send_replace(list.clone());
                            let _ = events.try_send(SfuEvent::Roster(list));
                        }
                    }
                    Some(Message::Challenge { cookie }) => {
                        let join = Message::Join { room, member, cookie: Some(cookie) };
                        let _ = socket.send_to(&join.encode(), sfu).await;
                    }
                    Some(Message::Refused(r)) => {
                        let _ = events.try_send(SfuEvent::Refused(r));
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_message_round_trips() {
        for m in [
            Message::Join { room: [1; 32], member: [2; 16], cookie: None },
            Message::Join { room: [1; 32], member: [2; 16], cookie: Some([3; 16]) },
            Message::Leave { member: [4; 16] },
            Message::Media { member: [5; 16], payload: b"sealed".to_vec() },
            Message::Challenge { cookie: [6; 16] },
            Message::Joined { members: 7 },
            Message::Roster(vec![[8; 16], [9; 16]]),
            Message::Roster(vec![]),
            Message::Refused(Refusal::RoomFull),
            Message::Refused(Refusal::NodeFull),
            Message::Refused(Refusal::TooManyRooms),
            Message::Refused(Refusal::TagTaken),
            Message::Refused(Refusal::Disabled),
        ] {
            assert_eq!(Message::decode(&m.encode()), Some(m));
        }
    }

    #[test]
    fn media_sender_reads_the_tag_without_the_payload() {
        let bytes = encode_media(&[7; 16], b"x");
        assert_eq!(Message::media_sender(&bytes), Some([7; 16]));
        assert_eq!(Message::media_sender(&Message::Leave { member: [7; 16] }.encode()), None);
        assert_eq!(Message::media_sender(&encode_media(&[7; 16], b"")), None);
    }

    #[test]
    fn other_services_traffic_is_never_mistaken_for_sfu_traffic() {
        let frame = antiphon_protocol::encode(antiphon_protocol::FrameType::Audio, 1, b"hello");
        assert!(!Message::is_sfu_datagram(&frame));
        assert!(Message::decode(&frame).is_none());
        assert!(antiphon_protocol::decode(&Message::Joined { members: 1 }.encode()).is_err());
        let relay = crate::relay::Message::Dial([0; 32]).encode();
        assert!(!Message::is_sfu_datagram(&relay));
        assert!(!crate::relay::Message::is_relay_datagram(&Message::Leave { member: [0; 16] }.encode()));
        assert!(!crate::mailbox::Message::is_mailbox_datagram(&Message::Leave { member: [0; 16] }.encode()));
        assert!(!crate::blob::Message::is_blob_datagram(&Message::Leave { member: [0; 16] }.encode()));
    }

    #[test]
    fn malformed_datagrams_are_rejected() {
        let mut join = Message::Join { room: [0; 32], member: [0; 16], cookie: None }.encode();
        join.push(1); // a ragged cookie
        assert!(Message::decode(&join).is_none());
        let mut roster = Message::Roster(vec![[0; 16]]).encode();
        roster.pop();
        assert!(Message::decode(&roster).is_none());
        assert!(Message::decode(&encode_media(&[0; 16], &[0; MAX_MEDIA_PAYLOAD + 1])).is_none());
        assert!(Message::decode(&[MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], 0x7f]).is_none());
        assert!(Message::decode(&[MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], OP_REFUSED, 99]).is_none());
    }

    #[test]
    fn a_full_roster_and_a_full_media_frame_fit_one_datagram() {
        assert!(Message::Roster(vec![[0; 16]; MAX_ROSTER]).encode().len() <= 4096);
        assert!(encode_media(&[0; 16], &[0; MAX_MEDIA_PAYLOAD]).len() <= 8192, "the client receive buffer");
    }

    #[test]
    fn room_tokens_are_shared_by_the_group_and_rotate_daily() {
        let secret = [42u8; 32];
        assert_eq!(room_token(&secret, 100), room_token(&secret, 100));
        assert_ne!(room_token(&secret, 100), room_token(&secret, 101));
        assert_ne!(room_token(&secret, 100), room_token(&[43u8; 32], 100));
        // Never the relay's invite token for the same secret.
        assert_ne!(room_token(&secret, 100), antiphon_crypto::relay_token::from_invite_secret(&secret, 100));
    }

    #[test]
    fn member_tags_are_fresh() {
        assert_ne!(new_member_tag(), new_member_tag());
    }
}
