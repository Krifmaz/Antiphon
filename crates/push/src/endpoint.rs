//! Where to send a phone's wake-ups, and how a phone tells its contacts.
//!
//! A [`Target`] is the pair a sender needs: the push service address
//! ([`Transport`]) and the device's push key to seal to. The phone
//! publishes its targets inside its **signed route record**
//! ([`antiphon_crypto::SignedRouteRecord`]), the same record that already
//! says where to leave its mail. That record is signed by the phone's
//! identity key and checked against the key the contact is pinned to, so a
//! contact only ever seals to a push key the phone itself published, never
//! to one a man in the middle supplied (which would let them read who is
//! calling).
//!
//! Each target is one route: the kind is `push:` followed by the push key
//! in hex, and the address is the transport's text form. The key goes in
//! the kind rather than the address so the address keeps the record's full
//! 256-byte limit for the endpoint itself; UnifiedPush endpoints from
//! FCM-backed distributors carry a long token and need most of it. Older
//! builds keep unknown kinds as they are and never mistake one of these for
//! a mailbox, because a mailbox route's address always ends in `#label`.

use antiphon_crypto::push::WakeKind;
use antiphon_crypto::{Route, SignedRouteRecord};

/// The route kind prefix every push target is published under.
pub const ROUTE_KIND: &str = "push";

/// The most an endpoint's text form may be: the route record's limit on
/// one address, so a target that parses here always fits in a record.
pub const MAX_ENDPOINT_LEN: usize = 256;

/// The text prefix of an APNs VoIP (PushKit) device token.
const APNS_VOIP: &str = "apns-voip:";
/// The text prefix of an APNs alert (ordinary notification) device token.
const APNS_ALERT: &str = "apns-alert:";

/// How a wake-up reaches the device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Transport {
    /// A UnifiedPush endpoint: an `https://` URL that takes the sealed
    /// bytes as a POST body. Carries every kind of wake-up.
    UnifiedPush(String),
    /// An APNs PushKit token, lowercase hex. Only for [`WakeKind::Call`]:
    /// iOS requires every VoIP push to be reported as a call at once, and
    /// stops delivering them to an app that receives one that does not
    /// ring.
    ApnsVoip(String),
    /// An APNs token for ordinary alert pushes, lowercase hex. For every
    /// kind except calls, for the same reason.
    ApnsAlert(String),
}

impl Transport {
    /// Parses the text form: a URL for UnifiedPush, or `apns-voip:` /
    /// `apns-alert:` followed by the token in hex. Refuses anything else,
    /// anything over [`MAX_ENDPOINT_LEN`], and URLs with a fragment, since
    /// a fragment is never sent to the server and would only make the text
    /// ambiguous.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() || text.len() > MAX_ENDPOINT_LEN {
            return None;
        }
        if let Some(token) = text.strip_prefix(APNS_VOIP) {
            return is_token(token).then(|| Transport::ApnsVoip(token.to_ascii_lowercase()));
        }
        if let Some(token) = text.strip_prefix(APNS_ALERT) {
            return is_token(token).then(|| Transport::ApnsAlert(token.to_ascii_lowercase()));
        }
        let rest = text.strip_prefix("https://").or_else(|| text.strip_prefix("http://"))?;
        let host = rest.split(['/', '?']).next().unwrap_or("");
        if host.is_empty() || text.contains('#') || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return None;
        }
        Some(Transport::UnifiedPush(text.to_string()))
    }

    /// The text form [`Transport::parse`] reads back.
    pub fn to_text(&self) -> String {
        match self {
            Transport::UnifiedPush(url) => url.clone(),
            Transport::ApnsVoip(token) => format!("{APNS_VOIP}{token}"),
            Transport::ApnsAlert(token) => format!("{APNS_ALERT}{token}"),
        }
    }

    /// Whether a wake-up of `kind` may go this way. UnifiedPush takes
    /// anything; APNs splits calls (VoIP) from everything else (alert), as
    /// iOS insists.
    pub fn carries(&self, kind: WakeKind) -> bool {
        match self {
            Transport::UnifiedPush(_) => true,
            Transport::ApnsVoip(_) => kind == WakeKind::Call,
            Transport::ApnsAlert(_) => kind != WakeKind::Call,
        }
    }

    /// What the phone's platform layer reported, as a transport: Android
    /// reports a UnifiedPush URL, iOS a bare PushKit token in hex, which is
    /// always a VoIP token since PushKit is the only registry it has.
    pub fn from_platform(reported: &str) -> Option<Self> {
        let reported = reported.trim();
        if is_token(reported) {
            return Some(Transport::ApnsVoip(reported.to_ascii_lowercase()));
        }
        Self::parse(reported)
    }
}

/// Whether `s` looks like an APNs device token: hex, an even number of
/// digits, and between 32 and 200 bytes' worth (today's tokens are 32
/// bytes, and Apple has said they may grow).
fn is_token(s: &str) -> bool {
    (64..=400).contains(&s.len()) && s.len().is_multiple_of(2) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One way to wake one device: where to send, and what to seal to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    /// The device's X25519 push key ([`antiphon_crypto::push::PushKeyPair`]).
    pub push_key: [u8; 32],
    /// The push service address.
    pub transport: Transport,
}

