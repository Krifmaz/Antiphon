//! Posting a sealed wake-up to a push service.
//!
//! **UnifiedPush**: the 368 sealed bytes are the body of an HTTPS POST to
//! the endpoint URL, nothing else, which is what the phone's distributor
//! hands back to the app unchanged. Done with ureq over rustls: pure Rust
//! TLS (with ring underneath), blocking, and small, so it runs on tokio's
//! blocking pool rather than pulling an async HTTP stack into the desktop.
//!
//! **APNs** (feature `apns`): Apple's provider API, HTTP/2 only, with the
//! ES256 provider token from [`crate::apns_token`]. Only a sender that was
//! configured with an Apple key has it; without one, an APNs target is
//! refused with [`SendError::NoApns`] rather than tried.
//!
//! An endpoint URL comes from someone else (a contact's record, or
//! whoever registered an inbox at a node), so posting to it is a request
//! made on their say-so. To keep that from reaching into the sender's own
//! network, a [`Sender`] refuses plain `http://`, refuses hosts that
//! resolve to loopback, private, link-local or other non-public addresses,
//! and never follows redirects, unless it was built with
//! [`Sender::allowing_local`] for tests or a self-hosted distributor on the
//! LAN. (The check resolves the name itself before ureq resolves it again,
//! so a name that changes its answer between the two can still slip past;
//! the redirect and scheme rules hold regardless.)

use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use antiphon_crypto::push::{WakeKind, SEALED_LEN};

use crate::endpoint::Transport;

/// How long one post may take, end to end. A wake-up that takes longer is
/// late enough that the call it announces has likely been given up on.
pub const POST_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a wake-up was not handed to the push service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The endpoint is not one this sender will post to: plain HTTP or a
    /// private address without [`Sender::allowing_local`], or a host that
    /// does not resolve.
    Refused(String),
    /// This transport cannot carry this kind of wake-up (a message to a
    /// VoIP token); see [`Transport::carries`].
    WrongTransport,
    /// An APNs target, and this sender has no Apple key configured.
    NoApns,
    /// The service says the endpoint no longer exists (HTTP 404 or 410).
    /// The phone has unregistered or reinstalled; stop using it.
    Gone,
    /// The service answered with another error status.
    Status(u16, String),
    /// The connection failed or timed out.
    Network(String),
}

impl std::fmt::Display for SendError {
    /// One line, for logs and the desktop's notices.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Refused(why) => write!(f, "refused to post: {why}"),
            SendError::WrongTransport => write!(f, "that push address cannot carry this kind of wake-up"),
            SendError::NoApns => write!(f, "no Apple push key is configured here"),
            SendError::Gone => write!(f, "the push address no longer exists"),
            SendError::Status(code, why) => write!(f, "push service said {code} {why}"),
            SendError::Network(why) => write!(f, "could not reach the push service: {why}"),
        }
    }
}

impl std::error::Error for SendError {}

/// Posts wake-ups. Cheap to clone; every clone shares the connection pool
/// and, with `apns`, the one HTTP/2 connection to Apple.
#[derive(Clone)]
pub struct Sender {
    /// The HTTP client for UnifiedPush.
    agent: ureq::Agent,
    /// Whether plain HTTP and non-public addresses are allowed.
    allow_local: bool,
    /// The APNs connection, when a key is configured.
    #[cfg(feature = "apns")]
    apns: Option<std::sync::Arc<tokio::sync::Mutex<apns::Apns>>>,
}

impl Default for Sender {
    /// Public HTTPS endpoints only, no APNs.
    fn default() -> Self {
        Self::new()
    }
}

impl Sender {
    /// A sender for public HTTPS endpoints, honouring the system's proxy
    /// settings like any other HTTP client on the machine.
    pub fn new() -> Self {
        Self::build(false)
    }

    /// A sender that also posts to plain HTTP and to private and loopback
    /// addresses, and goes direct rather than through a proxy. For tests,
    /// and for someone running their own distributor on the LAN.
    pub fn allowing_local() -> Self {
        Self::build(true)
    }

