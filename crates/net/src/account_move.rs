//! Moving an account from one device to another, directly, on the same
//! network.
//!
//! The old device already *is* the account, so it is the right one to vouch
//! for the new one: it shows a link (and the same link as a QR code), the
//! new device opens it, and the account comes across in one connection.
//! Nothing touches a server and nothing has to be typed but the link.
//!
//! # The link
//!
//! `stoop-move:1/<ip>:<port>/<64 hex digits>` -- where to connect, and a
//! fresh random 32-byte key. The key does two jobs, through two different
//! derivations so neither weakens the other:
//!
//! - the account is sealed under it ([`antiphon_crypto::account_seal::seal_with_key`]),
//!   so someone watching the network sees only ciphertext;
//! - the new device proves it holds the link before the old one sends
//!   anything ([`hello`]), so a stranger who finds the port open cannot
//!   even collect the ciphertext, let alone use up the one-time offer.
//!
//! The link is as sensitive as the account while it is live, which is why
//! it lives for minutes ([`OFFER_LIFETIME`]), is good for exactly one
//! successful receive, and closes the moment the old device's screen does.
//!
//! # The exchange
//!
//! ```text
//! new -> old : "STOOPMV2" hello[32] device[128] device_proof[32]
//! old -> new : length u64 BE, sealed account
//! new -> old : "OK"
//! ```
//!
//! `device` is the new device's own public keys (spec §3 "new devices
//! added by approving them from an existing device"): the old device signs
//! them into the account's device list before it seals the account, so the
//! list the new device receives already names it. `device_proof` binds
//! those keys to the link the same way `hello` does, so someone relaying
//! the connection can't swap in keys of their own and get *them* approved.
//!
//! The first version's greeting -- `"STOOPMV1" hello[32]`, no device -- is
//! still accepted; the account then arrives without a new entry in the
//! list, and the new device signs itself in on its first start.
//!
//! Blocking `std::net` on a thread of its own, rather than tokio: it is a
//! single connection, it runs once in a device's life, and both apps can
//! drive it from their interface without a runtime.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::PublicIdentity;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// What every move link starts with.
const SCHEME: &str = "stoop-move:";
/// The link layout this module writes.
const LINK_VERSION: &str = "1";
/// The first bytes a receiving device without its own keys sends.
const HELLO_MAGIC: &[u8; 8] = b"STOOPMV1";
/// The first bytes a receiving device sends when its own public keys follow.
const HELLO_MAGIC_V2: &[u8; 8] = b"STOOPMV2";
/// Label for the hello proof, so it is never the same bytes as the AEAD key.
const HELLO_LABEL: &[u8] = b"stoop/account-move/hello/v1";
/// Label for the proof that binds the new device's keys to the link.
const DEVICE_LABEL: &[u8] = b"stoop/account-move/device/v1";
/// How long a move offer stays open.
pub const OFFER_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// How long either side waits on a quiet connection.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// The most a receiver accepts, matching the backup format's own ceiling.
const MAX_PAYLOAD: u64 = 1 << 30;

/// Where to fetch an account from, and the key it is sealed under.
#[derive(Clone, PartialEq, Eq)]
pub struct MoveLink {
    /// The old device's address on the local network.
    pub addr: SocketAddr,
    /// The one-time key.
    pub key: [u8; 32],
}

/// Redacted: the key is the account while the offer is open.
impl std::fmt::Debug for MoveLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MoveLink({}, <key redacted>)", self.addr)
    }
}

impl MoveLink {
    /// The link as text, for a QR code or the clipboard.
    pub fn to_link(&self) -> String {
        format!("{SCHEME}{LINK_VERSION}/{}/{}", self.addr, antiphon_crypto::hex::encode(&self.key))
    }

    /// Reads a link, forgiving the spaces and line breaks a paste picks up.
    pub fn parse(text: &str) -> Option<Self> {
        let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let rest = text.strip_prefix(SCHEME)?;
        let mut parts = rest.split('/');
        if parts.next()? != LINK_VERSION {
            return None;
        }
        let addr: SocketAddr = parts.next()?.parse().ok()?;
        let key = antiphon_crypto::hex::decode_array(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(MoveLink { addr, key })
    }
}

/// Where a move offer stands, for the old device's screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveStatus {
    /// Open, waiting for the new device.
    Waiting,
    /// The new device connected and the account is on its way.
    Sending,
    /// The new device has the account.
    Done,
    /// Nobody came within [`OFFER_LIFETIME`].
    Expired,
    /// Stopped from this side.
    Stopped,
    /// Something broke; the text says what.
    Failed(String),
}

