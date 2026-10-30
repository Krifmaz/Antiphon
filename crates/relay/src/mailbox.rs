//! Mailbox service: holding sealed mail for people who are offline (spec
//! §8 "Delivery Routes" and "Server-side opt-in").
//!
//! **Off by default.** The operator switches it on, and sets:
//!
//! - a total storage cap and a per-inbox cap,
//! - a per-inbox deposit rate limit,
//! - how long mail is kept before it expires,
//! - whether the node serves anyone, or only its own community (people
//!   who register with the community's code).
//!
//! What the node holds is exactly: per inbox, a random-looking label, the
//! hash of a fetch secret, and sealed blobs with a time each arrived. No
//! identities, no sender addresses, no contents -- the blobs are sealed
//! end to end and the label is derived per node, so two nodes can't even
//! tell they hold mail for the same person. Writes to labels nobody
//! registered are refused before anything is stored, so a public node
//! can't be filled by strangers. The operator's view is aggregate
//! counters ([`MailboxStats`]) and nothing else.
//!
//! **Push wake-ups** (off unless [`Mailbox::enable_push`] is called, which
//! `antiphon-relay --mailbox-push` does). An inbox's owner may register a
//! push endpoint and push key against their label; when new mail arrives
//! the node seals a `Message` wake-up to that key, signed with the node's
//! own key, and queues it for [`Mailbox::take_wakes`]. What the node learns
//! by this is the endpoint and push key *of a label it already holds*: it
//! still has no identity, no sender and no contents. Two nodes holding
//! inboxes for the same phone could match them up by the shared endpoint,
//! which the per-node label otherwise prevents, so a phone should register
//! its endpoint at one node only (or a separate endpoint per node, which
//! UnifiedPush allows). The node also passes on already sealed wake-ups
//! for senders ([`Message::Wake`]), which it can neither open nor link to
//! anyone. Both are rate-limited, per inbox and per endpoint and address,
//! so a node can't be used to make a phone buzz all day.
//!
//! **Community boards** ([`crate::board`]) are served alongside, on the
//! same port and magic: a community's one-upload-per-message log. Board
//! bytes count toward the node's storage cap together with inbox mail.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use antiphon_net::mailbox::{InboxStatus, Label, MailId, Message, Refusal};
use antiphon_push::{RateLimit, Target, Transport};

use crate::board::{BoardLimits, Boards};

/// How many blobs one fetch hands back before its count. The rest come
/// on the next fetch, after these are acknowledged.
pub const FETCH_BATCH: usize = 32;

#[derive(Clone, Debug)]
pub struct MailboxLimits {
    pub enabled: bool,
    /// Total bytes of mail held, across everyone.
    pub storage_cap: u64,
    /// Bytes any one inbox may hold.
    pub inbox_cap: u64,
    /// Deposits per inbox per minute.
    pub deposits_per_minute: usize,
    /// How long mail is kept if nobody collects it.
    pub mail_expiry: Duration,
    /// How long a registration lasts without being refreshed.
    pub registration_ttl: Duration,
    /// Most inboxes registered at once.
    pub max_inboxes: usize,
    /// `Some` for a node that serves only its own community: registering
    /// takes this code. Depositing never does -- anyone may write to a
    /// member, which is the point of a mailbox.
    pub member_code: Option<Vec<u8>>,
    /// Community boards' own limits.
    pub boards: BoardLimits,
}

impl Default for MailboxLimits {
    fn default() -> Self {
        Self {
            enabled: false,
            storage_cap: 256 * 1024 * 1024,
            inbox_cap: 2 * 1024 * 1024,
            deposits_per_minute: 30,
            mail_expiry: Duration::from_secs(14 * 86_400),
            registration_ttl: Duration::from_secs(7 * 86_400),
            max_inboxes: 100_000,
            member_code: None,
            boards: BoardLimits::default(),
        }
    }
}

/// The operator dashboard: totals only, by construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MailboxStats {
    pub inboxes: usize,
    pub held: usize,
    pub held_bytes: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub refused: u64,
    /// Community boards, and the bytes they hold.
    pub boards: usize,
    pub board_bytes: u64,
}

struct Held {
    id: MailId,
    blob: Vec<u8>,
    arrived: Instant,
}

