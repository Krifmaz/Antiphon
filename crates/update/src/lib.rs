//! Stoop's in-app updater.
//!
//! Stoop's releases are GitHub releases of [`release::REPO`], made by
//! `.github/workflows/release.yml`: a release for each version, and a hotfix
//! for every push to main, whichever is newest marked Latest. This crate
//! looks at that Latest release, says whether it is newer than the running
//! copy, downloads the file meant for this install, checks it against the
//! release's `SHA256SUMS`, and puts it in place where the platform lets an
//! app do that:
//!
//! | Install                     | Update                                                     |
//! |-----------------------------|------------------------------------------------------------|
//! | Windows installer (`.exe`)  | the new installer, run silently, then the new version      |
//! | Windows `.msi`              | `msiexec` on the new MSI, then the new version             |
//! | Linux AppImage              | the new image replaces the old file, then is started       |
//! | Linux `.deb`                | the new package, opened in the system's software installer |
//! | Linux Flatpak or tarball    | the release page, to update by hand                        |
//! | macOS `Stoop.app`           | the new app swapped in from the `.dmg`, then started       |
//! | Android                     | the APK, handed to Android's installer to confirm          |
//! | iOS                         | nothing: the App Store updates it                          |
//!
//! The pieces:
//!
//! - [`version`]: Stoop's three- and four-part version numbers, ordered.
//! - [`target`]: how the running copy was installed, and which file of a
//!   release updates it.
//! - [`release`]: GitHub's description of a release, and checksum lists.
//! - [`fetch`]: the network calls, blocking.
//! - [`install`]: putting a desktop update in place.
//! - [`Updater`]: all of it as one state machine the apps poll from their
//!   own loops, with the network on threads of its own, and the one setting
//!   (look automatically) saved beside the app's other data.
//!
//! The interface itself is `antiphon-ui`'s: each app copies
//! [`Updater::status`] into the model it draws, and turns the buttons'
//! actions into calls here.

#![warn(missing_docs, clippy::missing_docs_in_private_items)]

pub mod fetch;
pub mod install;
pub mod release;
pub mod target;
pub mod version;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub use install::After;
pub use release::{Asset, Release};
pub use target::Target;
pub use version::Version;

/// How often to look again while the app stays open, when looking
/// automatically. Releases come at most a few times a day.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// The settings file, in the app's data folder.
const CONFIG_FILE: &str = "updates.json";

/// Where an update stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// Nothing has been looked for yet.
    #[default]
    Idle,
    /// Asking GitHub for the newest release.
    Checking,
    /// The running copy is the newest.
    UpToDate,
    /// A newer version is out.
    Available(Release),
    /// The newer version is downloading: bytes so far, and in all when
    /// known.
    Downloading {
        /// The release being downloaded.
        release: Release,
        /// Bytes so far.
        done: u64,
        /// Bytes in all, when the server says.
        total: Option<u64>,
    },
    /// Downloaded and checked; ready to install.
    Ready {
        /// The release downloaded.
        release: Release,
        /// The checked file.
        file: PathBuf,
    },
    /// Something went wrong, in words for the person.
    Failed(String),
}

impl Status {
    /// The release this status is about, once one has been found.
    pub fn release(&self) -> Option<&Release> {
        match self {
            Status::Available(r) | Status::Downloading { release: r, .. } | Status::Ready { release: r, .. } => Some(r),
            _ => None,
        }
    }

    /// Whether a thread is working: checking or downloading.
    fn busy(&self) -> bool {
        matches!(self, Status::Checking | Status::Downloading { .. })
    }
}

/// The updater's one saved setting, and the version the person put off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// Look for updates when the app starts, and every [`CHECK_EVERY`].
    auto: bool,
    /// A version the person said "Later" to: no banner for it, though
    /// Settings still offers it. A newer one brings the banner back.
    later: Option<String>,
}

impl Default for Config {
    /// Looking automatically is on until the person turns it off.
    fn default() -> Self {
        Config { auto: true, later: None }
    }
}

/// [`fetch::download`]'s shape: the release, its file, the folder, a
/// progress callback, and the cancel flag.
type Download = fn(&Release, &Asset, &Path, &dyn Fn(u64, Option<u64>), &AtomicBool) -> Result<PathBuf, String>;

