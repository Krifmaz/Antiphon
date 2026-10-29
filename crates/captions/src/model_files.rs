//! The caption model's files: where they live, how they are checked, and
//! fetching them once the user has agreed to.
//!
//! The model is not bundled -- it is 150 MB that most people will never
//! use -- and nothing is downloaded until someone turns captions on and
//! accepts the download, which states the size and the host. After that
//! it is local for good; captioning itself never touches the network.
//!
//! **Integrity.** Each file's SHA-256 is pinned below, and a download that
//! does not match is deleted, not used. A pin left as `None` means the
//! hash has not been confirmed for this build, and then the app refuses to
//! download at all rather than run unverified weights fetched from the
//! internet; the user can still point it at files they already have.
//!
//! **Transport.** The fetch is done by the system's `curl` (shipped with
//! Windows 10 and later, macOS and practically every Linux), restricted to
//! HTTPS. That keeps a TLS stack out of the app for one optional download,
//! and the pinned hash -- not the transport -- is what makes the result
//! trustworthy.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};

pub struct ModelFile {
    pub name: &'static str,
    pub url: &'static str,
    /// Approximate size, for the consent prompt and progress bar.
    pub size: u64,
    /// Lower-case hex SHA-256. `None` until confirmed; see module docs.
    pub sha256: Option<&'static str>,
}

/// Folder under the app's config directory.
pub const MODEL_DIR: &str = "captions-whisper-tiny.en";

/// Whisper tiny, English-only: the smallest model, and English-only
/// because at this size the multilingual one is noticeably worse at
/// English. Revision-pinned URLs would be better still; the hashes are
/// what is actually relied on.
//
// TODO(parent): fill in the three hashes (`sha256sum` of each file from
// these URLs) -- they could not be fetched from the build sandbox.
pub const FILES: &[ModelFile] = &[
    ModelFile {
        name: "config.json",
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/main/config.json",
        size: 2_000,
        sha256: None,
    },
    ModelFile {
        name: "tokenizer.json",
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/main/tokenizer.json",
        size: 2_500_000,
        sha256: None,
    },
    ModelFile {
        name: "model.safetensors",
        url: "https://huggingface.co/openai/whisper-tiny.en/resolve/main/model.safetensors",
        size: 151_000_000,
        sha256: None,
    },
];

pub const HOST: &str = "huggingface.co";

pub fn total_size() -> u64 {
    FILES.iter().map(|f| f.size).sum()
}

/// True when every hash is pinned, i.e. downloading is allowed.
pub fn download_available() -> bool {
    FILES.iter().all(|f| f.sha256.is_some())
}

pub fn model_dir(config_dir: &Path) -> PathBuf {
    config_dir.join(MODEL_DIR)
}

/// Every file present. Not re-hashed: that would mean reading 150 MB on
/// every launch, and the hash was checked when each file arrived.
pub fn installed(dir: &Path) -> bool {
    FILES.iter().all(|f| dir.join(f.name).is_file())
}

pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Checks a file against its pin. Unpinned files pass: that case only
/// arises for files the user supplied themselves.
pub fn verify(file: &ModelFile, path: &Path) -> Result<(), String> {
    let Some(want) = file.sha256 else { return Ok(()) };
    let got = sha256_file(path).map_err(|e| format!("{}: {e}", file.name))?;
    if got == want {
        Ok(())
    } else {
        Err(format!("{} does not match its pinned checksum", file.name))
    }
}

/// Copies the model from a folder the user picked (for machines that
/// cannot reach the host, or people who would rather fetch it
/// themselves). Pinned files are still verified.
pub fn import_from(src: &Path, dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    for f in FILES {
        let from = src.join(f.name);
        if !from.is_file() {
            return Err(format!("{} is not in that folder", f.name));
        }
        verify(f, &from)?;
    }
    for f in FILES {
        let tmp = dir.join(format!("{}.part", f.name));
        std::fs::copy(src.join(f.name), &tmp).map_err(|e| format!("{}: {e}", f.name))?;
        std::fs::rename(&tmp, dir.join(f.name)).map_err(|e| format!("{}: {e}", f.name))?;
    }
    Ok(())
}

