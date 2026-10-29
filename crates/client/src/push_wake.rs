//! Waking a contact's phone before calling it or after leaving it mail
//! (spec §10, `docs/mobile-push.md`).
//!
//! A phone publishes its push targets (endpoint plus push key) in its
//! signed route record, which this app already keeps per contact and
//! checks against the contact's pinned identity key. So a wake-up is only
//! ever sealed to a key the phone itself published.
//!
//! - **Calls**: [`wake_later`] starts a timer when a call to a contact is
//!   dialed. If the call has not connected when it fires, the phone was
//!   not reachable, so a `Call` wake-up goes out and the dial carries on;
//!   the phone wakes, starts listening and rings when the dial lands. If
//!   the call connects first, the caller aborts the timer and nothing is
//!   sent, so a phone that is already awake is not pushed at.
//! - **Mail**: [`send`] after a message has been left at one of the
//!   phone's mailboxes, so it fetches now rather than at its next poll.
//!
//! UnifiedPush targets are posted straight from here. APNs targets need an
//! Apple key that no app user holds, so they are handed to the mailbox
//! nodes on this user's own route list, in order, for one with APNs
//! configured to pass on ([`antiphon_net::mailbox::forward_wake`]). Both
//! are rate-limited per contact, so a redial loop or a burst of queued
//! mail cannot turn into a burst of pushes.

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use antiphon_crypto::push::{WakeKind, SEALED_LEN};
use antiphon_crypto::{IdentityKeyPair, SignedRouteRecord};
use antiphon_net::mailbox;
use antiphon_net::vpn::TrafficKind;
use antiphon_push::endpoint::{pick, targets_in};
use antiphon_push::send::Sender;
use antiphon_push::{RateLimit, Transport};

/// How long a dial gets to connect on its own before the phone is woken.
/// Long enough for a direct call to an awake phone on the same network to
/// finish its handshake, short enough that a sleeping phone still rings
/// while the caller is listening for it.
pub const CALL_GRACE: Duration = Duration::from_secs(2);

/// Call wake-ups sent, per contact identity key.
static CALL_LIMIT: Mutex<Option<RateLimit<[u8; 32]>>> = Mutex::new(None);
/// Mail wake-ups sent, per contact identity key.
static MAIL_LIMIT: Mutex<Option<RateLimit<[u8; 32]>>> = Mutex::new(None);

/// The one HTTP sender, so its connection pool is shared.
fn sender() -> &'static Sender {
    static SENDER: OnceLock<Sender> = OnceLock::new();
    SENDER.get_or_init(Sender::new)
}

/// Whether one more wake-up of `kind` may go to `contact` now.
fn allow(contact: &[u8; 32], kind: WakeKind) -> bool {
    let (limit, fresh) = if kind == WakeKind::Call {
        (&CALL_LIMIT, RateLimit::for_calls as fn() -> RateLimit<[u8; 32]>)
    } else {
        (&MAIL_LIMIT, RateLimit::for_mail as fn() -> RateLimit<[u8; 32]>)
    };
    limit.lock().unwrap().get_or_insert_with(fresh).allow(contact, Instant::now())
}

/// Whether a contact's record names anywhere to wake them for `kind`.
pub fn can_wake(record: &SignedRouteRecord, kind: WakeKind) -> bool {
    pick(&targets_in(record), kind).is_some()
}

/// Seals a wake-up of `kind` for the contact whose verified record this
/// is, and sends it in the background. Returns whether one was sent off:
/// false when they publish no target for this kind or the rate limit says
/// not now. Must be called inside the tokio runtime.
pub fn send(identity: &IdentityKeyPair, contact: &[u8; 32], record: &SignedRouteRecord, kind: WakeKind, hint: &[u8]) -> bool {
    let targets = targets_in(record);
    let Some(target) = pick(&targets, kind) else { return false };
    if !allow(contact, kind) {
        return false;
    }
    let Ok(sealed) = antiphon_push::seal_wake(identity, target, kind, hint, antiphon_push::now_secs()) else { return false };
    let transport = target.transport.clone();
    tokio::spawn(async move {
        if let Err(e) = deliver(kind, &transport, &sealed).await {
            eprintln!("push wake-up not delivered: {e}");
        }
    });
    true
}

/// Starts the call timer described in the module comment: after
/// [`CALL_GRACE`], wake the contact's phone. Returns `None`, and starts
/// nothing, when the contact's saved record does not verify against their
/// key or names no call target. The caller aborts the handle when the call
/// connects or is given up.
pub fn wake_later(
    identity: Arc<IdentityKeyPair>,
    contact_key_hex: &str,
    record_hex: &str,
    hint: Vec<u8>,
) -> Option<tokio::task::JoinHandle<()>> {
    let contact = crate::connection_mode::decode_hex32(contact_key_hex)?;
    let record = crate::delivery::accept_record(record_hex, &contact, None)?;
    if !can_wake(&record, WakeKind::Call) {
        return None;
    }
    Some(tokio::spawn(async move {
        tokio::time::sleep(CALL_GRACE).await;
        send(&identity, &contact, &record, WakeKind::Call, &hint);
    }))
}

