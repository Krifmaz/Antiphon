//! The WireGuard protocol itself: the Noise_IKpsk2 handshake, the
//! transport-data packets and the timers that keep a session alive
//! (spec §6 "userspace WireGuard inside the app").
//!
//! Written to the whitepaper (Donenfeld, "WireGuard: Next Generation Kernel
//! Network Tunnel", §5 and §6) rather than taken from boringtun, because
//! boringtun pulls in `ring`, whose C build does not cross-compile to
//! Windows here. Interoperability with boringtun was checked out of tree
//! by handshaking and exchanging data with it on loopback.
//!
//! One peer per tunnel. A "bring your own VPN" file from a provider names
//! exactly one server; a mesh of peers is a different product. Sans-IO: the
//! caller owns the socket and the clock, which is what makes this testable
//! without one.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake2::digest::consts::U16;
use blake2::digest::{Digest, FixedOutput, KeyInit, Update};
use blake2::{Blake2s256, Blake2sMac};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, XChaCha20Poly1305, XNonce};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

const CONSTRUCTION: &[u8] = b"Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
const IDENTIFIER: &[u8] = b"WireGuard v1 zx2c4 Jason@zx2c4.com";
const LABEL_MAC1: &[u8] = b"mac1----";
const LABEL_COOKIE: &[u8] = b"cookie--";

const TYPE_INIT: u8 = 1;
const TYPE_RESPONSE: u8 = 2;
const TYPE_COOKIE: u8 = 3;
const TYPE_DATA: u8 = 4;

const INIT_LEN: usize = 148;
const RESPONSE_LEN: usize = 92;
const COOKIE_LEN: usize = 64;
const DATA_HEADER: usize = 16;
const TAG: usize = 16;

// Whitepaper §6.1 timers.
const REKEY_AFTER_MESSAGES: u64 = 1 << 60;
const REJECT_AFTER_MESSAGES: u64 = u64::MAX - (1 << 13);
const REKEY_AFTER_TIME: Duration = Duration::from_secs(120);
const REJECT_AFTER_TIME: Duration = Duration::from_secs(180);
const REKEY_ATTEMPT_TIME: Duration = Duration::from_secs(90);
const REKEY_TIMEOUT: Duration = Duration::from_secs(5);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const COOKIE_LIFETIME: Duration = Duration::from_secs(120);

/// Packets that queue while a handshake is in flight. Small: this is voice
/// and short control messages, and anything older than a handshake round
/// trip is stale anyway.
const MAX_QUEUED: usize = 64;

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Blake2s256::new();
    for p in parts {
        Digest::update(&mut h, p);
    }
    h.finalize().into()
}

fn mac(key: &[u8], data: &[u8]) -> [u8; 16] {
    let mut m = <Blake2sMac<U16> as KeyInit>::new_from_slice(key).expect("key is at most 32 bytes");
    Update::update(&mut m, data);
    m.finalize_fixed().into()
}

/// HMAC-BLAKE2s, written out: BLAKE2s has a 64-byte block and every key
/// here is 32 bytes, so it is the textbook construction with no edge cases.
fn hmac(key: &[u8; 32], data: &[&[u8]]) -> [u8; 32] {
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..32 {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner_parts: Vec<&[u8]> = vec![&ipad];
    inner_parts.extend_from_slice(data);
    let inner = hash(&inner_parts);
    hash(&[&opad, &inner])
}

fn kdf<const N: usize>(key: &[u8; 32], input: &[u8]) -> [[u8; 32]; N] {
    let t0 = Zeroizing::new(hmac(key, &[input]));
    let mut out = [[0u8; 32]; N];
    let mut prev: Vec<u8> = Vec::new();
    for (i, slot) in out.iter_mut().enumerate() {
        let counter = [(i + 1) as u8];
        *slot = hmac(&t0, &[&prev, &counter]);
        prev = slot.to_vec();
    }
    out
}

fn aead_nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    Nonce::from(n)
}

fn seal(key: &[u8; 32], counter: u64, plain: &[u8], aad: &[u8]) -> Vec<u8> {
    let cipher = <ChaCha20Poly1305 as KeyInit>::new(&Key::from(*key));
    cipher.encrypt(&aead_nonce(counter), Payload { msg: plain, aad }).expect("sealing cannot fail")
}

