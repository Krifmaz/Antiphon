//! Everything the desktop app does that is not drawing.
//!
//! Calls ([`state`]), contacts and settings ([`config`]), chat history
//! ([`history`]), offline delivery ([`outbox`], [`delivery`]), file
//! transfer, groups, the VPN, discovery and the rest. This was the body of
//! the Slint app's crate; it moved here unchanged so the iced desktop app
//! and the Slint one run the same code, and so the phone app can keep
//! compiling the wire-level modules in by path from one place.

pub mod account_backup;
pub mod anonymity;
pub mod captions;
pub mod caps_lock;
pub mod community;
pub mod config;
pub mod connection_mode;
pub mod content_store;
pub mod control;
pub mod crash_report;
pub mod data_export;
pub mod delivery;
pub mod discovery;
pub mod fallback_relay;
pub mod file_integrity;
pub mod file_preview;
pub mod file_transfer;
pub mod first_run;
pub mod group;
pub mod group_keys;
pub mod history;
pub mod introduction;
pub mod key_capture;
pub mod link_preview;
pub mod link_quality;
pub mod mixer;
pub mod moderation;
pub mod notify;
pub mod outbox;
pub mod pacer;
pub mod presence;
pub mod push_wake;
pub mod reachability;
pub mod reactions;
pub mod replay_history;
pub mod sealed_ring;
pub mod search;
pub mod sharing;
pub mod soundboard;
pub mod sfu_call;
pub mod state;
pub mod swarm;
pub mod theme_presets;
pub mod profile;
pub mod transfer_limits;
pub mod transfer_queue;
pub mod video;
pub mod video_capture;
pub mod voice_message;
pub mod vpn;
pub mod wav;