struct Inbox {
    fetch_hash: [u8; 32],
    registered_until: Instant,
    mail: VecDeque<Held>,
    bytes: u64,
    recent: VecDeque<Instant>,
    /// Where to wake the owner when mail arrives, if they asked.
    push: Option<Target>,
}

/// A wake-up the node owes someone, for whoever drives the node to post.
/// Kept apart from sending so the mailbox stays plain, synchronous and
/// testable, and the HTTP happens off the packet loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeJob {
    /// Mail arrived for an inbox with a push target: seal a `Message`
    /// wake-up to it with the node's key, then post it.
    Seal(Target),
    /// A sender's own sealed wake-up, to post unchanged.
    Forward { transport: Transport, sealed: Vec<u8> },
}

/// Most wake-ups queued at once. Past this they are dropped: the queue is
/// drained every fraction of a second, so a full one means the push
/// service is not keeping up and more would only wait longer.
pub const MAX_PENDING_WAKES: usize = 1024;

/// The node's push settings and limits, present once push is enabled.
struct Push {
    /// The identity key the node signs its own wake-ups with, told to each
    /// owner who registers so their phone can recognise them.
    node_key: [u8; 32],
    /// Whether APNs targets can be served (an Apple key is configured).
    apns: bool,
    /// One mail wake-up per inbox per minute: the phone fetches everything
    /// waiting when it wakes, so more would only drain its battery.
    per_inbox: RateLimit<Label>,
    /// Forwarded wake-ups per endpoint, as a phone would allow calls.
    per_endpoint: RateLimit<String>,
    /// Forwarded wake-ups per sending address, so one sender can't spray
    /// many endpoints through the node.
    per_addr: RateLimit<std::net::IpAddr>,
    /// Waiting for [`Mailbox::take_wakes`].
    pending: Vec<WakeJob>,
}

pub struct Mailbox {
    limits: MailboxLimits,
    inboxes: HashMap<Label, Inbox>,
    held_bytes: u64,
    stats: MailboxStats,
    /// `None` until [`Mailbox::enable_push`]; push requests are refused
    /// with [`Refusal::NoPush`] until then.
    push: Option<Push>,
    boards: Boards,
}

impl Mailbox {
    pub fn new(limits: MailboxLimits) -> Self {
        let boards = Boards::new(limits.boards.clone());
        Self { limits, inboxes: HashMap::new(), held_bytes: 0, stats: MailboxStats::default(), push: None, boards }
    }

    /// Lets owners register push endpoints and senders forward wake-ups.
    /// `node_key` is the public identity key the caller will seal the
    /// node's own wake-ups with; `apns` says whether it can post to APNs,
    /// so APNs endpoints are refused up front rather than accepted and
    /// silently dropped.
    pub fn enable_push(&mut self, node_key: [u8; 32], apns: bool) {
        self.push = Some(Push {
            node_key,
            apns,
            per_inbox: RateLimit::for_mail(),
            per_endpoint: RateLimit::for_calls(),
            per_addr: RateLimit::new(10, Duration::from_secs(6)),
            pending: Vec::new(),
        });
    }

    /// The wake-ups owed since the last call, oldest first.
    pub fn take_wakes(&mut self) -> Vec<WakeJob> {
        self.push.as_mut().map(|p| std::mem::take(&mut p.pending)).unwrap_or_default()
    }

    /// Whether this node can post to `transport` at all.
    fn serves(push: &Push, transport: &Transport) -> bool {
        push.apns || matches!(transport, Transport::UnifiedPush(_))
    }

    /// Queues a job unless the queue is full.
    fn queue(push: &mut Push, job: WakeJob) {
        if push.pending.len() < MAX_PENDING_WAKES {
            push.pending.push(job);
        }
    }

    /// Whether anyone may register, rather than only members.
    pub fn is_open(&self) -> bool {
        self.limits.member_code.is_none()
    }

