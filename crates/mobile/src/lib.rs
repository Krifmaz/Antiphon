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
//! Everything is `cfg`'d to Android and iOS: on a desktop this crate is
//! empty, so `cargo build --workspace` is unaffected.

#![cfg(any(target_os = "android", target_os = "ios"))]

mod call;
mod convert;
pub mod platform;
pub mod runner;
mod settings;
mod ui;
