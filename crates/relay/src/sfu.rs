//! Group-call forwarding: a selective forwarding unit (spec §3 "Group
//! calls": "Beyond that -- SFU relay forwarding encrypted frames", and
//! "Community owners can self-host their own SFU").
//!
//! **Off by default.** The operator switches it on and sets how many
//! rooms it holds, how many members a room may have, and how many rooms
//! one address may be in.
//!
//! Members join a room named by a blinded token; every MEDIA datagram a
//! member sends is forwarded, byte for byte, to every other member of
//! that room and nobody else. What the node holds is exactly, per member:
//! a room token only the group can compute and which rotates daily, a
//! member tag picked at random for this one call, a socket address, and
//! when it was last heard from. No identity, no key, no payload -- media
//! is sealed end to end by the members and never parsed here beyond the
//! tag that routes it. Members who stop refreshing are dropped after
//! [`MEMBER_IDLE`], and an empty room is dropped with its last member.
//! Nothing is logged; the operator's view is [`SfuStats`].
//!
//! A new member must first prove it can receive at the address it claims:
//! the first JOIN is answered with a stateless cookie (a keyed hash of the
//! address, room and tag), and only a JOIN carrying it is admitted. That
//! keeps a spoofed JOIN from turning a room's fan-out against a victim.
//!
//! The core is a plain state machine ([`Sfu::handle`]) so it can be
//! tested without sockets; `run_node_with_sfu` dispatches to it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use antiphon_net::sfu::{Cookie, MemberTag, Message, Refusal, RoomToken, COOKIE_LEN, MAX_ROSTER};
use sha2::{Digest, Sha256};

/// How long a member survives without a refresh or a frame. Clients
/// refresh every `antiphon_net::sfu::JOIN_INTERVAL` (10 s).
pub const MEMBER_IDLE: Duration = Duration::from_secs(30);

/// A member whose address changed (a new NAT mapping) may take its tag to
/// the new address only once the old one has gone this quiet, so a room
/// member who learned the tag from the roster cannot steal a live seat.
pub const MOVE_GRACE: Duration = Duration::from_secs(5);

/// How long a cookie stays valid: one to two of these.
const COOKIE_PERIOD: Duration = Duration::from_secs(60);

/// What the operator allows the SFU to hold, so hosting group calls cannot
/// be turned into a way to exhaust the node.
#[derive(Clone, Copy, Debug)]
pub struct SfuLimits {
    /// Rooms held at once, across everyone.
    pub max_rooms: usize,
    /// Clamped to `antiphon_net::sfu::MAX_ROSTER`.
    pub max_members_per_room: usize,
    /// Room memberships any one address may hold at once.
    pub max_rooms_per_addr: usize,
}

impl Default for SfuLimits {
    fn default() -> Self {
        Self { max_rooms: 10_000, max_members_per_room: 50, max_rooms_per_addr: 4 }
    }
}

/// Aggregate counters -- the only thing the SFU can report, by design.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SfuStats {
    /// Rooms with at least one member right now.
    pub rooms: usize,
    /// Members across all rooms right now.
    pub members: usize,
    /// Datagrams sent out by fan-out (one per receiving member).
    pub forwarded: u64,
}

/// Everything the SFU holds about one member: no identity, no key.
struct Member {
    /// The room the member joined.
    room: RoomToken,
    /// Where the member's frames and roster go; only this address may speak
    /// for its tag.
    addr: SocketAddr,
    /// When the member was last heard from, for [`MEMBER_IDLE`].
    last_seen: Instant,
}

/// The SFU state machine: rooms, members and the cookie key, with no
/// sockets, so it can be tested directly.
pub struct Sfu {
    /// Operator limits, with the per-room cap already clamped.
    limits: SfuLimits,
    /// Keys the join cookies. Random per process and never stored.
    secret: [u8; 32],
    /// When this process started, the epoch for cookie periods.
    started: Instant,
    /// Member tags in each room, in join order.
    rooms: HashMap<RoomToken, Vec<MemberTag>>,
    /// Every member, by tag.
    members: HashMap<MemberTag, Member>,
    /// Room memberships per address, for [`SfuLimits::max_rooms_per_addr`].
    per_addr: HashMap<SocketAddr, usize>,
    /// Datagrams sent by fan-out, for [`SfuStats`].
    forwarded: u64,
    /// When quiet members were last swept, so sweeping stays cheap.
    last_sweep: Option<Instant>,
}