impl Target {
    /// This target as a route for the phone's signed record.
    pub fn to_route(&self) -> Route {
        Route { kind: format!("{ROUTE_KIND}:{}", hex(&self.push_key)), address: self.transport.to_text() }
    }

    /// The target a route describes, or `None` for any other kind of route
    /// or one that does not parse.
    pub fn from_route(route: &Route) -> Option<Self> {
        let key = route.kind.strip_prefix(ROUTE_KIND)?.strip_prefix(':')?;
        Some(Self { push_key: unhex32(key)?, transport: Transport::parse(&route.address)? })
    }

    /// Whether a route is a push target at all, parseable or not, so that
    /// code walking the record for mailboxes can skip it.
    pub fn is_push_route(route: &Route) -> bool {
        route.kind == ROUTE_KIND || route.kind.starts_with("push:")
    }
}

/// Every push target in a contact's record, in the order they published
/// them. The caller must have checked the record against the contact's
/// identity key first (`delivery::accept_record` on the desktop).
pub fn targets_in(record: &SignedRouteRecord) -> Vec<Target> {
    record.record.routes.iter().filter_map(Target::from_route).collect()
}

/// The first of `targets` that can carry a wake-up of `kind`: a call goes
/// to UnifiedPush or a VoIP token, anything else to UnifiedPush or an
/// alert token.
pub fn pick(targets: &[Target], kind: WakeKind) -> Option<&Target> {
    targets.iter().find(|t| t.transport.carries(kind))
}

/// Lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Exactly 32 bytes from 64 hex digits, or `None`.
pub fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Parsing, the route round trip, and which transport carries what.
#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::{IdentityKeyPair, RouteRecord};

    /// A 32-byte token in hex, as PushKit hands it over.
    const TOKEN: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

    /// Each text form reads back as what wrote it.
    #[test]
    fn transports_round_trip_through_text() {
        for t in [
            Transport::UnifiedPush("https://ntfy.sh/upAbC123?up=1".into()),
            Transport::ApnsVoip(TOKEN.into()),
            Transport::ApnsAlert(TOKEN.into()),
        ] {
            assert_eq!(Transport::parse(&t.to_text()), Some(t));
        }
    }

    /// Junk, other schemes, fragments, spaces and overlong text are refused.
    #[test]
    fn bad_endpoints_are_refused() {
        for bad in [
            "",
            "ftp://x/y",
            "https://",
            "https://host/path#frag",
            "https://host/a b",
            "apns-voip:xyz",
            "apns-alert:abc",
            "mailto:someone",
        ] {
            assert_eq!(Transport::parse(bad), None, "{bad}");
        }
        let long = format!("https://h/{}", "a".repeat(MAX_ENDPOINT_LEN));
        assert_eq!(Transport::parse(&long), None);
    }

    /// What each platform reports maps to the right transport.
    #[test]
    fn platform_reports_map_to_transports() {
        assert_eq!(Transport::from_platform(TOKEN), Some(Transport::ApnsVoip(TOKEN.into())));
        assert_eq!(
            Transport::from_platform("https://push.example/up/1"),
            Some(Transport::UnifiedPush("https://push.example/up/1".into()))
        );
        assert_eq!(Transport::from_platform("nonsense"), None);
    }

    /// A target survives a signed record, and is skipped when walking it
    /// for anything else.
    #[test]
    fn a_target_round_trips_through_a_signed_record() {
        let me = IdentityKeyPair::generate();
        let target = Target { push_key: [7; 32], transport: Transport::UnifiedPush("https://ntfy.sh/upX".into()) };
        let record = RouteRecord {
            identity_key: me.public().identity_key,
            sequence: 1,
            timestamp_secs: 0,
            routes: vec![Route { kind: "direct".into(), address: String::new() }, target.to_route()],
        }
        .sign(&me)
        .unwrap();
        let back = SignedRouteRecord::decode(&record.encode()).unwrap();
        back.verify().unwrap();
        assert_eq!(targets_in(&back), vec![target]);
        assert!(!Target::is_push_route(&back.record.routes[0]));
        assert!(Target::is_push_route(&back.record.routes[1]));
        // No '#', so an older build's mailbox parser can never take it for one.
        assert!(!back.record.routes[1].address.contains('#'));
    }

    /// Calls go VoIP or UnifiedPush; everything else alert or UnifiedPush.
    #[test]
    fn each_kind_picks_a_transport_that_carries_it() {
        let voip = Target { push_key: [1; 32], transport: Transport::ApnsVoip(TOKEN.into()) };
        let alert = Target { push_key: [1; 32], transport: Transport::ApnsAlert(TOKEN.into()) };
        let both = [voip.clone(), alert.clone()];
        assert_eq!(pick(&both, WakeKind::Call), Some(&voip));
        assert_eq!(pick(&both, WakeKind::Message), Some(&alert));
        assert_eq!(pick(&[voip], WakeKind::Message), None, "a VoIP token must never carry mail");
        let up = Target { push_key: [1; 32], transport: Transport::UnifiedPush("https://h/x".into()) };
        assert_eq!(pick(std::slice::from_ref(&up), WakeKind::File), Some(&up));
    }

    /// Hex helpers round trip and refuse the wrong length.
    #[test]
    fn hex_round_trips() {
        assert_eq!(unhex32(&hex(&[0xAB; 32])), Some([0xAB; 32]));
        assert_eq!(unhex32("ab"), None);
    }
}
