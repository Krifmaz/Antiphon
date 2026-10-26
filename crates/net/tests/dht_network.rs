//! A real DHT on real sockets (spec §4): twenty nodes on 127.0.0.1, a
//! record published through one node and found through another, and the
//! three things that must not work -- forging someone's record, rolling it
//! back to an older one, and reading it without their identity key.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use antiphon_crypto::dht_record::{self, DhtRecord, Whereabouts};
use antiphon_crypto::IdentityKeyPair;
use antiphon_net::dht::{self, Config, DhtNode, LookupOptions, Message, NodeKey, Request, StoreResult, POW_BITS};
use tokio::net::UdpSocket;

const NODES: usize = 20;
const EPOCH: u64 = 20_400;

/// Every node shares 127.0.0.0/24, so the per-network limits are opened
/// up here; they have their own unit tests.
fn loopback_config() -> Config {
    Config { per_subnet_per_bucket: 64, per_subnet_total: 1024, ..Config::default() }
}

async fn network() -> (Vec<SocketAddr>, Vec<Arc<Mutex<DhtNode>>>) {
    let mut addrs = Vec::new();
    let mut nodes = Vec::new();
    for i in 0..NODES {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let node = Arc::new(Mutex::new(DhtNode::new(NodeKey::generate(POW_BITS), loopback_config())));
        // Each joins through the first, as nodes started from the same
        // published bootstrap list would.
        if i > 0 {
            let out = node.lock().unwrap().bootstrap(&[addrs[0]], Instant::now());
            for (to, bytes) in out {
                socket.send_to(&bytes, to).await.unwrap();
            }
        }
        tokio::spawn(dht::serve(socket, node.clone()));
        addrs.push(addr);
        nodes.push(node);
    }
    // Let the self-lookups spread. A bucket holds K, so with twenty nodes
    // about half of them land in one bucket and nobody knows everyone.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if nodes.iter().all(|n| n.lock().unwrap().stats().nodes >= 6) {
            break;
        }
    }
    (addrs, nodes)
}

fn whereabouts(addr: &str) -> Whereabouts {
    Whereabouts { addresses: vec![addr.into()], relay: String::new(), route_record: vec![] }
}

async fn client() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn twenty_nodes_carry_a_record_from_one_contact_to_another() {
    let (addrs, nodes) = network().await;
    for (i, n) in nodes.iter().enumerate() {
        assert!(n.lock().unwrap().stats().nodes >= 6, "node {i} only knows {}", n.lock().unwrap().stats().nodes);
    }
    let opts = LookupOptions::default();
    let alice = IdentityKeyPair::generate();
    let alice_key = alice.public().identity_key;

    // Alice publishes "where I am now" through node 3.
    let record = DhtRecord::seal(&alice, EPOCH, 10, &whereabouts("198.51.100.20:4100").encode()).unwrap();
    let stored = dht::publish(&client().await, &[addrs[3]], &record, opts).await.unwrap();
    assert!(stored >= dht::K / 2, "stored at {stored} nodes");

    // Bob, who knows only her identity key, finds it through node 17.
    let key = dht_record::lookup_key(&alice_key, EPOCH).unwrap();
    let found = dht::find(&client().await, &[addrs[17]], key, opts).await.unwrap();
    let (sequence, payload) = dht_record::best(&found, &alice_key, EPOCH, None).expect("found and opened");
    assert_eq!(sequence, 10);
    assert_eq!(Whereabouts::decode(&payload).unwrap().addresses, vec!["198.51.100.20:4100".to_string()]);

    // Her address changes: a higher sequence replaces it everywhere.
    let moved = DhtRecord::seal(&alice, EPOCH, 11, &whereabouts("203.0.113.9:4100").encode()).unwrap();
    assert!(dht::publish(&client().await, &[addrs[8]], &moved, opts).await.unwrap() > 0);
    let found = dht::find(&client().await, &[addrs[12]], key, opts).await.unwrap();
    let (sequence, payload) = dht_record::best(&found, &alice_key, EPOCH, None).unwrap();
    assert_eq!(sequence, 11);
    assert_eq!(Whereabouts::decode(&payload).unwrap().addresses, vec!["203.0.113.9:4100".to_string()]);

    // Rollback: replaying her older (validly signed) record is refused by
    // every node that holds the newer one.
    assert_eq!(dht::publish(&client().await, &[addrs[5]], &record, opts).await.unwrap(), 0);
    let found = dht::find(&client().await, &[addrs[1]], key, opts).await.unwrap();
    assert_eq!(dht_record::best(&found, &alice_key, EPOCH, None).unwrap().0, 11);
    // And a reader who already saw 11 accepts nothing older or equal.
    assert!(dht_record::best(&found, &alice_key, EPOCH, Some(11)).is_none());

    // Forgery: Mallory files a record under Alice's key with a higher
    // sequence. Nodes refuse it, and it never reaches a reader.
    let mallory = IdentityKeyPair::generate();
    let mut forged = DhtRecord::seal(&mallory, EPOCH, 99, &whereabouts("192.0.2.66:6666").encode()).unwrap();
    forged.blinded = dht_record::blinded_key(&alice_key, EPOCH).unwrap();
    forged.key = key;
    assert_eq!(dht::publish(&client().await, &[addrs[2]], &forged, opts).await.unwrap(), 0);
    // Straight at a node, bypassing the client: refused as invalid.
    let sock = client().await;
    sock.send_to(&Message::encode_request([1; 8], None, &Request::Store(forged)), addrs[0]).await.unwrap();
    let mut buf = vec![0u8; 4096];
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf)).await.unwrap().unwrap();
    match Message::decode(&buf[..n]) {
        Some(Message::Reply { body: dht::Reply::Stored(result), .. }) => assert_eq!(result, StoreResult::Invalid),
        other => panic!("{other:?}"),
    }
    let found = dht::find(&client().await, &[addrs[19]], key, opts).await.unwrap();
    assert!(found.iter().all(|r| r.sequence <= 11), "no forged record is served");

    // Without Alice's identity key, a reader can neither compute the key
    // nor open what is stored under it.
    let stranger = IdentityKeyPair::generate().public().identity_key;
    for r in &found {
        assert!(r.open(&stranger, EPOCH).is_err());
    }
    assert!(dht_record::best(&found, &stranger, EPOCH, None).is_none());
    let nothing =
        dht::find(&client().await, &[addrs[4]], dht_record::lookup_key(&stranger, EPOCH).unwrap(), opts).await.unwrap();
    assert!(nothing.is_empty());

    // The nodes holding it hold nothing that names Alice.
    let holders = nodes.iter().filter(|n| n.lock().unwrap().stats().records > 0).count();
    assert!(holders >= dht::K / 2, "{holders} holders");
    assert!(!record.encode().windows(32).any(|w| w == alice_key));
}

#[tokio::test]
async fn a_dead_bootstrap_list_is_an_error_not_a_hang() {
    let dead = client().await.local_addr().unwrap(); // bound, then dropped: nobody answers
    let opts = LookupOptions { overall_timeout: Duration::from_secs(3), request_timeout: Duration::from_millis(300), ..Default::default() };
    let started = Instant::now();
    let result = dht::find(&client().await, &[dead], [0; 32], opts).await;
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(4));
}
