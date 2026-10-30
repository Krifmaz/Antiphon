//! This phone's side of push wake-ups (`docs/mobile-push.md`).
//!
//! Three jobs:
//!
//! - **Keep a push key.** An X25519 key of this device's own, in a file in
//!   the app's private storage, apart from the identity: the code that
//!   opens a wake-up should not need anything else unlocked.
//! - **Tell contacts how to wake it.** When the platform reports an
//!   endpoint ([`PlatformEvent::PushRegistered`](crate::platform::PlatformEvent)),
//!   it is put with the push key into a route record signed by this
//!   phone's identity, and that record goes out as the `routes` field of
//!   every `Hello` ([`published_routes`]). A contact checks it against the
//!   key they pinned for this phone, so nobody in between can substitute a
//!   push key of their own and read who is calling.
//! - **Open what arrives.** [`Push::open`] opens a sealed wake-up, drops
//!   it unless it is fresh, new, from a saved contact, and within that
//!   contact's rate limit, and says what to do: ring, fetch mail, or
//!   expect a file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use antiphon_crypto::push::{self, PushKeyPair, WakeKind};
use antiphon_crypto::{IdentityKeyPair, Route, RouteRecord, SignedRouteRecord};
use antiphon_push::{RateLimit, Target, Transport};
use serde::{Deserialize, Serialize};

/// How old a call wake-up may be and still ring. A minute allows for a
/// slow push service and two clocks that disagree, and no more: a call
/// from longer ago has been given up on.
const CALL_MAX_AGE: u64 = 60;
/// How old any other wake-up may be. Mail waits, so an hour-old notice is
/// still worth acting on.
const OTHER_MAX_AGE: u64 = 3600;

/// This phone's signed route record as last published, hex, for the
/// `routes` field of the `Hello` a call sends. A static so the call code
/// can read it without the interface handing it over on every call.
static PUBLISHED: Mutex<String> = Mutex::new(String::new());

/// What the next `Hello` should carry as `routes`: this phone's signed
/// record naming its push target, or empty before an endpoint is known.
pub fn published_routes() -> String {
    PUBLISHED.lock().unwrap().clone()
}

/// What is kept between launches, beside the key.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Saved {
    /// The endpoint as the platform last reported it.
    endpoint: String,
    /// The sequence of the record last signed. Each new record must be
    /// higher, or contacts keep the old one (that is what stops an old
    /// record being replayed).
    sequence: u64,
    /// The signed record, hex, so the same bytes go out every time until
    /// the endpoint changes.
    record: String,
}

/// What a wake-up asks this phone to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Woken {
    /// A contact is calling: be listening when the call lands.
    Ring {
        /// The caller's identity key, hex.
        from: String,
    },
    /// A contact left mail at a mailbox.
    Mail {
        /// The sender's identity key, hex.
        from: String,
    },
    /// A contact is offering a file.
    File {
        /// The sender's identity key, hex.
        from: String,
    },
}

/// The push key, the endpoint, and what has been received lately.
pub struct Push {
    /// The app's private storage.
    dir: PathBuf,
    /// This device's push key.
    key: PushKeyPair,
    /// The endpoint and record as last published.
    saved: Saved,
    /// Ids of wake-ups already acted on, with when they were sent, kept
    /// for as long as one could still count as fresh, so a push service
    /// replaying a blob gets nothing.
    seen: HashMap<[u8; 16], u64>,
    /// Call wake-ups accepted per sender.
    calls: RateLimit<[u8; 32]>,
    /// Other wake-ups accepted per sender.
    others: RateLimit<[u8; 32]>,
}

/// The push key's file.
fn key_path(dir: &Path) -> PathBuf {
    dir.join("push.key")
}

/// The endpoint and record's file.
fn saved_path(dir: &Path) -> PathBuf {
    dir.join("push.json")
}

/// Lowercase hex of any bytes.
fn hex(bytes: &[u8]) -> String {
    antiphon_crypto::hex::encode(bytes)
}

