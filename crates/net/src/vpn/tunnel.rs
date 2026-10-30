//! A running WireGuard tunnel and the UDP sockets that live inside it.
//!
//! Nothing here touches the operating system's routing: there is no TUN
//! device, no system VPN, no administrator prompt (spec §6 "userspace
//! WireGuard inside the app, not a system-level VPN"). The tunnel is one
//! ordinary UDP socket to the VPN server; app sockets that are routed
//! through it are [`TunnelSocket`]s, whose datagrams are wrapped in IP and
//! UDP headers, encrypted, and sent over that one socket. The rest of the
//! machine is unaffected, and so is any app traffic not routed through it.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::config::WgConfig;
use super::noise::WireGuard;
use super::packet;
use crate::error::NetError;

/// Datagrams waiting for one tunnel socket's reader. Past this they are
/// dropped, which is what a real socket buffer does too.
const SOCKET_QUEUE: usize = 1024;
const TICK: Duration = Duration::from_millis(250);

type Inbox = mpsc::Sender<(Vec<u8>, SocketAddr)>;

/// What the tunnel is doing, for the settings screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TunnelStatus {
    /// Started, no handshake yet: the server has not answered.
    Connecting,
    /// A handshake completed this long ago. WireGuard rekeys every two
    /// minutes while in use, so a large number means the server stopped
    /// answering.
    Up { last_handshake: Duration },
    /// Stopped, with the reason.
    Down(String),
}

pub struct Tunnel {
    config: WgConfig,
    endpoint: SocketAddr,
    udp: Arc<UdpSocket>,
    wg: Mutex<WireGuard>,
    ports: Mutex<HashMap<u16, Inbox>>,
    running: AtomicBool,
    stopped_because: Mutex<Option<String>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

fn io_err(kind: io::ErrorKind, msg: impl Into<String>) -> io::Error {
    io::Error::new(kind, msg.into())
}

impl Tunnel {
    /// Resolves the server, opens the one real socket, and starts the
    /// handshake. Returns without waiting for the server to answer; see
    /// [`Self::wait_up`].
    pub async fn start(config: WgConfig) -> Result<Arc<Self>, NetError> {
        let endpoint = tokio::net::lookup_host(config.endpoint.as_str())
            .await?
            .next()
            .ok_or_else(|| io_err(io::ErrorKind::NotFound, format!("{} did not resolve", config.endpoint)))?;
        let bind: SocketAddr = if endpoint.is_ipv4() { "0.0.0.0:0".parse().unwrap() } else { "[::]:0".parse().unwrap() };
        let udp = Arc::new(UdpSocket::bind(bind).await?);
        udp.connect(endpoint).await?;

        let wg = WireGuard::new(
            &config.private_key,
            config.peer_public_key,
            config.preshared_key.as_deref().copied(),
            config.persistent_keepalive,
        );
        let tunnel = Arc::new(Self {
            config,
            endpoint,
            udp: udp.clone(),
            wg: Mutex::new(wg),
            ports: Mutex::new(HashMap::new()),
            running: AtomicBool::new(true),
            stopped_because: Mutex::new(None),
            task: Mutex::new(None),
        });

        let first = tunnel.wg.lock().unwrap().initiate(Instant::now());
        let _ = udp.send(&first).await;

        let weak = Arc::downgrade(&tunnel);
        let handle = tokio::spawn(io_loop(weak, udp));
        *tunnel.task.lock().unwrap() = Some(handle);
        Ok(tunnel)
    }

