//! Group calls through a real SFU over loopback sockets: members join a
//! room by its blinded token, seal their frames with SFrame keys the
//! SFU never sees, and each frame reaches every other member of that room
//! -- and nobody in any other room.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_crypto::sframe::SframeContext;
use antiphon_net::media_seal::{MediaSealer, SframeSealer};
use antiphon_net::sfu::{self, MemberTag, Refusal, RoomToken, SfuClient, SfuEvent};
use antiphon_net::NetError;
use antiphon_relay::sfu::{Sfu, SfuLimits};
use antiphon_relay::{run_node, run_node_with_sfu, Limits, Relay};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

const WAIT: Duration = Duration::from_secs(5);

async fn start_sfu(limits: SfuLimits) -> (SocketAddr, Arc<Mutex<Sfu>>) {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    let sfu = Arc::new(Mutex::new(Sfu::new(limits)));
    tokio::spawn(run_node_with_sfu(
        socket,
        Arc::new(Mutex::new(Relay::new(Limits::default()))),
        None,
        None,
        Some(sfu.clone()),
    ));
    (addr, sfu)
}

async fn join(sfu: SocketAddr, room: RoomToken) -> Result<(SfuClient, mpsc::Receiver<SfuEvent>), NetError> {
    let socket = Arc::new(antiphon_net::AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
    SfuClient::join(socket, sfu, room, sfu::new_member_tag(), WAIT).await
}

struct Member {
    client: SfuClient,
    events: mpsc::Receiver<SfuEvent>,
    sealer: SframeSealer,
}

/// Joins `n` members to `room`, keyed into one SFrame epoch as an MLS
/// group would key them, member `i` speaking as leaf `i`.
async fn members(sfu: SocketAddr, room: RoomToken, n: usize) -> Vec<Member> {
    let base_key = [42u8; 16];
    let mut joined = Vec::new();
    for _ in 0..n {
        joined.push(join(sfu, room).await.unwrap());
    }
    let tags: Vec<MemberTag> = joined.iter().map(|(c, _)| c.member()).collect();
    joined
        .into_iter()
        .enumerate()
        .map(|(i, (client, events))| {
            let mut ctx = SframeContext::new(i as u32);
            ctx.install_epoch(1, i as u32, &base_key);
            let mut sealer = SframeSealer::new(ctx);
            for (j, tag) in tags.iter().enumerate() {
                if j != i {
                    sealer.set_sender(*tag, j as u32);
                }
            }
            Member { client, events, sealer }
        })
        .collect()
}

async fn wait_for_roster(m: &SfuClient, others: usize) {
    let mut watch = m.roster_watch();
    tokio::time::timeout(WAIT, watch.wait_for(|r| r.len() == others)).await.expect("roster never filled").unwrap();
}

/// Drains media events for up to `window` or until `enough` frames have
/// arrived, opening each frame.
async fn collect(m: &mut Member, window: Duration, enough: usize) -> Vec<(MemberTag, Vec<u8>)> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while got.len() < enough || enough == 0 {
        let Ok(Some(event)) = tokio::time::timeout_at(deadline, m.events.recv()).await else { break };
        if let SfuEvent::Media { from, payload } = event {
            got.push((from, m.sealer.open(from, &payload).expect("a frame that does not open")));
        }
    }
    got
}