    pub fn stats(&self) -> MailboxStats {
        MailboxStats {
            inboxes: self.inboxes.len(),
            held: self.inboxes.values().map(|i| i.mail.len()).sum(),
            held_bytes: self.held_bytes,
            boards: self.boards.count(),
            board_bytes: self.boards.bytes(),
            ..self.stats
        }
    }

    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        if let Some(request) = antiphon_net::board::Message::decode(bytes) {
            return self.handle_board(from, bytes.len(), request, now);
        }
        let Some(request) = Message::decode(bytes) else { return Vec::new() };
        self.stats.bytes_in += bytes.len() as u64;
        let replies = if !self.limits.enabled {
            vec![Message::Refused(Refusal::Disabled)]
        } else {
            self.answer(from, request, now)
        };
        replies
            .into_iter()
            .map(|m| {
                if matches!(m, Message::Refused(_)) {
                    self.stats.refused += 1;
                }
                let out = m.encode();
                self.stats.bytes_out += out.len() as u64;
                (from, out)
            })
            .collect()
    }

    /// A community board request, answered by [`Boards`].
    fn handle_board(&mut self, from: SocketAddr, len: usize, request: antiphon_net::board::Message, now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        use antiphon_net::board::Message as Board;
        self.stats.bytes_in += len as u64;
        let replies = if !self.limits.enabled {
            vec![Board::Refused(Refusal::Disabled)]
        } else {
            let room = self.limits.storage_cap.saturating_sub(self.held_bytes + self.boards.bytes());
            self.boards.answer(request, room, now)
        };
        replies
            .into_iter()
            .map(|m| {
                if matches!(m, Board::Refused(_)) {
                    self.stats.refused += 1;
                }
                let out = m.encode();
                self.stats.bytes_out += out.len() as u64;
                (from, out)
            })
            .collect()
    }

    fn answer(&mut self, from: SocketAddr, request: Message, now: Instant) -> Vec<Message> {
        let one = |m| vec![m];
        match request {
            Message::Register { label, fetch_hash, code } => {
                if let Some(expected) = &self.limits.member_code {
                    if !constant_time_eq(expected, &code) {
                        return one(Message::Refused(Refusal::NotAMember));
                    }
                }
                let ttl = self.limits.registration_ttl;
                if let Some(inbox) = self.inboxes.get_mut(&label) {
                    if inbox.fetch_hash != fetch_hash {
                        return one(Message::Refused(Refusal::WrongSecret));
                    }
                    inbox.registered_until = now + ttl;
                } else {
                    if self.inboxes.len() >= self.limits.max_inboxes {
                        return one(Message::Refused(Refusal::Full));
                    }
                    self.inboxes.insert(
                        label,
                        Inbox {
                            fetch_hash,
                            registered_until: now + ttl,
                            mail: VecDeque::new(),
                            bytes: 0,
                            recent: VecDeque::new(),
                            push: None,
                        },
                    );
                }
                one(Message::Registered { ttl_secs: ttl.as_secs().min(u32::MAX as u64) as u32 })
            }
            Message::Deposit { label, id, blob } => {
                let Some(inbox) = self.inboxes.get_mut(&label).filter(|i| i.registered_until > now) else {
                    return one(Message::Refused(Refusal::UnknownLabel));
                };
                // A resend of something already held: say yes again, so a
                // sender whose first acknowledgement was lost stops trying.
                if inbox.mail.iter().any(|h| h.id == id) {
                    return one(Message::Deposited { id });
                }
                while inbox.recent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                    inbox.recent.pop_front();
                }
                if inbox.recent.len() >= self.limits.deposits_per_minute {
                    return one(Message::Refused(Refusal::RateLimited));
                }
                let size = blob.len() as u64;
                if inbox.bytes + size > self.limits.inbox_cap || self.held_bytes + self.boards.bytes() + size > self.limits.storage_cap {
                    return one(Message::Refused(Refusal::Full));
                }
                inbox.recent.push_back(now);
                inbox.bytes += size;
                self.held_bytes += size;
                inbox.mail.push_back(Held { id, blob, arrived: now });
                if let (Some(push), Some(target)) = (self.push.as_mut(), inbox.push.as_ref()) {
                    if push.per_inbox.allow(&label, now) {
                        Self::queue(push, WakeJob::Seal(target.clone()));
                    }
                }
                one(Message::Deposited { id })
            }
            Message::Fetch { label, secret } => match self.owned(&label, &secret) {
                Err(r) => one(Message::Refused(r)),
                Ok(inbox) => {
                    let mut out: Vec<Message> = inbox
                        .mail
                        .iter()
                        .take(FETCH_BATCH)
                        .map(|h| Message::Mail { id: h.id, blob: h.blob.clone() })
                        .collect();
                    out.push(Message::FetchDone { count: inbox.mail.len().min(u16::MAX as usize) as u16 });
                    out
                }
            },
            Message::Ack { label, secret, id } => {
                let freed = match self.owned(&label, &secret) {
                    Err(r) => return one(Message::Refused(r)),
                    Ok(inbox) => match inbox.mail.iter().position(|h| h.id == id) {
                        Some(at) => {
                            let h = inbox.mail.remove(at).expect("position is in range");
                            inbox.bytes -= h.blob.len() as u64;
                            h.blob.len() as u64
                        }
                        None => 0,
                    },
                };
                self.held_bytes -= freed;
                one(Message::Acked { id })
            }
            Message::Status { label, secret } => {
                let cap = self.limits.inbox_cap;
                match self.owned(&label, &secret) {
                    Err(r) => one(Message::Refused(r)),
                    Ok(inbox) => one(Message::StatusReply(InboxStatus {
                        held: inbox.mail.len() as u32,
                        held_bytes: inbox.bytes,
                        cap_bytes: cap,
                    })),
                }
            }
            Message::RegisterPush { label, secret, push_key, endpoint } => {
                let Some(push) = self.push.as_ref() else { return one(Message::Refused(Refusal::NoPush)) };
                let node_key = push.node_key;
                let target = if endpoint.is_empty() {
                    None
                } else {
                    match Transport::parse(&endpoint) {
                        Some(transport) if Self::serves(push, &transport) => Some(Target { push_key, transport }),
                        _ => return one(Message::Refused(Refusal::NoPush)),
                    }
                };
                match self.owned(&label, &secret) {
                    Err(r) => one(Message::Refused(r)),
                    Ok(inbox) => {
                        inbox.push = target;
                        one(Message::PushRegistered { node_key })
                    }
                }
            }
            Message::Wake { code, endpoint, sealed } => {
                if let Some(expected) = &self.limits.member_code {
                    if !constant_time_eq(expected, &code) {
                        return one(Message::Refused(Refusal::NotAMember));
                    }
                }
                let Some(push) = self.push.as_mut() else { return one(Message::Refused(Refusal::NoPush)) };
                let transport = match Transport::parse(&endpoint) {
                    Some(t) if Self::serves(push, &t) => t,
                    _ => return one(Message::Refused(Refusal::NoPush)),
                };
                // Both limits are checked before either is spent, so a
                // refusal by one doesn't use up the other.
                if !push.per_addr.allow(&from.ip(), now) || !push.per_endpoint.allow(&transport.to_text(), now) {
                    return one(Message::Refused(Refusal::RateLimited));
                }
                Self::queue(push, WakeJob::Forward { transport, sealed });
                one(Message::Woken)
            }
            // Replies arriving at the node are noise.
            _ => Vec::new(),
        }
    }

    fn owned(&mut self, label: &Label, secret: &[u8; 32]) -> Result<&mut Inbox, Refusal> {
        let inbox = self.inboxes.get_mut(label).ok_or(Refusal::UnknownLabel)?;
        if !constant_time_eq(&inbox.fetch_hash, &antiphon_crypto::mail::fetch_hash(secret)) {
            return Err(Refusal::WrongSecret);
        }
        Ok(inbox)
    }

    /// Drops expired mail, and inboxes that have lapsed and hold nothing.
    pub fn sweep(&mut self, now: Instant) {
        let expiry = self.limits.mail_expiry;
        let mut freed = 0;
        self.inboxes.retain(|_, inbox| {
            while inbox.mail.front().is_some_and(|h| now.duration_since(h.arrived) > expiry) {
                let h = inbox.mail.pop_front().expect("front exists");
                inbox.bytes -= h.blob.len() as u64;
                freed += h.blob.len() as u64;
            }
            inbox.registered_until > now || !inbox.mail.is_empty()
        });
        self.held_bytes -= freed;
        self.boards.sweep(now);
        if let Some(push) = self.push.as_mut() {
            push.per_inbox.forget_refilled(now);
            push.per_endpoint.forget_refilled(now);
            push.per_addr.forget_refilled(now);
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9);

    fn node() -> Mailbox {
        Mailbox::new(MailboxLimits { enabled: true, ..Default::default() })
    }

    fn ask(m: &mut Mailbox, msg: &Message, now: Instant) -> Vec<Message> {
        m.handle(FROM, &msg.encode(), now).into_iter().map(|(_, b)| Message::decode(&b).unwrap()).collect()
    }

    fn register(m: &mut Mailbox, label: Label, secret: [u8; 32], now: Instant) {
        let fetch_hash = antiphon_crypto::mail::fetch_hash(&secret);
        let r = ask(m, &Message::Register { label, fetch_hash, code: vec![] }, now);
        assert!(matches!(r[0], Message::Registered { .. }), "{r:?}");
    }

    #[test]
    fn off_by_default() {
        let mut m = Mailbox::new(MailboxLimits::default());
        let r = ask(&mut m, &Message::Fetch { label: [0; 32], secret: [0; 32] }, Instant::now());
        assert_eq!(r, vec![Message::Refused(Refusal::Disabled)]);
    }

    #[test]
    fn deposit_fetch_ack() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        let r = ask(&mut m, &Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![9; 100] }, now);
        assert_eq!(r, vec![Message::Deposited { id: [3; 16] }]);
        let r = ask(&mut m, &Message::Fetch { label: [1; 32], secret: [2; 32] }, now);
        assert_eq!(r, vec![Message::Mail { id: [3; 16], blob: vec![9; 100] }, Message::FetchDone { count: 1 }]);
        assert_eq!(m.stats().held_bytes, 100);
        ask(&mut m, &Message::Ack { label: [1; 32], secret: [2; 32], id: [3; 16] }, now);
        assert_eq!(m.stats().held_bytes, 0);
        let r = ask(&mut m, &Message::Fetch { label: [1; 32], secret: [2; 32] }, now);
        assert_eq!(r, vec![Message::FetchDone { count: 0 }]);
    }

    #[test]
    fn strangers_cannot_fill_a_node() {
        let mut m = node();
        let r = ask(&mut m, &Message::Deposit { label: [7; 32], id: [0; 16], blob: vec![1; 500] }, Instant::now());
        assert_eq!(r, vec![Message::Refused(Refusal::UnknownLabel)]);
        assert_eq!(m.stats().held_bytes, 0);
        assert_eq!(m.stats().inboxes, 0);
    }

    #[test]
    fn only_the_fetch_secret_reads_or_deletes() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        ask(&mut m, &Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![9; 10] }, now);
        for wrong in [
            Message::Fetch { label: [1; 32], secret: [9; 32] },
            Message::Ack { label: [1; 32], secret: [9; 32], id: [3; 16] },
            Message::Status { label: [1; 32], secret: [9; 32] },
        ] {
            assert_eq!(ask(&mut m, &wrong, now), vec![Message::Refused(Refusal::WrongSecret)]);
        }
        // Nor can someone else take the label over by re-registering it.
        let hijack = Message::Register { label: [1; 32], fetch_hash: [0; 32], code: vec![] };
        assert_eq!(ask(&mut m, &hijack, now), vec![Message::Refused(Refusal::WrongSecret)]);
        assert_eq!(m.stats().held, 1);
    }

    #[test]
    fn caps_and_rate_limits_hold() {
        let mut m = Mailbox::new(MailboxLimits {
            enabled: true,
            inbox_cap: 250,
            deposits_per_minute: 100,
            ..Default::default()
        });
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        for i in 0..2u8 {
            ask(&mut m, &Message::Deposit { label: [1; 32], id: [i; 16], blob: vec![0; 100] }, now);
        }
        let r = ask(&mut m, &Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 100] }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::Full)]);

        let mut m = Mailbox::new(MailboxLimits { enabled: true, deposits_per_minute: 3, ..Default::default() });
        register(&mut m, [1; 32], [2; 32], now);
        for i in 0..3u8 {
            ask(&mut m, &Message::Deposit { label: [1; 32], id: [i; 16], blob: vec![0; 10] }, now);
        }
        let r = ask(&mut m, &Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 10] }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::RateLimited)]);
        let later = now + Duration::from_secs(61);
        let r = ask(&mut m, &Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 10] }, later);
        assert_eq!(r, vec![Message::Deposited { id: [9; 16] }]);
    }

    #[test]
    fn a_community_node_needs_its_code_to_register() {
        let mut m =
            Mailbox::new(MailboxLimits { enabled: true, member_code: Some(b"guild".to_vec()), ..Default::default() });
        let now = Instant::now();
        let r = ask(&mut m, &Message::Register { label: [1; 32], fetch_hash: [0; 32], code: b"nope".to_vec() }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::NotAMember)]);
        let r = ask(&mut m, &Message::Register { label: [1; 32], fetch_hash: [0; 32], code: b"guild".to_vec() }, now);
        assert!(matches!(r[0], Message::Registered { .. }));
    }

    #[test]
    fn mail_expires_and_lapsed_inboxes_go() {
        let mut m = Mailbox::new(MailboxLimits {
            enabled: true,
            mail_expiry: Duration::from_secs(100),
            registration_ttl: Duration::from_secs(50),
            ..Default::default()
        });
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        ask(&mut m, &Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![0; 10] }, now);
        m.sweep(now + Duration::from_secs(60));
        assert_eq!(m.stats().inboxes, 1, "still holds mail");
        let r = ask(&mut m, &Message::Deposit { label: [1; 32], id: [4; 16], blob: vec![0; 10] }, now + Duration::from_secs(60));
        assert_eq!(r, vec![Message::Refused(Refusal::UnknownLabel)], "lapsed registration takes no new mail");
        m.sweep(now + Duration::from_secs(101));
        assert_eq!(m.stats(), MailboxStats { refused: 1, bytes_in: m.stats().bytes_in, bytes_out: m.stats().bytes_out, ..Default::default() });
    }

    /// A push-enabled node with an inbox registered at label 1, secret 2.
    fn push_node(now: Instant) -> Mailbox {
        let mut m = node();
        m.enable_push([0xEE; 32], false);
        register(&mut m, [1; 32], [2; 32], now);
        m
    }

    /// Without `enable_push`, both push requests are refused, so a node never posts to the web unless its operator asked it to.
    #[test]
    fn push_is_refused_until_enabled() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        let r = ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: "https://h/x".into() }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::NoPush)]);
        let r = ask(&mut m, &Message::Wake { code: vec![], endpoint: "https://h/x".into(), sealed: vec![0; antiphon_net::mailbox::WAKE_LEN] }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::NoPush)]);
    }

    /// The wire format's copy of the sealed size agrees with the crypto crate's.
    #[test]
    fn the_wake_length_matches_the_crypto_crate() {
        assert_eq!(antiphon_net::mailbox::WAKE_LEN, antiphon_crypto::push::SEALED_LEN);
    }

    /// New mail queues one wake-up, and more mail within the minute does not queue another.
    #[test]
    fn mail_wakes_a_registered_owner_once_a_minute() {
        let now = Instant::now();
        let mut m = push_node(now);
        let endpoint = "https://ntfy.sh/upX".to_string();
        let r = ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: endpoint.clone() }, now);
        assert_eq!(r, vec![Message::PushRegistered { node_key: [0xEE; 32] }]);
        for i in 0..3u8 {
            ask(&mut m, &Message::Deposit { label: [1; 32], id: [i; 16], blob: vec![0; 10] }, now);
        }
        let target = Target { push_key: [3; 32], transport: Transport::UnifiedPush(endpoint) };
        assert_eq!(m.take_wakes(), vec![WakeJob::Seal(target.clone())], "three deposits, one wake-up");
        assert!(m.take_wakes().is_empty());
        let later = now + Duration::from_secs(61);
        ask(&mut m, &Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 10] }, later);
        assert_eq!(m.take_wakes(), vec![WakeJob::Seal(target)]);
    }

    /// Setting or clearing the endpoint takes the fetch secret, and a cleared one is not woken.
    #[test]
    fn only_the_owner_sets_or_clears_the_endpoint() {
        let now = Instant::now();
        let mut m = push_node(now);
        let r = ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [9; 32], push_key: [3; 32], endpoint: "https://h/x".into() }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::WrongSecret)]);
        ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: "https://h/x".into() }, now);
        ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: String::new() }, now);
        ask(&mut m, &Message::Deposit { label: [1; 32], id: [1; 16], blob: vec![0; 10] }, now);
        assert!(m.take_wakes().is_empty(), "cleared endpoints are not woken");
    }

    /// APNs endpoints need an Apple key on the node, and unparseable endpoints are never stored.
    #[test]
    fn apns_and_junk_endpoints_are_refused_without_a_key() {
        let now = Instant::now();
        let mut m = push_node(now);
        let voip = format!("apns-voip:{}", "ab".repeat(32));
        for endpoint in [voip.as_str(), "not a url"] {
            let r = ask(&mut m, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: endpoint.into() }, now);
            assert_eq!(r, vec![Message::Refused(Refusal::NoPush)], "{endpoint}");
        }
        let mut with_apns = node();
        with_apns.enable_push([0; 32], true);
        register(&mut with_apns, [1; 32], [2; 32], now);
        let r = ask(&mut with_apns, &Message::RegisterPush { label: [1; 32], secret: [2; 32], push_key: [3; 32], endpoint: voip }, now);
        assert!(matches!(r[0], Message::PushRegistered { .. }));
    }

    /// Forwarded wake-ups are queued unchanged, and an endpoint gets at most a burst of them.
    #[test]
    fn forwarded_wakes_are_queued_and_rate_limited() {
        let now = Instant::now();
        let mut m = push_node(now);
        let wake = |n: u8| Message::Wake { code: vec![], endpoint: "https://h/x".into(), sealed: vec![n; antiphon_net::mailbox::WAKE_LEN] };
        for n in 0..3 {
            assert_eq!(ask(&mut m, &wake(n), now), vec![Message::Woken]);
        }
        assert_eq!(ask(&mut m, &wake(3), now), vec![Message::Refused(Refusal::RateLimited)]);
        let jobs = m.take_wakes();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0], WakeJob::Forward { transport: Transport::UnifiedPush("https://h/x".into()), sealed: vec![0; antiphon_net::mailbox::WAKE_LEN] });
    }

    /// A members-only node forwards only for members, as it registers only members.
    #[test]
    fn a_community_node_wants_its_code_to_forward() {
        let mut m = Mailbox::new(MailboxLimits { enabled: true, member_code: Some(b"guild".to_vec()), ..Default::default() });
        m.enable_push([0; 32], false);
        let msg = |code: &[u8]| Message::Wake { code: code.to_vec(), endpoint: "https://h/x".into(), sealed: vec![0; antiphon_net::mailbox::WAKE_LEN] };
        assert_eq!(ask(&mut m, &msg(b"nope"), Instant::now()), vec![Message::Refused(Refusal::NotAMember)]);
        assert_eq!(ask(&mut m, &msg(b"guild"), Instant::now()), vec![Message::Woken]);
    }

    #[test]
    fn a_resent_deposit_is_held_once() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        for _ in 0..3 {
            ask(&mut m, &Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![0; 10] }, now);
        }
        assert_eq!(m.stats().held, 1);
    }

    #[test]
    fn boards_share_the_port_and_the_storage_cap() {
        use antiphon_net::board::{token_hash, Message as Board};
        let ask_board = |m: &mut Mailbox, msg: Board| -> Board {
            Board::decode(&m.handle(FROM, &msg.encode(), Instant::now()).remove(0).1).unwrap()
        };
        let mut off = Mailbox::new(MailboxLimits::default());
        assert_eq!(ask_board(&mut off, Board::Open { board: [1; 32], token_hash: [2; 32] }), Board::Refused(Refusal::Disabled));

        let mut m = Mailbox::new(MailboxLimits { enabled: true, storage_cap: 1000, ..Default::default() });
        let token = [3; 32];
        assert_eq!(ask_board(&mut m, Board::Open { board: [1; 32], token_hash: token_hash(&token) }), Board::Opened { board: [1; 32] });
        let posted = ask_board(&mut m, Board::Post { board: [1; 32], token, entry: vec![0; 900] });
        assert_eq!(posted, Board::Posted { board: [1; 32], seq: 1 });
        assert_eq!((m.stats().boards, m.stats().board_bytes), (1, 900));
        // The board's bytes leave less room for inbox mail.
        let label = [5; 32];
        ask(&mut m, &Message::Register { label, fetch_hash: antiphon_crypto::mail::fetch_hash(&[6; 32]), code: vec![] }, Instant::now());
        let deposit = ask(&mut m, &Message::Deposit { label, id: [0; 16], blob: vec![0; 200] }, Instant::now());
        assert_eq!(deposit, vec![Message::Refused(Refusal::Full)]);
    }
}