fn open(key: &[u8; 32], counter: u64, sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
    let cipher = <ChaCha20Poly1305 as KeyInit>::new(&Key::from(*key));
    cipher.decrypt(&aead_nonce(counter), Payload { msg: sealed, aad }).ok()
}

fn dh(secret: &StaticSecret, public: &[u8; 32]) -> Zeroizing<[u8; 32]> {
    Zeroizing::new(secret.diffie_hellman(&PublicKey::from(*public)).to_bytes())
}

fn tai64n() -> [u8; 12] {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&(0x4000_0000_0000_000au64 + now.as_secs()).to_be_bytes());
    out[8..].copy_from_slice(&now.subsec_nanos().to_be_bytes());
    out
}

fn random_index() -> u32 {
    let mut b = [0u8; 4];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    u32::from_le_bytes(b)
}

fn random_secret() -> StaticSecret {
    let mut seed = Zeroizing::new([0u8; 32]);
    getrandom::fill(seed.as_mut()).expect("OS randomness is unavailable");
    StaticSecret::from(*seed)
}

/// The public key for a WireGuard private key, as `wg pubkey` would print it.
pub fn public_key(private: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*private)).to_bytes()
}

/// Rejects replays and very late packets: a 2048-counter sliding window
/// (RFC 6479's bitmap), so a little reordering on the path is tolerated.
struct ReplayWindow {
    greatest: u64,
    seen_any: bool,
    bitmap: [u64; Self::WORDS],
}

impl ReplayWindow {
    const WORDS: usize = 32;
    const BITS: u64 = (Self::WORDS as u64) * 64;

    fn new() -> Self {
        Self { greatest: 0, seen_any: false, bitmap: [0; Self::WORDS] }
    }

    fn would_accept(&self, n: u64) -> bool {
        if n >= REJECT_AFTER_MESSAGES {
            return false;
        }
        if !self.seen_any || n > self.greatest {
            return true;
        }
        if self.greatest - n >= Self::BITS - 64 {
            return false;
        }
        let word = ((n / 64) % Self::WORDS as u64) as usize;
        self.bitmap[word] & (1 << (n % 64)) == 0
    }

    fn mark(&mut self, n: u64) {
        let index = n / 64;
        if !self.seen_any {
            self.bitmap = [0; Self::WORDS];
            self.greatest = n;
            self.seen_any = true;
        } else if n > self.greatest {
            let current = self.greatest / 64;
            let diff = (index - current).min(Self::WORDS as u64);
            for i in 1..=diff {
                self.bitmap[((current + i) % Self::WORDS as u64) as usize] = 0;
            }
            self.greatest = n;
        }
        self.bitmap[(index % Self::WORDS as u64) as usize] |= 1 << (n % 64);
    }
}

struct Keypair {
    local_index: u32,
    remote_index: u32,
    send: Zeroizing<[u8; 32]>,
    recv: Zeroizing<[u8; 32]>,
    send_counter: u64,
    replay: ReplayWindow,
    created: Instant,
    initiator: bool,
    /// A responder may not send on a session until the initiator has used
    /// it (whitepaper §5.4.7): that first packet is what confirms the
    /// initiator holds the same keys.
    confirmed: bool,
}

struct PendingInit {
    local_index: u32,
    chaining: Zeroizing<[u8; 32]>,
    hash: [u8; 32],
    ephemeral: StaticSecret,
    sent_at: Instant,
    mac1: [u8; 16],
}

/// What a received datagram produced.
#[derive(Default, Debug)]
pub struct Decapsulated {
    /// Datagrams to send back to the peer (a handshake response, packets
    /// that were waiting for this handshake, a keepalive).
    pub to_network: Vec<Vec<u8>>,
    /// The decrypted inner IP packet, if this was data (not a keepalive).
    pub to_tunnel: Option<Vec<u8>>,
}

/// One end of a WireGuard tunnel to one peer.
pub struct WireGuard {
    static_private: StaticSecret,
    static_public: [u8; 32],
    peer_public: [u8; 32],
    psk: Zeroizing<[u8; 32]>,
    /// HASH(LABEL_MAC1 || peer public): checks what we send.
    mac1_key_peer: [u8; 32],
    /// HASH(LABEL_MAC1 || our public): checks what we receive.
    mac1_key_self: [u8; 32],
    persistent_keepalive: Option<Duration>,