    pub fn config(&self) -> &WgConfig {
        &self.config
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn status(&self) -> TunnelStatus {
        if !self.is_running() {
            let why = self.stopped_because.lock().unwrap().clone();
            return TunnelStatus::Down(why.unwrap_or_else(|| "stopped".into()));
        }
        match self.wg.lock().unwrap().last_handshake() {
            Some(t) => TunnelStatus::Up { last_handshake: t.elapsed() },
            None => TunnelStatus::Connecting,
        }
    }

    /// Waits until the server has completed a handshake, or `timeout`.
    pub async fn wait_up(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.is_running() {
                return false;
            }
            if self.wg.lock().unwrap().last_handshake().is_some() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// Stops the tunnel. Every socket inside it fails from now on -- they
    /// never fall back to the real connection (spec §6 "kill switch").
    pub fn stop(&self, reason: &str) {
        self.running.store(false, Ordering::Release);
        self.stopped_because.lock().unwrap().get_or_insert_with(|| reason.to_string());
        // Dropping the senders ends every reader's `recv_from`.
        self.ports.lock().unwrap().clear();
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }

    /// Opens a UDP socket inside the tunnel. `port` 0 picks a free one.
    pub fn bind(self: &Arc<Self>, port: u16) -> io::Result<TunnelSocket> {
        if !self.is_running() {
            return Err(io_err(io::ErrorKind::NotConnected, "the VPN tunnel is down"));
        }
        let (tx, rx) = mpsc::channel(SOCKET_QUEUE);
        let mut ports = self.ports.lock().unwrap();
        let port = if port != 0 {
            if ports.contains_key(&port) {
                return Err(io_err(io::ErrorKind::AddrInUse, format!("port {port} is in use inside the tunnel")));
            }
            port
        } else {
            let mut seed = [0u8; 2];
            getrandom::fill(&mut seed).map_err(|_| io_err(io::ErrorKind::Other, "no randomness"))?;
            let start = u16::from_le_bytes(seed);
            (0..16384u16)
                .map(|i| 49152 + (start.wrapping_add(i) % 16384))
                .find(|p| !ports.contains_key(p))
                .ok_or_else(|| io_err(io::ErrorKind::AddrInUse, "no free port inside the tunnel"))?
        };
        ports.insert(port, tx);
        Ok(TunnelSocket { tunnel: self.clone(), port, rx: tokio::sync::Mutex::new(rx) })
    }

    async fn send_inner(&self, payload: &[u8], src_port: u16, dst: SocketAddr) -> io::Result<usize> {
        if !self.is_running() {
            return Err(io_err(io::ErrorKind::NotConnected, "the VPN tunnel is down"));
        }
        if !self.config.routes(dst.ip()) {
            return Err(io_err(
                io::ErrorKind::AddrNotAvailable,
                format!("{} is outside this VPN's AllowedIPs", dst.ip()),
            ));
        }
        let src_ip = self.config.source_for(dst.ip()).ok_or_else(|| {
            io_err(io::ErrorKind::AddrNotAvailable, format!("this VPN has no address to reach {} from", dst.ip()))
        })?;
        let inner = packet::build_udp(SocketAddr::new(src_ip, src_port), dst, payload)
            .ok_or_else(|| io_err(io::ErrorKind::InvalidInput, "datagram too large"))?;
        let outer = self.wg.lock().unwrap().encapsulate(&inner, Instant::now());
        for datagram in outer {
            self.udp.send(&datagram).await?;
        }
        Ok(payload.len())
    }

    fn deliver(&self, inner: &[u8]) {
        let Some(udp) = packet::parse_udp(inner) else { return };
        if !self.config.addresses.iter().any(|a| a.addr == udp.dst.ip()) {
            return;
        }
        let ports = self.ports.lock().unwrap();
        if let Some(inbox) = ports.get(&udp.dst.port()) {
            let _ = inbox.try_send((udp.payload.to_vec(), udp.src));
        }
    }

    /// Looks a name up through the tunnel, using the config's `DNS`
    /// servers, so the lookup does not go to the local network's resolver
    /// either. With no `DNS` line it falls back to the system resolver.
    pub async fn resolve(self: &Arc<Self>, host_port: &str) -> io::Result<SocketAddr> {
        if let Ok(addr) = host_port.parse::<SocketAddr>() {
            return Ok(addr);
        }
        let Some(&server) = self.config.dns.iter().find(|d| self.config.source_for(**d).is_some()) else {
            return tokio::net::lookup_host(host_port)
                .await?
                .next()
                .ok_or_else(|| io_err(io::ErrorKind::NotFound, format!("{host_port} did not resolve")));
        };
        let (host, port) = host_port
            .rsplit_once(':')
            .and_then(|(h, p)| Some((h, p.parse::<u16>().ok()?)))
            .ok_or_else(|| io_err(io::ErrorKind::InvalidInput, format!("{host_port} needs a port")))?;
        let want_v6 = self.config.source_for("8.8.8.8".parse().unwrap()).is_none();
        let socket = self.bind(0)?;
        let query = dns_query(host, want_v6)?;
        let mut buf = vec![0u8; 1500];
        for _ in 0..3 {
            socket.send_to(&query, SocketAddr::new(server, 53)).await?;
            let Ok(got) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf)).await else {
                continue;
            };
            let (n, _) = got?;
            if let Some(ip) = dns_answer(&buf[..n], &query[..2]) {
                return Ok(SocketAddr::new(ip, port));
            }
            return Err(io_err(io::ErrorKind::NotFound, format!("{host} has no address")));
        }
        Err(io_err(io::ErrorKind::TimedOut, "the VPN's DNS server did not answer"))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

async fn io_loop(tunnel: Weak<Tunnel>, udp: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 65536];
    let mut ticker = tokio::time::interval(TICK);
    let mut errors = 0u32;
    loop {
        tokio::select! {
            got = udp.recv(&mut buf) => {
                let Some(t) = tunnel.upgrade() else { return };
                match got {
                    Ok(n) => {
                        errors = 0;
                        let result = t.wg.lock().unwrap().decapsulate(&buf[..n], Instant::now());
                        for datagram in result.to_network {
                            let _ = udp.send(&datagram).await;
                        }
                        if let Some(inner) = result.to_tunnel {
                            t.deliver(&inner);
                        }
                    }
                    // A port-unreachable from the server shows up here
                    // (and as a reset on Windows). Transient as far as we
                    // know; the handshake timers decide when it is dead.
                    Err(_) => {
                        errors += 1;
                        if errors > 1000 {
                            t.stop("the connection to the VPN server keeps failing");
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            _ = ticker.tick() => {
                let Some(t) = tunnel.upgrade() else { return };
                let out = t.wg.lock().unwrap().tick(Instant::now());
                for datagram in out {
                    let _ = udp.send(&datagram).await;
                }
            }
        }
    }
}

/// A UDP socket inside the tunnel. Its datagrams leave from the VPN
/// server's address, never from this machine's.
pub struct TunnelSocket {
    tunnel: Arc<Tunnel>,
    port: u16,
    rx: tokio::sync::Mutex<mpsc::Receiver<(Vec<u8>, SocketAddr)>>,
}

impl TunnelSocket {
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.tunnel.send_inner(buf, self.port, target).await
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut rx = self.rx.lock().await;
        let (data, from) = rx
            .recv()
            .await
            .ok_or_else(|| io_err(io::ErrorKind::NotConnected, "the VPN tunnel is down"))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok((n, from))
    }

    /// The address inside the tunnel. Not reachable from outside unless
    /// the VPN provider forwards a port to it.
    pub fn local_addr(&self) -> SocketAddr {
        let ip = self.tunnel.config.addresses.first().map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |a| a.addr);
        SocketAddr::new(ip, self.port)
    }

    pub fn tunnel(&self) -> &Arc<Tunnel> {
        &self.tunnel
    }
}

impl Drop for TunnelSocket {
    fn drop(&mut self) {
        self.tunnel.ports.lock().unwrap().remove(&self.port);
    }
}

fn dns_query(host: &str, v6: bool) -> io::Result<Vec<u8>> {
    let mut id = [0u8; 2];
    getrandom::fill(&mut id).map_err(|_| io_err(io::ErrorKind::Other, "no randomness"))?;
    let mut q = vec![id[0], id[1], 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in host.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(io_err(io::ErrorKind::InvalidInput, format!("`{host}` is not a host name")));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&[0, if v6 { 28 } else { 1 }, 0, 1]);
    Ok(q)
}

fn skip_name(msg: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let len = *msg.get(at)? as usize;
        if len == 0 {
            return Some(at + 1);
        }
        if len & 0xc0 == 0xc0 {
            return Some(at + 2);
        }
        at += 1 + len;
    }
}

fn dns_answer(msg: &[u8], id: &[u8]) -> Option<IpAddr> {
    if msg.len() < 12 || &msg[..2] != id || msg[3] & 0x0f != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([msg[4], msg[5]]);
    let answers = u16::from_be_bytes([msg[6], msg[7]]);
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(msg, at)? + 4;
    }
    for _ in 0..answers {
        at = skip_name(msg, at)?;
        let rtype = u16::from_be_bytes([*msg.get(at)?, *msg.get(at + 1)?]);
        let rdlen = u16::from_be_bytes([*msg.get(at + 8)?, *msg.get(at + 9)?]) as usize;
        let data = msg.get(at + 10..at + 10 + rdlen)?;
        match (rtype, rdlen) {
            (1, 4) => return Some(IpAddr::from(<[u8; 4]>::try_from(data).ok()?)),
            (28, 16) => return Some(IpAddr::from(<[u8; 16]>::try_from(data).ok()?)),
            _ => at += 10 + rdlen,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_answer_follows_cnames_and_compression() {
        let query = dns_query("relay.example.org", false).unwrap();
        let mut reply = query.clone();
        reply[2] = 0x81;
        reply[3] = 0x80;
        reply[7] = 2; // two answers
        // CNAME pointing elsewhere, then the A record.
        reply.extend_from_slice(&[0xc0, 12, 0, 5, 0, 1, 0, 0, 0, 60, 0, 2, 0xc0, 12]);
        reply.extend_from_slice(&[0xc0, 12, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4, 203, 0, 113, 9]);
        assert_eq!(dns_answer(&reply, &query[..2]), Some("203.0.113.9".parse().unwrap()));
        assert_eq!(dns_answer(&reply, &[query[0] ^ 1, query[1]]), None, "a reply to some other query");
    }
}
