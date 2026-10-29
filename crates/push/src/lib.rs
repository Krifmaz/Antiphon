//! Sending push wake-ups to sleeping phones (spec §10, `docs/mobile-push.md`).
//!
//! The wake-up itself is [`antiphon_crypto::push`]: a constant-size blob
//! sealed to the phone's push key. This crate is everything around it that
//! a *sender* needs, and that the phone needs to tell senders where it is:
//!
//! - [`endpoint`]: what a push target is (a UnifiedPush URL or an APNs
//!   device token, plus the push key to seal to), and how it travels to
//!   contacts inside the phone's signed route record.
//! - [`limit`]: a per-key rate limit, so wake-ups cannot be used to make
//!   someone's phone buzz all day. The desktop, the mailbox node and the
//!   phone itself all apply one.
//! - [`apns_token`]: the ES256 provider token Apple's API takes. Pure
//!   computation, so it is always compiled and always tested.
//! - [`send`] (feature `send`): posting to a UnifiedPush endpoint over
//!   HTTPS with ureq and rustls, and to APNs when the `apns` feature is on
//!   and a key is configured.
//!
//! The phone builds this crate without default features, which leaves the
//! network out of its build entirely.

#![warn(missing_docs, clippy::missing_docs_in_private_items)]

pub mod apns_token;
pub mod endpoint;
pub mod limit;
#[cfg(feature = "send")]
pub mod send;

pub use endpoint::{Target, Transport, ROUTE_KIND};
pub use limit::RateLimit;

use antiphon_crypto::push::{self, Wake, WakeKind, SEALED_LEN};
use antiphon_crypto::{CryptoError, IdentityKeyPair};

/// Seals a fresh wake-up of `kind` for `target`, signed by `sender`, dated
/// `now` (Unix seconds). A thin wrapper over [`antiphon_crypto::push::seal`]
/// so every sender builds wake-ups the same way, with a new random id each
/// time and the hint checked against its limit.
pub fn seal_wake(
    sender: &IdentityKeyPair,
    target: &Target,
    kind: WakeKind,
    hint: &[u8],
    now: u64,
) -> Result<[u8; SEALED_LEN], CryptoError> {
    let wake = Wake::new(kind, now, hint)?;
    push::seal(sender, &target.push_key, &wake)
}

/// The current time in Unix seconds, or 0 if the clock is before 1970.
/// Wake-ups carry this so the receiver can refuse stale ones.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Sealing through the wrapper opens on the phone with the same kind.
#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::push::PushKeyPair;

    /// What [`seal_wake`] makes is what the phone's key opens.
    #[test]
    fn a_sealed_wake_opens_on_the_target() {
        let me = IdentityKeyPair::generate();
        let phone = PushKeyPair::generate();
        let target = Target { push_key: phone.public(), transport: Transport::UnifiedPush("https://ntfy.sh/upAbc".into()) };
        let sealed = seal_wake(&me, &target, WakeKind::Message, b"hint", 1_000).unwrap();
        let (from, wake) = push::open(&phone, &sealed).unwrap();
        assert_eq!(from.identity_key, me.public().identity_key);
        assert_eq!(wake.kind, WakeKind::Message);
        assert_eq!(wake.hint, b"hint");
        assert_eq!(wake.sent_at, 1_000);
    }
}