    pending: Option<PendingInit>,
    /// When the first handshake attempt of the current run started, so a
    /// peer that never answers is given up on after REKEY_ATTEMPT_TIME.
    attempt_started: Option<Instant>,
    /// Newest first; at most three (current, previous, and a responder
    /// session not yet confirmed).
    keypairs: Vec<Keypair>,
    last_timestamp: [u8; 12],
    cookie: Option<([u8; 16], Instant)>,
    queue: Vec<Vec<u8>>,
    last_sent: Option<Instant>,
    last_received: Option<Instant>,
    last_handshake: Option<Instant>,
}

impl WireGuard {
    pub fn new(private: &[u8; 32], peer_public: [u8; 32], psk: Option<[u8; 32]>, keepalive: Option<Duration>) -> Self {
        let static_private = StaticSecret::from(*private);
        let static_public = PublicKey::from(&static_private).to_bytes();
        Self {
            static_private,
            static_public,
            peer_public,
            psk: Zeroizing::new(psk.unwrap_or([0; 32])),
            mac1_key_peer: hash(&[LABEL_MAC1, &peer_public]),
            mac1_key_self: hash(&[LABEL_MAC1, &static_public]),
            persistent_keepalive: keepalive.filter(|d| !d.is_zero()),
            pending: None,
            attempt_started: None,
            keypairs: Vec::new(),
            last_timestamp: [0; 12],
            cookie: None,
            queue: Vec::new(),
            last_sent: None,
            last_received: None,
            last_handshake: None,
        }
    }

    /// When the last handshake completed -- what "the tunnel is up" means.
    pub fn last_handshake(&self) -> Option<Instant> {
        self.last_handshake
    }

    /// A session usable for sending right now.
    fn sending_keypair(&mut self, now: Instant) -> Option<&mut Keypair> {
        self.keypairs.iter_mut().find(|k| {
            k.confirmed && now.duration_since(k.created) < REJECT_AFTER_TIME && k.send_counter < REJECT_AFTER_MESSAGES
        })
    }

    fn add_macs(&self, msg: &mut [u8], now: Instant) {
        let len = msg.len();
        let m1 = mac(&self.mac1_key_peer, &msg[..len - 32]);
        msg[len - 32..len - 16].copy_from_slice(&m1);
        let m2 = match self.cookie {
            Some((cookie, at)) if now.duration_since(at) < COOKIE_LIFETIME => mac(&cookie, &msg[..len - 16]),
            _ => [0; 16],
        };
        msg[len - 16..].copy_from_slice(&m2);
    }

    /// Builds a handshake initiation (whitepaper §5.4.2).
    pub fn initiate(&mut self, now: Instant) -> Vec<u8> {
        let ephemeral = random_secret();
        let e_pub = PublicKey::from(&ephemeral).to_bytes();
        let local_index = random_index();

        let mut c = Zeroizing::new(hash(&[CONSTRUCTION]));
        let mut h = hash(&[&c[..], IDENTIFIER]);
        h = hash(&[&h, &self.peer_public]);
        *c = kdf::<1>(&c, &e_pub)[0];
        h = hash(&[&h, &e_pub]);
        let [c1, k] = kdf::<2>(&c, &dh(&ephemeral, &self.peer_public)[..]);
        *c = c1;
        let enc_static = seal(&k, 0, &self.static_public, &h);
        h = hash(&[&h, &enc_static]);
        let ss = Zeroizing::new(self.static_private.diffie_hellman(&PublicKey::from(self.peer_public)).to_bytes());
        let [c2, k] = kdf::<2>(&c, &ss[..]);
        *c = c2;
        let enc_time = seal(&k, 0, &tai64n(), &h);
        h = hash(&[&h, &enc_time]);

        let mut msg = Vec::with_capacity(INIT_LEN);
        msg.extend_from_slice(&[TYPE_INIT, 0, 0, 0]);
        msg.extend_from_slice(&local_index.to_le_bytes());
        msg.extend_from_slice(&e_pub);
        msg.extend_from_slice(&enc_static);
        msg.extend_from_slice(&enc_time);
        msg.extend_from_slice(&[0; 32]);
        self.add_macs(&mut msg, now);

        let mut mac1 = [0u8; 16];
        mac1.copy_from_slice(&msg[INIT_LEN - 32..INIT_LEN - 16]);
        self.pending = Some(PendingInit { local_index, chaining: c, hash: h, ephemeral, sent_at: now, mac1 });
        self.attempt_started.get_or_insert(now);
        self.last_sent = Some(now);
        msg
    }