pub fn remove(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Shared with the UI while a download runs.
#[derive(Default)]
pub struct Progress {
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
    pub done: AtomicBool,
    pub error: Mutex<Option<String>>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        (self.bytes.load(Ordering::Relaxed) as f64 / total_size() as f64).min(1.0) as f32
    }
}

/// Downloads every file into `dir`, verifying each. Blocking; run it on a
/// thread. Leaves nothing half-written behind on failure.
pub fn download(dir: &Path, progress: Arc<Progress>) {
    let result = download_inner(dir, &progress);
    if let Err(e) = result {
        for f in FILES {
            let _ = std::fs::remove_file(dir.join(format!("{}.part", f.name)));
        }
        *progress.error.lock().unwrap() = Some(e);
    }
    progress.done.store(true, Ordering::Relaxed);
}

fn download_inner(dir: &Path, progress: &Progress) -> Result<(), String> {
    if !download_available() {
        return Err("this build has no pinned checksum for the caption model, so it will not \
                    download one; use \"Use files I already have\" instead"
            .into());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let mut finished = 0u64;
    for f in FILES {
        let dest = dir.join(f.name);
        if dest.is_file() && verify(f, &dest).is_ok() {
            finished += f.size;
            progress.bytes.store(finished, Ordering::Relaxed);
            continue;
        }
        let part = dir.join(format!("{}.part", f.name));
        let mut child = curl(f.url, &part).spawn().map_err(|e| format!("could not run curl: {e}"))?;
        let status = loop {
            if progress.cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("cancelled".into());
            }
            let so_far = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
            progress.bytes.store(finished + so_far.min(f.size), Ordering::Relaxed);
            if let Some(status) = poll(&mut child)? {
                break status;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        if !status.success() {
            return Err(format!("downloading {} failed ({status})", f.name));
        }
        verify(f, &part)?;
        std::fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        finished += f.size;
        progress.bytes.store(finished, Ordering::Relaxed);
    }
    Ok(())
}

fn poll(child: &mut Child) -> Result<Option<std::process::ExitStatus>, String> {
    child.try_wait().map_err(|e| e.to_string())
}

fn curl(url: &str, out: &Path) -> Command {
    let mut cmd = Command::new("curl");
    cmd.args(["--fail", "--location", "--silent", "--show-error", "--proto", "=https", "--proto-redir", "=https"])
        .arg("--output")
        .arg(out)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing up over the app.
        cmd.creation_flags(0x0800_0000);
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("antiphon-captions-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sha256_of_a_known_file() {
        let d = tmp("sha");
        std::fs::write(d.join("x"), b"abc").unwrap();
        assert_eq!(
            sha256_file(&d.join("x")).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_mismatched_file_is_rejected() {
        let d = tmp("pin");
        std::fs::write(d.join("x"), b"abc").unwrap();
        let f = ModelFile { name: "x", url: "", size: 3, sha256: Some("00") };
        assert!(verify(&f, &d.join("x")).is_err());
        let f = ModelFile {
            sha256: Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"),
            ..f
        };
        assert!(verify(&f, &d.join("x")).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn import_needs_every_file_and_installs_them() {
        let src = tmp("src");
        let dst = tmp("dst").join("model");
        std::fs::write(src.join("config.json"), b"{}").unwrap();
        assert!(import_from(&src, &dst).is_err());
        assert!(!installed(&dst));
        if download_available() {
            return; // Pinned: fake files would (rightly) fail the check.
        }
        std::fs::write(src.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(src.join("model.safetensors"), b"").unwrap();
        import_from(&src, &dst).unwrap();
        assert!(installed(&dst));
        remove(&dst).unwrap();
        assert!(!installed(&dst));
        let _ = std::fs::remove_dir_all(src);
    }
}
