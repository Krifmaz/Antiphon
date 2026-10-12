//! Wire framing that ties [`voip_crypto::Session`] to raw UDP datagrams
//! (spec §3): a small binary header plus AEAD ciphertext. Deliberately
//! hand-rolled rather than built on a general serialization framework --
//! it's four fixed fields, and an explicit, stable wire format matters more
//! here than derive-macro convenience.

mod error;
mod frame;

pub use error::ProtocolError;
pub use frame::{decode, encode, FrameType, RawFrame};
