//! Settings > Account & backup on the phone: the recovery phrase, backup
//! files, and moving the account to or from another device.
//!
//! The phone's half of what `crates/desktop/src/account.rs` does on a
//! computer, over the same shared pieces (`account_backup` for the bundle,
//! `antiphon_crypto::account_seal` for the lock, `antiphon_net::account_move`
//! for the transfer), so a backup or move made on either opens on the
//! other. What differs is only the phone's: a backup is saved straight to
//! Downloads (there is no save dialog), and a backup file is chosen with
//! the system picker, whose answer arrives later as a platform event.
//!
//! The phone is one of the account's devices with a key of its own (see
//! `crate::devices`): a move approves the receiving device into the
//! account's list, and the list is shown here, with the mailboxes the
//! phone keeps in touch with the account's other devices through (see
//! `crate::my_devices`). Removing a device needs at least one of them.
//!
//! Slow work runs on its own thread and is collected by [`PhoneAccount::poll`].
//! A restore rewrites the account on disk, and the app then reloads
//! itself from that disk ([`Outcome::Reload`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use antiphon_crypto::account_seal;
use antiphon_crypto::{BackupKey, DeviceKeyPair, IdentityKeyPair, PublicIdentity};
use antiphon_net::account_move::{self, MoveLink, MoveSender, MoveStatus};
use antiphon_ui as stoop;
use iced_widget::qr_code;

use crate::account_backup::{self, Bundle, Platform};
use crate::devices;
use crate::platform;

// The interface checks the passphrase length before asking; the two
// numbers must agree, or it would offer what the seal then refuses.
const _: () = assert!(stoop::MIN_PASSPHRASE_CHARS == account_seal::MIN_PASSPHRASE_CHARS);

/// How this phone keeps in touch with the account's other devices, in
/// words, for `count` mailboxes.
fn sync_line(count: usize) -> String {
    match count {
        0 => "Add a mailbox below -- one your other devices use too -- so this phone can remove a device and share your notes.".into(),
        1 => "Your devices keep in touch through this mailbox, sealed so only each device can read what's meant for it.".into(),
        n => format!("Your devices keep in touch through these {n} mailboxes, sealed so only each device can read what's meant for it."),
    }
}

/// What the app should do after an account action or poll.
pub enum Outcome {
    /// Show a toast.
    Toast(String, stoop::ToastKind),
    /// The account on disk changed: reload everything from it.
    Reload(String),
}

/// A slow job's result, handed back from its thread.
enum Done {
    /// A backup file was written here, or why not.
    Saved(Result<PathBuf, String>),
    /// A backup or move became this phone's account, or why not.
    Restored(Result<account_backup::Applied, String>),
    /// A move offer opened, or why not.
    Offer(Result<MoveSender, String>),
    /// A move approved a new device into the account's list: the list as
    /// it is now.
    Devices(Vec<stoop::DeviceRow>),
}

/// An open move offer and its QR code.
struct Offer {
    /// The listening side.
    sender: MoveSender,
    /// The link as a QR code, made once.
    qr: Option<Arc<qr_code::Data>>,
}

/// The account section's own state.
#[derive(Default)]
pub struct PhoneAccount {
    /// What slow job is running, in words.
    busy: Option<String>,
    /// The open move offer.
    moving: Option<Offer>,
    /// The passphrase to open a backup with, while the system file picker
    /// is up for it. `Some` routes the next picked file here instead of to
    /// a call's file transfer.
    pending_open: Option<String>,
    /// Results from job threads, collected on the next poll.
    results: Arc<Mutex<Vec<Done>>>,
    /// This phone's own keypair, once [`PhoneAccount::load_devices`] has run.
    device: Option<DeviceKeyPair>,
    /// The account's devices as last read, this phone first.
    devices: Vec<stoop::DeviceRow>,
}

impl PhoneAccount {
    /// Loads (or makes) this phone's own key, makes sure the phone is on
    /// the account's device list, and reads the list for Settings. Called
    /// at start and whenever the identity changes.
    pub fn load_devices(&mut self, dir: &Path, identity: &IdentityKeyPair) -> Vec<Outcome> {
        let device = match devices::load_or_create_device(dir) {
            Ok(d) => d,
            Err(e) => return vec![Outcome::Toast(format!("Couldn't load this phone's device key: {e}"), stoop::ToastKind::Err)],
        };
        match devices::ensure(dir, identity, &device, "Phone") {
            Ok(list) => self.devices = rows(&list, &device.public().identity_key),
            Err(e) => return vec![Outcome::Toast(format!("Couldn't update your device list: {e}"), stoop::ToastKind::Err)],
        }
        self.device = Some(device);
        Vec::new()
    }

