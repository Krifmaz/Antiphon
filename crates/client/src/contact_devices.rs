//! Contacts' devices: which of a contact's devices may speak for them, and
//! what this device last told each contact about its own.
//!
//! Every device of an account holds the account's master seed, so a
//! signature by the account says nothing about *which* device made it --
//! and a device the account has removed can still make one. So what a
//! device sends contacts also carries a [`DeviceProof`], its own signature,
//! and a contact believes it only from a device on the list the account
//! showed them ([`antiphon_crypto::device`]):
//!
//! - **Mail** carries a proof over the recipient and the body
//!   ([`mail_message`]); **calls** carry one in the Hello ([`hello_message`]);
//!   **route records** carry one as a route of their own ([`prove_record`],
//!   [`PROOF_ROUTE_KIND`]), which also covers the mail-key certificate.
//! - **Device lists** reach contacts with the devices' endorsements, on
//!   calls and by mailbox. A newer list is only taken when a device the
//!   contact already knows endorses it ([`antiphon_crypto::judge`]); one
//!   nobody they know endorses waits for the user to say it's fine
//!   ([`ContactDevices::trust_pending`]) -- that is what restoring from the
//!   recovery phrase with every old device gone looks like, and also what
//!   a removed device trying to sign itself back in looks like.
//! - **Mail keys** only go forward: once a contact's record names mail key
//!   *n*, a record with an older one (or none) is refused, so a removed
//!   device can't steer mail back to a key it holds.
//!
//! A contact whose list this device has never seen (an older build, or a
//! new contact before their first list arrives) is believed on the
//! account's signature alone, as before; from the first list on, proofs
//! are required.
//!
//! `contacts.devices` is sealed under the history key and travels with the
//! account in a backup or move. Pure file handling over `antiphon_crypto`,
//! so the phone shares it by path.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use antiphon_crypto::mail_key::{self, MailKeyCert};
use antiphon_crypto::{Route, RouteRecord, SignedRouteRecord};
use antiphon_crypto::{hex, judge, BackupKey, CarriedList, DeviceKeyPair, DeviceProof, Judged, SignedDeviceList};
use serde::{Deserialize, Serialize};

/// The file the lists are kept in.
pub const FILE: &str = "contacts.devices";
/// The route kind a route record's device proof travels under.
pub const PROOF_ROUTE_KIND: &str = "device-proof";
/// What a proof on mail is made for.
const MAIL: &[u8] = b"mail";
/// What a proof in a call's Hello is made for.
const HELLO: &[u8] = b"hello";
/// What a proof on a route record is made for.
const ROUTE: &[u8] = b"route";

/// What is kept about one contact.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
struct Entry {
    /// Their device list as accepted, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    list: Option<String>,
    /// A newer list no device they had endorsed, hex: kept for the user to
    /// accept or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<String>,
    /// The newest mail key their records have named.
    #[serde(default)]
    mail_epoch: u64,
    /// Our route record's sequence when it was last pushed to them.
    #[serde(default)]
    sent_record: u64,
    /// Our device list's sequence when it was last pushed to them.
    #[serde(default)]
    sent_list: u64,
}

/// Whether to believe something a contact's device signed.
#[derive(Clone, Debug, PartialEq)]
pub enum Trust {
    /// No list held for them yet: believed on the account's signature, as
    /// before device lists.
    Unknown,
    /// Signed by this listed device (its name).
    Device(String),
    /// Unsigned, or signed by a device that isn't on their list (removed,
    /// or never added): refuse it.
    Refused,
}

/// What became of a list a contact offered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Offer {
    /// Nothing new.
    Unchanged,
    /// Taken: it is their list now.
    Updated,
    /// Newer, but endorsed by no device of theirs this device knew: kept
    /// aside until the user accepts it.
    Unendorsed,
    /// Not theirs, or damaged.
    Invalid,
}

/// Every contact's devices, as this device knows them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContactDevices {
    /// By the contact's identity key, hex.
    entries: BTreeMap<String, Entry>,
}

impl ContactDevices {
    /// What is saved in `dir`, or nothing (when there is none, or it
    /// can't be opened: then every contact is [`Trust::Unknown`] again).
    pub fn load(dir: &Path, key: &BackupKey) -> Self {
        let entries = std::fs::read(dir.join(FILE))
            .ok()
            .and_then(|sealed| key.open_content(&sealed).ok())
            .and_then(|plain| serde_json::from_slice(&plain).ok())
            .unwrap_or_default();
        Self { entries }
    }

