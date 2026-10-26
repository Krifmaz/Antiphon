//! Invite links (spec §4 "QR code / invite link -- primary method").
//!
//! ```text
//! antiphon://<64-hex identity key>@<host>:<port>?relay=<host>:<port>&k=<64-hex>
//! ```
//!
//! The query is optional (spec §3 "Everyone else -> Relay"): `relay` names
//! the relay the link's owner listens at, and `k` is a secret from which
//! the daily relay token is derived (`relay_token::from_invite_secret`).
//! Whoever holds the link can ring its owner through that relay without
//! either side seeing the other's address.
//!
//! The link carries **who** and **where to start**, and those two halves
//! age very differently:
//!
//! - The identity key is permanent. It is what gets pinned, what the
//!   verification code is computed from, and what makes the contact a
//!   contact.
//! - The address is a *bootstrap hint*, true only when the link was made.
//!   It exists so first contact works today, with no DHT, no relay and no
//!   directory -- paste a link, place a call.
//!
//! That split is what keeps this from being a dead end. When distributed
//! lookup does land, the address becomes optional and old links keep
//! working, because the identity was always the real identifier and the
//! address was never load-bearing for *who* you are talking to. A link
//! whose address has gone stale still names the right person; only the
//! starting point is lost.
//!
//! Nothing here is secret except `k`, which only lets someone *ring* you
//! through your relay -- revoked by generating a new one. An invite reveals
//! an identity key and an address the holder chose to publish -- it does not authenticate anyone
//! by itself, and pasting one only pins a key on first contact (see
//! [`crate::contact`]). Someone who intercepts a link can substitute their
//! own; that is exactly the gap the verification code closes.

use crate::error::CryptoError;

const SCHEME: &str = "antiphon://";

/// The scheme from before the project was named Antiphon. Still accepted,
/// never produced: links already handed to people keep working, and a
/// rename is no reason to make someone ask for a new one.
const LEGACY_SCHEME: &str = "voip://";

/// A parsed invite: a permanent identity plus a hint about where to reach
/// it right now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invite {
    pub identity_key: [u8; 32],
    /// `host:port`, or empty when the link carries no bootstrap hint.
    pub address: String,
    /// The relay its owner listens at, `host:port`, or empty.
    pub relay: String,
    /// The secret the relay token is derived from. Only meaningful with
    /// `relay` set.
    pub relay_secret: Option<[u8; 32]>,
}

impl Invite {
    /// An invite with no relay part.
    pub fn new(identity_key: [u8; 32], address: impl Into<String>) -> Self {
        Self { identity_key, address: address.into(), relay: String::new(), relay_secret: None }
    }

    /// Builds an invite link. An empty `address` produces an
    /// identity-only link, which is valid and forward-looking -- it just
    /// can't be dialled until discovery exists.
    pub fn to_link(&self) -> String {
        let hex: String = self.identity_key.iter().map(|b| format!("{b:02x}")).collect();
        let mut link = if self.address.is_empty() {
            format!("{SCHEME}{hex}")
        } else {
            format!("{SCHEME}{hex}@{}", self.address)
        };
        if let (false, Some(secret)) = (self.relay.is_empty(), self.relay_secret) {
            let k: String = secret.iter().map(|b| format!("{b:02x}")).collect();
            link.push_str(&format!("?relay={}&k={k}", self.relay));
        }
        link
    }

    /// Parses an invite link.
    ///
    /// Tolerant of surrounding whitespace and of a missing scheme, since
    /// these get pasted out of chat messages and sometimes arrive with the
    /// prefix already eaten by a link detector. Everything else is
    /// rejected rather than guessed at -- a malformed identity must not
    /// silently become a different valid one.
    pub fn parse(text: &str) -> Result<Self, CryptoError> {
        let trimmed = text.trim();
        let body = trimmed
            .strip_prefix(SCHEME)
            .or_else(|| trimmed.strip_prefix(LEGACY_SCHEME))
            .unwrap_or(trimmed);

        let (body, query) = match body.split_once('?') {
            Some((body, query)) => (body, query),
            None => (body, ""),
        };
        let mut relay = String::new();
        let mut relay_secret = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("relay", value)) if looks_like_host_port(value) => relay = value.to_string(),
                Some(("k", value)) => relay_secret = Some(parse_hex32(value)?),
                // Unknown keys are ignored so a newer link still opens in
                // an older build; a malformed known key is an error.
                Some(("relay", _)) => return Err(CryptoError::MalformedInvite),
                _ => {}
            }
        }
        if relay.is_empty() != relay_secret.is_none() {
            return Err(CryptoError::MalformedInvite);
        }

        let (hex, address) = match body.split_once('@') {
            Some((hex, address)) => (hex, address.trim()),
            None => (body, ""),
        };

        let identity_key = parse_hex32(hex.trim())?;

        if !address.is_empty() && !looks_like_host_port(address) {
            return Err(CryptoError::MalformedInvite);
        }

        Ok(Self { identity_key, address: address.to_string(), relay, relay_secret })
    }
}

fn parse_hex32(hex: &str) -> Result<[u8; 32], CryptoError> {
    if hex.len() != 64 {
        return Err(CryptoError::MalformedInvite);
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2).ok_or(CryptoError::MalformedInvite)?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| CryptoError::MalformedInvite)?;
    }
    Ok(out)
}