/// An open offer on the old device. Dropping it closes the offer.
pub struct MoveSender {
    /// The link to show.
    link: MoveLink,
    /// Updated by the offer's thread.
    status: Arc<Mutex<MoveStatus>>,
    /// Set to close the offer.
    stop: Arc<AtomicBool>,
}

/// Makes the account to send once the new device has said who it is: its
/// public keys (`None` from a device that sent none), in, the encoded
/// account bundle, not yet sealed, out. Called again if a first attempt
/// was cut off partway, so it must be safe to repeat for the same device.
pub type MakeAccount = Box<dyn FnMut(Option<&PublicIdentity>) -> Result<Vec<u8>, String> + Send>;

impl MoveSender {
    /// Opens an offer for `account` (an encoded bundle, not yet sealed) on
    /// `ip`, which should be this device's address on the local network
    /// ([`lan_ip`]). The account is sealed here, under a key made here.
    pub fn start(account: &[u8], ip: IpAddr) -> std::io::Result<Self> {
        let account = account.to_vec();
        Self::start_approving(Box::new(move |_| Ok(account.clone())), ip)
    }

    /// Opens an offer whose account is made only after the new device has
    /// proved it holds the link and named its own keys, so `make` can
    /// approve that device into the account's device list and send the
    /// list along. See [`MakeAccount`].
    pub fn start_approving(make: MakeAccount, ip: IpAddr) -> std::io::Result<Self> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
        let listener = TcpListener::bind(SocketAddr::new(ip, 0))?;
        // Polled, so the thread notices a stop or the deadline between
        // connection attempts.
        listener.set_nonblocking(true)?;
        let link = MoveLink { addr: listener.local_addr()?, key };
        let status = Arc::new(Mutex::new(MoveStatus::Waiting));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_status = status.clone();
        let thread_stop = stop.clone();
        std::thread::Builder::new()
            .name("account-move".into())
            .spawn(move || serve(&listener, key, make, &thread_status, &thread_stop))?;
        Ok(MoveSender { link, status, stop })
    }

    /// The link the new device opens.
    pub fn link(&self) -> &MoveLink {
        &self.link
    }

    /// Where the offer stands.
    pub fn status(&self) -> MoveStatus {
        self.status.lock().unwrap().clone()
    }

    /// Closes the offer now.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for MoveSender {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The offer's thread: accepts until one device takes the account, the
/// offer is stopped, or it expires. A connection that fails the hello is
/// dropped and the offer stays open for the real one.
fn serve(listener: &TcpListener, key: [u8; 32], mut make: MakeAccount, status: &Mutex<MoveStatus>, stop: &AtomicBool) {
    let key = Zeroizing::new(key);
    let deadline = Instant::now() + OFFER_LIFETIME;
    let set = |s: MoveStatus| *status.lock().unwrap() = s;
    loop {
        if stop.load(Ordering::Relaxed) {
            set(MoveStatus::Stopped);
            return;
        }
        if Instant::now() >= deadline {
            set(MoveStatus::Expired);
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => match send_to(stream, &key, &mut make, &set) {
                Ok(true) => {
                    set(MoveStatus::Done);
                    return;
                }
                // Not someone with the link: keep waiting for them.
                Ok(false) => set(MoveStatus::Waiting),
                // The account could not be made for this device: nothing
                // was sent, and trying again would fail the same way.
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    set(MoveStatus::Failed(e.to_string()));
                    return;
                }
                // The real device, cut off partway: it can open the link
                // again, so the offer stays open for its retry.
                Err(_) => set(MoveStatus::Waiting),
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                set(MoveStatus::Failed(e.to_string()));
                return;
            }
        }
    }
}

