//! Choosing how a call is carried (spec §3 "Who you connect to directly").
//!
//! - **Known contacts -> Direct.** Fastest, but both sides see each
//!   other's IP address.
//! - **Everyone else -> Relay**, the default for anyone not yet known: the
//!   relay passes sealed packets along and neither side learns the other's
//!   address.
//! - Chosen automatically, **always overridable per conversation**.
//!
//! "Known" means a call has completed with them at least once (so their
//! prekey is on file) or their key was verified out of band. A contact
//! saved from an invite link but never spoken to is still a stranger in
//! every sense that matters for exposing an IP address.
//!
//! When the automatic choice is Relay and there is no relay to use, the
//! call is *refused with the reason* rather than quietly placed direct:
//! "nothing happens to your connection that you weren't told about first"
//! (spec §0). Picking Direct for that contact is one click and is then the
//! user's informed choice.

use antiphon_crypto::relay_token::{self, RelayToken};
use antiphon_crypto::IdentityKeyPair;

use crate::config::Contact;

/// A contact's override, as stored (`Contact::connection_mode`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Override {
    Auto,
    Direct,
    Relay,
}

impl Override {
    pub fn from_i32(v: i32) -> Self {
        match v {
            1 => Override::Direct,
            2 => Override::Relay,
            _ => Override::Auto,
        }
    }
}

/// What to do to reach a contact.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Plan {
    Direct { address: String },
    Relay { relay: String, token: RelayToken },
}

pub fn is_known(contact: &Contact) -> bool {
    decode_hex32(&contact.prekey).is_some() || contact.verified
}

/// Decides how to reach `contact` today, or says why it can't be done.
pub fn plan(contact: &Contact, identity: &IdentityKeyPair, unix_secs: u64) -> Result<Plan, String> {
    let direct = || -> Result<Plan, String> {
        if contact.address.trim().is_empty() {
            Err(format!("There's no address for {} -- calling them direct needs one.", contact.name))
        } else {
            Ok(Plan::Direct { address: contact.address.trim().to_string() })
        }
    };
    let relay = || -> Result<Plan, String> {
        relay_plan(contact, identity, unix_secs).ok_or_else(|| {
            format!(
                "{} hasn't shared a relay, so the only way to reach them is direct -- which \
                 shows each of you the other's IP address. Set them to Direct to call anyway.",
                contact.name
            )
        })
    };
    match Override::from_i32(contact.connection_mode) {
        Override::Direct => direct(),
        Override::Relay => relay(),
        Override::Auto if is_known(contact) => direct().or_else(|_| relay()),
        Override::Auto => relay(),
    }
}

fn relay_plan(contact: &Contact, identity: &IdentityKeyPair, unix_secs: u64) -> Option<Plan> {
    let relay = contact.relay.trim();
    if relay.is_empty() {
        return None;
    }
    let day = relay_token::day_number(unix_secs);
    // A pairwise token once there's a prekey: it's derived from a DH only
    // the two of you can do, so it's the stronger of the two. Before that,
    // the secret from their invite link.
    let token = if let Some(prekey) = decode_hex32(&contact.prekey) {
        relay_token::pairwise(identity, &prekey, day)
    } else {
        relay_token::from_invite_secret(&decode_hex32(&contact.relay_secret)?, day)
    };
    Some(Plan::Relay { relay: relay.to_string(), token })
}