#[tokio::test]
async fn eight_members_hear_each_other_through_one_sfu() {
    let (addr, state) = start_sfu(SfuLimits::default()).await;
    let room = sfu::room_token(b"the group's shared secret", 20_000);
    let mut group = members(addr, room, 8).await;
    for m in &group {
        wait_for_roster(&m.client, 7).await;
    }

    const FRAMES: usize = 5;
    for (i, m) in group.iter_mut().enumerate() {
        for f in 0..FRAMES {
            let plain = format!("member {i} frame {f}");
            let sealed = m.sealer.seal(plain.as_bytes()).unwrap();
            // What the SFU carries is not the audio.
            assert!(!sealed.windows(plain.len()).any(|w| w == plain.as_bytes()));
            m.client.send_media(&sealed).await.unwrap();
        }
        // Loopback is fast, but a burst of 8 x 7 x 5 datagrams into small
        // receive buffers can still overrun them; pace the senders a little.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let tags: Vec<MemberTag> = group.iter().map(|m| m.client.member()).collect();
    for (i, m) in group.iter_mut().enumerate() {
        let got = collect(m, WAIT, 7 * FRAMES).await;
        let mut by_sender: HashMap<MemberTag, Vec<String>> = HashMap::new();
        for (from, plain) in got {
            by_sender.entry(from).or_default().push(String::from_utf8(plain).unwrap());
        }
        assert!(!by_sender.contains_key(&tags[i]), "a member heard itself");
        assert_eq!(by_sender.len(), 7, "member {i} heard {} of 7", by_sender.len());
        for (j, tag) in tags.iter().enumerate().filter(|(j, _)| *j != i) {
            let expected: Vec<String> = (0..FRAMES).map(|f| format!("member {j} frame {f}")).collect();
            assert_eq!(by_sender[tag], expected, "member {i} from member {j}");
        }
    }
    let stats = state.lock().unwrap().stats();
    assert_eq!((stats.rooms, stats.members), (1, 8));
    assert_eq!(stats.forwarded, (8 * 7 * FRAMES) as u64);
}

#[tokio::test]
async fn rooms_are_isolated_from_each_other() {
    let (addr, state) = start_sfu(SfuLimits::default()).await;
    let mut a = members(addr, sfu::room_token(b"group a", 20_000), 3).await;
    let mut b = members(addr, sfu::room_token(b"group b", 20_000), 3).await;
    for m in a.iter().chain(&b) {
        wait_for_roster(&m.client, 2).await;
    }
    assert_eq!(state.lock().unwrap().stats().rooms, 2);

    let sealed = a[0].sealer.seal(b"for room a only").unwrap();
    a[0].client.send_media(&sealed).await.unwrap();

    for m in a.iter_mut().skip(1) {
        let got = collect(m, WAIT, 1).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, b"for room a only");
    }
    for m in b.iter_mut() {
        assert!(collect(m, Duration::from_millis(300), 0).await.is_empty(), "room b heard room a");
        assert!(m.client.roster().iter().all(|t| !a.iter().any(|x| x.client.member() == *t)));
    }
}

#[tokio::test]
async fn a_full_room_refuses_and_a_leave_frees_the_seat() {
    let (addr, _) = start_sfu(SfuLimits { max_members_per_room: 3, ..SfuLimits::default() }).await;
    let room = sfu::room_token(b"small group", 20_000);
    let mut group = members(addr, room, 3).await;

    let err = join(addr, room).await.err().expect("a fourth member got in");
    assert!(matches!(err, NetError::SfuRefused(Refusal::RoomFull)), "{err}");

    // Someone leaves: the others see the roster shrink, and there is room.
    let leaver = group.pop().unwrap();
    let gone = leaver.client.member();
    leaver.client.leave().await;
    for m in &group {
        let mut watch = m.client.roster_watch();
        tokio::time::timeout(WAIT, watch.wait_for(|r| r.len() == 1 && !r.contains(&gone))).await.unwrap().unwrap();
    }
    let event = tokio::time::timeout(WAIT, async {
        loop {
            if let Some(SfuEvent::Roster(r)) = group[0].events.recv().await {
                if r.len() == 1 {
                    return r;
                }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(event, vec![group[1].client.member()]);
    assert!(join(addr, room).await.is_ok());
}

#[tokio::test]
async fn one_address_cannot_hold_too_many_rooms() {
    let (addr, _) = start_sfu(SfuLimits { max_rooms_per_addr: 2, ..SfuLimits::default() }).await;
    let socket = Arc::new(antiphon_net::AppSocket::from(UdpSocket::bind("127.0.0.1:0").await.unwrap()));
    // A client owns its socket's receive side, so drop each one once it
    // has joined; the SFU still counts the membership until it idles out.
    for g in 0..2u8 {
        let (client, _) =
            SfuClient::join(socket.clone(), addr, sfu::room_token(&[g], 1), sfu::new_member_tag(), WAIT).await.unwrap();
        drop(client);
    }
    let err = SfuClient::join(socket, addr, sfu::room_token(&[9], 1), sfu::new_member_tag(), WAIT).await.err().unwrap();
    assert!(matches!(err, NetError::SfuRefused(Refusal::TooManyRooms)), "{err}");
}

#[tokio::test]
async fn a_node_without_the_sfu_says_so() {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = socket.local_addr().unwrap();
    tokio::spawn(run_node(socket, Arc::new(Mutex::new(Relay::new(Limits::default()))), None, None));
    let err = join(addr, [1; 32]).await.err().unwrap();
    assert!(matches!(err, NetError::SfuRefused(Refusal::Disabled)), "{err}");
}