    fn data_packet(kp: &mut Keypair, inner: &[u8]) -> Vec<u8> {
        // Padded to a multiple of 16 so packet sizes leak less.
        let padded_len = inner.len().div_ceil(16) * 16;
        let mut plain = Vec::with_capacity(padded_len);
        plain.extend_from_slice(inner);
        plain.resize(padded_len, 0);
        let counter = kp.send_counter;
        kp.send_counter += 1;
        let sealed = seal(&kp.send, counter, &plain, &[]);
        let mut out = Vec::with_capacity(DATA_HEADER + sealed.len());
        out.extend_from_slice(&[TYPE_DATA, 0, 0, 0]);
        out.extend_from_slice(&kp.remote_index.to_le_bytes());
        out.extend_from_slice(&counter.to_le_bytes());
        out.extend_from_slice(&sealed);
        out
    }

    /// Encrypts one inner IP packet. With no session yet, the packet waits
    /// and a handshake initiation goes out instead.
    pub fn encapsulate(&mut self, inner: &[u8], now: Instant) -> Vec<Vec<u8>> {
        if let Some(kp) = self.sending_keypair(now) {
            let needs_rekey = kp.initiator
                && (now.duration_since(kp.created) >= REKEY_AFTER_TIME || kp.send_counter >= REKEY_AFTER_MESSAGES);
            let packet = Self::data_packet(kp, inner);
            self.last_sent = Some(now);
            let mut out = vec![packet];
            if needs_rekey && self.pending.is_none() {
                out.push(self.initiate(now));
            }
            return out;
        }
        if self.queue.len() >= MAX_QUEUED {
            self.queue.remove(0);
        }
        self.queue.push(inner.to_vec());
        match &self.pending {
            Some(p) if now.duration_since(p.sent_at) < REKEY_TIMEOUT => Vec::new(),
            _ => vec![self.initiate(now)],
        }
    }

    fn flush_queue(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let queued = std::mem::take(&mut self.queue);
        let mut out = Vec::new();
        if let Some(kp) = self.sending_keypair(now) {
            for inner in &queued {
                out.push(Self::data_packet(kp, inner));
            }
        }
        if !out.is_empty() {
            self.last_sent = Some(now);
        }
        out
    }

    fn install(&mut self, kp: Keypair, now: Instant) {
        self.keypairs.insert(0, kp);
        self.keypairs.truncate(3);
        self.last_handshake = Some(now);
        self.attempt_started = None;
    }

    /// Handles one datagram from the peer.
    pub fn decapsulate(&mut self, datagram: &[u8], now: Instant) -> Decapsulated {
        let mut out = Decapsulated::default();
        let Some(&kind) = datagram.first() else { return out };
        if datagram.len() < 4 || datagram[1..4] != [0, 0, 0] {
            return out;
        }
        match kind {
            TYPE_INIT if datagram.len() == INIT_LEN => {
                if let Some(reply) = self.consume_init(datagram, now) {
                    self.last_sent = Some(now);
                    out.to_network.push(reply);
                }
            }
            TYPE_RESPONSE if datagram.len() == RESPONSE_LEN => {
                if self.consume_response(datagram, now) {
                    let mut flushed = self.flush_queue(now);
                    if flushed.is_empty() {
                        // Confirms the session to the responder, which may
                        // not send until it hears from us on it.
                        if let Some(kp) = self.sending_keypair(now) {
                            flushed.push(Self::data_packet(kp, &[]));
                            self.last_sent = Some(now);
                        }
                    }
                    out.to_network = flushed;
                }
            }
            TYPE_COOKIE if datagram.len() == COOKIE_LEN => self.consume_cookie(datagram, now),
            TYPE_DATA if datagram.len() >= DATA_HEADER + TAG => {
                let receiver = u32::from_le_bytes(datagram[4..8].try_into().unwrap());
                let counter = u64::from_le_bytes(datagram[8..16].try_into().unwrap());
                let Some(pos) = self.keypairs.iter().position(|k| k.local_index == receiver) else { return out };
                let kp = &mut self.keypairs[pos];
                if now.duration_since(kp.created) >= REJECT_AFTER_TIME || !kp.replay.would_accept(counter) {
                    return out;
                }
                let Some(plain) = open(&kp.recv, counter, &datagram[DATA_HEADER..], &[]) else { return out };
                kp.replay.mark(counter);
                let newly_confirmed = !kp.confirmed;
                kp.confirmed = true;
                self.last_received = Some(now);
                if newly_confirmed {
                    // Move it to the front: it is now the session to send on.
                    let kp = self.keypairs.remove(pos);
                    self.keypairs.insert(0, kp);
                    out.to_network.extend(self.flush_queue(now));
                }
                if !plain.is_empty() {
                    out.to_tunnel = Some(trim_padding(plain));
                }
            }
            _ => {}
        }
        out
    }