/// The network calls, as plain functions so tests can stand in for GitHub.
#[derive(Clone, Copy)]
struct Net {
    /// [`fetch::latest`].
    latest: fn() -> Result<Option<Release>, String>,
    /// [`fetch::download`].
    download: Download,
}

/// What the worker threads and the app share.
#[derive(Default)]
struct Shared {
    /// Where things stand.
    status: Status,
    /// The file in [`Status::Ready`] has been handed to the app to install,
    /// so [`Updater::tick`] hands it over only once.
    handed: bool,
}

/// Looks for, downloads and hands over updates. Owned by the app and polled
/// from its loop with [`Updater::tick`]; never blocks.
pub struct Updater {
    /// How this copy was installed.
    target: Target,
    /// The version running.
    current: Version,
    /// Where downloads go.
    downloads: PathBuf,
    /// Where [`Config`] is saved.
    config_path: PathBuf,
    /// The saved setting.
    config: Config,
    /// Shared with the worker thread, when one runs.
    shared: Arc<Mutex<Shared>>,
    /// Set to stop a download.
    cancel: Arc<AtomicBool>,
    /// When the last automatic look started.
    last_auto: Option<Instant>,
    /// The network calls.
    net: Net,
}

impl Updater {
    /// An updater for this copy of Stoop, keeping its setting in
    /// `data_dir` and downloads in `downloads` (a folder of its own: it is
    /// emptied before each download).
    pub fn new(data_dir: &Path, downloads: PathBuf) -> Updater {
        Updater::with(Target::detect(), Version::current(), data_dir, downloads, Net { latest: fetch::latest, download: fetch::download })
    }

    /// An updater with everything given, for [`Updater::new`] and tests.
    fn with(target: Target, current: Version, data_dir: &Path, downloads: PathBuf, net: Net) -> Updater {
        let config_path = data_dir.join(CONFIG_FILE);
        let config = std::fs::read_to_string(&config_path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Updater { target, current, downloads, config_path, config, shared: Arc::default(), cancel: Arc::default(), last_auto: None, net }
    }

    /// How this copy was installed.
    pub fn target(&self) -> &Target {
        &self.target
    }

    /// The version running.
    pub fn current(&self) -> Version {
        self.current
    }

    /// Where things stand now.
    pub fn status(&self) -> Status {
        self.lock().status.clone()
    }

    /// Whether the app looks for updates by itself.
    pub fn auto(&self) -> bool {
        self.config.auto
    }

    /// Turns looking by itself on or off, and saves that. Turning it on
    /// looks at once.
    pub fn set_auto(&mut self, on: bool) {
        self.config.auto = on;
        self.save();
        if on {
            self.last_auto = None;
        }
    }

    /// Whether the newer version, if there is one, should be shown across
    /// the top of the window: not after "Later", unless it is already on its
    /// way in.
    pub fn banner(&self) -> bool {
        match &self.lock().status {
            Status::Available(r) => self.config.later.as_deref() != Some(r.version.to_string().as_str()),
            Status::Downloading { .. } | Status::Ready { .. } => true,
            _ => false,
        }
    }

    /// Whether the app can install the update itself, rather than only
    /// point to the release page: the install kind allows it and the
    /// release has a file for it.
    pub fn can_install(&self) -> bool {
        self.target.can_install() && self.lock().status.release().is_some_and(|r| r.asset_for(&self.target).is_some())
    }

    /// The page of the release found, for reading about it or downloading
    /// it by hand.
    pub fn page(&self) -> String {
        self.lock().status.release().map(|r| r.page.clone()).unwrap_or_else(|| format!("https://github.com/{}/releases/latest", release::REPO))
    }

    /// Moves things on; call it from the app's regular tick. Starts an
    /// automatic look when one is due, and returns the downloaded file once,
    /// when a download the person asked for is checked and ready to install.
    pub fn tick(&mut self) -> Option<PathBuf> {
        let due = self.last_auto.is_none_or(|t| t.elapsed() >= CHECK_EVERY);
        if self.config.auto && self.target.can_check() && due && !self.lock().status.busy() {
            self.last_auto = Some(Instant::now());
            // Don't throw away a download or a found update by looking again.
            if matches!(self.lock().status, Status::Idle | Status::UpToDate | Status::Failed(_)) {
                self.look(false);
            }
        }
        let mut shared = self.lock();
        match &shared.status {
            Status::Ready { file, .. } if !shared.handed => {
                let file = file.clone();
                shared.handed = true;
                Some(file)
            }
            _ => None,
        }
    }

    /// Looks for an update now, because the person asked: a failure is
    /// shown, where an automatic look's is not.
    pub fn check(&mut self) {
        if self.target.can_check() && !self.lock().status.busy() {
            self.look(true);
        }
    }

    /// The "Update" button: downloads the newer version, which
    /// [`Updater::tick`] then hands over to install. Pressed again while the
    /// checked file is still waiting, hands it over again.
    pub fn start(&mut self) {
        if !self.can_install() {
            return;
        }
        let status = self.status();
        match status {
            Status::Available(release) => self.fetch(release),
            Status::Ready { .. } => self.lock().handed = false,
            _ => {}
        }
    }

    /// The "Later" button: hides the banner for this version, and stops its
    /// download if one is running.
    pub fn later(&mut self) {
        let status = self.status();
        if let Some(r) = status.release() {
            self.config.later = Some(r.version.to_string());
            self.save();
        }
        if let Status::Downloading { release, .. } = status {
            self.cancel.store(true, Ordering::Relaxed);
            self.lock().status = Status::Available(release);
        }
    }

    /// Records that installing the handed-over file failed, in words for the
    /// person. "Check now" finds the release again, to download it afresh.
    pub fn failed(&mut self, why: String) {
        self.lock().status = Status::Failed(why);
    }

    /// Starts a look on a thread of its own.
    fn look(&self, asked: bool) {
        self.lock().status = Status::Checking;
        let (shared, current, latest) = (self.shared.clone(), self.current, self.net.latest);
        std::thread::Builder::new()
            .name("update check".into())
            .spawn(move || {
                let status = match latest() {
                    Ok(Some(r)) if r.version > current => Status::Available(r),
                    Ok(_) => Status::UpToDate,
                    Err(why) if asked => Status::Failed(format!("Couldn't look for updates: {why}.")),
                    // An automatic look that fails says nothing: being
                    // offline is not news.
                    Err(_) => Status::Idle,
                };
                shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner).status = status;
            })
            .map(drop)
            .unwrap_or_else(|e| self.lock().status = Status::Failed(format!("Couldn't look for updates: {e}.")));
    }