    /// The agent: a global timeout, no redirects (a redirect would be a way
    /// around the address check), and errors left to [`Sender::post_unified`]
    /// so the status can be reported exactly.
    fn build(allow_local: bool) -> Self {
        let mut config = ureq::Agent::config_builder()
            .timeout_global(Some(POST_TIMEOUT))
            .max_redirects(0)
            .http_status_as_error(false)
            .https_only(!allow_local)
            .user_agent("antiphon-push");
        if allow_local {
            config = config.proxy(None);
        }
        Self {
            agent: ureq::Agent::new_with_config(config.build()),
            allow_local,
            #[cfg(feature = "apns")]
            apns: None,
        }
    }

    /// Adds APNs, signing with the operator's key. Fails if the key does not
    /// read as a `.p8` file.
    #[cfg(feature = "apns")]
    pub fn with_apns(mut self, config: apns::ApnsConfig) -> Result<Self, String> {
        self.apns = Some(std::sync::Arc::new(tokio::sync::Mutex::new(apns::Apns::new(config)?)));
        Ok(self)
    }

    /// Whether this sender can reach APNs targets at all, so a caller can
    /// skip sealing for one it could never deliver.
    #[cfg(feature = "apns")]
    pub fn has_apns(&self) -> bool {
        self.apns.is_some()
    }

    /// Whether this sender can reach APNs targets: never, in a build
    /// without the `apns` feature.
    #[cfg(not(feature = "apns"))]
    pub fn has_apns(&self) -> bool {
        false
    }

    /// Sends one sealed wake-up of `kind` the way `transport` says. Runs the
    /// blocking UnifiedPush post on tokio's blocking pool, so it must be
    /// called inside a tokio runtime.
    pub async fn send(&self, kind: WakeKind, transport: &Transport, sealed: &[u8; SEALED_LEN]) -> Result<(), SendError> {
        if !transport.carries(kind) {
            return Err(SendError::WrongTransport);
        }
        match transport {
            Transport::UnifiedPush(url) => {
                let (this, url, sealed) = (self.clone(), url.clone(), *sealed);
                tokio::task::spawn_blocking(move || this.post_unified(&url, kind, &sealed))
                    .await
                    .map_err(|e| SendError::Network(e.to_string()))?
            }
            #[cfg(feature = "apns")]
            Transport::ApnsVoip(token) | Transport::ApnsAlert(token) => {
                let Some(apns) = &self.apns else { return Err(SendError::NoApns) };
                let voip = matches!(transport, Transport::ApnsVoip(_));
                apns.lock().await.send(token, voip, sealed).await
            }
            #[cfg(not(feature = "apns"))]
            Transport::ApnsVoip(_) | Transport::ApnsAlert(_) => Err(SendError::NoApns),
        }
    }

