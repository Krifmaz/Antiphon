//! Everything the desktop app does that is not drawing.
//!
//! Calls ([`state`]), contacts and settings ([`config`]), chat history
//! ([`history`]), offline delivery ([`outbox`], [`delivery`]), file
//! transfer, groups, the VPN, discovery and the rest. The desktop app
//! (`crates/desktop`) draws it; the phone app compiles the wire-level
//! modules in by path from here.

pub mod account_backup;
pub mod anonymity;
pub mod captions;
pub mod caps_lock;
pub mod community;
pub mod community_board;
pub mod config;
pub mod connection_mode;
pub mod contact_devices;
pub mod content_store;
pub mod control;
pub mod crash_report;
pub mod data_export;
pub mod delivery;
pub mod device_sync;
pub mod devices;
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
pub mod link_quality;
pub mod mail_keys;
pub mod parts;
pub mod mixer;
pub mod moderation;
pub mod node_picker;
pub mod notify;
pub mod outbox;
pub mod pacer;
pub mod presence;
pub mod push_wake;
pub mod reachability;
pub mod reactions;
pub mod replay_history;
pub mod sealed_ring;
pub mod screen_audio;
pub mod sharing;
pub mod soundboard;
pub mod sfu_call;
pub mod state;
pub mod swarm;
pub mod profile;
pub mod transfer_limits;
pub mod transfer_queue;
pub mod video;
pub mod video_capture;
pub mod voice_message;
pub mod vpn;
pub mod wav;
