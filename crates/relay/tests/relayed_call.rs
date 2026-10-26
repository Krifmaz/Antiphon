//! A real relayed call over loopback sockets: the listener registers, the
//! caller dials, and an unmodified `PeerConnection` handshakes and carries
//! sealed audio through the relay without either side ever learning the
//! other's address.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::relay_token;
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::{relay, PeerConnection, Received};
use antiphon_relay::{run, Limits, Relay};
use tokio::net::UdpSocket;

#[tokio::test]
async fn a_call_goes_through_the_relay_end_to_end() {
    let relay_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_addr = relay_socket.local_addr().unwrap();
    let state = Arc::new(Mutex::new(Relay::new(Limits::default())));
    tokio::spawn(run(relay_socket, state.clone()));

    let alice = Arc::new(IdentityKeyPair::generate());
    let bob = Arc::new(IdentityKeyPair::generate());
    let day = 20_000;
    let bob_token = relay_token::pairwise(&bob, &alice.public().prekey, day);
    let alice_token = relay_token::pairwise(&alice, &bob.public().prekey, day);
    assert_eq!(bob_token, alice_token);

    // Bob listens, registered at the relay.
    let bob_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    relay::register(&bob_socket, relay_addr, &[bob_token]).await.unwrap();
    // Let the registration land before Alice dials.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bob_listen = {
        let socket = bob_socket.clone();
        let bob = bob.clone();
        tokio::spawn(async move { PeerConnection::listen(socket, &bob).await })
    };

    // Alice dials the token and calls the relay as if it were Bob.
    let alice_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    relay::dial(&alice_socket, relay_addr, alice_token, Duration::from_secs(3)).await.unwrap();
    let alice_conn = tokio::time::timeout(
        Duration::from_secs(5),
        PeerConnection::connect(alice_socket, relay_addr, &alice),
    )
    .await
    .expect("handshake through the relay timed out")
    .unwrap();
    let bob_conn = bob_listen.await.unwrap().unwrap();

    // Each authenticated the other, through a relay that saw neither key.
    assert_eq!(alice_conn.peer_identity(), bob.public());
    assert_eq!(bob_conn.peer_identity(), alice.public());
    assert_eq!(bob_conn.remote_addr(), relay_addr);

    let bob_conn = Arc::new(bob_conn);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(bob_conn.clone().recv_loop(move |m| {
        if let Received::Audio { plaintext, .. } = m {
            let _ = tx.send(plaintext);
        }
    }));
    alice_conn.send_audio(b"hello through the relay").await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.unwrap().unwrap();
    assert_eq!(got, b"hello through the relay");
    assert!(state.lock().unwrap().stats().forwarded >= 3);
}

#[tokio::test]
async fn dialling_a_token_nobody_registered_fails_fast() {
    let relay_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let relay_addr = relay_socket.local_addr().unwrap();
    tokio::spawn(run(relay_socket, Arc::new(Mutex::new(Relay::new(Limits::default())))));
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let err = relay::dial(&socket, relay_addr, [1; 32], Duration::from_secs(3)).await.unwrap_err();
    assert!(matches!(err, antiphon_net::NetError::RelayNotFound), "{err}");
}
