//! Anonymity mode over real loopback sockets: a caller reaches a listener
//! through two hop nodes and a target relay -- three nodes in all -- and an
//! unmodified `PeerConnection` handshakes and carries sealed audio over
//! the circuit. The target relay is instrumented to record every address
//! it hears from, to show the caller's never reaches it.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::onion::HopSecret;
use antiphon_crypto::{relay_token, IdentityKeyPair};
use antiphon_net::onion::{connect_via, fetch_hop_key, OnionShim};
use antiphon_net::{relay, NetError, PeerConnection, Received};
use antiphon_relay::onion::{HopLimits, HopNode};
use antiphon_relay::{run, run_node_with_hops, Limits, Relay};
use tokio::net::UdpSocket;

/// How long any single wait in these tests may take before it fails.
const T: Duration = Duration::from_secs(5);

/// A hop node on loopback. Loopback is "private", so it has to be let in.
async fn hop_node() -> (SocketAddr, Arc<Mutex<HopNode>>) {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let limits = HopLimits { allow_private: true, ..HopLimits::default() };
    let hops = Arc::new(Mutex::new(HopNode::new(HopSecret::generate(), limits)));
    let relay = Arc::new(Mutex::new(Relay::new(Limits::default())));
    tokio::spawn(run_node_with_hops(socket, relay, None, None, Some(hops.clone())));
    (addr, hops)
}

/// A plain relay that writes down who it hears from -- what an operator
/// who *did* keep logs would have.
async fn logging_relay() -> (SocketAddr, Arc<Mutex<HashSet<SocketAddr>>>) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(HashSet::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut relay = Relay::new(Limits::default());
        let mut buf = vec![0u8; 65_536];
        loop {
            let (n, from) = socket.recv_from(&mut buf).await.unwrap();
            log.lock().unwrap().insert(from);
            for (to, bytes) in relay.handle(from, &buf[..n], Instant::now()) {
                let _ = socket.send_to(&bytes, to).await;
            }
        }
    });
    (addr, seen)
}

/// Fetches each node's hop key from one probe socket, in path order.
async fn hop_keys(nodes: &[SocketAddr]) -> Vec<(SocketAddr, [u8; 32])> {
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut out = Vec::new();
    for &n in nodes {
        out.push((n, fetch_hop_key(&probe, n, T).await.unwrap()));
    }
    out
}

