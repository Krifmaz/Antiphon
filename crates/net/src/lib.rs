//! UDP transport for direct P2P calls (spec §3 "Known contacts -> Direct
//! P2P"): handshake + encrypted send/receive over a single peer connection.
//! Relay fallback, the DHT, and SFU group transport are not implemented
//! here -- each is substantial additional infrastructure layered on top of
//! the same [`PeerConnection`] abstraction.

mod error;
pub mod fec;
pub mod file_transfer;
mod jitter;
mod peer;
pub mod port_map;
pub mod stun;

pub use error::NetError;
pub use jitter::{JitterBuffer, PopResult};
pub use peer::{PeerConnection, Received};