/// Posts one sealed wake-up: UnifiedPush directly, APNs through the first
/// of this user's own mailbox nodes that will pass it on.
async fn deliver(kind: WakeKind, transport: &Transport, sealed: &[u8; SEALED_LEN]) -> Result<(), String> {
    if let Transport::UnifiedPush(_) = transport {
        return sender().send(kind, transport, sealed).await.map_err(|e| e.to_string());
    }
    let config = crate::config::load();
    let nodes = crate::delivery::inbox_nodes(&config.routes, config.strict_mode);
    if nodes.is_empty() {
        return Err("an iPhone needs a mailbox node with an Apple push key to pass it on, and none is set up".into());
    }
    let socket = crate::vpn::bind(TrafficKind::Messages, 0).await.map_err(|e| e.to_string())?;
    let endpoint = transport.to_text();
    let mut last = String::new();
    for (_, node, code) in nodes {
        let Some(addr) = crate::delivery::resolve(TrafficKind::Messages, &node).await else { continue };
        match mailbox::forward_wake(&socket, addr, &code, &endpoint, sealed, crate::delivery::NODE_TIMEOUT).await {
            Ok(()) => return Ok(()),
            Err(e) => last = format!("{node}: {e}"),
        }
    }
    Err(last)
}

/// What the desktop decides before anything goes on the network.
#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::push::{open, PushKeyPair};
    use antiphon_crypto::{Route, RouteRecord};
    use antiphon_push::Target;

    /// A phone's record with one UnifiedPush target.
    fn phone_record(phone: &IdentityKeyPair, push: &PushKeyPair) -> SignedRouteRecord {
        let target = Target { push_key: push.public(), transport: Transport::UnifiedPush("https://ntfy.sh/upTest".into()) };
        RouteRecord {
            identity_key: phone.public().identity_key,
            sequence: 1,
            timestamp_secs: 0,
            routes: vec![Route { kind: "direct".into(), address: String::new() }, target.to_route()],
        }
        .sign(phone)
        .unwrap()
    }

    /// A phone's record verifies, names a target for calls and mail, and
    /// a record without one names none.
    #[test]
    fn records_say_whether_a_phone_can_be_woken() {
        let phone = IdentityKeyPair::generate();
        let push = PushKeyPair::generate();
        let record = phone_record(&phone, &push);
        let hex = crate::delivery::encode_record(&record);
        let back = crate::delivery::accept_record(&hex, &phone.public().identity_key, None).unwrap();
        assert!(can_wake(&back, WakeKind::Call));
        assert!(can_wake(&back, WakeKind::Message));
        // The push route is never taken for a mailbox.
        assert!(crate::delivery::mailboxes_in(&back).is_empty());
        let bare = RouteRecord { identity_key: phone.public().identity_key, sequence: 1, timestamp_secs: 0, routes: vec![] }
            .sign(&phone)
            .unwrap();
        assert!(!can_wake(&bare, WakeKind::Call));
    }

    /// A record that someone else signed is not used, so nobody can slip a
    /// contact a push key of their own.
    #[test]
    fn only_the_contacts_own_record_is_used() {
        let phone = IdentityKeyPair::generate();
        let impostor = IdentityKeyPair::generate();
        let record = phone_record(&impostor, &PushKeyPair::generate());
        let hex = crate::delivery::encode_record(&record);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _guard = runtime.enter();
        let me = Arc::new(IdentityKeyPair::generate());
        assert!(wake_later(me, &crate::connection_mode::encode_hex32(&phone.public().identity_key), &hex, vec![]).is_none());
    }

    /// The per-contact limit lets a burst of calls through, then holds.
    #[test]
    fn call_wakes_are_rate_limited_per_contact() {
        let someone = [0x5A; 32];
        let allowed = (0..10).filter(|_| allow(&someone, WakeKind::Call)).count();
        assert_eq!(allowed, 3);
        assert!(allow(&[0x5B; 32], WakeKind::Call), "another contact is unaffected");
    }

    /// What the phone opens is what this module sealed.
    #[test]
    fn the_phone_opens_what_is_sealed_here() {
        let me = IdentityKeyPair::generate();
        let phone = IdentityKeyPair::generate();
        let push = PushKeyPair::generate();
        let record = phone_record(&phone, &push);
        let targets = targets_in(&record);
        let target = pick(&targets, WakeKind::Call).unwrap();
        let sealed = antiphon_push::seal_wake(&me, target, WakeKind::Call, b"tok", 5).unwrap();
        let (from, wake) = open(&push, &sealed).unwrap();
        assert_eq!(from.identity_key, me.public().identity_key);
        assert_eq!(wake.kind, WakeKind::Call);
    }
}