    fn consume_init(&mut self, msg: &[u8], now: Instant) -> Option<Vec<u8>> {
        if mac(&self.mac1_key_self, &msg[..INIT_LEN - 32]) != msg[INIT_LEN - 32..INIT_LEN - 16] {
            return None;
        }
        let sender = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        let e_pub: [u8; 32] = msg[8..40].try_into().unwrap();

        let mut c = Zeroizing::new(hash(&[CONSTRUCTION]));
        let mut h = hash(&[&c[..], IDENTIFIER]);
        h = hash(&[&h, &self.static_public]);
        *c = kdf::<1>(&c, &e_pub)[0];
        h = hash(&[&h, &e_pub]);
        let [c1, k] = kdf::<2>(&c, &dh(&self.static_private, &e_pub)[..]);
        *c = c1;
        let initiator_static = open(&k, 0, &msg[40..88], &h)?;
        if initiator_static.as_slice() != self.peer_public {
            return None;
        }
        h = hash(&[&h, &msg[40..88]]);
        let [c2, k] = kdf::<2>(&c, &dh(&self.static_private, &self.peer_public)[..]);
        *c = c2;
        let timestamp = open(&k, 0, &msg[88..116], &h)?;
        let timestamp: [u8; 12] = timestamp.try_into().ok()?;
        // Byte order is big-endian, so lexical comparison is numeric: an
        // initiation no newer than the last one is a replay.
        if timestamp <= self.last_timestamp {
            return None;
        }
        self.last_timestamp = timestamp;
        h = hash(&[&h, &msg[88..116]]);

        // Response (whitepaper §5.4.3).
        let ephemeral = random_secret();
        let er_pub = PublicKey::from(&ephemeral).to_bytes();
        let local_index = random_index();
        *c = kdf::<1>(&c, &er_pub)[0];
        h = hash(&[&h, &er_pub]);
        *c = kdf::<1>(&c, &dh(&ephemeral, &e_pub)[..])[0];
        *c = kdf::<1>(&c, &dh(&ephemeral, &self.peer_public)[..])[0];
        let [c3, tau, k] = kdf::<3>(&c, &self.psk[..]);
        *c = c3;
        h = hash(&[&h, &tau]);
        let empty = seal(&k, 0, &[], &h);

        let mut reply = Vec::with_capacity(RESPONSE_LEN);
        reply.extend_from_slice(&[TYPE_RESPONSE, 0, 0, 0]);
        reply.extend_from_slice(&local_index.to_le_bytes());
        reply.extend_from_slice(&sender.to_le_bytes());
        reply.extend_from_slice(&er_pub);
        reply.extend_from_slice(&empty);
        reply.extend_from_slice(&[0; 32]);
        self.add_macs(&mut reply, now);

        let [recv, send] = kdf::<2>(&c, &[]);
        self.install(
            Keypair {
                local_index,
                remote_index: sender,
                send: Zeroizing::new(send),
                recv: Zeroizing::new(recv),
                send_counter: 0,
                replay: ReplayWindow::new(),
                created: now,
                initiator: false,
                confirmed: false,
            },
            now,
        );
        Some(reply)
    }