/// Every token this machine should be reachable under at its relay today:
/// one per known contact, plus its own invite secret's, each for
/// yesterday, today and tomorrow so a clock across midnight still meets.
pub fn listening_tokens(
    contacts: &[Contact],
    identity: &IdentityKeyPair,
    invite_secret: Option<[u8; 32]>,
    unix_secs: u64,
) -> Vec<RelayToken> {
    let days = relay_token::listening_days(relay_token::day_number(unix_secs));
    let mut tokens = Vec::new();
    for contact in contacts {
        if let Some(prekey) = decode_hex32(&contact.prekey) {
            tokens.extend(days.iter().map(|&d| relay_token::pairwise(identity, &prekey, d)));
        }
    }
    if let Some(secret) = invite_secret {
        tokens.extend(days.iter().map(|&d| relay_token::from_invite_secret(&secret, d)));
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

pub fn decode_hex32(hex: &str) -> Option<[u8; 32]> {
    let hex = hex.trim();
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

pub fn encode_hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000;

    fn stranger(name: &str) -> Contact {
        Contact { name: name.into(), address: "203.0.113.7:6000".into(), ..Default::default() }
    }

    #[test]
    fn a_known_contact_goes_direct() {
        let me = IdentityKeyPair::generate();
        let them = IdentityKeyPair::generate();
        let mut c = stranger("Sam");
        c.prekey = encode_hex32(&them.public().prekey);
        c.relay = "relay.example:3479".into();
        assert_eq!(plan(&c, &me, NOW), Ok(Plan::Direct { address: "203.0.113.7:6000".into() }));
    }

    #[test]
    fn a_stranger_goes_through_their_relay() {
        let me = IdentityKeyPair::generate();
        let mut c = stranger("Sam");
        c.relay = "relay.example:3479".into();
        c.relay_secret = encode_hex32(&[4; 32]);
        let expected = relay_token::from_invite_secret(&[4; 32], relay_token::day_number(NOW));
        assert_eq!(plan(&c, &me, NOW), Ok(Plan::Relay { relay: "relay.example:3479".into(), token: expected }));
    }

    #[test]
    fn a_stranger_with_no_relay_is_refused_with_the_reason_not_dialled_direct() {
        let me = IdentityKeyPair::generate();
        let err = plan(&stranger("Sam"), &me, NOW).unwrap_err();
        assert!(err.contains("IP address"), "{err}");
        assert!(err.contains("Direct"), "should say how to proceed: {err}");
    }

    #[test]
    fn the_override_always_wins() {
        let me = IdentityKeyPair::generate();
        let mut c = stranger("Sam");
        c.connection_mode = 1;
        assert!(matches!(plan(&c, &me, NOW), Ok(Plan::Direct { .. })));

        let them = IdentityKeyPair::generate();
        let mut c = stranger("Sam");
        c.prekey = encode_hex32(&them.public().prekey);
        c.connection_mode = 2;
        assert!(plan(&c, &me, NOW).is_err(), "relay forced but none known");
        c.relay = "relay.example:3479".into();
        assert!(matches!(plan(&c, &me, NOW), Ok(Plan::Relay { .. })));
    }

    #[test]
    fn a_dialled_pairwise_token_is_one_the_listener_registered() {
        let alice = IdentityKeyPair::generate();
        let bob = IdentityKeyPair::generate();
        let mut bob_as_seen_by_alice = stranger("Bob");
        bob_as_seen_by_alice.prekey = encode_hex32(&bob.public().prekey);
        bob_as_seen_by_alice.relay = "r:1".into();
        bob_as_seen_by_alice.connection_mode = 2;
        let mut alice_as_seen_by_bob = stranger("Alice");
        alice_as_seen_by_bob.prekey = encode_hex32(&alice.public().prekey);

        let Ok(Plan::Relay { token, .. }) = plan(&bob_as_seen_by_alice, &alice, NOW) else { panic!() };
        // Bob's clock is nearly a day behind -- still found.
        let registered = listening_tokens(&[alice_as_seen_by_bob], &bob, None, NOW - 80_000);
        assert!(registered.contains(&token));
    }

    #[test]
    fn an_invite_holder_finds_the_listener() {
        let me = IdentityKeyPair::generate();
        let secret = [9u8; 32];
        let mut c = stranger("Me, from their side");
        c.relay = "r:1".into();
        c.relay_secret = encode_hex32(&secret);
        let Ok(Plan::Relay { token, .. }) = plan(&c, &IdentityKeyPair::generate(), NOW) else { panic!() };
        assert!(listening_tokens(&[], &me, Some(secret), NOW).contains(&token));
    }
}
