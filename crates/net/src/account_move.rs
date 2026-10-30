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
//! new -> old : "STOOPMV1" hello[32]
//! old -> new : length u64 BE, sealed account
//! new -> old : "OK"
//! ```
//!
//! Blocking `std::net` on a thread of its own, rather than tokio: it is a
//! single connection, it runs once in a device's life, and both apps can
//! drive it from their interface without a runtime.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// What every move link starts with.
const SCHEME: &str = "stoop-move:";
/// The link layout this module writes.
const LINK_VERSION: &str = "1";
/// The first bytes the receiving device sends.
const HELLO_MAGIC: &[u8; 8] = b"STOOPMV1";
/// Label for the hello proof, so it is never the same bytes as the AEAD key.
const HELLO_LABEL: &[u8] = b"stoop/account-move/hello/v1";
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

impl MoveSender {
    /// Opens an offer for `account` (an encoded bundle, not yet sealed) on
    /// `ip`, which should be this device's address on the local network
    /// ([`lan_ip`]). The account is sealed here, under a key made here.
    pub fn start(account: &[u8], ip: IpAddr) -> std::io::Result<Self> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| std::io::Error::other(e.to_string()))?;
        let sealed = antiphon_crypto::account_seal::seal_with_key(account, &key).map_err(|e| std::io::Error::other(e.to_string()))?;
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
            .spawn(move || serve(listener, key, sealed, thread_status, thread_stop))?;
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
fn serve(listener: TcpListener, key: [u8; 32], sealed: Vec<u8>, status: Arc<Mutex<MoveStatus>>, stop: Arc<AtomicBool>) {
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
            Ok((stream, _)) => match send_to(stream, &key, &sealed, &set) {
                Ok(true) => {
                    set(MoveStatus::Done);
                    return;
                }
                // Not someone with the link: keep waiting for them.
                Ok(false) => set(MoveStatus::Waiting),
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
/// link, `Ok(true)` when it took the account and said so.
fn send_to(mut stream: TcpStream, key: &[u8; 32], sealed: &[u8], set: &dyn Fn(MoveStatus)) -> std::io::Result<bool> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut greeting = [0u8; 8 + 32];
    if stream.read_exact(&mut greeting).is_err() {
        return Ok(false);
    }
    if &greeting[..8] != HELLO_MAGIC || !same(&greeting[8..], &hello(key)) {
        return Ok(false);
    }
    set(MoveStatus::Sending);
    stream.write_all(&(sealed.len() as u64).to_be_bytes())?;
    stream.write_all(sealed)?;
    stream.flush()?;
    let mut ok = [0u8; 2];
    stream.read_exact(&mut ok)?;
    Ok(&ok == b"OK")
}

/// Fetches the account a link offers: the encoded bundle, opened.
pub fn receive(link: &MoveLink) -> Result<Zeroizing<Vec<u8>>, String> {
    let unreachable = |e: std::io::Error| {
        format!("Couldn't reach the other device ({e}). Both need to be on the same Wi-Fi, with its Move screen still open.")
    };
    let mut stream = TcpStream::connect_timeout(&link.addr, Duration::from_secs(10)).map_err(unreachable)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    let mut greeting = Vec::with_capacity(40);
    greeting.extend_from_slice(HELLO_MAGIC);
    greeting.extend_from_slice(&hello(&link.key));
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
    fn stopping_closes_the_offer() {
        let sender = MoveSender::start(b"x", "127.0.0.1".parse().unwrap()).unwrap();
        sender.stop();
        assert_eq!(settle(&sender), MoveStatus::Stopped);
    }
}