    fn consume_response(&mut self, msg: &[u8], now: Instant) -> bool {
        if mac(&self.mac1_key_self, &msg[..RESPONSE_LEN - 32]) != msg[RESPONSE_LEN - 32..RESPONSE_LEN - 16] {
            return false;
        }
        let sender = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        let receiver = u32::from_le_bytes(msg[8..12].try_into().unwrap());
        let Some(pending) = self.pending.as_ref() else { return false };
        if pending.local_index != receiver {
            return false;
        }
        let er_pub: [u8; 32] = msg[12..44].try_into().unwrap();
        let mut c = Zeroizing::new(*pending.chaining);
        let mut h = pending.hash;
        *c = kdf::<1>(&c, &er_pub)[0];
        h = hash(&[&h, &er_pub]);
        *c = kdf::<1>(&c, &dh(&pending.ephemeral, &er_pub)[..])[0];
        *c = kdf::<1>(&c, &dh(&self.static_private, &er_pub)[..])[0];
        let [c3, tau, k] = kdf::<3>(&c, &self.psk[..]);
        *c = c3;
        h = hash(&[&h, &tau]);
        if open(&k, 0, &msg[44..60], &h).is_none() {
            return false;
        }
        let [send, recv] = kdf::<2>(&c, &[]);
        let local_index = pending.local_index;
        self.pending = None;
        self.install(
            Keypair {
                local_index,
                remote_index: sender,
                send: Zeroizing::new(send),
                recv: Zeroizing::new(recv),
                send_counter: 0,
                replay: ReplayWindow::new(),
                created: now,
                initiator: true,
                confirmed: true,
            },
            now,
        );
        self.last_received = Some(now);
        true
    }

    /// A server under load answers with a cookie instead of a response
    /// (whitepaper §5.4.7); the next initiation must carry it as mac2.
    fn consume_cookie(&mut self, msg: &[u8], now: Instant) {
        let receiver = u32::from_le_bytes(msg[4..8].try_into().unwrap());
        let Some(pending) = &self.pending else { return };
        if pending.local_index != receiver {
            return;
        }
        let key = hash(&[LABEL_COOKIE, &self.peer_public]);
        let cipher = <XChaCha20Poly1305 as KeyInit>::new(&Key::from(key));
        let nonce = XNonce::from(<[u8; 24]>::try_from(&msg[8..32]).unwrap());
        let Ok(cookie) = cipher.decrypt(&nonce, Payload { msg: &msg[32..64], aad: &pending.mac1 }) else { return };
        if let Ok(cookie) = <[u8; 16]>::try_from(cookie.as_slice()) {
            self.cookie = Some((cookie, now));
        }
    }

    /// Timer work, called a few times a second: handshake retries, rekeys,
    /// keepalives, and dropping expired keys.
    pub fn tick(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        self.keypairs.retain(|k| now.duration_since(k.created) < REJECT_AFTER_TIME * 3);

        if let Some(started) = self.attempt_started {
            if now.duration_since(started) >= REKEY_ATTEMPT_TIME {
                // Give up until there is something to send again.
                self.pending = None;
                self.attempt_started = None;
                self.queue.clear();
            } else if self.pending.as_ref().is_some_and(|p| now.duration_since(p.sent_at) >= REKEY_TIMEOUT) {
                out.push(self.initiate(now));
                return out;
            }
        }

        let has_session = self.sending_keypair(now).is_some();
        // An initiator rekeys a session nearing the end of its life even
        // without traffic, when a persistent keepalive is keeping it in use.
        if self.pending.is_none() && self.persistent_keepalive.is_some() {
            let newest = self.keypairs.first();
            let stale = newest.is_none_or(|k| k.initiator && now.duration_since(k.created) >= REKEY_AFTER_TIME);
            if stale || !has_session {
                out.push(self.initiate(now));
                return out;
            }
        }

        if has_session {
            let idle_send = self.last_sent.is_none_or(|t| now.duration_since(t) >= KEEPALIVE_TIMEOUT);
            let heard_since_sent = match (self.last_received, self.last_sent) {
                (Some(r), Some(s)) => r > s,
                (Some(_), None) => true,
                _ => false,
            };
            let persistent_due =
                self.persistent_keepalive.is_some_and(|p| self.last_sent.is_none_or(|t| now.duration_since(t) >= p));
            if (idle_send && heard_since_sent) || persistent_due {
                if let Some(kp) = self.sending_keypair(now) {
                    out.push(Self::data_packet(kp, &[]));
                    self.last_sent = Some(now);
                }
            }
        }
        out
    }
}

