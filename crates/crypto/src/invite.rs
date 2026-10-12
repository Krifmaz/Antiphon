//! Invite links (spec §4 "QR code / invite link -- primary method").
//!
//! ```text
//! voip://<64-hex identity key>@<host>:<port>
//! ```
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
//! Nothing here is secret. An invite reveals an identity key and an
//! address the holder chose to publish -- it does not authenticate anyone
//! by itself, and pasting one only pins a key on first contact (see
//! [`crate::contact`]). Someone who intercepts a link can substitute their
//! own; that is exactly the gap the verification code closes.

use crate::error::CryptoError;

const SCHEME: &str = "voip://";

/// A parsed invite: a permanent identity plus a hint about where to reach
/// it right now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Invite {
    pub identity_key: [u8; 32],
    /// `host:port`, or empty when the link carries no bootstrap hint.
    pub address: String,
}

impl Invite {
    /// Builds an invite link. An empty `address` produces an
    /// identity-only link, which is valid and forward-looking -- it just
    /// can't be dialled until discovery exists.
    pub fn to_link(&self) -> String {
        let hex: String = self.identity_key.iter().map(|b| format!("{b:02x}")).collect();
        if self.address.is_empty() {
            format!("{SCHEME}{hex}")
        } else {
            format!("{SCHEME}{hex}@{}", self.address)
        }
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
        let body = trimmed.strip_prefix(SCHEME).unwrap_or(trimmed);

        let (hex, address) = match body.split_once('@') {
            Some((hex, address)) => (hex, address.trim()),
            None => (body, ""),
        };

        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(CryptoError::MalformedInvite);
        }
        let mut identity_key = [0u8; 32];
        for (i, byte) in identity_key.iter_mut().enumerate() {
            let pair = hex.get(i * 2..i * 2 + 2).ok_or(CryptoError::MalformedInvite)?;
            *byte = u8::from_str_radix(pair, 16).map_err(|_| CryptoError::MalformedInvite)?;
        }

        if !address.is_empty() && !looks_like_host_port(address) {
            return Err(CryptoError::MalformedInvite);
        }

        Ok(Self { identity_key, address: address.to_string() })
    }
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
        let invite = Invite {
            identity_key: id.public().identity_key,
            address: address.to_string(),
        };
        (id, invite)
    }

    #[test]
    fn a_link_round_trips() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert_eq!(Invite::parse(&invite.to_link()).unwrap(), invite);
    }

    #[test]
    fn a_link_starts_with_the_scheme() {
        let (_, invite) = invite_for("203.0.113.7:6001");
        assert!(invite.to_link().starts_with("voip://"), "got {}", invite.to_link());
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
        let bare = invite.to_link().replace("voip://", "");
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
        let bad = format!("voip://{}@203.0.113.7:6001", "z".repeat(64));
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
    fn junk_is_an_error_not_a_panic() {
        for junk in ["", "   ", "voip://", "hello", "voip://@", "@:", "voip://:@:"] {
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
