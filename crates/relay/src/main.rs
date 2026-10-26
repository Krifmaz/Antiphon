//! `antiphon-relay`: a self-hostable relay (spec §3). See the library docs
//! for exactly what it holds and for how long.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_relay::files::{FileHost, FileStore, HostPolicy, Retention};
use antiphon_relay::mailbox::{Mailbox, MailboxLimits};
use antiphon_relay::{run_node, Limits, Relay, COMMITMENT};
use clap::Parser;
use tokio::net::UdpSocket;

#[derive(Parser)]
#[command(version, about = "Antiphon relay: forwards encrypted calls between people who can't reach each other directly.", after_help = COMMITMENT)]
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
            member_code: args.mailbox_code.map(String::into_bytes),
            ..MailboxLimits::default()
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
    println!("antiphon-relay listening on {}\n\n{COMMITMENT}", socket.local_addr()?);
    if args.files {
        println!("\nHosting files in {}: sealed copies this node cannot read.", args.files_dir.display());
    }
    if let Some(m) = &mailbox {
        let open = if m.lock().unwrap().is_open() { "anyone" } else { "members with the code" };
        println!("\nMailbox on: holding sealed mail for {open}.");
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
                if let Some(f) = &files {
                    let u = f.lock().unwrap().usage();
                    println!(
                        "files held={} stored_bytes={} chunks_served={} bytes_served={} refused={}",
                        u.files, u.stored_bytes, u.chunks_served, u.bytes_served, u.refused
                    );
                }
            }
        });
    }

    tokio::select! {
        result = run_node(socket, relay, mailbox, files.clone()) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
