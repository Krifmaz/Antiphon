//! UDP transport for direct P2P calls (spec §3 "Known contacts -> Direct
//! P2P"): handshake + encrypted send/receive over a single peer connection,
//! and the client side of a relay ([`relay`]) for "everyone else -> Relay".
//! The DHT ([`dht`]) lets contacts find each other again after an
//! address changes. The SFU group transport is not implemented here.

mod error;
pub mod fec;
pub mod file_transfer;
mod jitter;
mod peer;
pub mod port_map;
pub mod blob;
pub mod dht;
pub mod mailbox;
pub mod relay;
pub mod stun;

pub use error::NetError;
pub use jitter::{JitterBuffer, PopResult};
pub use peer::{PeerConnection, Received};