/// Strips the zero padding after an IP packet, using the packet's own
/// length field.
fn trim_padding(mut plain: Vec<u8>) -> Vec<u8> {
    let len = match plain.first().map(|b| b >> 4) {
        Some(4) if plain.len() >= 20 => u16::from_be_bytes([plain[2], plain[3]]) as usize,
        Some(6) if plain.len() >= 40 => 40 + u16::from_be_bytes([plain[4], plain[5]]) as usize,
        _ => plain.len(),
    };
    plain.truncate(len.min(plain.len()));
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> ([u8; 32], [u8; 32]) {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        getrandom::fill(&mut a).unwrap();
        getrandom::fill(&mut b).unwrap();
        (a, b)
    }

    fn ipv4_packet(payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2];
        p.extend_from_slice(payload);
        let len = p.len() as u16;
        p[2..4].copy_from_slice(&len.to_be_bytes());
        p
    }

    #[test]
    fn handshake_then_data_both_ways() {
        let (a, b) = keys();
        let psk = [7u8; 32];
        let mut alice = WireGuard::new(&a, public_key(&b), Some(psk), None);
        let mut bob = WireGuard::new(&b, public_key(&a), Some(psk), None);
        let now = Instant::now();

        let packet = ipv4_packet(b"hello through the tunnel");
        let init = alice.encapsulate(&packet, now);
        assert_eq!(init.len(), 1);
        assert_eq!(init[0].len(), INIT_LEN);

        let reply = bob.decapsulate(&init[0], now);
        assert_eq!(reply.to_network.len(), 1);
        let after = alice.decapsulate(&reply.to_network[0], now);
        // The queued packet goes out as soon as the session exists.
        assert_eq!(after.to_network.len(), 1);
        let got = bob.decapsulate(&after.to_network[0], now);
        assert_eq!(got.to_tunnel.as_deref(), Some(packet.as_slice()));

        // Bob may answer now that Alice has used the session.
        let back = ipv4_packet(b"and back");
        let sent = bob.encapsulate(&back, now);
        assert_eq!(sent.len(), 1);
        assert_eq!(alice.decapsulate(&sent[0], now).to_tunnel.as_deref(), Some(back.as_slice()));

        // A replayed data packet is dropped.
        assert!(alice.decapsulate(&sent[0], now).to_tunnel.is_none());
    }

    #[test]
    fn wrong_psk_or_wrong_peer_never_completes() {
        let (a, b) = keys();
        let now = Instant::now();
        let mut alice = WireGuard::new(&a, public_key(&b), Some([1; 32]), None);
        let mut bob = WireGuard::new(&b, public_key(&a), Some([2; 32]), None);
        let init = alice.encapsulate(&ipv4_packet(b"x"), now);
        let reply = bob.decapsulate(&init[0], now);
        assert!(alice.decapsulate(&reply.to_network[0], now).to_network.is_empty());
        assert!(alice.last_handshake().is_none());

        let (c, _) = keys();
        let mut stranger = WireGuard::new(&c, public_key(&b), None, None);
        let mut bob = WireGuard::new(&b, public_key(&a), None, None);
        let init = stranger.encapsulate(&ipv4_packet(b"x"), now);
        assert!(bob.decapsulate(&init[0], now).to_network.is_empty());
    }

    #[test]
    fn replay_window_tolerates_reordering_but_not_repeats() {
        let mut w = ReplayWindow::new();
        for n in [0u64, 2, 1, 5, 3] {
            assert!(w.would_accept(n));
            w.mark(n);
        }
        assert!(!w.would_accept(2));
        w.mark(5000);
        assert!(!w.would_accept(10), "far behind the window");
        assert!(w.would_accept(4999));
    }

    #[test]
    fn retries_handshake_after_rekey_timeout() {
        let (a, b) = keys();
        let mut alice = WireGuard::new(&a, public_key(&b), None, None);
        let start = Instant::now();
        alice.encapsulate(&ipv4_packet(b"x"), start);
        assert!(alice.tick(start + Duration::from_secs(1)).is_empty());
        let retry = alice.tick(start + REKEY_TIMEOUT + Duration::from_millis(1));
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0][0], TYPE_INIT);
    }
}
