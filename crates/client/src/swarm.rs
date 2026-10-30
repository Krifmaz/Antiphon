//! Multi-peer download (spec §5: "Because chunks verify independently, a
//! download can pull from several peers at once. In a large community a
//! popular file gets faster the more people have it").
//!
//! Every holder that answers gets its own worker pulling chunk numbers
//! from one shared queue, so a fast holder simply takes more of them. A
//! chunk is checked against the file id the moment it arrives and opened
//! with the file key; one that fails goes back on the queue and the
//! holder that sent it is dropped. The hash list itself is fetched from
//! whichever holder answers first and trusted only once it hashes to the
//! file id the sender gave us.
//!
//! # Anonymous downloads
//!
//! With anonymous seeding on (spec §5 "Anonymous seeding through
//! relays"), every holder is reached through its own onion circuit
//! (`antiphon_net::onion`, built by `crate::anonymity::route_to`) instead
//! of from this device's address. A holder then sees only the circuit's
//! last hop: it can still check the read token or a contact claim, which
//! prove *permission*, but it never learns *where* the downloader is.
//! Without enough hop nodes the download is refused rather than quietly
//! made in the open -- see [`Route`].

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::file_seal::{self, FileId, FileKey, CHUNK};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::blob;
use antiphon_net::vpn::TrafficKind;
use antiphon_net::AppSocket;

pub const TIMEOUT: Duration = Duration::from_secs(3);

/// How a download or availability check reaches holders.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Route {
    /// From this device's address (through the VPN when files are routed
    /// there).
    #[default]
    Direct,
    /// Through an onion circuit over these hop nodes, a fresh circuit per
    /// holder, so no holder sees this device's address.
    Anonymous(Vec<String>),
}

/// One way of talking to a holder: the socket to use and the address to
/// send to. For a circuit, the address is the circuit's local stand-in
/// for the holder, and the circuit lives as long as the socket.
type Link = (Arc<AppSocket>, SocketAddr);

/// Opens a link to `holder` along `route`.
async fn open_link(route: &Route, holder: SocketAddr) -> Result<Link, String> {
    match route {
        // Shared-file traffic goes through the VPN when the user routed it
        // there, and is refused rather than sent direct under the kill
        // switch (spec §6).
        Route::Direct => {
            let socket = crate::vpn::bind(TrafficKind::Files, 0).await.map_err(|e| e.to_string())?;
            Ok((Arc::new(socket), holder))
        }
        Route::Anonymous(hops) => crate::anonymity::route_to(hops, holder)
            .await
            .map_err(|e| format!("couldn't build an anonymous path to {holder}: {e}")),
    }
}

/// The spec's three availability states, worded as it asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    SenderOffline,
    NoLongerShared,
}

impl Availability {
    pub fn wording(&self) -> &'static str {
        match self {
            Availability::Available => "Available",
            Availability::SenderOffline => "Sender offline — will resume when they're back",
            Availability::NoLongerShared => "No longer shared",
        }
    }
}

/// Asks every holder; the best answer wins. Asking goes along `route`
/// too, since even "is it there?" tells a holder who is interested.
pub async fn availability(holders: &[SocketAddr], file: FileId, token: [u8; 32], route: &Route) -> Availability {
    let mut removed = false;
    for h in holders {
        let Ok((socket, to)) = open_link(route, *h).await else { continue };
        match blob::info(&*socket, to, file, token, Duration::from_millis(1500)).await {
            Ok(_) => return Availability::Available,
            // Contacts-only holders answer that before anything else;
            // the file is there.
            Err(antiphon_net::NetError::FileRefused(blob::Refusal::NotAContact)) => return Availability::Available,
            Err(antiphon_net::NetError::FileRefused(blob::Refusal::NoLongerShared)) => removed = true,
            Err(_) => {}
        }
    }
    if removed {
        Availability::NoLongerShared
    } else {
        Availability::SenderOffline
    }
}

/// How a download went, for the progress line.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: u32,
    pub total: u32,
    /// How many holders contributed at least one chunk.
    pub holders_used: usize,
}

