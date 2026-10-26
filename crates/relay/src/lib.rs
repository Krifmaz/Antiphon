//! A relay server (spec §3 "Everyone else -> Relay", "Community owners can
//! self-host", and "What a relay operator can see -- and the commitment").
//!
//! What it does: pairs two UDP sockets that name the same blinded token,
//! then forwards datagrams between them byte for byte. What it holds, and
//! for how long, is the whole of the privacy story, so it is listed here
//! rather than implied:
//!
//! - **Tokens**, 32 random-looking bytes that rotate daily and that only
//!   the two people involved can compute (`antiphon_crypto::relay_token`).
//!   Never an identity, a name or a key. Dropped [`REGISTRATION_TTL`] after
//!   the listener stops refreshing them.
//! - **Socket addresses**, because routing a packet needs one. Held only
//!   while a registration or a pairing is live, and dropped
//!   [`PAIR_IDLE`] after the last packet.
//! - **Nothing else.** No key ever reaches it -- the datagrams it forwards
//!   are the same sealed frames a direct call carries -- and no log is
//!   written: [`Relay`] has no logging at all, and the binary prints only
//!   aggregate counters, and only when asked to.
//!
//! The core is a plain state machine ([`Relay::handle`]) so it can be
//! tested without sockets; [`run`] is the thin loop around it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use antiphon_net::relay::{Message, RelayToken};
use tokio::net::UdpSocket;

/// How long a registration survives without a refresh. Clients refresh
/// every `antiphon_net::relay::REGISTER_INTERVAL` (20 s).
pub const REGISTRATION_TTL: Duration = Duration::from_secs(60);
/// How long a pairing survives with no traffic either way. A call sends
/// audio every 20 ms, so this only ever fires after a call has ended.
pub const PAIR_IDLE: Duration = Duration::from_secs(120);
/// A pairing counts as a call in progress -- and the listener as busy --
/// while traffic is newer than this.
pub const BUSY_WINDOW: Duration = Duration::from_secs(10);

/// Caps, so a public relay cannot be filled by one misbehaving client.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_registrations: usize,
    pub max_tokens_per_addr: usize,
    pub max_pairs: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_registrations: 200_000, max_tokens_per_addr: 1_024, max_pairs: 50_000 }
    }
}

/// Aggregate counters -- the only thing the relay can report, by design.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub registrations: usize,
    pub pairs: usize,
    pub forwarded: u64,
}

struct Registration {
    owner: SocketAddr,
    expires: Instant,
}

struct Pair {
    other: SocketAddr,
    last_seen: Instant,
}

#[derive(Default)]
pub struct Relay {
    limits: Limits,
    registrations: HashMap<RelayToken, Registration>,
    tokens_per_addr: HashMap<SocketAddr, usize>,
    pairs: HashMap<SocketAddr, Pair>,
    forwarded: u64,
    last_sweep: Option<Instant>,
}

impl Relay {
    pub fn new(limits: Limits) -> Self {
        Self { limits, ..Default::default() }
    }

    pub fn stats(&self) -> Stats {
        Stats { registrations: self.registrations.len(), pairs: self.pairs.len() / 2, forwarded: self.forwarded }
    }

    /// Handles one datagram from `from`, returning what to send where.
    pub fn handle(&mut self, from: SocketAddr, bytes: &[u8], now: Instant) -> Vec<(SocketAddr, Vec<u8>)> {
        self.sweep_if_due(now);

        if !Message::is_relay_datagram(bytes) {
            // Ordinary traffic: forward it if this socket is paired, drop
            // it otherwise. Never inspected -- it is ciphertext anyway.
            let Some(pair) = self.pairs.get_mut(&from) else { return vec![] };
            pair.last_seen = now;
            let to = pair.other;
            if let Some(back) = self.pairs.get_mut(&to) {
                back.last_seen = now;
            }
            self.forwarded += 1;
            return vec![(to, bytes.to_vec())];
        }

        match Message::decode(bytes) {
            Some(Message::Register(tokens)) => {
                let mut accepted = 0u16;
                for token in tokens {
                    if self.register(from, token, now) {
                        accepted += 1;
                    }
                }
                vec![(from, Message::Registered(accepted).encode())]
            }
            Some(Message::Dial(token)) => {
                let reply = if self.dial(from, &token, now) { Message::Dialed } else { Message::NotFound };
                vec![(from, reply.encode())]
            }
            Some(Message::Leave) => {
                self.forget(from);
                vec![]
            }
            // Replies are for clients, and garbage is garbage.
            _ => vec![],
        }
    }