    /// The UnifiedPush post itself, blocking: the sealed bytes as the body,
    /// `application/octet-stream`, with a `TTL` so a call wake-up that
    /// cannot be delivered within a minute is dropped by the service rather
    /// than ringing later, and `Urgency: high` for calls so a distributor
    /// that relays through FCM wakes the phone from Doze.
    pub fn post_unified(&self, url: &str, kind: WakeKind, sealed: &[u8; SEALED_LEN]) -> Result<(), SendError> {
        self.check_url(url)?;
        let (ttl, urgency) = if kind == WakeKind::Call { ("60", "high") } else { ("86400", "normal") };
        let response = self
            .agent
            .post(url)
            .header("Content-Type", "application/octet-stream")
            .header("TTL", ttl)
            .header("Urgency", urgency)
            .send(&sealed[..])
            .map_err(|e| SendError::Network(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else if status.as_u16() == 404 || status.as_u16() == 410 {
            Err(SendError::Gone)
        } else {
            Err(SendError::Status(status.as_u16(), status.canonical_reason().unwrap_or("").to_string()))
        }
    }

    /// Refuses a URL this sender should not post to; see the module comment.
    fn check_url(&self, url: &str) -> Result<(), SendError> {
        let Some(Transport::UnifiedPush(_)) = Transport::parse(url) else {
            return Err(SendError::Refused("not a push URL".into()));
        };
        let (secure, rest) = match url.strip_prefix("https://") {
            Some(rest) => (true, rest),
            None => (false, url.strip_prefix("http://").unwrap_or(url)),
        };
        if self.allow_local {
            return Ok(());
        }
        if !secure {
            return Err(SendError::Refused("plain http".into()));
        }
        let authority = rest.split(['/', '?']).next().unwrap_or("");
        let authority = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
        let addrs = (if authority.starts_with('[') || authority.matches(':').count() == 1 {
            authority.to_socket_addrs()
        } else {
            (authority, 443).to_socket_addrs()
        })
        .map_err(|e| SendError::Refused(format!("{authority} does not resolve: {e}")))?;
        let mut any = false;
        for addr in addrs {
            any = true;
            if !is_public(addr.ip()) {
                return Err(SendError::Refused(format!("{authority} is not a public address")));
            }
        }
        if !any {
            return Err(SendError::Refused(format!("{authority} does not resolve")));
        }
        Ok(())
    }
}

/// Whether an address is on the public internet: not loopback, private,
/// link-local, carrier-grade NAT, unique-local, multicast or unspecified.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00
                || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// Apple's provider API over HTTP/2.
#[cfg(feature = "apns")]
pub mod apns {
    use std::sync::Arc;

    use antiphon_crypto::push::SEALED_LEN;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;

    use super::{SendError, POST_TIMEOUT};
    use crate::apns_token::ProviderToken;

    /// What an operator gives a node to send iOS wake-ups.
    #[derive(Debug, Clone)]
    pub struct ApnsConfig {
        /// The contents of the `.p8` key file from the Apple developer
        /// account.
        pub key_p8: String,
        /// The key's 10-character id.
        pub key_id: String,
        /// The developer account's team id.
        pub team_id: String,
        /// The app's bundle id. VoIP pushes go to `<topic>.voip`, as Apple
        /// requires.
        pub topic: String,
        /// Use Apple's sandbox, for development builds of the app; their
        /// tokens are not valid in production and the other way round.
        pub sandbox: bool,
    }

    /// The live state: the token signer and one reused HTTP/2 connection,
    /// which Apple asks providers to keep open rather than reconnect per
    /// push.
    pub struct Apns {
        /// Signs the provider token.
        token: ProviderToken,
        /// The bundle id.
        topic: String,
        /// `api.push.apple.com` or the sandbox.
        host: &'static str,
        /// The open connection, if there is one.
        conn: Option<h2::client::SendRequest<bytes::Bytes>>,
    }

    impl Apns {
        /// Reads the key; connects lazily on the first push.
        pub fn new(config: ApnsConfig) -> Result<Self, String> {
            Ok(Self {
                token: ProviderToken::from_p8(&config.key_p8, &config.key_id, &config.team_id)?,
                topic: config.topic,
                host: if config.sandbox { "api.sandbox.push.apple.com" } else { "api.push.apple.com" },
                conn: None,
            })
        }

        /// Sends one wake-up to `token`, as a VoIP push or an alert push.
        /// Retries once on a fresh connection if the kept one has gone away.
        pub async fn send(&mut self, token: &str, voip: bool, sealed: &[u8; SEALED_LEN]) -> Result<(), SendError> {
            match tokio::time::timeout(POST_TIMEOUT, self.try_send(token, voip, sealed)).await {
                Ok(Err(SendError::Network(_))) => {
                    self.conn = None;
                    tokio::time::timeout(POST_TIMEOUT, self.try_send(token, voip, sealed))
                        .await
                        .map_err(|_| SendError::Network("timed out".into()))?
                }
                Ok(result) => result,
                Err(_) => {
                    self.conn = None;
                    Err(SendError::Network("timed out".into()))
                }
            }
        }

        /// One attempt: connect if needed, then POST `/3/device/<token>`.
        async fn try_send(&mut self, token: &str, voip: bool, sealed: &[u8; SEALED_LEN]) -> Result<(), SendError> {
            let net = |e: &dyn std::fmt::Display| SendError::Network(e.to_string());
            if self.conn.is_none() {
                self.conn = Some(connect(self.host).await?);
            }
            let jwt = self.token.token(crate::now_secs()).to_string();
            let now = crate::now_secs();
            let (push_type, topic, expiration) = if voip {
                // A ring that arrives half a minute late is worse than none.
                ("voip", format!("{}.voip", self.topic), now + 30)
            } else {
                ("alert", self.topic.clone(), now + 86_400)
            };
            let body = payload(voip, sealed);
            let request = http::Request::builder()
                .method("POST")
                .uri(format!("https://{}/3/device/{token}", self.host))
                .header("authorization", format!("bearer {jwt}"))
                .header("apns-topic", topic)
                .header("apns-push-type", push_type)
                .header("apns-priority", "10")
                .header("apns-expiration", expiration.to_string())
                .body(())
                .map_err(|e| net(&e))?;
            let conn = self.conn.as_mut().expect("connected above");
            let mut ready = conn.clone().ready().await.map_err(|e| net(&e))?;
            let (response, mut stream) = ready.send_request(request, false).map_err(|e| net(&e))?;
            stream.send_data(body.into(), true).map_err(|e| net(&e))?;
            let response = response.await.map_err(|e| net(&e))?;
            let status = response.status().as_u16();
            let mut reason = Vec::new();
            let mut incoming = response.into_body();
            while let Some(chunk) = incoming.data().await {
                let chunk = chunk.map_err(|e| net(&e))?;
                let _ = incoming.flow_control().release_capacity(chunk.len());
                if reason.len() < 1024 {
                    reason.extend_from_slice(&chunk);
                }
            }
            match status {
                200 => Ok(()),
                410 => Err(SendError::Gone),
                _ => Err(SendError::Status(status, String::from_utf8_lossy(&reason).into_owned())),
            }
        }
    }

    /// The JSON body: the sealed bytes in base64 under `"p"`, as the iOS app
    /// reads them. An alert push also needs an `aps` alert to be shown at
    /// all; its text is fixed and generic, because it travels in the clear
    /// to Apple and must not depend on who sent it or what it is.
    pub fn payload(voip: bool, sealed: &[u8; SEALED_LEN]) -> Vec<u8> {
        let p = B64.encode(sealed);
        let value = if voip {
            serde_json::json!({ "p": p })
        } else {
            serde_json::json!({
                "aps": { "alert": { "title": "Stoop", "body": "Something new is waiting" }, "mutable-content": 1 },
                "p": p,
            })
        };
        serde_json::to_vec(&value).expect("a JSON value always serializes")
    }

    /// A TLS connection to `host` with HTTP/2 negotiated by ALPN, as APNs
    /// requires, checked against the Mozilla root set compiled in.
    async fn connect(host: &'static str) -> Result<h2::client::SendRequest<::bytes::Bytes>, SendError> {
        let net = |e: &dyn std::fmt::Display| SendError::Network(e.to_string());
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| net(&e))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let tcp = tokio::net::TcpStream::connect((host, 443)).await.map_err(|e| net(&e))?;
        let name = rustls::pki_types::ServerName::try_from(host).map_err(|e| net(&e))?;
        let stream = tokio_rustls::TlsConnector::from(Arc::new(tls)).connect(name, tcp).await.map_err(|e| net(&e))?;
        let (send, connection) = h2::client::handshake(stream).await.map_err(|e| net(&e))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(send)
    }

    /// The payload's shape for each push type.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// VoIP carries only `p`; alert adds a fixed, generic `aps` alert.
        #[test]
        fn payloads_carry_the_seal_under_p() {
            let sealed = [9u8; SEALED_LEN];
            let voip: serde_json::Value = serde_json::from_slice(&payload(true, &sealed)).unwrap();
            assert_eq!(B64.decode(voip["p"].as_str().unwrap()).unwrap(), sealed.to_vec());
            assert!(voip.get("aps").is_none());
            let alert: serde_json::Value = serde_json::from_slice(&payload(false, &sealed)).unwrap();
            assert_eq!(alert["aps"]["alert"]["title"], "Stoop");
            assert_eq!(B64.decode(alert["p"].as_str().unwrap()).unwrap(), sealed.to_vec());
        }
    }
}

