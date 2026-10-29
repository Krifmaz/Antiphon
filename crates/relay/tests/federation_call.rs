//! Relay federation end to end, on loopback: a person registered at one
//! relay is rung through another that only learned of the first by
//! peering, and the call's datagrams pass through both untouched.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::federation::PeerKey;
use antiphon_net::relay::{dial, register};
use antiphon_net::NetError;
use antiphon_relay::federation::{FedLimits, Federation};
use antiphon_relay::{run_node_with_federation, Relay};
use tokio::net::UdpSocket;

/// A running federating relay: its address and its federation, to watch.
struct Node {
    /// Where it listens.
    addr: SocketAddr,
    /// Its federation state, for waiting on peers.
    fed: Arc<Mutex<Federation>>,
}

/// Starts a federating relay on loopback that peers with `named`.
async fn node(named: &[&Node]) -> Node {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let limits = FedLimits { allow_private: true, gossip_interval: Duration::from_millis(300), ..FedLimits::default() };
    let pins: Vec<_> = named.iter().map(|n| (n.addr, Some(n.fed.lock().unwrap().public()))).collect();
    let fed = Arc::new(Mutex::new(Federation::new(PeerKey::generate(), limits, &pins, Instant::now())));
    let relay = Arc::new(Mutex::new(Relay::default()));
    tokio::spawn(run_node_with_federation(socket, relay, None, None, None, None, None, Some(fed.clone())));
    Node { addr, fed }
}

/// Waits until `node` peers with at least `n` relays.
async fn peered(node: &Node, n: usize) {
    for _ in 0..100 {
        if node.fed.lock().unwrap().stats().peers >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never peered with {n}: {:?}", node.fed.lock().unwrap().stats());
}

/// Receives one datagram on `socket`, failing after a second.
async fn recv(socket: &UdpSocket) -> (Vec<u8>, SocketAddr) {
    let mut buf = [0u8; 2048];
    let (n, from) = tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buf)).await.unwrap().unwrap();
    (buf[..n].to_vec(), from)
}

#[tokio::test]
async fn a_caller_at_one_relay_reaches_a_listener_at_another() {
    let a = node(&[]).await;
    let b = node(&[&a]).await;
    peered(&b, 1).await;
    peered(&a, 1).await;

    let token = [42u8; 32];
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    register(&listener, a.addr, &[token]).await.unwrap();
    let _ = recv(&listener).await; // REGISTERED

    let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    dial(&caller, b.addr, token, Duration::from_secs(4)).await.expect("bridged through b to a");
    assert_eq!(b.fed.lock().unwrap().stats().bridges, 1);

    // Sealed call traffic, byte for byte, both ways.
    caller.send_to(b"\x01sealed hello", b.addr).await.unwrap();
    let (got, from) = recv(&listener).await;
    assert_eq!(got, b"\x01sealed hello");
    assert_eq!(from, a.addr, "the listener only ever talks to its own relay");
    listener.send_to(b"\x01sealed reply", a.addr).await.unwrap();
    let (got, from) = recv(&caller).await;
    assert_eq!(got, b"\x01sealed reply");
    assert_eq!(from, b.addr, "the caller only ever talks to its own relay");
}

#[tokio::test]
async fn gossip_spreads_the_mesh_so_a_third_relay_finds_the_first() {
    let a = node(&[]).await;
    let b = node(&[&a]).await;
    let c = node(&[&b]).await;
    // c was never told about a: it learns of it from b, and a proves itself.
    peered(&c, 2).await;

    let token = [7u8; 32];
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    register(&listener, a.addr, &[token]).await.unwrap();
    let _ = recv(&listener).await;

    let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    dial(&caller, c.addr, token, Duration::from_secs(4)).await.expect("bridged through c to a");
    caller.send_to(b"\x01x", c.addr).await.unwrap();
    assert_eq!(recv(&listener).await.0, b"\x01x");
}

#[tokio::test]
async fn nobody_in_the_mesh_means_not_found() {
    let a = node(&[]).await;
    let b = node(&[&a]).await;
    peered(&b, 1).await;
    let caller = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let r = dial(&caller, b.addr, [1u8; 32], Duration::from_secs(4)).await;
    assert!(matches!(r, Err(NetError::RelayNotFound)), "{r:?}");
}

#[tokio::test]
async fn an_app_learns_a_relays_peers_as_fallbacks() {
    let a = node(&[]).await;
    let b = node(&[&a]).await;
    peered(&b, 1).await;
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peers = antiphon_net::federation::fetch_peers(&app, b.addr, Duration::from_secs(2)).await.unwrap();
    assert_eq!(peers, vec![a.addr]);
}

#[tokio::test]
async fn a_relay_that_does_not_federate_says_so() {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    tokio::spawn(antiphon_relay::run_node(socket, Arc::new(Mutex::new(Relay::default())), None, None));
    let app = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let r = antiphon_net::federation::fetch_peers(&app, addr, Duration::from_secs(2)).await;
    assert!(matches!(r, Err(NetError::FederationRefused)), "{r:?}");
}
