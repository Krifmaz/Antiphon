//! The network half: asking GitHub for the newest release, and downloading
//! a file of it with its checksum checked.
//!
//! Blocking calls through ureq; [`crate::Updater`] runs them on a thread of
//! their own so neither app's interface ever waits on the network.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::release::{hex, sum_for, Asset, Release, REPO};

/// How long to wait for a name lookup, a connection, or the first answer,
/// before giving up.
const PATIENCE: Duration = Duration::from_secs(30);

/// The longest a whole download may take. An installer is 20 to 60 MB, so
/// this is room for a very slow line, and stops a stalled one hanging the
/// download forever.
const DOWNLOAD_LIMIT: Duration = Duration::from_secs(60 * 60);

/// The most a checksum list may be. The real one is about a kilobyte; this
/// only stops a broken server from filling memory.
const SUMS_LIMIT: u64 = 1 << 20;

/// The HTTP client both calls share: a Stoop user agent (GitHub's API
/// refuses requests without one), timeouts, and the proxy from the
/// environment, as ureq does by default.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(format!("Stoop/{} (+https://github.com/{REPO})", crate::Version::current()))
        .timeout_resolve(Some(PATIENCE))
        .timeout_connect(Some(PATIENCE))
        .timeout_recv_response(Some(PATIENCE))
        .timeout_recv_body(Some(DOWNLOAD_LIMIT))
        .build()
        .into()
}

/// The release GitHub marks Latest: the newest release or hotfix, whichever
/// came last (`.github/workflows/release.yml` marks each one Latest as it is
/// published). `Ok(None)` when there is none, or it is not one to offer.
pub fn latest() -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut response = agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .map_err(|e| match e {
            // A repository with no releases yet answers 404.
            ureq::Error::StatusCode(404) => "no releases yet".to_string(),
            ureq::Error::StatusCode(403 | 429) => "GitHub asked to wait before looking again".to_string(),
            e => format!("couldn't reach GitHub ({e})"),
        })?;
    let json = response.body_mut().read_to_string().map_err(|e| format!("couldn't read GitHub's answer ({e})"))?;
    Release::from_json(&json)
}

/// Downloads `asset` of `release` into `dir` and checks it against the
/// release's `SHA256SUMS`, which must name it.
///
/// The file is written as `<name>.part` and renamed only once its checksum
/// matches, so a half-finished or tampered download is never mistaken for
/// a good one. `progress` hears (bytes so far, bytes in all when known) as
/// the file arrives; setting `cancel` stops it, and leaves nothing behind.
pub fn download(release: &Release, asset: &Asset, dir: &Path, progress: &dyn Fn(u64, Option<u64>), cancel: &AtomicBool) -> Result<PathBuf, String> {
    let sums = release.sums().ok_or("the release has no checksum list, so its files can't be checked")?;
    let agent = agent();
    let list = agent
        .get(&sums.url)
        .call()
        .map_err(|e| format!("couldn't download the checksum list ({e})"))?
        .body_mut()
        .with_config()
        .limit(SUMS_LIMIT)
        .read_to_string()
        .map_err(|e| format!("couldn't read the checksum list ({e})"))?;
    let want = sum_for(&list, &asset.name).ok_or_else(|| format!("the checksum list does not name {}", asset.name))?;

    std::fs::create_dir_all(dir).map_err(|e| format!("couldn't make {} ({e})", dir.display()))?;
    // A plain file name only: never a path out of the download folder,
    // whatever the release calls its files.
    let name = Path::new(&asset.name).file_name().ok_or("the release file has no name")?;
    let path = dir.join(name);
    let part = dir.join(format!("{}.part", name.to_string_lossy()));

    let result = (|| {
        let mut response = agent.get(&asset.url).call().map_err(|e| format!("couldn't download {} ({e})", asset.name))?;
        let total = response.body().content_length().or((asset.size > 0).then_some(asset.size));
        let mut body = response.body_mut().as_reader();
        let mut file = File::create(&part).map_err(|e| format!("couldn't write {} ({e})", part.display()))?;
        let mut hash = Sha256::new();
        let mut done = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        progress(0, total);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            let n = body.read(&mut buf).map_err(|e| format!("the download broke off ({e})"))?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| format!("couldn't write {} ({e})", part.display()))?;
            done += n as u64;
            progress(done, total);
        }
        file.sync_all().map_err(|e| format!("couldn't save {} ({e})", part.display()))?;
        let got = hex(&hash.finalize());
        if got != want {
            return Err(format!("{} did not match its checksum, so it was thrown away", asset.name));
        }
        std::fs::rename(&part, &path).map_err(|e| format!("couldn't save {} ({e})", path.display()))?;
        Ok(path.clone())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}