    /// Writes it, sealed, through a temporary file.
    pub fn save(&self, dir: &Path, key: &BackupKey) -> std::io::Result<()> {
        let plain = serde_json::to_vec(&self.entries).expect("contact devices always serialize");
        let sealed = key.seal_content(&plain).map_err(|e| std::io::Error::other(e.to_string()))?;
        let path = dir.join(FILE);
        let tmp = path.with_extension("devices.tmp");
        std::fs::write(&tmp, sealed)?;
        std::fs::rename(&tmp, &path)
    }

    /// Their accepted list.
    fn list(&self, who: &[u8; 32]) -> Option<SignedDeviceList> {
        let bytes = hex::decode(self.entries.get(&hex::encode(who))?.list.as_deref()?)?;
        SignedDeviceList::decode(&bytes).ok()
    }

    /// Takes a list `who` sent, if it is theirs and a device they had
    /// endorses it (see the module docs).
    pub fn offer(&mut self, who: &[u8; 32], carried: &CarriedList) -> Offer {
        if carried.list.list.identity_key != *who {
            return Offer::Invalid;
        }
        let known = self.list(who);
        let entry = self.entries.entry(hex::encode(who)).or_default();
        match judge(known.as_ref(), carried) {
            Err(_) => Offer::Invalid,
            Ok(Judged::Unchanged) => Offer::Unchanged,
            Ok(Judged::Accepted) => {
                entry.list = Some(hex::encode(&carried.list.encode()));
                // A pending list this one supersedes is moot.
                entry.pending = None;
                Offer::Updated
            }
            Ok(Judged::Unendorsed) => {
                let encoded = hex::encode(&carried.encode());
                if entry.pending.as_deref() == Some(encoded.as_str()) {
                    Offer::Unchanged
                } else {
                    entry.pending = Some(encoded);
                    Offer::Unendorsed
                }
            }
        }
    }

    /// Whether `who` offered a list none of their known devices endorses.
    pub fn has_pending(&self, who: &[u8; 32]) -> bool {
        self.entries.get(&hex::encode(who)).is_some_and(|e| e.pending.is_some())
    }

    /// The user checked with `who` and trusts their new devices: their
    /// pending list becomes their list. Returns whether there was one.
    pub fn trust_pending(&mut self, who: &[u8; 32]) -> bool {
        let Some(entry) = self.entries.get_mut(&hex::encode(who)) else { return false };
        let Some(carried) = entry.pending.take().and_then(|h| hex::decode(&h)).and_then(|b| CarriedList::decode(&b).ok()) else { return false };
        entry.list = Some(hex::encode(&carried.list.encode()));
        true
    }

    /// Whether to believe what `proof` signed for `who`: `message` made
    /// for `purpose`. See [`Trust`].
    fn check(&self, who: &[u8; 32], proof: Option<&DeviceProof>, purpose: &[u8], message: &[u8]) -> Trust {
        let Some(list) = self.list(who) else { return Trust::Unknown };
        match proof.map(|p| list.check(p, purpose, message)) {
            Some(Ok(credential)) => Trust::Device(credential.name.clone()),
            _ => Trust::Refused,
        }
    }

    /// Whether to believe mail from `who` to `me`: `body` is the mail's
    /// body as signed (without its proof).
    pub fn check_mail(&self, who: &[u8; 32], me: &[u8; 32], proof: Option<&DeviceProof>, body: &[u8]) -> Trust {
        self.check(who, proof, MAIL, &mail_message(me, body))
    }

    /// Whether to believe the call from `who` to `me` is from one of their
    /// listed devices.
    pub fn check_hello(&self, who: &[u8; 32], me: &[u8; 32], proof: Option<&DeviceProof>) -> Trust {
        self.check(who, proof, HELLO, &hello_message(who, me))
    }

    /// Whether to take `record` as its publisher's: proved by one of their
    /// listed devices (once a list is held), and naming no older mail key
    /// than they already moved to. Taking it remembers its mail key.
    pub fn check_record(&mut self, record: &SignedRouteRecord) -> bool {
        let who = record.record.identity_key;
        if self.check(&who, record_proof(&record.record).as_ref(), ROUTE, &record_message(&record.record)) == Trust::Refused {
            return false;
        }
        let epoch = mail_cert(record).map_or(0, |c| c.epoch);
        let entry = self.entries.entry(hex::encode(&who)).or_default();
        if epoch < entry.mail_epoch {
            return false;
        }
        entry.mail_epoch = epoch;
        true
    }

