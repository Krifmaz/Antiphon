use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use antiphon_crypto::{HandshakeInit, HandshakeMessage, IdentityKeyPair, PublicIdentity, Session};
use antiphon_protocol::{decode, encode, FrameType};

use crate::error::NetError;

const MAX_DATAGRAM: usize = 4096;

/// A "known contact -> direct P2P" connection (spec §3): one UDP socket
/// talking to exactly one peer, after an authenticated triple-DH handshake
/// has established an encrypted [`Session`]. Relay fallback, the DHT, and
/// group SFU transport are separate, larger pieces of work layered on top
/// of this.
///
/// Every constructor now takes the local [`IdentityKeyPair`] and hands
/// back the peer's verified [`PublicIdentity`]. Before that, the handshake
/// was ephemeral-only: it proved "whoever is at this address right now"
/// and nothing about *who*, which made contact pinning and verification
/// codes impossible to build on top.
pub struct PeerConnection {
    socket: Arc<UdpSocket>,
    remote: SocketAddr,
    session: Mutex<Session>,
    peer: PublicIdentity,
}

impl PeerConnection {
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote
    }

    /// The peer's verified long-term identity.
    ///
    /// Verified in the sense that they proved possession of the matching
    /// secret during the handshake -- not that this identity belongs to
    /// the person the user thinks it does. Closing *that* gap is what the
    /// out-of-band verification code is for.
    pub fn peer_identity(&self) -> PublicIdentity {
        self.peer
    }

    /// Initiates a handshake to `remote` and blocks (resending every 300ms)
    /// until the peer responds. Call this on the side placing the call.
    pub async fn connect(
        socket: Arc<UdpSocket>,
        remote: SocketAddr,
        identity: &IdentityKeyPair,
    ) -> Result<Self, NetError> {
        let hs = HandshakeInit::new();
        let packet = encode(FrameType::Handshake, 0, &hs.message(identity).encode());

        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            socket.send_to(&packet, remote).await?;

            let recv = tokio::time::timeout(Duration::from_millis(300), socket.recv_from(&mut buf)).await;
            let Ok(Ok((n, from))) = recv else { continue };
            if from != remote {
                continue;
            }
            let Ok(raw) = decode(&buf[..n]) else { continue };
            if raw.frame_type != FrameType::Handshake {
                continue;
            }
            let their_message =
                HandshakeMessage::decode(&raw.payload).map_err(|_| NetError::BadHandshake)?;
            let done = hs
                .complete(identity, &their_message, true)
                .map_err(|_| NetError::BadHandshake)?;
            return Ok(Self {
                socket,
                remote,
                session: Mutex::new(done.session),
                peer: done.peer,
            });
        }
    }

    /// Simultaneous open for NAT hole punching: both peers call this at
    /// roughly the same time, each aimed at the other's *public* address
    /// (discovered via [`crate::stun`]). Each side's outbound packets
    /// punch a hole in its own router, so the other side's packets have a
    /// mapping to arrive through -- which is the whole trick, and why
    /// neither side can be passive the way [`Self::listen`] is.
    ///
    /// Roles can't be decided by who dialed, because both did. Instead the
    /// side holding the numerically larger ephemeral public key takes the
    /// initiator role: both peers know both keys once they've exchanged
    /// them, both run the identical comparison, and so they always land on
    /// opposite roles without any extra negotiation. (Equal keys would
    /// mean both generated the same X25519 keypair, which does not
    /// happen; the connection would be broken anyway if it did.)
    pub async fn punch(
        socket: Arc<UdpSocket>,
        remote: SocketAddr,
        timeout: Duration,
        identity: &IdentityKeyPair,
    ) -> Result<Self, NetError> {
        let hs = HandshakeInit::new();
        let my_public = hs.ephemeral_public();
        let packet = encode(FrameType::Handshake, 0, &hs.message(identity).encode());

        let deadline = tokio::time::Instant::now() + timeout;
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            if tokio::time::Instant::now() >= deadline {
                return Err(NetError::PunchTimeout);
            }
            socket.send_to(&packet, remote).await?;

            let recv = tokio::time::timeout(Duration::from_millis(250), socket.recv_from(&mut buf)).await;
            let Ok(Ok((n, from))) = recv else { continue };
            if from != remote {
                continue;
            }
            let Ok(raw) = decode(&buf[..n]) else { continue };
            if raw.frame_type != FrameType::Handshake {
                continue;
            }
            let Ok(their_message) = HandshakeMessage::decode(&raw.payload) else { continue };
            let their_public = their_message.ephemeral;

            // We have theirs, but they may not have ours yet -- our first
            // sends could have been dropped by their NAT before the hole
            // existed. Keep offering for a moment so they can complete too.
            for _ in 0..6 {
                let _ = socket.send_to(&packet, remote).await;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }

            let initiator = my_public > their_public;
            let done = hs
                .complete(identity, &their_message, initiator)
                .map_err(|_| NetError::BadHandshake)?;
            return Ok(Self {
                socket,
                remote,
                session: Mutex::new(done.session),
                peer: done.peer,
            });
        }
    }

    /// Waits for an incoming handshake from any peer on `socket` and
    /// completes it. Call this on the side receiving the call.
    pub async fn listen(
        socket: Arc<UdpSocket>,
        identity: &IdentityKeyPair,
    ) -> Result<Self, NetError> {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await?;
            let Ok(raw) = decode(&buf[..n]) else { continue };
            if raw.frame_type != FrameType::Handshake {
                continue;
            }
            let Ok(their_message) = HandshakeMessage::decode(&raw.payload) else { continue };

            let hs = HandshakeInit::new();
            let reply = encode(FrameType::Handshake, 0, &hs.message(identity).encode());
            socket.send_to(&reply, from).await?;

            let done = hs
                .complete(identity, &their_message, false)
                .map_err(|_| NetError::BadHandshake)?;
            return Ok(Self {
                socket,
                remote: from,
                session: Mutex::new(done.session),
                peer: done.peer,
            });
        }
    }

    pub async fn send_audio(&self, plaintext: &[u8]) -> Result<(), NetError> {
        let sealed = {
            let mut session = self.session.lock().await;
            session.encrypt(plaintext, b"audio")?
        };
        let packet = encode(FrameType::Audio, sealed.counter, &sealed.ciphertext);
        self.socket.send_to(&packet, self.remote).await?;
        Ok(())
    }

    /// Sends an encrypted out-of-band message alongside the audio stream --
    /// e.g. profile info (display name/status), mute/deafen state, anything
    /// that isn't a mic frame. `net`/`protocol` don't know or care what the
    /// bytes mean; that's up to whichever caller defines a format for them.
    /// A distinct AAD ("control" vs. "audio") stops a captured frame of one
    /// kind from being replayed as the other.
    pub async fn send_control(&self, plaintext: &[u8]) -> Result<(), NetError> {
        let sealed = {
            let mut session = self.session.lock().await;
            session.encrypt(plaintext, b"control")?
        };
        let packet = encode(FrameType::Control, sealed.counter, &sealed.ciphertext);
        self.socket.send_to(&packet, self.remote).await?;
        Ok(())
    }

    /// Sends one file-transfer chunk (spec §5). `plaintext` is the whole
    /// `[transfer_id][chunk_index][bytes]` payload -- chunking/framing is
    /// `antiphon_net::file_transfer`'s job, this just seals and sends it. A
    /// distinct AAD ("file") from audio/control, same reasoning as those.
    pub async fn send_file_chunk(&self, plaintext: &[u8]) -> Result<(), NetError> {
        let sealed = {
            let mut session = self.session.lock().await;
            session.encrypt(plaintext, b"file")?
        };
        let packet = encode(FrameType::File, sealed.counter, &sealed.ciphertext);
        self.socket.send_to(&packet, self.remote).await?;
        Ok(())
    }

    /// Runs forever, decrypting incoming frames from `remote` and handing
    /// each to `on_message` as [`Received::Audio`] or [`Received::Control`].
    /// Frames from any other address, or that fail to decrypt (forged/
    /// corrupted/replayed), are dropped.
    pub async fn recv_loop(self: Arc<Self>, mut on_message: impl FnMut(Received) + Send) -> Result<(), NetError> {
        let mut buf = vec![0u8; MAX_DATAGRAM];
        loop {
            let (n, from) = self.socket.recv_from(&mut buf).await?;
            if from != self.remote {
                continue;
            }
            let Ok(raw) = decode(&buf[..n]) else { continue };
            let aad: &[u8] = match raw.frame_type {
                FrameType::Audio => b"audio",
                FrameType::Control => b"control",
                FrameType::File => b"file",
                FrameType::Handshake => continue, // stray retransmit after the session is already up
            };
            let mut session = self.session.lock().await;
            let Ok(plaintext) = session.decrypt(raw.counter, &raw.payload, aad) else { continue };
            drop(session);
            on_message(match raw.frame_type {
                FrameType::Audio => Received::Audio { counter: raw.counter, plaintext },
                FrameType::File => Received::File(plaintext),
                _ => Received::Control(plaintext),
            });
        }
    }
}

/// One decrypted message off the wire, tagged by what kind of frame it came in as.
pub enum Received {
    /// `counter` is the crypto session's per-message sequence number --
    /// already there for replay rejection, doubling here as the ordering
    /// key for a [`crate::JitterBuffer`].
    Audio { counter: u64, plaintext: Vec<u8> },
    Control(Vec<u8>),
    File(Vec<u8>),
}
