//! Putting a downloaded, checked update in place on a desktop.
//!
//! Each install kind ([`Target`]) has its own way in, and each fails soft:
//! when the app cannot replace itself (no permission to write where it is,
//! a tool missing) it hands the file to the system instead and says what
//! to do next. The phone's APK goes through the phone app's platform layer
//! instead, since only Android's own installer can install it.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::target::Target;

/// What the app should do once [`install`] has done its part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum After {
    /// Close down now: an installer is running, and starts the new version
    /// when it is done (Windows).
    Quit,
    /// The new version is in place: start it with [`relaunch`] and close
    /// down (AppImage, macOS).
    Restart(PathBuf),
    /// The file was handed to the system, which takes it from here; tell the
    /// person this, and keep running.
    Handed(String),
}

/// Installs `file`, the update for `target`, or hands it to the system.
/// The error is a sentence for the person.
pub fn install(target: &Target, file: &Path) -> Result<After, String> {
    match target {
        Target::WindowsExe | Target::WindowsMsi => windows(target, file),
        Target::AppImage(image) => appimage(image, file),
        Target::Deb => {
            open(file)?;
            Ok(After::Handed("Your software installer has the update. Finish there, then reopen Stoop.".into()))
        }
        Target::MacApp(app) => mac(app, file),
        Target::Android => Err("The phone installs its update through Android's own installer.".into()),
        Target::LinuxOther | Target::Unsupported => Err("This copy of Stoop can't update itself. Get the new version from the release page.".into()),
    }
}

/// Starts the copy of Stoop at `path`: an `.app` through `open`, so macOS
/// starts it as an app, anything else directly.
pub fn relaunch(path: &Path) -> Result<(), String> {
    let mut command = if cfg!(target_os = "macos") && path.extension().is_some_and(|e| e == "app") {
        let mut c = Command::new("open");
        c.arg("-n").arg(path);
        c
    } else {
        Command::new(path)
    };
    command.spawn().map(drop).map_err(|e| format!("couldn't start the new version ({e}); open Stoop again yourself"))
}

/// Opens a file or web page with whatever the system uses for it: the
/// release page in the browser, a `.deb` in the software installer.
pub fn open(what: impl AsRef<std::ffi::OsStr>) -> Result<(), String> {
    let what = what.as_ref();
    let mut command = if cfg!(target_os = "windows") {
        // `start` is a cmd built-in; its first quoted argument is a title.
        let mut c = Command::new("cmd");
        c.arg("/C").arg("start").arg("").arg(what);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(what);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(what);
        c
    };
    hide_console(&mut command);
    command.spawn().map(drop).map_err(|e| format!("couldn't open {} ({e})", what.to_string_lossy()))
}

/// Windows: a small script runs the installer silently, then starts the
/// new version. The NSIS installer installs where the old one is (it reads
/// its folder back from the registry) and waits for the old copy to close,
/// closing it after ten seconds itself; the MSI replaces the installed one
/// through its upgrade code. The script outlives this process, which quits
/// as soon as it has started it.
fn windows(target: &Target, file: &Path) -> Result<After, String> {
    let exe = std::env::current_exe().map_err(|e| format!("couldn't tell where Stoop is installed ({e})"))?;
    let run = match target {
        Target::WindowsMsi => format!("msiexec /i \"{}\" /qn /norestart", file.display()),
        _ => format!("\"{}\" /S", file.display()),
    };
    let script = format!("@echo off\r\n{run}\r\nstart \"\" \"{}\"\r\n", exe.display());
    let path = file.with_extension("cmd");
    std::fs::write(&path, script).map_err(|e| format!("couldn't prepare the installer ({e})"))?;
    let mut command = Command::new("cmd");
    command.arg("/C").arg(&path);
    hide_console(&mut command);
    command.spawn().map_err(|e| format!("couldn't start the installer ({e})"))?;
    Ok(After::Quit)
}

/// Keeps a console window from flashing up for a command run from the app
/// (Windows only; elsewhere there is none to hide).
fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// `CREATE_NO_WINDOW`, from the Windows process creation flags.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// AppImage: the new image replaces the old file under the same name. It is
/// copied next to the old one first, so the last step is a rename within one
/// folder, which either happens whole or not at all; the running copy keeps
/// its own, now unnamed, file until it closes.
fn appimage(image: &Path, file: &Path) -> Result<After, String> {
    let dir = image.parent().ok_or("couldn't tell which folder the AppImage is in")?;
    let name = image.file_name().ok_or("the AppImage has no name")?.to_string_lossy().into_owned();
    let staged = dir.join(format!(".{name}.update"));
    let fail = |e: std::io::Error| format!("couldn't replace {} ({e}). Download the new AppImage from the release page instead.", image.display());
    std::fs::copy(file, &staged).map_err(fail)?;
    let placed = (|| {
        make_executable(&staged)?;
        std::fs::rename(&staged, image)
    })();
    if let Err(e) = placed {
        let _ = std::fs::remove_file(&staged);
        return Err(fail(e));
    }
    let _ = std::fs::remove_file(file);
    Ok(After::Restart(image.to_path_buf()))
}