    /// Whether `who` should be sent this account's record and device list
    /// again: either changed since they were last pushed.
    pub fn needs_update(&self, who: &[u8; 32], record: u64, list: u64) -> bool {
        self.entries.get(&hex::encode(who)).is_none_or(|e| e.sent_record < record || e.sent_list < list)
    }

    /// Notes that `who` was pushed this record and device list.
    pub fn mark_sent(&mut self, who: &[u8; 32], record: u64, list: u64) {
        let entry = self.entries.entry(hex::encode(who)).or_default();
        entry.sent_record = entry.sent_record.max(record);
        entry.sent_list = entry.sent_list.max(list);
    }
}

/// How long a call's Hello from a device not on the list held waits for
/// the call's device list, which may name it, before the call is refused.
pub const CALL_GRACE: Duration = Duration::from_secs(4);

/// Checks the person on a call is calling from one of their devices: the
/// device list they sent with the call is offered first, then their
/// Hello's proof is checked against what is held. Fed whatever has arrived
/// on every tick of the call; only reads the file when something changed
/// or a verdict is due.
#[derive(Debug, Default)]
pub struct CallCheck {
    /// The device list and proof last looked at.
    seen: Option<(Option<String>, String)>,
    /// When the Hello was first found wanting, if it still is.
    refused_since: Option<Instant>,
    /// A verdict was reached: nothing more to do this call.
    settled: bool,
}

/// What has arrived on a call so far, for [`CallCheck::step`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Heard<'a> {
    /// The device proof in their Hello (hex, empty when it had none), once
    /// a Hello has arrived.
    pub proof: Option<&'a str>,
    /// Their `Devices` message's list (hex), once it has arrived.
    pub list: Option<&'a str>,
}

/// What a [`CallCheck`] step found.
#[derive(Debug, PartialEq)]
pub enum CallEvent {
    /// Their new device list isn't endorsed by a device of theirs this one
    /// knew: tell the user (see [`ContactDevices::trust_pending`]).
    Unendorsed,
    /// The call is from a device they removed, or never added: hang up.
    Refuse,
}

impl CallCheck {
    /// One look at the call from `who` to `me`, given what has been
    /// `heard` on it. `dir` and `key` open `contacts.devices`.
    pub fn step(&mut self, dir: &Path, key: &BackupKey, who: &[u8; 32], me: &[u8; 32], heard: Heard<'_>, now: Instant) -> Vec<CallEvent> {
        let Heard { proof: Some(proof), list } = heard else { return Vec::new() };
        let inputs = (list.map(str::to_string), proof.to_string());
        let changed = self.seen.as_ref() != Some(&inputs);
        let due = self.refused_since.is_some_and(|t| now.duration_since(t) >= CALL_GRACE);
        if !changed && (self.settled || !due) {
            return Vec::new();
        }
        self.seen = Some(inputs);
        let mut known = ContactDevices::load(dir, key);
        let mut out = Vec::new();
        if let Some(carried) = list.and_then(hex::decode).and_then(|b| CarriedList::decode(&b).ok()) {
            if known.offer(who, &carried) == Offer::Unendorsed {
                out.push(CallEvent::Unendorsed);
            }
            if let Err(e) = known.save(dir, key) {
                log_save_error(&e);
            }
        }
        let parsed = hex::decode(proof).and_then(|b| DeviceProof::decode(&b).ok());
        if known.check_hello(who, me, parsed.as_ref()) == Trust::Refused {
            let since = *self.refused_since.get_or_insert(now);
            // Their list may still be on its way; once it is here, or the
            // grace is over, the call goes.
            if list.is_some() || now.duration_since(since) >= CALL_GRACE {
                self.settled = true;
                out.push(CallEvent::Refuse);
            }
        } else {
            self.refused_since = None;
            self.settled = true;
        }
        out
    }
}

/// Reports a failed save of `contacts.devices`; the next call or update
/// offers the list again.
fn log_save_error(e: &std::io::Error) {
    eprintln!("[devices] couldn't save a contact's devices: {e}");
}

/// What a proof on mail covers: the recipient, so it can't be passed on to
/// someone else, and the body.
pub fn mail_message(recipient: &[u8; 32], body: &[u8]) -> Vec<u8> {
    [recipient.as_slice(), body].concat()
}

/// What a proof in a call's Hello covers: who is calling whom, so a Hello
/// one contact received can't be shown to another.
pub fn hello_message(from: &[u8; 32], to: &[u8; 32]) -> Vec<u8> {
    [from.as_slice(), to.as_slice()].concat()
}

