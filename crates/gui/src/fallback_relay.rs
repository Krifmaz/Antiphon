//! Fallback relays (spec §3 "Project-hosted relays exist as fallback", §4
//! "Infrastructure position": "fallback, not foundation").
//!
//! Your own relay -- self-hosted or a community's -- always comes first.
//! Only when none is set does the app look further: the relays you listed
//! as fallbacks, then the project's own. Each is probed before it is used,
//! so a dead one is skipped rather than discovered when a call fails to
//! connect.
//!
//! A fallback relay is just a relay: it sees exactly what any other relay
//! sees (blinded daily tokens and the addresses of a live call) and holds
//! no key. Using one is a choice about whose bandwidth carries the
//! ciphertext, never about who can read it.
//!
//! The probe dials a random token. No one can have registered it, so a
//! live relay answers "not found" and a dead one answers nothing -- which
//! is the whole question, asked without any new message type and without
//! telling the relay anything about you.

use std::net::SocketAddr;
use std::time::Duration;

use antiphon_net::relay::{dial, RelayToken};
use antiphon_net::NetError;

/// Relays run by the project, tried last. Empty until the project runs
/// nodes of its own: an address here is a promise to keep a machine up,
/// so none is listed that nobody is running.
pub const PROJECT_RELAYS: &[&str] = &[];

const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// The order relays are tried in: yours, then your fallbacks, then the
/// project's if you allow them. Blank entries and repeats are dropped.
pub fn candidates(own: &str, fallbacks: &[String], use_project: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let project = PROJECT_RELAYS.iter().map(|s| s.to_string());
    let listed = std::iter::once(own.to_string())
        .chain(fallbacks.iter().cloned())
        .chain(project.filter(|_| use_project));
    for candidate in listed {
        let candidate = candidate.trim().to_string();
        if !candidate.is_empty() && !out.contains(&candidate) {
            out.push(candidate);
        }
    }
    out
}

/// Parses the Settings text box: one relay per line, or comma-separated.
pub fn parse_list(text: &str) -> Vec<String> {
    text.split(['\n', ','])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether a relay at `addr` is up.
pub async fn probe(addr: SocketAddr) -> bool {
    // Through the VPN when voice goes that way: a probe that slips past
    // the kill switch would be the leak the switch exists to stop.
    let Ok(socket) = crate::vpn::bind(antiphon_net::vpn::TrafficKind::Voice, 0).await else { return false };
    let mut token: RelayToken = [0; 32];
    if getrandom::fill(&mut token).is_err() {
        return false;
    }
    matches!(dial(&socket, addr, token, PROBE_TIMEOUT).await, Err(NetError::RelayNotFound) | Ok(()))
}

/// The first candidate that resolves and answers, if any.
pub async fn first_live(candidates: &[String]) -> Option<String> {
    for candidate in candidates {
        let Ok(addr) = crate::state::resolve(candidate).await else { continue };
        if probe(addr).await {
            return Some(candidate.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn your_own_relay_comes_first_and_repeats_are_dropped() {
        let list = candidates(" r1:9000 ", &["r2:9000".into(), "r1:9000".into(), " ".into()], true);
        assert_eq!(list[..2], ["r1:9000".to_string(), "r2:9000".to_string()]);
        assert_eq!(list.len(), 2 + PROJECT_RELAYS.len());
    }

    #[test]
    fn the_settings_box_takes_lines_or_commas() {
        assert_eq!(parse_list("a:1\n b:2 ,c:3\n\n"), vec!["a:1", "b:2", "c:3"]);
    }

    #[tokio::test]
    async fn a_live_relay_answers_the_probe_and_a_dead_one_does_not() {
        let socket = std::sync::Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let relay = std::sync::Arc::new(std::sync::Mutex::new(antiphon_relay::Relay::default()));
        let server = tokio::spawn(antiphon_relay::run(socket, relay));
        assert!(probe(addr).await);
        let found = first_live(&["127.0.0.1:1".to_string(), addr.to_string()]).await;
        assert_eq!(found, Some(addr.to_string()));
        server.abort();
    }
}