    /// Starts downloading `release`'s file on a thread of its own.
    fn fetch(&mut self, release: Release) {
        let Some(asset) = release.asset_for(&self.target).cloned() else { return };
        // Old downloads and half-finished ones go; this folder is the
        // updater's alone.
        let _ = std::fs::remove_dir_all(&self.downloads);
        self.cancel = Arc::default();
        {
            let mut shared = self.lock();
            shared.status = Status::Downloading { release: release.clone(), done: 0, total: (asset.size > 0).then_some(asset.size) };
            shared.handed = false;
        }
        let (shared, cancel, dir, download) = (self.shared.clone(), self.cancel.clone(), self.downloads.clone(), self.net.download);
        std::thread::Builder::new()
            .name("update download".into())
            .spawn(move || {
                let lock = || shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let progress = |done: u64, total: Option<u64>| {
                    if let Status::Downloading { done: d, total: t, .. } = &mut lock().status {
                        *d = done;
                        *t = total.or(*t);
                    }
                };
                let result = download(&release, &asset, &dir, &progress, &cancel);
                // "Later" while it ran has put things back already.
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                lock().status = match result {
                    Ok(file) => Status::Ready { release, file },
                    Err(why) => Status::Failed(format!("Couldn't download the update: {why}.")),
                };
            })
            .map(drop)
            .unwrap_or_else(|e| self.lock().status = Status::Failed(format!("Couldn't download the update: {e}.")));
    }