/// This device's proof for its Hello to `to`, as an account `from`.
pub fn prove_hello(device: &DeviceKeyPair, from: &[u8; 32], to: &[u8; 32]) -> DeviceProof {
    DeviceProof::sign(device, HELLO, &hello_message(from, to))
}

/// This device's proof for mail to `recipient` with this body.
pub fn prove_mail(device: &DeviceKeyPair, recipient: &[u8; 32], body: &[u8]) -> DeviceProof {
    DeviceProof::sign(device, MAIL, &mail_message(recipient, body))
}

/// Adds this device's proof to a record about to be signed, replacing any
/// it had. Takes a route's place, so the record must leave one free.
pub fn prove_record(device: &DeviceKeyPair, record: &mut RouteRecord) {
    record.routes.retain(|r| r.kind != PROOF_ROUTE_KIND);
    let proof = DeviceProof::sign(device, ROUTE, &record_message(record));
    record.routes.push(Route { kind: PROOF_ROUTE_KIND.to_string(), address: hex::encode(&proof.encode()) });
}

/// The proof a record carries, if any.
fn record_proof(record: &RouteRecord) -> Option<DeviceProof> {
    let route = record.routes.iter().find(|r| r.kind == PROOF_ROUTE_KIND)?;
    DeviceProof::decode(&hex::decode(&route.address)?).ok()
}

/// What a record's proof covers: everything in it but the proof itself,
/// length-prefixed so routes can't be shifted between fields.
fn record_message(record: &RouteRecord) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&record.identity_key);
    out.extend_from_slice(&record.sequence.to_be_bytes());
    out.extend_from_slice(&record.timestamp_secs.to_be_bytes());
    for route in record.routes.iter().filter(|r| r.kind != PROOF_ROUTE_KIND) {
        for field in [&route.kind, &route.address] {
            out.extend_from_slice(&(field.len() as u32).to_be_bytes());
            out.extend_from_slice(field.as_bytes());
        }
    }
    out
}