    fn register(&mut self, owner: SocketAddr, token: RelayToken, now: Instant) -> bool {
        let expires = now + REGISTRATION_TTL;
        if let Some(existing) = self.registrations.get_mut(&token) {
            if existing.owner == owner {
                existing.expires = expires;
                return true;
            }
            // Someone else holds it. A token is only computable by the
            // two people involved, so a clash is a moved listener (new
            // NAT mapping) or an attack; either way the newest live
            // registration is the one that can be reached.
            let previous = existing.owner;
            existing.owner = owner;
            existing.expires = expires;
            self.decrement(previous);
            *self.tokens_per_addr.entry(owner).or_default() += 1;
            return true;
        }
        let held = self.tokens_per_addr.get(&owner).copied().unwrap_or(0);
        if held >= self.limits.max_tokens_per_addr || self.registrations.len() >= self.limits.max_registrations {
            return false;
        }
        self.registrations.insert(token, Registration { owner, expires });
        *self.tokens_per_addr.entry(owner).or_default() += 1;
        true
    }

    fn dial(&mut self, caller: SocketAddr, token: &RelayToken, now: Instant) -> bool {
        let Some(registration) = self.registrations.get(token) else { return false };
        if registration.expires <= now {
            return false;
        }
        let listener = registration.owner;
        if listener == caller {
            return false;
        }
        // Already in a call with somebody else: busy. Letting a second
        // dialler take over would cut the first call off.
        if let Some(pair) = self.pairs.get(&listener) {
            if pair.other != caller && now.duration_since(pair.last_seen) < BUSY_WINDOW {
                return false;
            }
        }
        if self.pairs.len() / 2 >= self.limits.max_pairs && !self.pairs.contains_key(&caller) {
            return false;
        }
        self.unpair(caller);
        self.unpair(listener);
        self.pairs.insert(caller, Pair { other: listener, last_seen: now });
        self.pairs.insert(listener, Pair { other: caller, last_seen: now });
        true
    }

    fn unpair(&mut self, addr: SocketAddr) {
        if let Some(pair) = self.pairs.remove(&addr) {
            if self.pairs.get(&pair.other).is_some_and(|p| p.other == addr) {
                self.pairs.remove(&pair.other);
            }
        }
    }

    fn forget(&mut self, addr: SocketAddr) {
        self.unpair(addr);
        self.registrations.retain(|_, r| r.owner != addr);
        self.tokens_per_addr.remove(&addr);
    }

    fn decrement(&mut self, addr: SocketAddr) {
        if let Some(n) = self.tokens_per_addr.get_mut(&addr) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.tokens_per_addr.remove(&addr);
            }
        }
    }

    fn sweep_if_due(&mut self, now: Instant) {
        if self.last_sweep.is_some_and(|t| now.duration_since(t) < Duration::from_secs(5)) {
            return;
        }
        self.last_sweep = Some(now);
        self.sweep(now);
    }

    /// Drops everything expired. Expiry is the retention policy: nothing
    /// outlives it, because nothing is written anywhere else.
    pub fn sweep(&mut self, now: Instant) {
        let expired: Vec<SocketAddr> =
            self.registrations.values().filter(|r| r.expires <= now).map(|r| r.owner).collect();
        self.registrations.retain(|_, r| r.expires > now);
        for owner in expired {
            self.decrement(owner);
        }
        self.pairs.retain(|_, p| now.duration_since(p.last_seen) < PAIR_IDLE);
    }
}

/// Serves `relay` on `socket` until the socket fails.
pub async fn run(socket: Arc<UdpSocket>, relay: Arc<std::sync::Mutex<Relay>>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 65_536];
    loop {
        let (n, from) = socket.recv_from(&mut buf).await?;
        let out = relay.lock().unwrap().handle(from, &buf[..n], Instant::now());
        for (to, bytes) in out {
            // One unreachable peer must not stop the relay for everyone.
            let _ = socket.send_to(&bytes, to).await;
        }
    }
}

