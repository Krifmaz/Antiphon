//! `antiphon-relay`: a self-hostable relay (spec §3). See the library docs
//! for exactly what it holds and for how long.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use antiphon_relay::{run, Limits, Relay, COMMITMENT};
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
    println!("antiphon-relay listening on {}\n\n{COMMITMENT}", socket.local_addr()?);

    if let Some(secs) = args.stats_every {
        let relay = relay.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(secs.max(1)));
            loop {
                tick.tick().await;
                let s = relay.lock().unwrap().stats();
                println!("registrations={} calls={} packets_forwarded={}", s.registrations, s.pairs, s.forwarded);
            }
        });
    }

    tokio::select! {
        result = run(socket, relay) => result?,
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}
