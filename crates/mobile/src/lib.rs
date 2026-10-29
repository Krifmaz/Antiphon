//! Antiphon's phone app, shared by Android and iOS, written entirely in
//! Rust with iced.
//!
//! `winit` provides the window, `wgpu` draws into it, and iced lays out
//! the interface. The call itself is the same code the desktop binaries
//! run -- `antiphon-crypto` for identity and the handshake,
//! `antiphon-audio` for the mic chain, `antiphon-net` for the encrypted
//! UDP link -- so a phone and a desktop can call each other.
//!
//! iced's own `iced_winit` shell does not build for Android (it calls
//! winit APIs that only exist on desktop), so [`runner`] drives iced's
//! `UserInterface` directly, the way iced's `integration` example does,
//! and [`convert`] translates winit's touch events itself.
//!
//! What differs between the phones -- permissions, the audio session,
//! the clipboard, screen insets -- is behind [`platform::Platform`], which
//! each platform crate implements and installs before starting
//! [`runner::Runner`].
//!
//! Chat, its history, and file transfer are the desktop's own modules,
//! compiled in here by path rather than rewritten: a phone and a desktop
//! must agree on every byte of the control messages, the chunk format and
//! the history file, and one copy of that code is the only way to be sure
//! they always will.
//!
//! Everything that touches the phone is `cfg`'d to Android and iOS, so
//! `cargo build --workspace` on a desktop does not build a second GUI
//! toolkit. The two rule modules, [`queue`] and [`data_rules`], are plain
//! std and compile everywhere, so their tests run on the host.

// Every item this crate owns is documented, and these keep it that way.
// The modules compiled in by `#[path]` below belong to the desktop crate
// and carry their own `allow`, so its documentation is not enforced twice.
#![warn(missing_docs, clippy::missing_docs_in_private_items)]

pub mod data_rules;
pub mod fling;
pub mod queue;

#[cfg(any(target_os = "android", target_os = "ios"))]
mod call;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod call_log;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod contacts;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod conversation;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod convert;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod files;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub mod platform;
#[cfg(any(target_os = "android", target_os = "ios"))]
pub mod runner;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod settings;
#[cfg(any(target_os = "android", target_os = "ios"))]
mod ui;

// The desktop's wire-level modules, shared by path. Each is used only in
// part here (the desktop has screens for things a phone does not offer
// yet), hence `dead_code`; none of them is edited for the phone's sake.
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/content_store.rs"]
mod content_store;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/control.rs"]
mod control;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/file_integrity.rs"]
mod file_integrity;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/file_preview.rs"]
mod file_preview;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/file_transfer.rs"]
mod file_transfer;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/history.rs"]
mod history;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/outbox.rs"]
mod outbox;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/pacer.rs"]
mod pacer;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/profile.rs"]
mod profile;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/reactions.rs"]
mod reactions;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/transfer_limits.rs"]
mod transfer_limits;
#[cfg(any(target_os = "android", target_os = "ios"))]
#[allow(dead_code, missing_docs, clippy::missing_docs_in_private_items)]
#[path = "../../client/src/video.rs"]
mod video;

/// What `file_transfer` asks of the desktop's `config` module, answered
/// from the phone's own storage.
///
/// The desktop's `config.rs` is the whole desktop settings file -- push to
/// talk keys, soundboard clips, delivery routes -- and cannot be compiled
/// here without most of the desktop app. `file_transfer` only needs two
/// things from it: where to keep received files, and the upload limits.
/// So this is a stand-in with exactly those two, filled in by the app.
#[cfg(any(target_os = "android", target_os = "ios"))]
mod config {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use crate::transfer_limits::TransferLimits;

    /// Where received files and partial downloads are kept: the app's
    /// private storage, set by [`set_dir`] before any transfer starts.
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    /// The current limits, or the defaults until the app first sets them.
    /// Behind a mutex because a transfer reads them from its own thread
    /// while the interface thread replaces them.
    static LIMITS: Mutex<Option<TransferLimits>> = Mutex::new(None);

    /// The slice of the desktop's `Config` that `file_transfer` reads.
    pub struct Config {
        /// The upload limits a transfer applies, as the desktop's field of the
        /// same name.
        pub transfer_limits: TransferLimits,
    }

    /// Set once at start-up to the app's private storage.
    pub fn set_dir(dir: PathBuf) {
        let _ = DIR.set(dir);
    }

    /// Replaced whenever the network or the data settings change, and
    /// picked up by a running transfer within a quarter of a second.
    pub fn set_transfer_limits(limits: TransferLimits) {
        *LIMITS.lock().unwrap() = Some(limits);
    }

    /// The root `file_transfer` stores received files under, or `None`
    /// before start-up has called [`set_dir`], when there is nowhere to
    /// keep one yet.
    pub fn config_dir() -> Option<PathBuf> {
        DIR.get().cloned()
    }

    /// A fresh snapshot of the settings `file_transfer` reads. Cheap, so a
    /// running transfer calls it repeatedly to notice changed limits.
    pub fn load() -> Config {
        Config { transfer_limits: LIMITS.lock().unwrap().unwrap_or_default() }
    }
}
