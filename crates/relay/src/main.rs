//! `antiphon-relay`: a self-hostable relay (spec §3). See the library docs
//! for exactly what it holds and for how long.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let limits = Limits {
        max_registrations: args.max_registrations,
        max_tokens_per_addr: args.max_tokens_per_addr,
        max_pairs: args.max_pairs,
    };
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
    println!("antiphon-relay listening on {}\n\n{COMMITMENT}", socket.local_addr()?);
    if let Some(m) = &mailbox {
        let open = if m.lock().unwrap().is_open() { "anyone" } else { "members with the code" };
        println!("\nMailbox on: holding sealed mail for {open}.");
    }

    if let Some(secs) = args.stats_every {
        let relay = relay.clone();
        let mailbox = mailbox.clone();
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
            }
        });
    }

    tokio::select! {
        result = run_node(socket, relay, mailbox) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
