//! `antiphon-relay`: a self-hostable relay, and optionally a mailbox, a file
//! host and a group-call SFU (spec §3). See the library docs
//! for exactly what it holds and for how long.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_relay::files::{FileHost, FileStore, HostPolicy, Retention};
use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
use antiphon_net::dht::{self, DhtNode, ListingEntry, NodeKey};
use antiphon_relay::onion::{load_or_create_hop_key, HopLimits, HopNode};
use antiphon_relay::sfu::{Sfu, SfuLimits};
use antiphon_relay::{run_node_full, Limits, Relay, COMMITMENT};
use clap::Parser;
use tokio::net::UdpSocket;

#[derive(Parser)]
#[command(version, about = "Stoop relay: forwards encrypted calls between people who can't reach each other directly.", after_help = COMMITMENT)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:3479")]
    listen: SocketAddr,
    /// Most tokens held at once, across everyone.
    #[arg(long, default_value_t = Limits::default().max_registrations)]
    max_registrations: usize,
    /// Most tokens any one address may register.
    #[arg(long, default_value_t = Limits::default().max_tokens_per_addr)]
    max_tokens_per_addr: usize,
    /// Most calls relayed at once.
    #[arg(long, default_value_t = Limits::default().max_pairs)]
    max_pairs: usize,
    /// Print aggregate counters (registrations, live calls, packets) every
    /// N seconds. Off by default; never prints addresses or tokens.
    #[arg(long)]
    stats_every: Option<u64>,
    /// Also hold sealed mail for people who are offline. Off by default.
    #[arg(long)]
    mailbox: bool,
    /// Total mail held, in megabytes.
    #[arg(long, default_value_t = 256)]
    mailbox_storage_mb: u64,
    /// Mail any one inbox may hold, in kilobytes.
    #[arg(long, default_value_t = 2048)]
    mailbox_inbox_kb: u64,
    /// Deposits per inbox per minute.
    #[arg(long, default_value_t = 30)]
    mailbox_rate: usize,
    /// Days mail is kept if nobody collects it.
    #[arg(long, default_value_t = 14)]
    mailbox_expiry_days: u64,
    /// Serve only your own community: registering an inbox needs this
    /// code. Without it, anyone may register.
    #[arg(long)]
    mailbox_code: Option<String>,
    /// Wake phones by push: post a wake-up when mail arrives for an inbox
    /// whose owner registered a push endpoint, and pass on senders' own
    /// sealed wake-ups. Makes outgoing HTTPS requests to the endpoints
    /// people register (public addresses only). Off by default.
    #[arg(long, requires = "mailbox")]
    mailbox_push: bool,
    /// Where the key this node signs its own wake-ups with is kept, so
    /// phones that registered keep recognising it after a restart.
    #[arg(long, default_value = "antiphon-push.key")]
    mailbox_push_key: std::path::PathBuf,
    /// APNs: the `.p8` key file from an Apple developer account. With the
    /// other three `--apns-*` options, lets this node wake iOS phones. Only
    /// someone with the app's Apple account can have one; see
    /// docs/mobile-push.md. Also readable from `ANTIPHON_APNS_KEY`.
    #[cfg(feature = "apns")]
    #[arg(long, env = "ANTIPHON_APNS_KEY", requires = "mailbox_push", requires = "apns_key_id", requires = "apns_team_id", requires = "apns_topic")]
    apns_key: Option<std::path::PathBuf>,
    /// APNs: the key's 10-character id.
    #[cfg(feature = "apns")]
    #[arg(long, env = "ANTIPHON_APNS_KEY_ID")]
    apns_key_id: Option<String>,
    /// APNs: the developer account's team id.
    #[cfg(feature = "apns")]
    #[arg(long, env = "ANTIPHON_APNS_TEAM_ID")]
    apns_team_id: Option<String>,
    /// APNs: the app's bundle id (VoIP pushes go to `<topic>.voip`).
    #[cfg(feature = "apns")]
    #[arg(long, env = "ANTIPHON_APNS_TOPIC")]
    apns_topic: Option<String>,
    /// APNs: use Apple's sandbox, for development builds of the app.
    #[cfg(feature = "apns")]
    #[arg(long, env = "ANTIPHON_APNS_SANDBOX")]
    apns_sandbox: bool,
    /// Host sealed files: serve what is pinned here, and accept uploads
    /// (spec §5 "Server-hosted mode"). Off by default.
    #[arg(long)]
    files: bool,
    /// Where hosted files are kept.
    #[arg(long, default_value = "antiphon-files")]
    files_dir: std::path::PathBuf,
    /// Only people with this code may upload (a community-only node).
    #[arg(long)]
    files_code: Option<String>,
    /// Largest file accepted, in megabytes.
    #[arg(long, default_value_t = 2048)]
    files_max_mb: u64,
    /// Total storage for hosted files, in gigabytes.
    #[arg(long, default_value_t = 20)]
    files_quota_gb: u64,
    /// Storage any one uploader may use, in megabytes.
    #[arg(long, default_value_t = 2048)]
    files_per_user_mb: u64,
    /// Remove files this many days after they arrive.
    #[arg(long, conflicts_with = "files_expire_unopened_days")]
    files_expire_days: Option<u32>,
    /// Remove files nobody has opened this many days after they arrive.
    #[arg(long)]
    files_expire_unopened_days: Option<u32>,
    /// Print what is hosted (ids, sizes, ages, reads; never names or
    /// contents) and exit.
    #[arg(long)]
    files_list: bool,
    /// Remove a hosted file by id, or "all", and exit.
    #[arg(long)]
    files_purge: Option<String>,
    /// Also forward group calls (an SFU): each member sends its sealed
    /// audio here once and it goes out to the rest of the call. Off by
    /// default.
    #[arg(long)]
    sfu: bool,
    /// Most people in one group call.
    #[arg(long, default_value_t = SfuLimits::default().max_members_per_room)]
    sfu_max_members: usize,
    /// Most group calls forwarded at once.
    #[arg(long, default_value_t = SfuLimits::default().max_rooms)]
    sfu_max_rooms: usize,
    /// Most group calls any one address may be in at once.
    #[arg(long, default_value_t = SfuLimits::default().max_rooms_per_addr)]
    sfu_max_rooms_per_addr: usize,
    /// Be a hop on anonymity-mode circuits: peel one routing layer and pass
    /// the rest on, or deliver it to the relay it names. Off by default.
    #[arg(long)]
    hops: bool,
    /// Keep the hop key in this file (created if missing) so it survives
    /// restarts. Without it, a new key is made at every start.
    #[arg(long)]
    hop_key: Option<std::path::PathBuf>,
    /// Most circuits held at once.
    #[arg(long, default_value_t = HopLimits::default().max_circuits)]
    hops_max_circuits: usize,
    /// Most circuits any one neighbouring address may hold.
    #[arg(long, default_value_t = HopLimits::default().max_circuits_per_addr)]
    hops_max_per_addr: usize,
    /// Let circuits reach private and loopback addresses (a LAN-only node).
    #[arg(long)]
    hops_allow_private: bool,
    /// Also be a DHT node, so contacts can find each other again after
    /// an address changes (spec §4). Off by default. Stores only sealed
    /// records under keys that change daily.
    #[arg(long)]
    dht: bool,
    /// Another DHT node to join through. Repeat for several. A node with
    /// none waits to be joined.
    #[arg(long = "dht-bootstrap", requires = "dht")]
    dht_bootstrap: Vec<String>,
    /// Where this node's DHT key is kept, so its id (and the work behind
    /// it) survives a restart.
    #[arg(long, default_value = "antiphon-dht.key")]
    dht_key: std::path::PathBuf,
    /// Serve a directory of public communities from this file: one per
    /// line, `name | description | invite link`. Opt-in for each
    /// community; never lists people.
    #[arg(long, requires = "dht")]
    directory: Option<std::path::PathBuf>,
}