impl Sfu {
    /// An empty SFU with a fresh cookie key.
    pub fn new(limits: SfuLimits) -> Self {
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).expect("OS randomness is unavailable");
        let limits = SfuLimits { max_members_per_room: limits.max_members_per_room.min(MAX_ROSTER), ..limits };
        Self {
            limits,
            secret,
            started: Instant::now(),
            rooms: HashMap::new(),
            members: HashMap::new(),
            per_addr: HashMap::new(),
            forwarded: 0,
            last_sweep: None,
        }
    }

    /// The limits in force, after clamping.
    pub fn limits(&self) -> SfuLimits {
        self.limits
    }

    /// Totals for the operator; nothing per room or per member.
    pub fn stats(&self) -> SfuStats {
        SfuStats { rooms: self.rooms.len(), members: self.members.len(), forwarded: self.forwarded }
    }

    /// Handles one datagram from `from`, returning what to send where.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut out = self.sweep_if_due(now);

        // The hot path: route a frame by its tag without copying the
        // payload into a `Message` first.
        if let Some(sender) = Message::media_sender(bytes) {
            let Some(member) = self.members.get_mut(&sender) else { return out };
            if member.addr != from {
                // Only the address that joined may speak for a tag.
                return out;
            }
            member.last_seen = now;
            let room = member.room;
            for tag in self.rooms.get(&room).into_iter().flatten() {
                if *tag == sender {
                    continue;
                }
                if let Some(m) = self.members.get(tag) {
                    out.push((m.addr, bytes.to_vec()));
                    self.forwarded += 1;
                }
            }
            return out;
        }

        match Message::decode(bytes) {
            Some(Message::Join { room, member, cookie }) => out.extend(self.join(from, room, member, cookie, now)),
            // Only the address that joined may take a member out.
            Some(Message::Leave { member }) if self.members.get(&member).is_some_and(|m| m.addr == from) => {
                out.extend(self.remove(member));
            }
            // Replies are for clients, and garbage is garbage.
            _ => {}
        }
        out
    }

    /// Handles a JOIN: refreshes a live member, moves a quiet one to a proven
    /// new address, or admits a new one once its cookie checks out.
    fn join(
        &mut self,
        from: SocketAddr,
        room: RoomToken,
        tag: MemberTag,
        cookie: Option<Cookie>,
        now: Instant,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        let refuse = |r| vec![(from, Message::Refused(r).encode())];
        if let Some(existing) = self.members.get_mut(&tag) {
            if existing.room != room {
                return refuse(Refusal::TagTaken);
            }
            if existing.addr == from {
                // A refresh.
                existing.last_seen = now;
                return self.welcome(from, &room);
            }
            if now.duration_since(existing.last_seen) < MOVE_GRACE {
                return refuse(Refusal::TagTaken);
            }
        }
        if cookie.is_none_or(|c| !self.cookie_valid(&c, from, &room, &tag, now)) {
            return vec![(from, Message::Challenge { cookie: self.cookie(from, &room, &tag, now, 0) }.encode())];
        }

        if let Some(existing) = self.members.get_mut(&tag) {
            // A member whose address changed, now proven: move it.
            let previous = existing.addr;
            if self.per_addr.get(&from).copied().unwrap_or(0) >= self.limits.max_rooms_per_addr {
                return refuse(Refusal::TooManyRooms);
            }
            existing.addr = from;
            existing.last_seen = now;
            self.release_addr(previous);
            *self.per_addr.entry(from).or_default() += 1;
            return self.welcome(from, &room);
        }

        if self.per_addr.get(&from).copied().unwrap_or(0) >= self.limits.max_rooms_per_addr {
            return refuse(Refusal::TooManyRooms);
        }
        match self.rooms.get(&room) {
            Some(members) if members.len() >= self.limits.max_members_per_room => return refuse(Refusal::RoomFull),
            None if self.rooms.len() >= self.limits.max_rooms => return refuse(Refusal::NodeFull),
            _ => {}
        }
        self.rooms.entry(room).or_default().push(tag);
        self.members.insert(tag, Member { room, addr: from, last_seen: now });
        *self.per_addr.entry(from).or_default() += 1;

        let mut out = vec![(from, Message::Joined { members: self.room_size(&room) }.encode())];
        out.extend(self.broadcast_roster(&room));
        out
    }

    /// JOINED and ROSTER, for the joiner alone.
    fn welcome(&self, to: SocketAddr, room: &RoomToken) -> Vec<(SocketAddr, Vec<u8>)> {
        let roster = self.rooms.get(room).cloned().unwrap_or_default();
        vec![
            (to, Message::Joined { members: self.room_size(room) }.encode()),
            (to, Message::Roster(roster).encode()),
        ]
    }

    /// How many members `room` holds, for JOINED.
    fn room_size(&self, room: &RoomToken) -> u16 {
        self.rooms.get(room).map_or(0, |m| m.len()) as u16
    }

    /// The room's roster, to every member in it.
    fn broadcast_roster(&self, room: &RoomToken) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(tags) = self.rooms.get(room) else { return vec![] };
        let bytes = Message::Roster(tags.clone()).encode();
        tags.iter().filter_map(|t| self.members.get(t)).map(|m| (m.addr, bytes.clone())).collect()
    }

    /// Forgets a member, and tells the rest of its room.
    fn remove(&mut self, tag: MemberTag) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(member) = self.members.remove(&tag) else { return vec![] };
        self.release_addr(member.addr);
        let Some(room) = self.rooms.get_mut(&member.room) else { return vec![] };
        room.retain(|t| *t != tag);
        if room.is_empty() {
            self.rooms.remove(&member.room);
            return vec![];
        }
        self.broadcast_roster(&member.room)
    }

    /// Drops one membership from `addr`'s count.
    fn release_addr(&mut self, addr: SocketAddr) {
        if let Some(n) = self.per_addr.get_mut(&addr) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_addr.remove(&addr);
            }
        }
    }

    /// The join cookie for this address, room and tag in the cookie period
    /// `periods_back` before now. Stateless: nothing is held until it returns.
    fn cookie(&self, from: SocketAddr, room: &RoomToken, tag: &MemberTag, now: Instant, periods_back: u64) -> Cookie {
        let period = (now.duration_since(self.started).as_secs() / COOKIE_PERIOD.as_secs()).saturating_sub(periods_back);
        let mut h = Sha256::new();
        h.update(b"antiphon/sfu/cookie/v1");
        h.update(self.secret);
        h.update(period.to_be_bytes());
        match from {
            SocketAddr::V4(a) => h.update(a.ip().octets()),
            SocketAddr::V6(a) => h.update(a.ip().octets()),
        }
        h.update(from.port().to_be_bytes());
        h.update(room);
        h.update(tag);
        let digest = h.finalize();
        let mut cookie = [0u8; COOKIE_LEN];
        cookie.copy_from_slice(&digest[..COOKIE_LEN]);
        cookie
    }

    /// Accepts a cookie from this period or the one before, so one minted just
    /// before a period boundary still works.
    fn cookie_valid(&self, cookie: &Cookie, from: SocketAddr, room: &RoomToken, tag: &MemberTag, now: Instant) -> bool {
        [0, 1].iter().any(|back| self.cookie(from, room, tag, now, *back) == *cookie)
    }

    /// Sweeps at most every five seconds, from the datagram path.
    fn sweep_if_due(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if self.last_sweep.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return vec![];
        }
        self.last_sweep = Some(now);
        self.sweep(now)
    }

    /// Drops every member gone quiet, and returns the roster updates owed
    /// to the rooms they left. Expiry is the retention policy: nothing
    /// outlives it, because nothing is written anywhere else.
    pub fn sweep(&mut self, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let idle: Vec<MemberTag> = self
            .members
            .iter()
            .filter(|(_, m)| now.duration_since(m.last_seen) >= MEMBER_IDLE)
            .map(|(t, _)| *t)
            .collect();
        let mut touched = Vec::new();
        for tag in idle {
            if let Some(m) = self.members.remove(&tag) {
                self.release_addr(m.addr);
                if let Some(room) = self.rooms.get_mut(&m.room) {
                    room.retain(|t| *t != tag);
                    if room.is_empty() {
                        self.rooms.remove(&m.room);
                    } else if !touched.contains(&m.room) {
                        touched.push(m.room);
                    }
                }
            }
        }
        touched.iter().filter(|r| self.rooms.contains_key(*r)).flat_map(|r| self.broadcast_roster(r)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_net::sfu::encode_media;

    /// A private address on `port`.
    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    /// Runs the cookie exchange and returns the final replies.
    fn join(s: &mut Sfu, from: SocketAddr, room: RoomToken, tag: MemberTag, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let out = s.handle(from, &Message::Join { room, member: tag, cookie: None }.encode(), now);
        let cookie = match out.as_slice() {
            [(to, bytes)] if *to == from => match Message::decode(bytes) {
                Some(Message::Challenge { cookie }) => cookie,
                _ => return out,
            },
            _ => return out,
        };
        s.handle(from, &Message::Join { room, member: tag, cookie: Some(cookie) }.encode(), now)
    }

    /// Every SFU message among `out` addressed to `to`, decoded.
    fn decoded(out: &[(SocketAddr, Vec<u8>)], to: SocketAddr) -> Vec<Message> {
        out.iter().filter(|(a, _)| *a == to).filter_map(|(_, b)| Message::decode(b)).collect()
    }

    #[test]
    fn a_join_needs_the_cookie_and_nothing_is_held_before_it() {
        let mut s = Sfu::new(SfuLimits::default());
        let now = Instant::now();
        let out = s.handle(addr(1), &Message::Join { room: [1; 32], member: [1; 16], cookie: None }.encode(), now);
        assert!(matches!(decoded(&out, addr(1)).as_slice(), [Message::Challenge { .. }]));
        assert_eq!(s.stats(), SfuStats::default());
        // A cookie for another address is no good.
        let Some(Message::Challenge { cookie }) = Message::decode(&out[0].1) else { panic!() };
        let out = s.handle(addr(2), &Message::Join { room: [1; 32], member: [1; 16], cookie: Some(cookie) }.encode(), now);
        assert!(matches!(decoded(&out, addr(2)).as_slice(), [Message::Challenge { .. }]));
        assert_eq!(s.stats().members, 0);
    }

    #[test]
    fn joining_announces_the_roster_to_everyone() {
        let mut s = Sfu::new(SfuLimits::default());
        let now = Instant::now();
        let out = join(&mut s, addr(1), [1; 32], [1; 16], now);
        assert_eq!(decoded(&out, addr(1)), vec![Message::Joined { members: 1 }, Message::Roster(vec![[1; 16]])]);
        let out = join(&mut s, addr(2), [1; 32], [2; 16], now);
        assert_eq!(decoded(&out, addr(2)), vec![Message::Joined { members: 2 }, Message::Roster(vec![[1; 16], [2; 16]])]);
        assert_eq!(decoded(&out, addr(1)), vec![Message::Roster(vec![[1; 16], [2; 16]])]);
        assert_eq!(s.stats(), SfuStats { rooms: 1, members: 2, forwarded: 0 });
    }

    #[test]
    fn media_fans_out_to_the_rest_of_the_room_only() {
        let mut s = Sfu::new(SfuLimits::default());
        let now = Instant::now();
        for i in 1..=3u8 {
            join(&mut s, addr(i as u16), [1; 32], [i; 16], now);
        }
        join(&mut s, addr(9), [2; 32], [9; 16], now);
        let frame = encode_media(&[1; 16], b"sealed");
        let mut out = s.handle(addr(1), &frame, now);
        out.sort();
        assert_eq!(out, vec![(addr(2), frame.clone()), (addr(3), frame.clone())]);
        assert_eq!(s.stats().forwarded, 2);
    }

    #[test]
    fn nobody_speaks_for_a_tag_from_another_address() {
        let mut s = Sfu::new(SfuLimits::default());
        let now = Instant::now();
        join(&mut s, addr(1), [1; 32], [1; 16], now);
        join(&mut s, addr(2), [1; 32], [2; 16], now);
        assert!(s.handle(addr(3), &encode_media(&[1; 16], b"spoof"), now).is_empty());
        assert!(s.handle(addr(3), &Message::Leave { member: [1; 16] }.encode(), now).is_empty());
        assert_eq!(s.stats().members, 2);
        assert!(s.handle(addr(1), &encode_media(&[7; 16], b"unknown"), now).is_empty());
    }

    #[test]
    fn a_live_seat_cannot_be_taken_but_a_moved_member_keeps_its_tag() {
        let mut s = Sfu::new(SfuLimits::default());
        let t0 = Instant::now();
        join(&mut s, addr(1), [1; 32], [1; 16], t0);
        join(&mut s, addr(2), [1; 32], [2; 16], t0);
        let out = join(&mut s, addr(3), [1; 32], [1; 16], t0);
        assert_eq!(decoded(&out, addr(3)), vec![Message::Refused(Refusal::TagTaken)]);
        // The same tag in another room is refused too.
        let out = join(&mut s, addr(3), [2; 32], [1; 16], t0);
        assert_eq!(decoded(&out, addr(3)), vec![Message::Refused(Refusal::TagTaken)]);

        // Old address quiet past the grace: a NAT rebinding.
        let later = t0 + MOVE_GRACE + Duration::from_secs(1);
        let out = join(&mut s, addr(4), [1; 32], [1; 16], later);
        assert_eq!(decoded(&out, addr(4))[0], Message::Joined { members: 2 });
        assert_eq!(s.handle(addr(2), &encode_media(&[2; 16], b"x"), later), vec![(addr(4), encode_media(&[2; 16], b"x"))]);
        assert!(s.handle(addr(1), &encode_media(&[1; 16], b"stale"), later).is_empty());
    }

    #[test]
    fn a_refresh_needs_no_cookie_and_returns_the_roster() {
        let mut s = Sfu::new(SfuLimits::default());
        let t0 = Instant::now();
        join(&mut s, addr(1), [1; 32], [1; 16], t0);
        let t1 = t0 + Duration::from_secs(20);
        let out = s.handle(addr(1), &Message::Join { room: [1; 32], member: [1; 16], cookie: None }.encode(), t1);
        assert_eq!(decoded(&out, addr(1)), vec![Message::Joined { members: 1 }, Message::Roster(vec![[1; 16]])]);
        // Kept alive by the refresh, past the original expiry.
        s.sweep(t0 + MEMBER_IDLE + Duration::from_secs(1));
        assert_eq!(s.stats().members, 1);
    }

    #[test]
    fn leaving_and_expiry_tell_the_rest_of_the_room() {
        let mut s = Sfu::new(SfuLimits::default());
        let t0 = Instant::now();
        for i in 1..=3u8 {
            join(&mut s, addr(i as u16), [1; 32], [i; 16], t0);
        }
        let out = s.handle(addr(3), &Message::Leave { member: [3; 16] }.encode(), t0);
        assert_eq!(decoded(&out, addr(1)), vec![Message::Roster(vec![[1; 16], [2; 16]])]);
        assert_eq!(decoded(&out, addr(2)), vec![Message::Roster(vec![[1; 16], [2; 16]])]);

        // Member 1 keeps talking; member 2 goes quiet and expires.
        let t1 = t0 + Duration::from_secs(20);
        s.handle(addr(1), &encode_media(&[1; 16], b"x"), t1);
        let out = s.sweep(t0 + MEMBER_IDLE + Duration::from_secs(1));
        assert_eq!(out, vec![(addr(1), Message::Roster(vec![[1; 16]]).encode())]);

        // ...and an empty room is gone with its last member.
        s.sweep(t1 + MEMBER_IDLE + Duration::from_secs(1));
        assert_eq!(s.stats(), SfuStats { rooms: 0, members: 0, forwarded: 1 });
        assert!(s.per_addr.is_empty());
    }

    #[test]
    fn limits_hold() {
        let mut s = Sfu::new(SfuLimits { max_rooms: 2, max_members_per_room: 2, max_rooms_per_addr: 2 });
        let now = Instant::now();
        join(&mut s, addr(1), [1; 32], [1; 16], now);
        join(&mut s, addr(2), [1; 32], [2; 16], now);
        let out = join(&mut s, addr(3), [1; 32], [3; 16], now);
        assert_eq!(decoded(&out, addr(3)), vec![Message::Refused(Refusal::RoomFull)]);

        join(&mut s, addr(1), [2; 32], [4; 16], now);
        let out = join(&mut s, addr(1), [3; 32], [5; 16], now);
        assert_eq!(decoded(&out, addr(1)), vec![Message::Refused(Refusal::TooManyRooms)]);
        let out = join(&mut s, addr(3), [3; 32], [6; 16], now);
        assert_eq!(decoded(&out, addr(3)), vec![Message::Refused(Refusal::NodeFull)]);
        assert_eq!(s.stats(), SfuStats { rooms: 2, members: 3, forwarded: 0 });
    }

    #[test]
    fn the_member_limit_is_clamped_to_what_a_roster_can_carry() {
        let s = Sfu::new(SfuLimits { max_members_per_room: 10_000, ..SfuLimits::default() });
        assert_eq!(s.limits().max_members_per_room, MAX_ROSTER);
    }

    #[test]
    fn cookies_expire() {
        let mut s = Sfu::new(SfuLimits::default());
        let t0 = Instant::now();
        let out = s.handle(addr(1), &Message::Join { room: [1; 32], member: [1; 16], cookie: None }.encode(), t0);
        let Some(Message::Challenge { cookie }) = Message::decode(&out[0].1) else { panic!() };
        let late = t0 + COOKIE_PERIOD * 3;
        let out = s.handle(addr(1), &Message::Join { room: [1; 32], member: [1; 16], cookie: Some(cookie) }.encode(), late);
        assert!(matches!(decoded(&out, addr(1)).as_slice(), [Message::Challenge { .. }]));
    }
}