/// Serves one connection. `Ok(false)` when it did not prove it holds the
/// link, `Ok(true)` when it took the account and said so. An error of kind
/// `InvalidData` means `make` refused, before anything was sent.
fn send_to(mut stream: TcpStream, key: &[u8; 32], make: &mut MakeAccount, set: &dyn Fn(MoveStatus)) -> std::io::Result<bool> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut greeting = [0u8; 8 + 32];
    if stream.read_exact(&mut greeting).is_err() {
        return Ok(false);
    }
    let with_device = match &greeting[..8] {
        m if m == HELLO_MAGIC => false,
        m if m == HELLO_MAGIC_V2 => true,
        _ => return Ok(false),
    };
    if !same(&greeting[8..], &hello(key)) {
        return Ok(false);
    }
    let device = if with_device {
        let mut rest = [0u8; PublicIdentity::ENCODED_LEN + 32];
        if stream.read_exact(&mut rest).is_err() {
            return Ok(false);
        }
        let (keys, proof) = rest.split_at(PublicIdentity::ENCODED_LEN);
        // Keys not bound to this link, or whose own prekey signature is
        // broken, are treated like a wrong hello: never approved.
        if !same(proof, &device_proof(key, keys)) {
            return Ok(false);
        }
        match PublicIdentity::decode(keys).and_then(|d| d.verify().map(|()| d)) {
            Ok(d) => Some(d),
            Err(_) => return Ok(false),
        }
    } else {
        None
    };
    set(MoveStatus::Sending);
    let account = Zeroizing::new(make(device.as_ref()).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?);
    let sealed = antiphon_crypto::account_seal::seal_with_key(&account, key).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    stream.write_all(&(sealed.len() as u64).to_be_bytes())?;
    stream.write_all(&sealed)?;
    stream.flush()?;
    let mut ok = [0u8; 2];
    stream.read_exact(&mut ok)?;
    Ok(&ok == b"OK")
}

/// Fetches the account a link offers: the encoded bundle, opened. Names no
/// device of its own; [`receive_as`] is the one that gets this device
/// approved on the way.
pub fn receive(link: &MoveLink) -> Result<Zeroizing<Vec<u8>>, String> {
    receive_as(link, None)
}

/// Fetches the account a link offers, telling the old device this
/// device's own public keys so it approves them into the account's device
/// list before sending.
pub fn receive_as(link: &MoveLink, device: Option<&PublicIdentity>) -> Result<Zeroizing<Vec<u8>>, String> {
    let unreachable = |e: std::io::Error| {
        format!("Couldn't reach the other device ({e}). Both need to be on the same Wi-Fi, with its Move screen still open.")
    };
    let mut stream = TcpStream::connect_timeout(&link.addr, Duration::from_secs(10)).map_err(unreachable)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    let mut greeting = Vec::with_capacity(8 + 32 + PublicIdentity::ENCODED_LEN + 32);
    greeting.extend_from_slice(if device.is_some() { HELLO_MAGIC_V2 } else { HELLO_MAGIC });
    greeting.extend_from_slice(&hello(&link.key));
    if let Some(device) = device {
        let keys = device.encode();
        greeting.extend_from_slice(&keys);
        greeting.extend_from_slice(&device_proof(&link.key, &keys));
    }
    stream.write_all(&greeting).map_err(unreachable)?;

    let lost = |e: std::io::Error| format!("The connection dropped partway ({e}). Try again.");
    let mut len = [0u8; 8];
    stream.read_exact(&mut len).map_err(|_| "The other device refused the link. It may have expired; open Move again there.".to_string())?;
    let len = u64::from_be_bytes(len);
    if len > MAX_PAYLOAD {
        return Err("The other device offered more than an account can hold.".into());
    }
    let mut sealed = vec![0u8; len as usize];
    stream.read_exact(&mut sealed).map_err(lost)?;
    let account = antiphon_crypto::account_seal::open_with_key(&sealed, &link.key)
        .map_err(|_| "What arrived didn't open with this link.".to_string())?;
    stream.write_all(b"OK").map_err(lost)?;
    Ok(account)
}

/// The proof a receiving device holds the link's key.
fn hello(key: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(HELLO_LABEL);
    h.update(key);
    h.finalize().into()
}

/// The proof that `device` (its encoded public keys) came from whoever
/// holds the link's key.
fn device_proof(key: &[u8; 32], device: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DEVICE_LABEL);
    h.update(key);
    h.update(device);
    h.finalize().into()
}