/// Loads the key the node signs its wake-ups with, or makes and saves one.
/// A plain 32-byte seed, like the DHT key: it names the node to the phones
/// that registered with it and nothing else, so it is not worth a keystore.
fn load_push_identity(path: &std::path::Path) -> anyhow::Result<antiphon_crypto::IdentityKeyPair> {
    if let Ok(bytes) = std::fs::read(path) {
        let seed: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is not a push key; move it aside to make a new one", path.display()))?;
        return Ok(antiphon_crypto::IdentityKeyPair::from_master_seed(seed));
    }
    let identity = antiphon_crypto::IdentityKeyPair::generate();
    std::fs::write(path, identity.master_seed())?;
    Ok(identity)
}

/// The push sender the options ask for: UnifiedPush always, APNs when a
/// key was given (and this build has the `apns` feature).
fn push_sender(args: &Args) -> anyhow::Result<antiphon_push::send::Sender> {
    let sender = antiphon_push::send::Sender::new();
    #[cfg(feature = "apns")]
    if let Some(path) = &args.apns_key {
        let config = antiphon_push::send::apns::ApnsConfig {
            key_p8: std::fs::read_to_string(path)?,
            key_id: args.apns_key_id.clone().unwrap_or_default(),
            team_id: args.apns_team_id.clone().unwrap_or_default(),
            topic: args.apns_topic.clone().unwrap_or_default(),
            sandbox: args.apns_sandbox,
        };
        return sender.with_apns(config).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()));
    }
    let _ = args;
    Ok(sender)
}