/// Downloads `file` from every holder that will serve it, writing the
/// plaintext to `dest`. `me` is used to prove you're a contact to
/// holders that share with contacts only. Each holder is reached along
/// `route`, over one link that both the first probe and that holder's
/// worker use -- a contact claim is tied to the address it came from, so
/// the worker must speak from the same one.
pub async fn download(
    holders: &[SocketAddr],
    file: FileId,
    key: FileKey,
    me: Option<Arc<IdentityKeyPair>>,
    dest: &Path,
    progress: Arc<Mutex<Progress>>,
    route: &Route,
) -> Result<(), String> {
    let token = file_seal::read_token(&key);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    // Find who has it, and the hash list from the first that does.
    let mut live = Vec::new();
    let mut size_and_hashes = None;
    let mut last_error = String::from("nobody holding it answered");
    for h in holders {
        let (socket, to) = match open_link(route, *h).await {
            Ok(link) => link,
            Err(e) => {
                last_error = e;
                continue;
            }
        };
        let mut answer = blob::info(&*socket, to, file, token, TIMEOUT).await;
        if let (Err(antiphon_net::NetError::FileRefused(blob::Refusal::NotAContact)), Some(me)) = (&answer, &me) {
            let sig = me.sign(&blob::claim_bytes(&file, now));
            if blob::claim(&*socket, to, file, me.public().identity_key, now, sig, TIMEOUT).await.is_ok() {
                answer = blob::info(&*socket, to, file, token, TIMEOUT).await;
            }
        }
        match answer {
            Ok((size, chunks)) => {
                if size_and_hashes.is_none() {
                    if let Ok(hashes) = blob::hashes(&*socket, to, file, token, chunks, TIMEOUT).await {
                        if file_seal::file_id(size, &hashes) == file {
                            size_and_hashes = Some((size, hashes));
                        }
                    }
                }
                live.push((socket, to));
            }
            Err(e) => last_error = e.to_string(),
        }
    }
    let Some((size, hashes)) = size_and_hashes else { return Err(last_error) };
    let total = hashes.len() as u32;
    progress.lock().unwrap().total = total;

    let out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    out.set_len(size).map_err(|e| e.to_string())?;
    let out = Arc::new(Mutex::new(out));
    let queue: Arc<Mutex<VecDeque<u32>>> = Arc::new(Mutex::new((0..total).collect()));
    let hashes = Arc::new(hashes);
    let remaining = Arc::new(std::sync::atomic::AtomicU32::new(total));

    let mut workers = Vec::new();
    for (socket, holder) in live {
        let queue = queue.clone();
        let hashes = hashes.clone();
        let out = out.clone();
        let progress = progress.clone();
        let remaining = remaining.clone();
        // The probe already claimed on this very link, so the worker is
        // known to the holder as the same contact.
        workers.push(tokio::spawn(async move {
            let mut used = false;
            let mut failures = 0;
            loop {
                if remaining.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    break;
                }
                let next = queue.lock().unwrap().pop_front();
                let Some(index) = next else {
                    // Others hold the last chunks; wait in case one fails.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                };
                let got = blob::chunk(&*socket, holder, file, token, index, TIMEOUT).await;
                let good = got.ok().filter(|data| file_seal::chunk_hash(data) == hashes[index as usize]);
                let plain = good.and_then(|data| file_seal::open_chunk(&key, index, &data).ok());
                let Some(plain) = plain else {
                    queue.lock().unwrap().push_back(index);
                    failures += 1;
                    if failures >= 3 {
                        break; // this holder is gone or lying
                    }
                    continue;
                };
                failures = 0;
                {
                    use std::io::{Seek, SeekFrom, Write};
                    let mut f = out.lock().unwrap();
                    if f.seek(SeekFrom::Start(index as u64 * CHUNK as u64)).is_err() || f.write_all(&plain).is_err() {
                        queue.lock().unwrap().push_back(index);
                        break;
                    }
                }
                remaining.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let mut p = progress.lock().unwrap();
                p.done += 1;
                if !used {
                    used = true;
                    p.holders_used += 1;
                }
            }
        }));
    }
    for w in workers {
        let _ = w.await;
    }
    if remaining.load(std::sync::atomic::Ordering::Relaxed) > 0 {
        return Err("the holders went away before it finished; try again later".into());
    }
    out.lock().unwrap().sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_relay::files::{FileHost, FileStore, HostPolicy, Scope};
    use tokio::net::UdpSocket;

    fn temp(tag: &str) -> std::path::PathBuf {
        let mut r = [0u8; 6];
        getrandom::fill(&mut r).unwrap();
        std::env::temp_dir().join(format!("antiphon-swarm-{tag}-{}", antiphon_crypto::hex::encode(&r)))
    }

    async fn holder(plain: &[u8], key: &FileKey, scope: Scope, contacts: antiphon_relay::files::ContactCheck) -> SocketAddr {
        let (id, chunks) = file_seal::seal_all(key, plain);
        let mut store = FileStore::open(&temp("store")).unwrap();
        let hashes = chunks.iter().map(|c| file_seal::chunk_hash(c)).collect();
        store.begin(id, plain.len() as u64, hashes, scope, Some(file_seal::read_token(key)), [0; 32], 0).unwrap();
        for (i, c) in chunks.iter().enumerate() {
            store.put_chunk(&id, i as u32, c).unwrap();
        }
        let host = Arc::new(Mutex::new(FileHost::new(store, HostPolicy::default(), contacts)));
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = sock.local_addr().unwrap();
        tokio::spawn(antiphon_relay::run_node(
            sock,
            Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default()))),
            None,
            Some(host),
        ));
        addr
    }

    #[tokio::test]
    async fn a_file_comes_down_from_two_holders_at_once() {
        let key = file_seal::new_key();
        let plain: Vec<u8> = (0..60_000u32).map(|i| (i * 7) as u8).collect();
        let (id, _) = file_seal::seal_all(&key, &plain);
        let a = holder(&plain, &key, Scope::Conversation, Arc::new(|_| false)).await;
        let b = holder(&plain, &key, Scope::Conversation, Arc::new(|_| false)).await;
        // A third address nobody answers on: it must not stop the rest.
        let dead: SocketAddr = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let dest = temp("out");
        let progress = Arc::new(Mutex::new(Progress::default()));
        download(&[a, dead, b], id, key, None, &dest, progress.clone(), &Route::Direct).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), plain);
        assert_eq!(progress.lock().unwrap().holders_used, 2, "both holders served chunks");
        assert_eq!(availability(&[a], id, file_seal::read_token(&key), &Route::Direct).await, Availability::Available);
        assert_eq!(availability(&[dead], id, file_seal::read_token(&key), &Route::Direct).await, Availability::SenderOffline);
    }

    #[tokio::test]
    async fn the_wrong_key_gets_nothing() {
        let key = file_seal::new_key();
        let plain = vec![1u8; 4000];
        let (id, _) = file_seal::seal_all(&key, &plain);
        let a = holder(&plain, &key, Scope::Conversation, Arc::new(|_| false)).await;
        let err = download(&[a], id, file_seal::new_key(), None, &temp("x"), Default::default(), &Route::Direct).await.unwrap_err();
        assert!(err.contains("isn't shared with you"), "{err}");
    }

    #[tokio::test]
    async fn a_contacts_only_holder_serves_a_contact() {
        let me = Arc::new(IdentityKeyPair::generate());
        let my_key = me.public().identity_key;
        let key = file_seal::new_key();
        let plain = vec![9u8; 2500];
        let (id, _) = file_seal::seal_all(&key, &plain);
        let a = holder(&plain, &key, Scope::Contacts, Arc::new(move |k| *k == my_key)).await;
        let dest = temp("c");
        download(&[a], id, key, Some(me), &dest, Default::default(), &Route::Direct).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), plain);
        let stranger = Arc::new(IdentityKeyPair::generate());
        assert!(download(&[a], id, key, Some(stranger), &temp("s"), Default::default(), &Route::Direct).await.is_err());
    }

    /// A hop node on its own loopback IP. Loopback counts as private, so
    /// it has to be let in explicitly.
    async fn hop_node(ip: &str) -> SocketAddr {
        use antiphon_relay::onion::{HopLimits, HopNode};
        let socket = Arc::new(UdpSocket::bind((ip, 0)).await.unwrap());
        let addr = socket.local_addr().unwrap();
        let limits = HopLimits { allow_private: true, ..HopLimits::default() };
        let hops = Arc::new(Mutex::new(HopNode::new(antiphon_crypto::onion::HopSecret::generate(), limits)));
        let relay = Arc::new(Mutex::new(antiphon_relay::Relay::new(Default::default())));
        tokio::spawn(antiphon_relay::run_node_with_hops(socket, relay, None, None, Some(hops)));
        addr
    }

    #[tokio::test]
    async fn an_anonymous_download_never_shows_the_holder_where_it_comes_from() {
        // Each node on its own loopback address, so who sent what can be
        // told apart: the downloader speaks from 127.0.0.1, the entry hop
        // is 127.0.0.2 and the exit 127.0.0.3. An exit sends from its own
        // IP, so a holder reached through the circuit hears only from .3.
        let entry = hop_node("127.0.0.2").await;
        let exit = hop_node("127.0.0.3").await;

        let key = file_seal::new_key();
        let plain: Vec<u8> = (0..9000u32).map(|i| (i * 13) as u8).collect();
        let (id, chunks) = file_seal::seal_all(&key, &plain);
        let mut store = FileStore::open(&temp("anon")).unwrap();
        let hashes = chunks.iter().map(|c| file_seal::chunk_hash(c)).collect();
        store.begin(id, plain.len() as u64, hashes, Scope::Conversation, Some(file_seal::read_token(&key)), [0; 32], 0).unwrap();
        for (i, c) in chunks.iter().enumerate() {
            store.put_chunk(&id, i as u32, c).unwrap();
        }
        // A holder that writes down every address it hears from -- what
        // a holder that *did* keep logs would have.
        let mut host = FileHost::new(store, HostPolicy::default(), Arc::new(|_| false));
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let holder = socket.local_addr().unwrap();
        let heard = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let log = heard.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_536];
            loop {
                let (n, from) = socket.recv_from(&mut buf).await.unwrap();
                log.lock().unwrap().insert(from.ip());
                for (to, bytes) in host.handle(from, &buf[..n], std::time::Instant::now(), 0) {
                    let _ = socket.send_to(&bytes, to).await;
                }
            }
        });

        let route = Route::Anonymous(vec![entry.to_string(), exit.to_string()]);
        let dest = temp("anon-out");
        download(&[holder], id, key, None, &dest, Default::default(), &route).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), plain);
        assert_eq!(availability(&[holder], id, file_seal::read_token(&key), &route).await, Availability::Available);

        let heard = heard.lock().unwrap().clone();
        let exit_ip: std::net::IpAddr = "127.0.0.3".parse().unwrap();
        assert_eq!(heard, [exit_ip].into_iter().collect(), "the holder heard from someone other than the exit");
    }

    #[tokio::test]
    async fn an_anonymous_route_that_cannot_be_built_fails_instead_of_going_direct() {
        let key = file_seal::new_key();
        let plain = vec![3u8; 1500];
        let (id, _) = file_seal::seal_all(&key, &plain);
        let a = holder(&plain, &key, Scope::Conversation, Arc::new(|_| false)).await;
        // Hops nobody answers on.
        let dead1 = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let dead2 = UdpSocket::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
        let route = Route::Anonymous(vec![dead1.to_string(), dead2.to_string()]);
        let err = download(&[a], id, key, None, &temp("nope"), Default::default(), &route).await.unwrap_err();
        assert!(err.contains("anonymous path"), "{err}");
    }
}
