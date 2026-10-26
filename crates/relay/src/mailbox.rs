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

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use antiphon_net::mailbox::{InboxStatus, Label, MailId, Message, Refusal};

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
}

pub struct Mailbox {
    limits: MailboxLimits,
    inboxes: HashMap<Label, Inbox>,
    held_bytes: u64,
    stats: MailboxStats,
}

impl Mailbox {
    pub fn new(limits: MailboxLimits) -> Self {
        Self { limits, inboxes: HashMap::new(), held_bytes: 0, stats: MailboxStats::default() }
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
            ..self.stats
        }
    }

    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        let Some(request) = Message::decode(bytes) else { return Vec::new() };
        self.stats.bytes_in += bytes.len() as u64;
        let replies = if !self.limits.enabled {
            vec![Message::Refused(Refusal::Disabled)]
        } else {
            self.answer(request, now)
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

    fn answer(&mut self, request: Message, now: Instant) -> Vec<Message> {
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
                if inbox.bytes + size > self.limits.inbox_cap || self.held_bytes + size > self.limits.storage_cap {
                    return one(Message::Refused(Refusal::Full));
                }
                inbox.recent.push_back(now);
                inbox.bytes += size;
                self.held_bytes += size;
                inbox.mail.push_back(Held { id, blob, arrived: now });
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

    fn ask(m: &mut Mailbox, msg: Message, now: Instant) -> Vec<Message> {
        m.handle(FROM, &msg.encode(), now).into_iter().map(|(_, b)| Message::decode(&b).unwrap()).collect()
    }

    fn register(m: &mut Mailbox, label: Label, secret: [u8; 32], now: Instant) {
        let fetch_hash = antiphon_crypto::mail::fetch_hash(&secret);
        let r = ask(m, Message::Register { label, fetch_hash, code: vec![] }, now);
        assert!(matches!(r[0], Message::Registered { .. }), "{r:?}");
    }

    #[test]
    fn off_by_default() {
        let mut m = Mailbox::new(MailboxLimits::default());
        let r = ask(&mut m, Message::Fetch { label: [0; 32], secret: [0; 32] }, Instant::now());
        assert_eq!(r, vec![Message::Refused(Refusal::Disabled)]);
    }

    #[test]
    fn deposit_fetch_ack() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        let r = ask(&mut m, Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![9; 100] }, now);
        assert_eq!(r, vec![Message::Deposited { id: [3; 16] }]);
        let r = ask(&mut m, Message::Fetch { label: [1; 32], secret: [2; 32] }, now);
        assert_eq!(r, vec![Message::Mail { id: [3; 16], blob: vec![9; 100] }, Message::FetchDone { count: 1 }]);
        assert_eq!(m.stats().held_bytes, 100);
        ask(&mut m, Message::Ack { label: [1; 32], secret: [2; 32], id: [3; 16] }, now);
        assert_eq!(m.stats().held_bytes, 0);
        let r = ask(&mut m, Message::Fetch { label: [1; 32], secret: [2; 32] }, now);
        assert_eq!(r, vec![Message::FetchDone { count: 0 }]);
    }

    #[test]
    fn strangers_cannot_fill_a_node() {
        let mut m = node();
        let r = ask(&mut m, Message::Deposit { label: [7; 32], id: [0; 16], blob: vec![1; 500] }, Instant::now());
        assert_eq!(r, vec![Message::Refused(Refusal::UnknownLabel)]);
        assert_eq!(m.stats().held_bytes, 0);
        assert_eq!(m.stats().inboxes, 0);
    }

    #[test]
    fn only_the_fetch_secret_reads_or_deletes() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        ask(&mut m, Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![9; 10] }, now);
        for wrong in [
            Message::Fetch { label: [1; 32], secret: [9; 32] },
            Message::Ack { label: [1; 32], secret: [9; 32], id: [3; 16] },
            Message::Status { label: [1; 32], secret: [9; 32] },
        ] {
            assert_eq!(ask(&mut m, wrong, now), vec![Message::Refused(Refusal::WrongSecret)]);
        }
        // Nor can someone else take the label over by re-registering it.
        let hijack = Message::Register { label: [1; 32], fetch_hash: [0; 32], code: vec![] };
        assert_eq!(ask(&mut m, hijack, now), vec![Message::Refused(Refusal::WrongSecret)]);
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
            ask(&mut m, Message::Deposit { label: [1; 32], id: [i; 16], blob: vec![0; 100] }, now);
        }
        let r = ask(&mut m, Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 100] }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::Full)]);

        let mut m = Mailbox::new(MailboxLimits { enabled: true, deposits_per_minute: 3, ..Default::default() });
        register(&mut m, [1; 32], [2; 32], now);
        for i in 0..3u8 {
            ask(&mut m, Message::Deposit { label: [1; 32], id: [i; 16], blob: vec![0; 10] }, now);
        }
        let r = ask(&mut m, Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 10] }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::RateLimited)]);
        let later = now + Duration::from_secs(61);
        let r = ask(&mut m, Message::Deposit { label: [1; 32], id: [9; 16], blob: vec![0; 10] }, later);
        assert_eq!(r, vec![Message::Deposited { id: [9; 16] }]);
    }

    #[test]
    fn a_community_node_needs_its_code_to_register() {
        let mut m =
            Mailbox::new(MailboxLimits { enabled: true, member_code: Some(b"guild".to_vec()), ..Default::default() });
        let now = Instant::now();
        let r = ask(&mut m, Message::Register { label: [1; 32], fetch_hash: [0; 32], code: b"nope".to_vec() }, now);
        assert_eq!(r, vec![Message::Refused(Refusal::NotAMember)]);
        let r = ask(&mut m, Message::Register { label: [1; 32], fetch_hash: [0; 32], code: b"guild".to_vec() }, now);
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
        ask(&mut m, Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![0; 10] }, now);
        m.sweep(now + Duration::from_secs(60));
        assert_eq!(m.stats().inboxes, 1, "still holds mail");
        let r = ask(&mut m, Message::Deposit { label: [1; 32], id: [4; 16], blob: vec![0; 10] }, now + Duration::from_secs(60));
        assert_eq!(r, vec![Message::Refused(Refusal::UnknownLabel)], "lapsed registration takes no new mail");
        m.sweep(now + Duration::from_secs(101));
        assert_eq!(m.stats(), MailboxStats { refused: 1, bytes_in: m.stats().bytes_in, bytes_out: m.stats().bytes_out, ..Default::default() });
    }

    #[test]
    fn a_resent_deposit_is_held_once() {
        let mut m = node();
        let now = Instant::now();
        register(&mut m, [1; 32], [2; 32], now);
        for _ in 0..3 {
            ask(&mut m, Message::Deposit { label: [1; 32], id: [3; 16], blob: vec![0; 10] }, now);
        }
        assert_eq!(m.stats().held, 1);
    }
}
