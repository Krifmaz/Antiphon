//! Offline delivery end to end over real sockets: the recipient registers
//! an inbox, the sender seals and deposits, the recipient fetches, opens
//! and acknowledges.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::{mail, IdentityKeyPair};
use antiphon_net::mailbox;
use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
use antiphon_relay::{run_node, Limits, Relay};
use tokio::net::UdpSocket;

const T: Duration = Duration::from_secs(3);

#[tokio::test]
async fn a_message_waits_at_the_node_until_its_recipient_collects_it() {
    let node_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let node = node_socket.local_addr().unwrap();
    let mailbox = Arc::new(Mutex::new(Mailbox::new(MailboxLimits { enabled: true, ..Default::default() })));
    tokio::spawn(run_node(node_socket, Arc::new(Mutex::new(Relay::new(Limits::default()))), Some(mailbox.clone()), None));

    let alice = IdentityKeyPair::generate();
    let bob = IdentityKeyPair::generate();
    let node_name = node.to_string();
    let label = mail::inbox_label(&bob, &node_name);
    let secret = mail::fetch_secret(&bob, &node_name);

    let bob_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    mailbox::register(&bob_sock, node, label, mail::fetch_hash(&secret), b"", T).await.unwrap();

    // Bob goes offline. Alice writes.
    let alice_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let blob = mail::seal(&alice, &bob.public().identity_key, &bob.public().prekey, b"call me when you're back").unwrap();
    mailbox::deposit(&alice_sock, node, label, [1; 16], &blob, T).await.unwrap();
    assert_eq!(mailbox.lock().unwrap().stats().held, 1);

    // Bob comes back.
    let waiting = mailbox::fetch(&bob_sock, node, label, secret, T).await.unwrap();
    assert_eq!(waiting.len(), 1);
    let (from, body) = mail::open(&bob, &waiting[0].1).unwrap();
    assert_eq!(from.identity_key, alice.public().identity_key);
    assert_eq!(body, b"call me when you're back");
    let status = mailbox::status(&bob_sock, node, label, secret, T).await.unwrap();
    assert_eq!(status.held, 1);

    mailbox::ack(&bob_sock, node, label, secret, waiting[0].0, T).await.unwrap();
    assert!(mailbox::fetch(&bob_sock, node, label, secret, T).await.unwrap().is_empty());
    assert_eq!(mailbox.lock().unwrap().stats().held_bytes, 0);
}

#[tokio::test]
async fn a_relay_without_a_mailbox_says_so() {
    let node_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let node = node_socket.local_addr().unwrap();
    tokio::spawn(run_node(node_socket, Arc::new(Mutex::new(Relay::new(Limits::default()))), None, None));
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let err = mailbox::deposit(&sock, node, [0; 32], [0; 16], b"x", T).await.unwrap_err();
    assert!(matches!(err, antiphon_net::NetError::MailboxRefused(mailbox::Refusal::Disabled)), "{err:?}");
}