/// Sends audio both ways over an established pair and checks it arrives.
async fn exchange_audio(caller: PeerConnection, listener: PeerConnection) {
    let caller = Arc::new(caller);
    let listener = Arc::new(listener);
    let (to_listener, mut at_listener) = tokio::sync::mpsc::unbounded_channel();
    let (to_caller, mut at_caller) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(listener.clone().recv_loop(move |m| {
        if let Received::Audio { plaintext, .. } = m {
            let _ = to_listener.send(plaintext);
        }
    }));
    tokio::spawn(caller.clone().recv_loop(move |m| {
        if let Received::Audio { plaintext, .. } = m {
            let _ = to_caller.send(plaintext);
        }
    }));
    for i in 0..5u8 {
        caller.send_audio(&[b'c', i]).await.unwrap();
        listener.send_audio(&[b'l', i]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got_l = Vec::new();
    let mut got_c = Vec::new();
    for _ in 0..5 {
        got_l.push(tokio::time::timeout(T, at_listener.recv()).await.unwrap().unwrap());
        got_c.push(tokio::time::timeout(T, at_caller.recv()).await.unwrap().unwrap());
    }
    got_l.sort();
    got_c.sort();
    assert_eq!(got_l, (0..5u8).map(|i| vec![b'c', i]).collect::<Vec<_>>());
    assert_eq!(got_c, (0..5u8).map(|i| vec![b'l', i]).collect::<Vec<_>>());
}

/// Every loopback address the caller's side of a shim uses: the call
/// socket, the stand-in relay address and the circuit socket. None of
/// them may ever reach the target relay.
fn shim_addrs(shim: &OnionShim) -> [SocketAddr; 3] {
    [shim.socket().local_addr().unwrap(), shim.relay_addr(), shim.circuit_addr()]
}

#[tokio::test]
async fn a_call_through_two_hops_never_shows_the_caller_to_the_target_relay() {
    let (h1, h1_state) = hop_node().await;
    let (h2, h2_state) = hop_node().await;
    let (target, seen) = logging_relay().await;
    let path = hop_keys(&[h1, h2]).await;

    let alice = Arc::new(IdentityKeyPair::generate());
    let bob = Arc::new(IdentityKeyPair::generate());
    let token = relay_token::pairwise(&bob, &alice.public().prekey, 20_000);

    // Bob listens at the target relay the ordinary way.
    let bob_socket = Arc::new(antiphon_net::AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
    relay::register(&bob_socket, target, &[token]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bob_listen = {
        let (socket, bob) = (bob_socket.clone(), bob.clone());
        tokio::spawn(async move { PeerConnection::listen(socket, &bob).await })
    };

    // Alice builds a circuit H1 -> H2 -> target and then runs the very
    // same dial-and-connect code as a plain relayed call, on the shim.
    let shim = connect_via(&path, target).await.unwrap();
    relay::dial(&shim.socket(), shim.relay_addr(), token, T).await.unwrap();
    let alice_conn = tokio::time::timeout(T, PeerConnection::connect(shim.socket(), shim.relay_addr(), &alice))
        .await
        .expect("handshake over the circuit timed out")
        .unwrap();
    let bob_conn = bob_listen.await.unwrap().unwrap();

    // Each authenticated the other end to end, through four sockets that
    // hold no call key.
    assert_eq!(alice_conn.peer_identity(), bob.public());
    assert_eq!(bob_conn.peer_identity(), alice.public());
    assert_eq!(bob_conn.remote_addr(), target);

    exchange_audio(alice_conn, bob_conn).await;

    // The target relay heard from exactly two addresses: Bob, and one
    // that is none of Alice's -- nor either hop node's own socket. It is
    // the exit's per-circuit socket.
    let seen = seen.lock().unwrap().clone();
    let bob_addr = bob_socket.local_addr().unwrap();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert!(seen.contains(&bob_addr));
    let caller_as_seen = *seen.iter().find(|a| **a != bob_addr).unwrap();
    for alice_addr in shim_addrs(&shim) {
        assert_ne!(caller_as_seen, alice_addr);
    }
    assert_ne!(caller_as_seen, h1);
    assert_ne!(caller_as_seen, h2);

    // H1 carried the circuit but is no exit; H2 is the only exit.
    let (s1, s2) = (h1_state.lock().unwrap().stats(), h2_state.lock().unwrap().stats());
    assert_eq!((s1.circuits, s1.exits), (1, 0));
    assert_eq!((s2.circuits, s2.exits), (1, 1));
    assert!(s1.cells >= 10 && s2.cells >= 10, "{s1:?} {s2:?}");

    // Dropping the shim tears the circuit down at both hops.
    drop(shim);
    let deadline = Instant::now() + T;
    while h2_state.lock().unwrap().stats().circuits != 0 {
        assert!(Instant::now() < deadline, "DESTROY never reached the exit");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h1_state.lock().unwrap().stats().circuits, 0);
}

#[tokio::test]
async fn both_ends_can_hide_behind_circuits() {
    let (h1, _) = hop_node().await;
    let (h2, _) = hop_node().await;
    let (h3, _) = hop_node().await;
    let (target, seen) = logging_relay().await;
    let keys = hop_keys(&[h1, h2, h3]).await;

    let alice = IdentityKeyPair::generate();
    let bob = Arc::new(IdentityKeyPair::generate());
    let token = relay_token::pairwise(&bob, &alice.public().prekey, 20_000);

    // Bob registers and listens over a three-hop circuit of his own.
    let bob_shim = connect_via(&[keys[2], keys[1], keys[0]], target).await.unwrap();
    relay::register(&bob_shim.socket(), bob_shim.relay_addr(), &[token]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let bob_listen = {
        let (socket, bob) = (bob_shim.socket(), bob.clone());
        tokio::spawn(async move { PeerConnection::listen(socket, &bob).await })
    };

    let alice_shim = connect_via(&[keys[0], keys[1]], target).await.unwrap();
    relay::dial(&alice_shim.socket(), alice_shim.relay_addr(), token, T).await.unwrap();
    let alice_conn = tokio::time::timeout(
        T,
        PeerConnection::connect(alice_shim.socket(), alice_shim.relay_addr(), &alice),
    )
    .await
    .unwrap()
    .unwrap();
    let bob_conn = bob_listen.await.unwrap().unwrap();
    assert_eq!(alice_conn.peer_identity(), bob.public());
    exchange_audio(alice_conn, bob_conn).await;

    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "one exit socket per circuit: {seen:?}");
    for shim in [&alice_shim, &bob_shim] {
        for a in shim_addrs(shim) {
            assert!(!seen.contains(&a));
        }
    }
}

#[tokio::test]
async fn a_node_without_hops_says_so() {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    tokio::spawn(run(socket, Arc::new(Mutex::new(Relay::new(Limits::default())))));
    let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let err = fetch_hop_key(&probe, addr, T).await.unwrap_err();
    assert!(matches!(err, NetError::OnionRefused), "{err}");
    // ...and so does a CREATE sent to it anyway.
    let fake = HopSecret::generate().public();
    let err = connect_via(&[(addr, fake), (addr, fake)], addr).await.err().unwrap();
    assert!(matches!(err, NetError::OnionRefused), "{err}");
}

/// A hop node whose state can be wiped, as a restart with a kept key does.
async fn restartable_hop() -> (SocketAddr, Arc<Mutex<HopNode>>, [u8; 32]) {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let secret = HopSecret::generate();
    let bytes = secret.to_bytes();
    let limits = HopLimits { allow_private: true, ..HopLimits::default() };
    let hops = Arc::new(Mutex::new(HopNode::new(secret, limits)));
    let relay = Arc::new(Mutex::new(Relay::new(Limits::default())));
    tokio::spawn(run_node_with_hops(socket, relay, None, None, Some(hops.clone())));
    (addr, hops, bytes)
}

#[tokio::test]
async fn a_circuit_rebuilds_itself_after_a_hop_restarts() {
    let (h1, _, _) = restartable_hop().await;
    let (h2, node2, key2) = restartable_hop().await;
    let (target, _) = logging_relay().await;
    let shim = connect_via(&hop_keys(&[h1, h2]).await, target).await.unwrap();

    // A listener at the target, so a dial has someone to find.
    let token = [3u8; 32];
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    relay::register(&listener, target, &[token]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    relay::dial(&shim.socket(), shim.relay_addr(), token, T).await.expect("dial over the circuit");

    // The exit restarts: same key, no memory of the circuit.
    let limits = HopLimits { allow_private: true, ..HopLimits::default() };
    *node2.lock().unwrap() = HopNode::new(HopSecret::from_bytes(key2), limits);

    // The next cell meets "unknown circuit", the shim rebuilds, and a dial
    // through the same shim works again.
    // (A second listener: the first counts as busy in its fresh pairing.)
    let second = [4u8; 32];
    let listener2 = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    relay::register(&listener2, target, &[second]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    relay::dial(&shim.socket(), shim.relay_addr(), second, Duration::from_secs(10)).await.expect("dial after the rebuild");
    assert_eq!(node2.lock().unwrap().stats().circuits, 1, "the exit holds the rebuilt circuit");
}
