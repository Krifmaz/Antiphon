//! How this copy of Stoop was installed, which decides which file of a
//! release replaces it and how.

use std::path::{Path, PathBuf};

/// The kind of install this copy of Stoop is, as far as updating it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Windows, from the NSIS installer (`Stoop-<v>-windows-<arch>.exe`),
    /// under `%LOCALAPPDATA%\Stoop`. Updated by running the new installer
    /// silently; it closes the old copy itself.
    WindowsExe,
    /// Windows, from the MSI (`Stoop-<v>-windows-<arch>.msi`), per user
    /// under `%LOCALAPPDATA%\Programs\Stoop`. Updated through `msiexec`.
    WindowsMsi,
    /// Linux, running as this AppImage (the `APPIMAGE` variable the
    /// AppImage runtime sets). Updated by putting the new image in its
    /// place.
    AppImage(PathBuf),
    /// Linux, from the `.deb`. The new package is handed to the system's
    /// software installer, which asks for the password a package needs.
    Deb,
    /// Linux, in a Flatpak sandbox or from the plain tarball. Neither can be
    /// replaced from inside, so the person is sent to the release page.
    LinuxOther,
    /// macOS, as `Stoop.app` at this path (from the `.dmg`). The new app
    /// is copied over it from the disk image.
    MacApp(PathBuf),
    /// Android, from the APK. The phone app downloads it and hands it to
    /// the system's package installer, which asks the person to confirm.
    Android,
    /// Somewhere Stoop cannot update itself and that has no file to send
    /// the person to: iOS, whose apps come from the App Store or TestFlight,
    /// and any platform the release does not build for.
    Unsupported,
}

impl Target {
    /// Works out how the running copy was installed.
    pub fn detect() -> Target {
        if cfg!(target_os = "android") {
            return Target::Android;
        }
        if cfg!(target_os = "ios") {
            return Target::Unsupported;
        }
        let exe = std::env::current_exe().ok();
        if cfg!(target_os = "windows") {
            // The MSI's folder is Programs\Stoop; the NSIS installer's is
            // Stoop straight under the local app data folder.
            let msi = exe.as_deref().is_some_and(|e| e.components().any(|c| c.as_os_str().eq_ignore_ascii_case("Programs")));
            return if msi { Target::WindowsMsi } else { Target::WindowsExe };
        }
        if cfg!(target_os = "macos") {
            return match exe.as_deref().and_then(app_bundle) {
                Some(app) => Target::MacApp(app),
                None => Target::Unsupported,
            };
        }
        if cfg!(target_os = "linux") {
            if let Some(image) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
                return Target::AppImage(PathBuf::from(image));
            }
            if std::env::var_os("FLATPAK_ID").is_some() {
                return Target::LinuxOther;
            }
            // The .deb puts the binary in /usr/bin; the tarball goes wherever
            // it was unpacked.
            let packaged = exe.as_deref().is_some_and(|e| e.starts_with("/usr/bin"));
            return if packaged && Path::new("/usr/bin/dpkg").exists() { Target::Deb } else { Target::LinuxOther };
        }
        Target::Unsupported
    }

    /// Whether the app can fetch and install the update itself. When it
    /// cannot, it can still say that one is out and open its page.
    pub fn can_install(&self) -> bool {
        !matches!(self, Target::LinuxOther | Target::Unsupported)
    }

    /// Whether looking for updates makes sense at all here. iOS updates
    /// through the App Store, so the app does not look.
    pub fn can_check(&self) -> bool {
        !matches!(self, Target::Unsupported)
    }

    /// Whether `name`, a file of a release, is the one that updates this
    /// install. Names are those `packaging/package.sh` and the Android job
    /// give (see the list at the top of `.github/workflows/release.yml`).
    pub fn wants(&self, name: &str) -> bool {
        let arch = std::env::consts::ARCH;
        match self {
            Target::WindowsExe => name.starts_with("Stoop-") && name.ends_with(&format!("-windows-{arch}.exe")),
            Target::WindowsMsi => name.starts_with("Stoop-") && name.ends_with(&format!("-windows-{arch}.msi")),
            Target::AppImage(_) => name.starts_with("Stoop-") && name.ends_with(&format!("-linux-{arch}.AppImage")),
            Target::Deb => name.starts_with("stoop_") && name.ends_with(&format!("_{}.deb", deb_arch(arch))),
            Target::MacApp(_) => name.starts_with("Stoop-") && name.ends_with(&format!("-macos-{arch}.dmg")),
            Target::Android => name.starts_with("Stoop-android-") && name.ends_with(".apk"),
            Target::LinuxOther | Target::Unsupported => false,
        }
    }
}

/// Debian's name for a Rust architecture (`x86_64` is `amd64`).
fn deb_arch(arch: &str) -> &str {
    match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "i386",
        other => other,
    }
}

/// The `.app` bundle an executable sits in (`Stoop.app/Contents/MacOS/x`
/// gives `Stoop.app`), or `None` for a bare binary.
fn app_bundle(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_install_picks_its_own_file() {
        let arch = std::env::consts::ARCH;
        let files = [
            format!("Stoop-0.3.1-windows-{arch}.exe"),
            format!("Stoop-0.3.1-windows-{arch}.msi"),
            format!("Stoop-0.3.1-linux-{arch}.AppImage"),
            format!("stoop_0.3.1_{}.deb", deb_arch(arch)),
            format!("stoop-0.3.1-linux-{arch}.tar.gz"),
            format!("Stoop-0.3.1-linux-{arch}.flatpak"),
            format!("Stoop-0.3.1-macos-{arch}.dmg"),
            "Stoop-android-0.3.1.apk".to_string(),
            "SHA256SUMS".to_string(),
        ];
        let picks = |t: Target| files.iter().filter(|f| t.wants(f)).cloned().collect::<Vec<_>>();
        assert_eq!(picks(Target::WindowsExe), [files[0].clone()]);
        assert_eq!(picks(Target::WindowsMsi), [files[1].clone()]);
        assert_eq!(picks(Target::AppImage("/x".into())), [files[2].clone()]);
        assert_eq!(picks(Target::Deb), [files[3].clone()]);
        assert_eq!(picks(Target::MacApp("/A/Stoop.app".into())), [files[6].clone()]);
        assert_eq!(picks(Target::Android), [files[7].clone()]);
        assert!(picks(Target::LinuxOther).is_empty());
        assert!(picks(Target::Unsupported).is_empty());
    }

    #[test]
    fn another_architecture_is_not_ours() {
        let other = if std::env::consts::ARCH == "x86_64" { "aarch64" } else { "x86_64" };
        assert!(!Target::WindowsExe.wants(&format!("Stoop-0.3.1-windows-{other}.exe")));
        assert!(!Target::AppImage("/x".into()).wants(&format!("Stoop-0.3.1-linux-{other}.AppImage")));
    }

    #[test]
    fn finds_the_app_bundle() {
        assert_eq!(app_bundle(Path::new("/Applications/Stoop.app/Contents/MacOS/antiphon")), Some(PathBuf::from("/Applications/Stoop.app")));
        assert_eq!(app_bundle(Path::new("/usr/local/bin/antiphon")), None);
    }

    #[test]
    fn says_what_it_can_do() {
        assert!(Target::WindowsExe.can_install());
        assert!(!Target::LinuxOther.can_install());
        assert!(Target::LinuxOther.can_check());
        assert!(!Target::Unsupported.can_check());
    }
}