/// Sets a file's mode to `rwxr-xr-x`, as a downloaded AppImage needs.
fn make_executable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// macOS: mounts the disk image out of sight, copies the `.app` in it next to
/// the installed one, swaps the two by renaming, and unmounts. When the
/// installed app's folder can't be written (an `/Applications` that needs an
/// administrator), the disk image is opened in Finder instead, for the
/// person to drag the app across as on first install.
fn mac(app: &Path, file: &Path) -> Result<After, String> {
    match swap_app(app, file) {
        Ok(()) => Ok(After::Restart(app.to_path_buf())),
        Err(_) => {
            open(file)?;
            Ok(After::Handed("Drag Stoop from the window that opened into Applications, then reopen it.".into()))
        }
    }
}

/// The swap behind [`mac`]: every step undone if a later one fails, so the
/// installed app is either the old one or the new one, never neither.
fn swap_app(app: &Path, dmg: &Path) -> Result<(), String> {
    let dir = app.parent().ok_or("no folder")?;
    let mount = dmg.with_extension("mount");
    let _ = std::fs::create_dir_all(&mount);
    run(Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mount).arg(dmg))?;
    let staged = dir.join(".Stoop.app.update");
    let copied = (|| {
        let source = std::fs::read_dir(&mount)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .ok_or("no app in the disk image")?;
        let _ = std::fs::remove_dir_all(&staged);
        // ditto keeps the bundle's links, modes and signature intact.
        run(Command::new("ditto").arg(&source).arg(&staged))
    })();
    let _ = run(Command::new("hdiutil").args(["detach", "-quiet"]).arg(&mount));
    let _ = std::fs::remove_dir(&mount);
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e);
    }
    let old = dir.join(".Stoop.app.old");
    let _ = std::fs::remove_dir_all(&old);
    std::fs::rename(app, &old).map_err(|e| {
        let _ = std::fs::remove_dir_all(&staged);
        e.to_string()
    })?;
    if let Err(e) = std::fs::rename(&staged, app) {
        // Put the old one back rather than leave no app at all.
        let _ = std::fs::rename(&old, app);
        let _ = std::fs::remove_dir_all(&staged);
        return Err(e.to_string());
    }
    let _ = std::fs::remove_dir_all(&old);
    let _ = std::fs::remove_file(dmg);
    Ok(())
}

/// Runs a command to the end; an error unless it succeeded.
fn run(command: &mut Command) -> Result<(), String> {
    let status = command.status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh, empty folder under the system's temporary one.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("stoop-update-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_appimage_is_replaced_in_place() {
        let dir = scratch("appimage");
        let image = dir.join("Stoop.AppImage");
        std::fs::write(&image, b"old").unwrap();
        let download = dir.join("download");
        std::fs::create_dir_all(&download).unwrap();
        let new = download.join("Stoop-0.3.1-linux-x86_64.AppImage");
        std::fs::write(&new, b"new").unwrap();

        let after = install(&Target::AppImage(image.clone()), &new).unwrap();
        assert_eq!(after, After::Restart(image.clone()));
        assert_eq!(std::fs::read(&image).unwrap(), b"new");
        assert!(!new.exists(), "the download is cleaned up");
        assert!(!dir.join(".Stoop.AppImage.update").exists(), "nothing is left staged");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&image).unwrap().permissions().mode() & 0o777, 0o755);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_appimage_that_cannot_be_reached_is_left_alone() {
        let dir = scratch("appimage-missing");
        let new = dir.join("new.AppImage");
        std::fs::write(&new, b"new").unwrap();
        let image = dir.join("no-such-folder").join("Stoop.AppImage");
        assert!(install(&Target::AppImage(image.clone()), &new).is_err());
        assert!(!image.exists());
        assert!(new.exists(), "the download is kept to try again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn installs_that_cannot_update_say_so() {
        let file = Path::new("/nonexistent");
        assert!(install(&Target::LinuxOther, file).is_err());
        assert!(install(&Target::Unsupported, file).is_err());
        assert!(install(&Target::Android, file).is_err());
    }
}