/// Drains the mailbox's owed wake-ups a few times a second and posts each
/// one on its own task, so a slow push service never holds up the packet
/// loop or the next wake-up. Failures are counted, never logged with the
/// endpoint: the operator's view stays aggregate.
fn spawn_push_loop(mailbox: Arc<Mutex<Mailbox>>, identity: antiphon_crypto::IdentityKeyPair, sender: antiphon_push::send::Sender) {
    use antiphon_crypto::push::WakeKind;
    use antiphon_relay::mailbox::WakeJob;
    let identity = Arc::new(identity);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            let jobs = mailbox.lock().unwrap().take_wakes();
            for job in jobs {
                let (sender, identity) = (sender.clone(), identity.clone());
                tokio::spawn(async move {
                    let (transport, sealed, kind) = match job {
                        WakeJob::Seal(target) => {
                            // The node's own wake-ups are only ever "mail is
                            // waiting"; it never rings anyone.
                            let Ok(sealed) = antiphon_push::seal_wake(&identity, &target, WakeKind::Message, &[], antiphon_push::now_secs()) else { return };
                            (target.transport, sealed, WakeKind::Message)
                        }
                        WakeJob::Forward { transport, sealed } => {
                            let Ok(sealed) = <[u8; antiphon_crypto::push::SEALED_LEN]>::try_from(sealed.as_slice()) else { return };
                            // The node can't see inside, so it goes as the
                            // endpoint says: a VoIP token only ever carries
                            // calls, anything else carries anything.
                            let kind = if matches!(transport, antiphon_push::Transport::ApnsVoip(_)) { WakeKind::Call } else { WakeKind::Message };
                            (transport, sealed, kind)
                        }
                    };
                    if let Err(e) = sender.send(kind, &transport, &sealed).await {
                        eprintln!("push wake-up not delivered: {}", short_reason(&e));
                    }
                });
            }
        }
    });
}

/// A push failure without the endpoint in it, for the operator's log.
fn short_reason(e: &antiphon_push::send::SendError) -> &'static str {
    use antiphon_push::send::SendError;
    match e {
        SendError::Refused(_) => "endpoint refused (not public HTTPS)",
        SendError::WrongTransport => "wrong transport for the kind",
        SendError::NoApns => "no APNs key",
        SendError::Gone => "endpoint gone",
        SendError::Status(..) => "push service error",
        SendError::Network(_) => "push service unreachable",
    }
}

