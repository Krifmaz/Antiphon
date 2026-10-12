//! Signed route records -- where to reach someone (spec §8 "Delivery
//! Routes", §4 "Reaching people").
//!
//! This is the module that keeps identity from becoming server-scoped.
//!
//! > "The recipient chooses where their mail is deposited, not the sender.
//! > Your route list is really your inbox configuration, published to your
//! > contacts as part of your profile."
//!
//! Identity is a keypair ([`crate::IdentityKeyPair`]) and never changes.
//! *Where* to reach that identity is this separate record, which changes
//! as often as the user likes. Because the record is signed by the
//! identity key, anyone holding the public identity can verify it without
//! a server vouching for anything -- which is precisely what a Matrix-style
//! `@user:server` identifier cannot do, since there the server name *is*
//! the identity and moving hosts means becoming someone else.
//!
//! Consequences worth being explicit about:
//!
//! - Moving from a self-hosted node to a community node, or to nothing at
//!   all, is publishing a new record. Contacts follow automatically and
//!   nobody has to re-add you.
//! - An empty route list is meaningful: it is spec §8's "Strict mode",
//!   where messages move only when both people are online and nothing is
//!   ever stored.
//! - The record carries no addresses of the *sender*, only of the
//!   publisher, so reading someone's route list tells you where to deposit
//!   their mail and nothing about anyone else.
//!
//! `sequence` exists to stop rollback. Without it, someone who captured an
//! old record could replay it to push a user back onto a route they had
//! deliberately abandoned.

use sha2::{Digest, Sha256};

use crate::error::CryptoError;
use crate::identity::{verify_signature, IdentityKeyPair};

const CONTEXT: &[u8] = b"voip-app/route-record/v1";

/// Upper bound on routes in one record, so a malformed or hostile record
/// can't make a peer allocate unboundedly while decoding.
const MAX_ROUTES: usize = 32;
/// Upper bound on a single route string, same reasoning.
const MAX_ROUTE_LEN: usize = 256;

/// One place a sender may deposit mail for the publisher, in the
/// publisher's order of preference (spec §8's tier list).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Route {
    /// Which tier this is -- `direct`, `own-device`, `trusted-node`,
    /// `community-node`, `project-relay`. Kept as a string rather than an
    /// enum so a peer running an older build can round-trip a tier it
    /// doesn't understand instead of dropping it.
    pub kind: String,
    /// Where to reach it. Empty for `direct`, which has no address of its
    /// own -- it means "only when we're both online".
    pub address: String,
}

/// A route list, before signing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RouteRecord {
    /// Whose record this is: the Ed25519 identity key.
    pub identity_key: [u8; 32],
    /// Monotonic. A record with a lower sequence than one already held is
    /// stale and must be rejected.
    pub sequence: u64,
    /// Unix seconds. Informational -- freshness is decided by `sequence`,
    /// not by clocks, which peers cannot be trusted to agree on.
    pub timestamp_secs: u64,
    pub routes: Vec<Route>,
}

impl RouteRecord {
    /// Canonical bytes covered by the signature.
    ///
    /// Hand-rolled and length-prefixed, matching `voip-protocol`'s framing.
    /// Canonical matters more than compact here: two encoders that
    /// disagree by a byte produce signatures that don't verify.
    fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(CONTEXT);
        out.extend_from_slice(&self.identity_key);
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp_secs.to_be_bytes());
        out.extend_from_slice(&(self.routes.len() as u32).to_be_bytes());
        for route in &self.routes {
            out.extend_from_slice(&(route.kind.len() as u32).to_be_bytes());
            out.extend_from_slice(route.kind.as_bytes());
            out.extend_from_slice(&(route.address.len() as u32).to_be_bytes());
            out.extend_from_slice(route.address.as_bytes());
        }
        out
    }

    /// Signs this record with the identity it names.
    ///
    /// Refuses to sign a record attributed to a different identity -- that
    /// would produce something that can never verify, and the failure
    /// would surface far from its cause.
    pub fn sign(self, identity: &IdentityKeyPair) -> Result<SignedRouteRecord, CryptoError> {
        if self.identity_key != identity.public().identity_key {
            return Err(CryptoError::MalformedIdentity);
        }
        if self.routes.len() > MAX_ROUTES {
            return Err(CryptoError::MalformedRouteRecord);
        }
        let signature = identity.sign(&self.signing_bytes());
        Ok(SignedRouteRecord { record: self, signature })
    }
}

