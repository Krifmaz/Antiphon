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

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::file_seal::{self, FileId, FileKey, CHUNK};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::blob;
use tokio::net::UdpSocket;

pub const TIMEOUT: Duration = Duration::from_secs(3);

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

/// Asks every holder; the best answer wins.
pub async fn availability(holders: &[SocketAddr], file: FileId, token: [u8; 32]) -> Availability {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { return Availability::SenderOffline };
    let mut removed = false;
    for h in holders {
        match blob::info(&socket, *h, file, token, Duration::from_millis(1500)).await {
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
/// holders that share with contacts only.
pub async fn download(
    holders: &[SocketAddr],
    file: FileId,
    key: FileKey,
    me: Option<Arc<IdentityKeyPair>>,
    dest: &Path,
    progress: Arc<Mutex<Progress>>,
) -> Result<(), String> {
    let token = file_seal::read_token(&key);
    let probe = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);

    // Find who has it, and the hash list from the first that does.
    let mut live = Vec::new();
    let mut size_and_hashes = None;
    let mut last_error = String::from("nobody holding it answered");
    for h in holders {
        let mut answer = blob::info(&probe, *h, file, token, TIMEOUT).await;
        if let (Err(antiphon_net::NetError::FileRefused(blob::Refusal::NotAContact)), Some(me)) = (&answer, &me) {
            let sig = me.sign(&blob::claim_bytes(&file, now));
            if blob::claim(&probe, *h, file, me.public().identity_key, now, sig, TIMEOUT).await.is_ok() {
                answer = blob::info(&probe, *h, file, token, TIMEOUT).await;
            }
        }
        match answer {
            Ok((size, chunks)) => {
                if size_and_hashes.is_none() {
                    if let Ok(hashes) = blob::hashes(&probe, *h, file, token, chunks, TIMEOUT).await {
                        if file_seal::file_id(size, &hashes) == file {
                            size_and_hashes = Some((size, hashes));
                        }
                    }
                }
                live.push(*h);
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
    for holder in live {
        let queue = queue.clone();
        let hashes = hashes.clone();
        let out = out.clone();
        let progress = progress.clone();
        let remaining = remaining.clone();
        // Claims are per address, so each worker reuses the probe's
        // address only for the first holder; the rest claim afresh.
        let me = me.clone();
        workers.push(tokio::spawn(async move {
            let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else { return };
            if let Some(me) = &me {
                let sig = me.sign(&blob::claim_bytes(&file, now));
                let _ = blob::claim(&socket, holder, file, me.public().identity_key, now, sig, TIMEOUT).await;
            }
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
                let got = blob::chunk(&socket, holder, file, token, index, TIMEOUT).await;
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

    fn temp(tag: &str) -> std::path::PathBuf {
        let mut r = [0u8; 6];
        getrandom::fill(&mut r).unwrap();
        std::env::temp_dir().join(format!("antiphon-swarm-{tag}-{}", r.iter().map(|b| format!("{b:02x}")).collect::<String>()))
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
        download(&[a, dead, b], id, key, None, &dest, progress.clone()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), plain);
        assert_eq!(progress.lock().unwrap().holders_used, 2, "both holders served chunks");
        assert_eq!(availability(&[a], id, file_seal::read_token(&key)).await, Availability::Available);
        assert_eq!(availability(&[dead], id, file_seal::read_token(&key)).await, Availability::SenderOffline);
    }

    #[tokio::test]
    async fn the_wrong_key_gets_nothing() {
        let key = file_seal::new_key();
        let plain = vec![1u8; 4000];
        let (id, _) = file_seal::seal_all(&key, &plain);
        let a = holder(&plain, &key, Scope::Conversation, Arc::new(|_| false)).await;
        let err = download(&[a], id, file_seal::new_key(), None, &temp("x"), Default::default()).await.unwrap_err();
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
        download(&[a], id, key, Some(me), &dest, Default::default()).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), plain);
        let stranger = Arc::new(IdentityKeyPair::generate());
        assert!(download(&[a], id, key, Some(stranger), &temp("s"), Default::default()).await.is_err());
    }
}