/// Loads the node's DHT key, or makes and saves one.
fn load_dht_key(path: &std::path::Path) -> anyhow::Result<NodeKey> {
    if let Ok(bytes) = std::fs::read(path) {
        return NodeKey::from_bytes(&bytes, dht::POW_BITS)
            .ok_or_else(|| anyhow::anyhow!("{} is not a valid DHT key; move it aside to make a new one", path.display()));
    }
    let key = NodeKey::generate(dht::POW_BITS);
    std::fs::write(path, key.to_bytes())?;
    Ok(key)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let limits = Limits {
        max_registrations: args.max_registrations,
        max_tokens_per_addr: args.max_tokens_per_addr,
        max_pairs: args.max_pairs,
    };
    // The operator's view and manual purge (spec §5 "Admin view of usage,
    // manual purge"), straight against the store.
    if args.files_list || args.files_purge.is_some() {
        let mut store = FileStore::open(&args.files_dir)?;
        if let Some(target) = &args.files_purge {
            let ids: Vec<_> = store.list().into_iter().map(|(id, _)| id).collect();
            let mut removed = 0;
            for id in ids {
                let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                if target == "all" || *target == hex {
                    removed += store.purge(&id) as usize;
                }
            }
            println!("Removed {removed} file(s).");
        } else {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
            for (id, m) in store.list() {
                let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
                println!(
                    "{hex}  {:>12} bytes  {:>4} days old  {:>6} reads{}",
                    m.stored_bytes(),
                    now.saturating_sub(m.created_secs) / 86_400,
                    m.reads,
                    if m.complete() { "" } else { "  (partial)" }
                );
            }
            println!("{} file(s), {} bytes.", store.list().len(), store.used_bytes());
        }
        return Ok(());
    }
    let socket = Arc::new(UdpSocket::bind(args.listen).await?);
    let relay = Arc::new(Mutex::new(Relay::new(limits)));
    let mailbox = args.mailbox.then(|| {
        Arc::new(Mutex::new(Mailbox::new(MailboxLimits {
            enabled: true,
            storage_cap: args.mailbox_storage_mb * 1024 * 1024,
            inbox_cap: args.mailbox_inbox_kb * 1024,
            deposits_per_minute: args.mailbox_rate,
            mail_expiry: Duration::from_secs(args.mailbox_expiry_days * 86_400),
            member_code: args.mailbox_code.clone().map(String::into_bytes),
            ..MailboxLimits::default()
        })))
    });
    if let (Some(m), true) = (&mailbox, args.mailbox_push) {
        let identity = load_push_identity(&args.mailbox_push_key)?;
        let sender = push_sender(&args)?;
        m.lock().unwrap().enable_push(identity.public().identity_key, sender.has_apns());
        let key: String = identity.public().identity_key.iter().map(|b| format!("{b:02x}")).collect();
        let apple = if sender.has_apns() { "UnifiedPush and APNs" } else { "UnifiedPush" };
        println!("\nPush wake-ups on ({apple}). Node push key {key}");
        spawn_push_loop(m.clone(), identity, sender);
    }
    let sfu = args.sfu.then(|| {
        Arc::new(Mutex::new(Sfu::new(SfuLimits {
            max_rooms: args.sfu_max_rooms,
            max_members_per_room: args.sfu_max_members,
            max_rooms_per_addr: args.sfu_max_rooms_per_addr,
        })))
    });
    let files = if args.files {
        let retention = match (args.files_expire_days, args.files_expire_unopened_days) {
            (Some(d), _) => Retention::AfterDays(d),
            (_, Some(d)) => Retention::UnopenedDays(d),
            _ => Retention::Forever,
        };
        let policy = HostPolicy {
            serve: true,
            accept_uploads: true,
            upload_code: args.files_code.clone().map(String::into_bytes),
            max_file_bytes: args.files_max_mb * 1024 * 1024,
            total_quota_bytes: args.files_quota_gb * 1024 * 1024 * 1024,
            per_uploader_quota_bytes: args.files_per_user_mb * 1024 * 1024,
            retention,
        };
        let store = FileStore::open(&args.files_dir)?;
        // Anyone the uploader shared the file with may download it; a
        // node has no contact list, so "contacts only" isn't offered here.
        Some(Arc::new(Mutex::new(FileHost::new(store, policy, Arc::new(|_| false)))))
    } else {
        None
    };
    let hops = if args.hops {
        let secret = match &args.hop_key {
            Some(path) => load_or_create_hop_key(path)?,
            None => antiphon_crypto::onion::HopSecret::generate(),
        };
        let limits = HopLimits {
            max_circuits: args.hops_max_circuits,
            max_circuits_per_addr: args.hops_max_per_addr,
            allow_private: args.hops_allow_private,
            ..HopLimits::default()
        };
        Some(Arc::new(Mutex::new(HopNode::new(secret, limits))))
    } else {
        None
    };
    let dht_node = if args.dht {
        let mut node = DhtNode::new(load_dht_key(&args.dht_key)?, dht::Config::default());
        if let Some(path) = &args.directory {
            let listing = ListingEntry::parse_file(&std::fs::read_to_string(path)?)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            node.set_listing(listing);
        }
        let mut bootstrap = Vec::new();
        for b in &args.dht_bootstrap {
            match tokio::net::lookup_host(b.as_str()).await?.next() {
                Some(a) => bootstrap.push(a),
                None => anyhow::bail!("{b} did not resolve"),
            }
        }
        let out = node.bootstrap(&bootstrap, std::time::Instant::now());
        for (to, bytes) in out {
            let _ = socket.send_to(&bytes, to).await;
        }
        Some(Arc::new(Mutex::new(node)))
    } else {
        None
    };
    println!("antiphon-relay listening on {}\n\n{COMMITMENT}", socket.local_addr()?);
    if let Some(h) = &hops {
        let key: String = h.lock().unwrap().public_key().iter().map(|b| format!("{b:02x}")).collect();
        println!("\nHops on: carrying anonymity circuits. Hop key {key}");
    }
    if let Some(d) = &dht_node {
        let id: String = d.lock().unwrap().id().iter().take(8).map(|b| format!("{b:02x}")).collect();
        println!("\nDHT node on (id {id}...).");
    }
    if args.directory.is_some() {
        println!("Serving a directory of public communities.");
    }

    // The DHT's timers: verification timeouts, record expiry, refresh.
    if let Some(d) = dht_node.clone() {
        let socket = socket.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let out = d.lock().unwrap().tick(std::time::Instant::now());
                for (to, bytes) in out {
                    let _ = socket.send_to(&bytes, to).await;
                }
            }
        });
    }
    if args.files {
        println!("\nHosting files in {}: sealed copies this node cannot read.", args.files_dir.display());
    }
    if let Some(m) = &mailbox {
        let open = if m.lock().unwrap().is_open() { "anyone" } else { "members with the code" };
        println!("\nMailbox on: holding sealed mail for {open}.");
    }
    if let Some(s) = &sfu {
        let most = s.lock().unwrap().limits().max_members_per_room;
        println!("\nGroup calls on: forwarding sealed audio for calls of up to {most}.");
    }

    // Retention runs on a clock, not on traffic: a quiet node still lets
    // go of what it promised to.
    if let Some(f) = files.clone() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let removed = f.lock().unwrap().sweep(secs);
                if removed > 0 {
                    println!("files expired={removed}");
                }
            }
        });
    }

    if let Some(secs) = args.stats_every {
        let relay = relay.clone();
        let mailbox = mailbox.clone();
        let files = files.clone();
        let sfu = sfu.clone();
        let hops = hops.clone();
        let dht_node = dht_node.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(secs.max(1)));
            loop {
                tick.tick().await;
                let s = relay.lock().unwrap().stats();
                println!("registrations={} calls={} packets_forwarded={}", s.registrations, s.pairs, s.forwarded);
                // The operator dashboard: totals, never labels or contents.
                if let Some(m) = &mailbox {
                    let m = m.lock().unwrap().stats();
                    println!(
                        "mailbox inboxes={} held={} held_bytes={} bytes_in={} bytes_out={} refused={}",
                        m.inboxes, m.held, m.held_bytes, m.bytes_in, m.bytes_out, m.refused
                    );
                }
                if let Some(s) = &sfu {
                    let s = s.lock().unwrap().stats();
                    println!("sfu rooms={} members={} packets_forwarded={}", s.rooms, s.members, s.forwarded);
                }
                if let Some(d) = &dht_node {
                    let s = d.lock().unwrap().stats();
                    println!("dht nodes={} records={} stores_refused={}", s.nodes, s.records, s.stores_refused);
                }
                if let Some(f) = &files {
                    let u = f.lock().unwrap().usage();
                    println!(
                        "files held={} stored_bytes={} chunks_served={} bytes_served={} refused={}",
                        u.files, u.stored_bytes, u.chunks_served, u.bytes_served, u.refused
                    );
                }
                if let Some(h) = &hops {
                    let s = h.lock().unwrap().stats();
                    println!("hops circuits={} exits={} cells={} refused={}", s.circuits, s.exits, s.cells, s.refused);
                }
            }
        });
    }

    tokio::select! {
        result = run_node_full(socket, relay, mailbox, files.clone(), sfu, hops, dht_node) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
