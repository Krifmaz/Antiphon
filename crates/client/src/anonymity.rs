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

/// How long to wait for each hop node to hand out its hop key.
const HOP_KEY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long building the circuit through all the hops may take.
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
///
/// Each hop's key is pinned: written into the hop list as `host:port#key`,
/// or remembered from the first time the hop gave one. A hop that answers
/// with a different key is refused, since that is what someone standing
/// between you and the hop would have to do to read the routing.
pub async fn route_to(hops: &[String], relay: SocketAddr) -> Result<(Arc<AppSocket>, SocketAddr)> {
    let probe = crate::vpn::bind(TrafficKind::Voice, 0).await?;
    let mut path = Vec::with_capacity(hops.len());
    for hop in hops {
        let (host, written) = split_pin(hop);
        let addr = crate::state::resolve(host).await?;
        let key = match written {
            Some(key) => key,
            None => {
                let fetched = fetch_hop_key(&probe, addr, HOP_KEY_TIMEOUT)
                    .await
                    .map_err(|e| anyhow::anyhow!("hop {host} didn't give its key: {e}"))?;
                pin(host, fetched)?
            }
        };
        path.push((addr, key));
    }
    let socket = Arc::new(crate::vpn::bind(TrafficKind::Voice, 0).await?);
    let shim = Circuit::build_on(socket, &path, relay, BUILD_TIMEOUT).await?.into_shim().await?;
    let socket = shim.socket();
    let stand_in = shim.relay_addr();
    keep_alive(shim, &socket);
    Ok((socket, stand_in))
}

/// Splits a hop entry into its address and the key written after `#`, if
/// any. A malformed key counts as none written.
pub fn split_pin(entry: &str) -> (&str, Option<[u8; 32]>) {
    match entry.trim().split_once('#') {
        Some((host, key)) => (host.trim(), crate::connection_mode::decode_hex32(key)),
        None => (entry.trim(), None),
    }
}

/// Where remembered hop keys live: a file of their own beside the config,
/// so a settings save from elsewhere in the app can never drop one.
fn pins_path() -> Option<std::path::PathBuf> {
    // Tests build circuits through throwaway hops on reused loopback ports;
    // remembering those keys would only make later tests refuse them.
    if cfg!(test) {
        return None;
    }
    crate::config::config_dir().map(|d| d.join("hop_keys.json"))
}

/// The remembered hop keys, as `(host:port, hex key)`.
fn load_pins() -> Vec<(String, String)> {
    pins_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Checks `fetched` against the key remembered for `host`, remembering it
/// if this is the first time. Fails if the hop's key has changed.
fn pin(host: &str, fetched: [u8; 32]) -> Result<[u8; 32]> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    let mut pins = load_pins();
    let hex = crate::connection_mode::encode_hex32(&fetched);
    match check_pin(&pins, host, &hex) {
        Pin::Matches => Ok(fetched),
        Pin::New => {
            if let Some(path) = pins_path() {
                pins.push((host.to_string(), hex));
                std::fs::write(path, serde_json::to_vec(&pins)?)?;
            }
            Ok(fetched)
        }
        Pin::Changed => anyhow::bail!(
            "hop {host} answered with a different key than before -- someone may be in the way, or its \
             operator replaced the key. If you trust the new one, write it into the hop list as {host}#<key>."
        ),
    }
}

/// What a fetched key is, against the remembered ones.
#[derive(Debug, PartialEq, Eq)]
enum Pin {
    /// The same key as remembered.
    Matches,
    /// Nothing remembered for this hop yet.
    New,
    /// A different key than remembered.
    Changed,
}

/// Compares `hex` with the key remembered for `host`.
fn check_pin(known: &[(String, String)], host: &str, hex: &str) -> Pin {
    match known.iter().find(|(h, _)| h == host) {
        None => Pin::New,
        Some((_, k)) if k == hex => Pin::Matches,
        Some(_) => Pin::Changed,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hop_entry_can_carry_its_key() {
        let hex = crate::connection_mode::encode_hex32(&[0xab; 32]);
        assert_eq!(split_pin(&format!(" h.example:7100#{hex} ")), ("h.example:7100", Some([0xab; 32])));
        assert_eq!(split_pin("h.example:7100"), ("h.example:7100", None));
        assert_eq!(split_pin("h.example:7100#junk"), ("h.example:7100", None));
    }

    #[test]
    fn a_changed_hop_key_is_caught() {
        let known = vec![("h:1".to_string(), "aa".to_string())];
        assert_eq!(check_pin(&known, "h:1", "aa"), Pin::Matches);
        assert_eq!(check_pin(&known, "h:1", "bb"), Pin::Changed);
        assert_eq!(check_pin(&known, "h:2", "bb"), Pin::New);
    }
}