    /// The shared state, even after a worker thread panicked holding it.
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Saves the setting; a failure only means it is asked again next time.
    fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(&self.config) {
            let _ = std::fs::write(&self.config_path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release one version ahead, with an AppImage and checksums.
    fn newer() -> Release {
        let arch = std::env::consts::ARCH;
        let asset = |name: String| Asset { url: format!("https://example.invalid/{name}"), name, size: 3 };
        Release {
            version: Version::parse("0.3.1").unwrap(),
            name: "Stoop 0.3.1".into(),
            page: "https://example.invalid/v0.3.1".into(),
            assets: vec![asset(format!("Stoop-0.3.1-linux-{arch}.AppImage")), asset("SHA256SUMS".into())],
        }
    }

    /// GitHub, answering with [`newer`].
    #[expect(clippy::unnecessary_wraps, reason = "stands in for the network call, which can fail")]
    fn found() -> Result<Option<Release>, String> {
        Ok(Some(newer()))
    }

    /// GitHub, out of reach.
    fn offline() -> Result<Option<Release>, String> {
        Err("offline".into())
    }

    /// A download that writes three bytes and succeeds.
    #[expect(clippy::unnecessary_wraps, reason = "stands in for the download, which can fail")]
    fn downloaded(_: &Release, asset: &Asset, dir: &Path, progress: &dyn Fn(u64, Option<u64>), _: &AtomicBool) -> Result<PathBuf, String> {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(&asset.name);
        std::fs::write(&path, b"new").unwrap();
        progress(3, Some(3));
        Ok(path)
    }

    /// An updater on fake network calls, in a scratch folder, as an
    /// AppImage at 0.3.0.
    fn updater(name: &str, latest: fn() -> Result<Option<Release>, String>) -> (Updater, PathBuf) {
        let dir = std::env::temp_dir().join(format!("stoop-updater-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let net = Net { latest, download: downloaded };
        let u = Updater::with(Target::AppImage(dir.join("Stoop.AppImage")), Version::parse("0.3.0").unwrap(), &dir, dir.join("downloads"), net);
        (u, dir)
    }

    /// Ticks until the worker thread has finished.
    fn settle(u: &mut Updater) -> Option<PathBuf> {
        for _ in 0..500 {
            let file = u.tick();
            if file.is_some() || !u.status().busy() {
                return file.or_else(|| u.tick());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the worker never finished");
    }

    #[test]
    fn finds_downloads_and_hands_over_an_update() {
        let (mut u, dir) = updater("flow", found);
        assert_eq!(u.status(), Status::Idle);
        assert!(u.auto(), "on until turned off");
        assert_eq!(settle(&mut u), None);
        assert_eq!(u.status(), Status::Available(newer()));
        assert!(u.banner());
        assert!(u.can_install());
        assert_eq!(u.page(), "https://example.invalid/v0.3.1");

        u.start();
        let file = settle(&mut u).expect("handed over once ready");
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        assert!(matches!(u.status(), Status::Ready { .. }));
        assert_eq!(u.tick(), None, "only once");

        // An install that failed says why.
        u.failed("no".into());
        assert_eq!(u.status(), Status::Failed("no".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn later_hides_the_banner_until_a_newer_one() {
        let (mut u, dir) = updater("later", found);
        settle(&mut u);
        u.later();
        assert!(!u.banner());
        assert!(matches!(u.status(), Status::Available(_)), "Settings still offers it");
        // Remembered across a restart.
        let again = Updater::with(u.target.clone(), u.current, &dir, dir.join("downloads"), u.net);
        assert_eq!(again.config.later.as_deref(), Some("0.3.1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn automatic_failures_are_quiet_and_asked_ones_are_not() {
        let (mut u, dir) = updater("offline", offline);
        settle(&mut u);
        assert_eq!(u.status(), Status::Idle);
        u.check();
        settle(&mut u);
        assert!(matches!(u.status(), Status::Failed(why) if why.contains("offline")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_same_version_is_up_to_date() {
        /// GitHub, with the running version as its newest.
        #[expect(clippy::unnecessary_wraps, reason = "stands in for the network call, which can fail")]
        fn same() -> Result<Option<Release>, String> {
            Ok(Some(Release { version: Version::parse("0.3.0").unwrap(), ..newer() }))
        }
        let (mut u, dir) = updater("same", same);
        settle(&mut u);
        assert_eq!(u.status(), Status::UpToDate);
        assert!(!u.banner());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn turning_automatic_off_is_saved_and_stops_looking() {
        let (mut u, dir) = updater("auto", found);
        u.set_auto(false);
        assert_eq!(u.tick(), None);
        assert_eq!(u.status(), Status::Idle);
        let again = Updater::with(u.target.clone(), u.current, &dir, dir.join("downloads"), u.net);
        assert!(!again.auto());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_install_that_cannot_update_itself_only_points_to_the_page() {
        let (mut u, dir) = updater("page", found);
        u.target = Target::LinuxOther;
        settle(&mut u);
        assert!(matches!(u.status(), Status::Available(_)));
        assert!(!u.can_install());
        u.start();
        assert!(matches!(u.status(), Status::Available(_)), "nothing to download");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