/// Compares two byte strings without stopping at the first difference, so
/// the time a refusal takes says nothing about how close a guess was.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// This device's address on the local network: the one its default route
/// leaves from. Connecting a UDP socket sends nothing; it only asks the
/// system which interface it would use. `None` when there is no network.
pub fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Waits up to two seconds for the offer to leave `Waiting`/`Sending`.
    fn settle(sender: &MoveSender) -> MoveStatus {
        for _ in 0..200 {
            let s = sender.status();
            if !matches!(s, MoveStatus::Waiting | MoveStatus::Sending) {
                return s;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        sender.status()
    }

    #[test]
    fn link_round_trip_and_paste_noise() {
        let link = MoveLink { addr: "192.168.1.5:40123".parse().unwrap(), key: [0xab; 32] };
        let text = link.to_link();
        assert_eq!(MoveLink::parse(&text), Some(link.clone()));
        let pasted = format!("  {}\n{} ", &text[..20], &text[20..]);
        assert_eq!(MoveLink::parse(&pasted), Some(link));
        assert!(MoveLink::parse("stoop-move:2/1.2.3.4:5/00").is_none());
        assert!(MoveLink::parse("https://example.com").is_none());
    }

    #[test]
    fn the_account_moves_once() {
        let sender = MoveSender::start(b"the whole account", "127.0.0.1".parse().unwrap()).unwrap();
        let got = receive(sender.link()).unwrap();
        assert_eq!(&**got, b"the whole account");
        assert_eq!(settle(&sender), MoveStatus::Done);
        assert!(receive(sender.link()).is_err(), "the offer closes after one move");
    }

    #[test]
    fn a_wrong_key_gets_nothing_and_the_offer_stays_open() {
        let sender = MoveSender::start(b"secret", "127.0.0.1".parse().unwrap()).unwrap();
        let wrong = MoveLink { addr: sender.link().addr, key: [1; 32] };
        assert!(receive(&wrong).is_err());
        let got = receive(sender.link()).unwrap();
        assert_eq!(&**got, b"secret");
    }

    #[test]
    fn the_new_device_is_named_to_the_old_one() {
        let phone = antiphon_crypto::IdentityKeyPair::generate().public();
        let seen = Arc::new(Mutex::new(None));
        let seen_by_offer = seen.clone();
        let sender = MoveSender::start_approving(
            Box::new(move |device| {
                *seen_by_offer.lock().unwrap() = device.copied();
                Ok(b"account with the phone approved".to_vec())
            }),
            "127.0.0.1".parse().unwrap(),
        )
        .unwrap();
        let got = receive_as(sender.link(), Some(&phone)).unwrap();
        assert_eq!(&**got, b"account with the phone approved");
        assert_eq!(*seen.lock().unwrap(), Some(phone));
        assert_eq!(settle(&sender), MoveStatus::Done);
    }

    #[test]
    fn swapped_device_keys_are_never_approved() {
        let approved = Arc::new(AtomicBool::new(false));
        let flag = approved.clone();
        let sender = MoveSender::start_approving(
            Box::new(move |_| {
                flag.store(true, Ordering::Relaxed);
                Ok(b"x".to_vec())
            }),
            "127.0.0.1".parse().unwrap(),
        )
        .unwrap();
        // Someone relaying the connection keeps the hello but puts in keys
        // of their own; the proof was made over the real ones.
        let real = antiphon_crypto::IdentityKeyPair::generate().public().encode();
        let theirs = antiphon_crypto::IdentityKeyPair::generate().public().encode();
        let mut stream = TcpStream::connect(sender.link().addr).unwrap();
        stream.write_all(HELLO_MAGIC_V2).unwrap();
        stream.write_all(&hello(&sender.link().key)).unwrap();
        stream.write_all(&theirs).unwrap();
        stream.write_all(&device_proof(&sender.link().key, &real)).unwrap();
        let mut buf = [0u8; 8];
        assert!(stream.read_exact(&mut buf).is_err(), "nothing is sent");
        assert!(!approved.load(Ordering::Relaxed));
        // The offer is still open for the real device.
        assert!(receive(sender.link()).is_ok());
    }

    #[test]
    fn a_refused_device_fails_the_offer_without_sending() {
        let sender = MoveSender::start_approving(Box::new(|_| Err("no room".into())), "127.0.0.1".parse().unwrap()).unwrap();
        assert!(receive(sender.link()).is_err());
        assert_eq!(settle(&sender), MoveStatus::Failed("no room".into()));
    }

    #[test]
    fn stopping_closes_the_offer() {
        let sender = MoveSender::start(b"x", "127.0.0.1".parse().unwrap()).unwrap();
        sender.stop();
        assert_eq!(settle(&sender), MoveStatus::Stopped);
    }
}
