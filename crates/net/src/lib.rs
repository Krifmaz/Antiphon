//! UDP transport for direct P2P calls (spec §3 "Known contacts -> Direct
//! P2P"): handshake + encrypted send/receive over a single peer connection,
//! and the client side of a relay ([`relay`]) for "everyone else -> Relay".
//! The client side of a group-call SFU ([`sfu`]), anonymity-mode
//! circuits through several relays ([`onion`]), and the DHT ([`dht`]),
//! which lets contacts find each other again after an address changes.

pub mod account_move;
mod error;
pub mod fec;
pub mod file_transfer;
mod jitter;
mod peer;
pub mod port_map;
pub mod blob;
pub mod dht;
pub mod mailbox;
pub mod media_seal;
pub mod onion;
pub mod relay;
pub mod sfu;
mod socket;
pub mod stun;
pub mod vpn;

pub use error::NetError;
pub use jitter::{JitterBuffer, PopResult};
pub use peer::{PeerConnection, Received};
pub use socket::{AppSocket, Datagram};
