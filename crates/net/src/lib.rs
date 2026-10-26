//! UDP transport for direct P2P calls (spec §3 "Known contacts -> Direct
//! P2P"): handshake + encrypted send/receive over a single peer connection,
//! and the client side of a relay ([`relay`]) for "everyone else -> Relay".
//! The client side of a group-call SFU ([`sfu`]) and the end-to-end
//! media sealing it relies on ([`media_seal`]). The DHT is not
//! implemented here.

mod error;
pub mod fec;
pub mod file_transfer;
mod jitter;
mod peer;
pub mod port_map;
pub mod blob;
pub mod mailbox;
pub mod media_seal;
pub mod relay;
pub mod sfu;
pub mod stun;

pub use error::NetError;
pub use jitter::{JitterBuffer, PopResult};
pub use peer::{PeerConnection, Received};
