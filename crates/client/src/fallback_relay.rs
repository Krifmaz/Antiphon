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
//!
//! Two more places to look, after those:
//!
//! - **The mesh.** A relay that federates says which relays it peers with
//!   (`antiphon_net::federation::fetch_peers`). Those are remembered
//!   (`Config::mesh_relays`) so that when your relays are down, another
//!   relay in the same mesh -- one that can still reach the people you
//!   know -- is there to use.
//! - **A contact's relay.** With no relay of your own and none of the
//!   above answering, you can listen at a relay a contact runs -- but only
//!   one whose owner said so (`lends_relay`, from their invite or their
//!   last call). Your contacts then ring you there, exactly as they would
//!   at a relay of yours. The owner lends bandwidth, never a key.

use std::net::SocketAddr;
use std::time::Duration;

use antiphon_net::relay::{dial, RelayToken};
use antiphon_net::NetError;

/// Relays run by the project, tried last. Empty until the project runs
/// nodes of its own: an address here is a promise to keep a machine up,
/// so none is listed that nobody is running.
pub const PROJECT_RELAYS: &[&str] = &[];

/// How long one relay gets to answer a probe before the next is tried.
const PROBE_TIMEOUT: Duration = Duration::from_millis(1_500);

/// The order relays are tried in: yours, then your fallbacks, then the
/// project's if you allow them. Blank entries and repeats are dropped.
pub fn candidates(own: &str, fallbacks: &[String], use_project: bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let project = PROJECT_RELAYS.iter().map(ToString::to_string);
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

/// Where the relay in use came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Your own relay.
    Own,
    /// One you listed, or the project's.
    Fallback,
    /// One a relay you used said it peers with.
    Mesh,
    /// A contact's, lent with their consent.
    Borrowed,
}

/// The relay to use, and why it is that one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// `host:port`.
    pub relay: String,
    /// Where it came from.
    pub source: Source,
    /// For [`Source::Borrowed`], whose it is (their saved name).
    pub lender: Option<String>,
}

/// Relays contacts lend: `(relay, their name)` for each contact who said
/// you may listen at theirs.
pub fn lent_by(contacts: &[crate::config::Contact]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for c in contacts {
        let relay = c.relay.trim();
        if c.lends_relay && !relay.is_empty() && !out.iter().any(|(r, _)| r == relay) {
            out.push((relay.to_string(), c.name.clone()));
        }
    }
    out
}

/// Picks the relay to listen at and advertise: yours, your fallbacks, the
/// project's, the mesh's, then a contact's -- the first that answers.
pub async fn choose(config: &crate::config::AppConfig) -> Option<Choice> {
    let own = config.relay_server.trim();
    let listed = candidates(own, &config.fallback_relays, config.use_project_relays);
    let mesh: Vec<String> = config.mesh_relays.iter().filter(|m| !listed.contains(m)).cloned().collect();
    for (i, candidate) in listed.iter().enumerate() {
        if is_live(candidate).await {
            let source = if i == 0 && candidate == own { Source::Own } else { Source::Fallback };
            return Some(Choice { relay: candidate.clone(), source, lender: None });
        }
    }
    if let Some(relay) = first_live(&mesh).await {
        return Some(Choice { relay, source: Source::Mesh, lender: None });
    }
    if own.is_empty() {
        for (relay, name) in lent_by(&config.contacts) {
            if is_live(&relay).await {
                return Some(Choice { relay, source: Source::Borrowed, lender: Some(name) });
            }
        }
    }
    None
}

/// Whether `candidate` resolves and answers a probe.
async fn is_live(candidate: &str) -> bool {
    match crate::state::resolve(candidate).await {
        Ok(addr) => probe(addr).await,
        Err(_) => false,
    }
}

/// The relays `relay` says it peers with, as `host:port`. Empty when it
/// does not federate or does not answer.
pub async fn learn_mesh(relay: &str) -> Vec<String> {
    let Ok(addr) = crate::state::resolve(relay).await else { return vec![] };
    let Ok(socket) = crate::vpn::bind(antiphon_net::vpn::TrafficKind::Voice, 0).await else { return vec![] };
    antiphon_net::federation::fetch_peers(&socket, addr, PROBE_TIMEOUT)
        .await
        .map(|peers| peers.into_iter().map(|a| a.to_string()).collect())
        .unwrap_or_default()
}

/// The most mesh relays remembered.
pub const MAX_MESH_RELAYS: usize = 32;

/// Merges newly learned mesh relays into the remembered ones, newest
/// first, without repeats, capped at [`MAX_MESH_RELAYS`].
pub fn merge_mesh(known: &[String], learned: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in learned.iter().chain(known) {
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    out.truncate(MAX_MESH_RELAYS);
    out
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
    fn only_contacts_who_lend_their_relay_are_offered() {
        use crate::config::Contact;
        let contacts = vec![
            Contact { name: "Sam".into(), relay: "r1:1".into(), lends_relay: true, ..Default::default() },
            Contact { name: "Kai".into(), relay: "r2:1".into(), lends_relay: false, ..Default::default() },
            Contact { name: "Ana".into(), relay: "r1:1".into(), lends_relay: true, ..Default::default() },
            Contact { name: "Lee".into(), relay: " ".into(), lends_relay: true, ..Default::default() },
        ];
        assert_eq!(lent_by(&contacts), vec![("r1:1".to_string(), "Sam".to_string())]);
    }

    #[test]
    fn learned_mesh_relays_go_first_and_stay_capped() {
        let known: Vec<String> = (0..40).map(|i| format!("k{i}:1")).collect();
        let merged = merge_mesh(&known, &["new:1".into(), "k0:1".into()]);
        assert_eq!(merged[..2], ["new:1".to_string(), "k0:1".to_string()]);
        assert_eq!(merged.len(), MAX_MESH_RELAYS);
    }

    #[tokio::test]
    async fn with_nothing_else_up_a_lent_relay_is_chosen() {
        let socket = std::sync::Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let addr = socket.local_addr().unwrap();
        let relay = std::sync::Arc::new(std::sync::Mutex::new(antiphon_relay::Relay::default()));
        let server = tokio::spawn(antiphon_relay::run(socket, relay));
        let mut config = crate::config::AppConfig { use_project_relays: false, ..Default::default() };
        config.contacts.push(crate::config::Contact {
            name: "Sam".into(),
            relay: addr.to_string(),
            lends_relay: true,
            ..Default::default()
        });
        let choice = choose(&config).await.unwrap();
        assert_eq!(choice, Choice { relay: addr.to_string(), source: Source::Borrowed, lender: Some("Sam".into()) });
        // Your own relay, when you have one, is never passed over for a lent one.
        config.relay_server = "127.0.0.1:1".into();
        assert_eq!(choose(&config).await, None);
        server.abort();
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
