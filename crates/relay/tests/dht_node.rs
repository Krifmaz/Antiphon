//! The relay as a DHT node (spec §4): `--dht` serves the DHT on the same
//! socket as relaying and mail, and a relay without it stays silent.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::dht_record::{self, DhtRecord, Whereabouts};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::dht::{self, DhtNode, LookupOptions, NodeKey};
use antiphon_relay::{run_node_with_dht, Relay};
use tokio::net::UdpSocket;

async fn relay(dht_on: bool, bootstrap: &[SocketAddr]) -> SocketAddr {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let node = dht_on.then(|| {
        let config = dht::Config { per_subnet_per_bucket: 64, per_subnet_total: 1024, ..Default::default() };
        Arc::new(Mutex::new(DhtNode::new(NodeKey::generate(dht::POW_BITS), config)))
    });
    if let Some(n) = &node {
        let out = n.lock().unwrap().bootstrap(bootstrap, Instant::now());
        for (to, bytes) in out {
            socket.send_to(&bytes, to).await.unwrap();
        }
        let (n, socket) = (n.clone(), socket.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let out = n.lock().unwrap().tick(Instant::now());
                for (to, bytes) in out {
                    let _ = socket.send_to(&bytes, to).await;
                }
            }
        });
    }
    let relay = Arc::new(Mutex::new(Relay::new(Default::default())));
    tokio::spawn(run_node_with_dht(socket, relay, None, None, node));
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relays_with_dht_on_carry_records() {
    let first = relay(true, &[]).await;
    let mut nodes = vec![first];
    for _ in 0..5 {
        nodes.push(relay(true, &[first]).await);
    }
    tokio::time::sleep(Duration::from_millis(800)).await;

    let alice = IdentityKeyPair::generate();
    let epoch = 20_500;
    let w = Whereabouts { addresses: vec!["198.51.100.4:4000".into()], ..Default::default() };
    let record = DhtRecord::seal(&alice, epoch, 1, &w.encode()).unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    assert!(dht::publish(&client, &[nodes[2]], &record, LookupOptions::default()).await.unwrap() >= 3);

    let key = dht_record::lookup_key(&alice.public().identity_key, epoch).unwrap();
    let found = dht::find(&client, &[nodes[5]], key, LookupOptions::default()).await.unwrap();
    let (_, payload) = dht_record::best(&found, &alice.public().identity_key, epoch, None).unwrap();
    assert_eq!(Whereabouts::decode(&payload).unwrap(), w);
}

#[tokio::test]
async fn a_relay_without_dht_does_not_answer() {
    let plain = relay(false, &[]).await;
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let opts = LookupOptions { overall_timeout: Duration::from_secs(2), request_timeout: Duration::from_millis(400), ..Default::default() };
    assert!(dht::find(&client, &[plain], [1; 32], opts).await.is_err());
}
