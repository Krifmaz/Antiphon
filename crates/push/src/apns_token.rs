//! The provider token Apple's push API takes (token-based authentication).
//!
//! APNs authenticates whoever sends with a short JSON Web Token signed
//! ES256 by a key from an Apple developer account (a `.p8` file), naming
//! the key id and the team. A phone user cannot hold that key: it would
//! let anyone push to every install of the app. So only the operator of a
//! mailbox node configures one, and that node posts iOS wake-ups for
//! everyone who uses it (see `docs/mobile-push.md`).
//!
//! This part is pure computation and always compiled, so it is tested on
//! every build; the HTTP/2 connection that carries the token is behind the
//! `apns` feature in [`crate::send`].

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;

/// How long one token is reused. Apple refuses a token older than an hour
/// and throttles a sender that mints new ones more often than every 20
/// minutes, so a token is renewed after 40.
pub const REUSE_SECS: u64 = 40 * 60;

/// A signing key with its ids, and the last token made from it.
pub struct ProviderToken {
    /// The P-256 key from the `.p8` file.
    key: SigningKey,
    /// The 10-character key id Apple shows beside the key.
    key_id: String,
    /// The 10-character team id of the developer account.
    team_id: String,
    /// The current token and when it was issued, in Unix seconds.
    cached: Option<(u64, String)>,
}

impl ProviderToken {
    /// Reads a `.p8` file's contents (PKCS#8 PEM, as Apple hands it out).
    pub fn from_p8(pem: &str, key_id: &str, team_id: &str) -> Result<Self, String> {
        let key = SigningKey::from_pkcs8_pem(pem.trim()).map_err(|e| format!("not an APNs .p8 key: {e}"))?;
        Ok(Self::from_key(key, key_id, team_id))
    }

    /// From a key already in hand; for tests and for callers that keep the
    /// key somewhere other than a file.
    pub fn from_key(key: SigningKey, key_id: &str, team_id: &str) -> Self {
        Self { key, key_id: key_id.trim().to_string(), team_id: team_id.trim().to_string(), cached: None }
    }

    /// The token to send at `now` (Unix seconds): the cached one while it
    /// is younger than [`REUSE_SECS`], otherwise a fresh one.
    pub fn token(&mut self, now: u64) -> &str {
        let stale = self.cached.as_ref().is_none_or(|(at, _)| now.saturating_sub(*at) >= REUSE_SECS || *at > now);
        if stale {
            self.cached = Some((now, self.mint(now)));
        }
        &self.cached.as_ref().expect("set just above").1
    }

    /// A new token issued at `now`: header `{alg, kid}`, claims
    /// `{iss, iat}`, each base64url without padding, then the ES256
    /// signature over `header.claims` as the 64-byte `r || s` JWS expects.
    fn mint(&self, now: u64) -> String {
        let header = format!(r#"{{"alg":"ES256","kid":"{}"}}"#, json_safe(&self.key_id));
        let claims = format!(r#"{{"iss":"{}","iat":{now}}}"#, json_safe(&self.team_id));
        let signing_input = format!("{}.{}", B64URL.encode(header), B64URL.encode(claims));
        let signature: Signature = self.key.sign(signing_input.as_bytes());
        format!("{signing_input}.{}", B64URL.encode(signature.to_bytes()))
    }
}

/// Keeps only the characters Apple's ids are made of, so an id with a
/// stray quote cannot change the JSON around it.
fn json_safe(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_').collect()
}

/// The token's shape, its signature, and when it is renewed.
#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::VerifyingKey;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};

    /// A fixed test key, so the tests need no randomness source.
    fn key() -> SigningKey {
        SigningKey::from_bytes(&[0x42; 32].into()).unwrap()
    }

    /// Three base64url parts; the header and claims say what Apple wants;
    /// the signature verifies with the key's public half.
    #[test]
    fn the_token_is_a_valid_es256_jwt() {
        let mut t = ProviderToken::from_key(key(), "ABC123DEFG", "TEAM123456");
        let jwt = t.token(1_700_000_000).to_string();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header = String::from_utf8(B64URL.decode(parts[0]).unwrap()).unwrap();
        let claims = String::from_utf8(B64URL.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header, r#"{"alg":"ES256","kid":"ABC123DEFG"}"#);
        assert_eq!(claims, r#"{"iss":"TEAM123456","iat":1700000000}"#);
        let sig = Signature::from_slice(&B64URL.decode(parts[2]).unwrap()).unwrap();
        let verifying = VerifyingKey::from(&key());
        verifying.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
    }

    /// Reused inside the window, renewed after it.
    #[test]
    fn the_token_is_reused_then_renewed() {
        let mut t = ProviderToken::from_key(key(), "K", "T");
        let first = t.token(1_000).to_string();
        assert_eq!(t.token(1_000 + REUSE_SECS - 1), first);
        assert_ne!(t.token(1_000 + REUSE_SECS), first);
    }

    /// A key written as Apple writes `.p8` files reads back.
    #[test]
    fn a_p8_file_loads() {
        let pem = key().to_pkcs8_pem(LineEnding::LF).unwrap();
        assert!(ProviderToken::from_p8(&pem, "K", "T").is_ok());
        assert!(ProviderToken::from_p8("not a key", "K", "T").is_err());
    }
}