/// The address rules, and a real post to a local HTTP server.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Private, loopback and link-local addresses are not public.
    #[test]
    fn non_public_addresses_are_recognised() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.5.5", "169.254.1.1", "100.64.0.1", "0.0.0.0", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip}");
        }
    }

    /// A normal sender refuses plain HTTP and loopback before connecting.
    #[test]
    fn a_normal_sender_refuses_local_and_plain_http() {
        let sender = Sender::new();
        let sealed = [0u8; SEALED_LEN];
        assert!(matches!(sender.post_unified("http://example.com/x", WakeKind::Call, &sealed), Err(SendError::Refused(_))));
        assert!(matches!(sender.post_unified("https://127.0.0.1/x", WakeKind::Call, &sealed), Err(SendError::Refused(_))));
        assert!(matches!(sender.post_unified("https://[::1]:8443/x", WakeKind::Call, &sealed), Err(SendError::Refused(_))));
    }

    /// A message never goes to a VoIP token, and APNs is refused without a key.
    #[tokio::test]
    async fn transports_are_checked_before_sending() {
        let sender = Sender::new();
        let token = "ab".repeat(32);
        let sealed = [0u8; SEALED_LEN];
        assert_eq!(sender.send(WakeKind::Message, &Transport::ApnsVoip(token.clone()), &sealed).await, Err(SendError::WrongTransport));
        assert_eq!(sender.send(WakeKind::Call, &Transport::ApnsVoip(token), &sealed).await, Err(SendError::NoApns));
    }

    /// Accepts one connection, records the request, answers `status`.
    fn serve_once(status: &'static str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/up/abc?up=1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            // Read the head, then exactly Content-Length bytes of body.
            loop {
                let n = conn.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                if let Some(end) = find(&got, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    while got.len() < end + 4 + len {
                        let n = conn.read(&mut buf).unwrap();
                        got.extend_from_slice(&buf[..n]);
                    }
                    break;
                }
            }
            write!(conn, "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            got
        });
        (url, handle)
    }

    /// Where `needle` first occurs in `hay`.
    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// The post is exactly the sealed bytes, as an octet stream, to the
    /// endpoint's path, with a short TTL for a call.
    #[test]
    fn the_post_body_is_the_sealed_bytes() {
        let (url, server) = serve_once("201 Created");
        let mut sealed = [0u8; SEALED_LEN];
        sealed.iter_mut().enumerate().for_each(|(i, b)| *b = i as u8);
        Sender::allowing_local().post_unified(&url, WakeKind::Call, &sealed).unwrap();
        let got = server.join().unwrap();
        let end = find(&got, b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&got[..end]).to_ascii_lowercase();
        assert!(head.starts_with("post /up/abc?up=1 http/1.1"), "{head}");
        assert!(head.contains("content-type: application/octet-stream"));
        assert!(head.contains("ttl: 60"));
        assert!(head.contains("urgency: high"));
        assert_eq!(&got[end + 4..], &sealed[..]);
    }

    /// A 410 says the endpoint is gone; other errors carry their status.
    #[test]
    fn error_statuses_are_reported() {
        let sealed = [0u8; SEALED_LEN];
        let (url, server) = serve_once("410 Gone");
        assert_eq!(Sender::allowing_local().post_unified(&url, WakeKind::Message, &sealed), Err(SendError::Gone));
        server.join().unwrap();
        let (url, server) = serve_once("429 Too Many Requests");
        assert!(matches!(Sender::allowing_local().post_unified(&url, WakeKind::Message, &sealed), Err(SendError::Status(429, _))));
        server.join().unwrap();
    }

    /// The async path posts through the blocking pool.
    #[tokio::test]
    async fn send_posts_through_the_blocking_pool() {
        let (url, server) = serve_once("200 OK");
        let sealed = [3u8; SEALED_LEN];
        Sender::allowing_local().send(WakeKind::Message, &Transport::UnifiedPush(url), &sealed).await.unwrap();
        let got = server.join().unwrap();
        assert!(got.ends_with(&sealed));
    }
}
