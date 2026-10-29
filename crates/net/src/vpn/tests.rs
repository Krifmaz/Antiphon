//! End-to-end tests on real sockets: a client [`Tunnel`] talks to a small
//! WireGuard "VPN server" on 127.0.0.1 that decrypts, forwards the inner
//! datagram to its real destination from its own socket (NAT, as a
//! provider's server does), and tunnels the replies back.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use tokio::net::UdpSocket;

use super::*;
use crate::vpn::noise::{public_key, WireGuard};
use crate::{PeerConnection, Received};

fn random_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).unwrap();
    k
}

fn b64(k: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(k)
}

const CLIENT_ADDR: Ipv4Addr = Ipv4Addr::new(10, 66, 0, 2);

/// A minimal VPN server. Returns its address and the client config for it.
async fn vpn_server(psk: Option<[u8; 32]>) -> (SocketAddr, WgConfig, tokio::task::JoinHandle<()>) {
    let server_key = random_key();
    let client_key = random_key();
    let outer = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let server_addr = outer.local_addr().unwrap();
    let wg = Arc::new(Mutex::new(WireGuard::new(&server_key, public_key(&client_key), psk, None)));
    let client_outer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();

    let mut conf = format!(
        "[Interface]\nPrivateKey = {}\nAddress = {CLIENT_ADDR}/32\n\n[Peer]\nPublicKey = {}\nAllowedIPs = 0.0.0.0/0\nEndpoint = {server_addr}\n",
        b64(&client_key),
        b64(&public_key(&server_key)),
    );
    if let Some(psk) = psk {
        conf += &format!("PresharedKey = {}\n", b64(&psk));
    }
    let config = WgConfig::parse(&conf).unwrap();

    let task = tokio::spawn(async move {
        // Inner source port -> the server's own egress socket for it.
        let mut nat: HashMap<u16, Arc<UdpSocket>> = HashMap::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let Ok((n, from)) = outer.recv_from(&mut buf).await else { return };
            *client_outer.lock().unwrap() = Some(from);
            let result = wg.lock().unwrap().decapsulate(&buf[..n], Instant::now());
            for d in result.to_network {
                outer.send_to(&d, from).await.unwrap();
            }
            let Some(inner) = result.to_tunnel else { continue };
            let Some(udp) = packet::parse_udp(&inner) else { continue };
            let inner_src = udp.src;
            let egress = match nat.get(&inner_src.port()) {
                Some(s) => s.clone(),
                None => {
                    let s = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
                    nat.insert(inner_src.port(), s.clone());
                    // Replies to the egress socket go back through the tunnel.
                    let (s2, wg2, outer2, client2) = (s.clone(), wg.clone(), outer.clone(), client_outer.clone());
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        while let Ok((n, from)) = s2.recv_from(&mut buf).await {
                            // The reply appears to come from the real sender.
                            let from = SocketAddr::new(from.ip(), from.port());
                            let pkt = packet::build_udp(from, inner_src, &buf[..n]).unwrap();
                            let out = wg2.lock().unwrap().encapsulate(&pkt, Instant::now());
                            let to = client2.lock().unwrap().unwrap();
                            for d in out {
                                outer2.send_to(&d, to).await.unwrap();
                            }
                        }
                    });
                    s
                }
            };
            egress.send_to(udp.payload, udp.dst).await.unwrap();
        }
    });
    (server_addr, config, task)
}

fn voice_only() -> Routes {
    Routes { voice: true, files: false, messages: false }
}

#[tokio::test]
async fn app_datagram_crosses_the_tunnel_and_leaves_from_the_server() {
    let (_, config, _server) = vpn_server(Some(random_key())).await;
    let tunnel = Tunnel::start(config).await.unwrap();
    assert!(tunnel.wait_up(Duration::from_secs(5)).await, "handshake completed");
    assert!(matches!(tunnel.status(), TunnelStatus::Up { .. }));

    // Somebody on the internet, as far as the tunnel is concerned.
    let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let far_addr = far.local_addr().unwrap();

    let router = Router::new(Some(tunnel.clone()), voice_only(), true);
    let socket = router.bind(TrafficKind::Voice, 0).await.unwrap();
    assert!(socket.is_tunneled());
    assert_eq!(socket.local_addr().unwrap().ip(), IpAddr::V4(CLIENT_ADDR));
    socket.send_to(b"hello from inside", far_addr).await.unwrap();

    let mut buf = [0u8; 64];
    let (n, seen_from) = tokio::time::timeout(Duration::from_secs(5), far.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"hello from inside");
    // The far end sees the VPN server's egress, not this machine's tunnel socket.
    assert_ne!(seen_from.port(), tunnel.endpoint().port());

    far.send_to(b"and back", seen_from).await.unwrap();
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), socket.recv_from(&mut buf)).await.unwrap().unwrap();
    assert_eq!(&buf[..n], b"and back");
    assert_eq!(from, far_addr);

    // A kind not routed through the tunnel gets an ordinary socket.
    assert!(!router.bind(TrafficKind::Files, 0).await.unwrap().is_tunneled());
}