/// A deliberately loose check: enough to catch a truncated paste or a
/// stray word, without trying to out-guess `SocketAddr` about what a valid
/// host looks like. The dialling code parses it properly and reports its
/// own error if it turns out to be unusable.
fn looks_like_host_port(address: &str) -> bool {
    let Some((host, port)) = address.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::IdentityKeyPair;

    fn invite_for(address: &str) -> (IdentityKeyPair, Invite) {
        let id = IdentityKeyPair::generate();
        let invite = Invite::new(id.public().identity_key, address);
        (id, invite)
    }

    #[test]
    fn a_relay_part_round_trips() {
        let (_, mut invite) = invite_for("203.0.113.7:6001");
        invite.relay = "relay.example.org:3479".into();
        invite.relay_secret = Some([0xab; 32]);
        let link = invite.to_link();
        assert!(link.contains("?relay=relay.example.org:3479&k=abab"), "{link}");
        assert_eq!(Invite::parse(&link).unwrap(), invite);
    }

    #[test]
    fn a_relay_without_its_secret_is_rejected() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert!(Invite::parse(&format!("{}?relay=r.example:1", invite.to_link())).is_err());
        assert!(Invite::parse(&format!("{}?k={}", invite.to_link(), "ab".repeat(32))).is_err());
    }

    #[test]
    fn unknown_query_keys_are_ignored() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert_eq!(Invite::parse(&format!("{}?future=1", invite.to_link())).unwrap(), invite);
    }

    #[test]
    fn a_link_round_trips() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert_eq!(Invite::parse(&invite.to_link()).unwrap(), invite);
    }

    #[test]
    fn a_link_starts_with_the_scheme() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert!(invite.to_link().starts_with("antiphon://"), "got {}", invite.to_link());
    }

    #[test]
    fn an_identity_only_link_is_valid() {
        let (_, invite) = invite_for("");
        let parsed = Invite::parse(&invite.to_link()).unwrap();

        // Forward-looking: once discovery exists these are the normal
        // form, and they must not be an error before then.
        assert_eq!(parsed.identity_key, invite.identity_key);
        assert!(parsed.address.is_empty());
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        let messy = format!("  \n {}  \t", invite.to_link());
        assert_eq!(Invite::parse(&messy).unwrap(), invite);
    }

    #[test]
    fn a_link_without_the_scheme_still_parses() {
        // Chat clients and link detectors sometimes eat the prefix.
        let (_, invite) = invite_for("203.0.113.7:6001");
        let bare = invite.to_link().replace("antiphon://", "");
        assert_eq!(Invite::parse(&bare).unwrap(), invite);
    }

    #[test]
    fn a_hostname_address_is_accepted() {
        let (_, invite) = invite_for("home.example.org:9000");
        assert_eq!(Invite::parse(&invite.to_link()).unwrap(), invite);
    }

    #[test]
    fn a_truncated_identity_is_rejected() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        let link = invite.to_link();
        let truncated = &link[..link.len() - 10];

        // Must never round down to some other valid identity.
        assert!(matches!(Invite::parse(truncated), Err(CryptoError::MalformedInvite)));
    }

    #[test]
    fn a_non_hex_identity_is_rejected() {
        let bad = format!("antiphon://{}@203.0.113.7:6001", "z".repeat(64));
        assert!(Invite::parse(&bad).is_err());
    }

    #[test]
    fn an_address_without_a_port_is_rejected() {
        let (_, invite) = invite_for("");
        let link = format!("{}@203.0.113.7", invite.to_link());
        assert!(Invite::parse(&link).is_err());
    }

    #[test]
    fn an_address_with_a_non_numeric_port_is_rejected() {
        let (_, invite) = invite_for("");
        let link = format!("{}@203.0.113.7:notaport", invite.to_link());
        assert!(Invite::parse(&link).is_err());
    }

    #[test]
    fn a_link_from_before_the_rename_still_works() {
        // Links handed out as voip:// must keep working; the rename is no
        // reason to make anyone ask for a new one.
        let (_, invite) = invite_for("203.0.113.7:6001");
        let old = invite.to_link().replace(SCHEME, LEGACY_SCHEME);
        assert!(old.starts_with("voip://"), "{old}");
        assert_eq!(Invite::parse(&old).unwrap(), invite);
    }

    #[test]
    fn new_links_use_the_new_name() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert!(!invite.to_link().contains("voip"), "{}", invite.to_link());
    }

    #[test]
    fn junk_is_an_error_not_a_panic() {
        for junk in ["", "   ", "antiphon://", "hello", "antiphon://@", "@:", "antiphon://:@:", "voip://", "voip://@"] {
            assert!(Invite::parse(junk).is_err(), "expected {junk:?} to be rejected");
        }
    }

    #[test]
    fn an_ipv6_address_keeps_its_port() {
        // `rsplit_once` on ':' is what makes this work -- splitting on the
        // first colon would mangle every IPv6 address.
        let (_, invite) = invite_for("[2001:db8::1]:6001");
        let parsed = Invite::parse(&invite.to_link()).unwrap();
        assert_eq!(parsed.address, "[2001:db8::1]:6001");
    }

    #[test]
    fn the_identity_survives_a_stale_address() {
        // The point of the split: an old link still names the right
        // person even when the address it was made with is long gone.
        let (id, invite) = invite_for("203.0.113.7:6001");
        let stale = Invite::parse(&invite.to_link()).unwrap();

        assert_eq!(stale.identity_key, id.public().identity_key);
    }
}
