//! Settings > Account & backup on the desktop: the recovery phrase, backup
//! files, and moving the account to or from another device.
//!
//! Kept apart from the call engine because none of it touches a call: it
//! reads and writes the config folder, and the slow parts (Argon2id, a
//! network transfer, a large history) run on threads of their own and
//! report back through [`AccountState::tick`]. The engine owns one
//! [`AccountState`] and hands it the account actions.
//!
//! Every restore ends the same way: the files on disk are the new
//! account, and the engine in memory is still the old one, so the app
//! restarts itself ([`Outcome::Relaunch`]) rather than trying to swap an
//! identity out from under running sockets.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use antiphon_client::account_backup::{self, Bundle, Platform};
use antiphon_client::{device_sync, devices};
use antiphon_crypto::account_seal;
use antiphon_crypto::{BackupKey, DeviceKeyPair, IdentityKeyPair};
use antiphon_net::account_move::{self, MoveLink, MoveSender, MoveStatus};
use antiphon_ui::{self as ui, ToastKind};
use iced::widget::qr_code;

// The interface checks the passphrase length before asking; the two
// numbers must agree, or it would offer what the seal then refuses.
const _: () = assert!(ui::MIN_PASSPHRASE_CHARS == account_seal::MIN_PASSPHRASE_CHARS);

/// What the engine should do after an account action or tick.
pub enum Outcome {
    /// Show a toast.
    Toast(String, ToastKind),
    /// The account on disk changed: restart the app to load it.
    Relaunch,
    /// A move approved a new device into the account's list: read it again.
    DevicesChanged,
}

/// A slow job's result, handed back from its thread.
enum Done {
    /// A backup file was written here, or why not.
    Saved(Result<PathBuf, String>),
    /// A backup or move was made this device's account, or why not.
    Restored(Result<account_backup::Applied, String>),
    /// A move offer opened, or why not.
    Offer(Result<MoveSender, String>),
    /// A move approved a new device into the account.
    DevicesChanged,
}

/// An open move offer and its QR code.
struct Offer {
    /// The listening side.
    sender: MoveSender,
    /// The link as a QR code, made once.
    qr: Option<Arc<qr_code::Data>>,
}

/// What the engine needs to lend an account action.
pub struct Ctx<'a> {
    /// The config folder; `None` when the app has nowhere to save.
    pub dir: Option<&'a Path>,
    /// This device's identity now.
    pub identity: &'a IdentityKeyPair,
    /// The key history is sealed under, if it loaded.
    pub key: Option<&'a BackupKey>,
    /// This device's own keypair, if it loaded: what a move approves the
    /// new device with, and what this device asks to be approved as.
    pub device: Option<DeviceKeyPair>,
    /// A call is up: restoring now would pull the identity out from under it.
    pub in_call: bool,
}

/// The account section's own state.
#[derive(Default)]
pub struct AccountState {
    /// The recovery phrase is on screen.
    phrase_shown: bool,
    /// What slow job is running, in words.
    busy: Option<String>,
    /// The open move offer.
    moving: Option<Offer>,
    /// Results from job threads, collected on the next tick.
    results: Arc<Mutex<Vec<Done>>>,
}