/// The operator commitment (spec §3), printed at start-up and in `--help`
/// so it is stated plainly rather than buried in a policy.
pub const COMMITMENT: &str = "\
This relay forwards encrypted packets it cannot read. It never holds a key.
It sees rotating blinded tokens, never who you are. It keeps addresses only
while a call or registration is live, and writes no connection logs.";

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn a_dial_pairs_caller_and_listener_and_traffic_flows_both_ways() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        let (listener, caller, token) = (addr(1), addr(2), [9u8; 32]);

        let out = r.handle(listener, &Message::Register(vec![token]).encode(), now);
        assert_eq!(out, vec![(listener, Message::Registered(1).encode())]);

        let out = r.handle(caller, &Message::Dial(token).encode(), now);
        assert_eq!(out, vec![(caller, Message::Dialed.encode())]);

        assert_eq!(r.handle(caller, b"\x01sealed", now), vec![(listener, b"\x01sealed".to_vec())]);
        assert_eq!(r.handle(listener, b"\x01reply", now), vec![(caller, b"\x01reply".to_vec())]);
        assert_eq!(r.stats().forwarded, 2);
    }

    #[test]
    fn unknown_tokens_and_unpaired_traffic_go_nowhere() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        assert_eq!(r.handle(addr(2), &Message::Dial([1; 32]).encode(), now), vec![(addr(2), Message::NotFound.encode())]);
        assert!(r.handle(addr(2), b"\x01anything", now).is_empty());
    }

    #[test]
    fn registrations_expire_without_a_refresh() {
        let mut r = Relay::new(Limits::default());
        let t0 = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), t0);
        let later = t0 + REGISTRATION_TTL + Duration::from_secs(1);
        let out = r.handle(addr(2), &Message::Dial([5; 32]).encode(), later);
        assert_eq!(out, vec![(addr(2), Message::NotFound.encode())]);
        r.sweep(later);
        assert_eq!(r.stats().registrations, 0);
    }

    #[test]
    fn idle_pairs_are_forgotten() {
        let mut r = Relay::new(Limits::default());
        let t0 = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), t0);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), t0);
        assert_eq!(r.stats().pairs, 1);
        r.sweep(t0 + PAIR_IDLE + Duration::from_secs(1));
        assert_eq!(r.stats().pairs, 0);
        assert!(r.handle(addr(2), b"\x01late", t0 + PAIR_IDLE * 2).is_empty());
    }

    #[test]
    fn a_listener_in_a_call_is_busy_to_a_second_caller() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        let out = r.handle(addr(3), &Message::Dial([5; 32]).encode(), now);
        assert_eq!(out, vec![(addr(3), Message::NotFound.encode())]);
        // ...but once that call has gone quiet, the listener is free.
        let later = now + BUSY_WINDOW + Duration::from_secs(1);
        let out = r.handle(addr(3), &Message::Dial([5; 32]).encode(), later);
        assert_eq!(out, vec![(addr(3), Message::Dialed.encode())]);
    }

    #[test]
    fn per_address_token_cap_holds() {
        let mut r = Relay::new(Limits { max_tokens_per_addr: 2, ..Limits::default() });
        let out = r.handle(addr(1), &Message::Register(vec![[1; 32], [2; 32], [3; 32]]).encode(), Instant::now());
        assert_eq!(out, vec![(addr(1), Message::Registered(2).encode())]);
    }

    #[test]
    fn leave_forgets_everything_about_an_address() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        r.handle(addr(1), &Message::Leave.encode(), now);
        assert_eq!(r.stats(), Stats { registrations: 0, pairs: 0, forwarded: 0 });
    }

    #[test]
    fn a_moved_listener_takes_its_token_with_it() {
        let mut r = Relay::new(Limits::default());
        let now = Instant::now();
        r.handle(addr(1), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(7), &Message::Register(vec![[5; 32]]).encode(), now);
        r.handle(addr(2), &Message::Dial([5; 32]).encode(), now);
        assert_eq!(r.handle(addr(2), b"\x01x", now), vec![(addr(7), b"\x01x".to_vec())]);
    }
}