/// A route record plus the signature proving its publisher minted it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SignedRouteRecord {
    pub record: RouteRecord,
    pub signature: [u8; 64],
}

impl SignedRouteRecord {
    /// Checks the signature against the identity the record claims.
    pub fn verify(&self) -> Result<(), CryptoError> {
        verify_signature(
            &self.record.identity_key,
            &self.record.signing_bytes(),
            &self.signature,
        )
    }

    /// Verifies, then checks this record supersedes what we already hold.
    ///
    /// `known_sequence` is the highest sequence previously accepted for
    /// this identity, or `None` on first contact. Equal sequences are
    /// rejected as well as lower ones: a publisher changing their routes
    /// must bump the counter, so accepting an equal-sequence record would
    /// let two different route lists both look current.
    pub fn accept(&self, known_sequence: Option<u64>) -> Result<(), CryptoError> {
        self.verify()?;
        if let Some(known) = known_sequence {
            if self.record.sequence <= known {
                return Err(CryptoError::StaleRouteRecord {
                    offered: self.record.sequence,
                    known,
                });
            }
        }
        Ok(())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.record.signing_bytes();
        out.extend_from_slice(&self.signature);
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, CryptoError> {
        // Signature is the trailing 64 bytes; everything before it is the
        // canonical body, which is re-parsed rather than trusted.
        if bytes.len() < 64 {
            return Err(CryptoError::MalformedRouteRecord);
        }
        let (body, signature_bytes) = bytes.split_at(bytes.len() - 64);
        let mut signature = [0u8; 64];
        signature.copy_from_slice(signature_bytes);

        let mut cursor = Cursor { bytes: body, at: 0 };
        cursor.expect(CONTEXT)?;
        let identity_key = cursor.take_array::<32>()?;
        let sequence = u64::from_be_bytes(cursor.take_array::<8>()?);
        let timestamp_secs = u64::from_be_bytes(cursor.take_array::<8>()?);
        let route_count = u32::from_be_bytes(cursor.take_array::<4>()?) as usize;
        if route_count > MAX_ROUTES {
            return Err(CryptoError::MalformedRouteRecord);
        }

        let mut routes = Vec::with_capacity(route_count);
        for _ in 0..route_count {
            let kind = cursor.take_string()?;
            let address = cursor.take_string()?;
            routes.push(Route { kind, address });
        }
        if !cursor.is_empty() {
            return Err(CryptoError::MalformedRouteRecord);
        }

        Ok(Self {
            record: RouteRecord { identity_key, sequence, timestamp_secs, routes },
            signature,
        })
    }

    /// Short stable digest of the record, handy for logging which version
    /// of someone's routes a node is holding without logging the routes.
    pub fn digest_hex(&self) -> String {
        let digest = Sha256::digest(self.encode());
        digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
    }
}

/// Minimal bounds-checked reader. Every `take` validates length first, so
/// a truncated or hostile record is an error rather than a panic.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn expect(&mut self, prefix: &[u8]) -> Result<(), CryptoError> {
        if self.remaining() < prefix.len() || &self.bytes[self.at..self.at + prefix.len()] != prefix
        {
            return Err(CryptoError::MalformedRouteRecord);
        }
        self.at += prefix.len();
        Ok(())
    }

    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], CryptoError> {
        if self.remaining() < N {
            return Err(CryptoError::MalformedRouteRecord);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.at..self.at + N]);
        self.at += N;
        Ok(out)
    }

    fn take_string(&mut self) -> Result<String, CryptoError> {
        let len = u32::from_be_bytes(self.take_array::<4>()?) as usize;
        if len > MAX_ROUTE_LEN || self.remaining() < len {
            return Err(CryptoError::MalformedRouteRecord);
        }
        let s = std::str::from_utf8(&self.bytes[self.at..self.at + len])
            .map_err(|_| CryptoError::MalformedRouteRecord)?
            .to_string();
        self.at += len;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_for(identity: &IdentityKeyPair, sequence: u64, routes: Vec<Route>) -> RouteRecord {
        RouteRecord {
            identity_key: identity.public().identity_key,
            sequence,
            timestamp_secs: 1_700_000_000,
            routes,
        }
    }

    fn sample_routes() -> Vec<Route> {
        vec![
            Route { kind: "direct".to_string(), address: String::new() },
            Route { kind: "trusted-node".to_string(), address: "home.example:9000".to_string() },
        ]
    }

    #[test]
    fn a_signed_record_verifies_under_its_own_identity() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, sample_routes()).sign(&id).unwrap();

        assert!(signed.verify().is_ok());
    }

    #[test]
    fn a_record_round_trips_through_its_wire_encoding() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 7, sample_routes()).sign(&id).unwrap();

        let decoded = SignedRouteRecord::decode(&signed.encode()).unwrap();
        assert_eq!(decoded, signed);
        assert!(decoded.verify().is_ok());
    }

    #[test]
    fn tampering_with_a_route_breaks_the_signature() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, sample_routes()).sign(&id).unwrap();

        let mut forged = signed.clone();
        forged.record.routes[1].address = "attacker.example:9000".to_string();

        // Redirecting someone's mail to a node they never authorized is
        // the attack this signature exists to stop.
        assert!(forged.verify().is_err());
    }

    #[test]
    fn a_record_cannot_be_signed_for_someone_elses_identity() {
        let alice = IdentityKeyPair::generate();
        let mallory = IdentityKeyPair::generate();

        let claiming_alice = record_for(&alice, 1, sample_routes());
        assert!(claiming_alice.sign(&mallory).is_err());
    }

    #[test]
    fn a_newer_sequence_supersedes_an_older_one() {
        let id = IdentityKeyPair::generate();
        let newer = record_for(&id, 5, sample_routes()).sign(&id).unwrap();

        assert!(newer.accept(Some(4)).is_ok());
        assert!(newer.accept(None).is_ok());
    }

    #[test]
    fn a_replayed_older_record_is_rejected() {
        let id = IdentityKeyPair::generate();
        let stale = record_for(&id, 3, sample_routes()).sign(&id).unwrap();

        // The rollback attack: push a user back onto a route they moved
        // off. The signature is perfectly valid, so only the sequence
        // check catches this.
        assert!(matches!(
            stale.accept(Some(9)),
            Err(CryptoError::StaleRouteRecord { offered: 3, known: 9 })
        ));
    }

    #[test]
    fn an_equal_sequence_is_rejected_too() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 4, sample_routes()).sign(&id).unwrap();

        assert!(signed.accept(Some(4)).is_err());
    }

    #[test]
    fn an_empty_route_list_is_valid_and_means_strict_mode() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, vec![]).sign(&id).unwrap();

        // Spec §8 "Strict mode": nothing stored anywhere, ever. It has to
        // be expressible, not an error.
        assert!(signed.verify().is_ok());
        assert!(SignedRouteRecord::decode(&signed.encode()).unwrap().record.routes.is_empty());
    }

    #[test]
    fn moving_hosts_keeps_the_same_identity() {
        let id = IdentityKeyPair::generate();

        let self_hosted = record_for(
            &id,
            1,
            vec![Route { kind: "trusted-node".to_string(), address: "home.example:9000".to_string() }],
        )
        .sign(&id)
        .unwrap();

        let moved = record_for(
            &id,
            2,
            vec![Route {
                kind: "community-node".to_string(),
                address: "community.example:9000".to_string(),
            }],
        )
        .sign(&id)
        .unwrap();

        // The entire point: the host changed, the identity did not.
        assert_eq!(self_hosted.record.identity_key, moved.record.identity_key);
        assert!(moved.accept(Some(self_hosted.record.sequence)).is_ok());
    }

    #[test]
    fn a_truncated_record_is_an_error_not_a_panic() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, sample_routes()).sign(&id).unwrap();
        let encoded = signed.encode();

        for cut in [0, 1, 20, encoded.len() / 2, encoded.len() - 1] {
            assert!(SignedRouteRecord::decode(&encoded[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn a_record_claiming_more_routes_than_it_carries_is_rejected() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, sample_routes()).sign(&id).unwrap();
        let mut encoded = signed.encode();

        // Overwrite the route count with something enormous. A decoder
        // that trusted it would try to allocate for routes that aren't
        // there.
        let count_at = CONTEXT.len() + 32 + 8 + 8;
        encoded[count_at..count_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());

        assert!(SignedRouteRecord::decode(&encoded).is_err());
    }

    #[test]
    fn trailing_bytes_after_the_routes_are_rejected() {
        let id = IdentityKeyPair::generate();
        let signed = record_for(&id, 1, sample_routes()).sign(&id).unwrap();

        let mut encoded = signed.encode();
        // Splice junk in just before the signature.
        let signature_at = encoded.len() - 64;
        encoded.splice(signature_at..signature_at, [0xAA, 0xBB]);

        assert!(SignedRouteRecord::decode(&encoded).is_err());
    }
}