    /// Seals the account under `passphrase` and saves it to Downloads (or
    /// the app's own storage, when the phone gives no Downloads folder).
    pub fn save(&mut self, dir: &Path, identity: &IdentityKeyPair, key: &BackupKey, passphrase: String, history: bool) {
        let folder = platform::downloads_dir().unwrap_or_else(|| dir.to_path_buf());
        let day = day_of(crate::call::now_secs());
        let path = folder.join(format!("stoop-account-{day}.stoopbackup"));
        let (dir, identity, key) = (dir.to_path_buf(), IdentityKeyPair::from_master_seed(identity.master_seed()), key.clone());
        self.run("Locking your backup with the passphrase…", move || {
            Done::Saved(
                account_backup::gather(&dir, Platform::Phone, &identity, &key, history)
                    .map_err(|e| format!("Couldn't read your account: {e}"))
                    .and_then(|bundle| account_seal::seal_with_passphrase(&bundle.encode(), &passphrase).map_err(|e| e.to_string()))
                    .and_then(|sealed| std::fs::write(&path, sealed).map_err(|e| format!("Couldn't write the backup: {e}")))
                    .map(|_| path),
            )
        });
    }

    /// Opens the system picker for a backup file, remembering the
    /// passphrase it will be opened with.
    pub fn choose_file(&mut self, passphrase: String) {
        self.pending_open = Some(passphrase);
        platform::pick_file();
    }

    /// The picker answered. Returns false when this pick was not for a
    /// backup, so the caller hands it to the file transfer as before.
    pub fn picked(&mut self, dir: &Path, handle: Option<&str>) -> bool {
        let Some(passphrase) = self.pending_open.take() else { return false };
        let Some(handle) = handle.map(str::to_string) else { return true };
        let dir = dir.to_path_buf();
        self.run("Opening your backup…", move || {
            Done::Restored(
                platform::read_file(&handle)
                    .map_err(|e| format!("Couldn't read that file: {e}"))
                    .and_then(|sealed| account_seal::open_with_passphrase(&sealed, &passphrase).map_err(|e| format!("{}.", capitalise(&e.to_string()))))
                    .and_then(|plain| apply(&dir, &plain)),
            )
        });
        true
    }

    /// Opens an offer to send this account to another device.
    pub fn start_move(&mut self, dir: &Path, identity: &IdentityKeyPair, key: &BackupKey, history: bool) -> Vec<Outcome> {
        let Some(ip) = account_move::lan_ip() else {
            return vec![Outcome::Toast("This phone isn't on a network. Join the same Wi-Fi as your other device.".into(), stoop::ToastKind::Err)];
        };
        let (dir, identity, key) = (dir.to_path_buf(), IdentityKeyPair::from_master_seed(identity.master_seed()), key.clone());
        let device = self.device.as_ref().map(|d| DeviceKeyPair::from_master_seed(d.master_seed()));
        let results = self.results.clone();
        // Gathered once the new device has said who it is, so the device
        // list it carries already names it.
        let make = move |new: Option<&PublicIdentity>| -> Result<Vec<u8>, String> {
            if let (Some(new), Some(device)) = (new, &device) {
                let list = devices::approve(&dir, &identity, device, new, "New device")?;
                results.lock().unwrap().push(Done::Devices(rows(&list, &device.public().identity_key)));
            }
            let bundle = account_backup::gather(&dir, Platform::Phone, &identity, &key, history).map_err(|e| format!("Couldn't read your account: {e}"))?;
            Ok(bundle.encode().to_vec())
        };
        self.run("Getting your account ready to send…", move || {
            Done::Offer(MoveSender::start_approving(Box::new(make), ip).map_err(|e| format!("Couldn't open the move: {e}")))
        });
        Vec::new()
    }

    /// Closes the offer.
    pub fn stop_move(&mut self) {
        self.moving = None;
    }

    /// Fetches the account a move link offers and makes it this phone's.
    pub fn receive(&mut self, dir: &Path, text: &str) -> Vec<Outcome> {
        let Some(link) = MoveLink::parse(text) else {
            return vec![Outcome::Toast("That isn't a move link. It starts with stoop-move:".into(), stoop::ToastKind::Err)];
        };
        let dir = dir.to_path_buf();
        // The old device approves this phone's own key on the way.
        let device = self.device.as_ref().map(|d| d.public());
        self.run("Receiving your account…", move || Done::Restored(account_move::receive_as(&link, device.as_ref()).and_then(|plain| apply(&dir, &plain))));
        Vec::new()
    }