impl AccountState {
    /// Carries out one account action. Anything else is ignored.
    pub fn act(&mut self, action: ui::Action, ctx: Ctx) -> Vec<Outcome> {
        use ui::Action as A;
        let toast = |text: &str, kind| vec![Outcome::Toast(text.to_string(), kind)];
        let Some(dir) = ctx.dir.map(Path::to_path_buf) else {
            return toast("There's no folder to keep an account in on this computer.", ToastKind::Err);
        };
        match action {
            A::ShowPhrase(on) => {
                self.phrase_shown = on;
                Vec::new()
            }
            A::Restore(words) => {
                if ctx.in_call {
                    return toast("Hang up before restoring an identity.", ToastKind::Warn);
                }
                let restored = match account_backup::identity_from_words(&words) {
                    Ok(id) => id,
                    Err(e) => return toast(&e, ToastKind::Err),
                };
                if restored.public().identity_key == ctx.identity.public().identity_key {
                    return toast("That's already this computer's identity. Nothing to do.", ToastKind::Info);
                }
                match account_backup::replace_identity(&dir, &restored) {
                    Ok(_) => vec![Outcome::Toast("Identity restored. Restarting…".into(), ToastKind::Ok), Outcome::Relaunch],
                    Err(e) => toast(&format!("Couldn't save the identity: {e}"), ToastKind::Err),
                }
            }
            A::BackupSave { passphrase, history } => {
                let Some(key) = ctx.key.cloned() else {
                    return toast("Your history key didn't load, so a backup can't be made yet.", ToastKind::Err);
                };
                let day = chrono::Local::now().format("%Y-%m-%d");
                let Some(path) = rfd::FileDialog::new()
                    .set_title("Save your account backup")
                    .set_file_name(format!("stoop-account-{day}.stoopbackup"))
                    .add_filter("Stoop backup", &["stoopbackup"])
                    .save_file()
                else {
                    return Vec::new();
                };
                let identity = IdentityKeyPair::from_master_seed(ctx.identity.master_seed());
                self.run("Locking your backup with the passphrase…", move || {
                    Done::Saved(save_backup(&dir, &identity, &key, history, &passphrase, &path).map(|_| path))
                });
                Vec::new()
            }
            A::BackupOpen { passphrase } => {
                if ctx.in_call {
                    return toast("Hang up before restoring an account.", ToastKind::Warn);
                }
                let Some(path) = rfd::FileDialog::new().set_title("Open an account backup").add_filter("Stoop backup", &["stoopbackup"]).pick_file() else {
                    return Vec::new();
                };
                self.run("Opening your backup…", move || Done::Restored(open_backup(&dir, &path, &passphrase)));
                Vec::new()
            }
            A::MoveStart { history } => {
                let Some(key) = ctx.key.cloned() else {
                    return toast("Your history key didn't load, so the account can't be sent yet.", ToastKind::Err);
                };
                let Some(ip) = account_move::lan_ip() else {
                    return toast("This computer isn't on a network. Join the same Wi-Fi as your other device.", ToastKind::Err);
                };
                let identity = IdentityKeyPair::from_master_seed(ctx.identity.master_seed());
                let device = ctx.device;
                let results = self.results.clone();
                // The account is gathered once the new device has said who
                // it is, so the list it carries already names that device.
                let make = move |new: Option<&antiphon_crypto::PublicIdentity>| -> Result<Vec<u8>, String> {
                    if let (Some(new), Some(device)) = (new, &device) {
                        let list = devices::approve(&dir, &identity, device, new, "New device")?;
                        // The account's other devices learn of it too.
                        if let Err(e) = device_sync::send_list(&dir, device, &list, None) {
                            eprintln!("[devices] couldn't tell your other devices about the new one: {e}");
                        }
                        results.lock().unwrap().push(Done::DevicesChanged);
                    }
                    let bundle = account_backup::gather(&dir, Platform::Desktop, &identity, &key, history).map_err(|e| format!("Couldn't read your account: {e}"))?;
                    Ok(bundle.encode().to_vec())
                };
                self.run("Getting your account ready to send…", move || {
                    Done::Offer(MoveSender::start_approving(Box::new(make), ip).map_err(|e| format!("Couldn't open the move: {e}")))
                });
                Vec::new()
            }
            A::MoveStop => {
                // Dropping the sender closes the offer.
                self.moving = None;
                Vec::new()
            }
            A::MoveReceive(text) => {
                if ctx.in_call {
                    return toast("Hang up before bringing an account here.", ToastKind::Warn);
                }
                let Some(link) = MoveLink::parse(&text) else {
                    return toast("That isn't a move link. It starts with stoop-move:", ToastKind::Err);
                };
                let device = ctx.device.map(|d| d.public());
                self.run("Receiving your account…", move || Done::Restored(receive(&dir, &link, device.as_ref())));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Collects finished jobs and notices a move that completed.
    pub fn tick(&mut self) -> Vec<Outcome> {
        let mut out = Vec::new();
        let done = std::mem::take(&mut *self.results.lock().unwrap());
        for d in done {
            // A device approved mid-move is news from a job still running.
            if !matches!(d, Done::DevicesChanged) {
                self.busy = None;
            }
            match d {
                Done::Saved(Ok(path)) => out.push(Outcome::Toast(format!("Backup saved to {}", path.display()), ToastKind::Ok)),
                Done::Saved(Err(e)) => out.push(Outcome::Toast(e, ToastKind::Err)),
                Done::Restored(Ok(applied)) => {
                    let who = applied.display_name.map(|n| format!(" as {n}")).unwrap_or_default();
                    out.push(Outcome::Toast(format!("Account restored{who}, {} contacts. Restarting…", applied.contacts), ToastKind::Ok));
                    out.push(Outcome::Relaunch);
                }
                Done::Restored(Err(e)) => out.push(Outcome::Toast(e, ToastKind::Err)),
                Done::Offer(Ok(sender)) => {
                    let qr = qr_code::Data::new(sender.link().to_link().as_bytes()).ok().map(Arc::new);
                    self.moving = Some(Offer { sender, qr });
                }
                Done::Offer(Err(e)) => out.push(Outcome::Toast(e, ToastKind::Err)),
                Done::DevicesChanged => out.push(Outcome::DevicesChanged),
            }
        }
        if let Some(offer) = &self.moving {
            if matches!(offer.sender.status(), MoveStatus::Expired) {
                self.moving = None;
                out.push(Outcome::Toast("The move link expired. Open it again when the other device is ready.".into(), ToastKind::Info));
            }
        }
        out
    }

    /// The section as the interface shows it.
    pub fn model(&self, identity: &IdentityKeyPair) -> ui::Account {
        ui::Account {
            phrase: self.phrase_shown.then(|| antiphon_crypto::mnemonic::phrase_from_seed(&identity.master_seed())),
            busy: self.busy.clone(),
            moving: self.moving.as_ref().map(|o| {
                let status = o.sender.status();
                ui::Moving {
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
            can_pick_file: true,
            can_export_data: true,
            // The engine fills these in from `my_devices`.
            devices: Vec::new(),
            device_sync: String::new(),
            can_remove_devices: true,
            // The desktop's mailboxes are its Settings > Delivery routes.
            mailboxes: None,
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

/// Gathers, seals and writes a backup file.
fn save_backup(dir: &Path, identity: &IdentityKeyPair, key: &BackupKey, history: bool, passphrase: &str, path: &Path) -> Result<(), String> {
    let bundle = account_backup::gather(dir, Platform::Desktop, identity, key, history).map_err(|e| format!("Couldn't read your account: {e}"))?;
    let sealed = account_seal::seal_with_passphrase(&bundle.encode(), passphrase).map_err(|e| e.to_string())?;
    std::fs::write(path, sealed).map_err(|e| format!("Couldn't write the backup: {e}"))
}

/// Opens a backup file and makes it this computer's account.
fn open_backup(dir: &Path, path: &Path, passphrase: &str) -> Result<account_backup::Applied, String> {
    let sealed = std::fs::read(path).map_err(|e| format!("Couldn't read that file: {e}"))?;
    let plain = account_seal::open_with_passphrase(&sealed, passphrase).map_err(|e| format!("{}.", capitalise(&e.to_string())))?;
    apply(dir, &plain)
}

/// Fetches the account a move link offers and makes it this computer's,
/// asking the old device to approve this one (`device`) on the way.
fn receive(dir: &Path, link: &MoveLink, device: Option<&antiphon_crypto::PublicIdentity>) -> Result<account_backup::Applied, String> {
    let plain = account_move::receive_as(link, device)?;
    apply(dir, &plain)
}

/// Decodes an opened bundle and writes it into the config folder.
fn apply(dir: &Path, plain: &[u8]) -> Result<account_backup::Applied, String> {
    let bundle = Bundle::decode(plain)?;
    account_backup::apply(dir, Platform::Desktop, &bundle).map_err(|e| format!("Couldn't write the account: {e}"))
}

/// The first letter in upper case, for an error message used as a sentence.
fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Starts a fresh copy of this program with the same arguments (so a
/// `--profile` stays the same profile). The caller then exits.
pub fn relaunch() {
    // Inside an AppImage the running file is in a mount that goes away on
    // exit; the AppImage itself is what to start again.
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).map(Ok).unwrap_or_else(std::env::current_exe);
    match exe {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).args(std::env::args_os().skip(1)).spawn() {
                eprintln!("[account] could not restart: {e}; start Stoop again to load the restored account");
            }
        }
        Err(e) => eprintln!("[account] could not find this program to restart it: {e}"),
    }
}