#[tokio::test]
async fn a_whole_call_handshake_runs_through_the_tunnel() {
    let (_, config, _server) = vpn_server(None).await;
    let tunnel = Tunnel::start(config).await.unwrap();
    let router = Router::new(Some(tunnel), voice_only(), true);

    let listener_socket = Arc::new(AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
    let listener_addr = listener_socket.local_addr().unwrap();
    let alice = antiphon_crypto::IdentityKeyPair::generate();
    let bob = antiphon_crypto::IdentityKeyPair::generate();
    let listen = tokio::spawn(async move { PeerConnection::listen(listener_socket, &bob).await.unwrap() });

    let caller_socket = Arc::new(router.bind(TrafficKind::Voice, 0).await.unwrap());
    let caller = tokio::time::timeout(
        Duration::from_secs(10),
        PeerConnection::connect(caller_socket, listener_addr, &alice),
    )
    .await
    .unwrap()
    .unwrap();
    let callee = Arc::new(listen.await.unwrap());
    assert_eq!(callee.peer_identity(), alice.public());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(callee.clone().recv_loop(move |m| {
        if let Received::Audio { plaintext, .. } = m {
            let _ = tx.send(plaintext);
        }
    }));
    caller.send_audio(b"a voice frame").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
    assert_eq!(got, b"a voice frame");
}

#[tokio::test]
async fn kill_switch_refuses_rather_than_going_direct() {
    // No tunnel running, voice routed through it, kill switch on.
    let held = Router::new(None, voice_only(), true);
    assert!(matches!(held.bind(TrafficKind::Voice, 0).await, Err(NetError::KillSwitch)));
    assert!(matches!(held.resolve(TrafficKind::Voice, "localhost:1").await, Err(NetError::KillSwitch)));
    // Other kinds are unaffected.
    assert!(!held.bind(TrafficKind::Messages, 0).await.unwrap().is_tunneled());

    // Kill switch off: the same choice falls back to direct.
    let open = Router::new(None, voice_only(), false);
    assert!(!open.bind(TrafficKind::Voice, 0).await.unwrap().is_tunneled());

    // A tunnel that goes down mid-use: its sockets fail, never fall back.
    let (_, config, _server) = vpn_server(None).await;
    let tunnel = Tunnel::start(config).await.unwrap();
    assert!(tunnel.wait_up(Duration::from_secs(5)).await);
    let router = Router::new(Some(tunnel.clone()), voice_only(), true);
    let socket = router.bind(TrafficKind::Voice, 0).await.unwrap();
    let far = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    tunnel.stop("test");
    assert!(socket.send_to(b"leak?", far.local_addr().unwrap()).await.is_err());
    let mut buf = [0u8; 16];
    assert!(socket.recv_from(&mut buf).await.is_err());
    assert!(tokio::time::timeout(Duration::from_millis(300), far.recv_from(&mut buf)).await.is_err());
    assert!(matches!(router.bind(TrafficKind::Voice, 0).await, Err(NetError::KillSwitch)));
    assert!(matches!(tunnel.status(), TunnelStatus::Down(_)));
}

#[tokio::test]
async fn split_tunnel_refuses_destinations_outside_allowed_ips() {
    let (_, mut config, _server) = vpn_server(None).await;
    config.allowed_ips = vec![IpNet { addr: "10.66.0.0".parse().unwrap(), prefix: 16 }];
    let tunnel = Tunnel::start(config).await.unwrap();
    let socket = tunnel.bind(0).unwrap();
    let err = socket.send_to(b"x", "127.0.0.1:9".parse().unwrap()).await.unwrap_err();
    assert!(err.to_string().contains("AllowedIPs"));
}