    /// Collects finished jobs and notices an offer that expired.
    pub fn poll(&mut self) -> Vec<Outcome> {
        use stoop::ToastKind as K;
        let mut out = Vec::new();
        let done = std::mem::take(&mut *self.results.lock().unwrap());
        for d in done {
            // A device approved mid-move is news from a job still running.
            if !matches!(d, Done::Devices(_)) {
                self.busy = None;
            }
            match d {
                Done::Saved(Ok(path)) => out.push(Outcome::Toast(format!("Backup saved to {}", path.display()), K::Ok)),
                Done::Saved(Err(e)) | Done::Restored(Err(e)) | Done::Offer(Err(e)) => out.push(Outcome::Toast(e, K::Err)),
                Done::Restored(Ok(applied)) => {
                    let who = applied.display_name.map(|n| format!(" as {n}")).unwrap_or_default();
                    out.push(Outcome::Reload(format!("Account restored{who}, {} contacts.", applied.contacts)));
                }
                Done::Offer(Ok(sender)) => {
                    let qr = qr_code::Data::new(sender.link().to_link().as_bytes()).ok().map(Arc::new);
                    self.moving = Some(Offer { sender, qr });
                }
                Done::Devices(rows) => self.devices = rows,
            }
        }
        if self.moving.as_ref().is_some_and(|o| o.sender.status() == MoveStatus::Expired) {
            self.moving = None;
            out.push(Outcome::Toast("The move link expired. Open it again when the other device is ready.".into(), K::Info));
        }
        out
    }

    /// Whether a job is out or an offer is open, so the app keeps polling.
    pub fn active(&self) -> bool {
        self.busy.is_some() || self.moving.is_some()
    }

    /// The section as the interface shows it. `phrase` is the recovery
    /// phrase while it is shown; `mailboxes` are the ones in Settings >
    /// Devices.
    pub fn model(&self, phrase: Option<String>, mailboxes: &[String]) -> stoop::Account {
        stoop::Account {
            phrase,
            busy: self.busy.clone(),
            moving: self.moving.as_ref().map(|o| {
                let status = o.sender.status();
                stoop::Moving {
                    link: o.sender.link().to_link(),
                    qr: o.qr.clone(),
                    done: status == MoveStatus::Done,
                    status: match status {
                        MoveStatus::Waiting | MoveStatus::Stopped | MoveStatus::Expired => "Waiting for your other device…".into(),
                        MoveStatus::Sending => "Sending…".into(),
                        MoveStatus::Done => "Your account is on the other device now.".into(),
                        MoveStatus::Failed(e) => format!("That didn't work: {e}"),
                    },
                }
            }),
            can_pick_file: false,
            devices: self.devices.clone(),
            device_sync: sync_line(mailboxes.len()),
            can_remove_devices: !mailboxes.is_empty(),
            mailboxes: Some(mailboxes.to_vec()),
        }
    }

    /// Runs `job` on its own thread, showing `what` until it reports back.
    fn run(&mut self, what: &str, job: impl FnOnce() -> Done + Send + 'static) {
        self.busy = Some(what.to_string());
        let results = self.results.clone();
        std::thread::spawn(move || {
            let done = job();
            results.lock().unwrap().push(done);
        });
    }
}

/// The account's devices as Settings lists them, this phone (`me`) first.
fn rows(list: &antiphon_crypto::SignedDeviceList, me: &[u8; 32]) -> Vec<stoop::DeviceRow> {
    let mut rows: Vec<stoop::DeviceRow> = list
        .devices()
        .iter()
        .map(|d| stoop::DeviceRow {
            id: antiphon_crypto::hex::encode(&d.device.identity_key),
            name: d.name.clone(),
            added: format!("Added {}", day_of(d.added_at_secs)),
            this: &d.device.identity_key == me,
            fingerprint: antiphon_crypto::fingerprint(&d.device.identity_key),
        })
        .collect();
    rows.sort_by_key(|r| !r.this);
    rows
}

/// Decodes an opened bundle and writes it into the app's storage.
fn apply(dir: &Path, plain: &[u8]) -> Result<account_backup::Applied, String> {
    let bundle = Bundle::decode(plain)?;
    account_backup::apply(dir, Platform::Phone, &bundle).map_err(|e| format!("Couldn't write the account: {e}"))
}

/// The first letter in upper case, for an error message used as a sentence.
fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The date `secs` (unix seconds) falls on, as `YYYY-MM-DD` (UTC): for the
/// backup's file name and the device list. Worked out by hand: the phone
/// crate has no date library, and neither use needs one.
fn day_of(secs: u64) -> String {
    // Days since 1970-01-01, then Howard Hinnant's civil-from-days.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
