//! Anonymity mode (spec §3): "Routes your connection through several
//! relays, each one only knowing the step before and after it. The
//! strongest privacy available, at the cost of noticeable delay."
//!
//! With it on, every relayed call and every relay registration goes
//! through a circuit (`antiphon_net::onion`) instead of straight to the
//! relay. The first hop sees your address but not where you are going;
//! the relay you reach sees the last hop, never you. The call code itself
//! is unchanged: a circuit hands back a local socket and an address that
//! stand in for the relay's.
//!
//! Direct calls are never routed this way -- a direct call is, by
//! definition, one where you can each see the other's address -- so with
//! anonymity mode on, a call that would have gone direct goes through
//! your relay instead (see `connection_mode`).

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use antiphon_net::onion::{fetch_hop_key, Circuit, OnionShim};
use antiphon_net::AppSocket;

use antiphon_net::vpn::TrafficKind;

const HOP_KEY_TIMEOUT: Duration = Duration::from_secs(3);
const BUILD_TIMEOUT: Duration = Duration::from_secs(5);

/// The hop nodes to use, if anonymity mode is on and has enough of them.
pub fn configured_hops() -> Option<Vec<String>> {
    let config = crate::config::load();
    (config.anonymity_mode && config.anonymity_hops.len() >= 2).then(|| {
        config.anonymity_hops.iter().take(3).cloned().collect()
    })
}

/// Whether anonymity mode is switched on, with or without enough hops.
pub fn requested() -> bool {
    crate::config::load().anonymity_mode
}

/// Local stand-in addresses of live circuits, so a connection arriving
/// from one can be shown as anonymous rather than merely relayed.
fn live() -> &'static Mutex<HashSet<SocketAddr>> {
    static LIVE: OnceLock<Mutex<HashSet<SocketAddr>>> = OnceLock::new();
    LIVE.get_or_init(Default::default)
}

/// Whether `addr` is the stand-in address of a live circuit.
pub fn is_circuit(addr: SocketAddr) -> bool {
    live().lock().unwrap().contains(&addr)
}

/// Builds a circuit through `hops` to `relay` and returns the socket and
/// address to use in place of the relay's. The circuit lives as long as
/// the returned socket does.
///
/// The circuit's own socket goes through the VPN when voice does, so the
/// kill switch holds for anonymous calls too.
pub async fn route_to(hops: &[String], relay: SocketAddr) -> Result<(Arc<AppSocket>, SocketAddr)> {
    let probe = crate::vpn::bind(TrafficKind::Voice, 0).await?;
    let mut path = Vec::with_capacity(hops.len());
    for hop in hops {
        let addr = crate::state::resolve(hop).await?;
        let key = fetch_hop_key(&probe, addr, HOP_KEY_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("hop {hop} didn't give its key: {e}"))?;
        path.push((addr, key));
    }
    let socket = Arc::new(crate::vpn::bind(TrafficKind::Voice, 0).await?);
    let shim = Circuit::build_on(socket, &path, relay, BUILD_TIMEOUT).await?.into_shim().await?;
    let socket = shim.socket();
    let stand_in = shim.relay_addr();
    keep_alive(shim, &socket);
    Ok((socket, stand_in))
}

/// Holds the circuit open until nothing else holds its socket, then
/// tears it down -- so it ends with the call or listen that used it.
fn keep_alive(shim: OnionShim, socket: &Arc<AppSocket>) {
    let stand_in = shim.relay_addr();
    live().lock().unwrap().insert(stand_in);
    let weak = Arc::downgrade(socket);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            // The shim holds one strong reference itself.
            if weak.strong_count() <= 1 {
                break;
            }
        }
        live().lock().unwrap().remove(&stand_in);
        drop(shim);
    });
}