/// The newest mail key a record names that its identity signed.
pub fn mail_cert(record: &SignedRouteRecord) -> Option<MailKeyCert> {
    record
        .record
        .routes
        .iter()
        .filter(|r| r.kind == mail_key::ROUTE_KIND)
        .filter_map(|r| MailKeyCert::from_address(&r.address))
        .filter(|c| c.verify(&record.record.identity_key).is_ok())
        .max_by_key(|c| c.epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use antiphon_crypto::{DeviceCredential, IdentityKeyPair};

    /// An account with two devices, its list endorsed by both.
    struct Account {
        /// The account.
        master: IdentityKeyPair,
        /// Its first device.
        desktop: DeviceKeyPair,
        /// Its second device.
        laptop: DeviceKeyPair,
        /// Its list.
        list: SignedDeviceList,
    }

    fn account() -> Account {
        let master = IdentityKeyPair::generate();
        let (desktop, laptop) = (DeviceKeyPair::generate(), DeviceKeyPair::generate());
        let list = antiphon_crypto::DeviceList {
            identity_key: master.public().identity_key,
            sequence: 1,
            devices: vec![
                DeviceCredential::approve(&master, desktop.public(), "Desktop", 1).unwrap(),
                DeviceCredential::approve(&master, laptop.public(), "Laptop", 1).unwrap(),
            ],
        }
        .sign(&master)
        .unwrap();
        Account { master, desktop, laptop, list }
    }

    /// `list` with endorsements by `by`.
    fn carried(list: &SignedDeviceList, by: &[&DeviceKeyPair]) -> CarriedList {
        CarriedList { list: list.clone(), endorsements: by.iter().map(|d| list.endorse(d)).collect() }
    }

    /// A record for `master`, proved by `device` if given.
    fn record(master: &IdentityKeyPair, sequence: u64, device: Option<&DeviceKeyPair>, cert: Option<MailKeyCert>) -> SignedRouteRecord {
        let mut r = RouteRecord { identity_key: master.public().identity_key, sequence, timestamp_secs: 5, routes: vec![Route { kind: "trusted-node".into(), address: "n:1#00".into() }] };
        if let Some(c) = cert {
            r.routes.push(Route { kind: mail_key::ROUTE_KIND.into(), address: c.to_address() });
        }
        if let Some(d) = device {
            prove_record(d, &mut r);
        }
        r.sign(master).unwrap()
    }

    #[test]
    fn a_removed_device_is_refused_once_the_list_without_it_arrives() {
        let a = account();
        let who = a.master.public().identity_key;
        let me = IdentityKeyPair::generate().public().identity_key;
        let mut known = ContactDevices::default();
        let from_laptop = prove_mail(&a.laptop, &me, b"hi");
        // Before any list: the account's signature is all there is.
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"hi"), Trust::Unknown);

        assert_eq!(known.offer(&who, &carried(&a.list, &[&a.desktop, &a.laptop])), Offer::Updated);
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"hi"), Trust::Device("Laptop".into()));
        assert_eq!(known.check_mail(&who, &me, None, b"hi"), Trust::Refused, "unsigned mail is refused once devices are known");
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"changed"), Trust::Refused);

        // The desktop removes the laptop.
        let without = a.list.without_device(&a.master, &a.laptop.public().identity_key).unwrap();
        assert_eq!(known.offer(&who, &carried(&without, &[&a.desktop])), Offer::Updated);
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"hi"), Trust::Refused);
        assert_eq!(known.check_hello(&who, &me, Some(&prove_hello(&a.laptop, &who, &me))), Trust::Refused);
        assert_eq!(known.check_hello(&who, &me, Some(&prove_hello(&a.desktop, &who, &me))), Trust::Device("Desktop".into()));

        // The laptop, holding the master seed, signs itself back in.
        let forged = without.with_device(&a.master, DeviceCredential::approve(&a.master, a.laptop.public(), "Laptop", 2).unwrap()).unwrap();
        assert_eq!(known.offer(&who, &carried(&forged, &[&a.laptop])), Offer::Unendorsed);
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"hi"), Trust::Refused, "still refused");
        assert!(known.has_pending(&who));
        // Only the user can let that list in.
        assert!(known.trust_pending(&who));
        assert_eq!(known.check_mail(&who, &me, Some(&from_laptop), b"hi"), Trust::Device("Laptop".into()));
    }

    #[test]
    fn records_need_a_listed_device_and_never_go_back_a_mail_key() {
        let a = account();
        let who = a.master.public().identity_key;
        let mut known = ContactDevices::default();
        let key_one = MailKeyCert::issue(&a.master, 1, &mail_key::holder([1; 32]));
        let key_two = MailKeyCert::issue(&a.master, 2, &mail_key::holder([2; 32]));
        // Unknown devices: taken on the account's signature.
        assert!(known.check_record(&record(&a.master, 1, None, Some(key_one))));

        known.offer(&who, &carried(&a.list, &[&a.desktop]));
        assert!(!known.check_record(&record(&a.master, 2, None, Some(key_two))), "no proof");
        assert!(known.check_record(&record(&a.master, 2, Some(&a.desktop), Some(key_two))));
        // The laptop, removed and holding the old key, publishes it again.
        let without = a.list.without_device(&a.master, &a.laptop.public().identity_key).unwrap();
        known.offer(&who, &carried(&without, &[&a.desktop]));
        assert!(!known.check_record(&record(&a.master, 3, Some(&a.laptop), Some(key_one))), "removed device");
        // Even a listed device can't take the mail key back.
        assert!(!known.check_record(&record(&a.master, 3, Some(&a.desktop), Some(key_one))));
        assert!(!known.check_record(&record(&a.master, 3, Some(&a.desktop), None)));
        // Tampering after the proof breaks it.
        let mut tampered = record(&a.master, 4, Some(&a.desktop), Some(key_two));
        tampered.record.routes[0].address = "evil:1#00".into();
        let tampered = tampered.record.sign(&a.master).unwrap();
        assert!(!known.check_record(&tampered));
    }

    #[test]
    fn pushes_are_remembered_and_the_file_is_sealed() {
        let dir = std::env::temp_dir().join(format!("stoop-contact-devices-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = BackupKey::generate();
        let a = account();
        let who = a.master.public().identity_key;
        let mut known = ContactDevices::default();
        assert!(known.needs_update(&who, 1, 1));
        known.mark_sent(&who, 3, 1);
        assert!(!known.needs_update(&who, 3, 1));
        assert!(known.needs_update(&who, 3, 2));
        known.offer(&who, &carried(&a.list, &[&a.desktop]));
        known.save(&dir, &key).unwrap();
        assert_eq!(ContactDevices::load(&dir, &key), known);
        assert_eq!(ContactDevices::load(&dir, &BackupKey::generate()), ContactDevices::default());
    }
}