impl Push {
    /// Loads the push key, making and saving one on first start, and the
    /// last published record, which goes straight back into
    /// [`published_routes`] if it is still signed by `identity` (it is not
    /// after an identity restore, and is then re-signed on the next
    /// endpoint report).
    pub fn load(dir: &Path, identity: Option<&IdentityKeyPair>) -> Self {
        let key = match std::fs::read(key_path(dir)).ok().and_then(|b| <[u8; 32]>::try_from(b.as_slice()).ok()) {
            Some(bytes) => PushKeyPair::from_bytes(bytes),
            None => {
                let key = PushKeyPair::generate();
                if let Err(e) = std::fs::write(key_path(dir), *key.to_bytes()) {
                    log::warn!("could not save the push key: {e}");
                }
                key
            }
        };
        let saved: Saved = std::fs::read_to_string(saved_path(dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let mut push = Self {
            dir: dir.to_path_buf(),
            key,
            saved,
            seen: HashMap::new(),
            calls: RateLimit::for_calls(),
            others: RateLimit::new(5, std::time::Duration::from_secs(60)),
        };
        if let Some(identity) = identity {
            if push.record_is_ours(identity) {
                (*PUBLISHED.lock().unwrap()).clone_from(&push.saved.record);
            } else if !push.saved.endpoint.is_empty() {
                let endpoint = push.saved.endpoint.clone();
                let _ = push.registered(&endpoint, identity);
            }
        }
        push
    }

    /// Whether the saved record is signed by `identity` and names the
    /// current push key and endpoint.
    fn record_is_ours(&self, identity: &IdentityKeyPair) -> bool {
        let Some(bytes) = unhex(&self.saved.record) else { return false };
        let Ok(record) = SignedRouteRecord::decode(&bytes) else { return false };
        let target = Transport::from_platform(&self.saved.endpoint).map(|t| Target { push_key: self.key.public(), transport: t });
        record.verify().is_ok()
            && record.record.identity_key == identity.public().identity_key
            && target.is_some_and(|t| antiphon_push::endpoint::targets_in(&record) == vec![t])
    }

    /// The platform reported an endpoint (a UnifiedPush URL, or an APNs
    /// PushKit token in hex). Signs a new record naming it, saves it, and
    /// publishes it to the next call's `Hello`. Reporting the same endpoint
    /// again changes nothing, so contacts are not handed a new record (and
    /// a new sequence) on every launch.
    pub fn registered(&mut self, reported: &str, identity: &IdentityKeyPair) -> Result<(), String> {
        let transport = Transport::from_platform(reported).ok_or("the push address the phone gave is not one Stoop can use")?;
        if self.saved.endpoint == reported && self.record_is_ours(identity) {
            (*PUBLISHED.lock().unwrap()).clone_from(&self.saved.record);
            return Ok(());
        }
        let now = crate::call::now_secs();
        let sequence = now.max(self.saved.sequence + 1);
        let target = Target { push_key: self.key.public(), transport };
        let record = RouteRecord {
            identity_key: identity.public().identity_key,
            sequence,
            timestamp_secs: now,
            // Direct first, as the desktop publishes it: this phone takes
            // messages only while both ends are online, and keeps no
            // mailbox yet.
            routes: vec![Route { kind: "direct".into(), address: String::new() }, target.to_route()],
        }
        .sign(identity)
        .map_err(|e| format!("could not sign the push address: {e}"))?;
        self.saved = Saved { endpoint: reported.to_string(), sequence, record: hex(&record.encode()) };
        let json = serde_json::to_string_pretty(&self.saved).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::write(saved_path(&self.dir), json) {
            log::warn!("could not save the push address: {e}");
        }
        (*PUBLISHED.lock().unwrap()).clone_from(&self.saved.record);
        Ok(())
    }

    /// Opens a wake-up and decides what it asks for. `None` for anything
    /// sealed to another key, damaged, stale, already seen, from someone
    /// not in `is_contact`, or over that sender's rate limit: each of those
    /// is dropped without a sound, as a wake-up from a stranger must be.
    pub fn open(&mut self, sealed: &[u8], is_contact: impl Fn(&str) -> bool, now: u64) -> Option<Woken> {
        let (sender, wake) = push::open(&self.key, sealed).ok()?;
        let max_age = if wake.kind == WakeKind::Call { CALL_MAX_AGE } else { OTHER_MAX_AGE };
        self.seen.retain(|_, sent| now.abs_diff(*sent) <= OTHER_MAX_AGE);
        if !wake.is_fresh(now, max_age) || self.seen.contains_key(&wake.id) {
            return None;
        }
        let from = hex(&sender.identity_key);
        if !is_contact(&from) {
            return None;
        }
        let limit = if wake.kind == WakeKind::Call { &mut self.calls } else { &mut self.others };
        if !limit.allow(&sender.identity_key, Instant::now()) {
            return None;
        }
        self.seen.insert(wake.id, wake.sent_at);
        Some(match wake.kind {
            WakeKind::Call => Woken::Ring { from },
            WakeKind::File => Woken::File { from },
            // Mail, and any kind from a newer build: wake up and look.
            WakeKind::Message | WakeKind::Other(_) => Woken::Mail { from },
        })
    }
}

/// Bytes from hex, or `None` for anything that is not.
fn unhex(s: &str) -> Option<Vec<u8>> {
    antiphon_crypto::hex::decode(s)
}
